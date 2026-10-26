# 设计提案：Agent 向用户提问工具（`ask_user`）

> 状态：待评审（design-only，未实施）
> 依据：`.ref/` 五个参考项目实现调研（opencode / codex / pi / minimax-code / deepseek-harness）、业界产品调研（Claude Code AskUserQuestion、Gemini CLI、Cursor、Amp、消费级 App）、本项目现状摸底（host call 通道、审批系统、Compose UI）。
> 约束：**移动端优先**——所有交互以 Android/Compose 小屏单手操作为第一约束；TUI/桌面端后续接入。
> 修订：r2（2026-10-18）吸收外部调研报告：MCP Elicitation 对齐（accept/decline/cancel）、敏感输入通道隔离、持久化三层模型、交互原语与契约字段扩展（标 Δ）。
> 修订：r3（2026-10-18）secret 设计拍板：env/file 双落地、用户勾选是否记住、仅引用回填 + 可开关的输出脱敏（新「内置工具行为」设置页）；secret 移入 S1 完整实现。
> 修订：r4（2026-10-18）全部决策点拍板：默认超时用户可配、decline 整卡+逐题、工具名 ask_user、PTC=Both、持久化三件套（持久化/重启恢复/迟到答案注入）提前进 S1（含协议扩展）。
> 修订：r5（2026-10-18）迟到答案语义改定：不注入新用户消息——答案直接补全原 tool call 的 tool result 并续跑 agent 循环；悬挂 tool call 在新 turn 前 / TTL 过期时以合成结果修复。
> 修订：r6（2026-10-18）§4 对齐 UI 设计稿（`ask-user-ui-design.md`）：分段指示器内联卡 + 全屏作答取代底部弹层草案。

---

## 0. 定位与边界

- `ask_user` 是**信息收集**工具：Agent 缺少必要信息、或面临分叉决策时，向用户提问并等待回答。
- 与现有审批系统（`ApprovalBanner`，approve/deny/approve_all）**保持分离**：
  - 审批 = 高风险操作的风险门（不给自由答案负载）；
  - 提问 = 信息获取（答案任意 JSON）。
  - 参考项目两种流派（opencode 把提问并入权限系统；codex/deepseek 分离）中，**分离派**更贴合本项目现有架构：审批走 `SessionApprovalBridge`，提问走 host call。
- **敏感信息交接是第三条独立通道** Δ（已拍板 r3）：密码/API Key/OAuth 不作为普通问题答案收集（MCP Elicitation 明确禁止 form 模式收密钥）。`secret` 题型落地：
  - 答案存入宿主凭据库，按模型声明的 `storage` 双形态生效——**环境变量**（后续本会话 shell/bash 工具执行时自动注入，如 `GITHUB_TOKEN`）或**文件**（写入指定路径，如 `~/.netrc`）；
  - 持久范围由用户在提交时勾选「记住此凭据」决定：默认仅本会话有效，勾选则入持久凭据库；
  - 模型只收到引用回填（变量名/路径 + 生效范围），**明文不进上下文与日志**；工具输出捕获对已知密钥值自动打码（***），该开关放在「内置工具行为」设置页由用户控制；
  - 防护边界：挡意外泄漏（回显/日志/转写），不承诺防刻意 exfiltration（模型仍可诱导 shell 间接拼接输出）。
- **不对 ask_user 做二次审批**：默认策略（AutoExcept shell/run_code）下自然放行；若用户开 Manual 全审模式，ask_user 应在豁免清单中显式排除——"问个问题还要先批准才能问"是体验灾难。
- PTC 可见性（已拍板 r4）：**Both**（默认）——QuickJS 沙箱编排内亦可调用 ask_user，host call 机制天然支持嵌套问答。

## 1. 场景覆盖矩阵

