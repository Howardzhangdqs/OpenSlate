可以。下面这份我按“**直接交给工程师/AI Coding Agent 执行**”的方式写，技术路线我已经重新权衡过：**不把整个 OpenSlate 强行 WASM 化，而是 Android 原生 Host + Rust 原生 Agent Core + WASM Skill 沙箱，并让 Core 保持未来可编译 WASM 的边界。**

这是我认为成功率、工程成本、扩展性和最终能力最平衡的方案。

# OpenSlate Mobile Agent 技术实施方案 v1.0

**项目基线：** Howardzhangdqs/OpenSlate
**方案日期：** 2026-09-30
**版本基线：** 2026-10-01 已逐项核实（见第 0 节）
**目标平台：** Android
**项目目标：** 将 OpenSlate 扩展为真正能够理解、操作和编排 Android 手机能力的本地 Agent Runtime，而不是单纯的聊天客户端。

---

# 0. 开发环境与版本基线（2026-10-01 核实）

**测试策略：实体机（arm64）直连 adb 测试，不使用模拟器。** 第一版只编译 `arm64-v8a`；`x86_64` 留给 CI / 模拟器场景再补（开发机有 KVM，随时可加）。不装模拟器可省 4–12 GiB 磁盘与 2–4 GiB 运行内存。

## 开发机基线（已核实）

```text
CPU     i5-1135G7（4 核 8 线程）
内存    14 GiB + 4 GiB swap
磁盘    /home 剩余 199 GiB（工作区所在分区）
已装    rustc/cargo 1.97.1、rustup、adb、JDK 25
缺失    Android SDK/NDK、Android Studio、cargo-ndk、aarch64-linux-android target
预算    SDK + NDK + Studio + Gradle 缓存 ≈ 10–15 GiB
```

## Android 侧锁定版本

| 组件 | 版本 | 说明 |
| --- | --- | --- |
| Android Studio | Quail 4（2026.1.4） | 当前稳定版 |
| Android Gradle Plugin | 9.4.0 | 2026-09 稳定版；9.5 仍在 alpha |
| Gradle | 9.8.0（wrapper 锁定） | AGP 9.4 最低要求 9.6.0 |
| JDK | 21 LTS（最低 17） | AGP 9.4 最低/默认 17；命令行用 Temurin 21，IDE 内用 Studio 自带 JBR；系统 JDK 25 不用于 Android 构建 |
| Kotlin | 2.4.20 | K2；Compose 编译器随 Kotlin 版本发行 |
| Jetpack Compose BOM | 2026.09.00 | 统一管理 Compose 库版本 |
| kotlinx-coroutines | 1.11.0 | |
| kotlinx-serialization | 1.11.0 | 1.12.0 仍在 RC，不采用 |
| DataStore | 最新稳定版 | 1.3.0 尚在 alpha，不采用 |
| compileSdk / targetSdk | 36 / 36 | AGP 9.4 最高支持 API 37；不上架 Play，36 更稳 |
| minSdk | 26 | 维持不变 |
| SDK Build Tools | 36.0.0 | AGP 9.4 默认 |
| cmdline-tools | 23.0 | |
| Android NDK | r29（29.0.14206865） | AGP 9.4 自带默认 28.2.13676358，两者 cargo-ndk 均可用；r30 已发布但不先采用 |
| Shizuku（App + API） | 13.6.0 | 已支持 Android 16 QPR1；若实体机为 Android 17，装机前先验证兼容性 |
| Termux | 0.118.3 | GitHub Releases / F-Droid；RUN_COMMAND 集成基线 |

## Rust 侧锁定版本

| 组件 | 版本 | 说明 |
| --- | --- | --- |
| Rust toolchain | 1.97.1 stable（本机已装） | sqlx 0.9 要求 ≥1.94、wasmtime 49 要求 ≥1.96，均满足 |
| Rust target | aarch64-linux-android | 第一版不装 x86_64-linux-android |
| cargo-ndk | 4.1.2 | 要求 rustc ≥1.86 |
| UniFFI / uniffi-bindgen | 0.32.2 | 生成 Kotlin 绑定 |
| tokio | 1.53.1 | |
| sqlx（sqlite） | 0.9.0 | bundled SQLite 静态链接 |
| rquickjs | 0.14.0 | 从 0.12.2 升级；wasm targets 已支持 |
| wasmtime | 49.0.1 | 第八阶段引入时按当时最新 patch 复核 |

## 安装清单

```bash
# Android：安装 Android Studio Quail 4，或仅装 cmdline-tools 23.0
sdkmanager "platform-tools" "platforms;android-36" "build-tools;36.0.0" \
           "cmdline-tools;23.0" "ndk;29.0.14206865"

# Rust
rustup target add aarch64-linux-android
cargo install cargo-ndk --locked                    # 4.1.2
cargo install uniffi-bindgen --version 0.32.2 --locked
```

环境变量：`ANDROID_HOME` 指向 SDK 根目录；Gradle 优先用 Studio 自带 JBR，命令行构建设 `JAVA_HOME` 为 Temurin 21。

---

# 1. 最终技术决策

最终采用：

