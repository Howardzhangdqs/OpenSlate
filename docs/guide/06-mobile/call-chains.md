# 06-mobile · 调用链时序（Rust ↔ Kotlin）

> 面向接手本项目的 AI agent。四条端到端链路：上行消息、下行事件、host call 往返、生命周期。模块细节见 [openslate-mobile.md](openslate-mobile.md) / [mobile-ffi.md](mobile-ffi.md) / [android-app.md](android-app.md)。

## 1. 上行（用户消息 → LLM）

```text
ChatScreen Composer
  → RuntimeBridge.submit(text)                          [Kotlin, 主线程可调]
      乐观本地回显 UiEntry.User（发送失败保留气泡可重发）
  → sendJson('{"type":"submit","text":…}')
  → OpenSlateRuntime.send(msg)                           [UniFFI/JNA 跨界，立即返回]
  → MobileRuntime::send (runtime.rs:177)
      ├ SetApiKey 消息 → 机制化封禁：广播 set_api_key_forbidden，不进 dispatch
      ├ Hello → 协议版本校验（不匹配广播 proto_mismatch）→ 补发 snapshot
      └ 其余 → rt.handle().clone().spawn
  → openslate_session::session::dispatch(&state, MOBILE_CONN_ID=1, msg)
  → RunManager（openslate-app/core）回合引擎
  → mobile_provider_factory → MobileSecrets 取注入 key（无则 env，再无则报错）
  → build_genai_provider → LLM（vendor/genai，流式）
```

要点：

- `submit` 是**唯一乐观回显**的消息；其余设置类消息（`upsert_provider` / `set_level` / `set_capability` / `upsert_mcp_server` / `answer_approval` / `new_session` / `cancel`…）全部转 ClientMsg JSON 等 Rust 处理后经 `config_changed` / `snapshot` 事件回证。
- FFI `send` 非阻塞：解析失败同步返回 Err（`MobileError.SendFailed`），解析成功即 spawn，处理结果全部走下行事件流。

## 2. 下行（事件 → UI）

```text
引擎各阶段产生 ServerMsg（openslate-protocol：snapshot/entry_started/delta/
  tool_call_started/…/notice/error/usage/config_changed/run_title/…）
  → EventSink::broadcast (events.rs:78)
      serde_json 序列化（失败兜底为协议 error 信封）→ std mpsc 入队
  → 泵线程 "openslate-event-pump" 逐条（天然全序，不阻塞引擎线程）
  → EventCallback.on_event(json)                          [跨 FFI，Rust 泵线程上]
  → RuntimeBridge.callback.onEvent                        [Kotlin]
      eventChannel.trySend(event)（Channel.UNLIMITED，非阻塞——回调立即返回）
  → 单消费协程（scope=Dispatchers.Default，"event pump"）
      for (event in eventChannel) { handleEvent(event) }  [JSON 解析 org.json]
  → 更新 MutableStateFlow<RuntimeUiState>
  → Compose 重组渲染
```

要点：

- 两级队列（Rust mpsc + Kotlin Channel）+ 两个单消费者（Rust 泵线程 + Kotlin pump 协程），**任何一侧都不得插入并发路径**，否则破坏全序。
- Kotlin `start()` 时先 `pumpJob?.cancel()` 再起新泵——同进程 shutdown→start 后 Channel 残留旧消费者会竞态分走事件。
- host call 信封（`host_call_requested`，见 §3）与 ServerMsg 走**同一条**事件流（`emit_raw`），宿主按 `type` 分发，命名空间不重叠。

## 3. host call 往返（模型调 Android 能力）

以模型调用 `termux_bash` 工具为例（**HostTool 兜底路径**；MCP 接管路径见 §3.1）：

