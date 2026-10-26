# 设计提案：Agent 主动通知工具（`notify`）

> 状态：已评审拍板（design-only，未实施；决策记录见 §10，2026-10-20 用户拍板 8 项）
> 依据：《Android Agent 消息与系统通知接口技术调研》（用户提供的调研报告，2026-10）、本项目现状摸底（host call 通道、`ask_user` 先例、AgentService FGS 通知、Manifest 权限）。注记：该外部调研报告为**用户提供的未入库材料**，仓库内无法溯源核对；本文以「报告/调研报告结论」指称，引用结论尽可能以官方公开 API 文档与真机行为交叉验证。
> 约束（用户拍板）：**现阶段只实现 Android 通用通知类型**（NotificationCompat 公开 API），**不做厂商适配**（小米超级岛 / OPPO 流体云 / vivo 原子通知另行立项）；兼容 API 26～37；**移动端优先**。
> 姊妹先例：`docs/proposals/ask-user-tool-design.md`（宿主直连 Tool + host call 模式的第一个完整实现；本设计大量复用其机制）。
> 修订：r2（2026-10-26）依代码核对修正——store 落位 v5→v9（现网 `SCHEMA_VERSION=8`，v5 已被 multimodal 版本占用）；白名单先例更正为 `output_query` 全套（DEFAULT + LEGACY + `REQUIRED_BUILTIN_TOOLS`，bootstrap.rs:30-85/:308），注明 `ask_user` 不在任何白名单；`pending_notifications` 字段拍板（`run_id` 为新增列、不设 `version` 列）；TTL 清扫明确在 mobile 层启动路径（store 无 TTL / 无 DELETE）；`fail_all` 豁免改为工具名集合 `[ask_user, notify]`（不引入按调用注册）；刷新行号（`handleHostCall` :2648、信封分发 :2086、submit :766、mobile.ping 注册 :175-184、`DEFAULT_ROOT_AGENT_MD` :30-41）；AgentService 前台化描述更正为单函数 `SDK_INT` 分支（AgentService.kt:129-155）；注明外部调研报告未入库；新增与 PLAN-phase1 §17（读通知）的互指消歧（§0）。
> 修订 r2 delta（同日，实施计划评审后）：下行信封 `run_id` 改**内嵌 args**（§3 示例同步；信封顶层保持 `{type,id,tool,args}` 不改 HostCallRouter）；`run_id` 列理由修正为纵深防御 + SQL 可查性（迟到补全不依赖它，§4.2 表后注）；§4.3「cancel 关闭 wait 行」补定防御性机制（关卷 `cancelled`）；测试矩阵补 timeout-竞态 no-op / cancel 防御 / restart-dismissed 三条（§8.1）；`[notify] default_wait_timeout`、`[ask_user] companion_notify_seconds` 设置页 UI descope（S1 仅文件配置 + 默认值）；§7.2 mask_secrets S1 仅文档化不接线。

---

## 0. 定位与边界

- `notify` 是**主动通知**工具：Agent 在用户可能不在前台看 App 时，把「任务完成 / 需要确认 / 进行中进度 / 重要结果」以系统通知形式送达通知栏，并允许用户**不打开 App** 就做出简单回应（按钮 / 内联回复）。
- **双模式**（已拍板）：
  - **默认（非阻塞）**：fire-and-forget——工具在通知提交后立即返回；用户的后续回应作为**新用户消息**开启新 turn，不属于原 tool call。
  - **`wait:true`（阻塞）**：工具挂起，直到用户在通知上点按钮 / 内联回复 / 划掉通知 / 超时——答案回填**原 tool call 的 tool result 并续跑**（与 ask_user 同款迟到等价语义）。
  - 与 `ask_user` 的分工：ask_user = **App 内**结构化问卷卡（用户在 App 里，多题型、批量、secret）；notify wait = **通知栏**轻量确认（用户大概率不在 App，按钮 + 短回复）。两者互补不替代；description 教模型按「用户在不在 App」选工具。
- **不做二次审批**：通知是低风险、用户全程可控（系统权限 + 渠道开关 + 可滑删）的能力，不挂 `[approval]` 默认门（对齐 ask_user 豁免的同一逻辑）。
- PTC 可见性：**Both**（默认）——沙箱内长任务编排（QuickJS）正是进度通知的典型场景。
- 送达语义诚实（沉淀调研报告结论）：`posted:true` 只表示**已提交系统**，不保证展示（渠道被关 / 勿扰模式 / 横幅由系统决定）、更不表示用户已读。工具结果文案与 UI 一律不用「已送达 / 已读」措辞。
- 非目标（本期不做，防范围蔓延）：
  - 厂商实时活动（超级岛 / 流体云 / 原子通知）——报告已论证均需业务准入与模板审核，属独立立项；
  - Android 16 `ProgressStyle` / Live Updates（`POST_PROMOTED_NOTIFICATIONS`）——通用 API 但准入条件复杂，放 S3；
  - 全屏 Intent（系统严格限定通话/闹钟场景）、气泡 Bubbles、`MessagingStyle` 伪装对话、`MediaStyle`/`CallStyle`；
  - 通知声音/振动自定义——跟渠道走，由用户系统设置控制；
  - 替用户向第三方应用发送内容（`ACTION_SEND` 分享、短信预填）——语义是「发送」不是「通知」，属另一个未来工具（如 `share_content`），不复用本工具；
  - 前台服务常驻通知（AgentService 1001）的改造——不动。
- **与 PLAN-phase1 §17 的分工（消歧）**：该节的 `android.notification.*`（`NotificationListenerService`）是**读通知**——把第三方 App 的通知内容取给 Agent；本提案是**发通知**——Agent 主动向系统通知栏投递并接收回应。方向相反、互不覆盖；读通知不在本提案范围（PLAN-phase1 §17 已互指至此）。

## 1. 通知类型覆盖矩阵