```text
Kotlin + Jetpack Compose
        │
        │ Android UI / Lifecycle / Permission
        ▼
┌──────────────────────────────────────┐
│        Android Capability Host       │
│                                      │
│ Accessibility                       │
│ Shizuku UserService                 │
│ Termux RUN_COMMAND                  │
│ NotificationListener                │
│ MediaProjection                     │
│ Android Intent / PackageManager     │
│ Keystore                            │
└─────────────────┬────────────────────┘
                  │ Host Tool Bridge
                  │
┌─────────────────▼────────────────────┐
│          OpenSlate Rust Core         │
│          libopenslate.so             │
│                                      │
│ Agent Loop                           │
│ Model Provider                       │
│ Tool Registry                        │
│ Approval                             │
│ Session / Context                    │
│ SQLite                               │
│ MCP HTTP                             │
│ PTC                                  │
└─────────────────┬────────────────────┘
                  │
                  │ optional
                  ▼
┌──────────────────────────────────────┐
│            WASM Runtime              │
│                                      │
│ Third-party Skills                   │
│ Untrusted Plugins                    │
│ Future portable components           │
└──────────────────────────────────────┘
```

一句话概括：

> **Agent Kernel 先原生 Rust，手机能力全部由 Kotlin Host 提供，WASM 专门负责不可信 Skill/Plugin 的隔离。**

同时要求在重构过程中让 Agent Kernel 不直接依赖 OS 特性，为未来：

```text
openslate-kernel
        │
        ├── native Android
        ├── native Desktop
        └── wasm32-wasip2
```

保留可能性。

---

# 2. 为什么不直接把整个 OpenSlate 编译成 WASM

OpenSlate 当前 `openslate-core` 并不是纯计算 Core。

它直接依赖：

```text
tokio + process
tokio-util
dirs
reqwest
rmcp
stdio MCP
openslate-mcp-builtin
openslate-ptc
filesystem
```

workspace 里同时启用了 Tokio 的 `rt-multi-thread / fs / signal`，`openslate-core` 又额外启用 `process`；MCP 还启用了 `transport-child-process`。SQLite 使用 SQLx，PTC 使用 rquickjs。([GitHub][1])

因此如果现在要求：

```bash
cargo build --target wasm32-wasip2
```

实际上不是“换一个 target”，而是需要先把：

```text
Agent logic
OS
filesystem
process
MCP
network
database
runtime
```

重新切开。

这项重构最终值得做，但不应该成为 Android MVP 的前置条件。

另外，Wasmtime 虽然支持 Android，但官方明确将 Android 列为支持程度低于 Windows/macOS/Linux、测试较少的平台。因此不应该让 Wasmtime 成为整个 Android App 是否能启动的关键依赖。([GitHub][2])

WASM 最有价值的地方不是“为了 WASM 而 WASM”，而是：

```text
插件隔离
权限控制
资源限制
跨平台 Skill
第三方代码执行
```

因此，本项目采用：

```text
Agent Core = Native Rust

Skill Sandbox = WASM
```

这是最终推荐方案。

---

# 3. 当前 OpenSlate 中应该保留的部分

不要重写 Agent。

OpenSlate 当前的抽象已经相当适合 Mobile Agent。

现有 `Tool` 接口包含：

```rust
name()
description()
parameters_schema()
execute(json)
```

同时已经存在 `ToolExecutor`、`ToolRegistry`、工具输出限制和 Tool Audit。([GitHub][3])

这套抽象继续使用。

现有：

```text
RunManager
AgentTree
ApprovalManager
ModelProvider
ToolRegistry
ToolAudit
PTC
limits
conversation
usage/cost
```

都继续保留。

尤其不要重新实现 Agent Loop。

---

# 4. 非常重要：继续使用 openslate-protocol

OpenSlate 已经有一个非常适合作为 Mobile Bridge 的协议。

`openslate-protocol` 本身是纯数据 crate，不依赖 Tokio/Axum，并且已经定义了：

```text
Submit
Cancel
ApprovalAnswer
NewSession
SetModel

Snapshot
Delta
Reasoning
ToolStart
ToolEnd
ApprovalRequested
ApprovalResolved
TurnOk
TurnError
```

协议还有明确的 `PROTOCOL_VERSION`。([GitHub][4])

因此 Android 不要重新设计一套：

```text
onToken()
onTool()
onApproval()
onRunDone()
```

FFI API。

应该直接复用现有协议语义。

建议最终 FFI：

```rust
MobileRuntime::create(...)
MobileRuntime::send(json)
MobileRuntime::resolve_host_call(id, json)
MobileRuntime::shutdown()
```

Rust → Kotlin：

```text
on_event(json)
```

其中 `json` 尽量复用：

```text
ServerMsg
```

Kotlin → Rust：

```text
ClientMsg
```

这样：

```text
CLI
TUI
WebSocket
Android FFI
```

实际上都操作同一种 OpenSlate 协议。

这会极大降低长期维护成本。

---

# 5. 新的仓库结构

在尽量保持现有 desktop 能力不变的前提下，重构成：

```text
crates/

  openslate-core/
      Agent
      RunManager
      Approval
      Tool abstraction
      AgentTree
      Context
      Limits
      Types

  openslate-model-genai/
      LLM provider

  openslate-store-sqlite/
      SQLite storage

  openslate-protocol/
      ClientMsg / ServerMsg

  openslate-ptc/
      QuickJS PTC

  openslate-platform/
      NEW
      Platform abstractions

  openslate-mobile/
      NEW
      Android runtime composition

  openslate-mobile-ffi/
      NEW
      UniFFI API

  openslate-wasm-runtime/
      NEW
      WASM Skill runtime

  openslate-skill-sdk/
      NEW
      WASM Skill SDK / WIT definitions


  openslate-app/
  openslate-cli/
  openslate-tui/
  openslate-server/
  openslate-mcp-builtin/


android/

  app/

    ui/
    agent/
    bridge/
    service/

    tools/
      android/
      shizuku/
      termux/
      accessibility/
      notification/
      screen/

    security/
    storage/

    shizuku-service/
```

