# 06-mobile · openslate-mobile（Rust 组合层）

> 面向接手本项目的 AI agent。本章覆盖 `crates/openslate-mobile`：Android 宿主与下层 crate 之间的**组合层**。姊妹文档：[mobile-ffi.md](mobile-ffi.md)（UniFFI 边界）、[android-app.md](android-app.md)（Kotlin 宿主工程）、[call-chains.md](call-chains.md)（端到端调用链）。

## 1. crate 定位与硬约束

`openslate-mobile` 是 FFI 边界的组合根：把 `openslate-session`（传输无关会话核心）与 `openslate-app`（装配链）拼成 FFI 友好的 `MobileRuntime`。**本 crate 无任何 Android 依赖**，可在 desktop 完整跑测试（见 §10）。

来自 `src/lib.rs` 模块头（设计约束，PLAN §4/§6）：

- **协议复用**：上行 `ClientMsg`、下行 `ServerMsg`（`openslate-protocol`），与 CLI/TUI/WS 同一套 JSON 协议；FFI **不发明**第二套 onToken/onTool 回调接口。
- **request/resolve**：host tool 经 `HostCallRouter` 发 `host_call_requested` 信封给宿主，宿主 `resolve_host_call(id, …)` 回填；Rust 侧 oneshot + timeout，永不让 FFI 阻塞。
- **secret 不落盘**：`set_api_key` 只做内存注入（`MobileSecrets`）；Android Keystore 持久化由 Kotlin 侧 `SecretStore` 负责（PLAN §18）。协议层的 `SetApiKey` 消息在本 crate 被机制化封禁（见 §3.2）。

依赖清单（`crates/openslate-mobile/Cargo.toml`）：openslate-core / app / protocol / session / store-sqlite + `toml_edit`（bootstrap 改写 `[database] path`）+ `tokio`（process feature，native shell）+ `zstd 0.13`（registry 压缩）+ `genai`（vendor 版，**仅为**注册 raw usage 诊断钩子）。

```text
Kotlin (UniFFI callback)◄──EventSink(单队列+泵线程)──┐
   │ ClientMsg JSON                                  │ ServerMsg JSON
   ▼                                                  ▼
MobileRuntime::send ──► session::dispatch ──► 引擎(RunManager) ──► LLM
```

## 2. 模块一览（8 个模块 + lib.rs）

| 模块 | 行数 | 关键导出 | 职责 |
|------|------|---------|------|
| `src/runtime.rs` | 1514 | `MobileRuntime`、`RuntimeOptions` | FFI 边界的组合根；create/send/shutdown/会话管理/registry 查询全部入口 |
| `src/bootstrap.rs` | 396 | `MobilePaths` | 宿主注入的 4 个路径；首启目录脚手架 + 默认配置；DB 路径归位 |
| `src/events.rs` | 111 | `EventSink`、`EventCallback`、`MOBILE_CONN_ID` | 事件出口：单条 std mpsc + 专用泵线程 |
| `src/hostcall.rs` | 310 | `HostCallRouter`、`HostTool`、`HOST_CALL_TIMEOUT` | host call 信封路由：request/resolve + oneshot 超时兜底 |
| `src/exec.rs` | 857 | `ExecSelection`、`ExecSelectionCell`、`NativeShellTool`、`register_bash_tools`、`mcp_owned_bash_names` | bash 工具后端多选（native / termux）+ 进程内 shell 工具 + MCP 接管判定（见 §7.1） |
| `src/provider.rs` | 88 | `mobile_provider_factory`、`MobileSecrets` | provider 工厂：内存 key 注入 + env 兜底 |
| `src/registry.rs` | 541 | `RegistryStore` | 模型元数据注册表本地存储（ZSTD 压缩 + 内存缓存 + 更新） |
| `src/alog.rs` | 92 | `alog!` 宏、`TAG`、`install_panic_hook` | logcat 桥（Android 直打 `__android_log_print`，desktop 空操作） |

`src/lib.rs`（45 行）re-export 关键类型，另有一个 crate 级函数：

