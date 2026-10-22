# 02-2 · openslate-protocol

> **单一文件 crate**（`crates/openslate-protocol/src/lib.rs`，862 行）：server 与 client（TUI/mobile）两侧共用的唯一协议真相源——`ClientMsg`/`ServerMsg` + 全部 DTO。纯数据层，**不依赖 tokio/axum**（Cargo.toml 只有 serde/serde_json/openslate-core），server 与 client 各自装配传输。

## 1. 版本与线格式

```rust
// lib.rs
pub const PROTOCOL_VERSION: u32 = 5;
```

- **v5**（附件 token 响应侧回填）：`AttachmentDto` 新增 `tokens: Option<u64>`（serde default + None 跳键，旧 payload 双向容忍）。值 = **真实模型响应**顺带携带的 image input tokens（OpenAI 系 chat usage `prompt_tokens_details.image_tokens`；provider 不分模态则恒无——智谱 chat usage 天然无此字段）。仅出现在 `EntryDto::User.attachments[]`（transcript 快照 / 持久化条目）；`ClientMsg::Submit` 方向忽略该字段。语义（首次调用口径 / 单图限定 / 智谱恒回退）与回填链路见 `06-mobile/openslate-mobile.md` §12.5——**导入侧绝不预估**（发起 API 请求之前附件数据不上行任何服务器）。
- **v4**（多模态输入，存储 v5）：新增 `AttachmentDto`（附件引用：`{id, mime, filename?}`，与 core `Attachment` 的持久化形状逐字段一致、From/Into 互转，`data` 永不过 wire）；`ClientMsg::Submit` 与 `EntryDto::User` 新增 `attachments: Option<Vec<AttachmentDto>>`（`serde(default)` + None 跳键——旧客户端 / 旧 payload 双向容忍，既有样例字节不变，快照内嵌 `proto` 除外）；`SessionListItemDto` 新增 `input_tokens`/`output_tokens`（v5 runs 聚合列）。图/视频混发等约束由 mobile dispatch 门控（notice 通道），协议层不表达。
- **v3**（tool-approval）：`ApprovalAnswerChoice` 扩为六值（新增 `always_tool`/`session_mcp`/`always_mcp`）；`ApprovalSummaryDto` 新增 `server`/`trigger`/`note`（全 Option + None 跳键，旧 payload 双向容忍）；新增 `ClientMsg::SetApprovalConfig` 与 `ConfigViewDto.approval`（生效规则视图）；`EntryDto::Approval.decision` / `ApprovalResolved.choice` 值域扩展（新增 "always-tool"/"session-mcp"/"always-mcp"）。
- **v2**（ask-user r4/r5）：新增 `ClientMsg::QuestionReply` 与 `SnapshotDto.pending_questions`。
- `hello.proto` 不等即拒连（server 侧发 `Error{code:"proto_mismatch"}` 后断开）。
- **线格式**：每 WS text frame = 一个 JSON 对象，tagged enum（`{"type":"..."}`；`EntryDto` 用 `"kind"`；`ToolStatusDto` 用 `"state"`），字段名 snake_case。
- mobile FFI 复用同一语义：上行 `send(ClientMsg JSON)`，下行 `on_event(ServerMsg JSON)`。**不发明第二套 onToken/onTool 接口**。
- `Box` 仅压 enum 变体尺寸（clippy large_enum_variant），serde 线格式与裸值一致（:582-585 注释）。
- 截断常量：`APPROVAL_SUMMARY_CLIP_CHARS = 120`（按**字符**截断，CJK 安全；单行化 = 去空行 trim 后 join 空格）。