原则：

```text
openslate-core
```

以后不能直接知道：

```text
Android
Shizuku
Termux
Accessibility
JNI
Compose
```

存在。

---

# 6. Rust ↔ Kotlin 的边界设计

使用：

```text
UniFFI
```

优先于手写 JNI。

但不要让 Rust：

```text
await Kotlin async callback
```

这会让生命周期、线程和异常处理变复杂。

采用 request / resolve 模型。

例如 Rust 需要执行：

```text
android.launch_app
```

流程：

```text
Rust Agent

ToolExecutor.execute()
        │
        ▼
生成 host_call_id = 42
        │
        ▼
发送 HostCallRequested
        │
        ▼
Kotlin
        │
        ▼
异步执行 Android API
        │
        ▼
runtime.resolveHostCall(
    id = 42,
    result = ...
)
        │
        ▼
Rust oneshot channel resume
        │
        ▼
Agent continues
```

也就是说：

```text
FFI 永远不阻塞 Android Main Thread。
```

Rust 内部可以继续保持：

```rust
async fn execute(...)
```

Android Host 则继续使用：

```text
Coroutine
Flow
Service
```

---

# 7. Android 技术栈

Android 端统一使用：

| 模块               | 技术                          |
| ---------------- | --------------------------- |
| Language         | Kotlin                      |
| UI               | Jetpack Compose             |
| State            | StateFlow / SharedFlow      |
| Async            | Coroutines                  |
| Agent lifecycle  | Foreground Service          |
| Rust FFI         | UniFFI                      |
| Settings         | DataStore                   |
| Secrets          | Android Keystore + AES-GCM  |
| System privilege | Shizuku                     |
| Linux runtime    | Termux                      |
| UI automation    | AccessibilityService        |
| Screen           | MediaProjection             |
| Notifications    | NotificationListenerService |

版本基线（2026-10 已核实，详见第 0 节）：

```text
compileSdk = 36
targetSdk  = 36
minSdk     = 26
```

第一版 ABI（实体机直测，不依赖模拟器）：

```text
arm64-v8a
```

`x86_64` 仅在需要模拟器或 CI 验证时再编译；本机有 KVM，可随时补。

不要优先支持：

```text
armeabi-v7a
```

---

# 8. Agent Runtime 必须运行在 Service，而不是 Activity

Rust Agent Runtime 生命周期不能绑定 Compose Activity。

架构：

```text
MainActivity
     │
     │ bind
     ▼
AgentForegroundService
     │
     ├── libopenslate.so
     ├── Runtime
     ├── ToolHost
     └── Session
```

用户从 UI 明确启动 Agent。

Android 12+ 已经限制后台随意启动 Foreground Service；Android 14+ 又要求 FGS 声明具体 service type。([Android Developers][5])

Agent 主服务建议评估：

```text
foregroundServiceType="specialUse"
```

并声明实际用途；Android 官方提供 `specialUse` 给其他 FGS 类型不能覆盖的有效用途。([Android Developers][6])

屏幕采集不要混在普通 Agent Service 中。

需要 MediaProjection 时启动对应：

```text
mediaProjection
```

服务。

---

# 9. Android Tool 的核心设计原则

**禁止把所有能力实现成一个 shell 工具。**

错误：

```text
shell("am start ...")
shell("settings put ...")
shell("input tap ...")
```

正确：

```text
android.app.launch
android.app.info
android.app.list

android.ui.snapshot
android.ui.click
android.ui.set_text
android.ui.scroll

android.device.info

android.notification.list
android.notification.reply

android.screen.capture

shizuku.settings.get
shizuku.settings.put
shizuku.package.force_stop

termux.exec
```

Agent 应尽可能操作：

```text
typed semantic tools
```

而不是生成 shell command。

---

# 10. Android 操作优先级

任何操作都按照以下优先级执行：

```text
① Android Public API / Intent

            ↓ 无法完成

② Shizuku / Binder

            ↓ 无法完成

③ Accessibility semantic action

            ↓ UI Tree 不足

④ Screenshot + Vision

            ↓

⑤ Coordinate click
```

**坐标点击永远是最后 fallback。**

---

# 11. Accessibility Tool

Accessibility 负责：

```text
当前 package
window tree
node tree
text
contentDescription
resourceId
className
bounds
clickable
editable
scrollable
checked
selected
actions
```

模型不直接获得 Android `AccessibilityNodeInfo`。

需要转换为稳定 JSON：

```json
{
  "window_id": 3,
  "package": "com.example",
  "nodes": [
    {
      "id": "n42",
      "text": "发送",
      "resource_id": "send_button",
      "class": "android.widget.Button",
      "bounds": [810, 1820, 1050, 1980],
      "clickable": true,
      "editable": false
    }
  ]
}
```

Agent：

```text
android.ui.click({
    "node_id": "n42"
})
```

而不是：

```text
tap(930,1900)
```

