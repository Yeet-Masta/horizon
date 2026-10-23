//! `std::process::id`: on the web build, where std's panics, the page's
//! game is process 1.
#[cfg(not(target_family = "wasm"))]
pub use std::process::id;

#[cfg(target_family = "wasm")]
pub fn id() -> u32 {
    1
}
