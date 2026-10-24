// One of the threaded web build's threads (web/README.md): the page's module, over the page's
// memory, running the work the game handed this worker (horizon_platform::thread::spawn_worker).
// The module's start (`main`) runs only on the page's thread.
import init, { horizon_thread_entry } from "./pkg/horizon-client.js";

self.onmessage = async ({ data: [module, memory, work] }) => {
  await init({ module_or_path: module, memory });
  horizon_thread_entry(work);
  close();
};