Android 官方 Accessibility API 可以获取当前窗口节点树，也支持节点 click、set text 和 global Back/Home 等操作。([Android Developers][7])

注意节点 ID 不能直接使用对象地址。

每次 snapshot 创建：

```text
snapshot_id
node_id
```

动作必须携带：

```text
snapshot_id
```

如果 UI 已变化，则返回：

```text
STALE_NODE
```

要求 Agent 重新 snapshot。

---

# 12. Screenshot / VLM fallback

屏幕采集通过：

```text
MediaProjection
```

Android 官方 MediaProjection 本质上是用户授权的屏幕捕获 token。([Android Developers][8])

Tool：

```text
android.screen.capture
```

返回：

```text
PNG/JPEG
width
height
rotation
timestamp
```

以后可以接 VLM。

UI Agent 流程应为：

```text
snapshot accessibility tree

如果足够
    semantic action
else
    screenshot
    VLM locate
    coordinate fallback
```

不要每一步都截图。

---

# 13. Shizuku 设计

Shizuku 必须使用：

```text
UserService
```

不要以 deprecated 的：

```text
Shizuku.newProcess()
```

为核心。

Shizuku 官方明确建议复杂需求使用 UserService；UserService 可以让代码运行在：

```text
shell UID 2000

或

root UID 0
```

身份。`newProcess` 已被标为 deprecated。([GitHub][9])

架构：

```text
Android Tool
      │
      ▼
ShizukuBackend
      │
      ▼
AIDL
      │
      ▼
Shizuku UserService
      │
      ▼
Android System / Binder
```

必须做 capability detection：

```json
{
  "available": true,
  "uid": 2000,
  "mode": "adb",
  "capabilities": [...]
}
```

或者：

```json
{
  "available": true,
  "uid": 0,
  "mode": "root"
}
```

不能假设：

```text
Shizuku available == root
```

---

# 14. 第一批 Shizuku Tools

实现：

```text
shizuku.status

shizuku.package.list
shizuku.package.info
shizuku.package.force_stop

shizuku.settings.get
shizuku.settings.put

shizuku.activity.start

shizuku.input.keyevent

shizuku.exec
```

其中：

```text
shizuku.exec
```

只是 escape hatch。

默认不要暴露给普通 Agent。

---

# 15. Termux 集成

Termux 不作为 OpenSlate 主运行环境。

它作为：

> **Agent 的 Linux execution backend。**

第三方 Android App 应通过 Termux 官方：

```text
RUN_COMMAND Intent
```

执行命令。

Termux 当前文档要求第三方应用声明：

```text
com.termux.permission.RUN_COMMAND
```

并要求 Termux：

```text
allow-external-apps=true
```

target SDK ≥30 时还需要处理 package visibility。命令执行结果可以通过 `PendingIntent` 返回。([GitHub][10])

实现：

```text
termux.status

termux.exec
```

Schema：

```json
{
  "executable": "python",
  "args": [
    "-c",
    "print(2+2)"
  ],
  "cwd": "~/",
  "stdin": null,
  "timeout_ms": 30000
}
```

不要默认提供：

```json
{
  "command": "arbitrary shell string"
}
```

优先 executable + args，避免 shell injection。

可以额外提供：

```text
termux.shell
```

但必须属于高风险 Tool。

---

# 16. Termux 能力定位

Termux 负责：

```text
python
node
git
ssh
curl
ffmpeg
jq
sqlite
ripgrep
clang
用户脚本
MCP Server
```

这意味着手机 Agent 同时拥有：

```text
Android Runtime

+

Linux Userland
```

这正是本项目和普通 Accessibility Agent 最大的差异。

---

# 17. Notification 能力

实现：

```text
NotificationListenerService
```

Android 官方允许 NotificationListenerService 接收通知 posted / removed 等系统回调。([Android Developers][11])

Tool：

```text
android.notification.list
android.notification.get
android.notification.dismiss
android.notification.action
android.notification.reply
```

不要把 Notification 对象直接交给 LLM。

转换成：

```json
{
  "id": "...",
  "package": "...",
  "title": "...",
  "text": "...",
  "timestamp": 0,
  "actions": [...]
}
```

---

# 18. Secret / API Key 改造

OpenSlate 当前 provider 直接通过：

```rust
std::env::var(...)
```

获取 API Key。([GitHub][12])

Android 不这么做。

增加：

```rust
trait SecretProvider {
    async fn get_secret(
        &self,
        key: &str
    ) -> Result<Option<SecretString>>;
}
```

Desktop：

```text
EnvSecretProvider
```

Android：

```text
AndroidSecretProvider
```

数据实际保存在：

```text
Android Keystore protected AES-GCM storage
```

Rust 请求 secret：

```text
SecretRequested(id, "OPENAI_API_KEY")
```

Kotlin 读取后：

```text
resolveSecret(id, value)
```

禁止：

```text
log API key
SQLite 保存明文 API key
config TOML 保存明文 API key
```

Rust 加：

```text
zeroize
secrecy
```

处理敏感字符串。

---

# 19. 文件系统设计

Android 第一版不要尝试模拟 Linux `/home`。

定义：

```text
workspace
config
data
cache
```

Kotlin 启动 Rust 时把真实路径传入。

例如：

```text
filesDir/openslate/workspace
filesDir/openslate/config
filesDir/openslate/data
cacheDir/openslate
```