| # | 场景 | 交互形态 | 阶段 |
|---|------|----------|------|
| 1 | 方案二选一/多选一 | 单选：选项 label + description，推荐项置顶标记 | S1 |
| 2 | 多选（要包含哪些模块） | `multi_select` | S1 |
| 3 | 开放信息（命名、URL、路径） | `text` + placeholder | S1 |
| 4 | 是/否（要不要顺带跑测试） | `confirm`（两选项特例，UI 渲染成两枚按钮） | S1 |
| 5 | 一次问清多件事 | `questions[]` 批量（1~4 题），UI 向导分页 | S1 |
| 6 | 可跳过的弱偏好 | `optional` + 逐题"跳过" | S1 |
| 7 | 自由输入兜底 | UI 恒附"其他"输入框；schema 禁止模型自带 Other（业界共识） | S1 |
| 8 | 用户不想答 | decline → 模型可见"用户未作答，请自行判断"（Gemini 式，区别于错误）；**整卡级 + 逐题级都有**（已拍板 r4） | S1 |
| 9 | 用户打断会话（停止按钮） | 现有 Cancel 通道 → 合成 cancelled 结果（现有机制，零改动） | S1 |
| 10 | 用户不在场（AFK） | 默认策略**用户可配**（永不超时 或 阈值，设置页）；工具入参 `timeout` 逐次覆盖；超时回填"用户可能不在，自行判断继续"（已拍板 r4） | S1 |
| 11 | 凭证/密钥输入 Δ | `secret`：掩码输入 → 凭据库 + env 注入/写文件（模型声明 storage）、勾选是否记住、模型仅获引用、输出脱敏（可开关） | S1 |
| 12 | 用户发新消息抢答/改主意 | 消息 steer + 问卷 superseded（minimax） | S2 |
| 13 | App 被杀后重启继续答 | 问卷持久化（sqlite，TTL 24h）+ 重启恢复 + 迟到答案**补全原 tool call 结果并续跑**（r5 改定，非用户消息注入） | **S1**（已拍板 r4） |
| 14 | 超时自动采纳推荐项 | auto-reply scheduler，`requiresExplicitResponse` 可禁（minimax） | S2 |
| 15 | 答案引出下一问（追问链） | `when` 条件字段级联显隐（opencode Form）——S1 起模型可再次调用工具实现追问，Form 为渐进增强 | S3 |
| 16 | 视觉/方案对比 | 选项带 image / preview（手机看图天然合适） | S3 |
| 17 | 图片作为答案（报错截图） | 附件回答 | S3 |
| 18 | 决策被长期记住 | VerifiedAnswer 式持久记忆，抗上下文压缩（codex） | S3 |
| 19 | MCP server elicitation | 复用同一提问 UI 基座（payload 中立于来源）；typed form 与 elicitation form-mode 子集兼容 Δ | S3 |
| 20 | 数字/日期/范围输入 Δ | typed form 字段：原生数字键盘、日期选择器 | S3 |
| 21 | 排序/优先级 Δ | 逐项定序（Claude 交互式提问有先例） | S3 |
| 22 | 仓库内路径/文件选择 Δ | 资源浏览器（文件树/最近修改），返回资源引用而非裸字符串 | S3 |
| 23 | 稍后回答/推送唤回 Δ | 待答收件箱 + 推送通知 + 深链定位（Amp iOS / Copilot Remote 先例） | S2 |
| 24 | 多端同时竞答 Δ | 首份有效答复原子胜出（first-response-wins；与本项目 approval 首答者赢同语义） | S2 |
| 25 | 人工接管（登录/支付等不可委托操作） | 浏览器/凭证流程接管后交还（Gemini Tasks 先例） | 远期 |
| 26 | 权限/风险确认 | **不属于本工具**——走现有 Approval | — |

## 2. 工具 Schema

工具名 `ask_user`。宿主直连 `Tool` trait 实现（`HostTool` 同款模式），注册进 `ToolRegistry`。

### 输入

```jsonc
{
  "questions": [                      // 1~4 题，建议 ≤3
    {
      "id": "db_choice",              // 可选，snake_case，答案键；缺省 q1..qn
      "header": "数据库",              // ≤12 字符，卡片标签
      "question": "本地存储用 SQLite 还是 sled？",  // 完整问句
      "type": "choice",               // choice | text | confirm | secret（默认 choice）
      "options": [                    // choice/confirm 必填，2~4 个
        { "label": "SQLite", "description": "零配置，移动端生态默认", "recommended": true },
        { "label": "sled", "description": "纯 Rust，写性能好" }
      ],
      "multi_select": false,          // 仅 choice
      "placeholder": "选其他时请说明",   // text/secret/其他 输入框提示
      "optional": false,              // true 时 UI 允许跳过
      "storage": { "kind": "env", "name": "GITHUB_TOKEN" }
                                       // 仅 secret，必填：落地形态 env | file；
                                       // env → 注入本会话后续 shell/bash 执行环境
                                       // file → 写入指定路径（如 ~/.netrc）
    }
  ],
  "timeout": -1                       // 秒；-1 = 无限等待；缺省取用户设置（永不超时/阈值，见「内置工具行为」页）
}
```

