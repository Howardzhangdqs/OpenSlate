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