| # | 类型 / 场景 | 实现形态 | 阶段 |
|---|------------|----------|------|
| 1 | 普通消息通知（任务完成、结果摘要） | `NotificationCompat` + `agent_messages` 渠道（DEFAULT），`BigTextStyle` 长文自动展开 | S1 |
| 2 | 重要提醒（需要用户尽快处理） | `agent_important` 渠道（HIGH，具备 heads-up 资格——**是否横幅由系统决定**） | S1 |
| 3 | 长文本结果 | 正文超阈值自动 `BigTextStyle.bigText`（展开态全文） | S1 |
| 4 | 确定进度（导出/同步/批量执行） | `setProgress(100, v, false)` + `agent_progress` 渠道（LOW）+ `ongoing` + `setOnlyAlertOnce` | S1 |
| 5 | 不定进度（耗时未知） | `setProgress(0, 0, true)` 转圈 | S1 |
| 6 | 进度终态（完成/失败） | `completed:true` → 去进度条、去常驻、恢复点击即消 | S1 |
| 7 | 操作按钮（确认/取消/重试/停止，≤3） | `NotificationCompat.Action` → 回流为用户消息 / 即时停止回合（§4.4） | S1 |
| 8 | 内联回复（RemoteInput 直接输入给 Agent） | `RemoteInput` + `FLAG_MUTABLE` 显式组件 PendingIntent | S1 |
| 9 | 更新既有通知（同 id 覆盖，不产生新条目） | 同 tag/id 重复 `notify()`（Android 原生替换语义） | S1 |
| 10 | 撤销通知（单条 / 本进程全部） | `cancel(tag, id)` 枚举法（§5.5，**禁 `cancelAll()`**） | S1 |
| 11 | 常驻进行中状态 | `setOngoing(true)` + turn 终态自动收敛（§4.6 防僵尸常驻） | S1 |
| 12 | 点击通知打开会话 | `contentIntent` → MainActivity + runId 路由（§4.5） | S1 |
| 13 | 锁屏隐私 | 默认隐藏内容（PRIVATE）；`public:true` 可选公开（已拍板） | S1 |
| 14 | **阻塞等待用户在通知上的决定**（`wait:true`） | 挂起 + 动作/回复/划掉/超时回填原 tool call（§4.2） | S1b |
| 15 | 跨会话回复路由（runId 对齐） | PendingIntent 带 runId，receiver 路由到原会话（已拍板进 S1） | S1 |
| 16 | ask_user 伴生唤回通知（问卷久未答→通知唤回） | 宿主行为（非模型调用），Kotlin timer 驱动（§4.8，已拍板进 S1） | S1 |
| 17 | 多条目摘要（InboxStyle 列表） | `InboxStyle` | S2 |
| 18 | 重启后遗留 ongoing 通知清理 | 发通知时落 tag 清单（磁盘），启动时逐个 cancel | S2 |
| 19 | 回复消息的转写来源标记（「来自通知」badge） | UiEntry.User 元数据 | S2 |
| 20 | 原生 `ProgressStyle` + Live Updates 提升（API 36+） | `setRequestPromotedOngoing` 等，准入条件全套检测 | S3 |
| 21 | 通知分组（group summary，多条聚合） | `setGroup`/`setGroupSummary` | S3 |
| 22 | 厂商实时活动（超级岛/流体云/原子通知） | 报告 §三 的授权接入路径 | 另行立项 |

## 2. 工具 Schema

工具名 **`notify`**（已拍板）。宿主直连 `Tool` trait 实现（`ask_user` 同款模式，**不进 mcp-builtin**）：`crates/openslate-mobile/src/notify.rs` 新模块 `NotifyTool`，内部持 `Arc<HostCallRouter>`，装配期在 `runtime.rs` `create_with` 第 6 步与 `mobile.ping` 同位点注册（`crates/openslate-mobile/src/runtime.rs:175-184` 先例）。

### 输入（模型侧，手写 JSON Schema，`additionalProperties:false`）

```jsonc
{
  "action": "post",            // post | cancel；缺省 post
  "id": "export-photos",       // 稳定标识，[A-Za-z0-9_-]{1,64}；进度/多段更新必须复用同 id；
                                // 缺省 = 自动生成一次性 id（等价「发完即弃」）
  "title": "照片导出",          // ≤60 字符，超出截断；post 必填
  "message": "已导出 128 张到 …", // ≤2000 字符，超出截断；post 必填；超阈值自动展开样式
  "kind": "normal",            // normal | important | progress；缺省 normal；映射渠道见 §5.2
  "progress": 40,              // 仅 progress：0..100 确定进度
  "progress_indeterminate": true, // 仅 progress：不定进度（与 progress 互斥，同时给报错）
  "ongoing": true,             // 常驻不可滑删；进行中任务置 true
  "completed": false,          // progress 终态：去进度条/去常驻/恢复点击即消
  "auto_cancel": true,         // 点击即消；缺省 true；未完成 progress 强制 false
  "public": false,             // 锁屏直接显示内容；缺省 false（锁屏只见「有新消息」）
  "wait": false,               // true = 阻塞等待用户在通知上的回应（回填本 tool call，§4.2）
  "timeout": 300,              // 仅 wait：等待秒数；-1 = 永不；缺省取 [notify] default_wait_timeout
  "cancel_scope": "id",        // action=cancel 时：id（需同请求 id 字段）| all（本进程全部 agent 通知）
  "actions": [                 // ≤3 个；点击不打开 App；wait 模式至少 1 个（否则无法回应）
    { "label": "确认",          // 1~8 字符
      "value": "用户在通知上点了「确认」", // 点击后作为用户消息发送的文本（非 wait）/
                                       // 回填 outcome 的 value（wait）；缺省 = "用户在通知上点了「<label>」"
      "style": "positive" },    // default | positive | destructive（仅 UI 语义）
    { "label": "停止任务", "behavior": "cancel_turn" },
                                // cancel_turn = 立即停止当前回合（RuntimeBridge.cancel，§4.4）；
                                // 不携带 value（行为即语义）
    { "label": "回复", "reply": true, "placeholder": "输入给 Agent 的话" }
                                // reply:true = 内联回复输入框（RemoteInput）；此时忽略 value
  ]
}
```

