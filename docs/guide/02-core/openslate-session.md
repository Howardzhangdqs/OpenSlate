# 02-3 · openslate-session

> 传输无关共享单会话核心（`crates/openslate-session/src/`，5 个模块）。从 openslate-server 抽取，WS server（ConnectionHub 扇出）与 mobile（EventSink → JSON 回调宿主）共用同一份事件序与转写镜像语义。依赖 core/app/protocol/store-sqlite，**不依赖任何传输**。

## 模块结构

| 文件 | 职责 | 核心符号 |
|------|------|----------|
| `state.rs`（824 行） | 共享状态 + 事件出口 + 转写镜像 | `SessionCore`/`CoreInner`/`MsgSink`/`AppState`/`ConfigPaths`/`SessionRun`/视图与镜像辅助函数 |
| `engine.rs`（530 行） | 引擎回合任务 | `SessionProgressBridge`/`spawn_engine_turn`/`engine_turn`/`run_auto_compact`/`generate_title` |
| `approval.rs`（436 行） | 会话审批桥（双作用域集合 + rules 共享 cell） | `SessionApprovalBridge`/`session_allows`/`clear_session_scopes`/`rules_handle` |
| `question.rs` | ask_user 应答接缝 | `QuestionHandler` |
| `config_ops.rs`（1312 行） | 配置 CRUD 写回链 | `ConfigChange`/`apply_config_change`/`apply_approval_rules`/`apply_set_api_key` |
| `session.rs`（895 行） | ClientMsg 派发 | `dispatch`/`handle_submit`/`open_session_run`/`generate_session_title` |

运行时事件：`EVENT_TARGET = "openslate_session"` + `session_event!` 宏——单行英文、grep 友好、**token/api key 绝不进**。

---

## 1. state.rs

### 类型定义

```rust
// :133
pub struct SessionCore { inner: Mutex<CoreInner> }   // ★ std Mutex（见锁序不变量）
// :93
pub struct CoreInner {
    pub session_id: String,           // new_session_id()（pub）= UUID v4 simple（mobile open_session 会话轮换复用）
    pub session_label: String,
    pub history: Vec<Message>,        // 真相源
    pub transcript: Vec<EntryDto>,    // 展示镜像
    pub running: bool,
    pub compacting: bool,
    pub depth_cur: u32,               // call_agent 嵌套深度（Delegate 标记增减）
    pub agents_running: u32,
    pub tool_calls_cur: u32,
    pub model_alias: String,
    pub cancel: Option<CancellationToken>,
    pub config: OpenSlateConfig,
    pub agents_cfg: AgentsConfig,
    pub agent_tree: AgentTree,
    pub skills: Vec<SkillInfoDto>,
    pub store: Option<SqliteStore>,
    pub session_run: Option<SessionRun>,       // {run_id, recorder: Arc<RunRecorder>}
    pub manager: Option<RunManager>,           // 在家/引擎持有两态槽位
    pub engine_task: Option<JoinHandle<()>>,
    pub paths: ConfigPaths,                    // {active, global?, local?}
    pub stats: SessionStatsDto,                // turns/tokens/cost 会话累计
    pub stream: StreamBuffers,                 // {answer, reasoning} 流式缓冲
    pub tool_timers: HashMap<usize, Instant>,  // transcript 下标 → 开始时刻
    pub pending_step_meta: Option<String>,     // 挂起的 usage 行（fix-19 hold）
}
// :126  busy() = running || compacting   （submit/new_session 守卫）
// :144  lock() 毒化容忍：unwrap_or_else(|e| e.into_inner())
```

- `AppState`（:150）：`{core, sink: Arc<dyn MsgSink>, approval, auth_token?, provider_factory, origin: &'static str, _mcp: McpConnectionGuard}`。origin：server 保持 `"server"`（兼容历史数据）、mobile 用 `"mobile"`（run 落库来源标注）；`_mcp` 是保活字段（drop 序约束）。
- `ProviderFactory`（:40）= `Arc<dyn Fn(&OpenSlateConfig, &str) -> Result<Box<dyn ModelProvider>>>`——**测试接缝**（见 §5）。
- `MsgSink` trait（:51，5 个方法 + as_any）：实现必须**非阻塞**（broadcast 入队即返回）。
- `TranscriptFlusher` trait（历史转写丢失修复 C1，state.rs）：`fn flush(&self, state: Arc<AppState>) -> BoxFuture<'static, ()>`——NewSession 清空内存转写**之前**的强制落库钩子（回合未干净收尾时 sync_loop 推迟的 ToolCall 尾部只有此刻还能写库）。注入点仿 questions：`SessionCore` 私有 `transcripts: Mutex<Option<Arc<dyn TranscriptFlusher>>>` + `set_transcript_flusher` / `transcript_flusher()`；server/cli/tui 不注册（None = 行为不变），mobile 装配 `SyncState` 委托（见 mobile 文档 §13）。

