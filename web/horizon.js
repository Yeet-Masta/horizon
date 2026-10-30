// The web build's page (web/README.md): checks the browser, takes the player's prepared
// resources (a zip or a folder, kept in IndexedDB for the next visit), keeps the game
// folder (options, saves) in the origin's private file system (else IndexedDB), and starts
// the game.
//
// The game calls three functions here (crates/horizon-platform/src/web/mod.rs):
//   horizonBoot()           -> Promise<{ args, env, resources | resourcesBlob, resourceFiles, files }>
//   horizonPersist(records) -> keeps [path, "file"|"dir"|"removed", bytes, mtime] records
//   horizonStatus(text, error)
// and sets globalThis.horizonFlush, called here as the page is hidden. On the threaded build
// the resources zip and the game folder's larger files stay Blobs (horizonBlobs, given to every
// worker), which the game reads as it opens them (blobs.rs).

const DB_NAME = "horizon";
const DB_VERSION = 2;
const FILES = "files"; // the game folder: path -> { kind, data, mtime }
const RESOURCES = "resources"; // "zip" -> { name, blob } or "folder" -> { name, files: [[path, blob]] }
// The Microsoft account: "key" -> a non-extractable AES-GCM CryptoKey, "account" -> { name, uuid,
// iv, data }, data being the account's record (its tokens) encrypted with it. Apart from the game
// folder, so neither its export nor "Delete saved worlds" touches it.
const ACCOUNTS = "accounts";

const $ = (id) => document.getElementById(id);
const shell = $("shell");
const statusLine = $("status");
const overlay = $("overlay");

function setStatus(text, error = false, busy = false) {
  statusLine.textContent = "";
  if (busy) {
    const spin = document.createElement("span");
    spin.className = "spinner";
    statusLine.append(spin);
  }
  statusLine.append(text);
  statusLine.classList.toggle("error", error);
}

// ---- IndexedDB ---------------------------------------------------------------------------

