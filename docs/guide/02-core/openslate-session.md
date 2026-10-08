# 02-3 · openslate-session

> 传输无关共享单会话核心（`crates/openslate-session/src/`，5 个模块）。从 openslate-server 抽取，WS server（ConnectionHub 扇出）与 mobile（EventSink → JSON 回调宿主）共用同一份事件序与转写镜像语义。依赖 core/app/protocol/store-sqlite，**不依赖任何传输**。

## 模块结构

| 文件 | 职责 | 核心符号 |
|------|------|----------|
| `state.rs`（472 行） | 共享状态 + 事件出口 + 转写镜像 | `SessionCore`/`CoreInner`/`MsgSink`/`AppState`/`ConfigPaths`/`SessionRun`/视图与镜像辅助函数 |
| `engine.rs`（530 行） | 引擎回合任务 | `SessionProgressBridge`/`spawn_engine_turn`/`engine_turn`/`run_auto_compact`/`generate_title` |
| `approval.rs`（164 行） | 会话审批桥 | `SessionApprovalBridge` |
| `config_ops.rs`（835 行） | 配置 CRUD 写回链 | `ConfigChange`/`apply_config_change`/`apply_set_api_key` |
| `session.rs`（449 行） | ClientMsg 派发 | `dispatch`/`handle_submit`/`open_session_run`/`generate_session_title` |

运行时事件：`EVENT_TARGET = "openslate_session"` + `session_event!` 宏——单行英文、grep 友好、**token/api key 绝不进**。

---

## 1. state.rs

### 类型定义

