use crate::{GatewayError, Result};
use std::{
    sync::{Mutex, OnceLock},
    time::Instant,
};
use tokio::sync::Notify;

pub fn monotonic_ms() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

/// A bounded owner (journal/media/queue) owns the work. Dropping a waiter does
/// not cancel accepted input or start a second capture/request on retry.
pub(crate) struct Flight<T> {
    pub value: Mutex<Option<Result<T>>>,
    changed: Notify,
}
impl<T: Clone> Flight<T> {
    pub fn new() -> Self {
        Self {
            value: Mutex::new(None),
            changed: Notify::new(),
        }
    }
    pub fn finish(&self, value: Result<T>) {
        let mut state = self.value.lock().unwrap();
        if state.is_none() {
            *state = Some(value);
        }
        drop(state);
        self.changed.notify_waiters();
    }
    pub async fn wait(&self) -> Result<T> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(value) = self.value.lock().unwrap().as_ref() {
                return value.clone();
            }
            changed.await;
        }
    }
}

impl<T> Drop for Flight<T> {
    fn drop(&mut self) {
        // No task is kept alive by the notification primitive itself.
        self.changed.notify_waiters();
    }
}

pub(crate) fn unknown() -> GatewayError {
    GatewayError::message(
        "EXECUTION_UNKNOWN",
        "Operation failed; observe before retrying",
    )
}
