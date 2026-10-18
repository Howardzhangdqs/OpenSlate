# 02-2 · openslate-protocol

> **单一文件 crate**（`crates/openslate-protocol/src/lib.rs`，669 行）：server 与 client（TUI/mobile）两侧共用的唯一协议真相源——`ClientMsg`/`ServerMsg` + 全部 DTO。纯数据层，**不依赖 tokio/axum**（Cargo.toml 只有 serde/serde_json/openslate-core），server 与 client 各自装配传输。

## 1. 版本与线格式

```rust
// lib.rs:25
pub const PROTOCOL_VERSION: u32 = 1;
```

- `hello.proto` 不等即拒连（server 侧发 `Error{code:"proto_mismatch"}` 后断开）。
- **线格式**：每 WS text frame = 一个 JSON 对象，tagged enum（`{"type":"..."}`；`EntryDto` 用 `"kind"`；`ToolStatusDto` 用 `"state"`），字段名 snake_case。
- mobile FFI 复用同一语义：上行 `send(ClientMsg JSON)`，下行 `on_event(ServerMsg JSON)`。**不发明第二套 onToken/onTool 接口**。
- `Box` 仅压 enum 变体尺寸（clippy large_enum_variant），serde 线格式与裸值一致（:582-585 注释）。
- 截断常量：`APPROVAL_SUMMARY_CLIP_CHARS = 120`（:43，按**字符**截断，CJK 安全；单行化 = 去空行 trim 后 join 空格，:46-59）。

## 2. DTO 原则（模块头注释 :12-15）

- core 已有 serde derive 的类型（`Message`/`Usage`/`RunId`/`RunStatus`）→ **直接嵌入**协议。
- core 仅 Deserialize 的 config 类型（`ProviderConfig`/`ModelConfig`）→ 协议侧定义**镜像 DTO + `From` 双向转换**——避免给 core 加 Serialize 耦合。

## 3. 上行 ClientMsg（16 变体，lib.rs:497-568）

| 变体 | 字段 | 语义/约束 |
|------|------|-----------|
| `Hello` | `proto: u32`, `token?: String` | 首条必须；失败即断 |
| `Submit` | `text: String` | 提交一轮用户输入（空白被 session 丢弃） |
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
| `SetCapability` | `capability`, `alias` | capability ∈ main/compact/title；== "main" 时同时热切会话模型 |
| `UpsertMcpServer` | `name`, `url`, `headers?: HashMap` | Streamable HTTP 条目（手机端 mcp-host 场景） |
| `RemoveMcpServer` | `name` | 不存在静默成功（幂等） |
| `SetApiKey` | `provider`, `value` | 派生 `<NAME>_API_KEY` 写 `.env`（0600），不回显不广播。**mobile 上不走此路径**（FFI `set_api_key` 内存注入 + Keystore，PLAN §18） |

`ApprovalAnswerChoice`（:85）= `approve` / `deny` / `approve_all`（snake_case，本会话后续审批自动放行）。

## 4. 下行 ServerMsg（21 变体，lib.rs:580-669）

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
| `ToolStart` | `name`, `args` | **live 不带 call_id**（core 的 on_tool_start 不传） |
| `ToolEnd` | `name`, `bytes`, `truncated`, `preview?` | 失败与否由回合结束的 Tool 消息折叠判定；preview None = 无输出 |
| `ApprovalRequested` | `id: u64`, `summary: ApprovalSummaryDto` | |
| `ApprovalResolved` | `id`, `choice: String`（approve/deny/approve_all） | 首答后广播，所有客户端清横幅 |
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

hello 成功后定向发送的全量快照：`proto`/`session_id`/`session_label`/`transcript: Vec<EntryDto>`/`running`/`depth_cur`/`agents_running`/`tool_calls_cur`/`model_alias`/`pending_approval?`/`config: ConfigViewDto`/`session_stats: SessionStatsDto`（:220——server 是统计真相源，重连/新客户端 attach 不清零；`new_session` 归零）。

### EntryDto（:128，tag "kind"）

转写条目，字段与 TUI `TranscriptEntry` 一一对应：

- `User{text}` / `Assistant{text}` / `Reasoning{text}` / `StepBreak` / `Meta{text}`（usage 行）
- `ToolCall{name, args, call_id?, status: ToolStatusDto, detail: ToolEntryDetailDto}`
  - `call_id` live 期恒 None；客户端从 `TurnSummaryDto.messages` 重建折叠时可回填（:141-144）
  - `ToolStatusDto`（:101，tag "state"）：`running` / `done{bytes, truncated, elapsed_ms?}` / `failed{summary}`。**live 迁移仅 Running→Done；`failed` 只由回合结束的折叠启发式产生**（:97-98）
  - `ToolEntryDetailDto`（:117）：`args`（完整调用参数 pretty JSON）+ `output?`（live 期为 None，回合结束从 Tool 消息回填，8KB 留存上限）
- `Approval{tool_name, decision}`：decision ∈ "approved"/"denied"/"approve-all"
- `Delegate{agent, done}`：call_agent 的标记（spec D2：子代理内容不进转写）

### TurnSummaryDto（:172）

回合终态：run_id/status/total_steps/total_input_tokens/total_output_tokens/total_cost_usd/model/`messages: Vec<Message>`。`From<ManagedRunResult>`（:183）。**execution_tree 不进协议**（客户端用它重建折叠 Tool 消息，不需要执行树）。

### ConfigViewDto（:409）

配置只读视图（providers/models/levels/capabilities/limits/agents 树/skills/三配置路径）。**BTreeMap 保证 key 有序**（快照 diff / 契约测试稳定）。capabilities 的 `serde(default)`（:417-418）：旧客户端反序列化新快照/新客户端反序列化旧快照都安全。**客户端是零本地配置的纯消费者，本视图必须自足**。

- `ProviderDto`（:244）：base_url/api_key_env/adapter?/title?/max_attempts/retry_base_ms；`From<&ProviderConfig>` :259 + `From<ProviderDto> for ProviderConfig` :272 双向。
- `ModelDto`（:287）：provider/model/限额/能力布尔/计价/**reasoning_effort**（`Option<String>` 七档含 none=显式关闭，`skip_serializing_if` None 跳键——wire 契约零变化）；双向 From :306/:322。
- `LimitsDto`（:348）：`From<&LimitsConfig>` :361——server 侧已解析默认值（Option 语义解掉）。
- `AgentNodeDto`（:381）：`From<&AgentTree>` :388 递归展开，children 顺序 = 配置声明序。
- `SkillInfoDto`（:340）：name + description（tier-1 渐进披露）。

### 审批 DTO

- `ApprovalSummaryDto`（:34）：tool_name/arguments（单行化 + 120 字符截断）/agent_id/risk_level（"low"/"medium"/"high"）。`From<&ApprovalRequest>` :61。
- `PendingApprovalDto`（:77）：id + summary（snapshot 内嵌）。

### REST 辅助

- `SessionListItemDto`（:440）：`/api/sessions` 的 run 行投影。
- `NoticeLevel`（:453）。

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