以后再加入：

```text
Storage Access Framework
```

让用户授权某个外部目录。

Agent 文件 Tool：

```text
workspace.read
workspace.write
workspace.list
workspace.delete
```

必须限制在授权目录。

Termux 文件系统不与 App filesystem 混淆。

---

# 20. SQLite

第一版继续保留：

```text
openslate-store-sqlite
SQLx
SQLite bundled
```

避免同时重写 storage。

SQLx SQLite 默认可以静态链接 bundled SQLite。([docs.rs][13])

Android 构建时验证（第一版只验 aarch64）：

```text
aarch64-linux-android
```

`x86_64-linux-android` 随 CI / 模拟器阶段再验证。

如果 SQLx/SQLite NDK 出现无法解决的交叉编译问题，再做：

```text
Store trait
       │
       ├ native SqliteStore
       └ Android RoomStore
```

不要第一阶段就迁移 Room。

---

# 21. Model Provider

第一版继续让模型请求发生在 Rust。

保留：

```text
openslate-model-genai
```

当前这个 crate 已经把 genai 类型隔离在 provider 实现中，core 本身保持 provider-agnostic，这个方向是正确的。([GitHub][14])

因此：

```text
Android
     │
secret provider
     ▼
Rust GenaiProvider
     │
     ▼
OpenAI / Anthropic / Gemini /
OpenRouter / compatible endpoint
```

以后如果需要：

```text
local llama.cpp
MLC
MediaPipe
```

再实现新的 ModelProvider。

---

# 22. MCP

MCP 分两类。

Native Rust 中保留：

```text
Streamable HTTP MCP Client
```

不要在 Android 中直接使用：

```text
child-process MCP
```

如果用户希望运行本地 MCP：

```text
Android
   │
   ▼
Termux
   │
   ▼
MCP server
```

再由 OpenSlate：

```text
HTTP
```

连接。

也就是说：

```text
stdio MCP
```

属于 desktop/Termux capability，不属于 Android App 自身。

---

# 23. PTC / QuickJS

PTC 不删除。

但 **MVP 默认关闭**。

当前 OpenSlate 使用：

```text
rquickjs 0.12.2
```

PTC 已经有：

```text
memory limit
timeout
tool call budget
host bridge
no network
credential isolation
```

这些设计很好。([GitHub][15])

升级：

```text
rquickjs 0.14.0（2026-09 发布，当前最新）
```

目前 rquickjs 0.14 已经正式支持：

```text
wasm32-wasip1
wasm32-wasip2
wasm32-unknown-unknown
```

但其正式平台表仍没有把 Android 列为 shipped/tested target；文档仅说明其他 target 可以尝试 `bindgen`。([docs.rs][16])

因此执行顺序：

```text
MVP
PTC disabled

↓

测试 rquickjs + bindgen + Android NDK

↓

通过
enable Android PTC

↓

不通过
将 PTC executor 单独放到 WASM runtime
```

绝对不要让 QuickJS Android 编译问题卡住整个 Mobile Agent。

---

# 24. WASM 在本项目中的正确位置

WASM 用于：

```text
Skill
Plugin
第三方逻辑
不可信代码
```

而不是第一版主 Agent Core。

引入：

```text
openslate-wasm-runtime
```

首选：

```text
Wasmtime
```

接口使用：

```text
WASI Component Model
WIT
```

例如：

```wit
package openslate:skill;

interface host {
    call-tool: func(
        name: string,
        arguments-json: string
    ) -> result<string, string>;

    log: func(
        level: string,
        message: string
    );
}
```

Skill：

```text
github-agent.wasm
```

无法直接：

```text
访问 Android
访问 filesystem
访问 network
启动 process
调用 Shizuku
```

除非 manifest 授权。

---

# 25. WASM Skill Manifest

例如：

```toml
id = "github-helper"
version = "1.0.0"
entry = "github-helper.wasm"

[limits]
memory_mb = 64
timeout_ms = 30000
max_tool_calls = 16

[capabilities]
tools = [
    "network.http",
    "workspace.read"
]

network_hosts = [
    "api.github.com"
]
```

另一个 Android Skill：

```toml
[capabilities]

tools = [
    "android.app.list",
    "android.ui.snapshot",
    "android.ui.click"
]
```

不能请求：

```text
shizuku.exec
termux.shell
```

除非用户明确批准。

---

# 26. Capability 系统

Android App 启动后生成：

```json
{
  "android_api": true,
  "accessibility": true,
  "notification_listener": true,
  "media_projection": false,
  "shizuku": {
    "available": true,
    "uid": 2000
  },
  "termux": {
    "available": true
  }
}
```

ToolRegistry 根据 capability 动态注册工具。

不要向模型暴露：

```text
当前根本无法执行的 tools
```

例如用户没开 Accessibility：

```text
android.ui.click
```

不注册。

---

# 27. Tool 风险体系

继续利用 OpenSlate 已有 ApprovalManager。

当前 OpenSlate 本身已经具有 approval、risk level 和针对 shell/run_code 的交互审批机制。([GitHub][17])

Mobile 增加四级风险：

| 风险       | 示例                                                                  | 默认     |
| -------- | ------------------------------------------------------------------- | ------ |
| Low      | device.info、package.info                                            | 自动     |
| Medium   | launch_app、scroll、back                                              | 自动/可配置 |
| High     | click、set_text、notification.reply、termux.exec                       | 需要策略判断 |
| Critical | arbitrary shell、root、install/uninstall、clear data、permission change | 每次审批   |

