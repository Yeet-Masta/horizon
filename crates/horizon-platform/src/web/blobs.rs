//! Bytes the page keeps as Blobs (`globalThis.horizonBlobs` in web/horizon.js,
//! the same list on every worker): on the threaded build, the resources zip and
//! the game folder's larger files, read where and when the game reads them
//! rather than copied into memory as it starts (vfs.rs). A worker reads a Blob
//! with FileReaderSync (web/worker.js); the page's thread, which has none and
//! may not wait, asks the storage reader worker and spins until it answers, as
//! it does on a lock.
use crate::sync::Mutex;
use js_sys::Uint8Array;
use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc, Condvar,
        atomic::{AtomicBool, Ordering},
    },
};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    /// Bytes `start..end` of the page's Blob `id` (worker.js).
    #[wasm_bindgen(catch, js_namespace = globalThis, js_name = horizonReadBlob)]
    fn horizon_read_blob(id: u32, start: f64, end: f64) -> Result<Uint8Array, JsValue>;
}

type Reply = Arc<Mutex<Option<io::Result<Vec<u8>>>>>;
struct Request {
    id: u32,
    start: u64,
    end: u64,
    reply: Reply,
}
static REQUESTS: Mutex<VecDeque<Request>> = Mutex::new(VecDeque::new());
static WAKE: Condvar = Condvar::new();
static READY: AtomicBool = AtomicBool::new(false);

fn read_here(id: u32, start: u64, end: u64) -> io::Result<Vec<u8>> {
    horizon_read_blob(id, start as f64, end as f64)
        .map(|bytes| bytes.to_vec())
        .map_err(|e| io::Error::other(format!("the browser's storage could not be read: {e:?}")))
}

/// Bytes `start..end` of Blob `id`.
pub(crate) fn read(id: u32, start: u64, end: u64) -> io::Result<Vec<u8>> {
    if !super::on_page_thread() {
        return read_here(id, start, end);
    }
    if !READY.load(Ordering::Acquire) {
        return Err(io::Error::other("the storage reader hasn't started"));
    }
    let reply = Reply::default();
    REQUESTS.lock().unwrap().push_back(Request {
        id,
        start,
        end,
        reply: reply.clone(),
    });
    WAKE.notify_one();
    loop {
        if let Some(read) = reply.lock().unwrap().take() {
            return read;
        }
        std::hint::spin_loop();
    }
}

/// Starts the worker that reads for the page's thread, and returns once it
/// runs: a worker starts only while the page's event loop turns, so the page
/// must not wait for one it needs.
pub(crate) async fn start_reader() {
    if READY.load(Ordering::Acquire) {
        return;
    }
    let started = crate::thread::spawn_worker("Storage reader", || {
        READY.store(true, Ordering::Release);
        loop {
            let request = {
                let mut requests = REQUESTS.lock().unwrap();
                loop {
                    if let Some(request) = requests.pop_front() {
                        break request;
                    }
                    requests = WAKE.wait(requests).unwrap();
                }
            };
            let read = read_here(request.id, request.start, request.end);
            *request.reply.lock().unwrap() = Some(read);
        }
    });
    if started.is_err() {
        return;
    }
    while !READY.load(Ordering::Acquire) {
        super::nap(5).await;
    }
}