```text
模型 tool call
  → 引擎在注册表命中 HostTool（termux_bash / mobile.ping / …）
  → HostTool::execute (hostcall.rs)
  → HostCallRouter::call("termux_bash", args) (hostcall.rs)
      next_id.fetch_add（AtomicU64，从 1 起）
      pending.insert(id, oneshot::Sender)
      emit {"type":"host_call_requested","id":42,"tool":"termux_bash","args":{"command":"…"}}
  → （经 §2 下行链路送达 Kotlin）
  → RuntimeBridge.handleEvent → handleHostCall
      ├ "mobile.ping"            → 自动应答 resolveHostCall(id, true, '{"pong":true,…}')
      └ "termux.run"/"termux_bash"/"bash" → TermuxExec.termux(ctx, command)
            （"bash" = 仅选 termux 后端时 HostTool 顶替名；输出按实际
              注册名配对 tool_end）
            Intent(com.termux.RUN_COMMAND) → Termux RunCommandService（background runner）
            + RUN_COMMAND_PENDING_INTENT → PendingIntent.getBroadcast(FLAG_MUTABLE)
              → 本 App TermuxResultReceiver
            → CompletableDeferred 回填（exitCode/stdout/stderr 合并）
            → OpenSlateRuntime.resolveHostCall(id.toULong(), ok, payloadJson)
  → Rust oneshot 唤醒（ok=true 时 payload 必须是合法 JSON 文本）
  → Tool::execute 返回 ToolOutput → 引擎把工具输出带上下一请求 → 模型继续
```

超时与失败语义（分层兜底，**不允许无限等**）：

| 层 | 上限 | 行为 |
|----|------|------|
| Rust `HOST_CALL_TIMEOUT`（hostcall.rs） | **120s** | 超时移除 pending，工具报错（回合可继续，不是 runtime 失败） |
| Rust termux HostTool（exec.rs `TERMUX_HOST_CALL_TIMEOUT`） | **900s** | bash/termux_bash/termux.run 的 HostTool 壳专用（`call_with_id` 覆盖，照 ask_user 先例）：停滞期间 Kotlin 侧进入等待连接模式（见 §3.1.1），给用户从容恢复 Termux 的余量，仍防永久挂死 |
| Kotlin Termux 结果（TermuxExec RESULT_TIMEOUT_MS） | **90s** | deferred 超时 → resolveHostCall(false, "timeout…")；「等待连接」模式豁免（见 §3.1.1） |
| Kotlin Termux 硬上限（TermuxExec HARD_WAIT_TIMEOUT_MS） | **880s** | 等待连接模式的全局兜底（略低于 Rust 900s，超时定性由 Kotlin 先给出） |
| `shutdown()` | — | `fail_all("runtime shutting down")`：全部在途立刻失败 + 审批全拒 |

重复/迟到应答：`resolve` 对不在途的 id 返回 false（超时已清/已答/关停），幂等安全。同回合同名工具多次调用的输出经 Kotlin `pendingToolOutputs`（工具名 → deque）按序配对。

### 3.1 termux bash 双路径（MCP 接管 + HostTool 兜底）

`termux_bash` 的执行路径由**降级阶梯**决定（MCP 优先，RUN_COMMAND 兜底）：

```text
主路径（MCP 接管，host 在线时）
  模型 tool call termux_bash
  → 注册表命中 McpTool（openslate-core mcp.rs；host 内置 BashServer
    经聚合 host 以裸名 bash 列出，App 侧按 [mcp].builtin_tool_name_template
    （默认 {prefix}_{tool}）展开注册为 termux_bash）
  → MCP over HTTP（127.0.0.1:8765，Authorization: Bearer token）
  → host 路由裸名 → BashServer 在 Termux 内执行（$PREFIX/bin/bash）
  → CallToolResult → ToolOutput（连接失败 = ExecutionError 带工具名，
    回合可继续；运行期 host 死亡经装饰器自动自愈 + 无感重试——见
    §3.1.2「MCP host 死亡自愈」，工具列表不热换、模型无感）

兜底路径（HostTool 壳，host 缺席时）
  即 §3 的 host call 往返（RUN_COMMAND Intent 逐命令桥接）
```