```rust
// lib.rs:43-45
pub fn set_mcp_host_token(token: &str) {
    openslate_core::mcp::set_mcp_auth_override(Some(token.to_owned()));
}
```

进程级 HTTP `Authorization: Bearer` 覆盖。**必须在 `MobileRuntime::create*` 之前调用**——MCP 连接在装配期建立，之后注入不影响已建连接。

## 3. `runtime.rs` 详解

### 3.1 `RuntimeOptions`（runtime.rs:38-58）

| 字段 | 类型 | 默认 | 语义 |
|------|------|------|------|
| `provider_factory` | `Option<ProviderFactory>` | `None` | None = mobile secret factory（`mobile_provider_factory`）；测试注入 ScriptedProvider |
| `extra_tools` | `Vec<Box<dyn Tool>>` | 空 | 额外 host 工具；装配时总会追加内置 `mobile.ping` |
| `host_call_timeout` | `Duration` | `HOST_CALL_TIMEOUT`（120s） | host call 等宿主应答的上限 |
| `run_retention` | `i64` | **100** | 启动保留策略：装配期只保留最近 N 个 run，其余连同子表数据删除（防历史库无限膨胀；0 = 清空，生产不建议） |

### 3.2 `MobileRuntime` 结构与装配链（runtime.rs:61-174）

字段：`rt: Mutex<Option<tokio::Runtime>>`（shutdown_timeout 要消费所有权，单次取用）、`state: Arc<AppState>`、`sink: Arc<EventSink>`、`secrets: Arc<MobileSecrets>`、`host_router: Arc<HostCallRouter>`、`registry: Arc<ToolRegistry>`（bash 工具热换名）、`model_registry: Arc<RegistryStore>`、`workspace_dir: PathBuf`、`exec_selection: ExecSelectionCell`、`shutdown_flag: AtomicBool`。

`create_with()`（runtime.rs:90-174）装配顺序，**不要打乱**：

1. `alog::install_panic_hook()` + `install_raw_usage_logger()`（各自 `Once` 幂等）；
2. `MobilePaths::ensure_layout()` → 返回生效配置路径；
3. `MobilePaths::realign_database_path()`（best-effort，失败仅告警，见 §4.3）；
4. `EventSink::new(callback)`（泵线程已启动）；
5. `HostCallRouter::new(sink, timeout)`；
6. 组装工具集：`opts.extra_tools` + 追加 `HostTool("mobile.ping")`（schema 为空 object，`additionalProperties:false`）；**bash 工具不进 extra_tools**（命名随设置变化），装配完成后由 `register_bash_tools` 按当前多选注册；
7. `MobileSecrets` + `mobile_provider_factory`（或注入 factory）；
8. `RegistryStore::new(&paths.config_dir)`；
9. 构建 tokio runtime：**multi_thread、worker_threads=2、线程名 `openslate-mobile`**（runtime.rs:133-138）；
10. `rt.block_on(assemble_state(...))`（见 §3.6）；
11. 从 state 取出运行期 `ToolRegistry` → **重算 MCP bash 接管**（统一谓词，MCP 已接管的名字 HostTool/native 让位）→ `register_bash_tools(默认 native-only, mcp_owned)`；
12. `emit_snapshot()` —— **create 即握手**：进程内单客户端（`MOBILE_CONN_ID=1`），创建即推首份 snapshot。

### 3.3 各方法语义（含行号）

- **`send(msg_json)`**（runtime.rs:177-225）：
  - `shutdown_flag` 已置位 → `bail!("runtime 已关闭")`；
  - 解析失败返回 Err（不产生事件）；
  - **`SetApiKey` 机制化封禁**（runtime.rs:190-197）：协议层实现会把密钥明文写入 `config_dir/.env`（mobile 存储区不受 Keystore 保护），一律拒绝并广播 `ServerMsg::Error{code:"set_api_key_forbidden"}`，绝不进 dispatch。mobile 注 key 的正道是 FFI `set_api_key`；
  - `Hello`（runtime.rs:199-210）：`proto != PROTOCOL_VERSION` → 广播 `proto_mismatch` 错误；匹配则补发 snapshot；
  - 其余消息：从 `Mutex<Option<Runtime>>` 借 `handle()` clone 后 `spawn`（不移动 runtime 本体），`session_dispatch::dispatch(&state, MOBILE_CONN_ID, msg).await`。
