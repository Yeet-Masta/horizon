// One of the threaded web build's threads (web/README.md): the page's module, over the page's
// memory, running the work the game handed this worker (horizon_platform::thread::spawn_worker).
// The module's start (`main`) runs only on the page's thread.
import init, { horizon_thread_entry } from "./pkg/horizon-client.js";

// A thread started from this one: the page starts it (horizon.js), so it outlives this worker.
globalThis.horizonSpawnThread = (name, _module, _memory, work) => postMessage({ spawn: [name, work] });
// What goes over the page (a crash on the integrated server's thread, say): the page shows it.
globalThis.horizonStatus = (text, error) => postMessage({ status: [text, error] });

// The page's Blobs (horizon.js): the resources zip and the game folder's larger files, read as
// the game opens them (crates/horizon-platform/src/web/blobs.rs).
const reader = new FileReaderSync();
globalThis.horizonReadBlob = (id, start, end) =>
  new Uint8Array(reader.readAsArrayBuffer(globalThis.horizonBlobs[id].slice(start, end)));

self.onmessage = async ({ data: [module, memory, work, blobs] }) => {
  globalThis.horizonBlobs = blobs ?? [];
  await init({ module_or_path: module, memory });
  horizon_thread_entry(work);
  close();
};