**接管/兜底的交接只在三个选择点发生**（统一判定谓词
`exec::mcp_owned_bash_names`：MCP 名册 ∩ builtin 模板展开名，另加
名册 ∩ bash 历史名的防误杀防线；见 openslate-mobile.md §7）：

| 选择点 | 位置 | 语义 |
|--------|------|------|
| 装配期 | `create_with`（runtime.rs） | assemble_state 注册外部 MCP 后重算：MCP 已接管的名字 HostTool/native 让位（不注册、不清理） |
| `reload_mcp` | runtime.rs | **先让位再 swap**：新名册含 bash 域名而 host bash 工具正占着 → 先卸载让位再 try_register（修掉撞名整体回滚死锁）；swap 成功后重算（接管保持 / MCP 缺席回装兜底）；回滚/全败路径让位的工具原样装回 |
| `set_exec_backends` | runtime.rs | 切后端前重算：MCP 已接管的名字不被 unregister/顶替（修误杀） |

白名单兼容：bootstrap 默认 root.md 的 `tools:` 白名单（`mobile.ping` /
`bash` / `termux_bash`）**零变动**——两条路径共用这两个名字，历史
配置免迁移；host 新增的其余裸名工具（read_file / write_file /
edit_file）对模型是 opt-in（root.md 不含即不可见）。

**展示 delta（纯展示级，可接受）**：HostTool 兜底形态的 `termux_bash`
名与 MCP 接管形态同名，Kotlin 侧 builtin 模板正则无法区分归属——
工具管理页/状态页把它分组为 MCP（`termux` 组）；PTC namespace 上 MCP
接管后从顶层工具变为 `tools.termux.bash` 组（server_alias = 前缀
termux），HostTool 兜底形态保持顶层。

### 3.1.1 termux 停滞面板 → 等待连接 → 重发续跑（HostTool 兜底路径）

Termux 未启动/被冻结/停止态时，RUN_COMMAND 的结果 PendingIntent 永不
回传——命令「停滞」。原实现是单向横幅提示（按钮只 `startActivity`，90s
后命令超时报错）；现为**活的连接面板 + 等待连接后续跑**，对 Agent 完全
透明（无论初始已连接还是后来才连上，结果都经同一链路回填）：

```text
命令停滞检测（RuntimeBridge.armTermuxStallWatchdog，首发与重发共用）
  runInTermux 派发后 0.5s（TERMUX_STALL_WATCHDOG_MS）仍 isPending
  → termuxStalledCalls += id；TermuxExec.setWaitingConnection(id, true)
      （豁免 TermuxExec 90s 计时——awaitResult 切换到 1s 轮询等待，
        不判死、不回错误；豁免解除后重起完整 90s 窗口）
  → UiState.termuxStallSince 置最早时刻 → 聊天页点亮连接面板

连接面板（ChatScreen.TermuxStallBanner + ui/TermuxStallTracker.kt）
  stall 激活期间聊天页周期重探（2.5s；tracker CAS 单飞防抖）+ ON_RESUME
  立即重探（gateRefresh 同款门控；tracker 与面板可见性解耦——Ready
  收起面板后重探继续到 stall 清零，重发后再 stall 能被重新捕获）
  → 探测阶梯复用 detectTermuxStatus（MCP host 探活 → 安装/权限 →
    echo ok 通道往返），状态-动作矩阵：
      Checking      → spinner「正在探测 Termux…」
      NotInstalled  → 「去安装」（F-Droid 详情页 intent）
      NoPermission  → 「去授权」（本应用系统权限页 intent）
      ChannelDown   → 「打开 Termux」（McpHostManager.startTermuxApp；
                      副文案带 TermuxDiagnostics.describe 摘要）
      Ready         → 面板收起（可见条件含 tracker !is Ready）

连接事件 → 重发续跑（TermuxStallTracker → RuntimeBridge.onTermuxConnectionRestored）
  仅「非 Ready → Ready」跳变触发一次，且需自上次 Ready 后观察到过
  通道中断（sawDownSinceReady）：
    未见中断（慢命令误报停滞）→ 只解除等待豁免、恢复正常计时，不重发
      （防命令重复执行；面板照常在 Ready 时收起）
    见过中断（Termux 真死过）→ 宽限 2s（TERMUX_RESEND_GRACE_MS：停止态
      期间排队的原始命令可能正在补跑，先等结果落地）→ 对 stalled 集合里
      仍 isPending 的 id 用**原 id** TermuxExec.resend：
        复用 pending 表既有 deferred（不新建 pending 项），重发即消费
        本次连接事件（id 清出 stalled 集合；每 id 每次连接事件至多一次）
        request code 按派发序号分段（0x1000_0000/段），避免与
        FLAG_ONE_SHOT 旧 PendingIntent 记录错绑
      → 解除等待豁免（恢复正常 90s 计时）+ 重武装看门狗
        （再 stall → 回到等待模式，等下一次连接事件——天然兜底
          「进程起了又死」的 Ready 误判）

结果回填（零新增链路）
  Termux RunCommandService → TermuxResultReceiver → TermuxExec.complete(id)
  → 原 deferred → runInTermux 挂起点唤醒 → recordToolOutput +
    resolveHostCall(id, ok, payload) → Rust oneshot → 工具结果带上
    下一请求，模型无感继续
  → finally：id 出 stalled 集合；集合空 → termuxStallSince=null → 面板收起
```