let dbPromise;
function db() {
  dbPromise ??= new Promise((resolve, reject) => {
    const open = indexedDB.open(DB_NAME, DB_VERSION);
    open.onupgradeneeded = () => {
      for (const name of [FILES, RESOURCES, ACCOUNTS]) {
        if (!open.result.objectStoreNames.contains(name)) open.result.createObjectStore(name);
      }
    };
    open.onsuccess = () => resolve(open.result);
    open.onerror = () => reject(open.error);
  });
  return dbPromise;
}
function request(r) {
  return new Promise((resolve, reject) => {
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}
async function store(name, mode = "readonly") {
  return (await db()).transaction(name, mode).objectStore(name);
}
async function done(tx) {
  return new Promise((resolve, reject) => {
    tx.oncomplete = resolve;
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error);
  });
}

async function storedResources() {
  const s = await store(RESOURCES);
  return (await request(s.get("zip"))) ?? (await request(s.get("folder"))) ?? null;
}
async function keepResources(entry) {
  const tx = (await db()).transaction(RESOURCES, "readwrite");
  const s = tx.objectStore(RESOURCES);
  s.clear();
  s.put(entry, entry.files ? "folder" : "zip");
  await done(tx);
}
async function forgetResources() {
  const tx = (await db()).transaction(RESOURCES, "readwrite");
  tx.objectStore(RESOURCES).clear();
  await done(tx);
}
// ---- The game folder's store ---------------------------------------------------------------

// The origin's private file system (OPFS), a file per file, where the browser writes to it
// from the page (createWritable); else IndexedDB, a record per path.
const opfs = !!navigator.storage?.getDirectory && "createWritable" in (globalThis.FileSystemFileHandle?.prototype ?? {});
const parts = (path) => path.split("/").filter(Boolean);
async function opfsDir(names, create) {
  let dir = await navigator.storage.getDirectory();
  for (const name of names) dir = await dir.getDirectoryHandle(name, { create });
  return dir;
}
async function opfsWrite(path, data) {
  const names = parts(path);
  const name = names.pop();
  const dir = await opfsDir(names, true);
  const out = await (await dir.getFileHandle(name, { create: true })).createWritable();
  await out.write(data);
  await out.close();
}
async function opfsRemove(path) {
  const names = parts(path);
  const name = names.pop();
  try {
    await (await opfsDir(names, false)).removeEntry(name, { recursive: true });
  } catch {}
}
// Every [path, kind, File] under `dir`.
async function opfsWalk(dir, path, out) {
  for await (const [name, handle] of dir.entries()) {
    const at = `${path}/${name}`;
    if (handle.kind === "directory") {
      out.push([at, "dir", null]);
      await opfsWalk(handle, at, out);
    } else {
      out.push([at, "file", await handle.getFile()]);
    }
  }
  return out;
}
// The first visit with the private file system moves what IndexedDB kept into it.
let moved = null;
function moveToOpfs() {
  moved ??= (async () => {
    const root = await navigator.storage.getDirectory();
    const kept = await idbFiles();
    if (!kept.length) return;
    for await (const _ of root.keys()) return; // already moved
    for (const [path, kind, data] of kept) {
      if (kind === "dir") await opfsDir(parts(path), true);
      else if (kind === "file") await opfsWrite(path, data ?? new Uint8Array());
    }
    const tx = (await db()).transaction(FILES, "readwrite");
    tx.objectStore(FILES).clear();
    await done(tx);
  })().catch((e) => console.error("Horizon: the game folder stays in IndexedDB:", e));
  return moved;
}
async function idbFiles() {
  const s = await store(FILES);
  const [keys, values] = await Promise.all([request(s.getAllKeys()), request(s.getAll())]);
  return keys.map((path, i) => [path, values[i].kind, values[i].data ?? null, values[i].mtime ?? 0]);
}

// The Blobs the threaded build's game reads as it opens them, by number: the same list goes to
// every worker (horizonSpawnThread), so all are added before the game starts.
globalThis.horizonBlobs = [];
const blobId = (blob) => globalThis.horizonBlobs.push(blob) - 1;
// Files this size or larger stay Blobs on the threaded build: worlds' region files, mostly.
// Smaller ones (options, level.dat), read on the page's thread, come in memory.
const LAZY = 64 << 10;

// The kept game folder as the game takes it: [path, kind, bytes, mtime], or with `lazy`, for a
// larger file, [path, "blob", id, mtime, size].
async function gameFiles(lazy = false) {
  let kept;
  if (opfs) {
    await moveToOpfs();
    kept = (await opfsWalk(await navigator.storage.getDirectory(), "", []))
      .map(([path, kind, file]) => [path, kind, file, file?.lastModified ?? 0]);
  } else {
    kept = await idbFiles();
  }
  return Promise.all(kept.map(async ([path, kind, data, mtime]) => {
    if (kind !== "file" || !(data instanceof Blob)) return [path, kind, data, mtime];
    if (lazy && data.size >= LAZY) return [path, "blob", blobId(data), mtime, data.size];
    return [path, kind, new Uint8Array(await data.arrayBuffer()), mtime];
  }));
}

// What the game wrote since the last call, in order; writes queue so none overtakes another.
let persisting = Promise.resolve();
globalThis.horizonPersist = (records) => {
  persisting = persisting.then(async () => {
    if (opfs) {
      await moveToOpfs();
      for (const [path, kind, data] of records) {
        if (kind === "removed") await opfsRemove(path);
        else if (kind === "dir") await opfsDir(parts(path), true);
        else await opfsWrite(path, data);
      }
      return;
    }
    const tx = (await db()).transaction(FILES, "readwrite");
    const s = tx.objectStore(FILES);
    for (const [path, kind, data, mtime] of records) {
      if (kind === "removed") s.delete(path);
      // A Blob: the next visit's threaded game reads it as it opens it.
      else s.put({ kind, data: kind === "file" ? new Blob([data]) : null, mtime }, path);
    }
    await done(tx);
  }).catch((e) => console.error("Horizon: could not keep the game folder:", e));
};
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "hidden") globalThis.horizonFlush?.();
});
addEventListener("pagehide", () => globalThis.horizonFlush?.());

