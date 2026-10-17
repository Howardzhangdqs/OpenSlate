# 04 · openslate-mcp-host：Termux 侧 MCP 聚合宿主

> 面向接手本项目的 AI agent。手机端 MCP 方案：Android App 不能随便 spawn 子进程；
> Termux 可以。mcp-host 作为**独立二进制**跑在 Termux 里，把任意多个 stdio MCP server
> 聚合成一个 Streamable HTTP 端点，App 作为 rmcp client 只连这一个。

---

## 1. 架构

```text
OpenSlate App (rmcp client) ──HTTP+Bearer──▶ 127.0.0.1:8765/mcp
                                              ┌─ openslate-mcp-host ─┐
                                              │ 聚合 tools/list      │
                                               │ 裸名 + `alias__tool` │
                                              ├──────────────────────┤
                                               │ builtin ×3（进程内） │
                                              │ stdio 子进程 × N     │
                                              └──────────────────────┘
```

上游 Streamable HTTP **stateless**：`legacy_session_mode=false` +
`json_response=true`（简单请求-响应走 `application/json`；handler 发通知/请求时 rmcp
自动 fallback SSE，progress 不丢）。下游 stdio 懒连接。builtin server（bash/fs/edit）
是 `openslate-mcp-builtin` 的进程内 server，tuple transport 直连（无子进程），启动时
一次拉起、常驻（见 §6.5）。

## 2. 模块与关键类型（crates/openslate-mcp-host/src/，共 ~970 行）

| 文件 | 行数 | 内容 |
|------|------|------|
| `src/lib.rs` | 207 | `HostConfig`（L76）、`BuiltinConfig`（L99）、`load_config`（L120）、`generate_token`（L128）、`build_router`（L139）、`resolve_bind`（L172）、`auth_middleware`（L181） |
| `src/downstream.rs` | 251 | `ServerSpec`（L35）、`DownstreamHub`（L51）、`CallFailure`（L58）、`PREFIX_SEP = "__"`（L30）、`collect_tools`（L173）、`call`（L215） |
| `src/handler.rs` | 115 | `AggregateHandler`（L25，rmcp `ServerHandler`，`Clone` 廉价——内部 `Arc`；裸名→builtin / `__`→下游 两分路由） |
| `src/builtin.rs` | 221 | `BuiltinHub`（L53）：builtin server 群拉起（L145 `spawn`）、裸名工具清单、裸名路由（L197 `call`）、`$PREFIX` bash 路径解析 |
| `src/bin/mcp-host.rs` | 100 | CLI 二进制：读清单 → 起 builtin 群 → 起 axum → 常驻 |
| `src/bin/mock-server.rs` | 74 | 测试用 mock stdio server（集成测试的下游） |

### 2.1 常量与超时

```rust
pub const DEFAULT_BIND: &str = "127.0.0.1:8765";  // lib.rs:49 — 只绑 loopback，
                                                   // Android 上 127.0.0.1 全设备共享，
                                                   // 必须配合 token，绝不 0.0.0.0
pub const MCP_PATH: &str = "/mcp";                 // lib.rs:52 — App 侧 url = http://127.0.0.1:8765/mcp
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);  // downstream.rs:22 — 下游 spawn+握手（npx 首次解析慢）；
                                                             // builtin 进程内握手复用同值
const CALL_TIMEOUT: Duration = Duration::from_secs(300);    // downstream.rs:25 — 单次调用（与 core 侧 DEFAULT_CALL_TIMEOUT 对齐）；
                                                             // builtin 调用同样外包此值
pub const PREFIX_SEP: &str = "__";                 // downstream.rs:28
```

## 3. 宿主清单（HostConfig，TOML）

