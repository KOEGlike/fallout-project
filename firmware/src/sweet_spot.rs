// ---------------------------------------------------------------------------
// Load-cell "sweet spot" mechanic
// ---------------------------------------------------------------------------

use embassy_executor;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Delay, Timer};
use esp_hal::{
    gpio::{Input, Output},
    rng::Rng,
};
use loadcell::{LoadCell, hx711::HX711};
use log::info;

use crate::state::AppState;
use crate::state_cell::StateCell;

/// Half-width of the sweet spot zone, in raw HX711 counts. The player must
/// keep the reading within `target ± SWEET_SPOT_TOLERANCE`.
const SWEET_SPOT_TOLERANCE: i32 = 50_000;

/// Range of possible sweet-spot centres, in raw HX711 counts (post-tare, so
/// resting = ~0 and pressing down gives positive values). A new target is
/// picked inside this range each time the zone moves.
const SWEET_SPOT_MIN_CENTER: i32 = 100_000;
const SWEET_SPOT_MAX_CENTER: i32 = 1_000_000;

/// How long the player must hold the force inside the zone (in ms) before the
/// soup takes damage and the zone jumps to a new position.
pub const SWEET_SPOT_HOLD_MS: u32 = 1500;

/// Damage dealt to the soup each time the hold completes.
const SOUP_DAMAGE: u32 = 10;

/// Load-cell poll interval. At HX711 gain A128 the chip samples at ~80 Hz;
/// 50 ms gives responsive force feedback without flooding the state bus.
const LOADCELL_POLL_MS: u64 = 50;

/// Pick a random sweet-spot centre inside the configured range.
fn random_sweet_spot_center(rng: &Rng) -> i32 {
    let span = (SWEET_SPOT_MAX_CENTER - SWEET_SPOT_MIN_CENTER) as u32;
    SWEET_SPOT_MIN_CENTER + (rng.random() % span) as i32
}

/// Sweet-spot mini-game: the player must hold a target force (read from the
/// HX711 load cell) inside a moving zone for `SWEET_SPOT_HOLD_MS` to damage
/// the soup. Each completed hold jumps the zone to a new random position.
#[embassy_executor::task]
pub async fn loadcell_task(
    state: &'static StateCell<CriticalSectionRawMutex, AppState, 1>,
    mut loadcell: HX711<Output<'static>, Input<'static>, Delay>,
) {
    let rng = Rng::new();

    // Use gain A128 on channel A for ~80 Hz sampling — responsive enough for
    // a real-time force meter.
    loadcell.set_gain_mode(loadcell::hx711::GainMode::A128);

    // Wait until the game phase begins.
    loop {
        if matches!(state.get().await, AppState::Game { .. }) {
            break;
        }
        Timer::after_millis(50).await;
    }

    // Tare the load cell with (hopefully) no load so that resting reads ~0.
    loadcell.tare(10);

    // Pick the first sweet-spot zone and publish it to the state.
    let center = random_sweet_spot_center(&rng);
    let mut ss_min = center - SWEET_SPOT_TOLERANCE;
    let mut ss_max = center + SWEET_SPOT_TOLERANCE;
    let mut progress: u32 = 0;

    state
        .update(|s| match s {
            AppState::Game {
                soup_hp,
                player_hp,
                soup_status,
                ..
            } => AppState::Game {
                soup_hp: *soup_hp,
                player_hp: *player_hp,
                soup_status: soup_status.clone(),
                sweet_spot_min: ss_min,
                sweet_spot_max: ss_max,
                sweet_spot_progress: 0,
                loadcell_reading: 0,
            },
            other => other.clone(),
        })
        .await;

    loop {
        if !matches!(state.get().await, AppState::Game { .. }) {
            return;
        }

        // Try to read — skip if the HX711 hasn't finished a conversion yet.
        let reading = match loadcell.read() {
            Ok(v) => v,
            Err(_) => {
                Timer::after_millis(LOADCELL_POLL_MS).await;
                continue;
            }
        };

        info!("readin {:#?}", reading);

        let in_zone = reading >= ss_min && reading <= ss_max;

        if in_zone {
            progress = progress.saturating_add(LOADCELL_POLL_MS as u32);
        } else {
            progress = 0;
        }

        if progress >= SWEET_SPOT_HOLD_MS {
            // Hold complete: deal damage, jump the sweet spot, reset progress.
            progress = 0;
            let new_center = random_sweet_spot_center(&rng);
            ss_min = new_center - SWEET_SPOT_TOLERANCE;
            ss_max = new_center + SWEET_SPOT_TOLERANCE;

            state
                .update(|s| match s {
                    AppState::Game {
                        soup_hp,
                        player_hp,
                        soup_status,
                        ..
                    } => AppState::Game {
                        soup_hp: soup_hp.saturating_sub(SOUP_DAMAGE),
                        player_hp: *player_hp,
                        soup_status: soup_status.clone(),
                        sweet_spot_min: ss_min,
                        sweet_spot_max: ss_max,
                        sweet_spot_progress: 0,
                        loadcell_reading: reading,
                    },
                    other => other.clone(),
                })
                .await;

            // Check for a win.
            if let AppState::Game { soup_hp: 0, .. } = state.get().await {
                state.set(AppState::EndScreen { player_won: true }).await;
                return;
            }
        } else {
            // Just publish the current reading + progress for the UI.
            let cur_min = ss_min;
            let cur_max = ss_max;
            let cur_progress = progress;
            state
                .update(|s| match s {
                    AppState::Game {
                        soup_hp,
                        player_hp,
                        soup_status,
                        ..
                    } => AppState::Game {
                        soup_hp: *soup_hp,
                        player_hp: *player_hp,
                        soup_status: soup_status.clone(),
                        sweet_spot_min: cur_min,
                        sweet_spot_max: cur_max,
                        sweet_spot_progress: cur_progress,
                        loadcell_reading: reading,
                    },
                    other => other.clone(),
                })
                .await;
        }

        Timer::after_millis(LOADCELL_POLL_MS).await;
    }
}
