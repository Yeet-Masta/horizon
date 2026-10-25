# Horizon on the web

The game compiled to WebAssembly and drawn with WebGPU, in a browser tab. It is
the same `horizon-client` as the native game, built for
`wasm64-unknown-unknown` (64-bit memory, the default) or `wasm32-unknown-unknown`.
Like the native game, it plays on the resources prepared from **your own copy**
of Minecraft: Java Edition 26.3. The page never downloads game files.

## Build

```sh
# The wasm-bindgen CLI from the commit Cargo.lock pins (web/build.sh prints this command too):
cargo install wasm-bindgen-cli --git https://github.com/wasm-bindgen/wasm-bindgen --rev 39ae823bbc14c9d0a4a2869361483b5453a9b133 --locked --force
web/build.sh               # wasm64 with threads: nightly Rust and -Zbuild-std (rustup installs rust-src)
web/build.sh --wasm32      # wasm32, for browsers without memory64 (Safari)
web/build.sh --no-threads  # one thread: for servers that can't send the COOP/COEP headers
web/build.sh --fast        # the quicker-to-build `fast` profile instead of release (thin LTO)
```

On Windows, `web\build.bat` takes the same options.

wasm-bindgen comes from its main branch for now (`[patch.crates-io]` in the
root `Cargo.toml`): threads on memory64 aren't in a release yet. The CLI must
be built from the same commit as the crates; `web/build.sh` says so when it
isn't.

The page and module land in `web/dist/`. When `wasm-opt` (binaryen) is on the
`PATH` the module is optimized further. Every build uses WebAssembly SIMD
(`web/simd.toml`), which every browser with WebGPU has.

## Play

```sh
cargo run --release -p horizon-prepare -- --prepare --server-jar ~/Downloads/server.jar
web/pack-resources.sh    # .local/horizon-web-resources.zip (--no-sounds: smaller, silent)
python3 web/serve.py     # http://localhost:8080/
```

On `localhost`, `serve.py` hands the page that zip, and the page keeps it in
the browser's IndexedDB: later visits start without it. Anywhere else, drop the
zip on the page, or pick the prepared folder itself. A release folder's
`resources/` works as well as `.local/horizon-client/26.3/`.

### On a server of your own

Any static server works; `web/apache.htaccess` (copied into `web/dist` as
`.htaccess`) sets Apache up. Copy `web/dist` into the document root, put the
resources zip beside `index.html` as `local-resources.zip`, and open
`index.html?resources=local-resources.zip`: the page downloads it once and keeps
it. Browsers give WebGPU only to secure pages, so anywhere but `localhost` the
server needs HTTPS (a self-signed certificate does for testing). Keep such a
server to yourself: the `.htaccess` has a password block to uncomment.

Game arguments go in the page's Options (`--world Survival --seed 5 --creative`,
as on the command line). `?env=HORIZON_WEB_WORK_MS=12` sets `HORIZON_*` variables.

The prepared resources are derived from Minecraft: never put the zip, or a page
serving it, anywhere public.

## Multiplayer

A browser opens WebSockets but no TCP connections, so the web build reaches
Minecraft servers through a WebSocket. The Minecraft protocol stays end to end,
between the game in the tab and the server; only offline-mode servers can be
joined for now (no account sign-in on the web).

- A **`ws://` or `wss://` address** in the server list is joined directly: a
  server with a WebSocket listener, or any server behind a WebSocket-to-TCP
  bridge such as `websockify 8081 localhost:25565`.
- **Any other address** (`play.example.net:25565`) goes through the relay set in
  the page's Options (*WebSocket relay*, or `?relay=wss://…` in the URL), which
  opens the TCP connection for it.

`horizon-web-relay` is that relay. It reaches only the servers you name:

```sh
cargo run --release -p horizon-web-relay -- --target localhost:25565          # every page reaches one server
cargo run --release -p horizon-web-relay -- --allow play.example.net:25565    # pages name it (?target=)
cargo run --release -p horizon-web-relay -- --allow '*:25565'                 # any public server on 25565
```

It listens on `127.0.0.1:8081` (`--listen`), so on `localhost` the relay is
`ws://127.0.0.1:8081/`. A page served over HTTPS needs `wss://`: put the relay
behind the web server (`web/apache.htaccess` has the lines for Apache).
`--origin https://example.net` admits only your page. Why a server refused the
relay (or the relay refused the server) shows on the game's disconnect screen.

An address without a port may redirect by its SRV record
(`_minecraft._tcp.<host>`), as in the game: the relay connects where the
record says. A name you `--target` or `--allow` goes wherever its record
points; through `*` a record leads only to a public address on a port from
1024 up (anyone can write one).

A server's resource pack downloads in the page: straight from the pack's
server when it lets pages read it (CORS), else through the relay
(`?download=URL` over `http(s)://`, the relay's address; served by the same
web server lines as the WebSocket). The relay downloads only from public
addresses, at most 250 MiB, and does so when it reaches servers
(`--no-downloads` turns it off, `--downloads` on for a relay with rooms only).