### AttachmentDto（v4 多模态输入；v5 增 `tokens`）

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentDto {
    pub id: String,        // att-<uuid>（store attachments 表主键）
    pub mime: String,      // ingest 归一化后（见下）
    pub filename: Option<String>,
    pub tokens: Option<u64>, // v5：实测 image tokens（响应侧回填；None 跳键）
}
```

- 与 core `Attachment` 的**持久化形状**一致；core 的 `data: Option<Arc<[u8]>>` 为 `#[serde(skip)]`，由 mobile ingest/resume 层经 content-addressed 文件（`files/{sha256}.{ext}`）按 id 装载——DB 行 / wire / transcript 都只有引用。
- `mime` 恒为 ingest 归一化白名单：图像 `image/jpeg|image/png`（webp/gif 已转码 JPEG），视频 `video/mp4|x-matroska|quicktime`——即 wire 发送白名单本身（L3 convert 层无需格式分支）。
- From/Into：`AttachmentDto::from(&Attachment)` 丢 data（tokens 置 None——core 引用形状不知晓测量值）；`Attachment::from(dto)` 置 `data: None`（tokens 亦不入 core 形状，只在协议/展示层流转）。
- `tokens`（v5）：本附件随 Submit 后**第一次**模型调用 usage 的 image tokens（agentic 后续调用重发历史图不计）；多图不拆分（无值）；非导入预估。来源链：vendor 补丁 #3 → genai `Usage::image_input_tokens` → core `Usage::image_tokens` → mobile 回填（06-mobile §12.5）。

## 2. DTO 原则（模块头注释 :12-15）

- core 已有 serde derive 的类型（`Message`/`Usage`/`RunId`/`RunStatus`）→ **直接嵌入**协议。
- core 仅 Deserialize 的 config 类型（`ProviderConfig`/`ModelConfig`）→ 协议侧定义**镜像 DTO + `From` 双向转换**——避免给 core 加 Serialize 耦合。

## 3. 上行 ClientMsg（18 变体，lib.rs:651-752）

| 变体 | 字段 | 语义/约束 |
|------|------|-----------|
| `Hello` | `proto: u32`, `token?: String` | 首条必须；失败即断 |
| `Submit` | `text: String`, `attachments?: Vec<AttachmentDto>`（v4） | 提交一轮用户输入（空白被 session 丢弃）；附件引用经 mobile dispatch 门控（模态/数量/体积/图视频互斥，拒绝走 notice），通过后由 session 构进 user 消息与 transcript 条目 |
| `ApprovalAnswer` | `id: u64`, `choice: ApprovalAnswerChoice` | 任一客户端首答生效；迟到者定向 notice |
| `Cancel` | — | 取消当前回合（复用 TUI Ctrl-C 语义） |
| `NewSession` | — | 清历史/转写、session_id 更新、广播 session_reset |
| `SetModel` | `alias: String` | 别名可解析性校验后热切换 |
| `UpsertProvider` | `name`, `provider: ProviderDto` | 写全局库（无则 active） |
| `DeleteProvider` | `name` | |
| `UpsertModel` | `entry`, `model: ModelDto` | |
| `DeleteModel` | `entry` | 删被引用条目放行（悬空合法，松耦合） |
| `SetLevel` | `level`, `entry` | 写 **active** |
| `DeleteLevel` | `level` | |
| `SetCapability` | `capability`, `alias` | capability ∈ main/compact/title/review（v3 新增 review=审批审查模型）；== "main" 时同时热切会话模型 |
| `SetApprovalConfig` | `default_level?: String`, `tool_levels: BTreeMap<String,String>`, `server_levels: BTreeMap<String,String>` | 工具审批规则写入（v3，设置页「工具权限」）：server **整段重写** `[approval]` 为新 schema（删 legacy `policy`/`tools` 键）→ 重载 → rules 热更 → 广播 config_changed。level 值域 `"manual"` / `"allow"` / `"auto"`；`default_level=None` = 缺省（allow）；`server_levels` 键 = 审批作用域 slug（core `mcp::server_slug_of` 对齐）；空 map 恒发送（Rust `serde(default)` 接受 `{}`） |
| `UpsertMcpServer` | `name`, `url`, `headers?: HashMap` | Streamable HTTP 条目（手机端 mcp-host 场景） |
| `RemoveMcpServer` | `name` | 不存在静默成功（幂等） |
| `SetApiKey` | `provider`, `value` | 派生 `<NAME>_API_KEY` 写 `.env`（0600），不回显不广播。**mobile 上不走此路径**（FFI `set_api_key` 内存注入 + Keystore，PLAN §18） |
| `QuestionReply` | `request_id`, `action: QuestionReplyAction`, `answers: serde_json::Value` | ask_user 问卷应答（即时/迟到，首答者赢同 approval_answer）。`action` ∈ accept/decline（对齐 MCP Elicitation）；`answers` 结构由 mobile 层定义、协议透传 |