顶层无 `required`（cancel 请求不带 title/message）；`post` 缺 title/message 属**业务校验错误** → `Ok(ToolOutput{status:Error})` 错误文本回喂模型自纠（对齐 ask_user / adding-a-tool.md 的错误约定：业务失败 `Ok(error_text)`，结构性非法 `Err(McpError::invalid_params)` —— 本工具走 core `Tool` trait，全部业务化）。`wait:true` 且 `actions` 为空、`wait:true` 与 `action:"cancel"` 组合等同样业务校验拒绝。

### 工具 description 文案要点（教模型用好）

- 通知用于**用户可能不在前台**的场景（后台长任务、任务完成提醒、需要用户确认）；用户正在聊天里看回复时不要发——App 内提问用 `ask_user`。
- `wait:true` 用于**必须等用户拍板才能继续、且用户大概率不在 App** 的分叉；用户在 App 内时直接 `ask_user` 或在回复里问。
- 长任务先发 `kind:"progress"` + `id` + `ongoing:true`，后续用**同一 id** 更新进度，结束时 `completed:true`（或换发一条 normal 结果通知并 cancel 原 id）——**绝不为每次进度变化换新 id**。
- `important` 慎用：有声、可能横幅打断用户；仅用于真正需要尽快处理的事。
- `actions` 让用户不打开 App 就能回应；需要自由文本用 `reply:true`；想允许用户一键中止长任务用 `behavior:"cancel_turn"`。
- `public:true` 仅用于无敏感内容的展示型场景——通知在锁屏明文可见，也可能被朗读/投屏。
- 不要通知轰炸：同类事件更新同一条（同 id），新通知有频率限额。
- 通知内容**不得包含密钥、凭据、敏感明文**。

### 输出（回填模型：自然语言 + 结构化并存）

非 wait 模式（立即返回）：

```
Notification posted (id=export-photos, kind=progress, channel=agent_progress).
{"posted":true,"id":"export-photos","kind":"progress","channel":"agent_progress"}
```

| 结果 | payload | 模型看到 |
|------|---------|----------|
| 提交成功 | `{"posted":true,"id","kind","channel"}` | posted 文案 |
| 权限未授予 | `{"posted":false,"reason":"permission_denied"}` | 「通知未展示：用户未授予通知权限。请改在回复中直接说明，或请用户到系统设置开启 OpenSlate 通知。」 |
| 渠道被用户关闭 | `{"posted":false,"reason":"channel_disabled","channel":…}` | 同上风格，提示对应渠道名 |
| 限流 | `{"posted":false,"reason":"rate_limited","retry_after_ms":5000}` | 「发送过于频繁，稍后再试或复用同 id 更新。」 |
| cancel 结果 | `{"cancelled":["a"],"not_found":["b"]}` 或 `{"cancelled":N}`（all） | 撤销清单 |

wait 模式（挂起后回填，outcome 对齐 ask_user 的三态扩展）：

| outcome | payload | 模型看到 |
|---------|---------|----------|
| action | `{"outcome":"action","label":"确认","value":"…"}` | `- 用户在通知上点了「确认」（value）` |
| replied | `{"outcome":"replied","text":"改成室内活动"}` | `- 用户回复：改成室内活动` |
| dismissed | `{"outcome":"dismissed"}` | 「用户划掉了通知（未作选择）。请自行判断继续，并在回复中说明你采用的假设。」 |
| timeout | `{"outcome":"timeout"}` | 「用户在 N 秒内未回应通知，可能不在。请自行判断继续。」 |
| cancelled | `{"outcome":"cancelled"}` | 用户点 `cancel_turn` 按钮 / App 内停止回合（现有合成 cancelled 通道，零改动） |

基础设施错误（`ok=false` 路径：host call 超时框架层、runtime 关停）→ `status:Error` 文本，回合可继续。

**ok 语义对齐 ask_user**：`ok=false` 仅基础设施错误；一切业务结果（含 posted:false、各 outcome）走 `ok=true` + payload。

## 3. 下发契约（host_call_requested，Kotlin 对齐实现）

Rust 归一化/校验**全部在发信封前完成**，Kotlin 收到即合法（仍做防御性兜底，§5.6）。信封与 resolve：

```jsonc
// 下行（emit_raw，与 ServerMsg 同一事件流）
{"type":"host_call_requested","id":42,"tool":"notify",
 "args":{"v":1,"run_id":"r-20261020-…",   // r2(D9)：发起时活动 run 内嵌 args——信封顶层保持
                                          // {type,id,tool,args} 不改 HostCallRouter；Kotlin 从
                                          // args.run_id 读（路由用，§4.5）
   "action":"post","wait":true,"timeout":300,
   "id":"confirm-delete","title":"确认删除","message":"要删除 3 个旧导出文件吗？",
   "kind":"important","public":false,
   "actions":[{"label":"删除","style":"positive"},{"label":"取消","style":"destructive"},
              {"label":"回复","reply":true}]}}

// 上行 resolveHostCall(id, true, payload)——posted:false / 各 outcome 也是 ok=true
{"posted":true,"id":"confirm-delete","kind":"important","channel":"agent_important"}
{"outcome":"action","label":"删除","value":"…"}    // wait 模式后续 resolve
```

- `v:1` 版本化（ask_user 先例），Kotlin 对未知字段向前兼容。
- **run_id 内嵌 args**（r2 D9）：`HostCallRouter` 信封结构不动（`{type,id,tool,args}`，hostcall.rs:111-116）；Rust 侧 `NotifyTool` 经 session_run 句柄取当前 run 写入 args（照 `AskUserContext::find_call_id` 先例，askuser.rs:864-870）——`Tool::execute` 本身无 run 上下文（core/tool.rs:37）。wait 行 `args_json` 快照天然含 run_id，与 §4.2 run_id 列冗余互证。
- 非 wait：默认 `HOST_CALL_TIMEOUT`（120s）足够（Kotlin 权限查询 + post 毫秒级）。
- wait：`allocate_id + call_with_id`（ask_user 先例，runtime 侧先落库再发信封，§4.2）；超时取入参/配置（近似无限 = 永不）。
- 幂等：resolve 对失效 id 无害 no-op（既有 `HostCallRouter::resolve` 语义）。