边界与现状：

- **MCP 路径不在此链路**：host 在线时 bash 走 MCP over HTTP；host 死亡
  由 §3.1.2 的自愈装饰器接管（连接失败签名 → 单飞恢复 → 无感重试），
  无停滞面板/重发。停滞面板只覆盖 HostTool 兜底路径（host 缺席时的
  RUN_COMMAND 桥接）。
- 跨进程重启的停滞挂起**不持久化**（二期限定）：App 进程被杀则等待中的
  命令随 deferred 消失，重启后无续跑。
- turn 取消 / runtime 关停照旧：`TermuxExec.cancelAll`（等待模式一并
  清理）+ Rust `fail_all`。

### 3.1.2 MCP host 死亡自愈（装饰器 → 单飞 host call → Kotlin 恢复阶梯 → 无感重试）

MCP host（Termux 聚合 host，HTTP 127.0.0.1:8765）中途被杀时，原行为是
`McpTool::execute` 向模型返回「call failed: Transport …」死端错误。现由
`crates/openslate-mobile/src/mcp_recovery.rs` 的 `RecoveringMcpTool` 装饰器
把这条错误变成一次自愈循环，**模型看不到死端错误，体感与 host 一直在线
一致**：

```text
错误签名分类（mcp_recovery::is_mcp_connection_failure）
  McpTool Err(ExecutionError) 文案含「call failed: Transport send error」
  /「call failed: Transport closed」= 连接层失败（host 进程死的确定性信号）
  → 进入自愈；「timed out after 300s」有意不纳入（挂死型超时与合法慢
    调用无法区分，误重启会杀掉在途合法工作）；MCP 业务失败走
    Ok(status=Error) 不经此路径（装饰器原样透传）

自愈循环（RecoveringMcpTool::execute）
  inner.execute(args)                    [旧 McpTool，连接已死，Err 且命中签名]
  → RecoveryCtx::run_recovery            [单飞闩 + 冷却，所有装饰器共享一份 ctx]
      └ HostCallRouter::call_with_id(id, "mcp.recover_host",
            {failing_tool, reason}, 900s)     [照 exec.rs / ask_user 超时先例]
  → Kotlin RuntimeBridge.handleHostCall → handleMcpRecoverHost（IO 线程，
      mcpRecoverMutex 单飞，与 Rust 侧互补）
      1. 点亮恢复面板（UiState.mcpRecovering + mcpRecoverPhase 阶梯：
         PROBE → TERMUX → HOST → RELOAD；聊天页 McpRecoverBanner，行集 =
         Termux 运行 → host 进程 → 工具注册，恢复期间沿用 TermuxStallTracker
         2.5s 周期重探范式喂「Termux 运行」行）
      2. hostAliveDetail 探活 → 活着直接跳 reload（传输层瞬时断裂场景）
      3. 不活：Termux 通道确认（probeTermux，20s 截断；通道死先
         startTermuxApp 前台拉起再复检一次）→ restartHost（kill 旧进程 →
         nohup/setsid 拉起 → ~8s 端口轮询 → wake-lock）
      4. reloadMcpServers()（既有 reloadMcpMutex 串行；热重载 = Rust 侧
         reload_mcp，新工具经 scratch 迁移时**重新包上恢复装饰器**）
      5. resolve(id, ok, {reloaded, tools} / reason)；面板收起
  → 恢复 Ok → registry.execute(同名同参) 重发一次
      [reload 后注册表里是新装饰器/新 McpTool；未换新时重入被 task-local
        标记（RETRY_INVOCATION）挡回原始错误，预算不超]
  → 结果无感回填：tool_call↔result 配对不破、审计一次执行（duration 含
    恢复等待）、模型零感知
```

