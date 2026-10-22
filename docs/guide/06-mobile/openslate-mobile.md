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

## 2. 模块一览（17 个模块 + lib.rs）

| 模块 | 行数 | 关键导出 | 职责 |
|------|------|---------|------|
| `src/runtime.rs` | 3371 | `MobileRuntime`、`RuntimeOptions` | FFI 边界的组合根；create/send/shutdown/会话管理/registry 查询全部入口 |
| `src/bootstrap.rs` | 396 | `MobilePaths` | 宿主注入的 4 个路径；首启目录脚手架 + 默认配置；DB 路径归位 |
| `src/events.rs` | 111 | `EventSink`、`EventCallback`、`MOBILE_CONN_ID` | 事件出口：单条 std mpsc + 专用泵线程 |
| `src/hostcall.rs` | 447 | `HostCallRouter`、`HostTool`、`HOST_CALL_TIMEOUT` | host call 信封路由：request/resolve + oneshot 超时兜底；`call_with_id`（自定义超时，ask_user 先例）；`HostTool::with_timeout`（超时覆盖 builder——termux 壳用，见 exec.rs）；`fail_all` 对 ask_user 豁免（持久化恢复） |
| `src/mcp_recovery.rs` | — | `RecoveringMcpTool`、`RecoveryCtx`、`MCP_RECOVER_HOST_TOOL`、`is_mcp_connection_failure` | MCP host 死亡自愈：外部 MCP 工具的恢复装饰器（连接失败签名 → 单飞恢复 host call `mcp.recover_host` → 注册表同名重发一次）；`RecoveryCtx` = 单飞闩 + 完成代数 + 冷却窗口（10s）+ 注册表弱引用；重试预算 = 1（task-local 重入标记）；包装点两处（create_with 装配期 `wrap_mcp_tools` / reload_mcp scratch 迁移期），见 call-chains.md §3.1.2 |
| `src/askuser.rs` | — | `AskUserTool`、`AskUserContext`、`MobileQuestionHandler`、`SideEffectFreeApproval`、`AskUserSecretVault` | ask_user 提问工具全链路：参数归一化/下发契约、问卷持久化、迟到答案续跑、secret 凭据 trait 接缝、无副作用工具免审装饰器（原 `AskUserExemptApproval`，C4 起覆盖 ask_user + output_query，见 [04-tools/ask-user.md](../04-tools/ask-user.md)） |
| `src/approval_review.rs` | 502 | `AiReviewApproval`、`DEFAULT_REVIEW_TIMEOUT` | 审批 AI 审查装饰器（mobile-only，tool-approval）：AutoRisk 请求先付一次小额 LLM 审查——safe 静默放行 / risky 弹人（note 带审查理由）/ 失败超时 fail-safe 弹人；verdict 缓存（(tool, args hash)→结论，容量 128）；审查模型 = `[capabilities].review` 未绑定继承会话模型；P0 红线全落实（见 [call-chains.md](call-chains.md) §3.3 与 [../01-architecture/dataflow.md](../01-architecture/dataflow.md) §4.1） |
| `src/secrets.rs` | — | `PersistentSecretVault`、`env_source`、`mask_with_known_values` | ask_user secret 子系统：会话内存 + store v4 `credentials` 持久镜像两级凭据库、env 注入源、file 写入（0600）、脱敏值源（见 [04-tools/ask-user.md §8](../04-tools/ask-user.md)） |
| `src/exec.rs` | 940 | `ExecSelection`、`ExecSelectionCell`、`NativeShellTool`、`EnvSource`、`register_bash_tools`、`mcp_owned_bash_names` | bash 工具后端多选（native / termux）+ 进程内 shell 工具（env 注入：脱敏后应用 vault env 形态凭据）+ MCP 接管判定（见 §7.1）；termux HostTool 壳专用超时 `TERMUX_HOST_CALL_TIMEOUT` = **900s**（停滞期间 Kotlin 侧等待连接 + 重发续跑，见 android-app.md / call-chains.md §3.1.1；配套 Kotlin `HARD_WAIT_TIMEOUT_MS` = 880s） |
| `src/provider.rs` | 88 | `mobile_provider_factory`、`MobileSecrets` | provider 工厂：内存 key 注入 + env 兜底 |
| `src/attachments.rs` | ~1400 | `import_attachment`、`sniff_mime`、`normalize_image`（header 快速通道 + 迭代压缩）、`gate_submit_attachments`、`AttachmentDataProxy`、`refill_message_attachment_data` | 附件 ingest（多模态输入，存储 v5 §3）：magic-byte 嗅探 → sha256 content-addressing → 图像归一化（小图 JPEG/PNG header-only 零转码 passthrough；其余按用户双旋钮**迭代压缩达标** q85→70→55 → ×0.85 缩边，地板 1024）→ 视频 passthrough → 落盘；Submit 门控矩阵；请求路径字节装载代理（见 §12）。**导入纯本地零网络**（token 数由响应侧回填，见 §12.5）；全程分段耗时日志 |
| `src/transcripts.rs` | ~640 | `SyncState`（`persist_range`/`flush_now`/`invalidate`）、`sync_loop`、`rebuild_transcript`、`heal_truncated_tail`、`DANGLING_TAIL_META`/`INCOMPLETE_TURN_META`/`HEAL_META` | transcript 持久化（存储 v5 §4 + 历史转写丢失修复 C1/C3）：SyncState 收口的增量同步 + 三个高危时刻的强制冲刷 + 两级重建（entries 优先 / 消息投影回退 / 截断自愈）+ 存储健康横幅（见 §13）；sync_loop 兼附件 tokens 回填消费点（见 §12.5） |
| `src/attachment_tokens.rs` | ~330 | `AttachmentTokenBackfill`、`overlay_tokens_from_messages` | 附件 image tokens **响应侧回填**（协议 v5）：EventSink tap 闩首次调用值 → sync_loop 落 transcript → 重开经 messages.meta_json 重放（见 §12.5） |
| `src/output_storer.rs` | ~200 | `MobileOutputStorer` | 长输出落盘 storer（长输出落盘 + output_query C3）：实现 core `ToolOutputStorer` 接缝——超限工具输出 sha256 寻址 → zstd-3 压缩 → store `tool_outputs` 行 + `files/outputs/{sha}.zst`；**ref = 会话内自增短 id `out-N`**（同 run 同 sha 幂等复用既有 ref，分配在 storer 串行路径、唯一索引兜底）；任何失败返回 `None`（core 回落旧截断路径，绝不失败回合）。store 经 `SharedStore`（OnceLock 后注，AttachmentDataProxy 同款接缝）在 assemble_state 后由 create_with 注入；进程级单槽 `set_tool_output_storer` 随进程生灭（shutdown 无需清理）；装配启动序列挂 `gc_orphan_output_files`（失败仅告警） |
| `src/output_query.rs` | ~1200 | `OutputQueryTool`、`OUTPUT_QUERY_TOOL` | output_query 内置工具（长输出落盘 + output_query C4）：模型按 `[STORED OUTPUT ref=…]` 标记取回落盘全文的 head/tail/lines/grep/wc 五算子查询；**ref 会话内唯一短 id，`(run_id, ref)` 精确匹配严格会话隔离**（跨会话 ref 不可解析）；64KB 块式行迭代核心（改造自 uutils/coreutils head，MIT 署名）一份共用；累计封顶 = `max_output_bytes` 生效值；免审批（见 §15） |
| `src/registry.rs` | 541+ | `RegistryStore` | 模型元数据注册表本地存储（ZSTD 压缩 + 内存缓存 + 更新）；`supports_video`（models.dev modalities 直读——Submit 门控的 video 维） |
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
| `approval_review_timeout` | `Duration` | `DEFAULT_REVIEW_TIMEOUT`（20s） | 审批 AI 审查等待上限（tool-approval；测试接缝——超时路径单测注 200ms 级别） |
| `run_retention` | `i64` | **100** | 启动保留策略：装配期只保留最近 N 个 run，其余连同子表数据删除（防历史库无限膨胀；0 = 清空，生产不建议） |

