use embassy_executor;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Instant, Timer};
use esp_hal::{
    gpio::{Input, Output},
    rng::Rng,
};
use log::info;

use crate::state::{AppState, SoupStatus};
use crate::state_cell::StateCell;

/// Window in which the player must press the lit button to avoid damage.
const REACTION_WINDOW_MS: u64 = 1500;
/// Once the window expires, damage is applied every `DAMAGE_TICK_MS` and ramps
/// up slowly while the player keeps missing the matching button.
const DAMAGE_TICK_MS: u64 = 750;
/// Small penalty for pressing a button while that button's LED is off.
const OFF_LIGHT_DAMAGE: u32 = 1;

const LED_COUNT: usize = 2;
const LED_GPIO_LABELS: [&str; LED_COUNT] = ["GPIO4", "GPIO7"];
const BUTTON_GPIO_LABELS: [&str; LED_COUNT] = ["GPIO5", "GPIO6"];
const BUTTON_POLL_MS: u64 = 10;
const BUTTON_DEBOUNCE_MS: u64 = 30;
const BUTTON_WAIT_LOG_MS: u64 = 500;
const LED_SELF_TEST_MS: u64 = 1000;
const PRE_GAME_LED_BLINK_MS: u64 = 500;

/// Two normal active-high LEDs. The LED positive/anode side is connected to the
/// ESP GPIO, so `High` turns the LED on and `Low` turns it off.
pub struct ButtonLeds {
    pub led0: Output<'static>,
    pub led1: Output<'static>,
}

impl ButtonLeds {
    fn all_off(&mut self) {
        self.led0.set_low();
        self.led1.set_low();
    }

    fn all_on(&mut self) {
        self.led0.set_high();
        self.led1.set_high();
    }

    fn set_only(&mut self, idx: usize) {
        self.all_off();
        match idx {
            0 => self.led0.set_high(),
            _ => self.led1.set_high(),
        }
    }
}

/// The two remaining reaction buttons, each mapped to one LED. Pulled up so
/// that an external button to ground reads as `Level::Low` when pressed.
pub struct Buttons {
    pub btn0: Input<'static>,
    pub btn1: Input<'static>,
}

impl Buttons {
    fn is_pressed(&self, idx: usize) -> bool {
        match idx {
            0 => self.btn0.is_low(),
            _ => self.btn1.is_low(),
        }
    }

    fn pressed_index(&self) -> Option<usize> {
        for idx in 0..LED_COUNT {
            if self.is_pressed(idx) {
                return Some(idx);
            }
        }
        None
    }

    fn pressed_unlit_index(&self, lit_idx: Option<usize>) -> Option<usize> {
        for idx in 0..LED_COUNT {
            if Some(idx) != lit_idx && self.is_pressed(idx) {
                return Some(idx);
            }
        }
        None
    }
}

async fn damage_player(
    state: &'static StateCell<CriticalSectionRawMutex, AppState, 1>,
    damage: u32,
) -> bool {
    state
        .update(|s| match s {
            AppState::Game {
                soup_hp,
                player_hp,
                sweet_spot_min,
                sweet_spot_max,
                sweet_spot_progress,
                loadcell_reading,
                ..
            } => AppState::Game {
                soup_hp: *soup_hp,
                player_hp: player_hp.saturating_sub(damage),
                soup_status: SoupStatus::Angry,
                sweet_spot_min: *sweet_spot_min,
                sweet_spot_max: *sweet_spot_max,
                sweet_spot_progress: *sweet_spot_progress,
                loadcell_reading: *loadcell_reading,
            },
            other => other.clone(),
        })
        .await;

    let after = state.get().await;
    if let AppState::Game {
        player_hp: 0,
        soup_hp,
        loadcell_reading,
        ..
    } = after
    {
        info!(
            "game ended: player lost; result=player_defeated soup_hp={} player_hp=0 final_loadcell_reading={}",
            soup_hp, loadcell_reading
        );
        state.set(AppState::EndScreen { player_won: false }).await;
        return true;
    }

    false
}

