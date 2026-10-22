#!/usr/bin/env bash
# OpenSlate Mobile 构建+装机脚本。
# 前置：Rust android 交叉编译产物就位（见 android/README.md「构建链」一节），
#   cargo build -p openslate-mobile-ffi --release \
#     --target aarch64-linux-android --config <ndk-r21-config.toml>
#   cp target/aarch64-linux-android/release/libopenslate_mobile.so \
#     android/app/src/main/jniLibs/arm64-v8a/
# 用法: scripts/build-apk.sh [--release] [--install] [--launch]
#   默认 debug 包；--release 编 release 包（交给用户真机测试一律用 release——
#   debug 包 debuggable 未优化，真机体感卡顿）。release 用 debug 签名，
#   可与 debug 包互相覆盖安装不丢数据（见 app/build.gradle.kts）。
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO/android"
VARIANT=debug
INSTALL=0
LAUNCH=0
for arg in "$@"; do
  case "$arg" in
    --release) VARIANT=release ;;
    --install) INSTALL=1 ;;
    --launch)  LAUNCH=1 ;;
  esac
done
# pipefail：gradle 失败时管道整体非零 → 直接中止，绝不能把旧 APK 装到设备上。
timeout 900 ./gradlew ":app:assemble$(echo "$VARIANT" | sed 's/^./\U&/')" --console=plain 2>&1 | tr '\r' '\n' | grep -aE "BUILD|FAILURE|e: " || { echo "BUILD FAILED — abort"; exit 1; }

# ── .so 新鲜度守卫 ────────────────────────────────────────────────
# 本脚本不交叉编译 Rust（前置见 android-build.md [1/3]）。若源码比 jniLibs
# 里的 .so 新，打包出来的 APK 会「新 Kotlin 绑定 + 旧 .so」符号错配——
# 真机表现为 UniffiLib <clinit> UnsatisfiedLinkError、runtime 永远「启动中」
# （2026-10-21 实际踩过）。此处硬失败并给出重建命令，防止旧件上架。
SO_FILE="$REPO/android/app/src/main/jniLibs/arm64-v8a/libopenslate_mobile.so"
NEWEST_SRC=$(find "$REPO/crates" "$REPO/vendor/genai/src" "$REPO/Cargo.toml" \
    \( -name '*.rs' -o -name 'Cargo.toml' \) -newer "$SO_FILE" 2>/dev/null | head -1)
if [ -n "$NEWEST_SRC" ]; then
    echo "STALE .so: Rust sources newer than jniLibs artifact (e.g. $NEWEST_SRC)"
    echo "Rebuild first (see docs/guide/07-build-dev/android-build.md [1/3]):"
    echo "  NDK=~/android-sdk/ndk/21.4.7075529/toolchains/llvm/prebuilt/linux-x86_64/bin"
    echo "  CARGO_TARGET_DIR=target-android CC_aarch64_linux_android=\$NDK/aarch64-linux-android26-clang \\"
    echo "  AR_aarch64_linux_android=\$NDK/llvm-ar CXX_aarch64_linux_android=\$NDK/aarch64-linux-android26-clang++ \\"
    echo "  cargo build --target aarch64-linux-android --profile mobile -p openslate-mobile-ffi --config ~/.cargo-android/config.toml"
    echo "  cp target-android/aarch64-linux-android/mobile/libopenslate_mobile.so android/app/src/main/jniLibs/arm64-v8a/"
    exit 1
fi
APK="$REPO/android/app/build/outputs/apk/$VARIANT/app-$VARIANT.apk"
[ -f "$APK" ] || { echo "APK missing"; exit 1; }
ls -la "$APK"
if [[ "$INSTALL" == 1 ]]; then
  adb install -r "$APK" | tail -1
fi
if [[ "$LAUNCH" == 1 ]]; then
  adb logcat -c
  adb shell am force-stop dev.openslate.mobile
  adb shell am start -n dev.openslate.mobile/.MainActivity | tail -1
fi
