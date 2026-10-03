#!/usr/bin/env bash
# OpenSlate Mobile 构建+装机脚本。
# 前置：Rust android 交叉编译产物就位（见 android/README.md「构建链」一节），
#   cargo build -p openslate-mobile-ffi --release \
#     --target aarch64-linux-android --config <ndk-r21-config.toml>
#   cp target/aarch64-linux-android/release/libopenslate_mobile.so \
#     android/app/src/main/jniLibs/arm64-v8a/
# 用法: scripts/build-apk.sh [--install] [--launch]
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO/android"
# pipefail：gradle 失败时管道整体非零 → 直接中止，绝不能把旧 APK 装到设备上。
timeout 900 ./gradlew :app:assembleDebug --console=plain 2>&1 | tr '\r' '\n' | grep -aE "BUILD|FAILURE|e: " || { echo "BUILD FAILED — abort"; exit 1; }
APK="$REPO/android/app/build/outputs/apk/debug/app-debug.apk"
[ -f "$APK" ] || { echo "APK missing"; exit 1; }
ls -la "$APK"
if [[ "${1:-}" == "--install" || "${2:-}" == "--install" ]]; then
  adb install -r "$APK" | tail -1
fi
if [[ "${1:-}" == "--launch" || "${2:-}" == "--launch" ]]; then
  adb logcat -c
  adb shell am force-stop dev.openslate.mobile
  adb shell am start -n dev.openslate.mobile/.MainActivity | tail -1
fi