特别是：

```text
termux.shell
shizuku.exec
package.clear_data
package.uninstall
permission.grant
permission.revoke
```

永远不能被：

```text
Approve All
```

永久白名单自动放行。

Critical 必须逐次审批。

---

# 28. Tool Policy Engine

执行路径必须变成：

```text
Model Tool Call

      ↓

Schema validation

      ↓

Capability check

      ↓

Policy Engine

      ↓

Approval if needed

      ↓

Android Host

      ↓

Audit

      ↓

Result
```

Audit 至少记录：

```text
timestamp
agent_id
tool
arguments redacted
risk_level
decision
result status
duration
backend
```

敏感字段必须：

```text
***REDACTED***
```

---

# 29. Android UI

第一版 UI 只做四个主要页面：

```text
Chat

Capabilities

Models

Settings
```

Chat 支持：

```text
streaming text
reasoning indicator
tool execution
approval card
cancel
retry
session
```

Capabilities 页面显示：

```text
Shizuku
Termux
Accessibility
Notification
Screen capture
```

每一项显示：

```text
Available
Permission missing
Service disconnected
Unsupported
```

不要把这些错误隐藏起来。

---

# 30. 手机 Agent 的目标交互

用户：

```text
帮我看看手机里哪些 App 占空间比较大
```

Agent：

```text
android.app.list
       ↓
Shizuku package/storage info
       ↓
summarize
```

用户：

```text
打开设置，把某个 App 的页面打开
```

优先：

```text
Intent
```

而不是 UI 自动化。

用户：

```text
帮我在某 App 里打开某页面
```

如果不能 Intent：

```text
Accessibility snapshot
↓
click node
↓
snapshot
```

用户：

```text
clone 这个 repo，然后跑测试
```

Agent：

```text
termux.exec git clone
termux.exec cargo/test/python/npm...
```

这就是最终体验。

---

# 31. Android 发行策略

第一阶段按：

```text
GitHub Release APK
```

设计。

可进一步考虑：

```text
F-Droid
```

不要把 Google Play 当第一发行目标。

原因之一是 Google Play 当前明确规定：使用 Accessibility API 的普通应用，不得通过 Accessibility 自主发起、规划和执行动作/决策；确定性规则自动化例外，真正用于辅助残障人士的 Accessibility Tool 另有规则。([Google Help][18])

所以：

> 通用自主手机 Agent 与 Play Accessibility policy 天生存在冲突。

工程上不要因此削弱核心能力。

---

# 32. 第一阶段：建立 Mobile build

首先新增：

```text
openslate-mobile
openslate-mobile-ffi
```

确保：

```text
cargo test --workspace
```

原有 Desktop 全部继续通过。

建立 Android Rust build（cargo-ndk 4.1.2 + NDK r29；实体机 arm64 直测，第一版只编 arm64-v8a）：

```bash
cargo ndk \
  -t arm64-v8a \
  -o android/app/src/main/jniLibs \
  build \
  -p openslate-mobile-ffi \
  --release
```

产物：

```text
libopenslate_mobile.so
```

第一阶段禁止加入：

```text
Shizuku
Accessibility
Termux
WASM
```

先完成：

```text
Android UI
↓
Rust
↓
LLM
↓
stream response
```

---

# 33. 第二阶段：平台抽象

需要解决当前 desktop assumption。

重点移除 core 对：

```text
std::env
dirs::home_dir
current_dir
tokio::process
shell
MCP child process
```

的直接假设。

新增：

```rust
PlatformPaths
SecretProvider
HostToolExecutor
```

但不要为了架构纯洁一次性重写所有东西。

原则：

```text
能够保持 Desktop 行为不变的最小改造。
```

---

# 34. 第三阶段：基础 Android Tools

实现：

```text
android.device.info

android.app.list
android.app.info
android.app.launch
android.app.open_settings

android.intent.start

android.clipboard.get
android.clipboard.set
```

此阶段 Agent 已经能够：

```text
查询手机
启动 App
打开系统页面
```

但还不能操作任意 UI。

---

# 35. 第四阶段：Shizuku

接：

```text
Shizuku permission
Shizuku binder status
UserService
AIDL
```

然后实现：

```text
package
settings
activity
input
```

验收必须覆盖：

```text
Shizuku ADB UID 2000

和

Sui/root UID 0
```

两种模式。

Shizuku UserService 并不是普通 Android App process；官方也提醒很多 Context API 在 UserService 内并不可用。因此普通 Android API 留在 App 进程，UserService 只承载真正需要 shell/root identity 的能力。([GitHub][19])

---

# 36. 第五阶段：Termux

接入官方：

```text
RUN_COMMAND
```

支持：

```text
stdout
stderr
exit_code
timeout
cancel best-effort
```

测试：

```text
echo
python
node
git
```

Agent 应能够完成：

```text
termux.exec(
  executable="python",
  args=["-c", "print(2+2)"]
)
```

返回：

```json
{
  "exit_code": 0,
  "stdout": "4\n",
  "stderr": ""
}
```

---

# 37. 第六阶段：UI Agent

实现：

```text
AccessibilityService
UI snapshot serializer
node cache
node action
snapshot stale detection
```

第一批：