## 4. 生命周期与控制流

### 4.1 状态机（每条通知，非 wait）

`active(posted) → updated(同 id) → {completed | cancelled | auto_cancelled(点击) | user_dismissed(滑删)}`
通知是 **OS 拥有的瞬时状态**；非 wait **不做持久化**（对比 ask_user 的 pending_questions 表）：进程重启后已发通知仍在通知栏，但本进程登记表丢失——重启自愈是 S2（§1 #18）。

### 4.2 wait 模式（阻塞等待，S1b）

状态机：`pending → action | replied | dismissed | timeout | cancelled | superseded | expired`（对齐 ask_user 的问卷状态机）。

| 事件 | 路径 | 模型看到 |
|------|------|----------|
| 工具发起 | **先落库**（store v9 `pending_notifications`：request_id=host call id、tool_call_id=悬挂声明、run_id（新增列，见表后字段注）、args 快照、status=pending；TTL 24h——清扫在 mobile 层启动路径，store 无 TTL / 无 DELETE）→ 发信封 → 自定义超时等待 | （挂起） |
| 用户点动作按钮（在途） | `resolveHostCall(id, true, {outcome:"action",…})` → oneshot 唤醒 | action 输出 |
| 用户内联回复（在途） | 同上 `{outcome:"replied","text":…}` | replied 输出 |
| 用户划掉通知 | `setDeleteIntent` PendingIntent → receiver → resolve `{outcome:"dismissed"}` | dismissed 文案，**继续干活** |
| 用户点 `cancel_turn` 按钮 | resolve `{outcome:"cancelled"}` + 同步 `RuntimeBridge.cancel()`（停止整个回合） | cancelled（现有合成通道收口） |
| 超时 | router 按 per-call timeout resolve `{outcome:"timeout"}` | timeout 文案 |
| App 内停止回合 | 现有 `ClientMsg::Cancel` → select! drop 工具 future → 合成 cancelled；行收 superseded | cancelled（零改动） |
| 新 turn 抢答 | 对齐 ask_user 的悬挂修复：新 turn 开始前对未决行以合成结果关卷（superseded） | superseded |
| 进程被杀 | `fail_all` 豁免集合含 `notify`（§6；行留持久层，通知由系统持有继续可点）；重启后 `assemble_state` keep-dangling + 启动清扫（mobile 层启动路径，对齐 `startup_question_maintenance`：TTL 过期→合成 expired 补全；已解决→关卷；其余保留） | 该工具调用得到迟到结果，循环续跑 |
| 重启后用户点按钮 | 迟到路径：`ClientMsg::notification_reply`（§5.1）→ `MobileNotifyHandler`：在途先 resolve（即时路径）；否则原子关卷（**首答者赢**）→ 与即时路径**同一格式化函数** → `RunManager::resume_from_tool_result` 补全原 tool result 并续跑 | 迟到等价：模型无法区分即时/迟到 |

**表字段与落位决策**（r2，与先例 `pending_questions` 的差异）：

- store 落位 **v9**：现网 `SCHEMA_VERSION=8`（schema.rs:61），v5 已被 multimodal 版本占用——新表随 `migrate_v8_to_v9`（幂等 IF NOT EXISTS）进入。
- `run_id` 为**新增列**：先例 `pending_questions` 故意不设 run_id（schema.rs:224-226 注释明示不 FK runs 表）。迟到补全本身**不依赖**该列（ask_user 无它照样迟到补全——靠 session_run + Kotlin `openSession` 对齐）；notify 落它的实际价值是**纵深防御**（`openSession` 失败回退当前会话时——§4.5 允许——可检测 `message.run_id ≠ session_run` 错配）与 TTL 清扫/诊断的 SQL 可查性。DDL 注释按此表述。
- **不设 `version` 列**（已拍板）：`args_json` 存完整信封快照（含 `v` 字段），版本信息随快照携带；开发期不做多余兼容列。
- TTL（24h）常量与清扫逻辑在 **mobile 层启动路径**（对齐 `startup_question_maintenance`，askuser.rs:56 / :1570-1625）；store 层无 TTL 任务、无 DELETE 语句（关卷走 UPDATE status）。

**迟到等价不变量**（同 ask_user r5）：动作/回复无论即时返回还是迟到补全，消息历史与工具结果完全一致；迟到的时间信息只进 UI 转写，不进模型上下文。悬挂修复与 `may_pop_dangling_tail` 的 store 权威守卫对 `pending_notifications` 行同样适用（尾部声明仍被 pending 行引用时不 pop）。

### 4.3 post / update / cancel（两种模式共用）

| 事件 | 路径 | 模型看到 |
|------|------|----------|
| post（新 id） | Rust 归一化/限额 → host call → Kotlin 建渠道（幂等）→ 权限检查 → `notify(tag,id)` → resolve | posted / posted:false 原因 |
| update（同 id） | 同 post；Android 同 tag+id 原生替换，不产生新条目、不重复响铃（`setOnlyAlertOnce`） | posted |
| kind 变化（progress → normal） | 同 id 重发时渠道跟随 kind 切换（Android 支持渠道迁移） | posted（channel 字段反映新渠道） |
| cancel(id) | Kotlin 按登记表逐条 `cancel(tag,id)`；**wait 模式 pending 通知被 cancel → 防御性收口**（r2 补定机制：先试在途 `resolveHostCall(request_id, {outcome:"cancelled"})`——失效 id 无害 no-op；迟到则上行 `notification_reply` outcome=cancelled）→ Rust 原子关卷 `status='cancelled'` + 以 cancelled 结果回填续跑。顺序工具执行下该路径通常不可达（wait 阻塞整个 turn），仅 PTC 沙箱并发工具可触达，故为防御实现 | cancelled / not_found |
| cancel(all) | **枚举本进程登记的 agent 通知逐条 cancel**（§5.5） | cancelled:N |