### 视图构造（纯函数）

- `effective_limits(config) -> LimitsConfig`（:170）：`limits.clone().unwrap_or_default()`——Option 语义解掉。
- `build_config_view(inner, approval_rules: &ApprovalRules) -> ConfigViewDto`（:206，tool-approval 起带第二参数）：capabilities 出**生效值**（并入缺省回退——客户端拿到的即真相，与 limits 的"server 解析默认值"同一先例）；`approval` 视图 = 传入的生效规则快照投影（`ApprovalConfigViewDto`——legacy 映射后的真相，非 toml 原文）。**调用方必须先取 bridge rules 快照再进 core 锁**（锁序红线：先 bridge 后 core）。注意 compact 行的 "fast" 缺省仅是展示层历史默认值——引擎未绑定时实际继承会话当前模型（见 `run_auto_compact`），本视图不反映该继承（纯展示，无行为消费方）。
- `build_snapshot(inner, approval) -> SnapshotDto`（:215）：纯投影 + 审批桥队首待审。

### 转写镜像辅助（与 TUI transcript.rs 行为 1:1）

| 函数 | 行为 |
|------|------|
| `args_preview(args)`（:244） | trim + 去引号，≤40 列截断加 `…`（UTF-8 边界安全） |
| `cap_stored_text(s, cap)`（:258） | 字节预算截断，标记 `\n…（已截断）`；`ARGS_STORE_CAP = 4KB`（:271）/ `OUTPUT_STORE_CAP = 8KB`（:273） |
| `delegate_target(args)`（:277） | 从 call_agent 参数取子 agent id（agent_id/agent/child 三键尝试，失败回退 "call_agent"） |
| `tool_failure_summary(content)`（:290） | 5 个行首失败标记锚定：`Error:` / `approval denied tool` / `child agent call denied` / `[tool call cancelled` / `[error]`；首行 ≤60 字符摘要 |
| `flush_stream(inner)`（:321） | 冲刷 answer/reasoning 缓冲为条目（reasoning 先于 answer；空白丢弃）+ 冲 pending_step_meta |
| `tool_start_entry(inner, name, args, call_id)`（:344） | call_agent 走 `Delegate{agent, done:false}` + 深度/计数增；其余 push `ToolCall{call_id, status: Running, detail.args=capped}` + tool_timers 计时 |
| `tool_end_entry(inner, name, bytes, truncated, call_id)`（:372） | 配对**call_id 精确匹配优先**（身份匹配，不受状态限制），任一侧缺失 → 最后一条同名 Running（历史行为）；call_agent 额外 FIFO 完成最老未完成 Delegate |
| `fold_tool_outcomes(inner, messages)`（:409） | 回合 Tool 消息折叠：`msg.tool_call_id == entry.call_id` **身份匹配优先**（回合全量历史重放幂等，Failed 条目折合自身不错位），任一侧缺失 → **名字 + FIFO**配对（游标 = 扫描起点，TUI 同款语义；旧「过滤后 nth(count)」双重计数会丢同名第 2+ 个结果，已随本轮修正）；成功精化保住 live elapsed；失败标记翻转 Failed；输出全文留存 8KB；孤儿结果 debug 日志丢弃。`truncated` 判定 = `"[TRUNCATED:"`（旧 limit 截断）**或** core `STORED_OUTPUT_MARKER`（长输出落盘预览，C3——预览同样非全文，详情弹窗走全文闭环） |
| `approval_summary(req)`（:470） | 转接 protocol 的 From |

## 2. engine.rs

### SessionProgressBridge（:44，impl ProgressCallback）