- **`set_api_key(provider, value)`**（runtime.rs:228-230）：`MobileSecrets.set`（内存 map）。
- **`set_exec_backends(backends_csv)`**（runtime.rs）：`ExecSelection::parse_csv().normalized()` → 存 `ExecSelectionCell` → 重算 MCP bash 接管（统一谓词）→ `register_bash_tools` 热重注册（MCP 已接管的名字让位，见 §7.1）。
- **`list_tools_json()`**（runtime.rs:253-275）：全部已注册工具，按 name 排序的 JSON 数组；`input_schema` 原样透传（无 schema 恒为 `{}`，键恒存在）。
- **`get_agent_tool_visibility_json()`**（runtime.rs:280-287）：读 root agent frontmatter 的 `{"tools":[…],"hidden_tools":[…]}`（含 glob，保持落盘顺序）。
- **`set_agent_tool_visibility_json(tools, hidden)`**（runtime.rs:295-355）：①定位 root md（frontmatter `id:` 优先，`<id>.md` 文件名兜底，`find_agent_markdown` runtime.rs:942-970）→ ②外科手术式改写 frontmatter（`openslate_core::config::persist::set_agent_tool_lists`，其余字段与正文逐字节保留）→ ③重装 agent 树 + 热替换（manager 在家就换；回合进行中改 `inner.agent_tree`，引擎回收时自动对齐）。重装失败不回滚磁盘。下一回合即按新可见性过滤，无需重启。
- **`resolve_host_call(id, ok, payload) -> bool`**（runtime.rs:359-361）：转发 `HostCallRouter::resolve`；false = 无此在途调用。
- **`stats()`**（runtime.rs:364-366）：`SessionStatsDto` 快照。
- **registry 域**（runtime.rs:439-515）：
  - `registry_sources_json()`：数据源列表 + 本地状态（条目数/更新时间/体积）；
  - `registry_update_source_json(id)`：**阻塞至完成，最长 120s**（tokio timeout 包裹，超时返回 `{"ok":false,"error":"更新超时（120s）"}`）；FFI 注释要求宿主在 IO 线程调用；
  - `registry_schedule_auto_update()`：后台 spawn（不阻塞）；本地缺失或超过各源 `stale_after` 才拉取（models.dev 1 天、LiteLLM / CloudPrice 1 周）；
  - `registry_search_json(source_id, query, limit)`：纯本地（首读盘解压，同步返回）；`limit<=0` 按 50；未知 source_id → `{"total":0,"results":[]}`；
  - `lookup_model_meta_json(model_id)`：本地三源合并即时返回；全 miss 时在线单查 CloudPrice 兜底（**10s 超时**，仅数值字段采信，能力布尔不采信——策略见 `03-models` 章）。
- **`list_provider_models(provider)`**（runtime.rs:521-562）：GET /models（adapter 感知）；key 取「FFI 注入内存 key → env 兜底」（ollama 等本地端点允许无 key）；返回 `{"ok":true,"models":[…]}` 或 `{"ok":false,"error":"…"}`；含 10s 语义（宿主 IO 线程）。
- **会话域**（runtime.rs:566-752）：
  - `list_sessions_json(offset)`：每页 **50** 条；title 直接读 run 行 title 列（新 run 建立时已写 prompt 摘要，旧 run 由 open_session 治愈），无 N+1；
  - `delete_session(run_id) -> bool`：**当前活动会话拒绝删**（recorder 还在写，删了 FK 断裂）；成功后 best-effort `wal_checkpoint`；false 还涵盖 store 不可用 / run 不存在 / DB 错误；
  - `open_session(run_id)`：busy 守卫（回合进行中拒绝并 Notice Warn）→ 加载 messages → **存量标题治愈**（title 为 None/"mobile session" 时取首条 user 消息前 40 字符补写回库，runtime.rs:682-702）→ `RunRecorder::resume` → **切换前收束旧 run 为 "interrupted"**（同 run 重开不收束）→ 替换 inner 的 history/transcript/session_run/stats → `emit_snapshot()`；
  - `current_run_id()`：`Option<String>`（Kotlin transcript 本地持久化的键）。