// ---- What the game says --------------------------------------------------------------------

globalThis.horizonStatus = (text, error) => {
  overlay.hidden = false;
  overlay.textContent = text;
  overlay.classList.toggle("error", !!error);
  if (error) console.error(text);
};

// ---- The browser ---------------------------------------------------------------------------

// The smallest module with a 64-bit memory: memory64 (the wasm64 build) needs it.
const MEMORY64 = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0, 5, 3, 1, 4, 1]);
// A module with a function returning a v128 (i8x16.popcnt of a splat): valid where SIMD is.
const SIMD = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 0, 1, 123, 3, 2, 1, 0, 10, 10, 1, 8, 0,
  65, 0, 253, 15, 253, 98, 11]);
const build = await fetch("build.json").then((r) => r.json()).catch(() => ({ target: "wasm64-unknown-unknown" }));
const wasm64 = build.target.startsWith("wasm64");

// A host that can't send the isolation headers (a static one: GitHub Pages...) gets them from a
// service worker (coi.js) that adds them to this site's responses. Registered only when the page
// isn't isolated, it serves the page once it controls it: the page reloads, once per tab.
if (build.threads && !globalThis.crossOriginIsolated && globalThis.isSecureContext && "serviceWorker" in navigator) {
  let tried = true;
  try {
    tried = sessionStorage.getItem("horizon-coi") === "1";
    sessionStorage.setItem("horizon-coi", "1");
  } catch {}
  try {
    await navigator.serviceWorker.register(new URL("coi.js", import.meta.url));
    if (!tried) {
      await navigator.serviceWorker.ready;
      location.reload();
      await new Promise(() => {});
    }
  } catch (e) {
    console.warn("Horizon: the isolation service worker did not start:", e?.message ?? e);
  }
} else if (globalThis.crossOriginIsolated) {
  try { sessionStorage.removeItem("horizon-coi"); } catch {}
}

// The threaded build's threads (web/README.md): Web Workers that instantiate the page's module
// over its shared memory, each running the work the game hands it. A thread a worker starts is
// started here too (worker.js forwards it): a worker's own workers would end with it.
let threadModule, threadMemory;
globalThis.horizonSpawnThread = (name, module, memory, work) => {
  threadModule = module;
  threadMemory = memory;
  const worker = new Worker(new URL("worker.js", import.meta.url), { type: "module", name });
  worker.onerror = (e) => console.error(`Horizon: ${name} stopped:`, e.message ?? e);
  worker.onmessage = ({ data }) => {
    if (data?.spawn) globalThis.horizonSpawnThread(data.spawn[0], threadModule, threadMemory, data.spawn[1]);
    if (data?.status) globalThis.horizonStatus(...data.status);
  };
  worker.postMessage([module, memory, work, globalThis.horizonBlobs]);
};

async function checkBrowser() {
  const checks = [];
  let adapter = null;
  if (navigator.gpu) {
    try {
      adapter = await navigator.gpu.requestAdapter({ powerPreference: "high-performance" });
    } catch {}
  }
  checks.push(adapter
    ? [true, `WebGPU: ${adapter.info?.vendor || "adapter"} ${adapter.info?.architecture || ""}`.trim()]
    : [false, navigator.gpu
      ? "WebGPU is here but gives no adapter: turn on hardware acceleration."
      : "No WebGPU: use Chrome or Edge 113+, Firefox 141+ (Windows) or Safari 26+."]);
  if (wasm64) {
    const ok = WebAssembly.validate(MEMORY64);
    checks.push([ok, ok ? "64-bit WebAssembly (memory64)"
      : "This build is wasm64 and needs memory64 (Chrome 133+, Firefox 134+). Use the wasm32 build otherwise."]);
  }
  if (build.simd && !WebAssembly.validate(SIMD)) {
    checks.push([false, "This build needs WebAssembly SIMD (Chrome 91+, Firefox 89+, Safari 16.4+)."]);
  }
  // Shared memory exists only on a cross-origin isolated page: the server sends COOP and COEP.
  if (build.threads) {
    const ok = globalThis.crossOriginIsolated === true;
    checks.push([ok, ok ? `Threads: ${navigator.hardwareConcurrency || "?"} cores`
      : "This build uses threads, which need the page cross-origin isolated: serve it with the headers "
        + "Cross-Origin-Opener-Policy: same-origin and Cross-Origin-Embedder-Policy: require-corp "
        + "(web/serve.py and web/apache.htaccess do; elsewhere coi.js adds them on an HTTPS page), "
        + "or build with --no-threads."]);
  }
  const list = $("checks");
  list.replaceChildren(...checks.map(([ok, text]) => {
    const li = document.createElement("li");
    li.className = ok ? "ok" : "bad";
    const dot = document.createElement("span");
    dot.className = "dot";
    li.append(dot, text);
    return li;
  }));
  return checks.every(([ok]) => ok);
}