### 3.2 `MobileRuntime` 结构与装配链（runtime.rs:61-174）

字段：`rt: Mutex<Option<tokio::Runtime>>`（shutdown_timeout 要消费所有权，单次取用）、`state: Arc<AppState>`、`sink: Arc<EventSink>`、`secrets: Arc<MobileSecrets>`、`host_router: Arc<HostCallRouter>`、`registry: Arc<ToolRegistry>`（bash 工具热换名）、`model_registry: Arc<RegistryStore>`、`workspace_dir: PathBuf`、`exec_selection: ExecSelectionCell`、`bash_env: EnvSource`（ask_user secret env 注入源，热切换/重载随工具传递）、`ask: Arc<AskUserContext>`、`mcp_recovery: Arc<mcp_recovery::RecoveryCtx>`（host 死亡自愈共享上下文：单飞闩 + 冷却 + 注册表弱引用，reload_mcp 迁移新工具时复用）、`shutdown_flag: AtomicBool`。

`create_with()`（runtime.rs:90-174）装配顺序，**不要打乱**：

1. `alog::install_panic_hook()` + `install_raw_usage_logger()`（各自 `Once` 幂等）；
2. `MobilePaths::ensure_layout()` → 返回生效配置路径；
3. `MobilePaths::realign_database_path()`（best-effort，失败仅告警，见 §4.3）；
4. `EventSink::new(callback)`（泵线程已启动）；
5. `HostCallRouter::new(sink, timeout)`；**`PersistentSecretVault` + `AskUserContext::new(router, vault)`**（工具先于 AppState 构造）+ **env 注入源注册**（`env_source(vault)` → mcp-builtin `set_shell_env_source` 进程级注册 + native bash 经 `register_bash_tools` 传入）；
6. 组装工具集：`opts.extra_tools` + 追加 `HostTool("mobile.ping")`（schema 为空 object，`additionalProperties:false`）+ **`OutputQueryTool`**（长输出落盘 C4；复用下述 `output_store_handle` 的 `SharedStore` 接缝，AppState 弱引用经共享 OnceLock 在第 10 步后注）；**bash 工具不进 extra_tools**（命名随设置变化），装配完成后由 `register_bash_tools` 按当前多选注册；
7. `MobileSecrets` + `mobile_provider_factory`（或注入 factory）；
8. `RegistryStore::new(&paths.config_dir)`；
9. 构建 tokio runtime：**multi_thread、worker_threads=2、线程名 `openslate-mobile`**（runtime.rs:133-138）；
10. `rt.block_on(assemble_state(...))`（见 §3.6）；随后 `ask_ctx.wire(&state)` + QuestionHandler 装配 + **`vault.attach_store(store)`**（加载持久凭据镜像）+ **`push_secret_masker`**（输出脱敏闭包：`[ask_user] mask_secrets` 短路 + vault 已知值打码，见 ask-user.md §8.4）；
11. 从 state 取出运行期 `ToolRegistry` → **重算 MCP bash 接管**（统一谓词，MCP 已接管的名字 HostTool/native 让位）→ `register_bash_tools(默认 native-only, env, mcp_owned)` → **MCP 名册包恢复装饰器**（`RecoveryCtx::new(router, registry)` + `wrap_mcp_tools`：host 死亡自愈，见 mcp_recovery.rs / call-chains.md §3.1.2）；
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
- **`list_tools_json()`**（runtime.rs:792-820）：全部已注册工具，按 name 排序的 JSON 数组；`input_schema` 原样透传（无 schema 恒为 `{}`，键恒存在）；每项含 `server` = 审批作用域标识（tool-approval：聚合形态 = 下游 slug 如 `"github"`，host 内置裸名 = host 前缀，非 MCP 为 JSON null——反解走 core 权威实现 `mcp::server_slug_of`，namespace 取 registry `Tool::namespace()`）。Android 设置页「工具权限」tab 按它分组。
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
  - `open_session(run_id)`：busy 守卫（回合进行中拒绝并 Notice Warn）→ 【C1】**切换前强制冲刷**当前 run 转写尾部（`SyncState::flush_now(force=true)`，含回合中推迟的 ToolCall 中断快照 + `INCOMPLETE_TURN_META` 收尾行；busy 守卫已保证无并发 submit）→ 加载 messages → 尾部悬挂 assistant（带未答 tool_calls）pop 时【C2】**回写 DB**：`delete_messages_from_seq(run, max_seq)` 删尾行 + `delete_transcript_entries_for_run` 全删条目 + `SyncState::invalidate(run)`（游标自举 0 → sync_loop 全量重写重建转写；删干净后第二次 open 不再命中 pop）→ **存量标题治愈**（title 为 None/"mobile session" 时取首条 user 消息前 40 字符补写回库）→ `RunRecorder::resume` → **切换前收束旧 run 为 "interrupted"**（同 run 重开不收束）→ 【C2】**会话身份轮换**（!same_run：`clear_session_scopes()`（core 锁外，锁序先 bridge 后 core）+ swap 块内 `session_id = new_session_id()`——session_id 变化驱动 ChatScreen 会话揭示、A 会话 approve_all 不泄漏到 B；同 run 重开不轮换）→ 替换 inner 的 history/transcript/session_run/stats → `emit_snapshot()`；
  - `current_run_id()`：`Option<String>`（Kotlin transcript 本地持久化的键）。

### 3.4 `shutdown()`（runtime.rs:370-411）

幂等（`shutdown_flag.swap(true)`）。顺序：

1. `host_router.fail_all("runtime shutting down")` —— 在途 host call 全部立刻失败；
2. `state.approval.deny_all()` —— 审批全拒；
3. `sink.close_all()` —— 丢 sender，泵线程 drain 完余量后退出（已入队事件仍送达）；
3.5 【C1】**关停冲刷**：`handle.block_on(sync.flush_now(&state, force=true))`——把当前 run 转写尾部（含回合中推迟的 ToolCall 中断快照 + `INCOMPLETE_TURN_META`）强制落库；必须在 `rt.take()` 之前同步完成（此后无 runtime 可执行异步）；
4. **run 终态先于 runtime 收束落库**（runtime.rs:383-402）：`session_run.take()` 后 `handle.block_on(run.recorder.finish("interrupted", None, cost))`（先放 core 锁再 block_on——不能持锁等异步）；不做这步该 run 永远停留 running，只能等下次启动清扫；
5. `rt.take()` → `rt.shutdown_timeout(Duration::from_secs(5))` —— 有限等待 **~5s**，残余任务随 runtime drop 强制终止。

