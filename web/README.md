# Horizon on the web

The game compiled to WebAssembly and drawn with WebGPU, in a browser tab. It is
the same `horizon-client` as the native game, built for
`wasm64-unknown-unknown` (64-bit memory, the default) or `wasm32-unknown-unknown`.
Like the native game, it plays on the resources prepared from **your own copy**
of Minecraft: Java Edition 26.3. The page never downloads game files.

## Build

```sh
cargo install wasm-bindgen-cli --version 0.2.129 --locked   # the version Cargo.lock pins
web/build.sh             # wasm64: nightly Rust and -Zbuild-std (rustup installs rust-src)
web/build.sh --wasm32    # wasm32 on stable Rust, for browsers without memory64 (Safari)
web/build.sh --release   # thin LTO instead of the quicker `fast` profile
```

On Windows, `web\build.bat` takes the same options (`--wasm32`, `--release`).

The page and module land in `web/dist/`. When `wasm-opt` (binaryen) is on the
`PATH` the module is optimized further.

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

## Browsers

- WebGPU: Chrome and Edge 113+, Firefox 141+ on Windows, Safari 26+.
- The wasm64 build also needs memory64: Chrome and Edge 133+, Firefox 134+. Safari
  needs the wasm32 build.

The page checks both and says what is missing. No cross-origin isolation is
needed: the game runs on the page's one thread.

## What differs from the native game

The web build is the native game with the operating system swapped out
(`crates/horizon-platform`):

- **Files**: an in-memory file system. The resources zip is mounted read-only at
  `/app/resources` and read in place; the game folder (`/home/horizon`: options,
  saves, screenshots) is kept in IndexedDB, written back every two seconds and as
  the page is hidden. Options → *Download the game folder* exports it as a zip.
- **Threads**: none. Chunk generation, lighting and meshing run between frames,
  most urgent first, for 8 ms a frame (more while the level loads;
  `HORIZON_WEB_WORK_MS` sets it). Loaders run where they are started, so
  loading the resources holds the page for a while. Region files are written as
  they are saved.
- **Graphics**: WebGPU only. The device opens before the game starts. Block
  textures are split across texture arrays at the adapter's layer limit.
  Occlusion culling runs on the render thread. Far terrain (`lodDistance`) is off.
- **Sound**: WebAudio, mixed by the game's own mixer between frames; it starts
  at the first click or key press (browsers keep a page silent until then).
  Sounds decode where they start, so the first play of a long track stutters.
- **Network**: servers through WebSockets, and friends' browsers over WebRTC
  (see Multiplayer). Connections run beside the page's event loop rather than
  on threads of their own; a shared world's writers look for packets every
  few milliseconds.
- **Off**: controllers, mods with code (content-only mods still load),
  the system clipboard (copy and paste work inside the game), LAN discovery
  and Open to LAN (no ports to open), online-mode servers and account
  sign-in, skin downloads and telemetry.