### 工具 description 文案要点（教模型用好，参考各家收敛经验）

- 只问**阻塞你继续**的问题；能合理假设的就直接假设并在回复中说明。
- 推荐项放第一位、label 加 "(Recommended)" 并置 `recommended: true`。
- **禁止**自带"其他/Other/都不是"选项——UI 会自动附加自由输入框。
- 相关的小问题合并成一次批量调用；无关的不要凑数。
- confirm 用于二值决策；text 用于命名/地址类；secret 用于密钥。
- 选项 label 1~5 词，description 写清 trade-off。

### 输出（回填模型：自然语言 + 结构化并存，仿 opencode）

```
User answered your questions:
- "db_choice" → SQLite (Recommended)
- "scope" → tests, docs              // 多选拼接
- "note" → (skipped)                  // optional 未答
- "extra" → (declined)                // 逐题 decline
```
```jsonc
{ "outcome": "answered",
  "answers": [
    { "id": "db_choice", "selected": ["SQLite"] },
    { "id": "scope", "selected": ["tests", "docs"] },
    { "id": "note", "skipped": true },
    { "id": "extra", "declined": true }
  ] }
```

其他 outcome（对齐 MCP Elicitation 的 accept / decline / cancel 三态）：
- decline（不答，整卡或逐题）：整卡 `{ "outcome": "declined" }` + 文案 `"用户关闭了提问卡片未作答。请自行判断继续，并在回复中说明你采用的假设。"`；逐题 decline 则该题答案为 `{ "id": ..., "declined": true }`，其余题照常回填
- timeout（若配置）：`{ "outcome": "timeout" }` + `"用户在 N 秒内未回应，可能不在。请自行判断继续。"`
- turn 取消：走现有合成 cancelled 结果，不经本工具逻辑。
- secret 题型 Δ：答案写入宿主凭据库并按 `storage` 生效（env 注入后续 shell/bash 执行 / 写入指定文件）；回填模型的只有 `"已存入凭据：环境变量 GITHUB_TOKEN（本会话生效）"` 或 `"...（已记住，后续会话可用）"`；明文不进上下文、不进日志；工具输出对已知密钥值打码（设置页可关）。

## 3. 生命周期与控制流

状态机：`pending → answered | declined | timeout? | cancelled`

| 事件 | 路径 | 模型看到什么 |
|------|------|--------------|
| 用户提交 | `resolveHostCall(id, ok=true, answersJson)` → oneshot 唤醒 | answered 输出 |
| 用户关闭卡片不答（decline） | `resolveHostCall(id, ok=true, {outcome:"declined"})` | declined 文案，**继续干活** |
| 用户点停止（打断会话） | 现有 `ClientMsg::Cancel` → select! drop 工具 future | 合成 cancelled（现有机制） |
| 超时（若配置） | router 按 per-call timeout resolve `{outcome:"timeout"}` | timeout 文案 |
| App 被杀/重启 | 持久化恢复：重启后 pending 问卷回放（快照，含 tool_call_id 关联）；迟到答案经 `question_reply` **补全原 tool call 的 tool result 并续跑 agent 循环**（r5 改定，不注入新用户消息） | 该工具调用得到结果，循环继续 |
| App 退后台/锁屏 Δ | 异步 oneshot：进程不死即保持 pending，不自动取消 | （不触发 resolve） |
| 重复/多入口提交 Δ | host call id 一次性 resolve，幂等首答赢（stale resolve 为 no-op） | 首份有效答案 |

工程要点：

1. **通道**：`HostCallRouter::call`（request/resolve + 异步 oneshot）。对 abort 友好（select! drop 即解开），shutdown 走 `fail_all` 兜底。
2. **超时**：host call 默认 120s 对"等人"不够。`HostCallRouter` 构造已支持自定义超时；ask_user 默认取用户设置（永不超时 / 阈值，首装默认建议永不超时），工具入参 `timeout` 可逐次覆盖。
3. **ok 语义**：`ok=false` 只留给基础设施错误；一切业务结果（含 declined/timeout）用 `ok=true` + `outcome` 字段，避免把"用户没答"当异常。
4. **取消时的 UI 清理**：host call 没有显式取消事件。Kotlin 侧以 turn 状态投影为准——turn 离开 running 态即清 pending 问题卡片；resolve 到已失效 id 必须是无害 no-op（实施时验证 `HostCallRouter::resolve` 对 stale id 的行为）。
5. **答案双写**：ToolOutput（本轮模型可见）+ 聊天流转写（transcript 可见，复用 `UiEntry.ToolCall` 的 argsPreview/output 展示问题与答案）。
6. **payload 版本化**：host call payload 加 `v` 字段，UI 对未知字段向前兼容。
7. **迟到等价不变量（r5）**：对模型而言，答案无论即时返回、还是触发持久化后迟到补全，**消息历史与工具结果完全一致**（assistant(tool_calls) → tool result → 续跑循环），模型无法区分也没有必要区分；迟到的时间信息只进 UI 转写，不进模型上下文。推论：进程中断路径**不得**对悬挂的 ask_user tool call 预先合成 cancelled 结果（否则迟到答案无处补全）——仅三种情况合成关闭：用户显式停止（cancelled 关卷）、新 turn 开始前（superseded）、TTL 过期（expired）。