状态机三要素（并行批 / 冷却 / 预算）：

| 机制 | 实现位置 | 语义 |
|------|---------|------|
| **单飞恢复闩** | `RecoveryCtx.latch`（tokio Mutex）+ `generation`（仅成功 +1） | 同一时刻至多一轮恢复在途；并行批多个 MCP 工具同时失败时后到者排队，拿到闩时代数已前进即**共享**该轮（不发自己的 host call），直接进入重试 |
| **冷却窗口** | `last_finished`（无论成败）+ 10s | 一轮完成后窗口内的后续失败不再发起 host call（`Suppressed`），但仍给一次注册表重试（上一轮成功刷新过的注册表值得再试） |
| **重试预算 = 1** | `RETRY_INVOCATION` task-local 标记 | 每次用户可见调用至多经注册表重发一次；重发期间的重入（reload 未换新装饰器）直接上抛原始错误，附「recovery attempted」指引文案 |

失败兜底：恢复 host call 被 resolve false（如 Termux 未安装 / host 未装 /
reload 失败）/ 超时 / `fail_all` → 返回**原始错误**并附一行恢复动作说明
（模型可据此换路径，而非死端）。

包装点两处（与 `SharedTool`/备份回滚语义正交）：

- 装配期：`create_with` 在 `register_bash_tools` 后对 MCP 名册整体包一层
  （`wrap_mcp_tools`，register 覆盖式原子换入，对在途调用零影响）；
- reload 期：`reload_mcp` 第 4 步 scratch 迁移时逐工具包
  （`SharedTool(Arc<RecoveringMcpTool>)`；回滚装回的旧 Arc 本就是已包装
  装饰器，不双层包装）。

边界：

- **300s 挂死型超时不自愈**（签名取舍，见上）；此类仍走原超时错误。
- **跨进程重启不持久化**：App 进程被杀则挂起的恢复/重试随 future 消失
  （host call 通道已不存在）；重启后由装配期重连自愈。
- Kotlin 侧单飞（`mcpRecoverMutex` + round 计数）：恢复中再来的请求排队
  等同一轮完成后只做轻量复检（host 在线即 resolve ok）直接 resolve，
  不重走拉起阶梯。
- 语义兼容：挂起期间 LLM 预算不计 tool 时间、turn 取消 drop future、
  host call 有 900s 超时 + `fail_all` 兜底。

### 3.2 ask_user（Agent 向用户提问，UI 全链路）

Rust 侧工具本体在 `crates/openslate-mobile/src/askuser.rs`（`AskUserTool`
+ `AskUserContext` + `MobileQuestionHandler`；设计真相源
`docs/proposals/ask-user-tool-design.md`，工具文档见
[04-tools/ask-user.md](../04-tools/ask-user.md)）；Kotlin 侧按下列契约接线
（UI 设计规范：`docs/proposals/ask-user-ui-design.md`）：