### 4.4 通知动作分流（receiver 内分发）

```text
NotificationActionReceiver.onReceive（PendingIntent extras：agent_id / run_id / request_id? / kind）
  ├ reply 文本        → §4.5 回流
  ├ action 按钮       → wait 且在途/pending → resolve / notification_reply（outcome:action）
  │                     否则 → §4.5 回流（value 作为用户消息）
  ├ behavior:cancel_turn → wait 在途 → resolve {outcome:cancelled}；随后 RuntimeBridge.cancel()
  │                        非在途  → 直接 RuntimeBridge.cancel()（停止当前回合，即时生效）
  └ （contentIntent）  → 打开 MainActivity + runId 路由（§4.5 末）
```

### 4.5 回流：新用户消息 + runId 对齐 + busy 排队（S1）

**语义：非 wait 回应用户消息，开启新 turn**（无悬挂 tool call，天然满足消息序约束——与 wait 模式的「回填原 tool call」形成两套明确分工）。

```text
文本回流（按钮 value / 内联回复，带 runId extra）
  → RuntimeBridge.submitFromNotification(text, originRunId)
      1. 目标会话对齐（已拍板 S1 就做）：currentRunId() != originRunId
         → 回合空闲：FFI openSession(originRunId)（失败/已被删 → 回退当前会话 + notice + log）
         → 回合进行中：入 pendingSubmits 队列（记 originRunId），turn 终态 flush 时先切会话再 submit
      2. 回合空闲 → submit(text)（既有 ClientMsg::submit 通路，RuntimeBridge.kt:766）
      3. 回合进行中（同会话）→ 入 pendingSubmits 队列，turn_ok/turn_error 逐条 flush
         （session handle_submit busy 直接拒绝、无排队——session.rs:199-241，故必须在 Kotlin 侧等 turn 终态）
  → 新 turn 开始，模型收到该文本作为用户消息
```

- contentIntent 同样带 runId：MainActivity 读 intent extra → 空闲时 `openSession(runId)` 对齐、busy 时忽略保持当前（打开 App 看上下文是主诉求，深链到具体条目 S3）。
- 回复文本**原文注入**（模型上下文零噪音）；「来自通知」来源标记只进 UI 转写（S2 #19）。
- wait 模式的回复不走 submit：`request_id` 在途 → `resolveHostCall`（与 session 无关）；迟到 → 先对齐会话再上行 `notification_reply`（§4.2）。

### 4.6 turn 终态自动收敛（防僵尸常驻通知）

场景：模型发了 `ongoing` 进度通知后回合结束、忘记 `completed`/`cancel` → 通知永久常驻。
规则：RuntimeBridge 在 turn 离开 running 态（turn_ok / turn_error / 合成 cancelled）时调 `AgentNotifier.settleOngoing()`——把**本进程登记的** ongoing 通知重建为非常驻（去 `setOngoing`、恢复 `autoCancel`，内容不变）。模型之后仍可正常同 id 更新。mobile 单会话单活动 turn，全局收敛即正确。**wait 模式挂起中的通知不参与收敛**（其 ongoing 属等待语义，由 outcome 收口）。

### 4.7 冷启动（进程被杀后点通知动作）

Receiver 在 Manifest 注册（`exported=false`），进程死也能被 PendingIntent 拉起：

```text
receiver.onReceive → RuntimeBridge 未 started
  → 上下文 startForegroundService(AgentService)   // 拉起 runtime
  → 动作/文本入对应队列（pendingSubmits / 迟到 notification_reply 缓冲）
  → RuntimeBridge.start 完成（started=true）后 flush
  → runtime 起不来（CreateFailed）→ 队列丢弃 + log（尽力而为语义）
```

前台化不新增入口：AgentService 既有 `startForegroundWithNotification` **单函数内 `SDK_INT` 分支**（API 34+ 传 `FOREGROUND_SERVICE_TYPE_SPECIAL_USE`，其余走无类型调用；AgentService.kt:129-155，FGS 通知 1001 见 :158）——notify 只复用 `startForegroundService` 拉起路径，不改前台化机制、不新增 FGS 类型。

### 4.8 ask_user 伴生唤回通知（S1，宿主行为）

- 触发：`UiState.pendingQuestion` 持续超过 `[ask_user] companion_notify_seconds`（建议默认 60，0 = 关；S1 以文件配置 + 代码默认常量生效，设置页 UI 后补）且 App 不在前台——RuntimeBridge 协程 timer 驱动，**不经模型工具调用、不占模型限额**。
- 通知形态：`agent_messages` 渠道、固定 id `"ask-user-nudge"`（同问卷覆写复用）、title「Agent 在等你回答」、message = 首题摘要；动作：
  - 「去回答」：打开 MainActivity + 导航 `question-fullscreen`（intent extra）；
  - 「不回答」：走现有 decline 路径（在途 → `resolveHostCall(declined)`；恢复卡 → `question_reply decline`）。
- 问卷终态（answered/declined/timeout/…）或 App 回前台 → cancel 该通知 + 停 timer；重启回放 pending 问题后重新武装 timer。

## 5. Kotlin 侧设计

### 5.1 新文件与协议

