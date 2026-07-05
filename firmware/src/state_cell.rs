use embassy_sync::{
    blocking_mutex::raw::RawMutex,
    mutex::Mutex,
    watch::{Receiver, Watch},
};

/// A single-value cell with closure-based atomic updates and async change
/// notifications for multiple subscribers.
///
/// Combines an async [`Mutex`] for atomic read-modify-write with a [`Watch`]
/// for multi-subscriber notifications. Updates done through [`StateCell::update`]
/// are atomic with respect to other updates: the closure sees the current value
/// and the new value is published to subscribers before any other task can
/// interleave a competing update.
///
/// `T` must be [`Clone`] because the value is both stored and broadcast to
/// subscribers.
///
/// Typically declared as a `static` and shared between tasks.
pub struct StateCell<M, T, const N: usize>
where
    M: RawMutex,
    T: Clone,
{
    value: Mutex<M, T>,
    watch: Watch<M, T, N>,
}

impl<M, T, const N: usize> StateCell<M, T, N>
where
    M: RawMutex,
    T: Clone,
{
    /// Create a new `StateCell` storing `value`.
    pub const fn new(value: T) -> Self {
        Self {
            value: Mutex::new(value),
            watch: Watch::new(),
        }
    }

    /// Atomically read the current value, compute a new value via `f`, store it,
    /// and notify all subscribers.
    ///
    /// The mutex is held across the closure invocation and the notification, so
    /// concurrent updates are totally ordered: subscribers always see the value
    /// committed by the most recent `update`.
    pub async fn update<F>(&self, f: F)
    where
        F: FnOnce(&T) -> T,
    {
        let mut guard = self.value.lock().await;
        let new = f(&*guard);
        *guard = new.clone();
        self.watch.sender().send(new);
    }

    /// Replace the stored value with `value` and notify subscribers. Equivalent
    /// to `update(|_| value)` but skips one of the clones that `update` needs.
    pub async fn set(&self, value: T) {
        let mut guard = self.value.lock().await;
        *guard = value.clone();
        self.watch.sender().send(value);
    }

    /// Get a snapshot of the current value.
    pub async fn get(&self) -> T {
        let guard = self.value.lock().await;
        (*guard).clone()
    }

    /// Try to obtain a subscriber. Returns `None` if the maximum number of
    /// subscribers (`N`) has already been reached.
    pub fn subscriber(&self) -> Option<Receiver<'_, M, T, N>> {
        self.watch.receiver()
    }
}
