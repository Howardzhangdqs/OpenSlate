# 01-2 · Crate 依赖分层

> 依赖关系的权威描述。**改动时必须维持分层规则**（§3）；任何 crate 间依赖变化按 `AGENTS.md` 约定需同步更新本文。

## 1. 分层依赖图（箭头 = 依赖方向，上层依赖下层）

```text
┌─ 前端层 ─────────────────────────────────────────────────────────────┐
│  openslate-cli ──── openslate-tui          openslate-mobile-ffi      │
│   (run/repl/         (纯 WS 客户端,          (UniFFI 边界)            │
│    serve 子命令)       ratatui)                  │                    │
│        │                 │                      ▼                    │
│        │                 │              openslate-mobile             │
│        ▼                 ▼              (Android 组合层:             │
│  ┌─openslate-server─┐   attach WS        MobileRuntime/EventSink)    │
│  │ REST+WS 单通道    │                                  │             │
│  └───────┬──────────┘                                   │             │
└──────────┼──────────────────────────────────────────────┼─────────────┘
           ▼                                              ▼
┌─ 会话层 ──────────────────────────────────────────────────────────────┐
│  openslate-session（传输无关会话核心: SessionCore/engine/approval/    │
│                     config_ops/session::dispatch）                    │
└──────┬──────────────┬───────────────────┬─────────────────────────────┘
       ▼              ▼                   ▼
┌─ 装配层 ──┐   ┌─ 协议层 ─────────┐   （session 还依赖 store-sqlite）
│openslate- │   │openslate-protocol│
│app        │   │ClientMsg/        │
│wiring+    │   │ServerMsg+DTO     │
│provider   │   └───────┬──────────┘
│工厂       │           │
└──┬──┬─────┘           │
   │  └──────────────────────────────┐
   ▼                                 ▼
┌─ 数据层 ──────────┐   ┌─ 核心层 ──────────────────────────────────────┐
│openslate-store-   │   │ openslate-core                                │
│sqlite             │──▶│ （领域核心：types/provider/tool/runner/       │
│SqliteStore/       │   │  runtime/run_manager/config/mcp/skills/…）   │
│RunRecorder        │   │    │            │                            │
└───────────────────┘   │    ▼            ▼                            │
                        │ openslate-ptc  openslate-mcp-builtin          │
                        │ （QuickJS 沙箱）（内置 MCP server: fs/shell/  │
                        │                edit/skill）                   │
                        └───────────────────────────────────────────────┘
  另有独立 crate：openslate-model-genai（ModelProvider 的 genai 实现，依赖 core）
                  openslate-mcp-host  （Termux 侧 MCP 聚合宿主，零 workspace 依赖）
```

## 2. 精确依赖表（来源：各 crate 的 `Cargo.toml`）

| Crate | workspace 内依赖 |
|-------|-----------------|
| `openslate-core` | `openslate-mcp-builtin`, `openslate-ptc` |
| `openslate-ptc` | 无 |
| `openslate-mcp-builtin` | 无 |
| `openslate-mcp-host` | **无**（刻意独立，可单独装进 Termux） |
| `openslate-store-sqlite` | core |
| `openslate-protocol` | core |
| `openslate-model-genai` | core |
| `openslate-app` | core, store-sqlite, model-genai |
| `openslate-session` | core, app, protocol, store-sqlite |
| `openslate-server` | core, app, protocol, session, store-sqlite |
| `openslate-cli` | core, store-sqlite, app, server |
| `openslate-tui` | core, protocol, server（注：仅用 server 的 `ServerInfo` 发现常量等轻量部分，不启动 server） |
| `openslate-mobile` | core, app, protocol, session, store-sqlite, model-genai |
| `openslate-mobile-ffi` | mobile |

vendor 说明：`vendor/genai` 在根 `Cargo.toml` 的 `exclude` 列表里——它有自己的 `[workspace]`，测试在 vendor 目录内独立跑；workspace 通过 `[patch.crates-io]` 把 genai 指到该副本。

## 3. 分层规则（改动时必须维持）

1. **core 不依赖任何前端/传输**。core 的 `Cargo.toml` 里没有 axum/tokio-WS/FFI；tokio 只为 stdio MCP transport 与 shell builtin 引入 `process` feature。genai 类型被封死在 `openslate-model-genai` 内部，不进 core——core 只见 `ModelProvider` trait 与自有类型。
2. **ptc 不得依赖 core**（方向是 core → ptc）。`openslate-ptc` 的公开 API 只用纯数据类型（如 `[ptc]` 配置与沙箱共享的 `ToolCallMode` 枚举），保证沙箱 crate 可独立演进/复用。
3. **protocol 是纯数据层**：不依赖 tokio/axum，server 与 client（TUI）两侧共用同一真相源。任何传输概念（连接、队列）不进 protocol。
4. **session 不依赖任何传输**（axum/WS/FFI 均不在依赖树）。WS server 与 mobile 共用一个 `SessionCore`，事件出口抽象成 `MsgSink` trait。
5. **mcp-host 零 workspace 依赖**：它是部署在 Termux 里的独立二进制，与主工程只共享 MCP 协议本身。

