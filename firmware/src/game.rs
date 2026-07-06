use embassy_executor;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Timer;
use log::info;

use crate::state::{AppState, SoupStatus};
use crate::state_cell::StateCell;

/// Drives the top-level game flow: hold the start screen once, then repeat
/// `Rules -> Game -> EndScreen -> Rules`. The button and load-cell tasks enter
/// the end screen; this task starts each new round after the rules screen.
#[embassy_executor::task]
pub async fn logic_task(state: &'static StateCell<CriticalSectionRawMutex, AppState, 1>) {
    info!("Start screen");
    state.set(AppState::Start).await;
    Timer::after_secs(5).await;
    loop {
        info!("rules screen");
        state.set(AppState::Rules).await;
        Timer::after_secs(5).await;

        info!("Game screen");
        state
            .set(AppState::Game {
                soup_hp: 100,
                player_hp: 100,
                soup_status: SoupStatus::Neutral,
                sweet_spot_min: 0,
                sweet_spot_max: 0,
                sweet_spot_progress: 0,
                loadcell_reading: 0,
            })
            .await;

        loop {
            if matches!(state.get().await, AppState::EndScreen { .. }) {
                info!("end screen shown; returning to rules screen in 5 seconds");
                Timer::after_secs(5).await;
                break;
            }

            Timer::after_millis(100).await;
        }
    }
}