Kotlin 侧对应 `AgentService.onDestroy` 的 2.5s 上限（见 call-chains.md §4）。

### 3.5 `emit_snapshot()`（runtime.rs:413-424）

锁 `state.core` → `build_snapshot(&inner, &state.approval)` → `sink.send_to(MOBILE_CONN_ID, ServerMsg::Snapshot{session: Box::new(snap)})`。

### 3.6 `assemble_state()`（runtime.rs:758-937）

server `build_state` 的 mobile 变体（origin / 显式路径 / 注入 factory）：

- `build_app_context_with(config_flag, extra_tools)`（openslate-app wiring）解构出 config/agents/store/agent_tree/manager/skills/mcp_connections；
- **审批装配（tool-approval，runtime.rs:1398-1429）**：
  1. `apply_approval(&mut manager, &config, true, false)` 装上 legacy 语义基线（wiring 通用链）；
  2. 建桥 `SessionApprovalBridge::new(sink)` → **初始规则 = `rules_from_config(&config)` + mobile 专属 opt-in**：`config.approval.is_none()`（`[approval]` 段缺省）时 `default_level = Auto`（设计 v3 用户拍板：自动判断 = 启发式预过滤 + AI 审查；显式配置（legacy 或新键）按映射保持原行为）→ `bridge.set_rules(initial_rules)`；
  3. `ApprovalManager::with_rules(approval_bridge.rules_handle())` **挂桥的共享 cell**（P0-5：`apply_approval` 的重建形状必须被此步覆盖——游离副本会让 always_*/set_approval_config 热更永远到不了引擎且无编译错误；防回归单测 `approval_rules_cell_shared_between_bridge_and_engine` 钉死桥/引擎/manager 三方同一 cell）；
  4. callback 链 = `SideEffectFreeApproval(AiReviewApproval(bridge))`——无副作用工具（ask_user / output_query）豁免最外层；`AiReviewApproval::new(bridge, sink, provider_factory, registry, Handle::current()).with_timeout(review_timeout)`，SessionCore 构造完后 `review_approval.wire(&core)` 后注（core 字段 `Mutex<Option<Arc<SessionCore>>>` 防循环依赖，AskUserContext.wire 同款先例）；
- 初始会话模型：`[capabilities] main` 代号优先（不可解析时回退 root agent 别名）（runtime.rs:790-798）；session_label 默认 `"OpenSlate Mobile"`；
- **启动清扫**（runtime.rs:806-839，必须在恢复最近 run 之前）：
  - `mark_running_runs_interrupted(now_ms)`：上次进程被杀留下的 running run 统一标 interrupted（get_last_resumable_run 只排除 failed，interrupted 仍可续聊，先清扫不影响重启续聊）；
  - `prune_runs_keep_last(run_retention)`：只留最近 N 个 run；删了东西才 `wal_checkpoint`；
- **重启续聊**（runtime.rs:841-885）：`get_last_resumable_run()` → `RunRecorder::load_messages`（空消息/失败均退化为 fresh session）→ `RunRecorder::resume` → 恢复 history / transcript / cost。重启恢复的悬挂尾巴 pop（may_pop_dangling_tail 守卫通过后）同样走 C2 回写（删尾消息行 + 全删 entries 行；此刻 SyncState 尚未构造、游标表为空，无需失效——sync_loop 首周期自会从 DB 行数自举）；
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
| `limits.output_store_threshold` | `16384`（C2 长输出落盘阈值：工具输出超阈值全文（脱敏后）落 SQLite、对话内换头尾预览，模型经 `output_query` 取回；`0` = 关闭） |
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
- `call()`：oneshot 注册 → 信封经 `sink.emit_raw`（与 ServerMsg 同一条事件流，全局保序）→ `tokio::time::timeout` 等应答；三种结局：应答 / 发送端 drop（fail_all 或关停 → `"host call aborted (runtime shutting down)"`）/ 超时（移除 pending，报 `"host call timeout after {}ms (host did not resolve)"`）。
- `allocate_id()` + `call_with_id(id, tool, args, timeout)`：ask_user 用——id 预取（问卷行要在发信封前落库，request_id = id）+ 每调用自定义超时（永不超时配置取近似无限的极长时长）。
- `resolve(id, ok, payload)`：`ok=true` 时 payload 必须是合法 JSON 文本（解析失败转为 Err）；`ok=false` 时 payload 是错误说明；false = 无此在途调用（超时/已答/关停）——**对失效 id 无害 no-op**。
- `fail_all(reason)`：关停兜底，drain 全部 pending 立刻失败（引擎解栈）；**ask_user 豁免**（放回待答，问卷留给持久化恢复——若 fail 会把错误结果并入历史、行被迫关卷，迟到答案无处补全）。
- `HostTool`（hostcall.rs:139-196）：由宿主执行的 `Tool` 壳——name/description/schema 进注册表，execute 转发 router；Err 也返回 `Ok(ToolOutput{status: Error})`（工具失败，不炸回合）。

## 7. `exec.rs` 详解（bash 后端注册 × MCP 接管）

**命名矩阵**（2026-10 语义改版：名字与后端**一一绑定**——native ⇒ `bash`、termux ⇒ `termux_bash`，不存在「termux 顶替 bash」形态；固定的是名字，变化的是注册与否；未选中的名字直接从注册表注销，模型不可见；运行中途切换时旧名字调用得到 NotFound，模型可自纠。UI 侧（设置页单开关）恒写含 termux 形态，{native}/{}/全不选只可能来自旧存量——Kotlin 桥层 normalizeExecBackends 归一化后不再出现）：

| 后端集合 | `bash` | `termux_bash` |
|------|--------|---------------|
| {native,termux}（默认/UI ON） | native shell | termux（HostTool 壳兜底） |
| {termux}（UI OFF） | 不注册 | termux（HostTool 壳兜底） |
| {native} / {}（旧存量/异常） | native shell / 不注册 | 不注册（`normalized()` 恒等、**不自动回退**） |