### 3.4 `shutdown()`（runtime.rs:370-411）

幂等（`shutdown_flag.swap(true)`）。顺序：

1. `host_router.fail_all("runtime shutting down")` —— 在途 host call 全部立刻失败；
2. `state.approval.deny_all()` —— 审批全拒；
3. `sink.close_all()` —— 丢 sender，泵线程 drain 完余量后退出（已入队事件仍送达）；
4. **run 终态先于 runtime 收束落库**（runtime.rs:383-402）：`session_run.take()` 后 `handle.block_on(run.recorder.finish("interrupted", None, cost))`（先放 core 锁再 block_on——不能持锁等异步）；不做这步该 run 永远停留 running，只能等下次启动清扫；
5. `rt.take()` → `rt.shutdown_timeout(Duration::from_secs(5))` —— 有限等待 **~5s**，残余任务随 runtime drop 强制终止。

Kotlin 侧对应 `AgentService.onDestroy` 的 2.5s 上限（见 call-chains.md §4）。

### 3.5 `emit_snapshot()`（runtime.rs:413-424）

锁 `state.core` → `build_snapshot(&inner, &state.approval)` → `sink.send_to(MOBILE_CONN_ID, ServerMsg::Snapshot{session: Box::new(snap)})`。

### 3.6 `assemble_state()`（runtime.rs:758-937）

server `build_state` 的 mobile 变体（origin / 显式路径 / 注入 factory）：

- `build_app_context_with(config_flag, extra_tools)`（openslate-app wiring）解构出 config/agents/store/agent_tree/manager/skills/mcp_connections；
- `apply_approval(&mut manager, &config, true, false)` + 装上 `SessionApprovalBridge`（事件回推审批）；
- 初始会话模型：`[capabilities] main` 代号优先（不可解析时回退 root agent 别名）（runtime.rs:790-798）；session_label 默认 `"OpenSlate Mobile"`；
- **启动清扫**（runtime.rs:806-839，必须在恢复最近 run 之前）：
  - `mark_running_runs_interrupted(now_ms)`：上次进程被杀留下的 running run 统一标 interrupted（get_last_resumable_run 只排除 failed，interrupted 仍可续聊，先清扫不影响重启续聊）；
  - `prune_runs_keep_last(run_retention)`：只留最近 N 个 run；删了东西才 `wal_checkpoint`；
- **重启续聊**（runtime.rs:841-885）：`get_last_resumable_run()` → `RunRecorder::load_messages`（空消息/失败均退化为 fresh session）→ `RunRecorder::resume` → 恢复 history / transcript / cost；
- 组 `CoreInner{…, session_id: uuid simple}` → `AppState{core, sink, approval, auth_token: None, provider_factory, origin: "mobile", _mcp: mcp_connections}`。

辅助：`messages_to_transcript()`（runtime.rs:991-1062）把持久化消息投影回 transcript（User / Assistant / Reasoning（思维链随消息持久化，恢复为折叠 Reasoning 条目）/ ToolCall（args 截 40 字符、output 并入时截 2000 字符））。

## 4. `bootstrap.rs` 详解

### 4.1 `MobilePaths`（bootstrap.rs:15-25）

宿主（Kotlin）注入的 4 个真实路径，Rust 不做任何 Android 路径发现（PLAN §19）：

| 字段 | 内容 |
|------|------|
| `config_dir` | openslate.toml、agents/、prompts/、registry/ 居住地 |
| `workspace_dir` | bash 工具 cwd（App 私有可写目录） |
| `data_dir` | openslate.db |
| `cache_dir` | 可丢弃缓存 |

