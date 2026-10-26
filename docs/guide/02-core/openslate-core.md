# 02-1 · openslate-core 模块详解

> 领域核心 crate（`crates/openslate-core/src/`，lib.rs 声明 22 个顶层模块，其中 `config/` 与 `context_manager/` 是目录模块——按 21 个文件组逐一详解）。**不依赖任何前端/传输**；workspace 内仅依赖 `openslate-mcp-builtin` 与 `openslate-ptc`。模块声明见 `src/lib.rs:1-22`；`prelude` 重导出 callable/config/context_manager/error/paths/provider/types（lib.rs:23-31）。

## 模块地图

| # | 文件 | 职责 |
|---|------|------|
| 1 | `types.rs` | 领域 ID 与数据结构 |
| 2 | `provider.rs` | 模型调用抽象 + 进度回调 + 远端清单 |
| 3 | `tool.rs` | 工具抽象与注册表 |
| 4 | `callable.rs` | 工具与子代理公共基座 |
| 5 | `run_manager.rs` | run 编排入口 |
| 6 | `runner.rs` | 递归子代理编排器 |
| 7 | `runtime.rs` | 单 agent 主循环 |
| 8 | `config/` | 配置解析/合并/写回/校验 |
| 9 | `model_config.rs` | 别名解析（levels/capabilities） |
| 10 | `model_registry.rs` | LLM 元数据 registry（纯逻辑） |
| 11 | `mcp.rs` | MCP 接入 |
| 12 | `agent_tree.rs` | 静态配置树 |
| 13 | `execution.rs` | 运行时执行树 |
| 14 | `context.rs` | 子代理上下文隔离 |
| 15 | `context_manager/` | 会话窗口管理 + 压缩 |
| 16 | `approval.rs` | 审批闸门（规则引擎） |
| 17 | `skills.rs` | SKILL.md 发现与注入 |
| 18 | `prompt.rs` | 提示词解析（profile 体系） |
| 19 | `paths.rs` | 路径发现 |
| 20 | `trace.rs` + `error.rs` | Chrome Trace 导出 / 错误类型 |
| 21 | `ask_user.rs` | ask_user 消息历史手术与合成文案（迟到答案/悬挂修复） |
| 22 | `output_store.rs` | 长输出落盘接缝（ToolOutputStorer 注册槽 + 预览构造，C2） |

---

## 1. types.rs —— 领域 ID 与数据结构（589 行）

- `id_type!` 宏（:6-44）生成 newtype String 家族：`AgentId`/`RunId`/`StepId`/`ExecutionNodeId`/`ToolCallId`（:46-56）。均 derive `Debug/Clone/PartialEq/Eq/Hash/Serialize/Deserialize`，实现 `Display/FromStr(Infallible)/From<String>/From<&str>/AsRef<str>`。
- `MessageRole`（:61）= system/user/assistant/tool（serde lowercase）。
- `Message`（:70）：`role`/`content`/`tool_call_id?`/`name?`/`tool_calls?: Vec<ToolCall>`/`reasoning_content?`/`attachments?: Vec<Attachment>`。`reasoning_content` 是 Anthropic 多轮 extended-thinking 语义：历史 thinking 块随 assistant 消息回传，持久化自动携带（依赖 vendor 过的 genai）。`attachments` 是多模态输入侧 additive 字段（:87）：`#[serde(default, skip_serializing_if = "Option::is_none")]`——旧 payload（无该键）反序列化为 `None`，`None` 序列化时省略键，存量 wire/持久化行为不变（与 `reasoning_content` 同款兼容策略）。
- `Attachment`（:94，多模态附件条目）：

  | 字段 | 类型 | serde 行为 |
  |------|------|-----------|
  | `id` | `String` | 恒序列化（`att-<uuid>` 引用键） |
  | `mime` | `String` | 恒序列化（归一化后 mime） |
  | `filename` | `Option<String>` | `default` + None 省略 |
  | `data` | `Option<Arc<[u8]>>` | **`#[serde(skip, default)]`：永不序列化，反序列化恒 `None`** |

  derive 仅 `Debug/Clone/Serialize/Deserialize`（无 `PartialEq`）。**`data` 不过持久化线**：持久化形状 = `{id, mime, filename}` 引用（DB 与协议 wire 同形，字节本体走 content-addressed 文件系统）；`data` 只在内存中直达 provider——ingest 层填充、resume 时由 mobile 层从 store+files 回填，即使 JSON 里出现 `data` 键反序列化也会丢弃。设计依据：`docs/PLAN-storage-v5-multimodal.md` §6.1。
- `AgentConfig`（:105，字段语义见 [../01-architecture/domain-concepts.md](../01-architecture/domain-concepts.md)）。
- `RunStatus`（:124）= running/completed/failed/interrupted/cancelled。
- `StepKind(pub String)`（:134）+ 常量 `MODEL_CALL`/`TOOL_CALL`/`CHILD_AGENT_CALL`（:138-142）。
- `ModelResponse`（:147）：content?/reasoning_content?/tool_calls/usage?/finish_reason?。
- `ToolCall`（:160）：id/name/arguments(serde_json::Value)。
- `Usage`（:168）：input/output_tokens(u32) + **additive** 字段 `cached_input_tokens?`/`reasoning_tokens?`/`image_tokens?: Option<u64>`（`#[serde(default)]`——旧 payload 反序列化为 None；cost 计算刻意忽略它们：cached 不重复计价，reasoning 已含在 output_tokens 里，image 已含在 input_tokens 里）。`image_tokens` = image input tokens 的模态拆分（OpenAI 系 chat usage `prompt_tokens_details.image_tokens`，经 vendor 补丁 #3 + model-genai 映射；provider 不分模态则 None），落 `messages.meta_json` 供 mobile 重开时回放附件 token（06-mobile §12.5）。
- `MessageMeta`（:194，单条消息采集元数据，derive 含 `Default`）：

  | 字段 | 类型 | serde 行为 |
  |------|------|-----------|
  | `model_id` | `Option<String>` | default + None 省略 |
  | `usage` | `Option<Usage>` | default + None 省略 |
  | `finish_reason` | `Option<String>` | default + None 省略 |
  | `latency_ms` | `Option<u64>` | default + None 省略 |

  **不进 `Message` 本体**（Message 是 provider 回放单元，混入 usage 会在 convert/resume 回放路径泄漏给 provider）；assistant 消息专属，旁路落 store 的 `messages.meta_json`（v5），user/tool 行为 NULL。全空 `{}` 反序列化为全 `None`。设计依据：`docs/PLAN-storage-v5-multimodal.md` §2。
