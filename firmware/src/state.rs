/// Visual mood of the soup, selected by the display task based on game state.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SoupStatus {
    Angry,
    Sad,
    Neutral,
}

/// Top-level state machine shared between all tasks via the `STATE` cell.
#[derive(Clone)]
pub enum AppState {
    Start,
    Rules,
    Game {
        soup_hp: u32,
        player_hp: u32,
        soup_status: SoupStatus,
        /// Lower bound of the current sweet spot zone (raw loadcell reading).
        sweet_spot_min: i32,
        /// Upper bound of the current sweet spot zone (raw loadcell reading).
        sweet_spot_max: i32,
        /// How long the player has held the force inside the zone, in ms
        /// (0..=`SWEET_SPOT_HOLD_MS`). Reset to 0 when the zone moves or the
        /// player leaves the zone.
        sweet_spot_progress: u32,
        /// Latest raw loadcell reading (post-tare). The UI can use this to
        /// render a force meter / marker.
        loadcell_reading: i32,
    },
    EndScreen {
        player_won: bool,
    },
}