违反上述任一条（例如让 core 引用 protocol、让 ptc 引用 core、在 session 里 use axum）都会破坏既有的复用边界——review 时按红线处理。

## 4. 每层职责

### 核心层（core / ptc / mcp-builtin）

- `openslate-core`：领域类型系统（`types.rs` 的 ID newtype 家族、Message/Usage/ToolCall）、模型调用抽象（`provider.rs`）、agent 执行引擎（`runtime.rs` 单 agent 循环 + `runner.rs` 递归子代理编排 + `run_manager.rs` 入口）、配置解析/合并/写回（`config/`）、工具注册（`tool.rs`）、MCP 接入（`mcp.rs`）、审批（`approval.rs`）、技能（`skills.rs`）、提示词（`prompt.rs`）、路径发现（`paths.rs`）、Chrome Trace（`trace.rs`）、错误体系（`error.rs`）。详见 [../02-core/openslate-core.md](../02-core/openslate-core.md)。
- `openslate-ptc`：QuickJS 沙箱执行器。输入是模型写的 JS async 箭头函数字符串，沙箱内经桥接闭包调用宿主工具；无网络、硬超时、每 run 一个 isolate。
- `openslate-mcp-builtin`：内置工具的 in-process MCP server。用 tuple transport 直连（无子进程、无序列化开销），core 侧经 `connect_builtin_servers`（`core/src/mcp.rs:578`）挂进 ToolRegistry，工具名用裸名（`read_file` 等）。

### 数据层（store-sqlite）

- 7 张表（runs/execution_nodes/steps/messages/prompt_snapshots/audit_log/trace_events）+ 版本化迁移（`PRAGMA user_version`，当前 `SCHEMA_VERSION = 2`）+ `RunRecorder`（core `MessageSink` 的实现，逐消息落盘）。详见 [../02-core/openslate-store-sqlite.md](../02-core/openslate-store-sqlite.md)。

### 协议层（protocol）

- `ClientMsg`（上行 16 变体）/ `ServerMsg`（下行 21 变体）+ 全部 DTO。`PROTOCOL_VERSION = 1`，`hello.proto` 不等即拒连。详见 [../02-core/openslate-protocol.md](../02-core/openslate-protocol.md)。

### 装配层（app / model-genai）

- `openslate-app::wiring::AppContext`：config 加载（分层合并）→ 校验 → SqliteStore → AgentTree → ToolRegistry（含 MCP 连接）→ `RunManager`。**装配一次，多回合复用**。`AppContext` 字段声明顺序有 drop 序约束：`mcp_connections`（`McpConnectionGuard`）必须声明在 registry 之后，保证 drop 时 registry（内含持有 `ServerSink` 的 `McpTool`）先死、连接后关。
- `openslate-model-genai`：`GenaiProvider` —— `ModelProvider` 的唯一生产实现；`build_provider_for_model(config, alias)` 是生产用的 provider 工厂。

### 会话层（session）

- `SessionCore`（引擎 + 转写镜像 + 配置镜像 + 会话统计）+ `MsgSink`（事件出口）+ `SessionApprovalBridge`（审批桥）+ `dispatch`（ClientMsg 派发）。详见 [../02-core/openslate-session.md](../02-core/openslate-session.md)。

### 前端层

- **cli**：`run`（一次性）、`repl`（多回合共享 run id）、`serve`（起 server）。装配走 `openslate_app::wiring`。
- **tui**：纯 WS 客户端。无 `--server` 时经发现链（flag > env > `server.json`）自动 attach。
- **server**：REST（`/api/config`、`/api/sessions` 等）+ WS 单通道；`ConnectionHub` 实现 `MsgSink`。
- **mobile / mobile-ffi**：`MobileRuntime` 组合 session；`EventSink` 实现 `MsgSink`（单队列 + 泵线程 → JSON 回调）；UniFFI 边界只传 JSON 字符串，复用协议语义，**不发明第二套 onToken/onTool 接口**。

## 5. 反向依赖检查清单（改代码时自查）

- 给 core 加依赖前问：这个 crate 是否比 core 更"核心"？protocol/store 都依赖 core，core 反向引用它们会成环。
- 给 ptc 加 core 依赖 = 直接违反规则 2。
- 在 session 里 `use axum::*` / 在 protocol 里 `use tokio::*` = 违反规则 3/4。
- mcp-host 加任何 workspace 依赖 = 违反规则 5。
