#!/usr/bin/env bash
# OpenSlate Mobile — Rust .so 交叉编译 + UniFFI Kotlin 绑定 + jniLibs 落位。
#
# 本机网络适配说明：
# - NDK r21 直连（cargo-ndk 4.x 要求 r23+，绕行：lld + 空 libunwind 兜底，
#   真正的 unwinder 由 rust std 的 libunwind-*.rlib 提供）。
# - 绑定走 library 模式（host 编译 cdylib 后提取元数据）。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NDKB="$HOME/android-sdk/ndk/21.4.7075529/toolchains/llvm/prebuilt/linux-x86_64/bin"
FFI_CRATE="openslate-mobile-ffi"
PROFILE="mobile"

echo "==> [1/3] 交叉编译 $FFI_CRATE (arm64-v8a, $PROFILE)"
cd "$ROOT"
CARGO_TARGET_DIR=target-android \
CC_aarch64_linux_android="$NDKB/aarch64-linux-android26-clang" \
AR_aarch64_linux_android="$NDKB/llvm-ar" \
CXX_aarch64_linux_android="$NDKB/aarch64-linux-android26-clang++" \
cargo build --target aarch64-linux-android --profile "$PROFILE" -p "$FFI_CRATE" \
  --config "$HOME/.cargo-android/config.toml"

SO="target-android/aarch64-linux-android/$PROFILE/libopenslate_mobile.so"
mkdir -p "$ROOT/android/app/src/main/jniLibs/arm64-v8a"
cp "$SO" "$ROOT/android/app/src/main/jniLibs/arm64-v8a/"
echo "    产物: $SO ($(du -h "$SO" | cut -f1))"

echo "==> [2/3] 生成 UniFFI Kotlin 绑定（library 模式）"
cargo build -p "$FFI_CRATE"
BIND_OUT="$(mktemp -d)"
cargo run -p "$FFI_CRATE" --features bindgen --bin uniffi-bindgen -- generate \
    --library "$ROOT/target/debug/libopenslate_mobile.so" \
    --language kotlin \
    --out-dir "$BIND_OUT" \
    --config "$ROOT/crates/$FFI_CRATE/uniffi.toml"

echo "==> [3/3] 绑定落位"
cp -r "$BIND_OUT/uniffi" "$ROOT/android/app/src/main/java/"
rm -rf "$BIND_OUT"

echo "✅ 完成。下一步: cd android && ./gradlew :app:assembleDebug"
