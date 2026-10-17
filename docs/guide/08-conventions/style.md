# 08-conventions · 命名、模块组织与注释风格

> 从源码提炼的既有风格——新代码应与它们保持一致。

## 1. 分层与模块组织

- **Rust 分层**：`openslate-mobile`（组合，**无 Android 依赖，desktop 可测**）→ `openslate-mobile-ffi`（**唯一允许依赖 uniffi 的层**，包一层 `Arc<MobileRuntime>`，不污染下层）→ Kotlin `bridge/`（宿主桥）→ Kotlin `ui/`（Compose）。
- 下层 crate 不得反向依赖：`openslate-mobile` 不 import uniffi；`openslate-session` / `openslate-app` 不知道 mobile 的存在。
- **Kotlin 包结构**（`dev.openslate.mobile`）：
  - 根：`MainActivity` / `OpenSlateApp`
  - `.bridge`：Rust 桥 + Android 能力宿主（RuntimeBridge / SecretStore / TermuxExec / McpHostManager / ShizukuGrant / LogExporter / AppPrefs）
  - `.service`：前台服务（AgentService）
  - `.ui`：Compose 页面与基础件
  - UniFFI 生成代码：`uniffi.openslate.mobile`
- **页面文件按 screen 切分**（`ChatScreen` / `SettingsScreen` / `HistoryScreen` / `KeysScreen`；`ModelRegistryScreen.kt` 现 hosting 内嵌设置页的 `ModelRegistrySection` 子页，无独立路由）；可复用基础件（`SoftButton` / `Section` / `DropdownChoice` / `AppDialog` 等）留在 `ui/` 顶层。页面超大（SettingsScreen ~2900 行）是现状，**新增区块优先抽 `XxxSection` 私有 composable**，不再往单文件里堆。

## 2. 命名约定

| 对象 | 约定 | 例 |
|------|------|-----|
| FFI Record（Rust） | `XxxDto` 后缀 | `MobilePathsDto` |
| Kotlin UI 投影 | `XxxUi` 后缀 | `ProviderUi` / `ModelUi` / `ToolInfoUi` / `SessionSummaryUi` |
| JSON 信封 `type` 字段 | snake_case | `host_call_requested` / `run_title` |
| bash 工具名 | 固定 `bash` / `termux_bash`；历史名 `shell.run` / `termux.run` 只作清理与兼容 | `exec.rs:38-42` |
| Rust 模块内分区注释 | `// ── 区名 ────…` 分隔线 | 见 `runtime.rs` / `exec.rs` |
| 日志 tag | 见 [errors-and-logging.md](errors-and-logging.md) §5.1 | |

## 3. 状态管理约定

- **无 ViewModel 层**：`RuntimeBridge` object 单例 + `StateFlow`；Compose 经 `collectAsState` 收集。
- **transcript 完整镜像只在 Rust/DB**，Kotlin 持渲染投影（`UiEntry`）；delta 增量并入最后一条 Assistant 条目。
- 新增可持久化设置必须补 restore 路径（`AppPrefs` + `RuntimeBridge.restore*` / `ThemePrefs.read`），否则重启即丢。

## 4. 注释与文档风格

- 注释以**中文**为主（Rust doc `//!` 模块头、`///` 项注释、Kotlin KDoc 均如此）。
- **关键设计决策写明「为什么」并标注出处**：`PLAN §5`、`PLAN §18`、`AGENTS.md`、「见 03-models 章」等。新代码保持该风格——后人（含 AI agent）靠出处回溯决策语境。
- **Rust 模块头 `//!` 写职责 + 与其他模块的关系图**（ASCII 图）：见 `crates/openslate-mobile/src/lib.rs`、`events.rs`、`hostcall.rs` 的模块头。
- **版本锁定要写理由**：根 `Cargo.toml` 中 ratatui `=0.30.2`、`[patch.crates-io]` 各条都带注释说明原因与「何时可移除」；`app/build.gradle.kts` 的 debug keystore 签名也带「正式发布前必须换」注释。**新增 pin/patch 必须同样注明**。
- 有「机制化」语义的守卫要写清替代了什么约定（例：`runtime.rs` SetApiKey 封禁注释——「此前靠宿主自觉不发该消息的约定约束，这里把约定变成机制」）。

## 5. 其他小约定

- 时序敏感的等待一律带超时（总表见 [errors-and-logging.md](errors-and-logging.md) §4）。
- debug 专属能力（`DebugReceiver` 广播注入）放 `android/app/src/debug/` 源集，release 不得携带。
- 桌面 builtin fs/shell 工具在 mobile 默认关闭（`[builtin_tools] enabled = false`）；shell 能力经 `exec.rs` 的 bash 后端多选（native / termux）注入。
- 本机网络适配（aliyun 镜像、`openslate-responder`、askama/rquickjs-sys/genai patch）**勿"修复"**，背景见 [../07-build-dev/troubleshooting.md](../07-build-dev/troubleshooting.md)。