`ApprovalAnswerChoice`（:122）**六值**（snake_case，v3 扩展）：`approve`（单次放行，TUI y / 兜底保留）/ `deny`（禁止执行，选项 (e)）/ `approve_all`（选项 (a) 本会话允许该工具——写会话级工具集合）/ `always_tool`（(b) 一直允许该工具——持久化 `tool_levels=allow`）/ `session_mcp`（(c) 本会话允许该 MCP server——写会话级 server 集合）/ `always_mcp`（(d) 一直允许该 MCP——持久化 `server_levels=allow`）。mobile UI 只出后五项语义化入口；会话侧各 choice 落点见 [openslate-session.md](openslate-session.md) §3/§5。

## 4. 下行 ServerMsg（21 变体，lib.rs:764-862）

同一连接内事件**全序**（单发送队列）：

| 变体 | 载荷 | 语义 |
|------|------|------|
| `Snapshot` | `session: Box<SnapshotDto>` | hello 成功 / session_reset 后逐连接定向 |
| `RequestStart` | `step: u32`, `model: String` | |
| `FirstToken` | — | TTFT |
| `Delta` | `text` | 答案流式增量 |
| `Reasoning` | `text` | 思维链流式增量 |
| `InputEstimate` | `tokens: u32` | 流式期 ↑N 估算 |
| `Usage` | `usage: Usage` | 单次请求精确 usage（core 类型直嵌） |
| `RequestEnd` | — | |
| `StepEnd` | — | 步进分隔 |
| `ToolStart` | `name`, `args`, `call_id?` | live 携带 provider 分配的 call_id（客户端精确配对条目 / 懒加载全量输出）；旧版本事件无此字段（None，客户端按名字 + FIFO 兜底） |
| `ToolEnd` | `name`, `bytes`, `truncated`, `preview?`, `call_id?` | 失败与否由回合结束的 Tool 消息折叠判定；preview None = 无输出；call_id 同 `ToolStart` |
| `ApprovalRequested` | `id: u64`, `summary: ApprovalSummaryDto` | v3：summary 携带 `server?`（MCP 作用域）/`trigger?`（"manual"/"auto_risk"）/`note?`（AI 审查理由，manual 触发无）；UI 的 (c)/(d) MCP 级选项据 server 显示 |
| `ApprovalResolved` | `id`, `choice: String` | 首答后广播，所有客户端清横幅。choice 值域 = approve/deny/approve_all + v3 新增连字符拼写 "always-tool"/"session-mcp"/"always-mcp"（由 session 层 `choice_str` 产生） |
| `TurnOk` | `summary: Box<TurnSummaryDto>` | 回合终态 |
| `TurnError` | `message` | |
| `ConfigChanged` | `config: Box<ConfigViewDto>` | 配置 CRUD 成功后广播新视图 |
| `ModelChanged` | `alias` | |
| `RunTitle` | `run_id: String`, `title` | 异步 LLM 标题替换占位；历史列表按 run_id 原位更新 |
| `SessionReset` | — | |
| `Notice` | `text`, `level: NoticeLevel`（info/warn/error，仅影响着色） | |
| `Error` | `code`, `message` | 协议级错误：`proto_mismatch`/`bad_token`/`bad_message`，定向发送后断连 |

