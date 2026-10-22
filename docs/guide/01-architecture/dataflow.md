# 01-5 · 端到端数据流

> 一条用户输入从进来到渲染完成的完整路径。分四节：会话型回合（server WS / mobile FFI 共用 openslate-session）、一次性执行（CLI）、存储流向、横切关注点（审批桥 / 取消 / 持久化时序）。

## 1. 会话型回合数据流（Submit → dispatch → engine turn → 展示）

```text
用户输入
  │  TUI: WS text frame（JSON ClientMsg）
  │  Android: MobileRuntime::send(ClientMsg JSON) → FFI
  ▼
ClientMsg::Submit { text }
  │  session::dispatch(state, conn_id, msg)   crates/openslate-session/src/session.rs:18
  ▼
handle_submit（session.rs:165）逐步：
  1. busy 守卫（running/compacting 中 → notice 拒绝；空白输入直接丢）
     持 core 锁：tool_calls_cur/depth_cur 清零、history.push(User Message)、
     transcript.push(EntryDto::User)、running=true（先置位防竞态）
  2. 标题提示：prompt 截 40 字符（按字符 CJK 安全）→ session_event! 日志
     + 作为 run.title 占位落库
  3. 惰性建 run：open_session_run（session.rs:326）
     已有 session_run → clone 复用；没有 → RunRecorder::begin
     （单事务插 runs 行 status=running + {run_id}-persist 执行节点）；
     无 store / begin 失败 → None = 本会话不落库，照常对话
  4. 用户消息落库（recorder.write_message，seq 从此连续——引擎侧追加
     走同一 recorder）
  5. 首条消息（had_run=false）→ spawn 异步 LLM 标题任务（capability=title），
     与主回合并行；成功 → update_run_title 落库 + 广播 ServerMsg::RunTitle
     （历史列表按 run_id 原位更新）；失败 → 保留 prompt 截断占位
  6. pre-turn auto-compact：needs_compact(history, max_msgs, max_bytes, 0)
     超阈值 → compacting=true → run_auto_compact（见 §4.3）→ compacting=false
  7. provider 构建：provider_factory(config, model_alias)
     失败 → 回滚用户消息（history.pop）、running=false、定向 notice 报错、
     回合作废（TUI 同款语义）
  8. take manager（Option 槽位——"在家/引擎持有"两态）；同时把本回合的
     recorder 塞进 manager.message_sink；cancel = CancellationToken 新建
     存 inner.cancel；history clone 一份交给引擎
  ▼
spawn_engine_turn（engine.rs:169）
  ★ spawn_blocking 专用线程 + Handle::block_on
  原因：审批 decide() 是同步阻塞 trait、会等客户端应答；引擎执行期间
  还有多个同步回调。占 tokio worker 会饿死同 worker 上的转发/写任务
  （实测复现过）。阻塞池线程阻塞正是其设计用途；内层 future 经
  Handle::block_on 驱动，tokio 定时器/通道/sqlx 均可用。
  ▼
RunManager::execute_with_run_id → AgentRunner::run_root → runtime::execute_run 循环:
  ├─ 组装上下文：system prompt = prompt.rs 解析（profile 体系）
  │  + skills tier-1 目录注入（SkillsCatalog::catalog_prompt）
  │  + history（prior_messages 全量传入）
  ├─ ModelProvider::generate_stream → ModelStreamEvent::{Delta,Reasoning,Usage,Done}
  │    每个事件经 ProgressCallback（= SessionProgressBridge，engine.rs:48）:
  │      持 core 锁「先改 transcript 镜像、再经 MsgSink 广播 ServerMsg」
  │      → 不变量：同一连接上事件序 == 转写追加序（锁序保证）
  │      细节：Delta/Reasoning 先进 stream 缓冲，RequestEnd/tool_start/
  │      step_end 时 flush_stream 落成条目；Usage 挂起为 pending_step_meta
  │      （落在本步工具行之下，fix-19 hold 语义）
  ├─ 模型给 tool_calls:
  │    ├─ ApprovalManager.check（每个调用都过，无绕过路径；server
  │    │    作用域经 mcp::server_slug_of 反解注入）
  │    │    待批（Manual 等级直问 / Auto 等级启发式风险 ≥Medium）→
  │    │    回调链：mobile = AskUserExemptApproval(AiReviewApproval(
  │    │    SessionApprovalBridge))，AI 审查环节见 §4.1
  │    │    决策审计 → tracing target=openslate_approval
  │    ├─ call_agent → AgentRunner 递归（CallerFrame 压栈；
  │    │    子上下文 = build_child_context 隔离构造：子代理自己的
  │    │    system prompt + 父层初始消息快照的摘要 + task）
  │    ├─ run_code → openslate-ptc QuickJS 沙箱（桥接工具调用，
  │    │    errors-as-data；run_code + 桥接调用都计 max_tool_calls）
  │    └─ 普通工具 → ToolRegistry（内置 MCP / 外部 MCP / host 注入）
  │    工具批并发执行（parallel_tool_calls 默认开；含 call_agent/
  │    run_code 的批强制串行）；顺序敏感操作（进度发射、sink 写、
  │    转写追加）严格按 tool_call 序在 join 点协调
  │    结果逐条经 MessageSink（= RunRecorder）即时落 SQLite messages 表
  │    （seq 单调）
  └─ 循环至：无 tool_calls / 限额（max_steps/max_tool_calls/
     max_child_agent_calls/timeout_ms 纯 LLM 时间预算/max_empty_turns）
     / 取消（CancellationToken 全树共享）
  ▼
回合收尾 engine_turn（engine.rs:185）持 core 锁:
  inner.running=false、cancel=None、深度/工具计数清零
  manager.agent_tree = inner.agent_tree.clone()   ← 回家的 manager 采用
    最新树（回合期间工具可见性热替换只改 inner.agent_tree）
  inner.manager = Some(manager)                    ← manager 回家
  Ok:  history 替换为 run 结果 messages、flush_stream、
       fold_tool_outcomes（回合 Tool 消息回填 live 条目的输出/失败态，
       名字 + FIFO 游标配对）、stats 累计（turns/tokens/cost）、
       广播 ServerMsg::TurnOk(TurnSummaryDto)
  Err: flush_stream、保留 history 现状（用户消息留着可重发）、
       广播 ServerMsg::TurnError
  （后续: 首 turn 的异步标题任务完成 → ServerMsg::RunTitle）
  ▼
客户端展示:
  TUI: ServerMsg → TuiEvent 渲染 ratatui 转写
  Android: EventSink 泵线程 → JSON 回调 → Compose UI 状态
  重连/新客户端: Hello{proto} 校验 → server 定向发 Snapshot
  （全量 transcript + running/深度/工具计数 + pending_approval
  + ConfigViewDto + SessionStatsDto——server 是统计真相源，重连不清零）
```

