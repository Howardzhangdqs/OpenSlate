# 07-build-dev · 常见构建/运行问题对照表

> 面向接手本项目的 AI agent。本机（开发机）网络环境特殊，多数问题是大陆网络适配项被误动导致的——遇到先对照本表，**不要随手"修复"镜像/vendor/本地应答器**。

## 对照表（症状 / 原因 / 解法）

| # | 症状 | 原因 | 解法 |
|---|------|------|------|
| 1 | gradle configure 阶段挂死不动 | AGP 拉 `dl.google.com` addons 列表被墙（本地应答器没接住） | `systemctl status openslate-responder`；挂了 `sudo systemctl restart openslate-responder`。原理见 `android/README.md`「网络前置」 |
| 2 | 想给 Gradle 设 `http_proxy` 环境变量 | 没用——Gradle（JVM）不认 http_proxy | 仓库已用 aliyun 镜像（`android/settings.gradle.kts`）解决依赖拉取，不需要 proxy |
| 3 | cargo 拉 askama 系列长期 TLS 失败 | 大陆网络对 crates.io/git 源不稳 | 已知问题；根 `Cargo.toml [patch.crates-io]` 已用 GitHub 同版本 git 源补齐；网络恢复后可移除该 patch（注释在 Cargo.toml） |
| 4 | aarch64-linux-android 编 rquickjs-sys 硬失败 | 上游缺 aarch64-linux-android 绑定 | 已知问题；`vendor/rquickjs-sys` 补了绑定（path patch）；上游合入后移除 |
| 5 | 想装 cargo-ndk 简化 Android 构建 | 别装——cargo-ndk 4.x 要求 NDK r23+，本机锁 r21 | 现有 `~/.cargo-android/config.toml`（lld + ndk-stub）就是绕行方案 |
| 6 | 交叉编译链接报 libunwind 相关错误 | 环境变量 `RUSTFLAGS` 覆盖了 `~/.cargo-android/config.toml` 的 rustflags（`-L ndk-stub` 丢失）；或 `~/android-sdk/ndk-stub` 被删 | 清掉 RUSTFLAGS 再编；确认 `~/android-sdk/ndk-stub/libunwind.a`（空归档）存在 |
| 7 | uniffi-bindgen 报 "Unknown library format" | 传了源码路径——uniffi 0.32 CLI 已移除 source 模式 | 走 library 模式（对 host 编出的 `target/debug/libopenslate_mobile.so` 跑 bindgen），照抄 `scripts/uniffi-gen.sh` |
| 8 | 改了 FFI 接口但 APK 运行时崩 / 找不到方法 | 忘了重生成 Kotlin 绑定 + 重拷 `.so` | 跑 `scripts/build-android.sh` 全流程再编 APK |
| 9 | Android 15+ 16KB 页设备装不上 / 起不来 | `.so` 没做 16KB 对齐——`-Wl,-z,max-page-size=16384` 失效（大概率被 RUSTFLAGS 覆盖） | 确认 `~/.cargo-android/config.toml` 生效（没被覆盖）；重出 `.so` |
| 10 | Rust panic 穿越 FFI 导致进程崩（UB）/ 表现为无声崩溃 | 误用 `--profile release` 构建 mobile-ffi（panic=abort 下 catch_unwind 抓不住） | 必须 `mobile` profile（panic=unwind）；用 `scripts/build-android.sh` 不会踩 |
| 11 | genai 行为与官方版不一致 / thinking 丢失 | 用的是 `vendor/genai`（0.6.5 patched：anthropic adapter 回传 thinking block 等），不是 crates.io 版本 | 确认依赖来自 path patch；本地改动明细看 `vendor/genai` 内注释 |
| 12 | App 启动即 `UnsatisfiedLinkError` / JNA 找不到 `libopenslate_mobile.so` | jniLibs 里的 `.so` 过期或缺失（没跑 build-android.sh 就编了 APK）；或 abiFilters 不含设备 ABI | `scripts/build-android.sh` 重出并落位 `.so` 再编 APK；确认设备是 arm64（两台测试机均是） |
| 13 | 只改了 Kotlin 但运行行为像旧 Rust 逻辑 | APK 里打包的还是旧 `.so`（gradle 不感知 jniLibs 上游变化） | 重跑 `scripts/build-android.sh` + 重装；矩阵见 [android-build.md](android-build.md) §7 |
| 14 | `adb install` 报 more than one device / 装错设备 | 多设备同时在线（华为 USB + 小米无线） | `adb devices -l` 列出后 `adb -s <序列号> install -r …`；小米地址动态，以实际输出为准 |
| 15 | 小米无线 ADB 连不上 | 无线调试端口随机分配 / 未配对 | 让用户在手机上重新配对；`adb connect <IP:端口>`（地址以 `adb devices -l` 为准）；设备权威信息见根 `AGENTS.md` |
| 16 | Termux 工具报 "RUN_COMMAND permission not granted" | dangerous 运行时权限未授予 | 系统设置 → 应用 → OpenSlate → 权限；部分 ROM 需先打开一次 Termux 才能授予；或设置页 Shizuku 一键授权（`pm grant`）；详见 `bridge/TermuxExec.kt` 注释 |
| 17 | Kotlin 主线程 ANR，栈在 FFI 调用里 | 在主线程调了阻塞型 FFI 方法（`listProviderModels` / `registryUpdateSource` / `registrySearch` / `lookupModelMeta`） | 包进 `Dispatchers.IO`（FFI doc 注释已标明哪些方法必须 IO 线程） |
| 18 | logcat 看不到 Rust 日志 | tag 过滤错 / 进程未起 | Rust tag 是 `OpenSlateRust`（不是 crate 名）；`adb logcat -s OpenSlateRust`；确认 AgentService 前台服务在跑 |
| 19 | 历史会话页大量 run 显示 running | 上次进程被杀没走 onDestroy，run 停在 running | 下次启动 `assemble_state` 的 `mark_running_runs_interrupted` 会统一清扫为 interrupted——正常现象，无需手工修库 |
| 20 | 磁盘爆满（常在并行 `cargo test` 期间触发）/ `target/debug` 巨大 | 静态链接测试二进制 × 全量 DWARF（单个体积 ~75% 是调试段，410MB+）× **cargo 不做旧产物 GC**（源码一变新 hash 落盘、旧二进制永存；du 还会因 cargo 硬链接重复计数虚高，以 `df -h` 为准）；爆满刻是并行 rustc 临时文件的瞬时峰值 | 已预防：根 `Cargo.toml [profile.dev]` 调 `debug = "line-tables-only"` + `incremental = false`。应急清理（保 48h 内热缓存）：`find target/debug/deps -type f -mtime +2 -delete && rm -rf target/debug/incremental`；要干净基线直接 `cargo clean`（只清 host `target/`，不碰 `target-android/`，代价是下次全量重编） |

## 测试设备

测试设备信息（型号 / 序列号 / API 区间 / 连接方式）**不在此重复，权威来源是仓库根 `AGENTS.md`**；执行 adb 操作前先 `adb devices -l` 检测实际连接情况。
