# 04 · notify（Agent 主动系统通知工具）

> 设计真相源：`docs/proposals/notify-tool-design.md`（r2；拍板决策
> D1-D9 见 `.slim/deepwork/notify-tool.md`）。工具本体：
> `crates/openslate-mobile/src/notify.rs`（宿主直连 `Tool`，`AskUserTool`
> 同款模式，不进 mcp-builtin）。Android 侧（AgentNotifier /
> NotificationActionReceiver / RuntimeBridge notify 分支）见
> [06-mobile/](../06-mobile/)（Phase 2 补全）。

## 1. 定位与边界

- **主动通知**工具：Agent 在用户可能不在前台看 App 时（后台长任务、任务
  完成提醒、需要确认的分叉）把信息送达系统通知栏，并允许用户**不打开
  App** 就做简单回应（按钮 / 内联回复）。
- **双模式**：
  - **默认（非阻塞）**：fire-and-forget——工具在通知提交后立即返回；
    用户后续回应作为**新用户消息**开启新 turn（Kotlin 侧 submit 通路）。
  - **`wait:true`（阻塞，S1b）**：工具挂起，直到用户在通知上点按钮 /
    内联回复 / 划掉通知 / 超时——outcome 回填**原 tool call 的 tool
    result 并续跑**（ask_user 同款迟到等价语义）。
- 与 `ask_user` 的分工：ask_user = **App 内**结构化问卷卡；notify wait =
  **通知栏**轻量确认。description 教模型按「用户在不在 App」选工具。
- **不做二次审批**：通知低风险、用户全程可控（系统权限 + 渠道开关 +
  可滑删），`APPROVAL_EXEMPT_TOOLS` 含 notify（Manual 全审同样豁免，
  ask_user / output_query 同款逻辑）。
- PTC 可见性：默认 **Both**（沙箱内长任务编排正是进度通知典型场景）；
  bridge 路径无 tool_call 声明 → wait 不落库，仅内存通路。
- 送达语义诚实：`posted:true` 只表示**已提交系统**，不保证展示；工具
  结果与 UI 一律不用「已送达 / 已读」措辞。

## 2. 输入 schema（模型侧）

顶层无 required（cancel 请求不带 title/message）。`action`：`post`
（缺省）| `cancel`。post 形态：`id`（稳定标识 `[A-Za-z0-9_-]{1,64}`，
进度/多段更新必须复用同 id；缺省 = 自动生成一次性 id）/ `title`（必填，
≤60 字符截断）/ `message`（必填，≤2000 字符截断，超阈值 Android 侧
BigTextStyle 展开）/ `kind`（normal|important|progress，缺省 normal；
映射渠道 agent_messages / agent_important / agent_progress）/ `progress`
（0..100 越界钳制）/ `progress_indeterminate`（与 progress 互斥）/
`ongoing` / `completed`（progress 终态：去进度条去常驻）/ `auto_cancel`
（缺省 true；**未完成 progress 强制 false**）/ `public`（锁屏明文，
缺省 false）/ `wait` / `timeout`（仅 wait：秒数，`-1` 永不，缺省取
`[notify] default_wait_timeout`）/ `actions[]`（≤3；`label` 1~8 字符截断
+ `value`（缺省 =「用户在通知上点了「label」」）+ `style`
（default|positive|destructive）+ `behavior:"cancel_turn"`（**不携带
value——行为即语义，携带即业务校验拒绝**）+ `reply:true`（内联回复，
忽略 value）+ `placeholder`）。cancel 形态：`cancel_scope`（id（需同请求
id）| all）。

业务校验失败（post 缺 title/message、wait 无 action、wait 与 cancel 组合、
progress 互斥字段等）→ 错误文本回喂模型自纠（`Ok(ToolOutput{status:Error})`，
一切业务结果 ok=true 语义对齐 ask_user）。

限额（宽松档，拍板决策 7）：标题 60 / 正文 2000 / 动作 3 之上，**新 id
post 频率 12 条/分钟**（同 id 更新、wait、cancel 豁免——Rust 侧
`NotifyContext` 滑动窗闸）。活跃上限 16 与淘汰策略在**宿主侧
AgentNotifier** 实施（登记满 16 时按插入序淘汰首个非 ongoing 且非
waitPending 条目；全不可淘汰时放行，由频率限流兜底），Rust 侧不追踪。