- 全部回调持 core 锁「**先改镜像、再广播**」→ 同一连接上事件序 == 转写追加序。
- 事件映射：`on_request_start` 冲掉上一请求挂起 usage 行 + RequestStart；`on_delta`/`on_reasoning` 进 stream 缓冲 + 广播；`on_usage` 挂起 `pending_step_meta`（Meta 行落在本步工具行之下，fix-19 hold）；`on_tool_start/end` → `tool_start_entry`/`tool_end_entry` + 广播（preview 空串转 None；**call_id 透传**：条目落 id + `ServerMsg::ToolStart/ToolEnd.call_id` 携带，客户端按其精确配对与懒加载全量输出，空串转 None）；`on_step_end` 冲流 + 连续边界折叠为一个 StepBreak。
- `usage_meta_line`（:149）：`↑in ↓out [⎓cached]`（TUI 的 ttft/速率装饰是客户端渲染层，不进协议数据）。

### spawn_engine_turn（:169）与 engine_turn（:185）

- **spawn_blocking 专用线程 + `Handle::block_on`**：审批 `decide()` 是同步阻塞 trait、会等客户端应答；引擎执行期间还有多个同步回调。占 tokio worker 会饿死同 worker 上的转发/写任务（**实测复现过**）。阻塞池线程阻塞正是其设计用途；内层 future 经 Handle::block_on 驱动，tokio 定时器/通道/sqlx 均可用。
- 收尾（持 core 锁）：`running=false`、`cancel=None`、深度/工具计数清零；**回家的 manager 采用最新 agent_tree**（`manager.agent_tree = inner.agent_tree.clone()`，:218——回合期间工具可见性热替换只改 inner.agent_tree，碰不到被 take 的 manager）；`inner.manager = Some(manager)`。
- Ok：history 替换为 run 结果、flush、fold_tool_outcomes、stats 累计、广播 TurnOk。Err：flush、**保留 history 现状**（用户消息留着可重发）、广播 TurnError。

### run_auto_compact（:253）

pre-turn 摘要。history 整体搬出（std Mutex 守卫不能跨 await）→ `needs_compact` 判定 → capability=compact 计划（**代号选取：`[capabilities]` 显式绑定优先；未绑定 → 继承会话当前 `model_alias`**（同一 core 锁临界区取；历史上的"回退 fast 硬编码"已废止，不走 `capability_alias` 缺省）。resolve_model + provider_factory 任一失败 → None → 机械兜底）→ `compact()`（空白回复视作失败 → 机械兜底）→ 搬回。**compact 的 usage 计入会话累计（成本不计 run 行；token 在 session/server 的 RunFinal 口径下随 stats 进 run 行聚合——口径细节见 §5.1）**。`SUMMARY_SYSTEM_PROMPT`（:29）与 REPL/TUI **逐字一致**。

### 会话标题（:379-441）

- `TITLE_INPUT_MAX_CHARS = 400`（:383，输入截断）/ `TITLE_MAX_CHARS = 24`（:387，清洗上限——提示要求 ≤16 字符，留裕量兜啰嗦模型）。
- `generate_title(provider, model_id, first_prompt)`（:392）：`max_tokens: Some(48)` 小请求；失败/空白 → None（占位标题已在库）。
- `sanitize_title`（:427）：首行 → 去包裹引号/书名号/反引号（中英文都剥）→ 按字符截断；空/纯标点 → None。

## 3.5 question.rs —— ask_user 应答接缝（ask-user r5）

- `QuestionHandler` trait（宿主注入，默认未注入 = 通道关闭）：
  - `on_reply(request_id, action, answers) -> bool`：转发 `ClientMsg::QuestionReply`；`true` = 首答生效。宿主（mobile lane）职责：标记问卷状态 → `ask_user::append_late_tool_result` 补全原 tool call → `RunManager::resume_from_tool_result` 续跑。
  - `on_cancel_all()`（默认 no-op）：用户显式取消时关卷悬挂问题（best-effort）。
- **注入点**：`SessionCore::set_question_handler`（私有 `questions: Mutex<Option<Arc<dyn QuestionHandler>>>` 槽，`new()` 初始化）——刻意**不加 AppState/CoreInner 字段**（server/mobile 侧结构体字面量装配会破），`SessionCore` 仅经 `new` 构造零破坏；宿主实现持 `Weak<AppState>` 防引用环。
- 快照 `pending_questions` 由宿主持久层回填（server 投影恒空，见 state.rs `build_snapshot`）。

## 3. approval.rs —— SessionApprovalBridge

（审批链完整时序见 [../01-architecture/dataflow.md](../01-architecture/dataflow.md) §4.1。）