// ---- Choosing the resources ------------------------------------------------------------------

let chosen = null; // { name, blob } or { name, files: [[path, blob]] }

function showChosen() {
  const text = $("drop-text");
  if (!chosen) return;
  const what = chosen.files ? `${chosen.files.length} files` : `${(chosen.blob.size / 1048576).toFixed(0)} MB`;
  text.innerHTML = "";
  const ok = document.createElement("span");
  ok.className = "stored";
  ok.textContent = `Resources ready: ${chosen.name} (${what})`;
  text.append(ok, document.createElement("br"), "Drop or pick another to replace them.");
  $("forget").hidden = false;
}

async function choose(entry) {
  chosen = entry;
  showChosen();
  setStatus("Keeping the resources in this browser…", false, true);
  try {
    await keepResources(entry);
    setStatus("");
  } catch (e) {
    setStatus(`The resources are ready, but this browser could not keep them for next time (${e}).`);
  }
  updatePlay();
}

$("pick-zip").addEventListener("change", (e) => {
  const file = e.target.files[0];
  if (file) choose({ name: file.name, blob: file });
});
$("pick-folder").addEventListener("change", (e) => {
  const files = [...e.target.files];
  if (!files.length) return;
  // Paths inside the folder that holds horizon-resources.json.
  const marker = files.find((f) => f.webkitRelativePath.endsWith("horizon-resources.json"));
  const prefix = marker ? marker.webkitRelativePath.slice(0, -"horizon-resources.json".length) : "";
  const inside = files
    .filter((f) => f.webkitRelativePath.startsWith(prefix))
    .map((f) => [f.webkitRelativePath.slice(prefix.length), f]);
  choose({ name: files[0].webkitRelativePath.split("/")[0], files: inside });
});
const drop = $("drop");
drop.addEventListener("dragover", (e) => { e.preventDefault(); drop.classList.add("over"); });
drop.addEventListener("dragleave", () => drop.classList.remove("over"));
drop.addEventListener("drop", (e) => {
  e.preventDefault();
  drop.classList.remove("over");
  const file = [...e.dataTransfer.files].find((f) => f.name.endsWith(".zip"));
  if (file) choose({ name: file.name, blob: file });
  else setStatus("Drop a .zip of the prepared resources (or use Choose folder…).", true);
});
$("forget").addEventListener("click", async () => {
  await forgetResources();
  chosen = null;
  location.reload();
});

// ---- The Microsoft account ---------------------------------------------------------------------

// The game's module (its sign-in functions, web_account.rs), once it has loaded.
let game = null;
// HORIZON_* variables from the URL: ?env=HORIZON_WEB_WORK_MS=12,HORIZON_SPIKES=30
function urlEnv() {
  const env = {};
  for (const pair of (new URLSearchParams(location.search).get("env") ?? "").split(",").filter(Boolean)) {
    const at = pair.indexOf("=");
    if (at > 0) env[pair.slice(0, at)] = pair.slice(at + 1);
  }
  return env;
}
// Horizon's Azure app: the URL's, else the one the build has (empty).
const msaClientId = () => urlEnv().HORIZON_MSA_CLIENT_ID ?? "";

