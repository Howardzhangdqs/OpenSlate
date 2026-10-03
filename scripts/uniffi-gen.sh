#!/usr/bin/env bash
# 生成 UniFFI Kotlin 绑定 → android/app/src/main/java。
# 仅当 openslate-mobile-ffi 的 UniFFI 面变更时需要重跑。
#
# uniffi 0.32 的 CLI 已移除 source 模式（直接传 src/lib.rs 会报
# "Unknown library format"），必须走 library 模式：先构建 cdylib，
# 再对 .so 产物执行 bindgen。
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

cargo build -p openslate-mobile-ffi

BIND_OUT=$(mktemp -d)
trap 'rm -rf "$BIND_OUT"' EXIT
cargo run -q -p openslate-mobile-ffi --features bindgen --bin uniffi-bindgen -- generate \
    --library target/debug/libopenslate_mobile.so \
    --language kotlin \
    --out-dir "$BIND_OUT" \
    --config crates/openslate-mobile-ffi/uniffi.toml

DEST="$REPO/android/app/src/main/java/uniffi/openslate/mobile"
mkdir -p "$DEST"
cp "$BIND_OUT"/uniffi/openslate/mobile/*.kt "$DEST/"
ls -la "$DEST"