```text
模型 tool call ask_user
  → AskUserTool::execute → HostCallRouter::allocate_id + call_with_id   [Rust]
      （自定义超时：[ask_user] default_timeout，never → 信封 timeout=-1）
      （先落库问卷行 pending_questions：request_id=host call id、
        tool_call_id=消息历史中的悬挂声明，再发信封——崩溃窗口内行必在）
  → emit {"type":"host_call_requested","id":42,"tool":"ask_user",
          "args":{"v":1,"questions":[…],"timeout":-1}}
  → RuntimeBridge.handleEvent → handleHostCall → handleAskUser
      解析 questions[]（1~4 题；type=choice|text|confirm|secret，未知 type
      UI 按 text 兜底）→ UiState.pendingQuestion（requestId=id + 每题草稿 +
      argsKey=最近 running ask_user ToolCall 条目的 fullArgs 关联）
  → ChatScreen：groupEntries 对 name=="ask_user" 的 ToolCall 条目单独成条
      → QuestionCardHost（内联问卷卡 ui/QuestionCard.kt；分段条 + 题区横滑
        Pager + 全屏按钮）；PendingNudgePill（滚出视口时输入区上方提醒）
  → 全屏作答：NavHost 路由 "question-fullscreen"（竖直滑入 280ms；
      ui/QuestionFullScreen.kt；草稿挂 UiState，进出不丢）
  → 用户提交 → RuntimeBridge.submitQuestionAnswers
      resolveHostCall(id, true, {"outcome":"answered","answers":[…]})（FFI 直传；
        secret 明文仅在此 payload，不进日志/不落盘）
      + 把脱敏快照写回关联 ToolCall 条目 output（摘要卡与转写同源，重启可恢复）
      → Rust：工具格式化结果（自然语言 + 结构化 JSON）→ 行 answered
        → runner 并入 tool result → 回合继续（与普通工具结果同路径）
  → 整卡 decline → resolveHostCall(id, true, {"outcome":"declined"})
      → 条目 output 记 {"outcome":"declined"} → 终态轻摘要行
  → turn 离开 running 态（turn_ok/turn_error）→ finalizeAskUserOnTurnEnd：
      已答/已拒只清 pendingQuestion；仍 pending/submitting（用户点停止）→
      写 {"outcome":"cancelled"} 终态。迟到 resolve 对失效 id 无害（幂等首答赢）。
```

**重启恢复（迟到答案）**：进程被杀时 pending 问卷留在
`pending_questions` 表（shutdown 的 `fail_all` 对 ask_user 豁免，见 §4.3）。
重启后 `assemble_state` 用 keep-dangling 变体加载最近 run（悬挂的 ask_user
assistant 消息不丢）→ `startup_question_maintenance`（TTL 24h 过期合成
expired；已解决行关卷——判定证据是恢复 history 里确有该 tool_call_id 的
结果消息；其余保留可恢复，**含属于其它 run 的行**——恢复 history 查无其
调用不构成「被取代」证据，不误关，迟到答案可补全、TTL 过期兜底）→ 无
可恢复问卷时对尾部悬挂 assistant 执行经典 pop 丢弃，但以 store 为权威
守卫（`may_pop_dangling_tail`）：尾部声明的 tool_call 仍有 pending 行引用
则保留（补全点不可逆）；查询失败保守保留 → 快照 `pending_questions` 回放
（`AskUserContext::snapshot_dtos`）。

**Kotlin 消费端回放**（`RuntimeBridge.applySnapshot`）：

- 转写解析时顺带收集 ask_user `tool_call` 条目的 `call_id`（仅 DB 重建
  条目携带，live 恒缺省）→ `fullArgs` 关联表；
- `pending_questions[]` 取**首个** `status=="pending"` 且可解析的行，经
  `buildRestoredPendingQuestion`（internal 纯函数）重建
  `PendingQuestionUi(restored=true)`：题目解析复用 host call 信封路径的
  `parseAskUserQuestions`（DTO `questions` 即归一化题目数组，两处同构）、
  `request_id`→requestId（= 原 host call id）、argsKey 按 `tool_call_id`
  从关联表取（`QuestionCardHost` 凭 `argsKey==fullArgs` 出交互卡，查无则
  退化等待行、提交仍可用）；