## 4. 移动端 UI 设计（Compose）

交互骨架（用户拍板）+ 完整视觉/交互规范见 **`docs/proposals/ask-user-ui-design.md`**（@designer 产出，与 r5 对齐）：

- **QuestionCard 内联卡**：顶部**分段指示器**（每段一题；颜色表达已答/未答、高度表达当前题；1 题时整条隐藏），点分段切题、题在卡片内直接作答；横滑 Pager 与点分段等效（与聊天纵滚正交）。
- **全屏作答**：卡片头部全屏按钮 → NavHost 新路由竖直滑入全屏问卷，复用同套题型控件，草稿不丢；退出即回内联卡原样。
- **题型控件**：choice 单/多选（RadioButton/Checkbox + 推荐徽章）、text 多行输入、confirm 双按钮、secret 掩码输入 +「记住此凭据」勾选 + env/file 落地形态行；每题恒附「其他」输入兜底；optional「跳过」、逐题「此题不答」、整卡「不回答」（AppDialog 二次确认）。
- **状态呈现**：pending（0.5dp primary 呼吸边框 + 13dp spinner）→ submitting → answered（折叠为答案 chip 摘要卡）；declined / cancelled / superseded / expired 为轻摘要行。**ask_user 的 ToolCall 条目被 QuestionCard 吸收渲染**，不另出通用工具 chip。
- **草稿**：以 requestId 为键挂 UiState 层，切题/进出全屏/卡片重组不丢；未提交草稿不持久化（重启后空草稿重答）。
- 滚走提醒 pill（输入区上方悬浮）、必填校验（「还有 N 题未作答」+ 提交禁用）、键盘 imePadding、可点行 ≥48dp 等细节见规范文档。

## 5. 协议与兼容

- **提问下发**：沿用 host_call 信封（`emit_raw`，非 `ServerMsg` 成员）——实时问答路径不动 `ServerMsg`。
- **持久化与迟到答案（已拍板 r4 进 S1，需协议扩展；r5 改定语义）**：新增 `ClientMsg::question_reply {requestId, action, answers}` + `SnapshotDto.pending_questions`（重启恢复回放，含 tool_call_id 关联）；升 `PROTOCOL_VERSION` 并同步契约测试。迟到答案**不注入新用户消息**，而是：以答案补全原 tool call 的 tool result 消息（role=tool、tool_call_id 对应），随后从该点续跑 agent 循环——答案在语义上仍属于原问卷的那次工具调用。动机：悬挂 tool call 之后插入 user 消息违反 OpenAI/Anthropic 消息序约束（assistant 的 tool_calls 必须先有对应 tool result）。请求契约采用稳定字段 Δ：`requestId`（幂等主键）、`version`（防回答已撤销/已改的问题）、`status`（pending/answered/declined/superseded/expired，支撑待答收件箱与跨端恢复）。**悬挂修复规则**：新用户消息开启新 turn 前、或问卷 TTL 过期时，对未答问卷的 tool call 以合成结果（superseded / expired）补全后再继续。
- S3 typed form 字段集与 **MCP Elicitation form-mode 的受限 JSON Schema 子集保持兼容**（string/number/integer/boolean/枚举/多选枚举 + email/uri/date/date-time 格式），使 MCP elicitation 直接复用同一 UI Δ。

## 6. 分期计划

### S1（首版 —— 已拍板 r4：含 secret 子系统与持久化三件套）