async function accountPut(value, key) {
  const tx = (await db()).transaction(ACCOUNTS, "readwrite");
  if (value === undefined) tx.objectStore(ACCOUNTS).delete(key);
  else tx.objectStore(ACCOUNTS).put(value, key);
  await done(tx);
}
async function accountGet(key) {
  return request((await store(ACCOUNTS)).get(key));
}
// The key the record is encrypted with: made once, and never readable by a script.
async function accountKey() {
  let key = await accountGet("key");
  if (!key) {
    key = await crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, ["encrypt", "decrypt"]);
    await accountPut(key, "key");
  }
  return key;
}
async function saveAccount(record) {
  const key = await accountKey();
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const data = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, new TextEncoder().encode(record));
  const { name, uuid } = JSON.parse(record);
  await accountPut({ name, uuid, iv, data }, "account");
  showAccount(name);
}
async function loadAccount() {
  const entry = await accountGet("account");
  if (!entry) return null;
  const plain = await crypto.subtle.decrypt({ name: "AES-GCM", iv: entry.iv }, await accountKey(), entry.data);
  return new TextDecoder().decode(plain);
}
function showAccount(name) {
  const text = $("account-text");
  text.replaceChildren();
  if (name) {
    const strong = document.createElement("strong");
    strong.textContent = name;
    text.append("Signed in as ", strong, " (Microsoft account): online-mode servers let you in.");
  } else {
    text.append("Microsoft account: not signed in. Single player and offline-mode servers need none; online-mode servers do.");
  }
  $("sign-in").hidden = !!name;
  $("sign-out").hidden = !name;
}

let signing = null; // the AbortController of a sign-in under way
$("sign-in").addEventListener("click", async () => {
  if (!game || signing) return;
  const relay = $("relay").value.trim();
  if (!relay) {
    $("relay").closest("details").open = true;
    setStatus("Signing in needs a WebSocket relay (Options): a page can't reach Microsoft's sign-in by itself.", true);
    return;
  }
  localStorageSet("horizon.relay", relay);
  signing = new AbortController();
  $("sign-in").disabled = true;
  setStatus("Asking Microsoft for a sign-in code…", false, true);
  try {
    const record = await game.horizon_account_sign_in(relay, msaClientId(), (code, uri) => {
      $("user-code").textContent = code;
      $("sign-in-link").href = uri;
      $("sign-in-link").textContent = uri.replace(/^https?:\/\/(www\.)?/, "");
      $("sign-in-code").hidden = false;
      setStatus("Waiting for you to sign in at Microsoft…", false, true);
    }, signing.signal);
    await saveAccount(record);
    setStatus("");
  } catch (e) {
    setStatus(String(e?.message ?? e), true);
  } finally {
    signing = null;
    $("sign-in-code").hidden = true;
    $("sign-in").disabled = false;
  }
});
$("cancel-sign-in").addEventListener("click", () => signing?.abort());
$("copy-code").addEventListener("click", () => navigator.clipboard?.writeText($("user-code").textContent));
$("sign-out").addEventListener("click", async () => {
  await accountPut(undefined, "account");
  showAccount(null);
  setStatus("Signed out: this browser no longer keeps the account. To revoke Horizon's access too, see account.live.com/consent/Manage.");
});
accountGet("account").then((entry) => showAccount(entry?.name ?? null)).catch(() => {});

// On Play: the account's tokens renewed as needed, and its session handed to the game.
async function startSession(relay) {
  let record;
  try {
    record = await loadAccount();
  } catch (e) {
    console.warn("Horizon: the saved account can't be read:", e);
    return;
  }
  if (!record || !game) return;
  setStatus("Signing in to the Minecraft services…", false, true);
  try {
    await saveAccount(await game.horizon_account_session(relay, msaClientId(), record));
  } catch (e) {
    console.warn(`Horizon: playing offline: ${e?.message ?? e}`);
    globalThis.horizonStatus?.(`Playing offline: ${e?.message ?? e}`, false);
    setTimeout(() => { overlay.hidden = true; }, 8000);
  }
}