Kotlin 侧实际取值（RuntimeBridge.kt:252-255）：`filesDir/openslate`、`filesDir/workspace`、`filesDir/data`、`cacheDir/openslate`。

### 4.2 `ensure_layout()`（bootstrap.rs:81-121）

幂等：建 5 个目录（4 路径 + `config_dir/agents`）→ 无 `openslate.toml` 则写默认 → 无 `agents/root.md` 则写默认 root agent；**已有 root.md 且与 `LEGACY_ROOT_AGENT_MDS`（bootstrap.rs:44-68，两代历史命名 shell.run / termux.run）逐字节一致时自动迁移**到当前默认；用户定制过的文件不碰。

默认 root agent（bootstrap.rs:29-39）显式白名单：`mobile.ping` / `bash` / `termux_bash`（tools 为空 = 暴露全部注册工具，mobile 语义下不可接受）。

### 4.3 `realign_database_path()`（bootstrap.rs:137-213）

修「首启烙印」：`openslate.toml` 的 `[database] path` 是**绝对路径**，首启被烙上当时 data_dir；宿主之后迁移 data_dir 会造成新旧数据分叉。每次启动把 path 对齐到当前 `data_dir`：

- 已对齐（父目录 == data_dir）→ 不动（自定义文件名也尊重）；
- 目标位置已存在（新位置已产生数据）→ 放弃迁移（不覆盖新数据）；
- 旧库文件存在 → `rename`（失败回退 copy+delete，再失败 warn 放弃）；
- 最后 `toml_edit` 原地改写 path value（decor/注释/其余键零扰动）。

全程 best-effort：任何失败 warn 后 `Ok(())`；**唯一 Err 路径**是 toml 彻底无法解析（配置损坏，让上层暴露）。

### 4.4 默认 `openslate.toml`（bootstrap.rs:220-266）

与 CLI `openslate init` 同源，mobile 差异：`[builtin_tools] enabled = false`（桌面 fs/shell 工具在 Android 无意义且越权，PLAN §34）+ 数据库绝对路径。关键默认值：

| 配置 | 值 |
|------|-----|
| providers | `zhipu` → `https://open.bigmodel.cn/api/paas/v4`，`api_key_env = "ZHIPU_API_KEY"` |
| models | `main` / `fast` 均为 `glm-5.3-flash` |
| `limits.auto_compact` | `true` |
| `limits.max_context_bytes` | `60000` |
| `limits.max_context_messages` | `220` |
| `limits.max_turn_output_bytes` | `131072` |
| `limits.timeout_ms` | `9223372036854775807`（i64::MAX——无总时长预算；挂死防护 = 每请求 60s 无新数据即断） |

## 5. `events.rs` 详解（事件出口）

与 WS server 的 ConnectionHub 同语义不同拓扑：mobile 只有一个嵌入式客户端（宿主），所以**单条 std mpsc 队列 + 一个专用泵线程逐条回调，天然保序**（「同一连接内事件全序」），且 `broadcast` 永不阻塞引擎线程。

- `MOBILE_CONN_ID: u64 = 1`（events.rs:17）——固定连接 id；notice 定向用它。
- `EventCallback` trait（events.rs:21-23）：`fn on_event(&self, event_json: String)`；**回调运行在泵线程上**——宿主实现必须线程安全、尽快返回，重活转投宿主自己的队列。
- `EventSink::new`（events.rs:36-54）：起线程 `openslate-event-pump`（名字便于 logcat 排查），循环 `for event in rx { callback.on_event(event) }`，结束后 `alive=false`。
- `emit_raw(json)`：非 ServerMsg 的协议消息直接入队（**host call 信封走这里**）；`emit_msg` 把 `ServerMsg` 序列化，**编码失败兜底为协议 error 信封**（与 server 写任务同款语义）。
- `MsgSink` 实现（events.rs:77-111）：`send_to` 只认 `MOBILE_CONN_ID`（其余 conn_id 返回 false）；`count()` = alive；`close_all()` 丢 sender（优雅 drain 语义，与 WS hub 丢 sender 一致）。