- Rust：`crates/openslate-mobile/src/askuser.rs` 新模块——`AskUserTool`（impl `Tool`，schema §2），经 `HostCallRouter` 自定义超时；`runtime.rs` `create_with` 注册；审批豁免；PTC 可见性 Both。
- Rust（secret 子系统）Δ：宿主凭据库（加密存储；会话级 + 勾选「记住此凭据」的持久级）；env 注入——shell/bash 工具执行环境从凭据库注入对应变量；file 写入——受限路径写入；工具输出捕获对已知密钥值打码。
- Rust（持久化三件套，自 S2 提前）Δ：问卷持久化（store-sqlite，TTL 24h，requestId/version/status 契约 + tool_call_id 关联）；重启恢复（`SnapshotDto.pending_questions` 回放）；迟到答案补全（`ClientMsg::question_reply` → 以答案补全原 tool call 的 tool result 并从该点续跑 agent 循环——需 runner 支持「从悬挂 tool call 续跑」入口，r5 改定）；悬挂修复（新 turn 开始前 / TTL 过期时，对未答问卷的 tool call 合成 superseded/expired 结果）；协议扩展 + 契约测试 + `PROTOCOL_VERSION` 升级。
- Kotlin：`RuntimeBridge.handleHostCall` 加 `ask_user` 分支 → UiState `pendingQuestion`；`ChatScreen` 挂 `QuestionCard`；新文件 `ui/QuestionCard.kt`（+ QuestionSheet）；整卡 + 逐题 decline。
- Kotlin（设置）Δ：新增「内置工具行为」设置页——ask_user 默认超时（永不/阈值）、输出脱敏开关（可顺带收纳现有 builtin_tools 开关）；凭据管理入口（名称/生效范围/删除）。
- 文档（AGENTS.md 硬规则）：`docs/guide/04-tools/` 新增章节、`docs/guide/06-mobile/call-chains.md`、`docs/guide/02-core/`（协议变更）更新。
- 触碰面：`hostcall.rs`、`runtime.rs`、`openslate-protocol`（+ 契约测试）、`openslate-session`（reply 分发）、store 层、`RuntimeBridge.kt`、`ui/`（新组件 + 设置新页）、凭据存储模块、docs。

### S2（可靠性增强 —— 持久化三件套已提前进 S1）

- timeout → pending（超时不终止问卷：题仍可答，迟到答案按 r5 语义补全原 tool call 结果，deepseek 模式）。
- 待答收件箱 + 深链定位（用户不在 App 内时唤回）。「推送通知」唤回**已由 notify 提案提前至 S1 实施**（见 `docs/proposals/notify-tool-design.md` §4.8 ask_user 伴生唤回通知）。
- 新用户消息 supersession（问题被取代时明确告知模型）。
- 超时自动采纳推荐项（`requiresExplicitResponse` 可禁）。
- 多端竞答 first-response-wins（复用 session dispatch 首答者赢语义）。

### S3（富交互）

- `when` 条件追问链 / typed form（opencode Form 服务形态；字段集兼容 MCP elicitation form-mode）。
- 数字/日期/范围字段（原生数字键盘、日期选择器）；排序/优先级题型。
- 仓库内路径/文件选择器（返回资源引用）。
- 选项图片 / preview；图片附件作为答案。
- 回答后的 follow-up 建议 chips（独立于工具返回值，插入输入框、不自动发送）。
- VerifiedAnswer 式决策记忆。
- 桌面端接入：把 Tool 上移 core + 注入回调 seam（仿 `SessionApprovalBridge` 模式），TUI 自行实现宿主侧。

## 7. 决策记录（已全部拍板）

1. **默认超时（r4）**：用户在「内置工具行为」设置页自选——永不超时 或 阈值；工具入参 `timeout` 逐次覆盖。
2. **decline 粒度（r4）**：整卡级 + 逐题级都有。
3. **工具名（r4）**：`ask_user`。
4. **secret（r3）**：S1 完整实现——env/file 双落地、提交时勾选是否记住、仅引用回填 + 输出脱敏（开关放「内置工具行为」设置页）。
5. **PTC 可见性（r4）**：Both（默认），沙箱内亦可调用。
6. **S1 范围（r4）**：持久化 + 重启恢复 + 迟到答案注入提前进 S1，含协议扩展与 `PROTOCOL_VERSION` 升级；S1 不再是"零协议改动"轻量版。
7. **迟到答案语义（r5）**：不作为新用户消息注入——直接 append 为原 tool call 的 tool result 并续跑 agent 循环（也满足 OpenAI/Anthropic 消息序约束）；悬挂 tool call 在新 turn 前 / TTL 过期时以合成结果（superseded/expired）修复。
