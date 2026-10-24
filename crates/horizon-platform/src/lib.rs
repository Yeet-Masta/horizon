//! The operating system services the game reads through std: files, clocks,
//! the environment and threads. Native targets get std itself, re-exported;
//! the web build (wasm32- or wasm64-unknown-unknown, `web/README.md`) gets
//! the browser's: an in-memory file system holding the prepared resources
//! and the game folder (kept in IndexedDB by `web/horizon.js`), the page's
//! clock, and threads that run where they are spawned, since the game runs
//! on the page's one thread there.
//!
//! The rest of the workspace names these modules where it named std's
//! (`horizon_platform::fs::read`, `horizon_platform::time::Instant`), so a
//! native build is unchanged.
pub mod env;
pub mod fs;
pub mod process;
pub mod sync;
pub mod thread;
pub mod time;
#[cfg(target_family = "wasm")]
pub mod web;

/// True on the web build.
pub const WEB: bool = cfg!(target_family = "wasm");

/// `pollster::block_on` natively. On the web a future can only finish while
/// the page's event loop runs, so this polls once and fails if the future
/// is still waiting: only futures that are ready at once (the renderer's,
/// once `web::start` has opened the device) may be blocked on there.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    #[cfg(not(target_family = "wasm"))]
    {
        pollster::block_on(future)
    }
    #[cfg(target_family = "wasm")]
    {
        use std::task::{Context, Poll, Waker};
        let mut future = std::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("block_on: the future waits on the browser"),
        }
    }
}

/// [`block_on`], except that on the web a future still waiting after one
/// poll gives `pending`: a WebGPU error scope answers only once the page's
/// event loop turns, so the web build reads it as no error.
pub fn block_on_or<F: std::future::Future>(future: F, pending: F::Output) -> F::Output {
    #[cfg(not(target_family = "wasm"))]
    {
        let _ = pending;
        pollster::block_on(future)
    }
    #[cfg(target_family = "wasm")]
    {
        use std::task::{Context, Poll, Waker};
        let mut future = std::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(output) => output,
            Poll::Pending => pending,
        }
    }
}
