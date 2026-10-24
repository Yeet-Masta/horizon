#!/usr/bin/env bash
# Builds the web version of the game into web/dist (web/README.md).
#
#   web/build.sh                 wasm64 (memory64) with threads, the default
#   web/build.sh --wasm32        wasm32: for browsers without memory64 (Safari)
#   web/build.sh --no-threads    one thread: no shared memory, so no COOP/COEP headers needed
#   web/build.sh --release       the release profile (thin LTO; slow to build) instead of `fast`
#
# Threads and wasm64 build std here, with nightly Rust (rustup adds rust-src). Needs the
# wasm-bindgen CLI from the commit Cargo.lock pins (the error says the command); wasm-opt
# (binaryen) is used when it is on the PATH. Serve the result with web/serve.py.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$here")"
target=wasm64-unknown-unknown
profile=fast
threads=1
for arg in "$@"; do
  case "$arg" in
    --wasm32) target=wasm32-unknown-unknown ;;
    --wasm64) target=wasm64-unknown-unknown ;;
    --no-threads) threads=0 ;;
    --threads) threads=1 ;;
    --release) profile=release ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

cd "$root"
# wasm-bindgen comes from its main branch for now (Cargo.toml's [patch.crates-io]): the CLI
# must be built from the same commit.
pinned="$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)"
rev="$(awk '/^name = "wasm-bindgen"$/ { getline; getline; if (match($0, /#[0-9a-f]+/)) print substr($0, RSTART + 1, RLENGTH - 1); exit }' Cargo.lock)"
have="$(wasm-bindgen --version 2>/dev/null | awk '{ print $2 }' || true)"
install="cargo install wasm-bindgen-cli --version $pinned --locked"
[ -n "$rev" ] && install="cargo install wasm-bindgen-cli --git https://github.com/wasm-bindgen/wasm-bindgen --rev $rev --locked --force"
if [ "$have" != "$pinned" ]; then
  echo "wasm-bindgen $pinned${rev:+ (wasm-bindgen main, $rev)} is needed (found: ${have:-none}):" >&2
  echo "  $install" >&2
  exit 1
fi

# No sound, controllers or mod runtime in a browser (web/README.md).
args=(-p horizon-client --bin horizon-client --no-default-features --target "$target" --profile "$profile")
if [ "$threads" = 1 ] || [ "$target" = wasm64-unknown-unknown ]; then
  # wasm64 is a tier 3 target, and threads need std built with atomics: std is built here,
  # with the nightly toolchain. web/threads.toml adds shared memory and atomics.
  rustup component add rust-src --toolchain nightly >/dev/null
  rustup target add "$target" --toolchain nightly >/dev/null 2>&1 || true
  [ "$threads" = 1 ] && args+=(--config web/threads.toml)
  cargo +nightly build -Zbuild-std=std,panic_abort "${args[@]}"
else
  rustup target add wasm32-unknown-unknown >/dev/null
  cargo build "${args[@]}"
fi

dist="$here/dist"
rm -rf "$dist"
mkdir -p "$dist/pkg" "$dist/fonts"
wasm="${CARGO_TARGET_DIR:-target}/$target/$profile/horizon-client.wasm"
if ! wasm-bindgen --target web --no-typescript --out-dir "$dist/pkg" "$wasm"; then
  echo "If wasm-bindgen says the module's bindgen format differs, install the pinned one:" >&2
  echo "  $install" >&2
  exit 1
fi
if command -v wasm-opt >/dev/null; then
  flags=(-O2 --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals --enable-reference-types --enable-multivalue)
  [ "$target" = wasm64-unknown-unknown ] && flags+=(--enable-memory64)
  [ "$threads" = 1 ] && flags+=(--enable-threads)
  wasm-opt "${flags[@]}" "$dist/pkg/horizon-client_bg.wasm" -o "$dist/pkg/horizon-client_bg.wasm"
else
  echo "wasm-opt not found: the module is not optimized further (install binaryen to shrink it)" >&2
fi

cp "$here/index.html" "$here/horizon.js" "$here/worker.js" "$dist/"
cp "$here/apache.htaccess" "$dist/.htaccess"
cp website/public/favicon.svg "$dist/"
cp website/public/fonts/Geist-Regular.woff2 website/public/fonts/Geist-Medium.woff2 \
  website/public/fonts/PixelifySans-SemiBold.woff2 website/public/fonts/OFL-*.txt "$dist/fonts/"
commit="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
threads_json=false
[ "$threads" = 1 ] && threads_json=true
printf '{"target":"%s","profile":"%s","threads":%s,"commit":"%s"}\n' "$target" "$profile" "$threads_json" "$commit" > "$dist/build.json"
echo "web/dist: $(du -sh "$dist/pkg/horizon-client_bg.wasm" | cut -f1) module ($target, $profile, $([ "$threads" = 1 ] && echo threads || echo one thread))"
