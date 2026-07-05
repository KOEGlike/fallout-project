#![no_std]
#![no_main]

mod state_cell;

extern crate alloc;
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Delay, Instant, Timer};
use embedded_graphics::{
    Drawable,
    draw_target::{DrawTarget, DrawTargetExt},
    geometry::Point,
    image::Image,
    mono_font::{
        MonoTextStyle,
        ascii::{FONT_6X10, FONT_8X13},
    },
    pixelcolor::{Rgb565, Rgb888, RgbColor},
    text::Text,
};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_alloc::heap_allocator;
use esp_backtrace as _;
use mipidsi::{Builder, interface::SpiInterface, models::ST7735s, options::Orientation};
use state_cell::StateCell;

use esp_hal::{
    Blocking,
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig},
    interrupt::software::SoftwareInterruptControl,
    rng::Rng,
    spi::{
        Mode,
        master::{Config, Spi},
    },
    time::Rate,
    timer::timg::TimerGroup,
};
use loadcell::{LoadCell, hx711::HX711};
use log::{error, info};
use tinyqoi::Qoi;
use ws2812_timer_delay as ws2812;

/// Size of heap for dynamically-allocated memory
const HEAP_MEMORY_SIZE: usize = 72 * 1024;
const TEXT_STYLE: MonoTextStyle<'_, Rgb565> = MonoTextStyle::new(&FONT_8X13, Rgb565::BLACK);

static STATE: StateCell<CriticalSectionRawMutex, AppState, 1> = StateCell::new(AppState::Start);

esp_bootloader_esp_idf::esp_app_desc!();

/// Main task
#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    heap_allocator!(size: HEAP_MEMORY_SIZE);

    let timg1 = TimerGroup::new(p.TIMG1);
    let sw_int = SoftwareInterruptControl::new(p.SW_INTERRUPT);
    esp_rtos::start(timg1.timer0, sw_int.software_interrupt0);

    info!("started");

    // Display pins (ST7735s on a sensible default ESP32-C3 pinout):
    //   SCK  = GPIO4   MOSI = GPIO5
    //   CS   = GPIO6   (software CS via ExclusiveDevice)
    //   DC   = GPIO2   RST  = GPIO7
    let cs = Output::new(p.GPIO21, Level::High, OutputConfig::default());
    let dc = Output::new(p.GPIO20, Level::Low, OutputConfig::default());
    let rst = Output::new(p.GPIO9, Level::High, OutputConfig::default());

    // SPI2 is the only general-purpose SPI master on the ESP32-C3.
    // 10 MHz, Mode 0, MSB-first (default) - safe for ST7735s write.
    let spi = Spi::new(
        p.SPI2,
        Config::default()
            .with_frequency(Rate::from_mhz(10))
            .with_mode(Mode::_0),
    )
    .expect("SPI2 config")
    .with_sck(p.GPIO8)
    .with_mosi(p.GPIO10);

    let state = &STATE;

    spawner.spawn(display_task(spi, cs, dc, rst, state).expect("spawn display_task"));

    let mut loadcell = HX711::new(
        Output::new(p.GPIO2, Level::Low, OutputConfig::default()),
        Input::new(p.GPIO3, InputConfig::default()),
        Delay,
    );

    loop {
        info!("Loadcell: {:#?}", loadcell.read());
        Timer::after_millis(200).await;
    }

    // let buttons = Buttons {
    //     btn0: Input::new(p.GPIO9, InputConfig::default()),
    //     btn1: Input::new(p.GPIO10, InputConfig::default()),
    //     btn2: Input::new(p.GPIO11, InputConfig::default()),
    //     btn3: Input::new(p.GPIO12, InputConfig::default()),
    // };

    // spawner.spawn(logic_task(state, loadcell, buttons).expect("spawn logic_task"));
}

struct Buttons {
    btn0: Input<'static>,
    btn1: Input<'static>,
    btn2: Input<'static>,
    btn3: Input<'static>,
}

#[embassy_executor::task]
async fn logic_task(
    state: &'static StateCell<CriticalSectionRawMutex, AppState, 1>,
    loadcell: HX711<Output<'static>, Input<'static>, Delay>,
    mut buttons: Buttons,
) {
    state.set(AppState::Start).await;
    Timer::after_secs(20).await;
    state.set(AppState::Rules).await;
    Timer::after_secs(30).await;
    state
        .set(AppState::Game {
            soup_hp: 100,
            player_hp: 100,
            soup_status: SoupStatus::Neutral,
        })
        .await;
}

#[embassy_executor::task]
async fn rand_btn() {}

#[embassy_executor::task]
async fn display_task(
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

    let mut rng = Rng::new();

    state.set(AppState::Rules).await;
    loop {
        let app_state = receiver.changed().await;

        let sky_image = if rng.random() % 100 < 33 {
            &sky_seal
        } else {
            &sky
        };
        let sky = Image::new(sky_image, Point::new(-10, 0));
        if let Err(e) = sky.draw(&mut display.color_converted()) {
            error!("draw failed: {e:?}");
            continue;
        }

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
                    "Rules:\n-Press soup, be in\nthe green zone to\nattack soup\n-Press the buttons\nwhen to the LEDs\nlight up to protect\nyourself",
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
            } => {
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
            AppState::EndScreen { player_won } => todo!(),
        }
    }
}

#[derive(Clone)]
enum SoupStatus {
    Angry,
    Sad,
    Neutral,
}

#[derive(Clone)]
enum AppState {
    Start,
    Rules,
    Game {
        soup_hp: u32,
        player_hp: u32,
        soup_status: SoupStatus,
    },
    EndScreen {
        player_won: bool,
    },
}
