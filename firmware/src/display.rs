use embassy_executor;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Delay;
use embedded_graphics::{
    Drawable,
    draw_target::{DrawTarget, DrawTargetExt},
    geometry::{Point, Size},
    image::Image,
    mono_font::{MonoTextStyle, ascii::FONT_8X13},
    pixelcolor::{Rgb565, RgbColor},
    primitives::{Primitive, PrimitiveStyle, Rectangle},
    text::Text,
};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::{Blocking, gpio::Output, rng::Rng, spi::master::Spi};
use log::error;
use mipidsi::{Builder, interface::SpiInterface, models::ST7735s, options::Orientation};
use tinyqoi::Qoi;

use crate::state::{AppState, SoupStatus};
use crate::state_cell::StateCell;
use crate::sweet_spot::SWEET_SPOT_HOLD_MS;

const TEXT_STYLE: MonoTextStyle<'_, Rgb565> = MonoTextStyle::new(&FONT_8X13, Rgb565::BLACK);

// --- UI layout (screen is 160x128 after 270° rotation) ---
const HUD_BAR_Y: i32 = 50;
const HUD_BAR_W: u32 = 8;
const HUD_BAR_H: u32 = 70;
const SOUP_HP_X: i32 = 130;
const PLAYER_HP_X: i32 = 120;
const PROGRESS_X: i32 = 110;

const FORCE_METER_X: i32 = 145;
const FORCE_METER_Y: i32 = 50;
const FORCE_METER_W: u32 = 12;
const FORCE_METER_H: u32 = 70;
const FORCE_MAX: i64 = 1_100_000;

/// Draw a vertical bar (health bar or progress bar) at `(x, y)` with width
/// `w` and height `h`. `value`/`max` determines the bottom-up fill proportion;
/// `fill` and `outline` set the colors.
fn draw_bar<D: DrawTarget<Color = Rgb565>>(
    target: &mut D,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    value: u32,
    max: u32,
    fill: Rgb565,
    outline: Rgb565,
) -> Result<(), D::Error> {
    let bounds = Rectangle::new(Point::new(x, y), Size::new(w, h));
    bounds
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(target)?;
    bounds
        .into_styled(PrimitiveStyle::with_stroke(outline, 1))
        .draw(target)?;

    let inner_w = w.saturating_sub(2);
    let inner_h = h.saturating_sub(2);
    let fill_h = if max == 0 {
        0
    } else {
        (value.min(max) as u64 * inner_h as u64 / max as u64) as u32
    };
    if inner_w > 0 && fill_h > 0 {
        let fill_y = y + h as i32 - 1 - fill_h as i32;
        let fill_style = PrimitiveStyle::with_fill(fill);
        Rectangle::new(Point::new(x + 1, fill_y), Size::new(inner_w, fill_h))
            .into_styled(fill_style)
            .draw(target)?;
    }
    Ok(())
}

/// Draw the sweet-spot force meter: a vertical track with the target zone
/// (green band) and the current force reading (red horizontal marker line).
fn draw_force_meter<D: DrawTarget<Color = Rgb565>>(
    target: &mut D,
    reading: i32,
    zone_min: i32,
    zone_max: i32,
) -> Result<(), D::Error> {
    // Track background + outline. Filling the track clears the previous marker
    // without redrawing the whole screen.
    let track = Rectangle::new(
        Point::new(FORCE_METER_X, FORCE_METER_Y),
        Size::new(FORCE_METER_W, FORCE_METER_H),
    );
    track
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(target)?;
    track
        .into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, 1))
        .draw(target)?;

    let inner_h = FORCE_METER_H as i32 - 2;
    let meter_bottom = FORCE_METER_Y + FORCE_METER_H as i32 - 2;
    let value_to_y = |value: i32| {
        let value = value.clamp(0, FORCE_MAX as i32) as i64;
        let offset = if inner_h <= 1 {
            0
        } else {
            (value * (inner_h - 1) as i64 / FORCE_MAX) as i32
        };
        meter_bottom - offset
    };

    // Sweet-spot zone band (green), clamped to meter bounds.
    let zone_y_low = value_to_y(zone_min);
    let zone_y_high = value_to_y(zone_max);
    let zone_y_start = zone_y_high.min(zone_y_low).max(FORCE_METER_Y + 1);
    let zone_y_end = zone_y_high.max(zone_y_low).min(meter_bottom);
    if zone_y_end >= zone_y_start {
        let zone_h = (zone_y_end - zone_y_start + 1) as u32;
        Rectangle::new(
            Point::new(FORCE_METER_X + 1, zone_y_start),
            Size::new(FORCE_METER_W - 2, zone_h),
        )
        .into_styled(PrimitiveStyle::with_fill(Rgb565::GREEN))
        .draw(target)?;
    }

    // Current force marker (red horizontal line, 2px tall).
    let marker_y = value_to_y(reading).clamp(FORCE_METER_Y + 1, meter_bottom);
    Rectangle::new(
        Point::new(FORCE_METER_X, marker_y),
        Size::new(FORCE_METER_W, 2),
    )
    .into_styled(PrimitiveStyle::with_fill(Rgb565::RED))
    .draw(target)?;

    Ok(())
}