- 常量（exec.rs）：`DEFAULT_TIMEOUT_MS = 30_000`、`MAX_TIMEOUT_MS = 120_000`（调用方可指定，超出 clamp）、`MAX_OUTPUT_BYTES = 1MiB`（C5 抬升，原 64KiB；进程内执行无传输约束，与 mcp-builtin shell 统一心智，保留上限防 OOM）、`KEEP_BYTES = 256KiB`；`BASH_TOOL="bash"`、`TERMUX_BASH_TOOL="termux_bash"`；`ALL_BASH_NAMES`（pub）还含历史名 `"shell.run"` / `"termux.run"`（切换时统一清理；reload 的让位判定按它界定「bash 工具域」）。
- `ExecSelection`（位标志 struct）+ `parse_csv`（未知值忽略，全未知 = 空）+ `ExecSelectionCell`（`Arc<AtomicU8>` 存 bits，FFI `set_exec_backends` 热更新）。
- `register_bash_tools(registry, sel, workspace, router, mcp_owned)`：先清理全部旧名字（**MCP 已接管的名字除外**），再按需注册——native 注册 `NativeShellTool`；termux 注册 `HostTool`（描述里告知模型 Termux 有完整 coreutils/apt/python，输出超 ~256KB 截断——上限来自 Kotlin `TermuxExec.combined()` 的 Binder 预算，见 android-app.md）。`mcp_owned` 见下。
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
| `reload_mcp` | **先让位再 swap**：新名册含 bash 域名且注册表正被 host bash 工具占着 → 备份 Arc 后先卸载，再 try_register（修掉撞名整体回滚死锁——否则 MCP 永远接管不了）；swap 成功后重算（接管保持 / MCP 缺席 HostTool 回装兜底）；撞真冲突（bash 域外，如 `mobile.ping`）回滚时让位的工具原样装回；全败保旧路径注册表不动 → 兜底天然在场；scratch 迁移的新工具逐个**包恢复装饰器**（`SharedTool(Arc<RecoveringMcpTool>)`，host 死亡自愈的 reload 期包装点） |
| `set_exec_backends` | 切后端前重算（名册 = `mcp_tool_names` 快照，模板读最新落盘）——修掉旧实现「切后端 = 无条件 unregister 全部 bash 名」对 MCP bash 工具的误杀 |

运行期 host 死亡**不做工具热切换**（模型工具列表已进上下文）：`McpTool::execute` 失败语义 = `ExecutionError` 带工具名（回合可继续）。但连接失败签名（`call failed: Transport send error` / `Transport closed`）会被 **mcp_recovery 的恢复装饰器**接管：单飞 `mcp.recover_host` host call → Kotlin 恢复阶梯（拉起 Termux/host + `reload_mcp`）→ 注册表同名重发一次（预算 1），模型无感（见 call-chains.md §3.1.2）；reload 迁移的新工具同样重新包上装饰器。接管/兜底的交接只在上述三个选择点发生。

termux-only 命名矩阵（MCP 提供裸 `bash` 时）：HostTool 顶替 `bash`（RUN_COMMAND 兜底），MCP 注册名 = builtin 模板展开（默认 `termux_bash`、自定义如 `{prefix}.{tool}` → `termux.bash`）——白名单 `bash` / `termux_bash` 两者在默认模板下都有实体，历史配置零变动。

## 8. `provider.rs` 详解（secret 注入）

- `MobileSecrets`：`RwLock<HashMap<provider名, key>>`，进程重启由宿主重新注入。
- `mobile_provider_factory`（provider.rs:44-75）：解析别名 → provider 名（**解析失败也要尽量报出 provider 名**——常见场景：宿主先 set_api_key 再配置模型）→ key 查找顺序：**内存注入 → 该 provider 的 `api_key_env` 环境变量 → 报错**「provider '…' 的 API key 未配置（宿主请先 set_api_key）」→ `build_genai_provider(config, alias, Some(key))`。

## 9. `registry.rs` 与 `alog.rs` 概要

- `RegistryStore`（registry.rs:46-56）：`{config_dir}/registry/` 下每源一个 `<id>.json.zst`（**zstd level 3**）+ `meta.json`（`v` 格式版本 + 各源 `SourceStatus{fetched_at_ms, entries, raw_bytes, zst_bytes}`）；meta 写入走 tmp+rename。两级内存缓存：`parsed`（归一化索引，first-wins 折叠）与 `raw`（**原始 JSON 树**，搜索/浏览要遍历原始键——保留 provider 前缀，首次搜索才加载，Arc 共享零整树克隆）；`update_source` 成功后一并失效重建。解析 / 合并 / 匹配逻辑在 `openslate-core/src/model_registry.rs`（见 `03-models` 章）。更新网络失败/损坏按空处理并 alog 告警。
- `alog.rs`：`TAG = "OpenSlateRust"`；Android 上 `extern "C" __android_log_print` 直打（Verbose..Error → 2..6），desktop `log()` 为空操作；`alog!` 宏 = Info 级阶段打点；`install_panic_hook`（Once 幂等）：panic → logcat `PANIC: {msg} at {file}:{line}:{col}` 再走默认 hook（tokio spawn 会吞 panic，必须经 hook 外显，否则表现为无声的「空回合」）；`install_raw_usage_logger`：把 vendor/genai 的原始 usage 诊断流接到 logcat（网关 usage 字段变化看 logcat 即可，免抓包）。

## 10. 测试

- 集成测试 `crates/openslate-mobile/tests/runtime_flow.rs`（desktop 可跑，无 Android 依赖）：`mobile_runtime_closed_loop_streaming`（scripted provider 流式回包闭环）、`mobile_runtime_host_call_roundtrip`（信封往返 + resolve）、`mobile_runtime_hello_version_check`（版本不匹配报错）、`mobile_runtime_shutdown_rejects_send`、`mobile_runtime_tool_visibility_roundtrip`。harness：`ScriptedProvider`（脚本化 `ModelStreamEvent` 流）+ `Collector`（事件采集 + `wait_for_count` 按类型计数等待）。
- 审批集成测试 `crates/openslate-mobile/tests/approval_flow.rs`（tool-approval P1b 出口标准，六路径）：Manual 直问 / Auto+Low 跳过审查 / 审查 safe 静默放行 / risky 弹人（note 下发）/ 审查失败 fail-safe 弹人 / 超时 fail-safe 弹人 + 作用域语义（approve_all / session_mcp 会话放行、always_* 持久化热更、rules cell 共享防回归）。审查模型走 ScriptedProvider 注入。
- 单元测试在各模块 `#[cfg(test)] mod tests`：runtime 生命周期 / 安全守卫（触达私有状态；DB 校验经独立 scratch 连接）、hostcall 四条路径（roundtrip/error/timeout/fail_all）、exec（注册矩阵 / 热切换 / native 执行 / 截断 / 脱敏 / MCP 接管判定与让位）、bootstrap（幂等 / realign 四场景）、provider（set/get 往返）。
- 降级阶梯（runtime tests，手写 std::net MCP HTTP 挡板同 app wiring 集成测试模式）：`set_exec_backends` 热切换不误杀 MCP bash 工具、HostTool 在场 reload 升级接管（不撞名回滚）、reload 全败兜底在场、reload 真冲突回滚装回让位工具、termux-only 命名矩阵（仅 termux_bash 在场、`bash` 不注册；默认 / 自定义 builtin 模板，证明不硬编码）、builtin 模板 setter（校验 + 落盘 + 空串删键）。
- 附件 tokens 响应侧回填（attachment_tokens/runtime tests）：状态机（首次调用闩值 + agentic 后续调用忽略 / 首次调用 None 即不分模态 → 无回填且后续 Some 不追认 / TurnError 也收尾已闩实值 / 未标记回合忽略 / clear_pending / map first-write-wins / transcript 只填未填条目）；集成（scripted provider 流 Usage 事件 + Done.usage 同值，真实 genai 流路径形态）：单图回合 → map + 快照 User 附件带 `tokens`、同回合第二条 Usage 忽略；多图提交恒无回填（覆盖一个 sync 周期 + 快照无 tokens 键）；不分模态 usage（智谱口径）→ 无回填；重启可见性（transcript_entries 追加式不可变 → 重开首份快照经 meta_json 重放即带 tokens）；import JSON 恒无 `tokens` 键（导入零网络契约）。
- 跑法：`cargo test -p openslate-mobile`。