```rust
// :57
pub struct SessionApprovalBridge {
    sink: Arc<dyn MsgSink>,
    next_id: AtomicU64,                  // 从 1 起
    inner: Mutex<BridgeInner>,           // {pending: Vec<Pending>,
                                         //   allowed_tools: HashSet<String>,
                                         //   allowed_servers: HashSet<String>}
    rules: Arc<RwLock<ApprovalRules>>,   // 规则引擎共享 cell（见下）
}
```

- **会话作用域双集合（tool-approval）**：`BridgeInner` 拆 `allowed_tools`（(a) `approve_all` 写入）与 `allowed_servers`（(c) `session_mcp` 写入；键 = 审批作用域 slug，与规则引擎 `ApprovalRules.servers` 同口径）。
- **rules 共享 cell（P0-5 正道）**：`rules_handle() -> Arc<RwLock<ApprovalRules>>`（:80）是给装配层注入 `ApprovalManager::with_rules` 的入口——manager 与桥共享同一 cell，always_* / set_approval_config 的热更直达引擎（重建 manager 会拿到游离 cell，热更失效且无编译错误）。初始值 = `ApprovalRules::default()`（= Allow 从不问，P0-4）；装配层随后 `set_rules` 写入 `rules_from_config` 结果（mobile 另做 Auto opt-in）。`set_rules` / `rules_snapshot` 与 handle 同一 cell。
- `decide()`（:199 impl ApprovalCallback）：**会话作用域快路径**（`session_allows`）→ `sink.count()==0` 即 Denied（**无人可问即拒绝**，不永久阻塞）→ 注册 Pending + 广播 ApprovalRequested（summary 含 server/trigger/note，v3）→ **阻塞在 std mpsc recv**（多线程 runtime worker 上阻塞是安全模式——TUI 模块文档论证过，session 沿用）→ `choice_to_decision` + 兜底清理。
- **`choice_to_decision`（:148，六 choice 落点）**：`approve` = 单次放行；`deny` = Denied；`approve_all` = 写 `allowed_tools`；`session_mcp` = 写 `allowed_servers`（server 缺失的非 MCP 请求退化为工具级放行——防御分支，UI 只对 server≠null 出该选项）；`always_tool`/`always_mcp` = **本次放行但不写会话集合**——持久化与规则热更由 dispatch 层经 `config_ops::apply_approval_rules` 共享链负责，bridge 只管会话作用域内存态。
- `session_allows(req) -> bool`（:103，P0-3）：`allowed_tools` 含工具名 ∨ `allowed_servers` 含 req.server——AI 审查装饰器**第一步**先查它（(a)/(c) 批准后不再付 LLM 审查费）。短临界区（inner 自锁，叶子锁）。
- `clear_session_scopes()`（:115）：清空双集合（`NewSession` 调用，「当前会话」语义；持久化规则不随会话重置）。bridge 自锁，调用方须在 core 锁外调用（锁序红线）。
- `respond(id, choice)`（:123）：首答者赢得通道（`pending.remove` + `responder.send`），返回 `Some(summary)` 供 dispatch 推转写；迟到者拿 `None` → 定向 notice。发送失败 = decide 已放弃，应答自然丢失。
- `deny_all()`（:135）：优雅停机解栈；`front_pending()`（:143）：snapshot 用。
- **锁序红线**（模块头注释）：应答路径必须**先 `respond()` 释放 bridge 锁、再取 core 锁**推转写 `EntryDto::Approval`；严禁 bridge → core（与 snapshot 构成死锁环）。

## 4. config_ops.rs —— 配置 CRUD 写回链

- `ConfigChange`（:41）从 ClientMsg 提取（`from_msg` :58）；`apply_config_change(state, conn_id, change)`（:113 起）流程：persist 落盘 → 重装 merged config（`load_config_layered`）→ validate 兜底 → 热替换（inner.config/agent_tree/skills）→ 广播 `config_changed`。
- **路由规则**（:5-6 注释 + :113-117）：
  - provider/model 条目 → **全局库**（无全局库写 active）；
  - levels / capabilities / MCP server → **active**；
  - `.env` 派生键（`<NAME>_API_KEY`）→ 库文件所在目录（0600）。