`ServerMsg`/`TurnOk` **不 derive PartialEq**（内嵌 core `Message` 无它）——测试用 `serde_json::to_value` 比对。

## 5. DTO 家族

### SnapshotDto（:200）

hello 成功后定向发送的全量快照：`proto`/`session_id`/`session_label`/`run_id: Option<String>`（当前会话 run 的键，`session_run` 投影，无持久化 run → `None`——mobile Kotlin 转写镜像的落盘/恢复键用它对齐事件泵 FIFO，替代 FFI 旁路直读实时态；`serde(default)`：旧快照缺键 → `None`，旧客户端忽略未知字段，双向兼容）/`transcript: Vec<EntryDto>`/`running`/`depth_cur`/`agents_running`/`tool_calls_cur`/`model_alias`/`pending_approval?`/`pending_questions: Vec<PendingQuestionDto>`（v2，`serde(default)` 向后兼容；server 投影恒空，mobile 装配层按持久层回填——重启恢复回放通路）/`config: ConfigViewDto`/`session_stats: SessionStatsDto`（:220——server 是统计真相源，重连/新客户端 attach 不清零；`new_session` 归零）。

### EntryDto（tag "kind"）

转写条目，字段与 TUI `TranscriptEntry` 一一对应：

- `User{text, attachments?}`（v4：附件引用，transcript 缩略图渲染用）/ `Assistant{text}` / `Reasoning{text}` / `StepBreak` / `Meta{text}`（usage 行）
- `ToolCall{name, args, call_id?, status: ToolStatusDto, detail: ToolEntryDetailDto}`
  - `call_id`：live 事件（`on_tool_start`）与 DB 重建条目均携带；旧版本 live 条目为 None（折叠配对回退名字 + FIFO）
  - `ToolStatusDto`（:101，tag "state"）：`running` / `done{bytes, truncated, elapsed_ms?}` / `failed{summary}`。**live 迁移仅 Running→Done；`failed` 只由回合结束的折叠启发式产生**（:97-98）
  - `ToolEntryDetailDto`（:117）：`args`（完整调用参数 pretty JSON）+ `output?`（live 期为 None，回合结束从 Tool 消息回填，8KB 留存上限）
- `Approval{tool_name, decision}`：decision ∈ "approved"/"denied"/"approve-all" + v3 新增 "always-tool"/"session-mcp"/"always-mcp"（六 choice 一一对应）
- `Delegate{agent, done}`：call_agent 的标记（spec D2：子代理内容不进转写）

### project_from_messages（消息投影 → 转写条目，存储 v5 §4 / L6）

`pub fn project_from_messages(messages: &[Message]) -> Vec<EntryDto>`（lib.rs，EntryDto 定义后的独立区段）——消息历史 → 转写条目的**共享投影**，原 mobile `messages_to_transcript` 的搬迁（行为逐条对齐）。入参直接用 core `Message`（protocol 本就依赖 core，零新增依赖）。

- **User** → `User` 条目，附件随行**引用形状**（`AttachmentDto::from(&Attachment)`——`data` 永不出现）
- **Assistant** → 非空 `reasoning_content` 先出 `Reasoning`，再出 `tool_calls` 的 `ToolCall`（折叠终值 `Done{bytes:0, truncated:false, elapsed_ms:None}`；顶层 `args` 预览截 40 字符、`detail.args` 全文），最后非空白 `content` 出 `Assistant`
- **Tool** → 不产生条目：输出（前 2000 字符）并入最近一条匹配的 `ToolCall`（`tool_call_id` 优先，无 id 按名兜底）；悬挂 tool_calls 保持 Done 零字节形态
- **System** 跳过；**Approval/Delegate/StepBreak/Meta 不可由消息投影**——只存在于 `transcript_entries` 行或 live 事件（两级重建优先 entries 表的原因）
- 消费方：mobile 两级重建回退路径（`transcripts::rebuild_transcript` → runtime 侧薄委托 `messages_to_transcript`）；TUI `entry_from_message` 是同型独立实现（条目枚举不同，架构合并记 follow-up）
- 快照测试 `tests/project.rs`：投影语义按 `serde_json::Value` 全量钉死（附件引用 / reasoning 还原 / call_id 与名字配对折叠 / 悬挂 tool_calls / 空 content / System 跳过 / 40 与 2000 字符截断 / 不可投影四类的缺席）