```text
android.ui.snapshot
android.ui.click
android.ui.long_click
android.ui.set_text
android.ui.scroll
android.ui.back
android.ui.home
```

完成后 Agent 才算真正具备：

```text
手机操作能力
```

---

# 38. 第七阶段：Screen + Vision

加入：

```text
MediaProjection
```

然后支持：

```text
android.screen.capture
```

UI Agent planner：

```text
Accessibility First
Vision Second
Coordinates Last
```

---

# 39. 第八阶段：WASM Skill

此时再增加：

```text
Wasmtime
Component Model
WIT
Skill manifest
Capability permission
Fuel/time/memory limits
```

Wasmtime Android runtime 必须封装：

```rust
trait WasmRuntime
```

禁止让业务层直接依赖 Wasmtime API。

例如：

```text
WasmRuntime

├ WasmtimeRuntime
└ FutureOtherRuntime
```

这是因为 Android 虽被 Wasmtime 支持，但官方仍称其测试程度较低。([GitHub][2])

---

# 40. 第九阶段：评估 Agent Core WASM

到这个阶段再做：

```bash
cargo build \
  -p openslate-kernel \
  --target wasm32-wasip2
```

目标不是替换 Android native core。

目标是验证：

```text
同一个 Agent Kernel
```

未来是否可以跑：

```text
Browser
Server
iOS host
plugin runtime
embedded runtime
```

如果需要大量牺牲 native 架构，则停止。

不要为了“全 WASM”破坏 Android 版本。

---

# 41. MVP 明确不做

MVP 不做：

```text
整个 OpenSlate wasm 化

WebView Agent Runtime

Tauri Mobile

Flutter/RN

root-only design

纯截图坐标 Agent

任意 shell 默认开放

Play Store 上架

WASM 第三方插件商店

iOS
```

这些全部不能阻塞 Android Agent 核心闭环。

---

# 42. 必须保持的工程原则

第一：

```text
Semantic Tool > Shell
```

第二：

```text
Android API > Shizuku > Accessibility > Vision > Coordinates
```

第三：

```text
Capability-based
```

第四：

```text
默认最小权限
```

第五：

```text
高风险操作必须 Human-in-the-loop
```

第六：

```text
Android Host 与 Agent Core 解耦
```

第七：

```text
Desktop OpenSlate 不能因为 Mobile 改造而退化
```

---

# 43. 必须加入 CI 的 build matrix

至少：

```text
Linux native
macOS native（如果已有 runner）
Android aarch64
Android x86_64
```

后期加：

```text
openslate-kernel wasm32-wasip2 compile check
```

但：

```text
整个 openslate workspace
```

不要求 wasm compile。

只有纯 Kernel/Skill SDK 要求。

---

# 44. 必须做的测试

Rust：

```text
Tool policy
Approval
HostCall request/resolve
Cancellation
Timeout
Protocol serialization
Session restore
Capability registry
```

Android：

```text
Shizuku unavailable
Shizuku disconnected
Shizuku ADB
Shizuku root

Termux absent
Termux permission denied
Termux command error

Accessibility disabled
Accessibility stale node

MediaProjection denied

App background/foreground

screen locked

process killed
```

还要测试 OEM：

```text
Pixel/AOSP
Samsung
Xiaomi/HyperOS
OPPO/ColorOS
```

尤其是：

```text
后台限制
电池管理
Service
```

---

# 45. MVP 最终验收标准

完成版本必须能现场演示以下完整流程：

```text
1. 安装 APK

2. 配置一个 LLM API

3. 正常聊天并流式输出

4. Agent 调用 Android tool

5. 打开一个 App

6. 读取当前 Accessibility UI Tree

7. 找到一个按钮并 semantic click

8. 输入文字

9. Shizuku 查询 package

10. Shizuku 执行 force-stop，
    并正确弹出审批

11. Termux 执行：
    python -c "print(2+2)"

12. 返回 stdout = 4

13. 用户点击 Cancel，
    当前 Agent run 可以停止

14. Tool audit 可以查看

15. Accessibility / Shizuku / Termux
    任意一个不存在时，
    Agent 不 crash，
    而是 capability graceful degradation
```

满足以上条件，才算：

> **OpenSlate Mobile Agent MVP 完成。**

---

# 46. 最终产品形态

最终 OpenSlate 不再只是 CLI Agent。

而是：

```text
                 OpenSlate Agent Kernel
                          │
        ┌─────────────────┼─────────────────┐
        │                 │                 │
    Desktop           Android            Server
        │                 │
        │        ┌────────┼─────────┐
        │        │        │         │
        │     Shizuku   Termux   Accessibility
        │
        └──────────── Tools / MCP ──────────────
                          │
                          ▼
                    WASM Skills
```

手机端真正的竞争力不是：

```text
“LLM 可以点屏幕”
```

而是同时拥有：

```text
LLM reasoning

+

Android semantic control

+

Shizuku system capability

+

Termux Linux runtime

+

MCP ecosystem

+

WASM sandboxed plugins

+

OpenSlate multi-agent / approval / audit
```

这才是值得构建的 Mobile Agent Runtime。

---

# 47. 最终执行原则

如果实现者对某个选择产生分歧，以这个优先级决策：

```text
可靠性
>
权限安全
>
语义化接口
>
代码复用
>
性能
>
架构美观
```