## 11. 接手检查清单

1. 改 `RuntimeOptions` 默认值 / 装配顺序 → 先看 `create_with` 注释与 §3.2 的顺序约束（清扫必须在恢复之前）。
2. 新增 host tool → Rust 侧注册 `HostTool`（或进 extra_tools），Kotlin 侧 `RuntimeBridge.handleHostCall` 补分发。
3. 改事件协议（新 ServerMsg 变体）→ `events.rs` 无需动（透传 JSON），但 Kotlin `handleEvent` 要补分支。
4. 改 bash 后端命名 → 同步 `DEFAULT_ROOT_AGENT_MD` 白名单与 `ALL_BASH_NAMES` 清理表 + Kotlin `handleHostCall` 的工具名匹配。

## 12. 附件（多模态输入，存储 v5 §3/§6/§7；L4）

端到端链路：Kotlin `PickVisualMedia` → 拷 cache（HEIC 在 Kotlin 预转码 JPEG）→ FFI `import_attachment(path, mime, name)`（**纯本地零网络**，返回 `{ok,id,mime,filename,size_bytes,orig_bytes,processed,orig_width?,orig_height?,width?,height?}`，对比信息见 §12.1 契约）→ id 入 composer → Submit 拼 `"attachments":[{id,mime,filename}]` → 门控 → session 构进 user 消息（引用形状）→ `AttachmentDataProxy` 请求时装载字节 → L3 convert 产 Binary part（image_url/video_url）。**ByteArray/base64 不出现在任何边界**；发起 API 请求之前附件数据绝不上行任何服务器（导入侧无任何 token 预估，见 §12.5）。

### 12.1 ingest（attachments.rs）

- **嗅探表**（不信任 declared_mime，仅记偏差）：jpeg `FF D8 FF` / png 8 字节 / gif87a|89a / webp `RIFF…WEBP` / mkv EBML `1A 45 DF A3` / mp4+mov `ftyp`（brand `qt  ` → mov，heic 家族 brand → 拒绝，其余 mp4）。
- **图像归一化**（2026-10-21 两轮演进：提速快速通道 → 用户可配置迭代压缩）：入口防呆 = **固定 50MB 源文件常量**（`MAX_SOURCE_IMAGE_BYTES`，非用户旋钮——归一化总能压到达标，入口只挡病态大文件；`max_image_mb` 语义已迁移为处理后上限）；
  - **快速通道（零解码零编码零重采样）**：JPEG/PNG 先 **header-only 读尺寸**（`header_dimensions` → `image::ImageReader::into_dimensions`，只解析 SOF/IHDR 不碰像素），长边 ≤ 用户 `max_image_dim` 且字节 ≤ 用户 `max_image_mb` → **字节原样 passthrough**。JPEG 本就在 wire 白名单内无需转码——相机截图/压缩图直接省掉全处理流程（12MP 照片在 P10 lite 级 SoC 上数秒 → 毫秒级 header 解析）。**EXIF 方向不处理**：passthrough 保留原字节，方向解释交给 provider（全解码路径同样不处理 EXIF，行为一致）。header 解析失败（畸形/截断）→ 回落全解码路径给准确错误；
  - **迭代压缩达标**（transcode 路径，**用户双旋钮**：`max_image_dim` 等比缩放目标 + `max_image_mb` 处理后大小上限，FFI `set_image_processing` 热更）：① 等比缩到 ≤ 用户 dim（只缩不放，**`CatmullRom`** 快档——纯 Rust 无 SIMD 下比 Lanczos3 快数倍，画质对视觉模型够用）；② JPEG **q85** 编码 ≤ 用户 mb → 完成；③ 超限 → 质量 **q85→70→55** 逐档重编码；仍超且还能缩（target ×0.85 后仍会真正变小、地板 1024 之上）→ **缩边重试**（每轮从原图重采样，不累积二次缩放）；④ **地板**：dim ≤1024 / 缩无可缩 且 q55 仍超（病态小图高噪点）→ 取最后结果 + `tracing::warn`，**不再失败**。动图取首帧；
  - 出口硬校验（provider 安全公共分母，两条路径都执行）：像素 ≤6000²、字节 ≤5MB——validation 已限用户旋钮 dim ≤6000 / mb ≤5，断言是 wire 侧最后防线（达标路径恒 ≤ user_cap ≤5MB）。出口 mime 恒 `image/jpeg|png`——归一化器即 wire 白名单执行者；
  - **分段耗时日志**（真机慢了 logcat 直接可诊，格式统一 `attachment timing:` 前缀）：快速通道打 `fast_path=…_passthrough`；迭代路径逐档 `attempt=N q=Q WxH → NB >|≤ cap`、缩边 `target X → Y`、地板 `floor q=…`、终值 `q=Q attempts=N`；`import_attachment` 打 `read/normalize/sha256/write/total`；FFI `import_attachment_json` 入口/出口各一条总耗时。样例：`attachment timing: attempt=2 q=70 1550x1200 → 1980000B ≤ cap 2097152B`。
- **视频 passthrough 不转码**；落盘 ≤`max_video_mb`，发送另受 `max_inline_video_mb`。
- **落盘**：sha256 over 最终字节 → `files/{sha256}.{ext}`（store `insert_attachment`，幂等覆盖同名字节）+ 行（run_id NULL 暂存）；`MobileRuntime::import_attachment_json` 为 FFI 后端（block_on，IO 线程）。
- **import JSON 对比信息契约**（2026-10-21，chip 点击展开展示）：成功对象
  `{"ok":true,"id","mime","filename","size_bytes","orig_bytes","processed","orig_width","orig_height","width","height"}`。
  `processed` = `"passthrough"`（小图 JPEG/PNG 零转码，orig 尺寸=处理后、orig_bytes=size_bytes）|
  `"transcode"`（缩放/重编码，orig_* 为 header 尺寸/源字节数，width/height 为缩放后）|
  `"video_passthrough"`（视频不转码，尺寸四键**省略**——嗅探层不解析容器头，不做重解析；
  orig_bytes=size_bytes）。尺寸四键图像恒在（orig 来自 header-only 读取）。UI 据
  `processed` 生成「为什么这样处理」文案；`tokens` 键恒缺省（响应侧回填，见 §12.5）。
