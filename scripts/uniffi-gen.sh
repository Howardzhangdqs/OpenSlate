#!/usr/bin/env bash
# 生成 UniFFI Kotlin 绑定（proc-macro source 模式）→ android/app/src/main/java。
# 仅当 openslate-mobile-ffi 的 UniFFI 面变更时需要重跑。
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
BIND_OUT=$(mktemp -d)
cargo run -p openslate-mobile-ffi --features bindgen --bin uniffi-bindgen -- generate \
    crates/openslate-mobile-ffi/src/lib.rs \
    --language kotlin \
    --out-dir "$BIND_OUT" \
    --config crates/openslate-mobile-ffi/uniffi.toml
DEST="$REPO/android/app/src/main/java/uniffi/openslate/mobile"
mkdir -p "$DEST"
cp "$BIND_OUT"/*.kt "$DEST/"
rm -rf "$BIND_OUT"
ls -la "$DEST"
