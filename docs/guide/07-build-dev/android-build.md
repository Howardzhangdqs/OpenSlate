# 07-build-dev · Android 交叉编译与 APK 构建

> 面向接手本项目的 AI agent。工具链前提（NDK / config.toml / 16KB 对齐）见 [toolchain.md](toolchain.md)；常见故障对照见 [troubleshooting.md](troubleshooting.md)。

## 1. 一条龙：`scripts/build-android.sh`

Rust `.so` 交叉编译 + UniFFI Kotlin 绑定生成 + jniLibs 落位，三步全做：

```bash
scripts/build-android.sh
```

脚本逐步解析（等价手动命令，供理解/排障）：

```bash
# [1/3] 交叉编译 .so（arm64-v8a, mobile profile）
CARGO_TARGET_DIR=target-android \
CC_aarch64_linux_android=~/android-sdk/ndk/21.4.7075529/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android26-clang \
AR_aarch64_linux_android=…/llvm-ar \
CXX_aarch64_linux_android=…/aarch64-linux-android26-clang++ \
cargo build --target aarch64-linux-android --profile mobile \
  -p openslate-mobile-ffi --config ~/.cargo-android/config.toml
# 产物 target-android/aarch64-linux-android/mobile/libopenslate_mobile.so
cp …/libopenslate_mobile.so android/app/src/main/jniLibs/arm64-v8a/

# [2/3] UniFFI 绑定（library 模式：先在 host 编出 target/debug 的 cdylib）
cargo build -p openslate-mobile-ffi
cargo run -p openslate-mobile-ffi --features bindgen --bin uniffi-bindgen -- \
  generate --library target/debug/libopenslate_mobile.so \
  --language kotlin --out-dir <tmp> \
  --config crates/openslate-mobile-ffi/uniffi.toml

# [3/3] 绑定拷到 android/app/src/main/java/uniffi/
```

- 产物目录独立（`CARGO_TARGET_DIR=target-android`）——不污染 host 构建缓存。
- **警告**：交叉编译时**不要在环境变量里设 `RUSTFLAGS`**——会覆盖 `~/.cargo-android/config.toml` 里的 lld / unwind / 16KB 对齐配置。
- 完成后下一步：`cd android && ./gradlew :app:assembleDebug`（或直接 §3）。

## 2. 只重生成绑定：`scripts/uniffi-gen.sh`

仅当 `openslate-mobile-ffi` 的 UniFFI 面（导出方法 / Record / Error / callback）变更时需要。产物：`android/app/src/main/java/uniffi/openslate/mobile/openslate_mobile.kt`（生成文件，**禁手改**）。

注意：uniffi 0.32 的 CLI **已移除 source 模式**（直接传 `src/lib.rs` 会报 "Unknown library format"），必须走 library 模式——先在 host 编出 cdylib，再对 `.so` 产物跑 bindgen（脚本已照此写）。

只改 `openslate-mobile` 内部实现（不动 FFI 面）→ 不用重生成绑定，`scripts/build-android.sh` 重编 `.so` 即可。

## 3. MCP host 二进制：`scripts/build-mcp-host.sh`

```bash
rustup target add aarch64-unknown-linux-musl
RUSTFLAGS="-C linker=rust-lld" cargo build --release \
  -p openslate-mcp-host --bin mcp-host --target aarch64-unknown-linux-musl
# 产物 → android/app/src/main/assets/mcp-host-aarch64
```

- **静态**链接（rust-lld + self-contained musl crt）：Termux 侧零运行时依赖。
- host crate 的 rmcp 依赖是独立最小 feature 集（无 reqwest/ring），见 `crates/openslate-mcp-host/Cargo.toml` 注释。
- 改了 mcp-host 代码需要重跑 + 重编 APK（assets 重新打包）；App 内「自动化操作」经 /sdcard 中转装进 Termux `~/.openslate/mcp-host`。
- 这里设 `RUSTFLAGS` 是**故意的且安全的**：musl host 构建不走 `~/.cargo-android/config.toml`（那是 aarch64-linux-android target 的配置），不冲突。

## 4. APK 构建 / 安装 / 启动

```bash
# 构建（gradle，900s 超时；失败即中止，绝不装旧 APK）
scripts/build-apk.sh                # 只构建
scripts/build-apk.sh --install      # 构建 + adb install -r
scripts/build-apk.sh --install --launch   # 再 force-stop + 冷启 + 清 logcat

# 或手动
cd android && ./gradlew :app:assembleDebug
# 产物：android/app/build/outputs/apk/debug/app-debug.apk
```

安装到指定设备（**多设备在线时必须 `-s`**；先看实际连接情况）：

```bash
adb devices -l                                  # 先跑这个！不要假设设备在线
adb -s P3PDU18319003124 install -r android/app/build/outputs/apk/debug/app-debug.apk
adb -s <无线IP:端口> install -r …               # 小米无线 ADB，地址动态
```

设备清单与识别特征的权威来源是根 `AGENTS.md`。

## 5. 日志排查

```bash
adb logcat -s OpenSlateRust      # Rust 侧（alog! / panic hook）
adb logcat -s OpenSlateBridge    # Kotlin 桥层
adb logcat -s OpenSlateTermux    # Termux 执行器
adb logcat -s OpenSlateRust OpenSlateBridge    # 组合
```

Rust panic 经 hook 外显为 `PANIC: {msg} at {file}:{line}:{col}`（tag `OpenSlateRust`）。

## 6. debug 包广播注入（DebugReceiver）

debug 构建可用 `adb shell am broadcast` 直接驱动 App（无需 UI 操作）：

```bash
adb shell am broadcast -a dev.openslate.mobile.SET_KEY \
  --es provider zhipu --es key <KEY>
adb shell am broadcast -a dev.openslate.mobile.SET_PROXY --es url http://127.0.0.1:7890
adb shell am broadcast -a dev.openslate.mobile.SEND --es msg '{"type":"new_session"}'
adb shell am broadcast -a dev.openslate.mobile.TERMUX --es command "echo hi"
```

（action 定义在 `android/app/src/debug/AndroidManifest.xml`；release 不携带。）

## 7. 变更 → 产物 矩阵

| 改了什么 | 必须跑 |
|---------|--------|
| `openslate-mobile` / 更下层 Rust（不动 FFI 面） | `scripts/build-android.sh` → `scripts/build-apk.sh --install` |
| `openslate-mobile-ffi` 的 UniFFI 面（方法/Record/Error/callback） | `scripts/build-android.sh`（内含绑定重生成）→ 编 APK |
| Kotlin UI / bridge | 只 `scripts/build-apk.sh --install`（gradle 增量） |
| `openslate-mcp-host` | `scripts/build-mcp-host.sh` → 编 APK |
| 配置 / 资源（非代码） | 编 APK |