```toml
bind = "127.0.0.1:8765"
token = "optional-managed-token"     # 缺省则自动生成写 .token 旁车文件；Some("") 显式关鉴权
[builtin]                            # 可选段；缺省 = enabled+bash 开、root=$HOME
bash = true
root = "/data/data/com.termux/files/home"
[servers.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/sdcard"]
[servers.git]
command = "/data/data/com.termux/files/home/.npm-global/bin/mcp-server-git"
```

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {           // lib.rs:76-90
    pub bind: Option<String>,     // 缺省 DEFAULT_BIND
    pub token: Option<String>,    // Some("") = 显式关闭鉴权（仅限本机调试）
    pub servers: BTreeMap<String, ServerSpec>,
    pub builtin: BuiltinConfig,   // serde default：旧清单无此段 = 全默认
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]   // Default 手写为"全开"——bool 的
pub struct BuiltinConfig {         // derive 默认 false 会把旧清单悄悄关掉
    pub enabled: bool,             // 默认 true：总开关，false → 零裸名工具
    pub root: Option<PathBuf>,     // 默认 None → $HOME（bash cwd / fs 沙箱 / edit root）
    pub bash: bool,                // 默认 true：BashServer 嵌入 = enabled && bash
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSpec {           // downstream.rs:35-41
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// 追加到子进程环境（不替换整个环境——PATH 等继承自 Termux）。
    pub env: Option<HashMap<String, String>>,
}
```

`load_config`（lib.rs:120）= 读文件 + `toml::from_str`，`deny_unknown_fields` 让拼错的键
当场报错而不是静默忽略。

### 3.1 `[builtin]` 配置表

| 键 | 类型 | 默认 | 语义 |
|----|------|------|------|
| `enabled` | bool | `true` | 总开关；`false` 时不拉起任何 builtin server，裸名工具全部消失 |
| `root` | path | `$HOME` | 工作目录：bash 的 cwd、fs 的沙箱 root、edit 的 root 三者共用 |
| `bash` | bool | `true` | 是否嵌入 BashServer（`bash` 工具）；fs/edit 只跟随 `enabled`，**没有独立开关**（无环境依赖，不需要单独逃生门） |

**向后兼容（硬约束）**：无 `[builtin]` 段的旧清单必须原样可用——serde default 走
手写 `Default`（全开）。**App 侧（Kotlin manifestToml）永不写此段**：新 app +
旧 host 二进制共存时，旧 host 的 `deny_unknown_fields` 不会因未知段解析失败
（版本偏斜防线，Phase C 已固化为约定）。

## 4. 鉴权（token）

- `generate_token()`（lib.rs:87-92）：`/dev/urandom` 读 32 字节 → hex（零额外依赖；
  Termux/桌面 Linux 均有 urandom）。
- token 三态（bin/mcp-host.rs:52-68）：`Some(t)` 非空 → 用；`Some("")` → 显式关鉴权
  （调试，不生成）；`None` → 生成随机 token 并写旁车文件
  `<config>.toml.token`（`config_path.with_extension("toml.token")`）。App 侧可经
  Termux RUN_COMMAND `cat` 取回（TermuxExec 回传通道）。
- `auth_middleware`（lib.rs:135-161）：接受 `Authorization: Bearer <token>` **或**
  `X-API-Key: <token>`；不匹配 → 401 + `WWW-Authenticate: Bearer`。常数时间比较省略
  （token 是高熵随机值，非密码场景 timing 攻击无意义）。
- `build_router(hub, builtin, token)`（lib.rs:139-166）：`/mcp` 挂 `StreamableHttpService` +
  `LocalSessionManager` + stateless 配置；`Some("")`/`None` 不挂鉴权层。拆出此函数是为
  集成测试能自行 bind 随机端口。

## 5. 下游管理（DownstreamHub）

`DownstreamHub { slots: Mutex<HashMap<String, Slot>> }`（downstream.rs:49-52；
**std Mutex**，临界区只做指针存取/规格读取，绝不跨 await 持锁）。`Slot { spec, peer:
Option<Peer> }`，`Peer = Arc<RunningService<RoleClient, ClientConfig>>`。

### 5.1 懒连接与失效重连（v1 保活策略）

- `peer(alias)`（L128-158）：快路径查已有连接；慢路径取规格（立即放锁）→ `spawn_peer`
  → 存回。并发窗口内别人已连上则直接复用。
- `spawn_peer`（L72-96）：tokio Command（非 std）+ `TokioChildProcess::builder(cmd)`
  .`stderr(Stdio::null())`（**下游子进程 stderr 丢弃**——协议走 stdout，启动横幅会刷屏）；
  `client.serve(transport)` 外包 30s 超时；client 身份
  `Implementation::new("openslate-mcp-host", CARGO_PKG_VERSION)`。
- `invalidate(alias)`（L161-167）：弃置连接（槽位置 None），下次调用重新 spawn + 握手。
  **call/list 传输错误即弃置**。代价：故障后首个请求承担重连延迟（本地子进程 + 握手
  通常 <1s）。比常驻守护循环简单且不会误杀慢启动的 server。
- `call(alias, tool, arguments)`（L213-248）：传输类失败**重连一次再试**（`for _attempt in 0..2`）；
  JSON-RPC 层错误（工具参数被拒等）**不重连**直接带给上游（连接大概率仍可用）；
  调用超时 `CALL_TIMEOUT` 后 invalidate。
- `CallFailure`（L56-61）：`UnknownServer`（无法路由）vs `Transport(String)`
  （连接/握手失败、调用超时或被下游 JSON-RPC 层拒绝）。

### 5.2 聚合

- `collect_tools()`（L171-179）：按清单顺序逐 alias 调 `tools_of`，**单个下游故障只跳过
  并 warn**（其工具暂不可见）——与 core 侧「连接失败不拖垮启动」语义一致。
- `tools_of`（L181-210）：`list_all_tools` 外包 300s 超时；每个 Tool clone 后改
  `name = format!("{alias}{PREFIX_SEP}{}", t.name)`（Tool 为 `non_exhaustive`，不能
  struct literal 重建）。list 失败/超时 → invalidate + 空数组。

## 6. 聚合协议（AggregateHandler，handler.rs）

- `get_info`（L41-47）：capabilities **有意只声明 tools**——resources / prompts / logging
  不在 v1 计划内（主动裁剪，非协议损失；OpenSlate 需要时再扩声明）。
- `list_tools`（L55-70）：**builtin 裸名工具在前 + `hub.collect_tools()` 在后**。
  v1 **不分页**（忽略 `next_cursor`，一次返回全部；本地聚合器工具量级远低于分页
  阈值 8192 tokens）。
- `call_tool`（L74-114）按名字**两分路由**：
  - 含 `__` → `alias__tool` 拆给下游 `tool`，结果原样透传（含 content blocks 与
    `is_error`，不吞下游失败语义）；
  - 裸名（无 `__`）→ `builtin.call(name)`（见 §6.5）。未知裸名返回
    `"unknown builtin tool '<name>' (known builtin tools: …)"`——列出已知裸名、
    builtin 关闭时明示 `[builtin] enabled = false`；**旧文案"missing the server
    prefix"已删除**（裸名路由落地后该语义失效）。
  - 两类失败都返回 **`Ok(CallToolResult::error(...))`**（协议层不报错，错误文本给 LLM）。
- 工具名分层：host 内部 `alias__tool` 双下划线 vs builtin 裸名 vs core 侧
  `{server}_{tool}` 单下划线（core 命名空间前缀）——裸名经 App 侧 builtin 模板
  展开为 `termux_bash` 等（core 侧双模板机制，见 `02-core/openslate-core.md`）。

### 6.5 builtin server 群（builtin.rs，BuiltinHub）

host 启动时（bin/main）以 **in-process tuple transport** 拉起三台 builtin server，
常驻整个进程生命周期：

| server | 工具 | 嵌入条件 |
|--------|------|---------|
| `BashServer::new(root, bash_path)` | `bash` | `enabled && bash` |
| `FsServer::new(root)` | `read_file` / `write_file` | `enabled` |
| `EditServer::new(root)` | `edit_file` | `enabled` |

- **连接方式**照抄 core 的 `connect_builtin_server`（core/mcp.rs:591）：futures mpsc
  双通道零序列化，握手与列工具各包 `CONNECT_TIMEOUT`。mcp-builtin 自带的
  `testing::spawn_server` 是 `#[cfg(test)]` 专用、不可跨 crate 复用，host 侧自备
  等价实现（`connect_builtin`）。
- **bash 路径解析在 host 侧**：`$PREFIX/bin/bash`（`$PREFIX` 缺省
  `/data/data/com.termux/files/usr`）→ 都不存在时回退 PATH 上的 `bash`（解析延迟
  到 spawn，缺 bash 的环境在调用时以工具错误回喂 LLM）。`$PREFIX` 是 Termux 环境
  知识，不进通用的 mcp-builtin crate。
- **执行语义**（30s/120s 超时阶梯、64KB 头尾截断、敏感 env 脱敏、exit_code +
  stdout/stderr 分节）由 mcp-builtin 的 `shell::run_shell_command` 共享核心供给，
  与桌面端 `shell` 工具零漂移。
- **失败语义**：in-process server 拉起失败 = 代码 bug（与环境无关）→ 硬错误退出，
  不像外部下游"失败即跳过"。调用期 JSON-RPC 错误（如空命令 invalid_params）不
  重连（没有连接可断），直接带给上游。
- **skill server 有意不嵌入**：`SkillServer` 的构造需要 core 的 `Skill` 快照
  （App 装配层从技能目录构建 `SkillInfo`），host 依赖 core 即破层（crate-layers.md
  规则5 白名单只豁免 mcp-builtin）。**未来 agent 不要"补全四件套"**——host 侧
  builtin 恒为 bash/fs/edit 三件。

## 7. CLI 二进制（src/bin/mcp-host.rs）

- 用法：`mcp-host --config ~/.openslate/mcp-host.toml`（`-c` 短参）。
- 默认清单：`dirs_or_env_home()`（L36-40）——`$HOME` 优先，否则
  `/data/data/com.termux/files/home`；即 `~/.openslate/mcp-host.toml`。Termux 下 $HOME
  是 Termux 私有目录，**App 不可读**——天然隔离。
- 启动序：token 处理 → `BuiltinHub::spawn(&cfg.builtin)`（builtin 群拉起，失败硬
  错误退出）→ `DownstreamHub::new` → `build_router(hub, builtin, token)`。
- 日志：tracing env filter，缺省 `mcp_host=info,axum=warn,tower=warn`。
- 优雅停机：Ctrl-C。启动日志含监听地址 / 下游数 / builtin 工具数 / 鉴权 on|OFF。

## 8. App 侧接法

`openslate.toml` 配 `[mcp.servers.openslate_host]` 之类 HTTP transport：

```toml
[mcp.servers.openslate_host]
url = "http://127.0.0.1:8765/mcp"
```

- **token 不落盘**：mobile 宿主用 `openslate_core::mcp::set_mcp_auth_override(Some(token))`
  （`crates/openslate-core/src/mcp.rs:76`）进程级注入
  （`OnceLock<RwLock<Option<String>>>`，L70）。**必须在任何 MCP 连接建立之前调**
  （覆盖发生在 transport 构建时，连接建立后再改不影响已建连接）；只作用于 HTTP 分支
  （stdio 子进程不受影响）；对所有 HTTP MCP server 统一生效。
- core 侧把每个工具包成 `McpTool`：`alias__tool` 暴露为
  `openslate_host_alias__tool`（单下划线前缀 + host 内部双下划线）；builtin 裸名
  （`bash` 等，无 `__`）暴露为 `openslate_host_bash` 形（core 侧 builtin 模板
  `{prefix}_{tool}`，Phase B 落地）。

### 8.1 ⚠ 下游 spawn 的 termux-exec 陷阱（真机实证 2026-10）

npm 安装的第三方 MCP server bin（如 `mcp-server-memory`）shebang 是
`#!/usr/bin/env node`，而 **Termux 没有 `/usr/bin/env`**。交互式 Termux 之所以能跑，
是靠 `LD_PRELOAD=libtermux-exec.so` 垫片在 execve 时改写 shebang；但 host 是经
RUN_COMMAND 非交互 shell 拉起的，**环境里没有这个 LD_PRELOAD** → host spawn 的下游
进程 ENOENT 秒死，host.log 表现为
`downstream connect failed: handshake: connection closed: initialize response`。

解法（已落地在 `McpHostManager.restartHost`）：启动 host 的命令行注入
`[ -f $LDEXEC ] && export LD_PRELOAD=$LDEXEC`（`$LDEXEC` =
`/data/data/com.termux/files/usr/lib/libtermux-exec.so`）。host 自身是静态 musl
二进制不受影响，LD_PRELOAD 仅随环境传递给子进程（npx/node 动态链接，垫片生效）。

诊断要点：交互式 Termux 能跑而 host 报握手失败 = 环境差异问题，优先查 shebang 与
LD_PRELOAD；`sh: 1: <bin>: not found`（npx 场景）与
`No such file or directory`（直接 exec 场景）都是此因。

## 9. 改动检查清单

- 新增上游能力（resources / prompts）→ 先扩 `AggregateHandler::get_info` 的 capabilities
  声明，再考虑 core 侧 `downgrade_content_blocks` 的占位符策略。
- 动保活策略 → v1 是「惰性重连」；换常驻守护要考虑不误杀慢启动 server。
- 动鉴权 → `Some("")` 关鉴权的调试语义别丢；旁车文件名 `<config>.toml.token` 与
  App 侧 RUN_COMMAND 读取约定配套。
- 动 builtin 范围 → 只许 bash/fs/edit 三件（skill 不嵌入的理由见 §6.5）；新 builtin
  server 必须来自 `openslate-mcp-builtin`（host 的唯一 workspace 依赖，规则5 白名单），
  且在 `[builtin]` 段配显式开关，绝不静默暴露。
- 改裸名错误文案 → 保持"unknown builtin tool"语义并同步集成测试断言。