事件流内容：`ServerMsg` 序列化 JSON + `host_call_requested` 信封，宿主按 `type` 字段分发（命名空间不重叠）。

## 6. `hostcall.rs` 详解（host tool 桥）

```text
Tool::execute ─► HostCallRouter::call(tool, args)
                    │ 注册 oneshot + 编号 id（AtomicU64 从 1 起）
                    │ emit {"type":"host_call_requested","id":42,"tool":"…","args":{…}}
                    ▼
               Kotlin 异步执行 Android API
                    ▼
               resolve_host_call(id, ok, payload)
                    │ oneshot 唤醒 / 超时兜底
                    ▼
               Tool::execute 返回，Agent 继续
```

- `HOST_CALL_TIMEOUT = Duration::from_secs(120)`（hostcall.rs:48）——宿主 UI 自动化/系统调用可能较慢，给足余量；**超时视为工具失败（回合可继续），不是 runtime 失败**。
- `call()`（hostcall.rs:71-100）：oneshot 注册 → 信封经 `sink.emit_raw`（与 ServerMsg 同一条事件流，全局保序）→ `tokio::time::timeout` 等应答；三种结局：应答 / 发送端 drop（fail_all 或关停 → `"host call aborted (runtime shutting down)"`）/ 超时（移除 pending，报 `"host call timeout after {}ms (host did not resolve)"`）。
- `resolve(id, ok, payload)`（hostcall.rs:104-123）：`ok=true` 时 payload 必须是合法 JSON 文本（解析失败转为 Err）；`ok=false` 时 payload 是错误说明；false = 无此在途调用（超时/已答/关停）。
- `fail_all(reason)`（hostcall.rs:126-134）：关停兜底，drain 全部 pending 立刻失败（引擎解栈）。
- `HostTool`（hostcall.rs:139-196）：由宿主执行的 `Tool` 壳——name/description/schema 进注册表，execute 转发 router；Err 也返回 `Ok(ToolOutput{status: Error})`（工具失败，不炸回合）。

## 7. `exec.rs` 详解（bash 后端多选 × MCP 接管）

**host 侧命名矩阵**（固定、变化的是注册与否；未选中的名字直接从注册表注销，模型不可见；运行中途切换时旧名字调用得到 NotFound，模型可自纠）：

| 多选 | `bash` | `termux_bash` |
|------|--------|---------------|
| 仅 native | native shell | 不注册 |
| 仅 termux | termux（HostTool 壳顶替） | 不注册 |
| 双选 | native | termux（HostTool 壳） |
| 全不选 | 不注册 | 不注册（bash 能力整体下线，`normalized()` 恒等、**不自动回退**） |

- 常量（exec.rs）：`DEFAULT_TIMEOUT_MS = 30_000`、`MAX_TIMEOUT_MS = 120_000`（调用方可指定，超出 clamp）、`MAX_OUTPUT_BYTES = 64KiB`（超限截断保头尾）、`KEEP_BYTES = 24KiB`；`BASH_TOOL="bash"`、`TERMUX_BASH_TOOL="termux_bash"`；`ALL_BASH_NAMES`（pub）还含历史名 `"shell.run"` / `"termux.run"`（切换时统一清理；reload 的让位判定按它界定「bash 工具域」）。
- `ExecSelection`（位标志 struct）+ `parse_csv`（未知值忽略，全未知 = 空）+ `ExecSelectionCell`（`Arc<AtomicU8>` 存 bits，FFI `set_exec_backends` 热更新）。
- `register_bash_tools(registry, sel, workspace, router, mcp_owned)`：先清理全部旧名字（**MCP 已接管的名字除外**），再按需注册——native 注册 `NativeShellTool`；termux 注册 `HostTool`（描述里告知模型 Termux 有完整 coreutils/apt/python，输出超 ~8KB 截断）。`mcp_owned` 见下。
- `NativeShellTool`（exec.rs）：进程内 shell，cwd 固定 workspace_dir；**Android 上必须用绝对路径 `/system/bin/sh`**（App 进程 PATH 不含它；desktop 测试回退 `/bin/sh`）；`kill_on_drop(true)`；stdout/stderr **并发读管道 + wait**（子进程输出超管道缓冲不并发即死锁）；超时杀进程后还有 **200ms 宽限**收敛已缓冲输出；环境变量**脱敏继承**——`SENSITIVE_ENV_MARKERS`（API_KEY/TOKEN/SECRET/PASSWORD/PASSWD/CREDENTIAL/PRIVATE_KEY，子串匹配大写）命中的变量 `env_remove`（模型可经 `env` 读到继承变量，绝不下发敏感命名）；输出格式 `exit_code: N\n\n--- stdout ---\n…\n--- stderr ---\n…`（与桌面 builtin shell 一致），截断按 UTF-8 字符边界切。