| 文件 | 角色 |
|------|------|
| `bridge/AgentNotifier.kt` | object 单例：渠道注册（幂等）、`areNotificationsEnabled` + API 33 `POST_NOTIFICATIONS` 检查、post/update/cancel/settleOngoing、agent-tag↔(tag,id) 登记表、渠道禁用检测（`getNotificationChannel().importance == IMPORTANCE_NONE`）、伴生通知入口 |
| `bridge/NotificationActionReceiver.kt` | 动作/回复/划掉接收器（Manifest 注册 `exported=false`）：RemoteInput 取文本、wait 分流、冷启动排队、cancel_turn 分发 |
| `RuntimeBridge.kt`（改） | `handleHostCall`（:2648；`host_call_requested` 信封分发在 :2086）加 `"notify"` 分支；`submitFromNotification` + `pendingSubmits`（带 runId）+ turn 终态 flush/settleOngoing 钩子；伴生 timer；`notification_reply` 迟到上行 |
| `MainActivity.kt`（改） | API 33+ 首启请求 `POST_NOTIFICATIONS`（拒绝后记 prefs 不再自动弹）；runId intent 路由；「去回答」深链导航 |
| `AndroidManifest.xml`（改） | 注册 receiver；权限**已声明**（xml:4-11），无需新增 |

协议扩展（S1b，对齐 ask_user r4 先例）：`ClientMsg::notification_reply {request_id, run_id, outcome, label?, value?, text?}`（outcome **上行白名单** `action|replied|dismissed|cancelled`；`timeout`/`expired` 仅 Rust 侧合成——router 超时 / TTL 清扫——上行收到白名单外值拒绝/忽略）+ store v9 `pending_notifications` 表（落位与字段决策见 §4.2 表后注）+ `PROTOCOL_VERSION` 升级 + 契约测试。

### 5.2 渠道设计（与 AgentService 既有渠道并存，不碰 1001）

| 渠道 id | 用户可见名 | Importance | 用途 |
|---------|-----------|------------|------|
| `agent_messages` | Agent 消息 | DEFAULT | normal 通知 / 伴生唤回 |
| `agent_important` | Agent 重要提醒 | HIGH | important / wait 确认（heads-up 资格，**是否弹横幅由系统/用户设置决定**） |
| `agent_progress` | Agent 任务进度 | LOW | progress（更新不响铃不横幅，配 `setOnlyAlertOnce`） |

- 渠道在 AgentNotifier 首次使用时幂等创建；AgentService 的 "Agent Runtime"（IMPORTANCE_LOW，NOTIFICATION_ID=1001）不动。
- 渠道级 `setLockscreenVisibility(VISIBILITY_PRIVATE)`（默认）；`public:true` 的通知用**通知级** `setVisibility(VISIBILITY_PUBLIC)` 覆盖（渠道默认不受影响）。

### 5.3 通知构造要点（沉淀调研报告的坑）

- 小图标：复用 AgentService 现有 smallIcon 资源（如需视觉区分再新增单色矢量 `ic_stat_agent`，实施时定）。
- `BigTextStyle`：message 超单行阈值自动挂（折叠态截断 + 展开态全文）。
- 进度：`setProgress(100, v, false)` / `(0,0,true)`；`completed` 时 `setProgress(0,0,false)` + 去 ongoing。
- 动作 PendingIntent：`PendingIntent.getBroadcast(..., FLAG_UPDATE_CURRENT or FLAG_MUTABLE)` + **显式组件 Intent**（RemoteInput 必须 MUTABLE；显式组件防 Intent 劫持——报告安全要点）。request code 按 agent-tag 派生稳定值（同 id 更新覆写，不叠 PendingIntents）。
- `contentIntent`：MainActivity launcher intent（`FLAG_ACTIVITY_NEW_TASK or FLAG_SINGLE_TOP`），extra 带 runId。
- `setDeleteIntent`：仅 wait 模式挂（用户滑删 → dismissed 回填）。
- RemoteInput 回复后用 `setRemoteInputHistory` 展示已发送的回复片段（后续同 id 更新时带上）。
- Receiver 内**不做任何耗时操作**：取文本 → 分流入队/resolve → return（报告强调 BroadcastReceiver 不跑 LLM/IO）。

### 5.4 wait 模式通知的附带数据

每个动作/回复/delete PendingIntent extras：`request_id`（host call id）、`run_id`、`agent_id`——进程重启后通道复位，迟到点击凭 `request_id` 走 `notification_reply`，凭 `run_id` 对齐会话。

### 5.5 通知 id 映射与 FGS 保护

- Android 通知寻址 `(tag: String, id: Int)`：agent id → `tag = "an-" + sanitize(agentId)`、`int id = stableHash32(agentId)`（**纯函数稳定派生**，重启后同 agent id 仍能命中既有通知）。自动生成的一次性 id 用内部计数器 `auto-{n}`。
- **禁用 `NotificationManager.cancelAll()`**：会连 AgentService 的 FGS 常驻通知（1001）一起取消。`cancel(all)` 只枚举本进程登记表逐条 cancel；登记表丢失时如实报 not_found。

### 5.6 双侧校验分工

**Rust 侧做全部业务校验与限额**（归一化：缺省、钳制、截断、UTF-8 字符边界安全）；Kotlin 侧只做防御性兜底（不信任任何入参：长度再截、枚举再滤、异常兜 `posted:false` + reason）。

## 6. Rust 侧设计（`crates/openslate-mobile/src/notify.rs`）

- `NotifyTool`：`impl Tool`（`crates/openslate-core/src/tool.rs:17-47` trait：name/description/手写 `parameters_schema`/async execute）。execute = 归一化校验 → 限额闸 → 非 wait `HostCallRouter::call("notify", args)` / wait `allocate_id + call_with_id`（先落库，ask_user 先例）→ 结果格式化（自然语言 + JSON 双写）。错误恒 `Ok(ToolOutput{status:Error})`，回合不中断。
- `MobileNotifyHandler`（对齐 `MobileQuestionHandler`）：`notification_reply` 上行的双通路处理（在途 resolve / 迟到原子关卷 + `resume_from_tool_result` 续跑）+ 启动清扫（TTL expired / 已解决关卷 / 可恢复保留——清扫在 **mobile 层启动路径**，对齐 `startup_question_maintenance`；store 无 TTL / 无 DELETE）。
- 注册：`runtime.rs` `create_with` 第 6 步工具集组装处加入（`mobile.ping` 同位点）。
- **可见性白名单**（全套先例 = `output_query`，走 DEFAULT + LEGACY + REQUIRED 三件；注意 `ask_user` **不在任何白名单**，不能当本项先例引用）。三步必做：
  1. `bootstrap.rs` `DEFAULT_ROOT_AGENT_MD`（:30-41）`tools:` 追加 `notify`——新装设备默认可见；
  2. 旧默认 root.md 经 `LEGACY_ROOT_AGENT_MDS` 逐字节匹配迁移（bootstrap.rs:30-85，仅逐字节等于旧默认的文件）；
  3. `REQUIRED_BUILTIN_TOOLS`（bootstrap.rs:308）补 `notify`——定制过 root.md 的存量设备启动时**外科式补齐**（app 私有目录无编辑器可达、设置页无该文件编辑入口，用户没有手动补的途径；刻意关闭请用工具自身配置开关而非白名单剔除，见 bootstrap.rs:303-308 注释）。
  `output_query` 是唯一走齐全套的先例；工具管理页手动开启（`setAgentToolVisibility`）仍可作为兜底说明文档保留。
