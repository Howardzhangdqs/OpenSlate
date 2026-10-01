# OpenSlate Mobile（第一阶段）

Android 原生 Host + Rust Agent Core 的最小闭环。

## 架构（当前）

```text
┌─ Android App (Kotlin/Compose) ────────────────────────────┐
│ MainActivity → ChatScreen（transcript 流 + 审批横幅）        │
│ AgentService（前台服务，持有 runtime 生命周期）               │
│ RuntimeBridge（事件 JSON → StateFlow；单消费协程保序）        │
└────────────┬───────────────────────────────────────────────┘
             │ UniFFI（JNA）libopenslate_mobile.so
┌────────────▼───────────────────────────────────────────────┐
│ openslate-mobile-ffi  create/send/setApiKey/resolveHostCall │
│ openslate-mobile      MobileRuntime + EventSink + HostCall  │
│ openslate-session     传输无关会话核心（与 WS server 共用）   │
│ openslate-app/core    装配链 + RunManager + 工具注册表        │
└─────────────────────────────────────────────────────────────┘
```

- 上行 `ClientMsg` / 下行 `ServerMsg` 与 CLI/TUI/WS 完全同协议（JSON）。
- host tool 走 request/resolve：`{"type":"host_call_requested","id",...}` 信封。
- API key 经 FFI `setApiKey` 内存注入（宿主负责 Keystore 持久化，不落盘）。

## 构建（本机网络已适配：aliyun 镜像 + NDK r21 直连）

```bash
# 1. Rust .so（arm64-v8a, mobile profile = panic unwind）
NDKB=$HOME/android-sdk/ndk/21.4.7075529/toolchains/llvm/prebuilt/linux-x86_64/bin
CARGO_TARGET_DIR=target-android \
CC_aarch64_linux_android=$NDKB/aarch64-linux-android26-clang \
AR_aarch64_linux_android=$NDKB/llvm-ar \
cargo build --target aarch64-linux-android --profile mobile \
  -p openslate-mobile-ffi --config ~/.cargo-android/config.toml
cp target-android/aarch64-linux-android/mobile/libopenslate_mobile.so \
   android/app/src/main/jniLibs/arm64-v8a/

# 2. Kotlin 绑定（改 FFI 接口后重跑）
cargo build -p openslate-mobile-ffi && \
cargo run -p openslate-mobile-ffi --features bindgen --bin uniffi-bindgen -- \
  generate --library target/debug/libopenslate_mobile.so \
  --language kotlin --out-dir /tmp/uniffi-out \
  --config crates/openslate-mobile-ffi/uniffi.toml
cp -r /tmp/uniffi-out/uniffi android/app/src/main/java/

# 3. APK
cd android && ./gradlew :app:assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

### 网络前置（本机必须）

AGP 每次 configure 都会拉 `dl.google.com` 的 addons 列表（直连被墙会挂死）。
本地应答器已改为 systemd 托管，开机常驻：

```bash
# 查看状态 / 重启
systemctl status openslate-responder
sudo systemctl restart openslate-responder
# 手动拉起（首次）
sudo systemd-run --unit=openslate-responder python3 \
  ~/android-sdk/localresponder/server.py
```

原理：`/etc/hosts` 把 dl.google.com 指到 127.0.0.1，本地 HTTPS 服务返回空仓库
（证书 CA 已装入 JDK 信任库）；addons_list 一律 404 让版本阶梯快速耗尽。

## 真机冒烟（无需 API key）

1. 安装并启动 App → 前台服务通知出现，聊天页显示就绪（收到 snapshot）。
2. 发送「ping 一下宿主」→ 模型会调用 `mobile.ping`（默认 zhipu glm-4.7，
   无 key 时在设置里注入：当前版本通过 `adb shell am broadcast` 或后续
   设置页；调试期可直接 `adb logcat | grep OpenSlate` 观察
   host_call_requested → resolveHostCall 往返）。
3. 配好 key 后发普通消息 → 观察流式 delta 与 turn_ok 统计。

## 已知限制（第一阶段）

- 桌面 builtin fs/shell 工具在 mobile 默认关闭（builtin_tools.enabled=false）。
- host tool 仅 `mobile.ping`（Phase 3 起接入 Android 能力宿主）。
- 密钥持久化（Keystore）待 Phase 2 SecretProvider。
- 本机网络替代件：NDK r21（计划 r29）、build-tools 36 为占位 stub、
  android.jar 为社区构建（Reginer/aosp-android-jar）；代理可用后建议
  升级官方组件并复验。