### Play with friends, browser to browser

A world played in the browser can be shared with friends in theirs. Pause,
open *Play with Friends* (the friends icon) and *Start Sharing*: the world gets
a code such as `K7QM-3XPD`. Friends type it on *Join a Friend* (Multiplayer),
with the same relay set, and their browser connects straight to yours over
WebRTC. The relay only introduces the two pages (`?host`, `?join=CODE`); the
game's bytes go between the browsers, encrypted by WebRTC, and the host's
world is the LAN server friends play on.

The browsers find each other's addresses through STUN (Play with Friends'
servers, or `HORIZON_STUN_SERVERS`; `none` for the local network only). Two
networks that both block direct connections can't meet: there is no fallback
through the relay yet. A native Horizon can't join a browser's world, nor a
browser a native one's invite code.

The friend's terrain arrives at the speed of both games' frames: the host
sends chunks in batches each tick, and every batch waits for the guest's
answer. A host with a high render distance on a slow machine takes a while
to fill a friend's view.

### Finding stutters

`?env=HORIZON_SPIKES=25` logs every frame slower than 25 ms to the browser
console, with where it went: `streaming_results` (finished chunk work taken
in), `streaming_unload_generate`, `streaming_schedule` (new jobs), `upload`
(meshes to the GPU), `save_chunks`, `tick_sound_engine`, `tick_music`,
`renderer`, `tick` and the rest. Several can go together:
`?env=HORIZON_SPIKES=25,HORIZON_WEB_WORKERS=2`.

## Browsers

- WebGPU: Chrome and Edge 113+, Firefox 141+ on Windows, Safari 26+.
- The wasm64 build also needs memory64: Chrome and Edge 133+, Firefox 134+. Safari
  needs the wasm32 build.

- The threaded build (the default) needs a cross-origin isolated page: the
  server sends `Cross-Origin-Opener-Policy: same-origin` and
  `Cross-Origin-Embedder-Policy: require-corp`. `web/serve.py` does, and so does
  `web/apache.htaccess` with `mod_headers`. Anywhere else, use `--no-threads`.

The page checks all of these and says what is missing.

## What differs from the native game

The web build is the native game with the operating system swapped out
(`crates/horizon-platform`):

- **Files**: an in-memory file system. The resources zip is mounted read-only at
  `/app/resources` and read in place; the game folder (`/home/horizon`: options,
  saves, screenshots) is kept in IndexedDB, written back every two seconds and as
  the page is hidden. Options → *Download the game folder* exports it as a zip.
- **Threads**: chunk generation, lighting and meshing run on Web Workers that
  share the page's memory (`web/worker.js`), as the native game's workers do.
  A browser can't run them behind the page as the native game does, so there
  are half as many as cores, at most 3 (Horizon's graphics settings, Worker
  Threads, or `?env=HORIZON_WEB_WORKERS=2`, set it). A frame spends at most
  4 ms on finished jobs, and uploads at most 4 MiB of section meshes (the
  Upload Budget setting). Each thread keeps the small blocks it
  frees for its next allocations (std's wasm allocator is one spinning lock
  for every thread). Sounds decode on two more workers (effects, and
  streamed music and ambience), and region files are written on another, as
  natively. Single player runs on the integrated server's thread, a worker
  too, as natively, so a world tick never holds a frame up; a worker starts
  only once the page's event loop turns, so the window joins it through
  memory at once and the server answers when the world is open
  (`game/integrated.rs`). `--in-frame` (or `?env=HORIZON_SERVER_THREAD=0`)
  runs the world in the frame instead. Loaders run where they are started,
  so loading the resources holds the page for a while. The page's thread may not
  wait on a lock, so it spins on the locks it shares with the workers
  (`horizon_platform::sync`); the workers wait on them. The `--no-threads` build has no workers: chunk
  work runs between frames, most urgent first, for 8 ms a frame (more while
  the level loads; `HORIZON_WEB_WORK_MS` sets it), and a job that wouldn't fit
  what is left of a frame waits for the next. Region files are written as
  they are saved, and single player runs in the frame.
- **Graphics**: WebGPU only. The device opens before the game starts. Block
  textures are split across texture arrays at the adapter's layer limit.
  Occlusion culling runs on the render thread. Far terrain (`lodDistance`) is off.
- **Sound**: WebAudio, mixed by the game's own mixer between frames; it starts
  at the first click or key press (browsers keep a page silent until then).
  Sounds decode where they start, so the first play of a long track stutters.
- **Network**: servers through WebSockets, and friends' browsers over WebRTC
  (see Multiplayer). Those connections run beside the page's event loop
  rather than on threads of their own (their sockets live on the page's
  thread); a shared world's writers look for packets every few milliseconds.
  Single player's connection through memory runs on workers, as natively.
- **Off**: controllers, mods with code (content-only mods still load),
  the system clipboard (copy and paste work inside the game), LAN discovery
  and Open to LAN (no ports to open), online-mode servers and account
  sign-in, skin downloads and telemetry.