### TurnSummaryDto（:172）

回合终态：run_id/status/total_steps/total_input_tokens/total_output_tokens/total_cost_usd/model/`messages: Vec<Message>`。`From<ManagedRunResult>`（:183）。**execution_tree 不进协议**（客户端用它重建折叠 Tool 消息，不需要执行树）。

### ConfigViewDto（:558）

配置只读视图（providers/models/levels/capabilities/limits/agents 树/skills/三配置路径 + approval 生效规则）。**BTreeMap 保证 key 有序**（快照 diff / 契约测试稳定）。capabilities 的 `serde(default)`：旧客户端反序列化新快照/新客户端反序列化旧快照都安全。**客户端是零本地配置的纯消费者，本视图必须自足**。

- `approval?: ApprovalConfigViewDto`（v3，tool-approval）：**生效规则视图——legacy `policy`/`tools` 已并入的映射后真相**（非 toml 原文），由 session 层 `build_config_view` 经 core `rules_from_config` 投影（bridge rules cell 快照）。`serde(default)` + None 跳键（旧快照/旧客户端双向容忍）。
- `ApprovalConfigViewDto`（:548）：`{default_level: String, tool_levels: BTreeMap<String,String>, server_levels: BTreeMap<String,String>}`——level 值域同 `SetApprovalConfig`（"manual"/"allow"/"auto"）；mobile 设置页「工具权限」tab 的唯一数据源（config_changed 自动回证，无第二真相源）。