- **导入零网络**（2026-10-21 方向调整）：导入是纯本地操作，**绝不发起任何网络请求**——附件字节只在真正发起模型请求时随请求上行。曾有的智谱 tokenizer 预估（fix-8）已整体移除（`estimate_attachment_tokens`/`is_zhipu_endpoint`/`tokenizer_client`/`tokenizer_mock` 与 import JSON 的 `tokens` 键、mobile 对 reqwest/webpki-roots 的依赖增量）。附件 chip 的 token 数改由**回合结束后响应侧回填**（见 §12.5）：真实模型响应顺带携带才显示，否则回退文件大小。

### 12.2 Submit 门控（runtime.rs `gate_submit_attachments`，§3.3/§3.4/§6.4）

`send()` 的 spawn 内、dispatch 前执行；矩阵（纯函数 `attachments::gate_submit_attachments` 可单测）：

| 条件 | 结果 |
|---|---|
| 图+视频混发 | 拒「图像与视频请分开发送」（单点常量，实测可放开） |
| 图 > 50 张 | 拒（`MAX_IMAGES_PER_SUBMIT` 常量；convert 层同值兜底） |
| 总字节 > `max_inline_total_mb` | 拒并列出实际总量（fail fast 不裁剪） |
| 含图且 registry caps 无 image 模态 | 拒「当前模型不支持图像输入…」 |
| 含视频且 caps 无 video 模态 | 拒（无论开关） |
| 含视频且 `experimental_video=false` | 拒「当前版本视频输入为实验性功能，请在设置中开启」 |
| 单视频 > `max_inline_video_mb` | 拒并列出文件与实际大小 |
| caps 未知（手填模型无元数据） | 放行，provider 错误原样透出 |
| 空文本 + 附件 | 拒「请为附件输入文字说明」（session 对空文本静默丢弃，必须 fail fast） |
| 附件 id 不存在/已过期 | 拒「附件不存在或已过期」 |

- caps 数据源：image = registry 三源合并 `lookup_local().supports_vision`；video = `RegistryStore::supports_video`（models.dev 条目 `modalities.input` 直读——`ModelMeta` 无 video 维，L3 冻结面）。均 `None`=未知放行。
- 同 sha256 重复导入去重（first-wins）——省 token 且避免 bind 撞 `UNIQUE(run_id, sha256)`。
- 拒绝全部走 **Notice（Error 级）**，文案 = 设计文档措辞。

### 12.3 绑定与字节装载

- **绑定**：门控通过 → dispatch（session 把引用构进 user 消息 + transcript 条目）→ 引擎 spawn 后 `bind_landed_attachments`（`bind_attachments(ids, run_id)`；以「history 尾部是刚落地的带附件 user 消息」为落地判定——busy 拒绝/回滚不绑，暂存行留给下次或 7 天 GC）。
- **装载**：provider 工厂统一包 `AttachmentDataProxy`（`create_with` 装配期，含测试注入的 factory）——GenerateRequest 进 provider 前为 data=None 的引用从 files 读字节，单点执行 `max_inline_total_mb` 封顶；多步回合每步请求、迟到答案续跑、resume 续聊全走这一处。**超封顶就地丢弃**（2026-10-21）：data=Some 的附件（含 read_file 工具附件——工具读图链路把字节装在 Tool Message 上）累计超总封顶时置 None + warn（R5：模型丢图不丢文本），而非仅计入预算。
- **resume 回填**：`open_session` 与重启恢复（assemble_state）加载消息后 `refill_message_attachment_data`（总字节封顶同配置；超限 warn 跳过——模型丢图不丢文本，R5）。工具附件 id `tool-{n}` 不落 attachments 表，resume 后不回填（跳过 + warn，同语义）。
- **缩略图**（2026-10-21）：`attachment_thumbnail_base64(store, id, max_px)`——id → files 原图 → 长边缩到 max_px（只缩不放，`<=0` 按 256、上限 2048）→ JPEG q80 → base64；FFI `getAttachmentThumbnail` 后端（des-2 Android lane）。仅图像（视频/未命中/解码失败 → Err）。

### 12.4 启动 GC

`assemble_state` 启动清扫块末尾调 `store.gc_stale_unbound_attachments(now_ms)`：删 7 天前导入未发送的暂存行 + 对应文件清扫（失败仅告警）。其后调 `store.gc_orphan_output_files()`（长输出 C3）：清 `files/outputs/` 下无行引用的文件（收敛「文件写成功、行未落」崩溃窗口），失败仅告警。

### 12.5 附件 image tokens 响应侧回填（协议 v5，2026-10-21）

方向（用户拍板）：① 发起 API 请求之前**绝不**把附件数据发到任何服务器（导入零预估，见 §12.1）；② 只有**真实模型响应**顺带携带的 image token 信息才显示——OpenAI 系 chat usage 的 `prompt_tokens_details.image_tokens`（vendor 补丁 #3 归一化为 genai `Usage::image_input_tokens` → core `Usage::image_tokens`，见 03-models/model-genai.md）。**智谱 chat usage 不分模态、天然无此字段 → 永远回退文件大小显示，符合预期。**

- **数据源口径**：本回合**第一次**模型调用的 `usage.image_tokens`。agentic 循环里历史图每轮全量重发、每次都计入 usage——只有首次调用才是"这张图的首次完整成本"，后续调用的值一律忽略（闩锁语义）。
- **单图限定**：仅当本回合 Submit 的 user 消息**恰好带 1 张图片附件**时回填；多图 usage 不分图拆分，回填哪个都是错 → 全部不填（UI 回退文件大小）。视频同理。
- **回合收尾**：TurnOk 与 TurnError 都收尾（首次调用已完成即测量真实，回合后续失败不否定该值）；首次调用即 None（不分模态）→ 本回合无回填，后续调用冒出的值也不追认。
- **流转**（`attachment_tokens.rs`，避开引擎线程持 core 锁的窗口）：`EventSink` tap（广播线程，新增 `set_tap`）收 `Usage` 闩值 / `TurnOk`·`TurnError` 收尾推 completed 队列（tap 里**绝不锁 core**——引擎广播 TurnOk 时持有 core 锁，锁了就死锁）；`transcripts.rs::sync_loop` 每周期（活跃 300ms / 空闲 1s）在持锁快照段消费队列、原地更新 live transcript 的 `EntryDto::User.attachments[].tokens` → 之后所有快照（`build_snapshot` 从 inner.transcript 构造）自然携带。Submit 标记在 runtime `send` 的 spawn 内设置（dispatch 前；busy/回滚未落地则清除）。
- **持久化 / 重启可见性**：`transcript_entries` 追加式不可变（回填晚于 User 条目落库，无 UPDATE API）——重开会话时 `rebuild_transcript` 经 `overlay_tokens_from_messages` 从 `messages.meta_json`（每条 assistant 消息的 `MessageMeta.usage.image_tokens`）**按同口径重放**（单图 user 消息 → 其后第一条 assistant 消息配对），重启后 tokens 照常读回。live 会话与重放路径共用 `AttachmentTokenBackfill::apply_to_transcript`。
- **协议形状**：`AttachmentDto.tokens: Option<u64>`（serde default + None 跳键，v5；仅 `EntryDto::User` 出现，`ClientMsg::Submit` 方向忽略）。Kotlin 契约：有值显示 token 数，缺键回退文件大小。