/// Reaction mini-game: lights a random LED and drains the player's HP if the
/// matching button isn't pressed in time. Damage ramps up slowly every tick
/// past the reaction window until the button is finally pressed.
#[embassy_executor::task]
pub async fn button_task(
    state: &'static StateCell<CriticalSectionRawMutex, AppState, 1>,
    mut leds: ButtonLeds,
    buttons: Buttons,
) {
    info!("button task started");
    info!("reaction buttons active-low: GPIO5/GPIO6 pressed when connected to GND");

    let rng = Rng::new();
    leds.all_off();

    info!("led self-test: both GPIO4 and GPIO7 on");
    leds.all_on();
    Timer::after_millis(LED_SELF_TEST_MS).await;
    leds.all_off();
    Timer::after_millis(200).await;
    info!("led self-test: GPIO4 then GPIO7");
    leds.set_only(0);
    Timer::after_millis(LED_SELF_TEST_MS).await;
    leds.set_only(1);
    Timer::after_millis(LED_SELF_TEST_MS).await;
    leds.all_off();
    info!("led self-test complete");

    loop {
        // Wait until the game phase begins. Blink the LEDs during Start/Rules so
        // the normal GPIO LED wiring is visibly testable before the button game.
        let mut pre_game_led = 0usize;
        loop {
            let s = state.get().await;
            if matches!(&s, AppState::Game { .. }) {
                break;
            }

            if matches!(&s, AppState::EndScreen { .. }) {
                leds.all_off();
                Timer::after_millis(PRE_GAME_LED_BLINK_MS).await;
                continue;
            }

            info!(
                "button states pre-game: GPIO5_pressed={} GPIO6_pressed={}",
                buttons.btn0.is_low(),
                buttons.btn1.is_low()
            );
            leds.set_only(pre_game_led);
            pre_game_led = (pre_game_led + 1) % LED_COUNT;
            Timer::after_millis(PRE_GAME_LED_BLINK_MS).await;
        }

        leds.all_off();
        info!("button task entered game state");

        'game_loop: loop {
            let current = state.get().await;
            if !matches!(current, AppState::Game { .. }) {
                leds.all_off();
                break 'game_loop;
            }

            // Light up a random LED and remember when it turned on.
            let idx = (rng.random() as usize) % LED_COUNT;
            leds.set_only(idx);
            info!(
                "led lit: led={} led_gpio={} button={} button_gpio={} GPIO5_pressed={} GPIO6_pressed={}",
                idx,
                LED_GPIO_LABELS[idx],
                idx,
                BUTTON_GPIO_LABELS[idx],
                buttons.btn0.is_low(),
                buttons.btn1.is_low()
            );
            let lit_at = Instant::now();
            let mut next_button_wait_log_ms = BUTTON_WAIT_LOG_MS;
            let mut next_damage_ms = REACTION_WINDOW_MS;
            let mut armed_for_press = !buttons.is_pressed(idx);
            let mut armed_for_off_light_press = buttons.pressed_unlit_index(Some(idx)).is_none();
            if !armed_for_press {
                info!(
                    "button already held when LED lit; waiting for release before accepting press: button={} button_gpio={}",
                    idx, BUTTON_GPIO_LABELS[idx]
                );
            }
            if !armed_for_off_light_press {
                info!("unlit button already held when LED lit; waiting for release before penalty");
            }

            // Damage amount, reset for each new LED. The first tick of damage
            // drains 1 HP; each subsequent missed tick adds 1 more HP of damage.
            let mut damage: u32 = 1;

            // Poll continuously so the buttons remain responsive even after damage
            // starts. Damage is scheduled by elapsed time instead of sleeping for a
            // whole damage tick and missing button transitions during that sleep.
            loop {
                if !matches!(state.get().await, AppState::Game { .. }) {
                    leds.all_off();
                    break 'game_loop;
                }

                let elapsed_ms = lit_at.elapsed().as_millis();
                if elapsed_ms >= next_button_wait_log_ms {
                    info!(
                        "waiting for button: lit_button={} led_gpio={} button_gpio={} pressed={} GPIO5_pressed={} GPIO6_pressed={}",
                        idx,
                        LED_GPIO_LABELS[idx],
                        BUTTON_GPIO_LABELS[idx],
                        buttons.is_pressed(idx),
                        buttons.btn0.is_low(),
                        buttons.btn1.is_low()
                    );
                    next_button_wait_log_ms += BUTTON_WAIT_LOG_MS;
                }

                if !armed_for_off_light_press {
                    if buttons.pressed_unlit_index(Some(idx)).is_none() {
                        armed_for_off_light_press = true;
                        info!("unlit button released; off-light penalty armed");
                    }
                } else if let Some(unlit_idx) = buttons.pressed_unlit_index(Some(idx)) {
                    Timer::after_millis(BUTTON_DEBOUNCE_MS).await;
                    if buttons.is_pressed(unlit_idx) {
                        info!(
                            "button pressed while its LED is off: button={} button_gpio={} damage={}",
                            unlit_idx, BUTTON_GPIO_LABELS[unlit_idx], OFF_LIGHT_DAMAGE
                        );
                        if damage_player(state, OFF_LIGHT_DAMAGE).await {
                            leds.all_off();
                            break 'game_loop;
                        }
                        armed_for_off_light_press = false;
                    }
                }

                if !armed_for_press {
                    if !buttons.is_pressed(idx) {
                        armed_for_press = true;
                        info!(
                            "button released; press armed: button={} button_gpio={}",
                            idx, BUTTON_GPIO_LABELS[idx]
                        );
                    }
                } else if buttons.is_pressed(idx) {
                    Timer::after_millis(BUTTON_DEBOUNCE_MS).await;
                    if buttons.is_pressed(idx) {
                        info!(
                            "button pressed: button={} button_gpio={}",
                            idx, BUTTON_GPIO_LABELS[idx]
                        );
                        break;
                    }
                }

                if elapsed_ms >= next_damage_ms {
                    info!(
                        "button missed damage: button={} button_gpio={} damage={}",
                        idx, BUTTON_GPIO_LABELS[idx], damage
                    );
                    if damage_player(state, damage).await {
                        leds.all_off();
                        break 'game_loop;
                    }
                    damage = damage.saturating_add(1);
                    next_damage_ms += DAMAGE_TICK_MS;
                }

                Timer::after_millis(BUTTON_POLL_MS).await;
            }

            // Button pressed: turn the LED off and wait a random beat before
            // lighting the next one. During this gap, any newly-pressed button is an
            // off-light press and gets a small penalty.
            leds.all_off();
            let gap_ms = 300 + (rng.random() % 700);
            let gap_started = Instant::now();
            let mut armed_for_gap_press = buttons.pressed_index().is_none();
            if !armed_for_gap_press {
                info!(
                    "button still held after LED turned off; waiting for release before off-light penalty"
                );
            }

            while gap_started.elapsed().as_millis() < gap_ms as u64 {
                if !matches!(state.get().await, AppState::Game { .. }) {
                    leds.all_off();
                    break 'game_loop;
                }

                if !armed_for_gap_press {
                    if buttons.pressed_index().is_none() {
                        armed_for_gap_press = true;
                        info!("all buttons released; no-light penalty armed");
                    }
                } else if let Some(pressed_idx) = buttons.pressed_index() {
                    Timer::after_millis(BUTTON_DEBOUNCE_MS).await;
                    if buttons.is_pressed(pressed_idx) {
                        info!(
                            "button pressed while no LED is lit: button={} button_gpio={} damage={}",
                            pressed_idx, BUTTON_GPIO_LABELS[pressed_idx], OFF_LIGHT_DAMAGE
                        );
                        if damage_player(state, OFF_LIGHT_DAMAGE).await {
                            leds.all_off();
                            break 'game_loop;
                        }
                        armed_for_gap_press = false;
                    }
                }

                Timer::after_millis(BUTTON_POLL_MS).await;
            }
        }
    }
}
