# 05 · openslate-cli（二进制 `openslate`）：子命令与 REPL

> 面向接手本项目的 AI agent。CLI 是 desktop 主入口：一次性运行 / 交互 REPL /
> serve 转发 / init·validate·skills 杂项。装配链不在本 crate（在 openslate-app，
> 见 `app-wiring.md`）。

---

## 1. 文件布局（crates/openslate-cli/src/）

| 文件 | 职责 |
|------|------|
| `main.rs` | `#[tokio::main]`；clap 解析 `Cli { --config(全局), --log-level(默认 info), command }`；tracing 安装；子命令分派 |
| `cmd/mod.rs` | 子命令模块聚合 |
| `cmd/run.rs` | 单次 agent 运行 |
| `cmd/serve.rs` | **薄转发**到 `openslate_server::serve(ServeOptions)`（flag 解析后即交棒） |
| `cmd/validate.rs` | 配置校验 |
| `cmd/skills.rs` | 列出发现的 SKILL.md 目录 |
| `repl.rs` | 交互会话（`ReplSession`） |
| `input.rs` | stdin 管道 + `@file` 展开 |
| `spinner.rs` | `SpinnerCallback`（`ProgressCallback` 实现，CLI 侧进度显示） |
| `markdown.rs` | `markdown` cargo feature 开启才渲染；默认纯文本直出（历史非 TTY 行为，feature off 时有个 verbatim fallback 模块） |

CLI 侧以 `use openslate_app as wiring;`（main.rs:40）保持历史 `wiring::X` 路径；
新代码用显式模块路径。

## 2. 入口与全局 flag

`main.rs::main`（`#[tokio::main]`）：

- `Cli`（L47-58）：`--config`（global）、`--log-level`（default `"info"`）、`command`。
- 除 `serve` 外都装 stdout tracing（`SimpleFormatter`：`HH:MM:SS message`，
  ERROR/WARN 染色时间戳；serve 由 openslate-server 自装 server.log 文件 subscriber）。
- `--quiet`（run/chat 专用，`Cli::is_quiet` L60-68）把默认级别压到 warn。

## 3. 子命令（`Commands` 枚举，main.rs:71 起 → `cmd/` 模块）

| 子命令 | 实现 | flags | 说明 |
|--------|------|-------|------|
| `init` | `main.rs::run_init` | `--name` | 脚手架 `.openslate/openslate.toml` + `agents/root.md` + `prompts/default/prompt.md`（模板 `default_openslate_toml`，zhipu/glm-5.1 开箱配置） |
| `validate` | `cmd/validate.rs` | `--strict`（警告升错误） | 复用 `select_config_files`/`load_config_layered` 组装发现链但**保留自己的报错文案**；只读 |
| `skills` | `cmd/skills.rs` | — | 列出发现的 SKILL.md 目录 |
| `run` | `cmd/run.rs` | `--agent`（默认 root）/ `--prompt` / `--profile`(default)/ `--format(text)` / `--output` / `--root-agent` / `--trace`(Chrome Trace JSON 导出) / `--yes`(自动批，覆盖 `[approval]`) / `--resume`(run id 断点续跑：持久化消息成为先前对话，同一 run id 继续；可配 `--prompt` 追加新用户回合) / `--quiet` | 单次 agent 运行：装配 → RunManager → execute。输入经 `input.rs`（stdin 管道 + `@file` 展开）；进度走 `SpinnerCallback` |
| `chat` | `repl.rs` | `--profile` / `--model`(别名覆盖) / `--quiet` | 交互会话 |
| `serve` | `cmd/serve.rs` | bind/port/auth-token/config | **薄转发**：`ServeArgs { bind, port, auth_token }` 镜像 `ServeOptions`，装配逻辑全在 openslate-server（见 `server.md`） |

## 4. REPL（repl.rs）

```rust
pub struct ReplSession { … }                       // L379
impl ReplSession {
    pub fn new(ctx: AppContext, profile: String, quiet: bool) -> Result<Self>;   // L401
    pub async fn run(&mut self) -> Result<()>;      // L452
    pub fn format_welcome(&self) -> String;         // L505
    // 内省（测试 / TUI 复用）：history / verbose / model_override / profile /
    // stats / session_run_id / approval_policy（L1250-1280）
}
```

要点：

- REPL 复用 `apply_approval` 的 **interactive=true** 语义（默认 `auto_except(["shell",
  "run_code"])`，见 `app-wiring.md` §审批）。
- `--model` 覆盖别名：每 turn 重建 provider（`build_provider_for_model`）。
- compact 摘要计价走 `ResolvedModel::cost_spec()`（与 runtime 循环同一份计价，见
  `03-models/model-genai.md` §4）。

## 5. 与其他入口的关系

- **provider**：每 run/turn 用 `wiring::build_provider_for_model` 重建（不缓存）。
- **审批**：交互提示（`ProgressCallback` 之外的审批回调）；`--yes` 强制 Auto。
- **配置写回**：无（validate 只读；改配置走编辑器或 TUI 的模型管理）。
- server/tui 复用 CLI 的 `serve` 子命令拉起后端（`openslate serve`）。

## 6. 改动检查清单

- 新子命令 → `Commands` 枚举 + `cmd/` 新模块；装配一律走 `wiring::build_app_context`，
  别复制装配链。
- 动输出渲染 → `markdown` feature 的默认 off 语义别破坏（非 TTY/无 feature 时 verbatim）。
- 动 `--resume` / trace 导出 → run 持久化语义在 store 层，先读 `02-core/openslate-store-sqlite.md`。