- **hostcall.rs 调整**：现状为硬编码 `tool == ASK_USER_TOOL`（hostcall.rs:173-180）；实施改为**豁免工具名集合 `[ask_user, notify]`**（最小改法，不引入按调用注册新机制）——`fail_all` 对集合内工具的在途调用放回 pending 而非合成失败：wait 模式 notify 行留持久层等待迟到补全（§4.2）；非 wait 调用无持久行，future 随 runtime 收束被 drop，豁免与否无落地行为差异；ask_user 行为不变。
- **限额常量**（模块顶部 `const`，对齐 shell.rs `MAX_OUTPUT_BYTES` 风格；**宽松档已拍板**）：

| 常量 | 值 | 语义 |
|------|------|------|
| `MAX_TITLE_CHARS` | 60 | 标题截断 |
| `MAX_MESSAGE_CHARS` | 2000 | 正文截断（字符边界安全） |
| `MAX_ACTIONS` | 3 | 动作数上限（Android 上限即 3） |
| `MAX_ACTIVE_NOTIFICATIONS` | 16 | 活跃 agent 通知上限，超出自动 cancel 最早的非 ongoing 条目 |
| `RATE_LIMIT_PER_MINUTE` | 12 | **新 id** post 频率；同 id 更新豁免（进度更新合法高频）；wait 模式通知豁免（一等待一条） |

- 配置：`[notify] default_wait_timeout`（wait 模式缺省秒数；建议默认 **300**，`-1` 永不）；`[ask_user] companion_notify_seconds`（§4.8，默认 60，0 = 关）。两值随装配快照固定、重启生效（对齐 AskUserContext 现状）。**S1 descope**：两键仅走文件配置 + 代码默认值；设置页「内置工具行为」调节 UI（对齐 `[ask_user] default_timeout` 的设置页模式）后补，不随 S1 交付。
- 审计：走既有 `ToolAuditRecord`（所有 Tool 调用自动覆盖），无特殊处理。

## 7. 安全、隐私与滥用防护

1. **锁屏默认隐藏内容**，`public:true` 显式 opt-in（description 明令公开内容不得含敏感信息——锁屏明文 + 可能被朗读/投屏/穿戴设备显示）。
2. **通知内容约束**：不得放密钥/敏感明文；Rust 侧可复用 `mask_secrets` 已知值打码管道（ask-user.md §8.4 的 masker 注册点同样作用于 notify 正文）。**S1 descope：不接线，仅本文档化**（接线随后续版本）。
3. **滥用防线**：频率限额（宽松档）+ 活跃上限 + important 渠道用户可系统级关闭（关了如实回 `channel_disabled`）。
4. **PendingIntent 安全**：全部显式组件；`FLAG_MUTABLE` 仅 RemoteInput/deleteIntent 动作使用。
5. **Receiver `exported=false`**：外部应用无法伪造通知动作注入用户消息/伪造 resolve。
6. **不虚报送达**：`posted:true` 措辞恒为「已提交」。
7. **迟到首答者赢**：`pending_notifications` 行原子迁移，重复点击/多入口并发只认首份（对齐 ask_user 不变量）。

## 8. 测试计划

### 8.1 Desktop（`cargo test -p openslate-mobile` 等，无 Android 依赖）

- `notify.rs` 单测：归一化矩阵（缺省/越界钳制/截断字符边界/非法 kind/progress 边界 0·100·101·-1/互斥字段/action 分支/wait 组合校验/cancel_turn 带 value 拒绝）、信封逐字段契约、结果格式化（posted / 各 posted:false 原因 / wait 全 outcome）、限额（新 id 计数、同 id 豁免、活跃上限淘汰、限流文案）、超时路径、`fail_all` 豁免集合矩阵（`[ask_user, notify]` 放回 pending 不合成失败；其余工具照旧；ask_user 行为回归不变）。
- `runtime_flow.rs` 集成：`notify_roundtrip`（信封 → resolve posted → ToolOutput 入历史）、`notify_permission_denied_roundtrip`、`notify_wait_immediate_path_roundtrip`（wait → 点按钮 resolve → outcome 入历史）、`notify_wait_timeout`、`notify_wait_restart_recovery_and_first_answer_wins`（预置崩溃现场 → 恢复 → notification_reply 续跑 → 重复应答首答者赢）、`notify_wait_dismissed`、`notify_wait_timeout_then_late_reply_noop`（r2 补：timeout 已获胜后迟到回复 → 原子 UPDATE 0 行 → no-op + log，不得注入新消息）、`notify_wait_cancel_superseded`（r2 补：cancel 防御收口路径，§4.3）、`notify_wait_restart_recovery_dismissed`（r2 补：重启恢复 dismissed 变体，验证迟到上行通路等价）。
- store crate：`pending_notifications` 行 insert / 原子迁移（v8→v9）/ CHECK 约束 / pending 清单（store 无 TTL / 无 DELETE；TTL 清扫逻辑归 mobile 层启动路径测试）。
- protocol：`notification_reply` 契约测试（含 uplink outcome 白名单 `action|replied|dismissed|cancelled` 之外的拒绝）+ `PROTOCOL_VERSION` 升级同步。

