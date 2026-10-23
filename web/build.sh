#!/usr/bin/env bash
# Builds the web version of the game into web/dist (web/README.md).
#
#   web/build.sh                 wasm64 (memory64), the default: nightly Rust, -Zbuild-std
#   web/build.sh --wasm32        wasm32: stable Rust, for browsers without memory64 (Safari)
#   web/build.sh --release       the release profile (thin LTO; slow to build) instead of `fast`
#
# Needs wasm-bindgen-cli at the version Cargo.lock pins
# (cargo install wasm-bindgen-cli --version <that> --locked); wasm-opt (binaryen) is used when
# it is on the PATH. Serve the result with web/serve.py.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$here")"
target=wasm64-unknown-unknown
profile=fast
for arg in "$@"; do
  case "$arg" in
    --wasm32) target=wasm32-unknown-unknown ;;
    --wasm64) target=wasm64-unknown-unknown ;;
    --release) profile=release ;;
    -h|--help) sed -n '2,11p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

cd "$root"
pinned="$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)"
have="$(wasm-bindgen --version 2>/dev/null | awk '{ print $2 }' || true)"
if [ "$have" != "$pinned" ]; then
  echo "wasm-bindgen $pinned is needed (found: ${have:-none}):" >&2
  echo "  cargo install wasm-bindgen-cli --version $pinned --locked" >&2
  exit 1
fi

# No sound, controllers or mod runtime in a browser (web/README.md).
features=(-p horizon-client --bin horizon-client --no-default-features)
if [ "$target" = wasm64-unknown-unknown ]; then
  # wasm64 is a tier 3 target: std is built here, with the nightly toolchain.
  rustup component add rust-src --toolchain nightly >/dev/null
  cargo +nightly build -Zbuild-std=std,panic_abort --target "$target" --profile "$profile" "${features[@]}"
else
  rustup target add wasm32-unknown-unknown >/dev/null
  cargo build --target "$target" --profile "$profile" "${features[@]}"
fi

dist="$here/dist"
rm -rf "$dist"
mkdir -p "$dist/pkg" "$dist/fonts"
wasm="target/$target/$profile/horizon-client.wasm"
wasm-bindgen --target web --no-typescript --out-dir "$dist/pkg" "$wasm"
if command -v wasm-opt >/dev/null; then
  flags=(-O2 --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals --enable-reference-types --enable-multivalue)
  [ "$target" = wasm64-unknown-unknown ] && flags+=(--enable-memory64)
  wasm-opt "${flags[@]}" "$dist/pkg/horizon-client_bg.wasm" -o "$dist/pkg/horizon-client_bg.wasm"
else
  echo "wasm-opt not found: the module is not optimized further (install binaryen to shrink it)" >&2
fi

cp "$here/index.html" "$here/horizon.js" "$dist/"
cp website/public/favicon.svg "$dist/"
cp website/public/fonts/Geist-Regular.woff2 website/public/fonts/Geist-Medium.woff2 \
  website/public/fonts/PixelifySans-SemiBold.woff2 website/public/fonts/OFL-*.txt "$dist/fonts/"
commit="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
printf '{"target":"%s","profile":"%s","commit":"%s"}\n' "$target" "$profile" "$commit" > "$dist/build.json"
echo "web/dist: $(du -sh "$dist/pkg/horizon-client_bg.wasm" | cut -f1) module ($target, $profile)"