- 回放**与 running 无关**（恢复场景 running 恒 false，旧代码的 running
  门控正是漏回放的断点）；live 卡在场时优先保留——UI 单卡槽，新 host
  call 到达时 `handleAskUser` 无条件覆写，恢复卡自然让位；多行 pending
  （同 turn 并发提问）取最早一张，兄弟悬挂由 Rust 下一 turn 前的
  superseded 修复收口。

**恢复卡的提交路径**（`submitQuestionAnswers` / `declineQuestionCard` 的
`restored` 分支）：不走 `resolveHostCall`（进程重启后 host call 通道已不
存在，id 打到失效信道），改 `send` 协议上行
`{"type":"question_reply","request_id":"<id>","action":"accept|decline",
"answers":…}`（accept 的 answers 与 live resolve payload 的 answers 数组
同构，Rust `reply_payload` 再包 outcome 外壳；decline 负载 null）。发送
成功即写脱敏转写 output 并出卡（恢复态无 live turn 锚定
submitting→answered 相位，续跑的 turn 事件走常规链路；迟到被拒时 Rust
广播 notice）；发送失败回滚 pending 允许重试。同会话 live 卡仍走
resolveHostCall，行为不变。新 turn（`request_start`）出清仍 pending 的
恢复卡并记 cancelled 终态——Rust 悬挂修复已将其 superseded，且重启后
host call id 从 1 复位、恢复卡的数字 request_id 可能撞上新在途调用
（`MobileQuestionHandler` 先按 id resolve 在途通路，出清防误投）。

**转写恢复仲裁**：`tryRestoreLocalTranscript` 弃用「条目总数多者胜」——
本地文件只落盘到上个完成 turn，多出的装饰条目（⚡meta/step）会让旧文件
总数压过含中断 turn 条目的 DB 权威镜像（悬挂的 user / ask_user 条目被
抹掉）。新规则 `shouldRestoreLocalTranscript`（internal 纯函数）：本地在
durable 条目（user/assistant/reasoning/tool/delegate/approval，两侧同源
持久化）上不落后且总数更多才覆盖；DB 转写为空时本地兜底。live 会话的
落盘逻辑（turn 结束写文件）不变。

**Rust 侧迟到处理**：`MobileQuestionHandler` 双通路——在途 host call 先
resolve（即时路径）；否则原子关卷（首答者赢）→ 与即时路径**同一格式化
函数**生成结果 → `RunManager::resume_from_tool_result` 补全原 tool call
的 tool result 并从该点续跑（迟到等价不变量：模型无法区分答案即时返回
还是迟到补全）。

**设置同步**（默认超时 / 输出脱敏）：设置页「内置工具」变更 →
`RuntimeBridge.pushAskUserConfig` 发
`{"type":"set_ask_user_config","default_timeout":"never"|{"seconds":N},
"mask_secrets":bool}` → session config_ops 写 active openslate.toml
`[ask_user]` 落盘 + 热替换 + 广播 config_changed；启动时
（`RuntimeBridge.start` → `syncAskUserConfig`）推送一次本地偏好做同步
（toml 为生效真值，本地 SharedPreferences 仅 UI 显示源）。
`AskUserContext` 的配置快照随装配固定——新值重启后生效（见
[04-tools/ask-user.md](../04-tools/ask-user.md) §8.5）。

resolve payload 的 answers[] 每题形态：`{id,selected:[label…]}`（choice，
「其他」说明文字并入 selected 尾部）｜`{id,value}`（text）｜
`{id,confirm:bool,selected:[label]}`（confirm）｜`{id,value,remember}`（secret）｜
`{id,skipped:true}`（optional 跳过）｜`{id,declined:true}`（逐题不答）。
转写 output JSON 同形但带 `header` 字段且 secret 只留引用
`{secret:{name,kind,remember}}`。Rust 侧对 payload 形状宽容（selected/value/
skipped/declined 逐题识别，secret 明文入会话凭据表后只回填引用）。