## 13. transcript 持久化与存储健康（存储 v5 §4/§5；L4；历史转写丢失修复 C1/C2/C3）

- **同步任务**（`transcripts.rs::sync_loop`，create_with spawn，300ms 活跃 / 1s 空闲）：对比内存 transcript 与已落库游标，新条目按 `seq`（= 下标）写 `transcript_entries`；兼**附件 tokens 回填消费点**（§12.5，每周期 drain completed 队列更新 User 条目）。**写库段收口在 `SyncState`**（C1 重构）：cursors 表 + `persist_range(force)`（原内联写库段原样提取），**唯一写者**、转写只在尾部追加 ⇒ 游标恒等于该 run 已落库行数——run 切换时从 DB 行数自举；pop 重建（C2）后经 `invalidate(run)` 清游标 → 自举 0 → 全量重写（幂等）。锁序唯一方向：**cursors（tokio Mutex，可跨 await）→ core（std Mutex，短临界区快照）**，不得反向嵌套。**回合进行中推迟 ToolCall 条目**（回合结束折叠后才是终值）；`insert_transcript_entry` 为 INSERT OR IGNORE（自举读失败按 0 重插 = 幂等补写，不再是致命告警死循环）。seq 分配与 recorder 的 AtomicI64 模式同构（自举例程）。
- **强制冲刷（C1，防新增丢失）**：三个「内存转写即将蒸发」的时刻都走 `SyncState::flush_now(force=true)`（与 sync_loop 共享同一游标表）——① **NewSession**（session.rs 经 `TranscriptFlusher` 钩子前置冲刷，锁段开头另设二次 busy 守卫挡 flush 窗口内的竞态 submit）；② **open_session 切换**（busy 守卫后、swap 前冲刷旧 run）；③ **shutdown**（`rt.take()` 之前 block_on 同步完成）。冲刷若写了未折叠终值的 ToolCall 条目（`detail.output.is_none()` = 回合未完成快照）→ 直接向 DB 追加 `EntryDto::Meta{text:"回合未完成，以下为中断时的快照"}`（`INCOMPLETE_TURN_META`，区别于 `DANGLING_TAIL_META`），不碰内存转写。
- **两级重建**（`rebuild_transcript`，open_session + 重启恢复共用）：优先 `list_transcript_entries`（v5 真相源，Approval/Meta/StepBreak 等投影拿不到的条目都在）；空（v5 前 run）→ `messages_to_transcript` 消息投影回退（L6 换 protocol 共享投影 `project_from_messages`）。
- **截断自愈（C3，修已损坏 run）**：重建时总是算消息投影对照；entries 行的 durable 前缀（User/Assistant/ToolCall 计数）短于投影且逐条对得上（User↔User / Assistant↔Assistant；ToolCall call_id 精确优先、任一侧缺失按名字——配对语义同 `fold_tool_outcomes` 双分支；种类错位/数量不齐立即 bail 宁缺勿错）→ 补 `EntryDto::Meta{text:"检测到未同步的转写尾部，已从消息记录恢复"}`（`HEAL_META`）+ 投影尾部。**不重写表**：重开后游标按行数自举 → 补齐尾部按接续 seq 写回（幂等自愈）。
- **悬挂尾部 Meta（§5）+ pop 回写（C2）**：`open_session` / 重启恢复 pop 悬挂 assistant（带未答 tool_calls 的尾消息）时**回写 DB**——`delete_messages_from_seq(run, max_seq)` 删尾行 + `delete_transcript_entries_for_run` 全删条目行 + `invalidate` 清游标，重建走消息投影（完整、无悬挂条目）；删干净后第二次 open 不再命中 pop 分支（修「反复重开 ⚡Meta 行累积、悬挂消息夹进历史中间」）。pop 后若转写非空仍追加 `DANGLING_TAIL_META` Meta 行——恢复后的 UI 不再"无声少一截"。
- **存储健康（§5）**：同循环检查 `recorder.pending_persisted_writes() > 0` → Notice Warn「存储写入失败，历史可能不完整」（per pending 期一次）。
- **token 聚合**：`list_sessions_json` 每项含 `input_tokens`/`output_tokens`（v5 runs 聚合列）。
- **会话身份轮换（C2）**：open_session 切到**不同** run 时 `clear_session_scopes()`（core 锁外）+ `session_id = new_session_id()`（session.rs pub 导出）——session_id 变化驱动 ChatScreen 会话揭示，A 会话的 approve_all 白名单不再泄漏到 B；同 run 重开不轮换。

## 14. 测试补充（L4；历史转写丢失修复测试面）

`attachments.rs` 单测：嗅探表（含 heic 拒绝）/ 归一化（缩放重编码 + PNG passthrough 逐字节）/ 门控矩阵（混发、50 张、25MB、20MB、开关两态、caps 三态）/ sha256 去重 / ingest 往返（真实文件库：PNG passthrough、MP4 passthrough、declared 说谎按嗅探、未知格式与超限拒绝）/ 回填封顶（超限跳过、缺行不失败）。
`runtime.rs` 集成（CapturingProvider 捕获 GenerateRequest）：import→submit e2e（data 装载字节一致 + bind + 持久化引用 + transcript 行带引用）；门控 notice 矩阵（纯文本模型拒图且引擎零调用 / 混发 / 实验关 / 空文本 / 未知 id；多模态+开关开 mp4 放行）；重启 resume 回填（第二轮请求仍带图）；启动 GC（8 天前暂存清、新暂存留）；transcript 往返（entries 行重建 + 旧 run 投影回退不回归 + !same_run 重开轮换 session_id）。
历史转写丢失修复（C1/C2/C3）：`transcripts.rs` walk 单测（call_id 配对 / 名字兜底 / 错位与数量不齐 bail / 装饰跳过 / 无缺尾 no-op + 三条 Meta 文案 pin）；`runtime.rs` 集成——悬挂 pop 回写（第一次 open 删行 + 投影重建 + 恰一条 DANGLING Meta；第二次 open 无 pop 无新增 Meta、messages 尾行已删）、截断自愈幂等（messages=5/entries=2 → 2 前缀 + HEAL Meta + 投影尾 3，再 open 一致）、中断回合冲刷（tool 执行后模型流错误 → NewSession → DB 含 as-is ToolCall + INCOMPLETE_TURN_META，重开转写完整）、NewSession 二次 busy 守卫（冲刷钩子注入 submit → 拒绝、转写未清、session_id 未轮换）、shutdown 中冲刷（挂死工具回合中关停 → entries 含 User + as-is ToolCall + INCOMPLETE Meta）；store `delete_messages_from_seq` / `delete_transcript_entries_for_run` 边界（空集/越界/计数/幂等/不误伤他 run）与 `insert_transcript_entry` OR IGNORE 语义（重复 seq 静默让位首写行）。