- 松耦合语义：levels/capabilities 对模型条目是引用而非依赖——**删除被引用条目放行**（悬空合法；UI 显示"绑定缺失"、`resolve_model` 用时报 `InvalidLevelRef`、compact/title 静默回退默认档）。
- `apply_set_api_key`（:368）：**mobile 上不应走此路径**（secret 由 FFI `set_api_key` 内存注入 + Android Keystore，PLAN §18）。
- `SetCapability{capability:"main"}`：热切换会话模型 + 广播 model_changed。
- **`SetApprovalConfig`（v3，tool-approval）**：`ConfigChange::SetApprovalConfig`（config_ops.rs:78）从 ClientMsg 提取，level 字符串值域校验（manual/allow/auto，非法 → 定向 notice 拒绝不落盘）后构造 `ApprovalRules` 终态，进 `apply_approval_rules` 收敛链。
- **`apply_approval_rules(state, conn_id, new_rules)`（:643，单一收敛链，oracle P1）**——设置页 `set_approval_config` 与审批应答 `always_tool`/`always_mcp`（经 dispatch 构造规则增量）**两个入口共用**：
  1. `persist::set_approval_section` 整段重写 active 配置 `[approval]` 为新 schema（删 legacy `policy`/`tools` 键；全默认删段）；
  2. `load_config_layered` 重载 → validate 兜底 → 持 core 锁热替换 `inner.config`（含在家的 manager，config_ops 既有模式）；
  3. bridge rules cell 写入新规则（**进行中回合**的下一次 check 即可见——manager 经 `with_rules` 挂同一 cell）；
  4. 广播 `ServerMsg::ConfigChanged`（approval 视图 = 新规则）。
  失败语义与通用链一致：落盘/重载/校验失败 → 定向 notice（重载失败时磁盘已改、运行时保留旧配置）。

## 5. session.rs —— dispatch

- `dispatch(state, conn_id, msg)`（:18）：WS 读任务与 mobile FFI `send()` 共用的同一状态机。分支：重复 Hello 忽略 / Submit → `handle_submit` / **ApprovalAnswer（先 bridge 后 core，锁序）：`respond` 首答生效 → 推转写 `EntryDto::Approval`（decision = approved/denied/approve-all/always-tool/session-mcp/always-mcp 六值）+ 广播 `ApprovalResolved`（choice 连字符拼写）→ (b)/(d) `always_tool`/`always_mcp` 取 `rules_snapshot()` 构造规则增量（tools[tool]=Allow / servers[server]=Allow）经 `apply_approval_rules` 落盘 + 热更（失败仅 warn——本次调用已放行，新规则作用于下一次 check；always_mcp 遇 server 缺失的请求定向 warn「仅本次生效」）** / **QuestionReply → `SessionCore::question_handler()` 转发**（桥在 core 锁外调用；首答者赢，迟到/未知 → 定向 notice；未注入 handler → 定向 notice「提问通道未接入」）/ Cancel（**先 `QuestionHandler::on_cancel_all`（ask_user 关卷 best-effort）再有 token 则 cancel**，否则 notice）/ **NewSession（busy 守卫 → 【C1 前置冲刷】`core.transcript_flusher()` 已注入则先 `flush(state).await`（best-effort，失败在实现内 log）→ 锁段开头二次 busy 守卫（flush 窗口内 submit 可能已起回合 → 拒绝，转写清空留给下一次）→ 清空 → **终态聚合快照（cost/tokens，先于 stats 重置读取，见 §5.1）** → `state.approval.clear_session_scopes()` 清会话作用域审批集合（core 锁外调 bridge——锁序红线；持久化规则不随会话重置）→ 异步关旧 run 行 `finish_with(RunFinal{completed})` → 广播 reset → 逐连接补 snapshot）** / SetModel（可解析性校验 → 广播）/ SetApiKey / 其余 → ConfigChange 家族（含 SetApprovalConfig）。
- `handle_submit`（:165）：九步全流程（逐步注释见 [../01-architecture/dataflow.md](../01-architecture/dataflow.md) §1）。step 1 起（用户消息入 history 之前）先做 **ask_user 悬挂修复**（`ask_user::repair_dangling_tool_calls`，superseded）——新 turn 前为所有悬挂 tool call 合成结果保证消息序合法；合成消息在 step 3 先于用户消息经 recorder 落库。
- `open_session_run`（:326）：lazy 建 run；title_hint（prompt 40 字符摘要）作占位标题落库（历史列表免 N+1）；begin 失败 → None（本会话不落库照常对话）。
- `generate_session_title`（:384）：capability=title 计划（alias → resolve → factory）任一步失败静默返回（占位已就位，**降级可用永不报错**）；成功 → `update_run_title` + 广播 RunTitle；usage 计入会话累计；run 可能已被删（UPDATE 零行也成功，广播无目标自然丢弃）。

