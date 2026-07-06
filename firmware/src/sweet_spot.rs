// ---------------------------------------------------------------------------
// Load-cell "sweet spot" mechanic
// ---------------------------------------------------------------------------

use core::hint::spin_loop;

use embassy_executor;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Timer;
use esp_hal::{
    gpio::{Input, Output},
    rng::Rng,
};
use log::{info, warn};

use crate::state::{AppState, SoupStatus};
use crate::state_cell::StateCell;

/// Half-width of the sweet spot zone, in raw HX711 counts. The player must
/// keep the reading within `target ± SWEET_SPOT_TOLERANCE`.
const SWEET_SPOT_TOLERANCE: i32 = 100_000;

/// Range of possible sweet-spot centres, in raw HX711 counts (post-tare, so
/// resting = ~0 and pressing down gives positive values). A new target is
/// picked inside this range each time the zone moves.
const SWEET_SPOT_MIN_CENTER: i32 = 100_000;
const SWEET_SPOT_MAX_CENTER: i32 = 700_000;

/// How long the player must hold the force inside the zone (in ms) before the
/// soup takes damage and the zone jumps to a new position.
pub const SWEET_SPOT_HOLD_MS: u32 = 1500;

/// Damage dealt to the soup each time the hold completes.
const SOUP_DAMAGE: u32 = 10;

/// Load-cell poll interval. At HX711 gain A128 the chip samples at ~80 Hz;
/// 50 ms gives responsive force feedback without flooding the state bus.
const LOADCELL_POLL_MS: u64 = 50;

/// Number of initial valid samples to use for the tare offset. Keep this at 1
/// while debugging the HX711 path so a single good sample gets us into the live
/// game read loop immediately.
const LOADCELL_TARE_SAMPLES: usize = 1;

/// Maximum time to spend collecting tare samples. If the HX711/wiring is bad,
/// gameplay should continue with warnings instead of silently hanging forever.
const LOADCELL_TARE_TIMEOUT_MS: u64 = 3_000;

/// How often to log while waiting for HX711 `DT` to go low.
const LOADCELL_NOT_READY_LOG_MS: u64 = 1000;

const HX711_MINIMUM: i32 = -(1 << 23);
const HX711_MAXIMUM: i32 = (1 << 23) - 1;
const HX711_GAIN_PULSES_A128: u8 = 1;
const HX711_SETTLE_SPINS: usize = 16;

#[inline(always)]
fn hx711_short_delay() {
    // The HX711 needs SCK high/low for at least ~0.2µs, but SCK high for
    // longer than ~60µs powers it down. Avoid timer-based microsecond delays
    // here; their overhead can be too high on ESP debug/RTOS builds.
    for _ in 0..HX711_SETTLE_SPINS {
        spin_loop();
    }
}

pub struct Hx711 {
    sck: Output<'static>,
    dt: Input<'static>,
    offset: i32,
}

impl Hx711 {
    pub fn new(mut sck: Output<'static>, dt: Input<'static>) -> Self {
        sck.set_low();
        Self { sck, dt, offset: 0 }
    }

    fn is_ready(&self) -> bool {
        self.dt.is_low()
    }

    fn set_offset(&mut self, offset: i32) {
        self.offset = offset;
    }

    fn read_force_debug(&mut self) -> Option<(i32, i32, i32)> {
        let raw = self.read_raw_if_ready()?;
        if is_saturated_hx711_reading(raw) {
            return Some((raw, 0, raw));
        }

        let delta = raw as i64 - self.offset as i64;
        let delta = delta.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        let force = (delta as i64).abs().min(i32::MAX as i64) as i32;

        Some((raw, delta, force))
    }

    fn read_raw_if_ready(&mut self) -> Option<i32> {
        if !self.is_ready() {
            return None;
        }

        Some(self.read_raw())
    }

    fn read_raw(&mut self) -> i32 {
        let value = critical_section::with(|_| {
            let mut value = 0u32;

            for _ in 0..24 {
                self.sck.set_high();
                hx711_short_delay();

                let bit = self.dt.is_high() as u32;
                value = (value << 1) | bit;

                self.sck.set_low();
                hx711_short_delay();
            }

            // One extra pulse selects channel A, gain 128 for the next sample.
            for _ in 0..HX711_GAIN_PULSES_A128 {
                self.sck.set_high();
                hx711_short_delay();
                self.sck.set_low();
                hx711_short_delay();
            }

            value
        });

        ((value as i32) << 8) >> 8
    }
}

/// Pick a random sweet-spot centre inside the configured range.
fn random_sweet_spot_center(rng: &Rng) -> i32 {
    let span = (SWEET_SPOT_MAX_CENTER - SWEET_SPOT_MIN_CENTER) as u32;
    SWEET_SPOT_MIN_CENTER + (rng.random() % span) as i32
}

fn is_saturated_hx711_reading(reading: i32) -> bool {
    reading == HX711_MAXIMUM || reading == HX711_MINIMUM
}