不要为了“纯 WASM”“纯 Rust”“全部跨平台”等目标牺牲 Android Agent 的实际可靠性。

**最终架构结论：**

```text
Android UI:
Kotlin + Compose

Agent Runtime:
Native Rust / libopenslate.so

Bridge:
UniFFI + async request/resolve protocol

LLM:
现有 openslate-model-genai

Storage:
现有 SQLite，优先继续使用

System privilege:
Shizuku UserService

Linux execution:
Termux RUN_COMMAND

UI automation:
Accessibility first

Screen fallback:
MediaProjection + VLM

Remote tools:
MCP HTTP

Untrusted extensions:
WASM Component / WIT

PTC:
保留，Android MVP feature-gate

Future portability:
逐步让 Agent Kernel wasm32-wasip2 compatible
```

**不要把整个 OpenSlate WASM 化作为 Android 项目的第一步。**

先做出一个稳定的：

> **OpenSlate Native Mobile Agent**

然后让 WASM 成为它的插件、安全和跨平台层。

这是本方案的核心技术决策。

这份可以直接转给开发的人。我特意把“**哪些东西不要做**”也写死了，因为这个项目最容易走偏的地方就是一开始陷入全 WASM、JNI、QuickJS、WebView 或坐标自动化，最后半年都没形成手机 Agent 闭环。

[1]: https://github.com/Howardzhangdqs/OpenSlate/blob/master/Cargo.toml "https://github.com/Howardzhangdqs/OpenSlate/blob/master/Cargo.toml"
[2]: https://github.com/bytecodealliance/wasmtime/blob/main/docs/stability-platform-support.md "https://github.com/bytecodealliance/wasmtime/blob/main/docs/stability-platform-support.md"
[3]: https://raw.githubusercontent.com/Howardzhangdqs/OpenSlate/master/crates/openslate-core/src/tool.rs "https://raw.githubusercontent.com/Howardzhangdqs/OpenSlate/master/crates/openslate-core/src/tool.rs"
[4]: https://raw.githubusercontent.com/Howardzhangdqs/OpenSlate/master/crates/openslate-protocol/src/lib.rs "https://raw.githubusercontent.com/Howardzhangdqs/OpenSlate/master/crates/openslate-protocol/src/lib.rs"
[5]: https://developer.android.com/develop/background-work/services/fgs/restrictions-bg-start "https://developer.android.com/develop/background-work/services/fgs/restrictions-bg-start"
[6]: https://developer.android.com/develop/background-work/services/fgs/service-types?authuser=1&hl=en "https://developer.android.com/develop/background-work/services/fgs/service-types?authuser=1&hl=en"
[7]: https://developer.android.com/reference/android/accessibilityservice/AccessibilityService "https://developer.android.com/reference/android/accessibilityservice/AccessibilityService"
[8]: https://developer.android.com/reference/kotlin/android/media/projection/MediaProjection "https://developer.android.com/reference/kotlin/android/media/projection/MediaProjection"
[9]: https://github.com/RikkaApps/Shizuku-API/blob/master/api/src/main/java/rikka/shizuku/Shizuku.java "https://github.com/RikkaApps/Shizuku-API/blob/master/api/src/main/java/rikka/shizuku/Shizuku.java"
[10]: https://github.com/termux/termux-app/wiki/RUN_COMMAND-Intent/eb108f7977938031f237ca624c48f9e38034ebe0 "https://github.com/termux/termux-app/wiki/RUN_COMMAND-Intent/eb108f7977938031f237ca624c48f9e38034ebe0"
[11]: https://developer.android.com/reference/kotlin/android/service/notification/NotificationListenerService "https://developer.android.com/reference/kotlin/android/service/notification/NotificationListenerService"
[12]: https://raw.githubusercontent.com/Howardzhangdqs/OpenSlate/master/crates/openslate-app/src/provider.rs "https://raw.githubusercontent.com/Howardzhangdqs/OpenSlate/master/crates/openslate-app/src/provider.rs"
[13]: https://docs.rs/crate/sqlx-sqlite/0.9.0 "https://docs.rs/crate/sqlx-sqlite/0.9.0"
[14]: https://raw.githubusercontent.com/Howardzhangdqs/OpenSlate/master/crates/openslate-model-genai/src/lib.rs "https://raw.githubusercontent.com/Howardzhangdqs/OpenSlate/master/crates/openslate-model-genai/src/lib.rs"
[15]: https://raw.githubusercontent.com/Howardzhangdqs/OpenSlate/master/crates/openslate-ptc/src/lib.rs "https://raw.githubusercontent.com/Howardzhangdqs/OpenSlate/master/crates/openslate-ptc/src/lib.rs"
[16]: https://docs.rs/crate/rquickjs/latest "https://docs.rs/crate/rquickjs/latest"
[17]: https://raw.githubusercontent.com/Howardzhangdqs/OpenSlate/master/crates/openslate-app/src/wiring.rs "https://raw.githubusercontent.com/Howardzhangdqs/OpenSlate/master/crates/openslate-app/src/wiring.rs"
[18]: https://support.google.com/googleplay/android-developer/answer/10964491?hl=en-EN "https://support.google.com/googleplay/android-developer/answer/10964491?hl=en-EN"
[19]: https://github.com/RikkaApps/Shizuku-API/blob/master/README.md?plain=1 "https://github.com/RikkaApps/Shizuku-API/blob/master/README.md?plain=1"