// ---- The game folder ---------------------------------------------------------------------------

$("wipe").addEventListener("click", async () => {
  if (!confirm("Delete every world, option and setting this browser keeps for Horizon?")) return;
  const tx = (await db()).transaction(FILES, "readwrite");
  tx.objectStore(FILES).clear();
  await done(tx);
  if (opfs) {
    const root = await navigator.storage.getDirectory();
    for (const name of await Array.fromAsync(root.keys())) await root.removeEntry(name, { recursive: true });
  }
  setStatus("The game folder is empty.");
});
$("export").addEventListener("click", async () => {
  const files = (await gameFiles()).filter(([, kind]) => kind === "file");
  const home = "/home/horizon/";
  const entries = files
    .filter(([path]) => path.startsWith(home))
    .map(([path, , data, mtime]) => [path.slice(home.length), data, mtime]);
  const blob = zipStored(entries);
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = "horizon-game-folder.zip";
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 10000);
});

// A zip of stored (uncompressed) entries.
function zipStored(entries) {
  const table = new Uint32Array(256).map((_, n) => {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c >>> 0;
  });
  const crc32 = (bytes) => {
    let c = 0xffffffff;
    for (const b of bytes) c = table[(c ^ b) & 0xff] ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  };
  const enc = new TextEncoder();
  const parts = [], central = [];
  let offset = 0;
  for (const [name, data] of entries) {
    const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
    const nameBytes = enc.encode(name);
    const crc = crc32(bytes);
    const local = new DataView(new ArrayBuffer(30));
    local.setUint32(0, 0x04034b50, true); local.setUint16(4, 20, true); local.setUint16(6, 0x0800, true);
    local.setUint32(14, crc, true); local.setUint32(18, bytes.length, true); local.setUint32(22, bytes.length, true);
    local.setUint16(26, nameBytes.length, true);
    parts.push(local, nameBytes, bytes);
    const dir = new DataView(new ArrayBuffer(46));
    dir.setUint32(0, 0x02014b50, true); dir.setUint16(4, 20, true); dir.setUint16(6, 20, true); dir.setUint16(8, 0x0800, true);
    dir.setUint32(16, crc, true); dir.setUint32(20, bytes.length, true); dir.setUint32(24, bytes.length, true);
    dir.setUint16(28, nameBytes.length, true); dir.setUint32(42, offset, true);
    central.push(dir, nameBytes);
    offset += 30 + nameBytes.length + bytes.length;
  }
  const size = central.reduce((n, p) => n + p.byteLength, 0);
  const end = new DataView(new ArrayBuffer(22));
  end.setUint32(0, 0x06054b50, true); end.setUint16(8, entries.length, true); end.setUint16(10, entries.length, true);
  end.setUint32(12, size, true); end.setUint32(16, offset, true);
  return new Blob([...parts, ...central, end], { type: "application/zip" });
}

// ---- Starting ------------------------------------------------------------------------------------

let browserOk = false;
function updatePlay() {
  $("play").disabled = !(browserOk && chosen);
}

const params = new URLSearchParams(location.search);
$("args").value = params.get("args") ?? localStorageGet("horizon.args") ?? "";
$("relay").value = params.get("relay") ?? localStorageGet("horizon.relay") ?? "";
function localStorageGet(key) {
  try { return localStorage.getItem(key); } catch { return null; }
}
function localStorageSet(key, value) {
  try { localStorage.setItem(key, value); } catch {}
}

let started;
const startPromise = new Promise((resolve) => (started = resolve));