### 8.2 真机验证（release 包，用户执行，agent 不驱动 UI）

| 设备 | 要点 |
|------|------|
| Huawei API 26 | 三渠道创建；普通/重要/长文本展开；进度转圈与百分比；同 id 更新不重复；按钮与内联回复（API 24+ RemoteInput）；cancel；锁屏默认只见标题、public:true 明文可见 |
| Xiaomi API 37 | POST_NOTIFICATIONS 首启弹窗 → 拒绝：模型收到 permission_denied 文案 → 系统设置开启后恢复；important 横幅（若系统允许）；progress 更新静默；渠道单独关闭 → channel_disabled；深色模式小图标 |
| 双端 | 进度通知回复 → busy 排队 → turn 结束自动提交；**runId 对齐**：切到会话 B 后点会话 A 的通知回复 → 消息进 A；wait 确认：点按钮续跑、划掉通知 dismissed 续跑、超时文案；**杀进程 → 点通知按钮 → 服务拉起 → notification_reply 迟到补全续跑**；伴生唤回：ask_user 挂起 60s → 通知出现 → 「去回答」直达全屏问卷；turn 结束 ongoing 自动收敛；cancel(all) 后 FGS 常驻通知仍在 |

## 9. 分期计划

### S1a（首版核心：非阻塞全类型 + 路由 + 伴生）

- Rust：`notify.rs`（NotifyTool：归一化 + 限额 + 非 wait 全格式化）；`runtime.rs` 注册；`bootstrap.rs` 白名单三件套（DEFAULT + LEGACY + REQUIRED，§6）；`hostcall.rs` 豁免泛化为集合 `[ask_user, notify]`（§6）。
- Kotlin：`AgentNotifier.kt`、`NotificationActionReceiver.kt`（+ Manifest）、`RuntimeBridge` notify 分支 / submitFromNotification（含 runId 对齐 + busy 排队）/ settleOngoing / 伴生 timer、MainActivity 首启权限 + runId 路由 + 「去回答」深链。
- 零协议变更、零 FFI 面变更（host call 信封既有机制，无需重跑 uniffi-gen）。

### S1b（wait 阻塞模式：持久化三件套，对齐 ask_user r4 先例）

- Rust：wait 路径（落库 + call_with_id + outcome 格式化）、`MobileNotifyHandler`（迟到双通路 + 启动清扫 + resume_from_tool_result）、`fail_all` 豁免接线。
- 协议：`ClientMsg::notification_reply` + `PROTOCOL_VERSION` 升级 + 契约测试。
- store：v9 `pending_notifications` 迁移（现网 `SCHEMA_VERSION=8`，v5 已被 multimodal 占用，见 §4.2 表后注；TTL 清扫在 mobile 层，store 无 TTL / 无 DELETE）。
- Kotlin：receiver 的 wait 分流（在途 resolve / 迟到 notification_reply）+ 冷启动缓冲。
- FFI：无新面（reply 走既有 `send`）。

### S2（体验增强）

- InboxStyle 多条目摘要；重启自愈（tag 清单落盘，启动清 ongoing 遗留）；回复转写「来自通知」标记；深链到具体条目。

### S3（原生增强，通用 API 但高门槛）

- API 36+ `ProgressStyle` 原生进度样式 + `setRequestPromotedOngoing`（Live Updates）：需声明 `POST_PROMOTED_NOTIFICATIONS`、渠道重要性/无自定义 RemoteViews 等全套准入检测，**只对真正有进度的长任务启用**（调研报告结论：不要把「Agent 正在思考」上岛）；通知分组 group summary。

### 另行立项

- 厂商实时活动（报告 §三：小米超级岛 extras 接入、OPPO 泛在服务 SDK 独立模块、vivo 原子通知/VPush）——均需厂商业务准入与模板审核，且需服务端配套（vivo AppSecret 不得进客户端），不属「Android 通用通知」。

## 10. 决策记录（2026-10-20 用户拍板 + 建议值）

| # | 决策 | 状态 |
|---|------|------|
| 1 | 工具名 `notify` | ✅ 拍板 |
| 2 | 双模式：默认非阻塞（回应走新 turn）+ `wait:true` 阻塞（回填原 tool call 续跑） | ✅ 拍板 |
| 3 | 通知动作支持 `behavior:"cancel_turn"`（即时停止当前回合） | ✅ 拍板 |
| 4 | runId 对齐进 S1（跨会话回复/点击路由到通知所属会话） | ✅ 拍板 |
| 5 | 权限首启请求一次，拒绝不追弹；工具侧如实回 permission_denied | ✅ 拍板 |
| 6 | ask_user 伴生唤回通知进 S1 | ✅ 拍板 |
| 7 | 限额宽松档：12 条新通知/分钟、活跃 ≤16（同 id 更新豁免） | ✅ 拍板 |
| 8 | 锁屏可选公开：默认隐藏，`public:true` 显式 opt-in | ✅ 拍板 |
| 9 | wait 超时缺省 300s（`-1` 永不；`[notify] default_wait_timeout` 可调） | 建议值（通知场景用户不在屏幕前，「永不」会让 turn 无限挂起；与 ask_user 的 App 内「永不」首装默认不同属有意差异） |
| 10 | wait 划掉通知 = `dismissed`（`setDeleteIntent`），区别于 timeout | 建议值 |
| 11 | 伴生唤回延迟 60s（`companion_notify_seconds`，0 = 关） | 建议值 |
| 12 | S1a/S1b 拆分：非阻塞核心先行，wait 持久化三件套（含协议扩展）第二批 | 建议值（wait 模式的迟到补全依赖协议 + store + handler，与 ask_user r4 同量级；拆批降实施风险） |
| 13 | PTC=Both、不挂审批门、单工具 `action: post|cancel`、三渠道、禁 cancelAll 保 FGS | 沿用首版设计（未变） |