## 3. 下发契约（host_call_requested，D9：run_id 内嵌 args）

```jsonc
// 下行（emit_raw；信封结构 {type,id,tool,args} 不变）
{"type":"host_call_requested","id":42,"tool":"notify",
 "args":{"v":1,"action":"post","wait":true,"timeout":300,
   "id":"confirm-delete","title":"确认删除","message":"要删除 3 个旧导出文件吗？",
   "kind":"important","public":false,"auto_cancel":true,
   "actions":[{"label":"删除","style":"positive","value":"用户在通知上点了「删除」"},
              {"label":"取消","style":"destructive","value":"用户在通知上点了「取消」"},
              {"label":"回复","reply":true,"placeholder":"输入给 Agent 的话"}],
   "run_id":"r-20261026-…"}}        // ← 内嵌 args（D9）；信封顶层无 run_id

// 上行 resolveHostCall(id, true, payload)——posted:false / 各 outcome 也是 ok=true
{"posted":true,"id":"confirm-delete","kind":"important","channel":"agent_important"}
{"outcome":"action","label":"删除","value":"…"}    // wait 模式后续 resolve
```

- Rust 侧做**全部归一化/校验/截断**（字符边界安全），Kotlin 收到即合法
  （仍做防御性兜底）。`run_id` 从 `session_run` 取（非 wait 也携带，
  Kotlin 路由用；无会话时省键，Kotlin 回退当前会话）。
- 非 wait：`HostCallRouter::call` 默认 120s 超时（权限查询 + post 毫秒级）。
- wait：`allocate_id + call_with_id` 自定义超时（ask_user 先例；超时取
  入参/配置，`-1` = 近似无限）。

## 4. wait 模式生命周期（S1b）

状态机（store v9 `pending_notifications` 行，D3 status 8 值）：
`pending → action | replied | dismissed | timeout | cancelled |
superseded | expired`。

| 事件 | 路径 | 模型看到 |
|------|------|----------|
| 工具发起 | **先落库**（request_id=host call id、tool_call_id、run_id、args 完整信封快照、status=pending）→ 发信封 → 自定义超时等待 | （挂起） |
| 点动作按钮（在途） | `resolveHostCall(id, true, {outcome:"action",…})` | `用户在通知上点了「label」（value）` + JSON |
| 内联回复（在途） | `{outcome:"replied","text":…}` | `用户回复：…` |
| 划掉通知 | deleteIntent → `{outcome:"dismissed"}` | 「用户划掉了通知（未作选择）。请自行判断继续…」 |
| 超时 | router 按入参超时合成 `{outcome:"timeout"}`；行 → timeout | 「用户在 N 秒内未回应通知…」 |
| App 内停止回合 | `ClientMsg::Cancel` → 引擎合成 cancelled 结果；**行收 superseded**（`NotifyHandler::on_cancel_all`） | cancelled（引擎合成通道） |
| cancel_turn 按钮 | Kotlin 先试在途 resolve（outcome=cancelled，失效 id 无害）再 `RuntimeBridge.cancel()`；**迟到则上行 notification_reply(outcome=cancelled)** → Rust 原子关卷 status='cancelled' + cancelled tool result 续跑（边角 2 决策：防御实现，文案对齐 dismissed 语义「通知被撤销」） | cancelled |
| 进程被杀 | `fail_all` 豁免集合 `[ask_user, notify]`（D7）——行留持久层等待迟到补全；重启 `assemble_state` keep-dangling + 启动清扫 | 迟到补全续跑 |
| 重启后点按钮 | 迟到路径：`ClientMsg::notification_reply`（协议 v6）→ `MobileNotifyHandler`：在途先 resolve；否则原子关卷（**首答者赢**）→ 与即时路径同一格式化函数 → `resume_from_tool_result` 续跑 | 迟到等价（模型无法区分） |

**迟到等价不变量**（ask_user r5 同款）：即时与迟到共用 `format_wait_result`，
消息历史与工具结果完全一致；迟到的时间信息只进 UI 转写。