- `ModelStreamEvent`（:207）：`Delta`/`Reasoning`/`Usage`/`Done(ModelResponse)`。
- `ToolOutput`（:223）：content/bytes/duration_ms/status（`ToolOutputStatus` = success/error/truncated，:233）
  + `attachments: Option<Vec<Attachment>>`（读图链路 2026-10-21，additive——旧 payload 反序列化
  `None`、`None` 序列化省略）。MCP 工具返回 Image content block 时，`mcp::downgrade_content_blocks`
  把 base64 解码为字节收集于此（id `tool-{n}`、filename None、**不落 attachments 表**——与用户
  ingest 附件的 DB 行无关）；runtime 把它透传进 Tool Message 的 `attachments`（顺序/并行两路径，
  runtime.rs 的 `attachments: output.attachments`），provider 层 1:N 展开（genai convert）。
  `limit_tool_output` 截断时附件原样保留（截断只作用文本）。
- serde 兼容锁定：types.rs 内测试覆盖旧 payload（无 `attachments`）→ `None`、`Message`/`Attachment`/`MessageMeta` 往返、`data` 永不出现于序列化输出（`message_old_payload_without_attachments`/`message_attachments_roundtrip`/`attachment_data_never_serialized`/`message_meta_serde_roundtrip`）。

## 2. provider.rs —— 模型调用抽象（275 行）

- `ToolDefinition`（:5）= name/description/parameters。
- `GenerateRequest`（:12）= model_id/system_prompt?/messages/tools/max_tokens?/temperature?。
- `ModelProvider` trait（:22，详见 [../01-architecture/traits-and-abstractions.md](../01-architecture/traits-and-abstractions.md) §1）。
- `ProgressCallback` trait（:57，10 个回调，`on_input_estimate`/`on_reasoning`/`on_step_end` 默认 no-op 保持向后兼容）。`on_tool_start(name, args, call_id)` / `on_tool_end(name, bytes, truncated, preview, call_id)` 的 `call_id` 为 provider 分配的工具调用 id（`ToolCallId` newtype 内串），runtime 4 个发射点（串行/并行批 × start/end）均传 `&tc.id.0`；UI 层按其精确配对 start/end 与条目、并懒加载全量输出。`on_tool_start` 的 args 经 `tool_args_for_event`（runtime.rs）按工具名分级：`ask_user` / `bash` / `termux_bash` **全量下发**（ask_user 供 Android 回看页重建问卷；bash 类参数是单 command 字符串，UI 需完整 JSON 才能解析命令渲染 Termux 芯片，截断会破坏 JSON 合法性），其余工具 120 字符预览（write 等大参数工具防事件洪泛）；日志路径恒用截断预览。
- `list_remote_models(base_url, adapter, api_key)`（:104）：Provider 设置页"自动检测"。端点/鉴权矩阵（:96-103 注释）：openai `GET {base}/models` Bearer；anthropic `{base}/v1/models` x-api-key + anthropic-version（base 已带 `/v1` 后缀则不重复拼，:142-148）；gemini `{base}/v1beta/models` x-goog-api-key（剥已有后缀，:149-152）；ollama `/api/tags` 无鉴权。10s 超时；响应解析宽松（`data[].id` / `models[].name` / `models[].id` 都认，Gemini name 剥 `models/` 前缀，:180-198）；空结果 bail。Android 下 webpki 静态根证书（:117-128）；代理语义：`OPENSLATE_HTTP_PROXY` 显式启用否则 no_proxy（:131-138）。

## 3. tool.rs —— 工具抽象与注册表（708 行）

- `Tool` trait（:17，含 `namespace()` PTC 暴露路径 `tools.<ns>.<method>`，:21-28）。
- `ToolExecutor`（:54）：runtime 消费的执行抽象（约定内部错误转 Error ToolOutput）。
- `ToolRegistry`（:66）：`RwLock<HashMap<String, Arc<dyn Tool>>>`。API：
  - `register`（:83 静默覆盖）/ `register_boxed`（:93，mobile host 工具注入接缝）/ `try_register`（:116，报 `ToolNameConflict(pub String)` :237）/ `unregister`（:101，热换名）/ `get` / `definitions` / `definitions_for(patterns)`（:152 glob 白名单）/ `definitions_visible(allow, hidden)`（:174，**tools 白名单与 hidden_tools 的唯一交汇点**）/ `tool_names` / `execute`（:203，NotFound 报 `ToolError::NotFound`）/ `contains`。
  - 名字匹配（`tool_name_matches` :242）：无 `*`/`?` 精确，否则 glob（`wildcard_match` :249；`*` 任意字符 run、`?` 恰一个）。
  - 返回顺序跟随 registry 内部（HashMap）顺序，非 pattern 序。
- `limit_tool_output` + `ToolAuditRecord` + `create_audit_record`（caller=direct/run_code 归因，供 runner 的审计日志）。

## 4. callable.rs —— 公共基座（300 行）

- `CallableKind`（:11）= Tool | ChildAgent；`Callable` trait（:21）。
- `CALL_AGENT_SCHEMA` 常量（:36）。
- `ChildAgentCallable`（:50）：实现 `Tool`，name 恒 `"call_agent"`；schema 的 enum 字段钉死只允许自己的 agent_id（:82-98）；`execute` 是**桩**（:100 注释钉死——真正执行由 runner 拦截）。
- `child_agent_definitions(child_ids, agent_tree)`（:119）：为树中存在的子代理生成 ToolDefinition。

## 5. run_manager.rs —— run 编排入口（1231 行，测试占大半）