### 5.1 run 收尾时机与 RunFinal 聚合语义（v5 L2）

`RunRecorder::finish_with(RunFinal{status, output_json, cost_usd, input_tokens, output_tokens})` 单事务落 run 终态 + `runs.input_tokens/output_tokens` 聚合列（旧三参 `finish` 保留为兼容包装，token 记 0——仅测试与 mobile 旧调用仍在用；mobile 迁移归 L4）。收尾时机与各处口径：

| 时机 | 状态 | token/cost 来源 |
|------|------|----------------|
| session `NewSession`（session.rs dispatch） | `completed` | **终态快照先于 stats 重置**（同一 core 锁临界区内）：stats 自上次 /new 起累计 = 该 run 生命周期累计（含 title/compact 侧调用，与 shutdown 同口径）。重置后读恒为 0——旧代码的快照顺序 bug 已随迁移修复 |
| server 优雅停机（`graceful_shutdown_work`，openslate-server） | `interrupted` | 停机快照 `inner.stats.total_*`（同上口径；等引擎任务落地后再落库） |
| CLI `run` 子命令（cmd/run.rs） | completed/interrupted/cancelled/failed | `ManagedRunResult.total_*`（run_manager.rs:46——run 生命周期权威累计；failed/cancelled-before-start 无 result 记 0） |
| CLI REPL（EOF 退出 / `/new` / `/resume` 换 run） | completed / completed / interrupted | REPL 私有 `SessionStats` 的 **run-only 口径**：`total − compact`（`run_*_tokens()`，与 `run_cost_usd()` 同型——compact 摘要调用计会话不计 run 行）；快照同样先于 stats 重置 |
| mobile | — | **归 L4**（本 lane 不动 mobile 调用点，避免并发写冲突） |

数据流：`engine_turn` 每回合把 `ManagedRunResult` 的 tokens/cost 累计进 `inner.stats`（engine.rs Ok 分支）——这是 session 侧聚合的唯一进料口；`generate_session_title` / `run_auto_compact` 的侧调用 usage 也进 stats（计会话展示），因此 session/server 口径的 run 行聚合**含**侧调用 token（REPL 口径不含，见上表）。assistant 消息的 per-message meta（model_id/usage/finish_reason/latency_ms）不经 stats——由 runtime 捕获点经 `MessageSink::append_with_meta` 直落 `messages.meta_json`（见 [openslate-core.md](openslate-core.md) §7）。端到端契约由 `tests/runfinal_aggregation.rs`（文件库）钉死。

## 6. 锁与顺序不变量（源码注释原话，state.rs:4-15，改动前必读）

1. **`SessionCore::inner` 是 std Mutex（不是 tokio Mutex）**：`ProgressCallback` 是同步 trait，无法 await。所有回调持锁「先改镜像、再广播」→ 同一连接上事件序 == 转写追加序。
2. **锁方向唯一**：`core → sink` 与 `core → approval bridge`（持 core 锁期间允许 broadcast / 读审批队首，二者内部都是非阻塞短临界区）。**严禁 `bridge → core`**：审批应答路径必须先 `respond()` 释放 bridge 锁、再取 core 锁推转写（反向与 snapshot 构成死锁环）。
3. **`RunManager` 所有权回传**：submit 时 take，回合结束引擎任务放回（manager 非 Sync，靠 `Option` 槽位表达「在家/引擎持有」）。回家的 manager 采用**最新** agent_tree。

补充不变量（实现细节层面）：

4. `run_auto_compact` 调用方保证此刻 `compacting == true`（互斥）；history 搬出再搬回（守卫不跨 await）。
5. `handle_submit` 的 running 先置位防竞态；provider 构建失败回滚用户消息 + running=false。
6. snapshot 构造（持 core 锁）与审批应答（先 bridge 后 core）的方向约束是同一死锁 avoidance 的两面。

## 7. 测试接缝

- `ProviderFactory`（state.rs:40）：生产 = `build_provider_for_model`；集成测试注入 ScriptedProvider（与 TUI 同构）。
- config_ops 测试装配最小 AppState（active 指向 tempdir toml，无全局库 → provider/model 写 active；内存 config 与磁盘同源，config_ops.rs:552-592）。