**run_id 错配防御**（D2 列的价值落点）：迟到上行 `reply.run_id ≠ 行 run_id`
（openSession 失败回退 / 通知串台）→ mobile handler 拒绝续跑（行保持
pending，等正确路由或 TTL 兜底）。

### 启动清扫（D6：mobile 层启动路径，store 无 TTL / 无 DELETE）

`startup_notify_maintenance`（`startup_question_maintenance` 同构兄弟，
TTL 24h = `NOTIFICATION_TTL_MS`）：过期且悬挂 → `append_late_tool_result`
合成 expired 补全（先落库再进内存）；已解决（history 有结果消息）→
superseded 关卷；其余 pending 保留（悬挂者可恢复；他人 run 行不误关）。
`may_pop_dangling_tail` 的 pending 守卫集合 = pending_questions ∪
pending_notifications 的 tool_call_id 并集（D8）。

## 5. 协议 / 存储接缝

- **协议 v6**：`ClientMsg::NotificationReply {request_id, run_id, outcome,
  label?, value?, text?}`（wire `notification_reply`）。`outcome` 上行白名单
  = action/replied/dismissed/cancelled——**枚举即白名单**，timeout/expired
  仅 Rust 侧合成、wire 不可表达。session 侧 `NotifyHandler` trait
  （`set_notify_handler` 注入，`QuestionHandler` 同款接缝）；`Cancel`
  同时触发 question / notify 两 handler 的 on_cancel_all。
- **存储 v9**：`pending_notifications` 表（见
  [02-core/openslate-store-sqlite.md](../02-core/openslate-store-sqlite.md)
  §1 版本历史）。
- **配置**：`[notify] default_wait_timeout`（`AskTimeout` 形态，缺省
  **300 秒**，`-1`/`"never"` 永不；工具入参 `timeout` 逐次覆盖）。
  **本阶段仅文件配置 + 代码默认值，设置页 UI 后补**（descope 拍板；
  与 ask_user 的 App 内「永不」首装默认不同属有意差异——通知场景用户
  大概率不在屏幕前，「永不」会让 turn 无限挂起）。

## 6. 注册与白名单（D4，output_query 全套先例）

- `runtime.rs::create_with` 的 `extra_tools` 注册（AskUserTool 同位点）；
  `NotifyContext` 装配后 `wire(&state)` 注入 store / `[notify]` 配置。
- `bootstrap.rs` 三件套：`DEFAULT_ROOT_AGENT_MD` 追加 `notify`（新装默认
  可见）；旧默认 root.md 经 `LEGACY_ROOT_AGENT_MDS` 逐字节匹配迁移；
  `REQUIRED_BUILTIN_TOOLS` 补 `notify`（定制过 root.md 的存量设备启动时
  外科式补齐）。
- `hostcall.rs` `fail_all` 豁免集合 `[ask_user, notify]`（D7 最小改法；
  非 wait notify 亦被豁免属**已知良性偏差**——无落库行、宿主毫秒级完成，
  实现处注释标注防后人误修）。

## 7. 测试

- 单测（notify.rs）：归一化矩阵（缺省 / 越界钳制 / 截断字符边界 / 非法
  kind / progress 边界 0·100·101·-1 / 互斥字段 / wait 组合 / cancel_turn
  带 value 拒绝）、信封逐字段契约（含 D9 run_id）、结果格式化
  （posted / 各 posted:false 原因 / wait 全 outcome）、超时矩阵、新 id
  限流、启动清扫矩阵。
- 集成（tests/runtime_flow.rs）：`notify_roundtrip` /
  `notify_permission_denied` / `notify_wait_immediate_path_roundtrip` /
  `notify_wait_timeout` / `notify_wait_timeout_then_late_reply_noop`
  （timeout 已获胜后迟到回复原子 UPDATE 0 行 → no-op + notice）/
  `notify_wait_cancel_superseded` / `notify_wait_dismissed` /
  `notify_wait_restart_recovery_dismissed`（杀进程恢复 + 首答者赢）。
- 协议契约：`notification_reply` 逐字节样例 + outcome 白名单拒绝
  （timeout/expired 上行解析失败）。
