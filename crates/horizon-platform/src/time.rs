//! `std::time`, with the page's clock on the web (`performance.now()` and
//! `Date.now()` through web-time: std's `Instant::now` panics there).
pub use std::time::{Duration, TryFromFloatSecsError};
#[cfg(not(target_family = "wasm"))]
pub use std::time::{Instant, SystemTime, SystemTimeError, UNIX_EPOCH};
#[cfg(target_family = "wasm")]
pub use web_time::{Instant, SystemTime, SystemTimeError, UNIX_EPOCH};

/// `time` as std's `SystemTime`, for crates that take that type (chrono):
/// itself on native targets; on the web, web-time's reading as std's type,
/// whose `now` panics there.
#[cfg(not(target_family = "wasm"))]
pub fn to_std(time: SystemTime) -> std::time::SystemTime {
    time
}
#[cfg(target_family = "wasm")]
pub fn to_std(time: SystemTime) -> std::time::SystemTime {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => std::time::UNIX_EPOCH + since,
        Err(before) => std::time::UNIX_EPOCH - before.duration(),
    }
}