### 7.1 MCP 接管（termux bash 降级阶梯的 host 侧半边）

MCP 聚合 host 在线时，其内置 BashServer 以裸名 `bash` 列工具，经
`[mcp].builtin_tool_name_template`（默认 `{prefix}_{tool}`，core
`McpConfig::builtin_template_or_default()` 接缝）展开注册（默认 →
`termux_bash`）。统一判定谓词（exec.rs，pub）：

- `mcp_bash_expanded_names(&McpConfig)`：builtin 模板按各 server 键展开裸名 `bash` 的候选名（**不硬编码 `termux_bash` 字面**——用户改小模板后展开名随之变化）。
- `mcp_owned_bash_names(roster, &McpConfig)`：MCP 名册 ∩ 展开名，**并上**名册 ∩ `ALL_BASH_NAMES`（防「模板已改、名册还是旧模板注册的」不一致窗口误杀）。非空 = MCP 接管：HostTool/native 对这些名字让位（`register_bash_tools` 不清理、不注册）；为空 = HostTool/native 兜底。

三个选择点（runtime.rs）都先经谓词重算：

| 选择点 | 行为 |
|--------|------|
| 装配期（`create_with`） | assemble_state 注册外部 MCP 后重算 → `register_bash_tools(…, mcp_owned)` 让位 |
| `reload_mcp` | **先让位再 swap**：新名册含 bash 域名且注册表正被 host bash 工具占着 → 备份 Arc 后先卸载，再 try_register（修掉撞名整体回滚死锁——否则 MCP 永远接管不了）；swap 成功后重算（接管保持 / MCP 缺席 HostTool 回装兜底）；撞真冲突（bash 域外，如 `mobile.ping`）回滚时让位的工具原样装回；全败保旧路径注册表不动 → 兜底天然在场 |
| `set_exec_backends` | 切后端前重算（名册 = `mcp_tool_names` 快照，模板读最新落盘）——修掉旧实现「切后端 = 无条件 unregister 全部 bash 名」对 MCP bash 工具的误杀 |

运行期 host 死亡**不做工具热切换**（模型工具列表已进上下文）：`McpTool::execute` 失败语义 = `ExecutionError` 带工具名（回合可继续），修复入口是显式 `reload_mcp`。接管/兜底的交接只在上述三个选择点发生。

termux-only 命名矩阵（MCP 提供裸 `bash` 时）：HostTool 顶替 `bash`（RUN_COMMAND 兜底），MCP 注册名 = builtin 模板展开（默认 `termux_bash`、自定义如 `{prefix}.{tool}` → `termux.bash`）——白名单 `bash` / `termux_bash` 两者在默认模板下都有实体，历史配置零变动。

## 8. `provider.rs` 详解（secret 注入）

- `MobileSecrets`：`RwLock<HashMap<provider名, key>>`，进程重启由宿主重新注入。
- `mobile_provider_factory`（provider.rs:44-75）：解析别名 → provider 名（**解析失败也要尽量报出 provider 名**——常见场景：宿主先 set_api_key 再配置模型）→ key 查找顺序：**内存注入 → 该 provider 的 `api_key_env` 环境变量 → 报错**「provider '…' 的 API key 未配置（宿主请先 set_api_key）」→ `build_genai_provider(config, alias, Some(key))`。

