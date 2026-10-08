# 07-build-dev · 工具链前提

> 面向接手本项目的 AI agent。开发机（本机）网络环境特殊：大陆直连受限，仓库里多处镜像 / vendor / 本地应答器都是为此适配的，**不要随手"修复"它们**（详见 [troubleshooting.md](troubleshooting.md)）。

## 1. Rust 工具链

- `rust-toolchain.toml` 锁 `channel = "stable"`（**无版本号锁定**）；当前实际 `rustc 1.97.1` / `cargo 1.97.1`。
- workspace 标准布局（成员见根 `Cargo.toml`）；`vendor/genai` 被 exclude（独立测试在其目录内跑，见 [build-and-test.md](build-and-test.md)）。
- musl target（mcp-host 静态构建用）：`rustup target add aarch64-unknown-linux-musl`（`scripts/build-mcp-host.sh` 会自动跑）。

## 2. Android SDK / NDK

- SDK 路径：`android/local.properties` → `sdk.dir=/home/howardzhangdqs/android-sdk`（该文件不入库，换机要改）。
- NDK：**r21（21.4.7075529）**，位于 `~/android-sdk/ndk/21.4.7075529`；`android/app/build.gradle.kts` 的 `ndkVersion` 同值。
- **不用 cargo-ndk**：cargo-ndk 4.x 要求 NDK r23+，本机锁 r21；走手工 linker 配置（§3）。
- LLVM 工具链前缀：`~/android-sdk/ndk/21.4.7075529/toolchains/llvm/prebuilt/linux-x86_64/bin`（linker=`aarch64-linux-android26-clang`、ar=`llvm-ar`、clang++=`aarch64-linux-android26-clang++`）。

## 3. 交叉编译配置：`~/.cargo-android/config.toml`

完整内容（构建脚本以 `--config ~/.cargo-android/config.toml` 显式引用）：

```toml
# Android aarch64 交叉编译（NDK r21 直连；cargo-ndk 4.x 要求 r23+，绕行）。
# -fuse-ld=lld：r21 默认 GNU ld 不识别 Rust 的 -lunwind；
# -L ndk-stub：空 libunwind.a 兜底（真正的 unwinder 由 rust std 的
#   libunwind-*.rlib 提供，r21 无独立 libunwind.a for aarch64）。
[target.aarch64-linux-android]
linker = "/home/howardzhangdqs/android-sdk/ndk/21.4.7075529/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android26-clang"
ar = "/home/howardzhangdqs/android-sdk/ndk/21.4.7075529/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar"
rustflags = [
  "-C", "link-arg=-fuse-ld=lld",
  "-C", "link-arg=-Wl,--build-id=none",
  "-C", "link-arg=-Wl,-z,max-page-size=16384",
  "-L", "/home/howardzhangdqs/android-sdk/ndk-stub",
]
```

逐条解释：

| 配置 | 作用 |
|------|------|
| linker = `aarch64-linux-android26-clang` | NDK clang 包装脚本，自动带 `--target=aarch64-linux-android26`（API 26 = minSdk）与 sysroot |
| `-fuse-ld=lld` | r21 默认 GNU ld 不识别 Rust 传的 `-lunwind`，必须换 lld |
| `-Wl,--build-id=none` | 去掉 build-id 段，减体积 |
| `-Wl,-z,max-page-size=16384` | **16KB 页对齐**（见 §5） |
| `-L ~/android-sdk/ndk-stub` | 空 `libunwind.a` 兜底（见 §4） |

**警告**：交叉编译时**不要在环境变量里设 `RUSTFLAGS`**——会整体覆盖本文件的 rustflags（lld / 16KB 对齐 / unwind stub 全丢）。

## 4. `~/android-sdk/ndk-stub`（空 libunwind 兜底）

目录内容：`stub.c`（空翻译单元）、`stub.o`、`empty.o`、`libunwind.a`（904 字节的空归档）。

原因：Rust 的 `-lunwind` 在 r21 的 aarch64 NDK 里无独立 `libunwind.a`（新版 NDK 才并入）；真正的 unwinder 由 rust std 自带的 `libunwind-*.rlib` 提供。链接器找不到 `-lunwind` 时由这个空归档满足符号要求。**删掉它 aarch64 链接会硬失败**。

这也是 `mobile` profile `panic=unwind` 能工作的前提之一（unwind 需要完整的 unwinder——std 自带 + lld 链接）。

## 5. 16KB 页对齐（Android 15+）

- `-Wl,-z,max-page-size=16384` 把 `.so` 的 ELF LOAD 段对齐到 16KB：Android 15+ 的 16KB 页设备要求；对 4KB 页设备**向下兼容**（对齐更大不影响加载）。
- 该参数在 `~/.cargo-android/config.toml` 的 rustflags 里，**被环境变量 RUSTFLAGS 覆盖即失效**——16KB 设备装不上/起不来的第一排查点。
- Gradle 侧配合：`android/app/build.gradle.kts` 的 `packaging { jniLibs { useLegacyPackaging = false } }`（.so 不压缩对齐装载，AGP 9 默认且支持 16KB 对齐校验）。

## 6. Gradle / JVM

- Gradle wrapper 锁定版本（`android/gradle/wrapper/gradle-wrapper.properties`）。
- AGP 9.4.0 / Kotlin 2.4.20（版本目录 `android/gradle/libs.versions.toml`；AGP 9 内置 Kotlin，无 kotlin-android 插件）。
- Java/JVM 17（`app/build.gradle.kts` compileOptions + kotlin compilerOptions）。
- 依赖拉取走 aliyun 镜像（`settings.gradle.kts`）；**Gradle JVM 不认 http_proxy 环境变量**。
- AGP 对 `dl.google.com` addons 列表的拉取由本机 `openslate-responder` systemd 服务拦截应答（`/etc/hosts` + 本地 HTTPS 空仓库，原理见 `android/README.md`）。

## 7. 测试设备

测试设备信息（型号 / 序列号 / API / 连接方式）的**权威来源是仓库根 `AGENTS.md`**，本章不重复；adb 操作前先 `adb devices -l` 确认实际连接。