## 2. 一次性执行（CLI `openslate run` / REPL）

```text
cli::cmd::run
  → openslate_app::wiring（AppContext 装配：config → store → tree → registry → manager）
  → build_provider_for_model
  → RunRecorder::begin（run 行先建）
  → RunManager::execute_with_run_id
  → markdown / 文本输出
REPL（cli::repl）: 复用同一 manager 与 recorder，多回合共享同一 run id
```

与 session 路径的差异：无 MsgSink/转写镜像（CLI spinner 直接实现 ProgressCallback）；审批走交互 prompt；执行树/steps/trace/audit 在 run 结束后由 CLI 侧整树写入。

## 3. 存储流向

```text
runtime 每产生一条 assistant/tool 消息
  ──MessageSink──▶ RunRecorder ──▶ messages 表（seq 单调，派生主键 msg-{run_id}-{seq}）

用户消息（submit 时）──recorder.write_message──▶ 同表同 seq 序列

run 生命周期:
  begin  → runs 行（status=running，title=prompt 截断占位）+ persist 节点
  finish → status ∈ {completed, interrupted, failed, cancelled} + cost_usd
           （四项单事务落库，避免"已 completed 但成本缺失"半状态）

resume 路径: list_messages_by_run 按 seq 读回 → 解码为 core::Message
  （尾部悬空 tool_calls 的 assistant 消息被丢弃——provider 拒绝乱序对；
   recorder.rs:229-241）

执行树/steps/trace/audit: run 结束后由 CLI 侧整树写入
  （execution_nodes 的 en-{uuid} 节点 + steps + trace_events + audit_log；
   与 recorder 的 {run_id}-persist 存储节点互不干扰）
```