## 4. 生命周期

### 4.1 启动

```text
MainActivity.onCreate
  → AgentService.start(ctx)（startForegroundService）
  → AgentService.onCreate
      startForegroundWithNotification（API 34+ 走 SPECIAL_USE 重载）
      lifecycleScope.launch { RuntimeBridge.start(this@AgentService) }
  → RuntimeBridge.start（startMutex 防重入；Dispatchers.IO——装配可能阻塞数秒）
      1. 组 MobilePathsDto：filesDir/openslate | filesDir/workspace | filesDir/data | cacheDir/openslate
      2. McpHostManager.token(ctx) → uniffi setMcpHostToken(token)   【必须在 create 前】
      3. OpenSlateRuntime.create(paths, callback)
           → MobileRuntime::create_with 装配链（见 openslate-mobile.md §3.2）
              panic hook / ensure_layout / realign DB / EventSink / HostCallRouter /
              tools / RegistryStore / tokio(2 workers) / assemble_state（含启动清扫 +
              重启续聊）/ register_bash_tools / emit_snapshot（create 即握手）
      4. SecretStore.allProviders → 逐个 load → runtime.setApiKey
           （Ok=注入；Failed=告警「需重新录入」；Missing=跳过）
      5. restoreHttpProxy() / restoreExecBackend()
      6. runtime.registryScheduleAutoUpdate()（后台按需）
      7. 起新事件消费协程（pumpJob）
```

失败路径：create 抛 `MobileError.CreateFailed` → `_state.lastError` 记录、`started=false`（下次 start 可重试）。

### 4.2 运行中

App 退到后台：前台服务存活，runtime 与事件流不受影响（POST_NOTIFICATIONS + FOREGROUND_SERVICE_SPECIAL_USE）。

### 4.3 关停

```text
AgentService.onDestroy
  → runBlocking { withTimeoutOrNull(2_500) { RuntimeBridge.shutdown() } }
      【宁可短暂阻塞主线程也要同步等——lifecycleScope 协程会被 DESTROYED
        竞态截断，Rust 收尾做一半 = run 僵尸 + SQLite 撕裂】
  → OpenSlateRuntime.shutdown() → MobileRuntime::shutdown（幂等）
      1. host_router.fail_all（在途 host call 全失败；**ask_user 豁免**——
         在途问卷不 fail，留给持久化恢复，重启后经 question_reply 迟到补全）
      2. approval.deny_all（审批全拒）
      3. sink.close_all（丢 sender；泵线程 drain 完余量退出，已入队事件仍送达）
      4. run.recorder.finish("interrupted", None, cost)   【先于 runtime 收束落库】
      5. rt.shutdown_timeout(5s)（有限等待，残余任务随 drop 强制终止）
```

下次启动的兜底：若上次仍留下 running 的 run（进程被杀没走 onDestroy），`assemble_state` 的 `mark_running_runs_interrupted` 统一清扫为 interrupted。

## 5. MCP host 相关时序（补充）

设置页「自动化操作」启用流程（McpHostManager）：

```text
checkTermux（探测安装/权限）→ installHost：
  assets/mcp-host-aarch64 → /sdcard/Download 中转（或 /dev/tcp/127.0.0.1 直推）
  → Termux ~/.openslate/mcp-host（chmod +x，字节数校验）
  → base64 内嵌推送 mcp-host.toml（token + 下游 server 列表）
  → kill 旧进程 → nohup/setsid 后台启动
  → 探活 http://127.0.0.1:8765/mcp（HTTP initialize）
Rust 侧装配期经 core::mcp 连接该地址，Authorization: Bearer <token>
（token 经 setMcpHostToken 进程级注入，不落 openslate.toml）
```

改 mcp-host 二进制 → 重跑 `scripts/build-mcp-host.sh` + 重编 APK（assets 重新打包）。