async fn tare_loadcell(loadcell: &mut Hx711) {
    // Do not use the crate's blocking `tare()`: it busy-waits until `DT` goes
    // low, which can silently stall this async task forever if the HX711 is not
    // wired/powered correctly or has not produced a sample yet.
    loadcell.set_offset(0);

    let mut sum: i64 = 0;
    let mut samples = 0usize;
    let mut elapsed_ms = 0u64;
    let mut next_diagnostic_log_ms = 0u64;

    info!("loadcell tare started");

    while samples < LOADCELL_TARE_SAMPLES && elapsed_ms < LOADCELL_TARE_TIMEOUT_MS {
        match loadcell.read_raw_if_ready() {
            Some(reading) if is_saturated_hx711_reading(reading) => {
                if elapsed_ms >= next_diagnostic_log_ms {
                    warn!(
                        "HX711 saturated during tare: {}; check DT/SCK wiring, power, and common ground",
                        reading
                    );
                    next_diagnostic_log_ms = elapsed_ms + LOADCELL_NOT_READY_LOG_MS;
                }
            }
            Some(reading) => {
                sum += reading as i64;
                samples += 1;
                info!(
                    "loadcell tare sample {}/{}: {}",
                    samples, LOADCELL_TARE_SAMPLES, reading
                );
            }
            None => {
                if elapsed_ms >= next_diagnostic_log_ms {
                    warn!("HX711 not ready during tare; check DT/SCK wiring and power");
                    next_diagnostic_log_ms = elapsed_ms + LOADCELL_NOT_READY_LOG_MS;
                }
            }
        }

        if samples >= LOADCELL_TARE_SAMPLES {
            break;
        }

        Timer::after_millis(LOADCELL_POLL_MS).await;
        elapsed_ms += LOADCELL_POLL_MS;
    }

    if samples == 0 {
        loadcell.set_offset(0);
        warn!(
            "loadcell tare timed out after {} ms with no valid samples; continuing without tare",
            elapsed_ms
        );
        return;
    }

    let offset = (sum / samples as i64) as i32;
    loadcell.set_offset(offset);

    if samples < LOADCELL_TARE_SAMPLES {
        warn!(
            "loadcell tare used only {}/{} samples before timeout: offset={}",
            samples, LOADCELL_TARE_SAMPLES, offset
        );
    } else {
        info!("loadcell tare complete: offset={}", offset);
    }
}

/// Sweet-spot mini-game: the player must hold a target force (read from the
/// HX711 load cell) inside a moving zone for `SWEET_SPOT_HOLD_MS` to damage
/// the soup. Each completed hold jumps the zone to a new random position.
#[embassy_executor::task]
pub async fn loadcell_task(
    state: &'static StateCell<CriticalSectionRawMutex, AppState, 1>,
    mut loadcell: Hx711,
) {
    info!("loadcell task started");

    let rng = Rng::new();

    loop {
        // Wait until the game phase begins.
        let mut waiting_ms = 0u64;
        loop {
            if matches!(state.get().await, AppState::Game { .. }) {
                break;
            }

            waiting_ms += LOADCELL_POLL_MS;
            if waiting_ms >= LOADCELL_NOT_READY_LOG_MS {
                info!("loadcell task waiting for game state");
                waiting_ms = 0;
            }

            Timer::after_millis(LOADCELL_POLL_MS).await;
        }

        info!("loadcell task entered game state");

        // Tare the load cell with (hopefully) no load so that resting reads ~0.
        tare_loadcell(&mut loadcell).await;

        info!("loadcell tare finished; publishing first sweet spot");

        // Pick the first sweet-spot zone and publish it to the state.
        let center = random_sweet_spot_center(&rng);
        let mut ss_min = center - SWEET_SPOT_TOLERANCE;
        let mut ss_max = center + SWEET_SPOT_TOLERANCE;
        let mut progress: u32 = 0;

        info!(
            "sweet spot range: min={} max={} center={}",
            ss_min, ss_max, center
        );

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

        info!("loadcell entering live read loop");

        let mut not_ready_ms = 0u64;

        'game_loop: loop {
            if !matches!(state.get().await, AppState::Game { .. }) {
                break 'game_loop;
            }

            // Try to read — skip if the HX711 hasn't finished a conversion yet.
            let (raw, delta, reading) = match loadcell.read_force_debug() {
                Some((raw, delta, force)) => {
                    not_ready_ms = 0;
                    if is_saturated_hx711_reading(raw) {
                        warn!(
                            "HX711 saturated game raw reading: {}; check DT/SCK wiring, power, and common ground",
                            raw
                        );
                    }
                    (raw, delta, force)
                }
                None => {
                    not_ready_ms += LOADCELL_POLL_MS;
                    if not_ready_ms >= LOADCELL_NOT_READY_LOG_MS {
                        warn!("HX711 not ready during game read; check DT/SCK wiring and power");
                        not_ready_ms = 0;
                    }
                    Timer::after_millis(LOADCELL_POLL_MS).await;
                    continue;
                }
            };

            info!(
                "loadcell force={} raw={} delta={} offset={}",
                reading, raw, delta, loadcell.offset
            );

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

                info!(
                    "sweet spot held for {} ms at reading {}; soup takes {} damage; next range: min={} max={} center={}",
                    SWEET_SPOT_HOLD_MS, reading, SOUP_DAMAGE, ss_min, ss_max, new_center
                );

                state
                    .update(|s| match s {
                        AppState::Game {
                            soup_hp, player_hp, ..
                        } => AppState::Game {
                            soup_hp: soup_hp.saturating_sub(SOUP_DAMAGE),
                            player_hp: *player_hp,
                            soup_status: SoupStatus::Sad,
                            sweet_spot_min: ss_min,
                            sweet_spot_max: ss_max,
                            sweet_spot_progress: 0,
                            loadcell_reading: reading,
                        },
                        other => other.clone(),
                    })
                    .await;

                // Check for a win.
                if let AppState::Game {
                    soup_hp: 0,
                    player_hp,
                    loadcell_reading,
                    ..
                } = state.get().await
                {
                    info!(
                        "game ended: player won; result=soup_defeated soup_hp=0 player_hp={} final_loadcell_reading={}",
                        player_hp, loadcell_reading
                    );
                    state.set(AppState::EndScreen { player_won: true }).await;
                    break 'game_loop;
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
}
