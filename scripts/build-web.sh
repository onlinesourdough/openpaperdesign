#!/usr/bin/env sh
# Build the browser bundle into target/openpaperdesign-web/ (served by openpaperdesign-serve).
# Needs the wasm32-unknown-unknown target and wasm-bindgen CLI 0.2.129 (same version as the
# wasm-bindgen crate in Cargo.lock). Extra arguments go to cargo, e.g. --offline.
set -eu
cd "$(dirname "$0")/.."
cargo build --locked --release -p openpaperdesign-web --target wasm32-unknown-unknown "$@"
out=target/openpaperdesign-web
mkdir -p "$out"
wasm-bindgen --target web --no-typescript --out-dir "$out" \
    target/wasm32-unknown-unknown/release/openpaperdesign_web.wasm
cp crates/openpaperdesign-web/static/index.html crates/openpaperdesign-web/static/app.js \
    crates/openpaperdesign-web/static/session.js \
    crates/openpaperdesign-web/static/page.js \
    crates/openpaperdesign-web/static/editor.js \
    crates/openpaperdesign-web/static/panel.js "$out/"
ls -l "$out"