## 4. 横切关注点

### 4.1 审批链时序（AI 审查 + SessionApprovalBridge）

**mobile 审批回调链（外→内）**：`AskUserExemptApproval(AiReviewApproval(SessionApprovalBridge))`（mobile runtime.rs 装配；ask_user 豁免保持最外层）。`AiReviewApproval::decide()`（mobile approval_review.rs）按序：

1. **Manual 直通**（P0-2）：`trigger == Manual` 的请求直接进内层 bridge 弹人，**绝不进 AI 审查**（否则「全部手动」被审查器过滤，违背语义）；
2. **会话作用域快路径**（P0-3）：`bridge.session_allows(req)` 命中（`allowed_tools ∪ allowed_servers`，见下）→ Approved——(a)/(c) 批准过的后续调用不再付一次 LLM 审查费；
3. **verdict 缓存**：key=(tool_name, args hash)，容量 128 超限整清（safe/risky 都缓存）；
4. **LLM 审查**（审查模型 = `[capabilities].review`，未绑定**继承会话当前模型**；prompt 带工具 description/schema/参数 JSON，max_tokens=128）：先广播 info Notice「正在 AI 审查…」（20s 空窗防「卡死」观感）→ **P0-1 红线：审查 future 经 `handle.spawn` 投递 + 本线程 std mpsc `recv_timeout`（默认 20s）阻塞等待——decide 已在 `Handle::block_on` 驱动的 poll 内，再 block_on 会 panic**；safe → 静默放行（只进 tracing，不弹人、不进转写）；risky → 弹人工（note 携带「AI 审查判定高风险：…」随 `ApprovalSummaryDto` 下发）；构建/请求/解析失败/超时 → **fail-safe 弹人，绝不因审查失败默认放行**。core 锁只在快照 config/model_alias 的短临界区内持有（不跨 await/阻塞），与「先 bridge 后 core」锁序红线相容。

**SessionApprovalBridge::decide()**（session/approval.rs，引擎阻塞线程内联）：

```text
引擎线程（decide 内联调用）                 客户端任务（dispatch）
  1. 会话作用域快路径（session_allows）：    ClientMsg::ApprovalAnswer{id, choice}
     allowed_tools 含工具名 ∨                 → approval.respond(id, choice)
     allowed_servers 含 server → Approved       拿走首答权（pending.remove）
  2. sink.count()==0 → Denied（无人可问即拒绝，不永久阻塞）
  3. pending.push + 广播 ApprovalRequested      ──▶ 迟到者拿 None →
     {id, summary}（summary 含 server/            定向 notice「已由其他客户端应答」
     trigger/note，v3）          ──▶ UI 出审批卡
  4. rx.recv() 阻塞…（阻塞池线程，安全）
  5. Ok(choice) → choice_to_decision（六 choice 落点）：
     approve = 单次放行；deny = Denied；
     approve_all(a) = 写 allowed_tools（会话作用域）；
     session_mcp(c) = 写 allowed_servers（server 缺失退化工具级，防御分支）；
     always_tool(b) / always_mcp(d) = 本次放行但**不写会话集合**——
     持久化与规则热更由 dispatch 层经 config_ops::apply_approval_rules
     收敛链（persist 整段重写 [approval] → rules cell 热更 → 广播
     config_changed），新规则作用于下一次 check
  6. Err(_)（通道关闭）→ Denied("审批通道已关闭")
  7. 兜底清理 pending.retain(id)
```