```rust
// :23
pub struct RunManager {
    pub config: OpenSlateConfig,
    pub agent_tree: AgentTree,
    pub tool_registry: Arc<ToolRegistry>,
    pub skills: SkillsCatalog,
    pub limits: RuntimeLimits,
    pub approval: ApprovalManager,          // 从 [approval] 初始化；CLI 层覆盖
    pub message_sink: Option<Arc<dyn MessageSink>>,  // CLI/session 层注入 RunRecorder
}
```

- `new`（:66）：`RuntimeLimits::from_config` + `ApprovalConfig::to_policy()` 派生 approval。
- `new_run_id()`（:94）：UUID v4。
- `execute_with_run_id(run_id, provider, prior_messages, cancel, progress) -> Result<ManagedRunResult>`（:118）：resolve root agent → TraceCollector 开 run span → 组 `AgentRunner`（快照 approval/sink/cancel）→ `run_root`。`prior_messages` 直接作 initial_messages（当前用户消息必须已在其中，不追加）；入口前已取消的 token 报 `RuntimeError::Cancelled`。
- `resume_from_tool_result(run_id, provider, prior_messages, tool_call_id, result_content, cancel, progress)`（ask-user r5）：**迟到答案续跑入口**——先 `ask_user::append_late_tool_result` 补全悬挂 tool call 的 tool result（经 message_sink 先落盘），再走同一 `execute_with_run_id` 从该点重进模型循环；对模型与「答案即时返回」完全等价（迟到等价不变量）。幂等：已答/未知 id 报 `OpenSlateError::Validation`。
- `execute_with_history`（:187）/`execute`（:243）：便捷包装。
- `ManagedRunResult`（:46）：run_id/status/messages/total_steps/total_input_tokens/total_output_tokens/total_cost_usd（各层按各自模型计价后聚合）/execution_tree/model/trace。**`total_*_tokens` 是 run 生命周期 token 聚合的权威来源**（v5 L2）：前端 finish 时经 `RunFinal` 落 `runs.input_tokens/output_tokens`（见 [openslate-session.md](openslate-session.md)）。

## 6. runner.rs —— 递归子代理编排器（3981 行，测试占大半）

- `CallerFrame`（:66）：`agent_id/exec_id/depth/messages_snapshot`。**进入时快照**——多层委派传的是各层初始上下文，非实时状态；accepted trade-off 换 `execute_run` 不改。`FrameGuard`（:159）RAII 弹栈，异常路径栈也平衡。
- `AgentRunner<'a>`（:86）字段组（:87-155）：
  - 借用：provider/agent_tree/tool_registry/config；
  - 快照：limits/context_config/skills_section/ptc/approval/message_sink/cancel_token；
  - 内部可变（`&self` 执行需 interior mutability）：`execution_tree: Mutex` / `caller_stack: Mutex<Vec<CallerFrame>>` / `child_call_count: AtomicU32` / `tool_call_count: AtomicU32`（run_code 本身 + 桥接调用都计，call_agent 不计——它有自己的预算，:126-132）/ `tool_audit_log: Arc<Mutex<Vec<ToolAuditRecord>>>`（Arc 供 'static 桥接闭包写入）/ `denial_counts: Mutex<HashMap<String,u32>>` + `watchdog_tool` + `abort_notify`（审批 watchdog：连续 Denied 中止 run）/ `total_input_tokens/AtomicU64` / `total_output_tokens` / `total_cost_usd: Mutex<f64>`（f64 无原子；fold 每层完成一次，call_agent 批串行故无竞争）。
- builder：`new`（:178）/ `with_approval`（:223）/ `with_message_sink`（:233）/ `with_cancel_token`（:244）。
- 只读访问器：`total_input_tokens`/`total_output_tokens`/`total_cost_usd`/`execution_tree`/`child_call_count`/`tool_call_count`/`tool_audit_records`（:250-298）。
- `run_root`（:532）：根入口（resume 语义、abort 分支在此）。
- `impl ToolExecutor for AgentRunner`（:1248）：**接缝核心**——`call_agent` 拦截递归，其余委托 ToolRegistry。
- `ChildProgress`（:1194 impl ProgressCallback）：子代理进度合并回父回调。
- 时间预算语义：每层拿**完整** `timeout_ms`（纯 LLM await 时间累积；审批等待与工具执行暂停计时）。
- **message_sink 只给 root 层**（:107-112 注释：子层消息混入同一 run 行会破坏 resume 顺序）。

## 7. runtime.rs —— 单 agent 主循环（4080 行，测试占大半）