## 9. `registry.rs` 与 `alog.rs` 概要

- `RegistryStore`（registry.rs:46-56）：`{config_dir}/registry/` 下每源一个 `<id>.json.zst`（**zstd level 3**）+ `meta.json`（`v` 格式版本 + 各源 `SourceStatus{fetched_at_ms, entries, raw_bytes, zst_bytes}`）；meta 写入走 tmp+rename。两级内存缓存：`parsed`（归一化索引，first-wins 折叠）与 `raw`（**原始 JSON 树**，搜索/浏览要遍历原始键——保留 provider 前缀，首次搜索才加载，Arc 共享零整树克隆）；`update_source` 成功后一并失效重建。解析 / 合并 / 匹配逻辑在 `openslate-core/src/model_registry.rs`（见 `03-models` 章）。更新网络失败/损坏按空处理并 alog 告警。
- `alog.rs`：`TAG = "OpenSlateRust"`；Android 上 `extern "C" __android_log_print` 直打（Verbose..Error → 2..6），desktop `log()` 为空操作；`alog!` 宏 = Info 级阶段打点；`install_panic_hook`（Once 幂等）：panic → logcat `PANIC: {msg} at {file}:{line}:{col}` 再走默认 hook（tokio spawn 会吞 panic，必须经 hook 外显，否则表现为无声的「空回合」）；`install_raw_usage_logger`：把 vendor/genai 的原始 usage 诊断流接到 logcat（网关 usage 字段变化看 logcat 即可，免抓包）。

## 10. 测试

- 集成测试 `crates/openslate-mobile/tests/runtime_flow.rs`（desktop 可跑，无 Android 依赖）：`mobile_runtime_closed_loop_streaming`（scripted provider 流式回包闭环）、`mobile_runtime_host_call_roundtrip`（信封往返 + resolve）、`mobile_runtime_hello_version_check`（版本不匹配报错）、`mobile_runtime_shutdown_rejects_send`、`mobile_runtime_tool_visibility_roundtrip`。harness：`ScriptedProvider`（脚本化 `ModelStreamEvent` 流）+ `Collector`（事件采集 + `wait_for_count` 按类型计数等待）。
- 单元测试在各模块 `#[cfg(test)] mod tests`：runtime 生命周期 / 安全守卫（触达私有状态；DB 校验经独立 scratch 连接）、hostcall 四条路径（roundtrip/error/timeout/fail_all）、exec（注册矩阵 / 热切换 / native 执行 / 截断 / 脱敏 / MCP 接管判定与让位）、bootstrap（幂等 / realign 四场景）、provider（set/get 往返）。
- 降级阶梯（runtime tests，手写 std::net MCP HTTP 挡板同 app wiring 集成测试模式）：`set_exec_backends` 热切换不误杀 MCP bash 工具、HostTool 在场 reload 升级接管（不撞名回滚）、reload 全败兜底在场、reload 真冲突回滚装回让位工具、termux-only 命名矩阵（默认 / 自定义 builtin 模板，证明不硬编码）、builtin 模板 setter（校验 + 落盘 + 空串删键）。
- 跑法：`cargo test -p openslate-mobile`。

## 11. 接手检查清单

1. 改 `RuntimeOptions` 默认值 / 装配顺序 → 先看 `create_with` 注释与 §3.2 的顺序约束（清扫必须在恢复之前）。
2. 新增 host tool → Rust 侧注册 `HostTool`（或进 extra_tools），Kotlin 侧 `RuntimeBridge.handleHostCall` 补分发。
3. 改事件协议（新 ServerMsg 变体）→ `events.rs` 无需动（透传 JSON），但 Kotlin `handleEvent` 要补分支。
4. 改 bash 后端命名 → 同步 `DEFAULT_ROOT_AGENT_MD` 白名单与 `ALL_BASH_NAMES` 清理表 + Kotlin `handleHostCall` 的工具名匹配。