/// Renders the current `AppState` to the ST7735s display. Subscribes to the
/// shared state cell and redraws whenever it changes.
#[embassy_executor::task]
pub async fn display_task(
    spi: Spi<'static, Blocking>,
    cs: Output<'static>,
    dc: Output<'static>,
    rst: Output<'static>,
    state: &'static StateCell<CriticalSectionRawMutex, AppState, 1>,
) {
    let soup_sad = Qoi::new(include_bytes!("../images/sad.qoi")).unwrap();
    let soup_angry = Qoi::new(include_bytes!("../images/angry.qoi")).unwrap();
    let soup_neutral = Qoi::new(include_bytes!("../images/neutral.qoi")).unwrap();
    let soup_sign = Qoi::new(include_bytes!("../images/sign.qoi")).unwrap();
    let sky_seal = Qoi::new(include_bytes!("../images/sky_seal.qoi")).unwrap();
    let sky = Qoi::new(include_bytes!("../images/sky.qoi")).unwrap();

    let soup_offset = Point::new(5, 45);

    let mut buffer = [0u8; 512];
    // Wrap the SpiBus + CS pin into a SpiDevice (mipidsi requires SpiDevice).
    let device = match ExclusiveDevice::new(spi, cs, Delay) {
        Ok(d) => d,
        Err(e) => {
            error!("ExclusiveDevice build failed: {e:?}");
            return;
        }
    };
    let di = SpiInterface::new(device, dc, &mut buffer);
    let mut display = match Builder::new(ST7735s, di)
        .reset_pin(rst)
        .orientation(Orientation::new().rotate(mipidsi::options::Rotation::Deg270))
        .init(&mut Delay)
    {
        Ok(d) => d,
        Err(e) => {
            error!("display init failed: {e:?}");
            return;
        }
    };

    let mut receiver = state.subscriber().expect("state subscriber");

    let rng = Rng::new();

    let mut last_variant = u8::MAX;
    let mut last_soup_status: Option<SoupStatus> = None;

    loop {
        let app_state = receiver.changed().await;

        let current_variant = match &app_state {
            AppState::Start => 0,
            AppState::Rules => 1,
            AppState::Game { .. } => 2,
            AppState::EndScreen { .. } => 3,
        };
        let current_soup_status = match &app_state {
            AppState::Game { soup_status, .. } => Some(*soup_status),
            AppState::EndScreen { player_won } => Some(if *player_won {
                SoupStatus::Angry
            } else {
                SoupStatus::Neutral
            }),
            _ => None,
        };
        let full_redraw = current_variant != last_variant;
        let soup_redraw = full_redraw || current_soup_status != last_soup_status;
        if full_redraw {
            last_variant = current_variant;
            let sky_seal_mode = rng.random() % 100 < 33;

            let sky_image = if sky_seal_mode { &sky_seal } else { &sky };
            let sky = Image::new(sky_image, Point::new(-10, 0));
            if let Err(e) = sky.draw(&mut display.color_converted()) {
                error!("draw failed: {e:?}");
                continue;
            }
        }
        last_soup_status = current_soup_status;

        match app_state {
            AppState::Start => {
                let image = Image::new(&soup_sign, soup_offset);
                if let Err(e) = image.draw(&mut display.color_converted()) {
                    error!("draw failed: {e:?}");
                    continue;
                }

                let welcome_text =
                    Text::new("Welcome to\nSoup Game!", Point::new(70, 80), TEXT_STYLE);
                if let Err(e) = welcome_text.draw(&mut display.color_converted()) {
                    error!("draw failed: {e:?}");
                    continue;
                }
            }
            AppState::Rules => {
                let rules_text = Text::new(
                    "Rules:\n-Press soup, be in\nthe green zone to\nattack soup\n-Press the buttons\nwhen the LEDs\nlight up to protect\nyourself",
                    Point::new(5, 35),
                    TEXT_STYLE,
                );
                if let Err(e) = rules_text.draw(&mut display.color_converted()) {
                    error!("draw failed: {e:?}");
                    continue;
                }
            }
            AppState::Game {
                soup_hp,
                player_hp,
                soup_status,
                sweet_spot_min,
                sweet_spot_max,
                sweet_spot_progress,
                loadcell_reading,
                ..
            } => {
                // Static game art only needs a full redraw when entering the game
                // screen. Live load-cell updates redraw only the HUD widgets below
                // to avoid full-screen flicker.
                if soup_redraw {
                    let soup_image = match soup_status {
                        SoupStatus::Angry => &soup_angry,
                        SoupStatus::Sad => &soup_sad,
                        SoupStatus::Neutral => &soup_neutral,
                    };
                    let soup_image = Image::new(soup_image, soup_offset);
                    if let Err(e) = soup_image.draw(&mut display.color_converted()) {
                        error!("draw failed: {e:?}");
                        continue;
                    }
                }

                let mut dc = display.color_converted();

                // Soup HP bar (red, vertical).
                if let Err(e) = draw_bar(
                    &mut dc,
                    SOUP_HP_X,
                    HUD_BAR_Y,
                    HUD_BAR_W,
                    HUD_BAR_H,
                    soup_hp,
                    100,
                    Rgb565::RED,
                    Rgb565::WHITE,
                ) {
                    error!("draw failed: {e:?}");
                    continue;
                }

                // Player HP bar (green, vertical).
                if let Err(e) = draw_bar(
                    &mut dc,
                    PLAYER_HP_X,
                    HUD_BAR_Y,
                    HUD_BAR_W,
                    HUD_BAR_H,
                    player_hp,
                    100,
                    Rgb565::GREEN,
                    Rgb565::WHITE,
                ) {
                    error!("draw failed: {e:?}");
                    continue;
                }

                // Sweet-spot hold progress bar (yellow, vertical).
                if let Err(e) = draw_bar(
                    &mut dc,
                    PROGRESS_X,
                    HUD_BAR_Y,
                    HUD_BAR_W,
                    HUD_BAR_H,
                    sweet_spot_progress,
                    SWEET_SPOT_HOLD_MS,
                    Rgb565::YELLOW,
                    Rgb565::WHITE,
                ) {
                    error!("draw failed: {e:?}");
                    continue;
                }

                // Force meter (zone band + current reading marker).
                if let Err(e) =
                    draw_force_meter(&mut dc, loadcell_reading, sweet_spot_min, sweet_spot_max)
                {
                    error!("draw failed: {e:?}");
                    continue;
                }
            }
            AppState::EndScreen { player_won } => {
                let soup_image = if player_won {
                    &soup_angry
                } else {
                    &soup_neutral
                };
                let soup_image = Image::new(soup_image, soup_offset);
                if let Err(e) = soup_image.draw(&mut display.color_converted()) {
                    error!("draw failed: {e:?}");
                    continue;
                }
                let msg = if player_won { "You win!" } else { "You lose!" };
                let end_text = Text::new(msg, Point::new(70, 80), TEXT_STYLE);
                if let Err(e) = end_text.draw(&mut display.color_converted()) {
                    error!("draw failed: {e:?}");
                    continue;
                }
            }
        }
    }
}