- `DEFAULT_MAX_EMPTY_TURNS = 3`（:30）。
- `MessageSink` trait（:47）：`append(&Message)` + **`append_with_meta(&Message, &MessageMeta)`（v5 §2 元数据旁路）**。默认实现丢弃 meta 委托 `append` —— 只 override `append` 的第三方 sink 不破坏（测试钉死）。辅助：`sink_append`（:62，无 meta）/ `sink_append_with_meta`（:70，assistant 消息专用）。
- **v5 §2 捕获点**（`execute_run` 内，assistant 消息构造处 :705-717）：流式/非流式两路径在 response 汇合点（:668）之后统一构造 `MessageMeta{model_id（当前请求模型）, usage/finish_reason（取 ModelResponse）, latency_ms（`call_start`（:519）Instant 差值，快照 :670）}`，随 assistant 消息走 `sink_append_with_meta`；**user/tool 消息维持无 meta 的 `append`**。meta 不进 `Message` 本体（provider 回放零泄漏），持久化层（RunRecorder override `append_with_meta`）落 `messages.meta_json`。
- 取消辅助：`interrupted_result`（:85，Interrupted + 部分转写）、`token_cancelled`（:107，None → pending 永不触发）、`cancelled_tool_message`（:116，`"[tool call cancelled before completion]"`）、`append_cancelled_tool_results`（:146，补合成 Tool 消息防悬空 tool_calls；**ask_user 豁免**——按工具名跳过，悬挂保留给迟到答案，ask-user r5 不变量，并行批 drain 的 None slot 同款豁免）。
- `RuntimeLimits`（:164）：max_steps/max_depth/max_tool_calls/max_child_agent_calls/**timeout_ms**（回合级纯 LLM 时间预算，fix-21）/max_context_bytes/max_output_bytes/**output_store_threshold**（长输出落盘阈值，C2：0=关，默认 16384；未注册 storer 时无效）/max_empty_turns/**parallel_tool_calls**。`Default`（:188）：steps=0(不限)/depth=4/tool_calls=20/child_calls=8/timeout=60s/ctx=64KB/out=64KB/store=16KB/empty=3/parallel=true。
- `check_limits`（:205）：步/深/工具/子调用四闸。
- **`fork_stored_output(config, tool_name, output)`（私有 async，C2）**：长输出落盘分叉，顺序/并行两路径共用。工具结果三段处理 **mask → fork → limit**（C2 起 mask 提前到最前——落盘的必须是脱敏后文本，vault 凭据绝不落盘；limit 保留为最后兜底）：`ask_user` 豁免（问卷重建链依赖全量原文）/阈值 0 或输出 ≤ 阈值直通/未注册 storer 或 `store` 返回 None（存储不可用）原样回落 limit 路径——双路径降级阶梯不变；命中则 content 换 `build_preview` 预览、status 置 `Truncated`（progress truncated 链路零改动），见 [output_store.rs](#22-output_storers-长输出落盘接缝c2)。
- `CostSpec`（:244）：`from_prices(in, out)` / `is_configured` / `cost_of(&Usage) -> f64`（per-mtok 计价；cached/reasoning additive 字段刻意忽略）。
- `RunConfig`（:283）/`RunResult`（:317）/`StepResult`（:332）。
- `execute_run`（:399）：**主循环**。流式事件→progress；tool_calls→审批→执行（并发批，join 点按 tool_call 序协调顺序敏感操作）→sink 落盘→循环。
- `validate_tool_arguments`（:1161）。

## 8. config/ —— 配置三件套（解析/合并/写回/校验）

### mod.rs

- `OpenSlateConfig`（:57）：project?/database?/prompts?/limits?/providers/models/**levels: HashMap<String,String>**（:78）/trace?/mcp?/builtin_tools/skills/ptc/approval?/tui/**ask_user**（ask-user：`[ask_user]` 段）/**notify**（notify S1b：`[notify]` 段）/**attachments**（多模态输入：`[attachments]` 段，存储 v5 §6.6）。
- `AskUserConfig`（default_timeout: `AskTimeout` + mask_secrets: bool——**已消费**：mobile 装配期注入 core 脱敏器闭包的开关短路（默认 Never/true，deny_unknown_fields））/ `AskTimeout`（`Never | Seconds(u64)`，serde 外部标签：`"never"` / `{ seconds = 300 }`）/ `NotifyConfig`（default_wait_timeout: `AskTimeout`——mobile `NotifyTool` wait 模式缺省等待；默认 **Seconds(300)**，与 ask_user 的 Never 首装默认有意不同：通知场景用户大概率不在屏幕前；Deserialize-only，设置页 UI 后补）。
- **`AttachmentsConfig`（`[attachments]`，多模态输入 §6.6；2026-10-21 图片处理用户可配置）**：`max_image_dim=2048`（**用户旋钮**：等比缩放目标边长，值域 512–6000）/ `max_image_mb=5`（**用户旋钮**：**处理后**大小上限 MB，值域 1–5——语义自「ingest 入口源文件上限（旧默认 10）」迁移，入口防呆改固定 50MB 常量，归一化迭代压缩 q85→70→55 → ×0.85 缩边以此达标，地板 dim 1024）/ `max_video_mb=50`（视频落盘上限，passthrough 不转码）/ `max_inline_video_mb=20`（data:URL 内联发送上限）/ `max_inline_total_mb=25`（单请求附件总字节）/ `experimental_video=false`（实验视频开关）。整段 `#[serde(default)]`（缺省即安全值）、deny_unknown_fields、Copy；validation 边界（`validation::attachments_floor_errors`）：512 ≤ dim ≤ 6000、1 ≤ max_image_mb ≤ 5（= ingest 出口 ≤5MB/≤6000² provider 公共分母）、其余 mb ≥ 1。消费方在 openslate-mobile（ingest / Submit 门控 / 回填封顶）；写回：mobile FFI setter（`set_image_processing` / `set_experimental_video`，toml_edit 原地改写）。
- `ProviderConfig`（:197）/ `ModelConfig`（:223）/ `LimitsConfig`（:147，`#[serde(default)]` 整段缺省；含 **`output_store_threshold`**（C2 长输出落盘阈值，默认 16384、0=关闭，validation 值域 0 或 4096..=16MiB））/ `TraceConfig`（:249）/ `McpConfig`+`McpServerConfig`（:270/:281）+ `TransportConfig`（:295，stdio/HTTP）/ `AgentsConfig`（:320）/ `BuiltinToolsConfig`（:332）/ `SkillsConfig`（:360）/ `TuiConfig`+`TuiIconsConfig`（:381/:395）/ `PtcConfig`（:403）/ `ApprovalPolicySetting`（:518，legacy `policy` 键拼写）+ `ToolApprovalLevelSetting`（:533，新 schema 等级拼写 "manual"/"allow"/"auto"）+ `ApprovalConfig`（:556）。
- **`ApprovalConfig`（`[approval]`）双 schema 共存（tool-approval）**：legacy `policy`（+`tools`）保留可解析（旧文件零破坏，`to_policy()` :579）；新键 `default_level: Option` / `tool_levels: BTreeMap<String, 等级>` / `server_levels: BTreeMap<String, 等级>`（键 = 审批作用域 slug，聚合下游 / host 前缀，见 `mcp::server_slug_of`）。生效规则统一经 `approval::rules_from_config` 推导（legacy 铺底 → 新键覆盖），配置层不自算。
- 解析函数：`parse_openslate_toml`（:525）/ `AgentFrontmatter`（:544）/ `derive_id_from_filename`（:562，agent id 缺省从文件名派生）/ `parse_agent_markdown`（:591，YAML frontmatter + 正文=default_prompt）/ `parse_agents_dir`（:636）。
- **注意**：config 类型只 derive Deserialize（协议侧镜像 DTO 是为了不给 core 加 Serialize）。

### merge.rs

- `merge_configs(global, local) -> OpenSlateConfig`（:27）：全局库 + 本地叠加（providers/models/levels/capabilities 等 map 类合并、local 覆盖）。

### persist.rs

- 格式保持写回（toml_edit：注释/布局存活）。API：`upsert_provider`（:163）/ `upsert_model`（:187）/ `set_level`（:225）/ `remove_level`（:254）/ `set_capability`（:279）/ `remove_model_entry` / `remove_provider` / `upsert_mcp_server` / `remove_mcp_server` / `upsert_env_key`（`.env` 0600）/ `set_agent_tool_lists` / `set_agent_prompt`（:889，agent md **正文**替换——`set_agent_tool_lists` 的对称面：frontmatter 逐字节保留、正文规范化（trim、空白回默认文案 `DEFAULT_AGENT_BODY`、单尾换行）、幂等（目标态即现态不写）；mobile 设置页编辑 agent 角色定位/system prompt 的落盘半边）/ **`set_approval_section`（:330，tool-approval：整段重写 `[approval]` 为新 schema——删除 legacy `policy`/`tools` 键（设置页写入即迁移）；只写非默认值（default=Allow 与缺省等价省略键、空 map 不写子表）；全默认（Allow + 两 map 空）**整段删除**；其余内容逐字节保真；幂等）**。
- **红线**：配置写入永远走本模块，禁手拼 TOML 字符串。

### validation.rs

- `Severity`（:22）/ `ValidationError`（:31）/ `ValidationFinding`（:38）/ `ValidationResult`（:46）。
- `validate_config`（:76，错误清单）/ `validate_strict`（:389，Err 化）/ `validate_config_full`（:423，finding 清单）。

## 9. model_config.rs —— 别名解析（469 行）

- 常量：`CAP_MAIN`/`CAP_COMPACT`/`CAP_TITLE`/`CAP_REVIEW`（:31 = `"review"`，审批 AI 审查模型，tool-approval）+ `CAPABILITIES`（:34，四值；SetCapability 校验白名单即此表）。
- `capability_title`（:42，`CAP_REVIEW` → "审批审查"）/ `capability_alias(config, cap) -> String`（:46，缺省回退 main→"main"、title→"fast"；compact 与 review 的 "fast" 缺省仅保留函数兼容——**review 无硬缺省**：session 引擎/AI 审查未绑定 review 一律**继承会话当前模型**（compact 同款运行时语义），不消费该缺省）。
- `ResolvedModel`（:61）：解析结果（model_id + 计价等）；`cost_spec()`（:86）→ `CostSpec`。
- `resolve_model(config, alias) -> Result<ResolvedModel, ConfigError>`（:111）：levels 映射优先，无 levels 段直接查 models（legacy 保持）。`reasoning_effort` 容忍归一（:145-162）：七档全集 none/minimal/low/medium/high/xhigh/max 白名单透传——**none = 显式关闭思考**（wire 发 `"reasoning_effort":"none"`，与缺省不传参的 Auto 语义不同；仅支持的家族有效，GLM-5.2 / Gemini 2.5）；非法值 warn 后按 None（配置瑕疵不炸 provider 构建，wire 少带字段回落 provider 默认）。
- `resolve_all_models`（:158）：批量。

## 10. model_registry.rs —— LLM 元数据 registry 纯逻辑（573 行）

- 输入任意 model id → context/output 限额、vision/reasoning/tool_call、计价（背景：Provider `/models` 只回 id 清单）。
- 三源定义 + 解析 + 归一化匹配（小写、去非字母数字、去 provider 前缀 / `@` 变体 / 尾部日期快照）+ 按源优先级合并（models.dev > LiteLLM > CloudPrice，逐字段 fill-None）。
- **无 IO**——拉取/存储在 `openslate-mobile/src/registry.rs`（ZSTD 本地持久化 `{config_dir}/registry/`）。在线兜底仅采信数值字段（限额/计价），能力布尔不采信（实证：GLM-5.3 base 被 CloudPrice 错标 image 输入）。详见 [../03-models/model-registry.md](../03-models/model-registry.md)。

## 11. mcp.rs —— MCP 接入（1014 行）

- 外部 server：`[mcp.servers]`（stdio/Streamable HTTP）→ `connect_mcp_server`（:358）；工具命名 `{server}_{tool}`（`McpTool::from_definition` :166，exposed_name vs definition_name 分离，:146-157）。
- 内置 in-process：`connect_builtin_servers`（:578）tuple transport，无子进程无序列化，裸名。
- `McpTool`（:146，impl Tool；`namespace()` 返回 server_alias 驱动 PTC 的 `tools.<server>.<method>` 路径；调用带 `DEFAULT_CALL_TIMEOUT` 超时，:224-238）。
- `McpConnectionGuard`（:272）：连接保活（`push`/`append`/`len`/`graceful_shutdown`）。**必须活得比 ToolRegistry 久**（AppContext 字段声明序保证，wiring.rs:43-58；`append` 供无热重载缝的前端把拆分的 builtin/外部两个 guard 合一保活）。
- `set_mcp_auth_override`（:76）。
- **`server_slug_of(exposed_name, namespace) -> Option<String>`（:217，审批作用域的权威反解，tool-approval）**：registry 暴露名 → MCP server 标识——`namespace = None`（native / HostTool / in-process builtin）→ `None`（非 MCP 无 server 作用域）；含 `__`（聚合形态，如 `termux_github__create_issue`）→ 首个 `__` 前段剥 `{namespace}_` 前缀 = 下游 slug（→ `"github"`，剥后为空回退 namespace）；裸名形态（如 `termux_bash`）→ namespace 本身（host 前缀组，host 内全部工具共享一个审批作用域）。精度局限：只认「首个 `__` 分界 + 前缀」形状（默认大模板 `{prefix}_{server}__{tool}` 的自然产物），用户改 `tool_name_template` 无 `__` 时回落 host 前缀组；Kotlin `mcpServerSlugOf` 语义对齐。消费方：审批规则 `ApprovalRules.servers` 键、`ApprovalSummaryDto.server`、UI (c)/(d) MCP 级选项。
- ContentBlock 处理（`downgrade_content_blocks`）：Text 拼接；**Image 双通道**（2026-10-21）——base64
  解码收进 `Vec<Attachment>`（`ToolOutput.attachments`，id `tool-{n}`；畸形 base64 warn + 占位、
  不中断）+ 文本占位 `[image: {mime}, {N} bytes]` 保留；Audio/Resource/ResourceLink/未知出占位符；
  `#[non_exhaustive]` 兜底 warn。返回 `(String, Vec<Attachment>)`。
- v1 决策：桌面端无热重载；连接/调用失败非致命（warn + skip）。mobile 端例外——运行期热重载（`MobileRuntime::reload_mcp` 复用 wiring 的 `connect_external_mcp`）。
- 工具注册名模板（**双模板模型**）：`[mcp].tool_name_template`（大模板，三占位符）与 `[mcp].builtin_tool_name_template`（小模板）——`{prefix}`=`[mcp.servers]` 键名（mobile「前缀」设置控制其值）、`{server}`=上游聚合名的 slug 段、`{tool}`=不带服务器前缀的裸工具名。**上游名含 `__`（聚合形态 `slug__tool`）才走大模板**（缺省 `"{prefix}_{server}__{tool}"`，`DEFAULT_TOOL_NAME_TEMPLATE`，单趟展开、值不二次展开）；**不含 `__` 的裸名走小模板**（占位符仅 `{prefix}`/`{tool}`——裸名没有 slug 段，缺省 `"{prefix}_{tool}"`，`DEFAULT_BUILTIN_TOOL_NAME_TEMPLATE`）。两个默认值均与历史 `{prefix}_{upstream}` 展开逐字节一致（默认配置零影响）。**注意：无 `__` 路径同时覆盖 host-builtin 裸名（聚合 host 内置 server 列出的 bash/read_file 等）与桌面直连裸名，用户改 `builtin_tool_name_template` 同时影响两者**（同一条展开分支）。`format_tool_name(template, builtin_template, prefix, upstream)` 收模板对、内部按上游形态分流（唯一的展开实现，勿在别处复制）；`McpTool::from_definition` 带 `Option<&str>` 模板对，只改 `exposed_name`（`definition_name`/`namespace` 语义不变）。校验两套独立：`validate_mcp_tool_name_template`（必须含 `{tool}`、三占位符外余量限 `[A-Za-z0-9_.-]`、字面 `__` 合法）；`validate_mcp_builtin_tool_name_template`（必须含 `{tool}`、禁 `{server}`、strip 占位符后**禁字面 `__`** 且限 `[A-Za-z0-9_.-]`——裸名注册不得与聚合形态 `slug__tool` 混淆；strip 会合并相邻分隔符，`{prefix}_{tool}_x` 一并被保守拒收）。生效值统一经 `McpConfig::builtin_template_or_default()`（None/空白回落默认，装配/reload/FFI 共用接缝）。落盘 `persist::set_mcp_tool_name_template` / `set_mcp_builtin_tool_name_template`（空串删键回默认）。
- 前缀改名（= `{prefix}` 展开值）：`persist::rename_mcp_server(path, old, new)`（toml_edit 整子表搬移，transport/headers 随迁；单键直取、多键须精确命中 old）；`validate_mcp_server_name`：非空、`[A-Za-z0-9_.-]`、**不含 `__`**（键名会泄入裸名/小模板展开路径（默认即 legacy `prefix_{tool}` 形）与 UI 启发式——**刻意不接入 `validate_config`**，桌面手写配置不受 mobile 聚合约束）。mobile FFI `get/set_mcp_server_name` 读写，改名后经 reload 热生效。
- 基于 rmcp（协议 2025-11-25）。

## 12. agent_tree.rs —— 静态配置树（364 行）

- `AgentNode`（:9）/ `AgentTree`（:23）/ `TreeError`（:30，悬空 children/环/无根等）。
- `from_configs(&[AgentConfig]) -> Result<Self, TreeError>`（:45，校验 children 引用）。
- `get_agent`（:128）/ `get_children`（:133）/ `get_root`（:146）/ `root_id`（:151）/ `all_agents`（:156）。

## 13. execution.rs —— 运行时执行树（197 行）

- `ExecutionStatus`（:10）/ `ExecutionNode`（:18）/ `ExecutionTree`（:30）。
- `new(run_id, root_agent_id)`（:37）建根；`create_child`（:56）记 `parent_execution_id + parent_call_id`。
- `get`（:84）/ `root`（:89）/ `root_id`（:94）/ `depth(id)`（:99，max_depth 依据）/ `update_status`（:104）/ `node_count`（:112）/ `all_nodes`（:118）。

## 14. context.rs —— 子代理上下文隔离（401 行）

- `ContextIsolationConfig`（:10）：`inherit_parent_context`（默认 false——**子代理不继承父的完整会话历史**）/ `include_conversation_summary`（默认 true）/ `max_context_messages`（默认 8）/ `max_context_bytes`（默认 32000）。
- `build_child_context(config, child_system_prompt, task, parent_messages) -> Vec<Message>`（:47）：子 system prompt（必有）+（继承模式：父尾 N 条 | 隔离模式：父对话机械摘要 system 消息）+ task（user，必有）。
- 注意：多层委派时 `parent_messages` 实际是**父层进入时快照**（CallerFrame.messages_snapshot），非实时状态（见 runner §6）。

## 15. context_manager/ —— 会话窗口管理 + 压缩

### mod.rs

- `ContextManager`（:32）：system_prompt + 消息缓冲。API：`new`（:45）/ `from_config`（:59）/ `add_user_message`（:72）/ `add_assistant_message`（:84）/ `add_tool_message`（:96）/ `add_tool_message_with_name`（:108）/ `get_context`（:131，窗口裁剪）/ `clear`（:212）/ `set_system_prompt`（:217）/ `message_count`（:222）/ `total_bytes`（:227）/ `system_prompt`（:232）/ `needs_compact`（:236）/ `compact`（:255）。

### compact.rs

- `CompactResult`（:11，messages_before/after 等统计）。
- `needs_compact(messages, max_messages, max_bytes, reserve) -> bool`（:16）。
- `compact(&mut messages, system_prompt, max_msgs, max_bytes, summarize) -> CompactResult`（:42）：LLM 摘要；summarize 返回 None（provider 失败/空白回复）→ **机械拼接兜底**（内建，永不失败）。

## 16. approval.rs —— 审批闸门 / 规则引擎（1322 行，测试占大半）

（概念语义见 [../01-architecture/domain-concepts.md](../01-architecture/domain-concepts.md) Approval 节；审批链时序见 [dataflow.md](../01-architecture/dataflow.md) §4.1。）

- **规则引擎（tool-approval）**：`ToolApprovalLevel`（:75）= `Manual`（每次必问，trigger=Manual）/ **`Allow`（默认值，从不问）** / `Auto`（自动判断：启发式 Low 放行、Medium/High 进审批链，trigger=AutoRisk；`as_str()` = "manual"/"allow"/"auto"）；`ApprovalRules`（:121）= `{default_level, tools: BTreeMap, servers: BTreeMap}`，评估序 **tools > servers > default_level**（`level_for` :146；精确未命中做 ASCII 大小写不敏感回退——保 legacy AutoExcept 语义）。
- `ApprovalManager`（:307，Clone）字段 = `rules: Arc<RwLock<ApprovalRules>>`（Clone 共享同一 cell → 热更新对进行中回合生效；毒化容忍）+ `callback: Option<Arc<dyn ApprovalCallback>>`。构造：`new(policy)`（legacy 经 `rules_from_policy` 映射）/ `auto()`（= default=Allow）/ `manual()` / **`with_rules(handle)`（:342，装配侧正道——与 bridge 共享同一 cell，P0-5）**。热更 API：`rules_handle()`（:351 外部改写入口）/ `set_rules()`（:356）/ `rules()`（:364 快照）。
- **`check(&self, tool_name, server: Option<&str>, args, agent_id)`**（:506，聚合闸门）：规则快照评估等级（锁外）→ 不需要（Allow / Auto+Low）→ `Approved`；需要且有 callback → `callback.decide()`（同步阻塞）；需要但无 callback → `Approved`（非交互直通，CLI `NonInteractiveGate` 依赖该语义）。`ApprovalRequest`（:266）携带 `server: Option<String>`（审批作用域）/ `trigger: ApprovalTrigger`（:248 = Manual/AutoRisk，P0-2）/ `note: Option<String>`（AI 审查理由——core 恒 None，mobile 审查装饰器回填）。
- `rules_from_policy`（:193，**P0-4 钉死勿改**：`Auto`→default=Allow；`Manual`→default=Manual；`AutoExcept(list)`→default=Allow + tools[每项]=Manual）；**`rules_from_config(&OpenSlateConfig)`**（:218，单一推导函数——装配 / config_ops 重载 / always_* 持久化**三个调用点共用**，防各处推导漂移：Allow 基线 → legacy `policy`(+`tools`) 铺底 → `default_level` 覆盖 → `tool_levels`/`server_levels` 逐键合并；`[approval]` 段缺省 = 纯 Allow 基线，**mobile 的 Auto 默认是装配层显式 opt-in，不在这里做**）。
- 兼容层：`needs_approval`（:381，旧三型语义，不感知 server 作用域与 Auto）/ `policy()`（:579，owned——规则快照重建 legacy 三型；`to_legacy_policy` :170：default=Auto 无法用 legacy 表达，有损降级为 `Auto`，仅供旧装配路径 round-trip）；`assess_risk`（:398，子串启发式；`run_code` 硬编码 High——一次审批盖全脚本，per-bridge-tool 审批是 P2 deferral）；`create_request`（:472）/ `auto_approve` / `deny`。
- 模块头注释：**每个**工具调用（含 call_agent 与 run_code）都过 check，无绕过路径；runner.rs `ToolExecutor::execute` 顶部闸口从 registry `namespace()` 经 `mcp::server_slug_of` 反解注入 server 作用域（runner.rs:1259-1267）。决策审计走 tracing target=`openslate_approval`，不写 store。

## 17. skills.rs —— SKILL.md 发现（974 行）

- `Skill`（:31）/ `SkillWarning`（:44）/ `SkillsCatalog`（:59）。
- `parse_skill_markdown`（:80）/ `skill_name_warnings`（:130）/ `discover_skills(sources)`（:169，源目录**低→高**优先级，重名后盖前；坏 skill 只出 warning 不阻断启动）。
- catalog API：`skills()`（:315）/ `is_empty`（:320）/ `get(name)`（:325）/ **`catalog_prompt(max_list_chars)`**（:339，tier-1 注入：仅 name+description，超限截断）。
- tier-2：`read_skill` 工具按需读正文（agentskills.io 约定）。

## 18. prompt.rs —— 提示词解析（517 行）

- `PromptSource`（:17）/ `ResolvedPrompt`（:30）/ `PromptSnapshotData`（:39，`to_snapshot_data` :62）/ `PromptError`（:50）。
- `resolve_root_prompt(prompts_dir, profile)`（:111）：解析链 `prompts/{profile}/prompt.md` → `prompts/default/prompt.md` → 内建。
- `resolve_agent_prompt`（:152）：agent 级 `prompts/{profile}/agents/{id}.md` → default agents → `default_prompt` → 内建。
- `apply_template`（:224）。

## 19. paths.rs —— 路径发现（246 行）

- `OpenSlatePaths`（:5）：local=`./.openslate/`、global=`~/.config/openslate/`、data=`~/.local/share/openslate/`。
- `resolve_paths(cwd)`（:61）：active = local 存在则 local 否则 global。

## 20. trace.rs + error.rs

### trace.rs（603 行）

- `TraceSpanId`（:16）/ `TraceEvent`（:30，`phase()` :80）/ `TraceCollector`（:162）。
- API：`new(process_id, thread_id)`（:172）/ `begin_span`（:191）/ `begin_span_with_args`（:196）/ `end_span`（:220）/ `record_complete`（:231）/ `record_instant`（:251）/ `record_counter`（:263）/ `to_chrome_trace_json`（:277）/ `events`（:283）/ `clear`（:288）/ `export_to_file`（:293）。
- 消费方：chrome://tracing / Perfetto；`ManagedRunResult.trace` 携带；run 后写 trace_events 表。

### error.rs（529 行）

- 错误体系（thiserror）：`OpenSlateError`（:5，top）/ `ConfigError`（:41）/ `ProviderError`（:74）/ `RuntimeError`（:104，含 `MaxStepsExceeded`/`Timeout`/`Cancelled`…）/ `StoreError`（:198，含 `MigrationError`/`ConnectionError`/`WriteError`/`QueryError`）/ `ToolError`（:214，含 `NotFound`/`ExecutionError`…）。

## 21. ask_user.rs —— ask_user 历史手术与合成文案（ask-user r5）

设计真相源 `docs/proposals/ask-user-tool-design.md`；工具本体与 host call 通道在 openslate-mobile（后续 lane），core 只承载引擎/会话共享的纯函数：

- 常量：`ASK_USER_TOOL = "ask_user"`（runtime 取消豁免按名判定，与 `run_code`/`call_agent` 串行判定同模式）；合成文案 `SUPERSEDED_RESULT`（"Question superseded by a new user message."）/ `EXPIRED_RESULT`（"Question expired unanswered (TTL)."）/ `INTERRUPTED_RESULT`（同批非 ask_user 兄弟兜底）。
- **悬挂定义**：assistant 声明 `tool_calls` 但其后**连续 Tool 结果段**缺对应 `tool_call_id`（合法消息序里批次结果必连续，扫描即完整判定）。
- `dangling_tool_calls(&[Message]) -> Vec<(id, name)>`：悬挂清单。
- `append_late_tool_result(&mut Vec<Message>, tool_call_id, content) -> Result<Message, AskUserError>`：**迟到答案补全**——插入位置 = 声明处 assistant 之后的连续结果段末尾（常态即尾部）；幂等（已答 → `AlreadyAnswered` 首答者赢；未知 id → `ToolCallNotFound`）。
- `repair_dangling_tool_calls(&mut Vec<Message>, DanglingClosure::{Superseded, Expired}) -> Vec<Message>`：**悬挂修复**——ask_user 按关卷原因取文案、兄弟兜底 `INTERRUPTED_RESULT`，全部插入合法位置；返回合成消息供落库；修复后无悬挂（测试钉死）。
- `AskUserError`（thiserror）：ToolCallNotFound / AlreadyAnswered；经 `OpenSlateError::Validation` 映射进引擎错误面。
- **工具输出脱敏注册表**（ask_user secret 子系统的 core 消费点；`SecretMasker` 类型）：`set_secret_masker(Option)` 整体替换（测试复位）/ `push_secret_masker` 追加（mobile 装配期，多 runtime 并行叠加安全）/ `mask_secrets(&str)` 依次应用全部已注册脱敏器 / `mask_tool_output(ToolOutput)`（content 替换 + **实际发生替换时追加 `MASKED_NOTE` 内部提示**（`<system-note>Output masked: …</system-note>`，tool result 内容的一部分、不注入独立 system 消息）+ bytes 重算）。消费点在 runtime.rs 工具结果形成处（顺序/并行两路径；C2 起位于 output-size 上限与长输出落盘分叉**之前**——mask → fork → limit，落盘文本必脱敏；`tool_end` preview / 消息历史 / tracing preview 形成之前全覆盖）；无注册恒等（桌面零影响）。值源与 `[ask_user] mask_secrets` 开关由宿主注入的闭包负责（与 `mcp::set_mcp_auth_override` 同款进程级接缝模式）。

## 22. output_store.rs —— 长输出落盘接缝（C2）

output_query 配套的 core 侧接缝（设计「长输出落盘 + output_query」C2 步）：

- `StoredOutput { ref_id, orig_bytes, line_count }`：落盘回执（预览构造用）。
- `ToolOutputStorer` trait（async）：`store(run_id, tool_name, content) -> Option<StoredOutput>`；实现吞错（持久化失败不 kill run），`None` = 存储不可用 → 引擎回落旧 limit 路径。
- **进程级单槽注册**（与 `set_shell_env_source` / `set_secret_masker` 同款接缝）：`set_tool_output_storer(Option<Arc<dyn ToolOutputStorer>>)` / `current_storer()`。**未注册（CLI/TUI/server 桌面形态）时分叉点直通，行为与旧截断路径完全一致**。
- `STORED_OUTPUT_MARKER = "[STORED OUTPUT"`（预览头行前缀，**格式冻结**：UI 恢复链与 mobile 全文读取都按它识别）/ `PREVIEW_EDGE_BYTES = 1024`（头尾字节预算）。
- `build_preview(tool_name, &StoredOutput, content) -> String`：首行元数据（`ref="…" size=… lines=…`）+ head ≤1KB + 省略行（`[... {omitted} bytes omitted ({总行数} lines) — use output_query to inspect ...]`，omitted = orig_bytes − head − tail 的诚实值）+ tail ≤1KB；字节截断按 UTF-8 字符边界回退/前进（对齐 mcp-builtin shell.rs truncate 先例，中文/emoji 不劈裂）。
- 配置面：`[limits].output_store_threshold`（u32，默认 16384，0=关闭，serde default——存量配置自动获得；validation 值域 `0 或 4096..=16MiB`，下限理由 = 预览固定 ~2.3KB，低于 4096 会出现预览比原文长）。镜像链：`LimitsConfig` → `RuntimeLimits`/`RunConfig`（runner 透传）→ protocol `LimitsDto`（serde default 16384，additive 兼容不 bump proto）→ TUI `config_from_view`/`view_from_config`。mobile 默认 toml（bootstrap.rs）显式写 `output_store_threshold = 16384`。

## 跨模块注意事项

1. `AgentRunner`/`RunManager` 用**所有权传递**注入关切（`with_*` builder 快照），不做共享引用。
2. `Message.reasoning_content` 多轮语义依赖 vendor 过的 genai（anthropic adapter 回传 thinking block；根 Cargo.toml `[patch.crates-io]`）。
3. 配置写入永远走 `config/persist.rs`（toml_edit 保格式）。
4. 改任何公开 API/数据模型/行为 → 按 `AGENTS.md` 同步更新文档。