- 锁序红线：应答路径**先 `respond()` 释放 bridge 锁、再取 core 锁**推转写 `EntryDto::Approval`——反向（bridge → core）会与 snapshot 构成死锁环。
- 会话作用域生命周期：`NewSession` 调 `clear_session_scopes()` 清空 `allowed_tools`/`allowed_servers` 双集合；持久化规则（always_*）不随会话重置（「一直允许」跨会话）。
- watchdog 只对 deny 计数（连续同名 Denied 中止 run），放行类 choice 不触发。
- 优雅停机：`deny_all()` 放行队列里所有阻塞请求为 deny，引擎才能解栈。

### 4.2 取消时序（CancellationToken 全树共享）

```text
客户端 Cancel → dispatch → inner.cancel.cancel()
  → root 与所有 child 层的 execute_run 在下一检查点观察到
    → 各层返回 Ok(RunStatus::Interrupted) + 部分转写
    → 未执行的 tool_calls 补合成 "[tool call cancelled before completion]"
      的 Tool 消息（append_cancelled_tool_results，runtime.rs:111——
      否则悬空 tool_calls 会被 provider 拒绝，resume 也无法继续）
    → 已追加的都过了 sink → 部分转写已在盘上，run 保持可恢复
入口前已取消 → RuntimeError::Cancelled（run_root 是真实触发点）
无进行中回合的 Cancel → 定向 notice「当前没有进行中的回合」
```

### 4.3 auto-compact 时序（engine.rs:253）

```text
submit 时 needs_compact(条数/字节超阈值, auto_compact 开)
  → history 整体搬出（std Mutex 守卫不能跨 await）
  → capability=compact → 代号（显式绑定优先；未绑定 → 继承会话当前
    模型 model_alias）→ resolve_model + provider_factory
    （别名缺失/构建失败 → None → 机械拼接兜底，core compact 内建）
  → compact(&mut history, summarize)  // LLM 摘要
  → 搬回 history；compact 的 usage 计入会话累计 stats（不进 run 行）
摘要系统提示 SUMMARY_SYSTEM_PROMPT 与 REPL/TUI 逐字一致（两前端摘要行为必须相同）
```

### 4.4 持久化时序不变量（汇总）

1. **先落盘再 continue**：assistant/tool 消息进对话即 await sink——崩溃窗口 = 当前正在生成的那条。
2. **seq 单调且 run 内唯一**（v2 唯一索引兜底 + 派生主键双保险）；用户消息与引擎消息共享同一 recorder 的 `AtomicI64 last_seq` 计数器。
3. **run 行先于一切 LLM 调用存在**（begin 单事务插 run + persist 节点）。
4. **所有终态路径都调 finish**（completed/interrupted/failed/cancelled + cost 单事务）。
5. **同一连接上事件序 == 转写追加序**（SessionProgressBridge 持锁「先改镜像、再广播」）。
6. **resume 不改 status**（避免再次中断留下僵尸 running 行；首次真正写消息才激活）。

## 5. 协议在数据流中的位置

`openslate-protocol` 是 server 与 client 之间的**唯一协议真相源**：WS 线格式 = 每 text frame 一个 JSON tagged enum（`"type"`/`"kind"`，snake_case）。mobile FFI 复用同一协议语义（上行 `send(ClientMsg JSON)`，下行 `on_event(ServerMsg JSON)`），**不发明第二套 onToken/onTool 接口**。任何字段增删改名必须同步契约测试（`crates/openslate-protocol/tests/contract.rs`）并升 `PROTOCOL_VERSION`。变体全清单见 [../02-core/openslate-protocol.md](../02-core/openslate-protocol.md)。