- `ProviderDto`（:244）：base_url/api_key_env/adapter?/title?/max_attempts/retry_base_ms；`From<&ProviderConfig>` :259 + `From<ProviderDto> for ProviderConfig` :272 双向。
- `ModelDto`（:287）：provider/model/限额/能力布尔/计价/**reasoning_effort**（`Option<String>` 七档含 none=显式关闭，`skip_serializing_if` None 跳键——wire 契约零变化）；双向 From :306/:322。
- `LimitsDto`（:348）：`From<&LimitsConfig>` :361——server 侧已解析默认值（Option 语义解掉）。含 **`output_store_threshold`**（C2 additive 字段，`#[serde(default = …)]` = 16384：旧 server 载荷缺键时客户端镜像取配置默认值，proto 不 bump）。
- `AgentNodeDto`（:381）：`From<&AgentTree>` :388 递归展开，children 顺序 = 配置声明序。
- `SkillInfoDto`（:340）：name + description（tier-1 渐进披露）。

### 审批 DTO

- `ApprovalSummaryDto`（:49）：tool_name / arguments（单行化 + 120 字符截断）/ agent_id / risk_level（"low"/"medium"/"high"）+ v3 新增 `server?`（MCP 作用域 slug / host 前缀；非 MCP 无此键）/ `trigger?`（"manual" = 等级直问 / "auto_risk" = Auto 等级启发式风险命中，可能经 AI 审查后才弹人）/ `note?`（AI 审查理由「AI 审查判定高风险：…」，manual 触发无）。新字段全 `Option` + `skip_serializing_if`——v2 形状 payload 双向容忍。`From<&ApprovalRequest>`（:90）。
- `PendingApprovalDto`（:109）：id + summary（snapshot 内嵌）。

### ask_user 问卷 DTO（v2，ask-user r4/r5）

- `QuestionReplyAction`（snake_case）：`accept` / `decline`（cancel 不在此表达——用户打断走回合取消通道）。
- `QuestionStatus`（snake_case）：`pending` / `answered` / `declined` / `superseded` / `expired`（cancelled 随 Tool 消息折叠，不进枚举）。
- `PendingQuestionDto`：`request_id`（幂等主键，host call id）/ `tool_call_id`（关联悬挂 tool call——迟到答案按 r5 补全其 tool result 并续跑，**不注入新用户消息**）/ `questions: serde_json::Value`（透传）/ `status` / `version`（防回答已撤销/已改的问题）。

### REST 辅助

- `SessionListItemDto`：`/api/sessions` 的 run 行投影；v4 起含 `input_tokens`/`output_tokens`（存储 v5 runs 聚合列，Option + None 跳键，存量 run 无采集时不出现或为 0——server 投影恒 Some）。
- `NoticeLevel`。

## 6. server 自描述文件

```rust
// lib.rs:465
pub const SERVER_INFO_FILE: &str = "server.json";
```

- `ServerInfo`（:471）：proto/url（完整 WS 路径）/pid/port/bind/started_at（ISO 8601 本地时区）/`token?`（`--auth-token` 值——文件权限 0600 的原因）/cwd。
- 生命周期：server 启动落盘 → TUI 无 `--server` 时读取自动 attach（发现链 **flag > env > server.json**）→ 优雅停机时删除。读侧 proto 不符视为陈旧文件。**防双启动靠对 url 的 health 探测而非 pid 校验**（pid 会复用，:469-470 注释）。

## 7. 契约测试

`crates/openslate-protocol/tests/contract.rs` **逐变体固化样例字节**。变更流程（红线）：

1. 任何字段增删改名 → 同步契约测试样例；
2. 升 `PROTOCOL_VERSION`；
3. 否则旧客户端在 hello 阶段直接拒连。

v3 的审批扩展样例（contract.rs「审批扩展 v3」区）：`approval_requested` 新字段（server/trigger/note）、`approval_resolved` 连字符新 choice、`approval_answer` 六 choice 序列化、`set_approval_config` 新旧 schema 双向（`default_level` 缺省、空 map、全字段三形态）、`EntryDto::Approval` 新 decision 值。

v4 的多模态样例（contract.rs「v4」区）：带附件 `submit`（filename 有/无两形态 + 旧格式反序列化 → None + 空数组容忍）、`EntryDto::User` 带附件、`AttachmentDto` ↔ core `Attachment` 互转（data 不可见）。既有样例字节不变——唯一例外是快照样例内嵌的 `"proto"` 值随版本号走。v5 补样例：`EntryDto::User` 附件带 `tokens`（wire `"tokens":728`；None 跳键样例由 v4 附件样例继续覆盖——字节不变），`protocol_version_pinned` 钉 5。

`tests/project.rs`（L6）：`project_from_messages` 的投影语义快照（`serde_json::Value` 全量钉死，见 §5 该条目）——改投影行为（条目顺序 / 截断上限 / 配对规则）会在此红灯。

`schema.rs` 的 CHECK 约束测试（store 侧）同属"防漂移"家族：协议侧靠契约测试钉死线格式与文档语义的一致性。

## 8. 设计决策速记

| 决策 | 理由 |
|------|------|
| 单文件 crate | 协议是高频同步点，单文件降低 merge 冲突面 |
| 不依赖 tokio/axum | TUI（client 侧）也要用，不能带进 server 专属依赖 |
| core 类型直嵌 vs 镜像 DTO | 已有双向 serde 的直嵌省一层；仅 Deserialize 的镜像，避免 core 加 Serialize 耦合 |
| Box 压尺寸 | clippy large_enum_variant；线格式无差 |
| 不 derive PartialEq | core `Message` 没有它；测试用 serde_json::to_value 比对 |
| 截断按字符 | CJK 安全（测试设备中文内容多） |