// What the game receives once the player presses Play.
globalThis.horizonBoot = async () => {
  await startPromise;
  setStatus("Reading the resources…", false, true);
  const boot = { args: [], env: {}, resources: null, resourceFiles: null, files: [] };
  const args = $("args").value.trim();
  localStorageSet("horizon.args", args);
  boot.args = args ? args.split(/\s+/) : [];
  // The WebSocket relay that reaches servers named by host:port (web/README.md).
  const relay = $("relay").value.trim();
  localStorageSet("horizon.relay", relay);
  if (relay) boot.env.HORIZON_WEB_RELAY = relay;
  Object.assign(boot.env, urlEnv());
  // The signed-in account's session, before the game reads its options.
  if (relay) await startSession(relay);
  if (chosen.files) {
    boot.resourceFiles = await Promise.all(
      chosen.files.map(async ([path, blob]) => [path, new Uint8Array(await blob.arrayBuffer())]));
  } else if (build.threads) {
    // Read in place: the game's workers read the entries they open.
    boot.resourcesBlob = blobId(chosen.blob);
    boot.resourcesSize = chosen.blob.size;
  } else {
    boot.resources = new Uint8Array(await chosen.blob.arrayBuffer());
  }
  boot.files = await gameFiles(!!build.threads);
  setStatus("Starting the game…", false, true);
  shell.hidden = true;
  $("horizon").focus();
  guardUnload();
  return boot;
};

// ---- Input ---------------------------------------------------------------------------------------

// Raw mouse movement, as the native game reads it: the pointer lock asks for unadjustedMovement,
// which skips the system's pointer acceleration, and falls back to the plain lock where the
// browser has no such thing. ?env=HORIZON_WEB_RAW_MOUSE=0 keeps the plain lock.
const requestPointerLock = Element.prototype.requestPointerLock;
if (requestPointerLock && urlEnv().HORIZON_WEB_RAW_MOUSE !== "0") {
  Element.prototype.requestPointerLock = function (options) {
    const plain = () => requestPointerLock.call(this, options);
    let asked;
    try {
      asked = requestPointerLock.call(this, { ...options, unadjustedMovement: true });
    } catch {
      return plain();
    }
    return asked?.catch?.((e) => (e?.name === "NotSupportedError" ? plain() : Promise.reject(e))) ?? asked;
  };
}

// Keyboard Lock in fullscreen: the browser's own shortcuts (Ctrl+W, Ctrl+T, Ctrl+N...) and Escape
// reach the game as in a native window, so Escape pauses without leaving fullscreen. Holding
// Escape still leaves it.
document.addEventListener("fullscreenchange", () => {
  if (!navigator.keyboard?.lock) return;
  if (document.fullscreenElement) navigator.keyboard.lock().catch(() => {});
  else navigator.keyboard.unlock();
});

// A message over the game for a few seconds.
let noticeTimer = 0;
function notice(text, error) {
  globalThis.horizonStatus(text, error);
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => { overlay.hidden = true; }, 6000);
}

// ---- Files, links and the clipboard (platform's web/mod.rs) ------------------------------------

// A file for the player to keep (a screenshot, F2): a download.
globalThis.horizonSaveFile = (name, bytes) => {
  const type = name.endsWith(".png") ? "image/png" : "application/octet-stream";
  const a = document.createElement("a");
  a.href = URL.createObjectURL(new Blob([bytes], { type }));
  a.download = name;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 60_000);
};
// A link the player confirmed (chat, books, server links): a new tab, web links only.
globalThis.horizonOpenUrl = (url) => {
  try {
    if (["http:", "https:"].includes(new URL(url).protocol)) window.open(url, "_blank", "noopener");
  } catch {}
};
// Text the game copies, to the system clipboard where the browser lets the page write it.
globalThis.horizonClipboardWrite = (text) => {
  navigator.clipboard?.writeText(text).catch(() => {});
};
// A page reads the clipboard only in a paste event, which the game's own handling of Ctrl+V
// (⌘V) would prevent: the key is held back from the game until the paste event has handed the
// text over (or 100 ms passed, when the browser sends none), then given to it.
let pasting = null;
window.addEventListener("keydown", (e) => {
  if (!e.isTrusted || !game || e.target !== $("horizon")) return;
  if (pasting) pasting(null);
  const v = e.key === "v" || e.key === "V" || e.code === "KeyV";
  if (!v || !(e.ctrlKey || e.metaKey) || e.altKey || e.repeat) return;
  e.stopImmediatePropagation();
  const again = new KeyboardEvent(e.type, e);
  const timer = setTimeout(() => pasting?.(null), 100);
  pasting = (text) => {
    pasting = null;
    clearTimeout(timer);
    if (text != null) game.horizon_clipboard_paste(text);
    $("horizon").dispatchEvent(again);
  };
}, { capture: true });
// The held key goes to the game before it is let go.
window.addEventListener("keyup", (e) => { if (e.isTrusted && pasting) pasting(null); }, { capture: true });
document.addEventListener("paste", (e) => {
  if (!pasting) return;
  e.preventDefault();
  pasting(e.clipboardData?.getData("text/plain") ?? "");
});

