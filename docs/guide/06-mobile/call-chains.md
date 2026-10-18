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
| Kotlin Termux 结果（TermuxExec RESULT_TIMEOUT_MS） | **90s** | deferred 超时 → resolveHostCall(false, "timeout…") |
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
    回合可继续；运行期 host 死亡不做工具热切换——工具列表已进上下文，
    修复入口 = 设置页「重载 MCP」/ 健康态门控自动触发 reloadMcpServers）

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
expired、已解决行关卷、其余保留可恢复）→ 快照 `pending_questions` 回放
（`AskUserContext::snapshot_dtos`）→ UI 重现问卷卡。用户作答走
`{"type":"question_reply","request_id":…,"action":"accept|decline","answers":…}`
→ `MobileQuestionHandler`：在途 host call 先 resolve（即时路径）；否则原子
关卷（首答者赢）→ 与即时路径**同一格式化函数**生成结果 →
`RunManager::resume_from_tool_result` 补全原 tool call 的 tool result 并从该
点续跑（迟到等价不变量：模型无法区分答案即时返回还是迟到补全）。

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