## 15. output_query 工具（长输出落盘 + output_query C4；模块 `output_query.rs`）

模型侧的长输出取回通道：工具结果超 `[limits].output_store_threshold` 落盘后，对话内只剩
`[STORED OUTPUT ref="out-…" …]` 头尾预览（core `build_preview`，冻结格式），模型经本工具按
`ref` 查询全文切片。全链路一图流见 [call-chains.md §3.4](call-chains.md)。

### 15.1 注册与接线

- **注册**（runtime.rs `create_with`）：`extra_tools.push(OutputQueryTool::new(output_store_handle.clone()))`
  ——在 `AskUserTool` / `mobile.ping` 旁；PTC namespace 默认 `None`（`run_code` 沙箱内经
  `tools.output_query` 平名可用，不需专门处理）。
- **store 接缝**：复用 `output_storer.rs` 的 `SharedStore`（`Arc<OnceLock<Arc<SqliteStore>>>`，
  AttachmentDataProxy 同款后注）——与 storer **同一注入点**（`assemble_state` 后
  `output_store_handle.set(...)`），同一句柄两用。
- **AppState 接线**：工具本体随即 move 进 registry，接线经共享
  `Arc<OnceLock<Weak<AppState>>>`（`state_handle()` 交出）在 `create_with` 尾段
  `set(Arc::downgrade(&state))`（`AskUserContext.wire` 同款防环先例）。用途两个：
  ① `find_tool_output(当前 run_id, ref)` 的**会话域**（严格隔离，见 §15.2）；
  ② 累计输出封顶 =
  `RuntimeLimits::from_config(&config).max_output_bytes` 生效值（0 = 不限）。未接线 /
  AppState 已释放（shutdown 后）→ 无当前 run + 缺省 64KiB。
- **装配前调用**（注入窗口，正常流程不可达——注入先于首个回合）：返回明确错误文案
  `output_query is not available: the output store is not wired yet …`（status=Error，
  不是 Err/panic）。
- **审批**：无副作用纯读（本会话已脱敏落盘的数据）——`SideEffectFreeApproval`（askuser.rs，
  原 `AskUserExemptApproval` 泛化）免审；否则 Auto 缺省下启发式 Medium 会为每次查询付一次
  AI 审查调用。

### 15.2 行为语义

- **ref 形态与会话隔离**（用户拍板 2026-10-22）：ref 是**会话内唯一**的自增短 id
  （`out-N`，由 output_storer 按 run 内序号分配——同 run 同 sha 幂等复用既有 ref）。
  行查找为 `find_tool_output(当前 run_id, ref)` 的 **`(run_id, ref)` 精确匹配**：
  一个 run 的 ref 不能被其他 run 解析（跨 run 同 ref 各自的行互不可见），无「当前
  run 优先 / 任意 run 回落」路径；无当前 run（未接线 / shutdown 后）返回明确错误。
- **数据链**：`ref` 校验（`out-` + 1..16 位数字）→
  `find_tool_output(当前 run_id, ref)` → 无行返回 Error 文案（含期望格式 +
  「ref 仅本会话内有效」提示 + 从 `[STORED OUTPUT` 标记取 ref 的提示）→
  `read_tool_output_bytes(sha64)` → zstd 解压（纯内存，≤1MiB 量级，对齐 storer 的不
  spawn_blocking 约定）→ **`len == orig_bytes` 完整性校验**（不等报错——防半写文件/内容
  错位被静默错读）→ 按 op 切片。参数校验错误（op 与字段不匹配 / start>end / n 越界 /
  非法 regex 编译错误原文回显）一律普通文本 status=Error 回喂自纠。
- **五算子**（flat schema，`additionalProperties:false`，required `ref`+`op`）：
  - `head` / `tail`：前/末 N 行（`n` 默认 50、上限 2000；tail 末行无换行也算一行，n ≥ 总行数返回全文）；
  - `lines`：`start..=end` 闭区间（1-based，start ≥ 1 且 ≤ end）；
  - `grep`：逐行 Rust regex（`ignore_case` 经 `RegexBuilder::case_insensitive`），恒带行号
    （`{line_no}:{line}`），全量收集后封顶，尾注 `showing M of T matches`（M = 实际保留行数，
    封顶时 M < T 即截断提示；单行超 cap 的命中计数但不展示）；
  - `wc`：`lines: N, bytes: M`（+ `note: no trailing newline`，仅非空且末字节非 `\n` 时）。
- **行迭代核心一份共用**（`LineScanner`，改造自 uutils/coreutils `head` take.rs，MIT 署名
  保留在源码常量区）：head/tail/lines 走同一条「跳过 + 拷贝」通路（tail = 跳过
  `total_line_count - n` 行，跳过量来自落盘行的 `line_count`——与已过完整性校验的原文同源）；
  grep/wc 逐块扫描。边界：恒定 64KB 读块复用不膨胀（超长单行只膨胀行缓冲）、读侧 EINTR
  重试、行跨块拼接、末行无换行亦算一行。行切分**只按 `\n`**：行内 `\r` 保留（CRLF 文件
  grep 输出行尾带 `\r`，工具 description 已注明；与 `str::lines()` 会剥 `\r\n` 之 `\r`
  不同——行**计数**两种约定恒等）。设计稿曾把 tail 记为
  `copy_all_but_n_lines(total-n)`，方向反了（该函数是 `head -n -K` 语义，会返回 head），
  已按「跳过 + 拷贝」正确实现，`copy_all_but_n_*` 两个无算子对应的函数未引入。
- **累计输出封顶** = `max_output_bytes` 生效值：超限 UTF-8 字符边界截断 + 尾注
  `[output_query: result truncated at {cap} bytes — narrow start/end, reduce n, or use grep]`。
- **root.md 白名单**：bootstrap 默认白名单已加 `output_query`；未定制存量经
  `LEGACY_ROOT_AGENT_MDS` 自动迁移；**定制过的 root.md 由启动时的外科式补齐兜底**
  （`ensure_builtin_tool_whitelisted`：`REQUIRED_BUILTIN_TOOLS` 缺失项追加到 tools:
  列表尾，其余逐字不动——app 私有目录无编辑器可达，用户没有手动途径）。
  不在白名单 ≠ 报错，只是模型看不到该工具。

### 15.3 测试面

`output_query.rs` 内嵌 16 个用例：五算子矩阵、末行无换行、CRLF 保 `\r`、空输出、1MiB 单行
（含「命中行超 cap 计数不展示」钉死）、64KB 块边界跨行、多字节封顶劈裂（65536 切点落
`中` 字节中间 → 65535 边界）、grep ignore_case、参数错误文案矩阵（16 组，含 ref 1..16
位数字边界与旧 16hex 拒收）、未知 ref、**跨会话隔离**（run-2 查 run-1 的 ref → Error +
会话隔离文案，run-1 对照组仍可解析）、orig_bytes 完整性失败、装配前错误（store 未注入 /
无当前 run）、`LineScanner` 单元直测（空内容/空行/EOF 后 skip）。
装配链回归：`tests/runtime_flow.rs` 默认白名单断言含 `output_query`。