```rust
// :133
pub struct SessionCore { inner: Mutex<CoreInner> }   // ★ std Mutex（见锁序不变量）
// :93
pub struct CoreInner {
    pub session_id: String,           // new_session_id() = UUID v4 simple
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

### 视图构造（纯函数）

- `effective_limits(config) -> LimitsConfig`（:170）：`limits.clone().unwrap_or_default()`——Option 语义解掉。
- `build_config_view(inner) -> ConfigViewDto`（:175）：capabilities 出**生效值**（并入缺省回退——客户端拿到的即真相，与 limits 的"server 解析默认值"同一先例）。
- `build_snapshot(inner, approval) -> SnapshotDto`（:215）：纯投影 + 审批桥队首待审。

### 转写镜像辅助（与 TUI transcript.rs 行为 1:1）

| 函数 | 行为 |
|------|------|
| `args_preview(args)`（:244） | trim + 去引号，≤40 列截断加 `…`（UTF-8 边界安全） |
| `cap_stored_text(s, cap)`（:258） | 字节预算截断，标记 `\n…（已截断）`；`ARGS_STORE_CAP = 4KB`（:271）/ `OUTPUT_STORE_CAP = 8KB`（:273） |
| `delegate_target(args)`（:277） | 从 call_agent 参数取子 agent id（agent_id/agent/child 三键尝试，失败回退 "call_agent"） |
| `tool_failure_summary(content)`（:290） | 5 个行首失败标记锚定：`Error:` / `approval denied tool` / `child agent call denied` / `[tool call cancelled` / `[error]`；首行 ≤60 字符摘要 |
| `flush_stream(inner)`（:321） | 冲刷 answer/reasoning 缓冲为条目（reasoning 先于 answer；空白丢弃）+ 冲 pending_step_meta |
| `tool_start_entry(inner, name, args)`（:344） | call_agent 走 `Delegate{agent, done:false}` + 深度/计数增；其余 push `ToolCall{status: Running, detail.args=capped}` + tool_timers 计时 |
| `tool_end_entry(inner, name, bytes, truncated)`（:372） | 最后一条同名 Running → Done{elapsed}；call_agent 额外 FIFO 完成最老未完成 Delegate |
| `fold_tool_outcomes(inner, messages)`（:409） | 回合 Tool 消息折叠：live 条目无 call_id → **名字 + FIFO 游标**配对；成功精化保住 live elapsed；失败标记翻转 Failed；输出全文留存 8KB；孤儿结果 debug 日志丢弃 |
| `approval_summary(req)`（:470） | 转接 protocol 的 From |

## 2. engine.rs

### SessionProgressBridge（:44，impl ProgressCallback）

- 全部回调持 core 锁「**先改镜像、再广播**」→ 同一连接上事件序 == 转写追加序。
- 事件映射：`on_request_start` 冲掉上一请求挂起 usage 行 + RequestStart；`on_delta`/`on_reasoning` 进 stream 缓冲 + 广播；`on_usage` 挂起 `pending_step_meta`（Meta 行落在本步工具行之下，fix-19 hold）；`on_tool_start/end` → `tool_start_entry`/`tool_end_entry` + 广播（preview 空串转 None）；`on_step_end` 冲流 + 连续边界折叠为一个 StepBreak。
- `usage_meta_line`（:149）：`↑in ↓out [⎓cached]`（TUI 的 ttft/速率装饰是客户端渲染层，不进协议数据）。

### spawn_engine_turn（:169）与 engine_turn（:185）

- **spawn_blocking 专用线程 + `Handle::block_on`**：审批 `decide()` 是同步阻塞 trait、会等客户端应答；引擎执行期间还有多个同步回调。占 tokio worker 会饿死同 worker 上的转发/写任务（**实测复现过**）。阻塞池线程阻塞正是其设计用途；内层 future 经 Handle::block_on 驱动，tokio 定时器/通道/sqlx 均可用。
- 收尾（持 core 锁）：`running=false`、`cancel=None`、深度/工具计数清零；**回家的 manager 采用最新 agent_tree**（`manager.agent_tree = inner.agent_tree.clone()`，:218——回合期间工具可见性热替换只改 inner.agent_tree，碰不到被 take 的 manager）；`inner.manager = Some(manager)`。
- Ok：history 替换为 run 结果、flush、fold_tool_outcomes、stats 累计、广播 TurnOk。Err：flush、**保留 history 现状**（用户消息留着可重发）、广播 TurnError。

### run_auto_compact（:253）

pre-turn 摘要。history 整体搬出（std Mutex 守卫不能跨 await）→ `needs_compact` 判定 → capability=compact 计划（resolve_model + provider_factory；任一失败 → None → 机械兜底）→ `compact()`（空白回复视作失败 → 机械兜底）→ 搬回。**compact 的 usage 计入会话累计（不计 run 行）**。`SUMMARY_SYSTEM_PROMPT`（:29）与 REPL/TUI **逐字一致**。

### 会话标题（:379-441）

- `TITLE_INPUT_MAX_CHARS = 400`（:383，输入截断）/ `TITLE_MAX_CHARS = 24`（:387，清洗上限——提示要求 ≤16 字符，留裕量兜啰嗦模型）。
- `generate_title(provider, model_id, first_prompt)`（:392）：`max_tokens: Some(48)` 小请求；失败/空白 → None（占位标题已在库）。
- `sanitize_title`（:427）：首行 → 去包裹引号/书名号/反引号（中英文都剥）→ 按字符截断；空/纯标点 → None。

## 3. approval.rs —— SessionApprovalBridge

（时序图见 [../01-architecture/dataflow.md](../01-architecture/dataflow.md) §4.1。）

```rust
// :42
pub struct SessionApprovalBridge {
    sink: Arc<dyn MsgSink>,
    next_id: AtomicU64,                  // 从 1 起
    inner: Mutex<BridgeInner>,           // {pending: Vec<Pending>, allowlist: HashSet<String>}
}
```

- `decide()`（:107 impl ApprovalCallback）：白名单快路径 → `sink.count()==0` 即 Denied（**无人可问即拒绝**，不永久阻塞）→ 注册 Pending + 广播 ApprovalRequested → **阻塞在 std mpsc recv**（多线程 runtime worker 上阻塞是安全模式——TUI 模块文档论证过，session 沿用）→ 决策 + 兜底清理。
- `respond(id, choice)`（:59）：首答者赢得通道（`pending.remove` + `responder.send`）；迟到者拿 `None` → dispatch 定向 notice。发送失败 = decide 已放弃，应答自然丢失。
- `deny_all()`（:71）：优雅停机解栈。
- `front_pending()`（:79）：snapshot 用。
- **锁序红线**（:30-32 注释）：应答路径必须**先 `respond()` 释放 bridge 锁、再取 core 锁**推转写 `EntryDto::Approval`；严禁 bridge → core（与 snapshot 构成死锁环）。

## 4. config_ops.rs —— 配置 CRUD 写回链

- `ConfigChange`（:41）从 ClientMsg 提取（`from_msg` :58）；`apply_config_change(state, conn_id, change)`（:113 起）流程：persist 落盘 → 重装 merged config（`load_config_layered`）→ validate 兜底 → 热替换（inner.config/agent_tree/skills）→ 广播 `config_changed`。
- **路由规则**（:5-6 注释 + :113-117）：
  - provider/model 条目 → **全局库**（无全局库写 active）；
  - levels / capabilities / MCP server → **active**；
  - `.env` 派生键（`<NAME>_API_KEY`）→ 库文件所在目录（0600）。
- 松耦合语义：levels/capabilities 对模型条目是引用而非依赖——**删除被引用条目放行**（悬空合法；UI 显示"绑定缺失"、`resolve_model` 用时报 `InvalidLevelRef`、compact/title 静默回退默认档）。
- `apply_set_api_key`（:368）：**mobile 上不应走此路径**（secret 由 FFI `set_api_key` 内存注入 + Android Keystore，PLAN §18）。
- `SetCapability{capability:"main"}`：热切换会话模型 + 广播 model_changed。

## 5. session.rs —— dispatch

- `dispatch(state, conn_id, msg)`（:18）：WS 读任务与 mobile FFI `send()` 共用的同一状态机。分支：重复 Hello 忽略 / Submit → `handle_submit` / ApprovalAnswer（**先 bridge 后 core**，锁序）/ Cancel（有 token 则 cancel，否则 notice）/ NewSession（busy 守卫 → 清空 → 异步关旧 run 行 `finish("completed")` → 广播 reset → 逐连接补 snapshot）/ SetModel（可解析性校验 → 广播）/ SetApiKey / 其余 → ConfigChange 家族。
- `handle_submit`（:165）：九步全流程（逐步注释见 [../01-architecture/dataflow.md](../01-architecture/dataflow.md) §1）。
- `open_session_run`（:326）：lazy 建 run；title_hint（prompt 40 字符摘要）作占位标题落库（历史列表免 N+1）；begin 失败 → None（本会话不落库照常对话）。
- `generate_session_title`（:384）：capability=title 计划（alias → resolve → factory）任一步失败静默返回（占位已就位，**降级可用永不报错**）；成功 → `update_run_title` + 广播 RunTitle；usage 计入会话累计；run 可能已被删（UPDATE 零行也成功，广播无目标自然丢弃）。

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
