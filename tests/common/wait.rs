//! Polling helper to avoid fixed sleeps (keeps CI stable).

use std::time::{Duration, Instant};

/// Poll `pred` every `interval` until it returns true or `timeout` elapses.
/// Returns `true` on success; the caller asserts on the result.
pub fn wait_for<F: FnMut() -> bool>(mut pred: F, timeout: Duration, interval: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(interval);
    }
}
