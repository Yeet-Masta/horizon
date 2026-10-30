# Horizon on the web

The game compiled to WebAssembly and drawn with WebGPU, in a browser tab. It is
the same `horizon-client` as the native game, built for
`wasm32-unknown-unknown` (the default) or `wasm64-unknown-unknown` (64-bit memory).
Like the native game, it plays on the resources prepared from **your own copy**
of Minecraft: Java Edition 26.3. The page never downloads game files.

## Build

```sh
# The wasm-bindgen CLI from the commit Cargo.lock pins (web/build.sh prints this command too):
cargo install wasm-bindgen-cli --git https://github.com/wasm-bindgen/wasm-bindgen --rev 39ae823bbc14c9d0a4a2869361483b5453a9b133 --locked --force
web/build.sh               # wasm32 with threads: nightly Rust and -Zbuild-std (rustup installs rust-src)
web/build.sh --wasm64      # wasm64: up to 8 GiB of memory instead of 4, 7-14% slower (memory64's bounds checks)
web/build.sh --no-threads  # one thread: for browsers or hosts where web/coi.js can't isolate the page
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
between the game in the tab and the server. Online-mode servers need a
Microsoft account signed in on the page (see Microsoft account below).

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

### Microsoft account

*Sign in with Microsoft* on the page signs in as the launcher does
(docs/microsoft-account.md): it shows a code to enter at microsoft.com/link,
then goes through Xbox Live and the Minecraft services, checks that the
account owns Java Edition and reads its profile. On Play the Minecraft token
is renewed when it has less than an hour left and the game plays as that
profile, so online-mode servers let it in (vanilla's join through the session
service). Without an account, or when the services can't be reached, the game
starts offline and says why.

Those services refuse requests from other sites' pages, so the requests go
through the relay set in Options: it opens TCP connections to them
(`ACCOUNT_HOSTS` in `crates/horizon-web-relay`, port 443 only;
`--no-accounts` turns that off) and the game speaks TLS with them itself
(rustls with RustCrypto's algorithms, `horizon_platform::web::https`), so the
relay carries ciphertext and never sees a token. The account's record (its
refresh and Minecraft tokens) stays in this browser's IndexedDB, encrypted
with a key the page makes and can't read back (a non-extractable WebCrypto
key), apart from the game folder: its export and *Delete saved worlds* leave
it alone. *Sign out* deletes it.

The sign-in is Horizon's Azure app, built in (`CLIENT_ID` in
`web_account.rs`). Another one can be set at build time
(`HORIZON_MSA_CLIENT_ID=… web/build.sh`) or on the page's URL
(`?env=HORIZON_MSA_CLIENT_ID=…`).

### Play with friends, browser to browser

A world played in the browser can be shared with friends in theirs. Pause,
open *Play with Friends* (the friends icon) and *Start Sharing*: the world gets
a code such as `K7QM-3XPD`. Friends type it on *Join a Friend* (Multiplayer),
with the same relay set, and their browser connects straight to yours over
WebRTC. The relay introduces the two pages (`?host`, `?join=CODE`); the
game's bytes go between the browsers, encrypted by WebRTC, and the host's
world is the LAN server friends play on.

The browsers find each other's addresses through STUN (Play with Friends'
servers, or `HORIZON_STUN_SERVERS`; `none` for the local network only). When
two networks both block direct connections, the friend's page asks the relay
to pass the game's bytes instead: both pages connect a pipe there
(`?pipe=TOKEN`), which the relay bounds with `--max-pipes` (32; 0 turns them
off). Those bytes are the game's own protocol, unencrypted on the relay as
on a LAN. A native Horizon can't join a browser's world, nor a browser a
native one's invite code.

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

## Checks

Upstream code calls std's file system, clocks, environment and threads, which fail, panic
or block in a browser, and that shows only when it runs. Two checks catch it
(`.github/workflows/web.yml` runs both on the `web` branch):

```sh
python3 tools/web_hazards.py              # the wasm32 build linted for those calls (web/clippy-web)
python3 tools/web_hazards.py --update     # accept the ones that run only natively (web/hazards.json)
python3 tools/web_smoke.py --resources .local/horizon-web-resources.zip   # web/dist in Chromium
```

The smoke test (Playwright for Python: `pip install playwright`) serves `web/dist`, checks
that the module loads, and with prepared resources opens a new world, waits for the player
to join and checks that the page keeps drawing. It runs Chromium in a window (`xvfb-run`
without a display): headless, SwiftShader's WebGPU device is lost. In CI it plays when the
repository has a `HORIZON_WEB_RESOURCES_URL` secret pointing at your own prepared zip,
which must stay private.

## Browsers

- WebGPU: Chrome and Edge 113+, Firefox 141+ on Windows, Safari 26+. Every build
  also uses WebAssembly SIMD and tail calls (`web/simd.toml`), which those have.
- The wasm64 build (`--wasm64`) also needs memory64: Chrome and Edge 133+,
  Firefox 134+.

- The threaded build (the default) needs a cross-origin isolated page: the
  server sends `Cross-Origin-Opener-Policy: same-origin` and
  `Cross-Origin-Embedder-Policy: require-corp`. `web/serve.py` does, and so does
  `web/apache.htaccess` with `mod_headers`. A host that can't send headers
  (GitHub Pages, most static hosts) gets them from `coi.js`, a service worker
  the page registers when it isn't isolated: it adds the headers to the site's
  own responses, and the page reloads once. Service workers need HTTPS (or
  `localhost`); without one, use `--no-threads`.

The page checks all of these and says what is missing.

## What differs from the native game

The web build is the native game with the operating system swapped out
(`crates/horizon-platform`):

- **Files**: an in-memory file system. The resources zip is mounted read-only at
  `/app/resources` and read in place; the game folder (`/home/horizon`: options,
  saves, screenshots) is kept in the origin's private file system (OPFS; a
  first visit there moves what IndexedDB kept), else in IndexedDB, written back
  every two seconds and as the page is hidden. Options → *Game data while
  playing* chooses where the resources zip and the game folder's files of
  64 KiB or more (region files, mostly) are while the game runs: *in memory*
  (the default), read as the game starts, or on the threaded build *in
  browser storage*, read as the game opens them, which takes less memory and
  starts slower (thousands of small reads): workers read them with
  `FileReaderSync`, and the page's thread asks a worker that does
  (`horizon-platform`'s `web/blobs.rs`). Options → *Download the game folder*
  exports it as a zip.
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
  runs the world in the frame instead. The resource loader, the world loader
  and the other loads run on workers, so their loading screens keep drawing;
  cloud meshes are built on one too. The page's thread may not
  wait on a lock, so it spins on the locks it shares with the workers
  (`horizon_platform::sync`); the workers wait on them. The `--no-threads` build has no workers: chunk
  work runs between frames, most urgent first, for 8 ms a frame (more while
  the level loads; `HORIZON_WEB_WORK_MS` sets it), and a job that wouldn't fit
  what is left of a frame waits for the next. Region files are written as
  they are saved, and single player runs in the frame.
- **Graphics**: WebGPU only. The device opens before the game starts. WebGPU
  has no multi-draw yet (gpuweb#5175), so each section's draw is a call of its
  own; where Chrome offers its experimental one
  (`chromium-experimental-multi-draw-indirect`, behind
  chrome://flags/#enable-webgpu-developer-features), the vendored wgpu draws a
  run of sections with one call. Block
  textures are split across texture arrays at the adapter's layer limit.
  Occlusion culling runs on the render thread. GPU occlusion culling tests the
  boxes of sections that may be hidden in the shader, as on Metal (WebGPU
  can't force the depth test before it); on a tile-based GPU (Apple, ARM,
  Qualcomm) it starts from 1,600 sections in view. `?env=HORIZON_GPU_CULL=0`
  turns it off, to compare. Far terrain (LOD Distance, off by default as
  natively) samples and meshes on workers of its own on the threaded build,
  at most 2 sampling beside the mesher; the `--no-threads` build has none.
- **Sound**: WebAudio. On the threaded build a worker runs the game's mixer
  about 43 ms ahead into shared memory and an AudioWorklet plays it on the
  browser's audio thread, so a long frame neither delays nor crackles it; the
  `--no-threads` build mixes between frames (a ScriptProcessor). Sound starts
  at the first click or key press (browsers keep a page silent until then).
  Sounds decode where they start, so the first play of a long track stutters.
- **Network**: servers through WebSockets, and friends' browsers over WebRTC
  (see Multiplayer). Those connections run beside the page's event loop
  rather than on threads of their own (their sockets live on the page's
  thread); a shared world's writers look for packets every few milliseconds.
  Single player's connection through memory runs on workers, as natively.
- **Input**: the pointer lock asks for raw movement (`unadjustedMovement`,
  no system pointer acceleration, as natively) where the browser has it
  (`?env=HORIZON_WEB_RAW_MOUSE=0` keeps the plain lock). In fullscreen the
  page locks the keyboard (Chrome and Edge), so Escape pauses without leaving
  fullscreen (hold it to leave) and the browser's shortcuts (Ctrl+W...) reach
  the game. Once the game runs, the browser asks before the tab closes.
- **Files and the clipboard**: text the game copies goes to the system
  clipboard, and Ctrl+V pastes from it: a page reads the clipboard only in a
  paste event, so the page holds the key back from the game until the event
  has handed the text over. A screenshot (F2) is downloaded as well as kept in
  the game folder, and its link in chat downloads it again. Web links open in
  a new tab. A world's zip (with its `level.dat`) or a resource pack's zip
  dropped on the game goes into `saves` or `resourcepacks`.
- **Off**: controllers, mods with code (content-only mods still load), LAN discovery
  and Open to LAN (no ports to open), chat signing (as natively), skin
  downloads and telemetry.