// A world's or a resource pack's zip dropped on the game goes into its folder (web_files.rs).
window.addEventListener("dragover", (e) => {
  if (!shell.hidden || !game) return;
  e.preventDefault();
  e.dataTransfer.dropEffect = "copy";
});
window.addEventListener("drop", async (e) => {
  if (!shell.hidden || !game) return;
  e.preventDefault();
  for (const file of e.dataTransfer.files) {
    try {
      notice(game.horizon_import(file.name, new Uint8Array(await file.arrayBuffer())), false);
    } catch (error) {
      notice(String(error?.message ?? error), true);
    }
  }
});

// Once the game runs, the browser asks before the tab closes or leaves: what was played since
// the last save would go with it.
const guardUnload = () => window.addEventListener("beforeunload", (e) => {
  e.preventDefault();
  e.returnValue = "";
});

$("play").addEventListener("click", () => {
  if (navigator.storage?.persist) navigator.storage.persist().catch(() => {});
  setStatus("Starting…", false, true);
  started();
});

// `url`'s body, with its progress on the status line.
async function download(url) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: ${response.status} ${response.statusText}`);
  const total = Number(response.headers.get("Content-Length")) || 0;
  const reader = response.body.getReader();
  const parts = [];
  let got = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    parts.push(value);
    got += value.length;
    setStatus(total
      ? `Downloading the resources… ${Math.floor((got * 100) / total)}%`
      : `Downloading the resources… ${got >> 20} MB`, false, true);
  }
  return new Blob(parts, { type: "application/zip" });
}

// The resources kept from last time. Else the zip the URL names (?resources=local-resources.zip,
// on a server of your own: web/README.md), or on localhost the zip beside the page (web/serve.py
// serves the one web/pack-resources.sh made, to this machine only).
let resourceProblem = null;
async function findResources() {
  try {
    chosen = await storedResources();
  } catch (e) {
    console.warn("Horizon: no IndexedDB:", e);
  }
  const named = params.get("resources");
  if (!chosen && named) {
    try {
      await choose({ name: named.split("/").pop(), blob: await download(named) });
    } catch (e) {
      resourceProblem = `Could not download the resources (${e.message}).`;
    }
  } else if (!chosen && ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname)) {
    const local = await fetch("local-resources.zip", { method: "HEAD" }).catch(() => null);
    if (local?.ok) {
      await choose({ name: "local-resources.zip", blob: await download("local-resources.zip") });
    }
  }
  showChosen();
}

setStatus("Checking this browser…", false, true);
const [ok] = await Promise.all([checkBrowser(), findResources()]);
browserOk = ok;
updatePlay();
if (!ok) setStatus("This browser can't run Horizon's web build.", true);
else setStatus(resourceProblem ?? "", !!resourceProblem);

// The game itself: its JS glue and module, built by web/build.sh into pkg/. Instantiating
// it runs `main`, which waits in horizonBoot until Play.
try {
  const glue = await import("./pkg/horizon-client.js");
  const ready = () => {
    game = glue;
    $("sign-in").disabled = false;
  };
  try {
    await glue.default({ module_or_path: new URL("pkg/horizon-client_bg.wasm", import.meta.url) });
  } finally {
    ready();
  }
} catch (e) {
  // winit's web event loop throws to hand control back to the page: not an error.
  if (!String(e).includes("Using exceptions for control flow")) {
    console.error(e);
    shell.hidden = false;
    setStatus(`The game could not start: ${e}`, true);
  }
}
