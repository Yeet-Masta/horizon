// The web build's page (web/README.md): checks the browser, takes the player's prepared
// resources (a zip or a folder, kept in IndexedDB for the next visit), keeps the game
// folder (options, saves) in IndexedDB, and starts the game.
//
// The game calls three functions here (crates/horizon-platform/src/web/mod.rs):
//   horizonBoot()           -> Promise<{ args, env, resources, resourceFiles, files }>
//   horizonPersist(records) -> keeps [path, "file"|"dir"|"removed", bytes, mtime] records
//   horizonStatus(text, error)
// and sets globalThis.horizonFlush, called here as the page is hidden.

const DB_NAME = "horizon";
const DB_VERSION = 1;
const FILES = "files"; // the game folder: path -> { kind, data, mtime }
const RESOURCES = "resources"; // "zip" -> { name, blob } or "folder" -> { name, files: [[path, blob]] }

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
      open.result.createObjectStore(FILES);
      open.result.createObjectStore(RESOURCES);
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
async function gameFiles() {
  const s = await store(FILES);
  const [keys, values] = await Promise.all([request(s.getAllKeys()), request(s.getAll())]);
  return keys.map((path, i) => [path, values[i].kind, values[i].data ?? null, values[i].mtime ?? 0]);
}

// What the game wrote since the last call, in order; writes queue so none overtakes another.
let persisting = Promise.resolve();
globalThis.horizonPersist = (records) => {
  persisting = persisting.then(async () => {
    const tx = (await db()).transaction(FILES, "readwrite");
    const s = tx.objectStore(FILES);
    for (const [path, kind, data, mtime] of records) {
      if (kind === "removed") s.delete(path);
      else s.put({ kind, data: kind === "file" ? data : null, mtime }, path);
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
const build = await fetch("build.json").then((r) => r.json()).catch(() => ({ target: "wasm64-unknown-unknown" }));
const wasm64 = build.target.startsWith("wasm64");

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

// ---- The game folder ---------------------------------------------------------------------------

$("wipe").addEventListener("click", async () => {
  if (!confirm("Delete every world, option and setting this browser keeps for Horizon?")) return;
  const tx = (await db()).transaction(FILES, "readwrite");
  tx.objectStore(FILES).clear();
  await done(tx);
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
  // HORIZON_* variables from the URL: ?env=HORIZON_WEB_WORK_MS=12,HORIZON_SPIKES=30
  for (const pair of (params.get("env") ?? "").split(",").filter(Boolean)) {
    const at = pair.indexOf("=");
    if (at > 0) boot.env[pair.slice(0, at)] = pair.slice(at + 1);
  }
  if (chosen.files) {
    boot.resourceFiles = await Promise.all(
      chosen.files.map(async ([path, blob]) => [path, new Uint8Array(await blob.arrayBuffer())]));
  } else {
    boot.resources = new Uint8Array(await chosen.blob.arrayBuffer());
  }
  boot.files = await gameFiles();
  setStatus("Starting the game…", false, true);
  shell.hidden = true;
  $("horizon").focus();
  return boot;
};

$("play").addEventListener("click", () => {
  if (navigator.storage?.persist) navigator.storage.persist().catch(() => {});
  setStatus("Starting…", false, true);
  started();
});

// The resources kept from last time, else the zip the local server offers (web/serve.py
// serves the one web/pack-resources.sh made, to this machine only).
async function findResources() {
  try {
    chosen = await storedResources();
  } catch (e) {
    console.warn("Horizon: no IndexedDB:", e);
  }
  if (!chosen && ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname)) {
    const local = await fetch("local-resources.zip").catch(() => null);
    if (local?.ok) {
      const blob = await local.blob();
      await choose({ name: "local-resources.zip (from web/serve.py)", blob });
    }
  }
  showChosen();
}

setStatus("Checking this browser…", false, true);
const [ok] = await Promise.all([checkBrowser(), findResources()]);
browserOk = ok;
updatePlay();
setStatus(ok ? "" : "This browser can't run Horizon's web build.", !ok);

// The game itself: its JS glue and module, built by web/build.sh into pkg/. Instantiating
// it runs `main`, which waits in horizonBoot until Play.
try {
  const glue = await import("./pkg/horizon-client.js");
  await glue.default({ module_or_path: new URL("pkg/horizon-client_bg.wasm", import.meta.url) });
} catch (e) {
  // winit's web event loop throws to hand control back to the page: not an error.
  if (!String(e).includes("Using exceptions for control flow")) {
    console.error(e);
    shell.hidden = false;
    setStatus(`The game could not start: ${e}`, true);
  }
}
