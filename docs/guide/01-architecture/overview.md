# 01-1 · 项目概览

> 架构章入口之一。先读本文建立"这是什么"的图景，再读 [crate-layers.md](crate-layers.md)（分层规则）→ [domain-concepts.md](domain-concepts.md)（领域概念）→ [traits-and-abstractions.md](traits-and-abstractions.md)（关键抽象）→ [dataflow.md](dataflow.md)（端到端数据流）。

## 1. OpenSlate 是什么

OpenSlate 是一个 Rust workspace 实现的**多前端 Agent Runtime**：同一套核心链路

```text
配置加载 → 模型解析 → agent 循环 → 工具执行 → 持久化
```

驱动四个前端：

| 前端 | crate | 形态 |
|------|-------|------|
| CLI | `openslate-cli` | 一次性 `openslate run` + REPL + `serve` 子命令 |
| TUI | `openslate-tui` | 桌面终端 UI（ratatui，**纯 WS 客户端**，零本地引擎） |
| WS server | `openslate-server` | HTTP(REST) + WebSocket 单通道后端 |
| Android App | `openslate-mobile` + `openslate-mobile-ffi` | FFI 嵌入（UniFFI → Kotlin，Compose UI） |

两个关键技术选型：

- **模型调用走 genai 适配器**（`vendor/genai` 是带 patch 的独立源码副本，被 workspace `exclude`）：一个 `ModelProvider` 实现覆盖 OpenAI / Anthropic / Gemini / Ollama 四个协议族。genai 的类型被封死在 `openslate-model-genai` 内部，不进 core（见 crate-layers.md 的分层规则）。
- **工具体系基于 MCP + PTC**：外部 MCP server（stdio / Streamable HTTP）+ 内置 in-process MCP server（fs/shell/edit/skill，tuple transport、无子进程无序列化）+ PTC（`run_code`：模型写 JS async 箭头函数，在 QuickJS 沙箱内编排工具调用，每 run 一个 isolate、无网络、硬超时）。

## 2. Workspace 组成（14 crates）

根 `Cargo.toml` 定义 14 个 members；`vendor/genai` 被 `exclude`（带自己的 `[workspace]` 表，独立测试）。

### 一览表（按层分组）

| Crate | 层 | 一句话职责 | workspace 内依赖 |
|-------|----|-----------|-----------------|
| `openslate-core` | 核心层 | 领域模型 + 执行引擎（types/provider/tool/runner/runtime/run_manager/config/mcp/skills/approval/…） | mcp-builtin, ptc |
| `openslate-ptc` | 核心层（叶子） | QuickJS 沙箱：PTC 脚本执行器 + `ToolCallMode` 枚举 | 无 |
| `openslate-mcp-builtin` | 核心层（叶子） | 内置 in-process MCP server（read_file/write_file/shell/bash/edit_file/skill） | 无 |
| `openslate-mcp-host` | 独立 | Termux 侧 MCP 聚合宿主（Android 外部工具供给 + builtin bash/fs/edit 进程内拉起） | mcp-builtin（唯一白名单项） |
| `openslate-store-sqlite` | 数据层 | SQLite 持久化：7 张表、版本化迁移、RunRecorder | core |
| `openslate-protocol` | 协议层 | ClientMsg/ServerMsg + 全部 DTO（单一文件 crate，纯数据） | core |
| `openslate-model-genai` | 装配层 | `ModelProvider` 的 genai 实现（唯一实现） | core |
| `openslate-app` | 装配层 | `AppContext` / `wiring::build_app_context`：config→store→AgentTree→ToolRegistry→RunManager 装配链 | core, store-sqlite, model-genai |
| `openslate-session` | 会话层 | 传输无关会话核心：SessionCore/engine/approval/config_ops/dispatch | core, app, protocol, store-sqlite |
| `openslate-server` | 前端层 | REST + WS 单通道；ConnectionHub 扇出 | core, app, protocol, session, store-sqlite |
| `openslate-cli` | 前端层 | run/repl/serve 子命令 | core, store-sqlite, app, server |
| `openslate-tui` | 前端层 | ratatui 终端 UI（纯 WS 客户端） | core, protocol, server |
| `openslate-mobile` | 前端层 | Android 组合层：MobileRuntime/EventSink/registry 存储 | core, app, protocol, session, store-sqlite, model-genai |
| `openslate-mobile-ffi` | 前端层 | UniFFI 边界（.so 生成 + Kotlin 绑定） | mobile |

> 依赖关系的精确来源是各 crate 的 `Cargo.toml`；上表以 `cargo tree` 语义核对过。分层图与规则见 [crate-layers.md](crate-layers.md)。

## 3. 技术栈

| 类别 | 选型 | 备注 |
|------|------|------|
| 异步运行时 | tokio 1.51（`rt-multi-thread/macros/sync/time/io-util/fs/signal`） | CLI 的 Ctrl-C 取消走 `signal` |
| 取消 | `tokio-util::sync::CancellationToken` | 线程穿透 `execute_run` 全树共享 |
| 序列化 | serde + serde_json；`serde_norway` 0.9（serde_yml → RUSTSEC-2025-0068 之后的维护分支） | agent markdown 的 YAML frontmatter |
| 配置 | toml（读）+ **toml_edit**（写回，格式保持） | 写配置永远走 `core/src/config/persist.rs`，禁手拼 TOML 字符串 |
| 模型调用 | `vendor/genai`（patch 过；anthropic adapter 回传 thinking block 支撑多轮 extended thinking） | 见根 Cargo.toml `[patch.crates-io]` |
| MCP | rmcp（协议 2025-11-25）+ reqwest（rustls, default-features=false） | stdio / Streamable HTTP 两种 transport |
| HTTP 服务端 | axum（server crate） | 仅 server 依赖，protocol/session 均不依赖 |
| 存储 | sqlx 0.9（sqlite-bundled，**不用宏**，动态 SQL 走 `AssertSqlSafe`） | WAL + 单连接池 |
| 沙箱 | QuickJS（rquickjs-sys，vendor patch） | PTC 执行器 |
| FFI | UniFFI | mobile-ffi → Kotlin |
| TLS（Android） | webpki 静态根证书（`cfg(target_os = "android")` 下的 rustls 预配置） | rustls-platform-verifier 需 JNI 宿主，纯 .so 场景不可用，故弃之 |
| 追踪 | tracing（多 target：`openslate_approval` / `openslate_store` / `openslate_mcp` / `openslate_session`…） | Chrome Trace 导出（Perfetto JSON）另有一套 `TraceCollector` |
| UUID / 时间 | uuid（v4）、chrono | RunId = UUID v4 字符串 |

## 4. 测试设备约束（影响架构决策）

- 目标 API 区间 **26（Android 8.0）～ 37（Android 17）**，新 API 必须做版本判断（见 `AGENTS.md`）。
- 两台测试设备均 arm64；Android 构建用 `arm64-v8a`，mobile profile 必须 `panic=unwind`（UniFFI 要求）。
- Android 上无 JNI 宿主初始化 → TLS 用 webpki 静态根证书（core/provider.rs 与 model-genai 同款）。

## 5. 深入阅读

- crate 依赖分层图与规则：[crate-layers.md](crate-layers.md)
- 领域概念（Provider/Model/Agent/Run/Step/…）：[domain-concepts.md](domain-concepts.md)
- 关键 trait（ModelProvider/Tool/MessageSink/…）：[traits-and-abstractions.md](traits-and-abstractions.md)
- 端到端数据流：[dataflow.md](dataflow.md)
- 核心四 crate 模块级细节：[../02-core/](../02-core/) 目录
