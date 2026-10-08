# 02-1 · openslate-core 模块详解

> 领域核心 crate（`crates/openslate-core/src/`，lib.rs 声明 21 个顶层模块，其中 `config/` 与 `context_manager/` 是目录模块——按 20 个文件组逐一详解）。**不依赖任何前端/传输**；workspace 内仅依赖 `openslate-mcp-builtin` 与 `openslate-ptc`。模块声明见 `src/lib.rs:1-21`；`prelude` 重导出 callable/config/context_manager/error/paths/provider/types（lib.rs:23-31）。

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
| 16 | `approval.rs` | 审批闸门 |
| 17 | `skills.rs` | SKILL.md 发现与注入 |
| 18 | `prompt.rs` | 提示词解析（profile 体系） |
| 19 | `paths.rs` | 路径发现 |
| 20 | `trace.rs` + `error.rs` | Chrome Trace 导出 / 错误类型 |

---

## 1. types.rs —— 领域 ID 与数据结构（443 行）

- `id_type!` 宏（:6-44）生成 newtype String 家族：`AgentId`/`RunId`/`StepId`/`ExecutionNodeId`/`ToolCallId`（:46-56）。均 derive `Debug/Clone/PartialEq/Eq/Hash/Serialize/Deserialize`，实现 `Display/FromStr(Infallible)/From<String>/From<&str>/AsRef<str>`。
- `MessageRole`（:61）= system/user/assistant/tool（serde lowercase）。
- `Message`（:70）：`role`/`content`/`tool_call_id?`/`name?`/`tool_calls?: Vec<ToolCall>`/`reasoning_content?`。`reasoning_content` 是 Anthropic 多轮 extended-thinking 语义：历史 thinking 块随 assistant 消息回传，持久化自动携带（依赖 vendor 过的 genai）。
- `AgentConfig`（:87，字段语义见 [../01-architecture/domain-concepts.md](../01-architecture/domain-concepts.md)）。
- `RunStatus`（:106）= running/completed/failed/interrupted/cancelled。
- `StepKind(pub String)`（:116）+ 常量 `MODEL_CALL`/`TOOL_CALL`/`CHILD_AGENT_CALL`（:120-124）。
- `ModelResponse`（:129）：content?/reasoning_content?/tool_calls/usage?/finish_reason?。
- `ToolCall`（:142）：id/name/arguments(serde_json::Value)。
- `Usage`（:150）：input/output_tokens(u32) + **additive** 字段 `cached_input_tokens?`/`reasoning_tokens?`（`#[serde(default)]`——旧 payload 反序列化为 None；cost 计算刻意忽略它们：cached 不重复计价，reasoning 已含在 output_tokens 里）。
- `ModelStreamEvent`（:174）：`Delta`/`Reasoning`/`Usage`/`Done(ModelResponse)`。
- `ToolOutput`（:190）：content/bytes/duration_ms/status（`ToolOutputStatus` = success/error/truncated，:200）。

## 2. provider.rs —— 模型调用抽象（275 行）

- `ToolDefinition`（:5）= name/description/parameters。
- `GenerateRequest`（:12）= model_id/system_prompt?/messages/tools/max_tokens?/temperature?。
- `ModelProvider` trait（:22，详见 [../01-architecture/traits-and-abstractions.md](../01-architecture/traits-and-abstractions.md) §1）。
- `ProgressCallback` trait（:57，10 个回调，`on_input_estimate`/`on_reasoning`/`on_step_end` 默认 no-op 保持向后兼容）。
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

## 5. run_manager.rs —— run 编排入口（1045 行，测试占大半）

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
- `execute_with_history`（:187）/`execute`（:207）：便捷包装。
- `ManagedRunResult`（:46）：run_id/status/messages/total_steps/total_input_tokens/total_output_tokens/total_cost_usd（各层按各自模型计价后聚合）/execution_tree/model/trace。

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

## 7. runtime.rs —— 单 agent 主循环（3565 行，测试占大半）

- `DEFAULT_MAX_EMPTY_TURNS = 3`（:30）。
- `MessageSink` trait（:47）+ `sink_append` 辅助（:53）。
- 取消辅助：`interrupted_result`（:64，Interrupted + 部分转写）、`token_cancelled`（:86，None → pending 永不触发）、`cancelled_tool_message`（:95，`"[tool call cancelled before completion]"`）、`append_cancelled_tool_results`（:111，补合成 Tool 消息防悬空 tool_calls）。
- `RuntimeLimits`（:126）：max_steps/max_depth/max_tool_calls/max_child_agent_calls/**timeout_ms**（回合级纯 LLM 时间预算，fix-21）/max_context_bytes/max_output_bytes/max_empty_turns/**parallel_tool_calls**。`Default`（:150）：steps=0(不限)/depth=4/tool_calls=20/child_calls=8/timeout=60s/ctx=64KB/out=64KB/empty=3/parallel=true。
- `check_limits`（:167）：步/深/工具/子调用四闸。
- `CostSpec`（:206）：`from_prices(in, out)` / `is_configured` / `cost_of(&Usage) -> f64`（per-mtok 计价；cached/reasoning additive 字段刻意忽略）。
- `RunConfig`（:245）/`RunResult`（:279）/`StepResult`（:294）。
- `execute_run`（:358）：**主循环**。流式事件→progress；tool_calls→审批→执行（并发批，join 点按 tool_call 序协调顺序敏感操作）→sink 落盘→循环。
- `validate_tool_arguments`（:1069）。

## 8. config/ —— 配置三件套（解析/合并/写回/校验）

### mod.rs

- `OpenSlateConfig`（:57）：project?/database?/prompts?/limits?/providers/models/**levels: HashMap<String,String>**（:78）/trace?/mcp?/builtin_tools/skills/ptc/approval?/tui。
- `ProviderConfig`（:197）/ `ModelConfig`（:223）/ `LimitsConfig`（:147，`#[serde(default)]` 整段缺省）/ `TraceConfig`（:249）/ `McpConfig`+`McpServerConfig`（:270/:281）+ `TransportConfig`（:295，stdio/HTTP）/ `AgentsConfig`（:320）/ `BuiltinToolsConfig`（:332）/ `SkillsConfig`（:360）/ `TuiConfig`+`TuiIconsConfig`（:381/:395）/ `PtcConfig`（:403）/ `ApprovalPolicySetting`（:456）+ `ApprovalConfig`（:473，`to_policy()` :485）。
- 解析函数：`parse_openslate_toml`（:525）/ `AgentFrontmatter`（:544）/ `derive_id_from_filename`（:562，agent id 缺省从文件名派生）/ `parse_agent_markdown`（:591，YAML frontmatter + 正文=default_prompt）/ `parse_agents_dir`（:636）。
- **注意**：config 类型只 derive Deserialize（协议侧镜像 DTO 是为了不给 core 加 Serialize）。

### merge.rs

- `merge_configs(global, local) -> OpenSlateConfig`（:27）：全局库 + 本地叠加（providers/models/levels/capabilities 等 map 类合并、local 覆盖）。

### persist.rs

- 格式保持写回（toml_edit：注释/布局存活）。API：`upsert_provider`（:163）/ `upsert_model`（:187）/ `set_level`（:225）/ `remove_level`（:254）/ `set_capability`（:279）/ `remove_model_entry`（:308）/ `remove_provider`（:315）/ `upsert_mcp_server`（:330）/ `remove_mcp_server`（:386）/ `upsert_env_key`（:430，`.env` 0600）/ `set_agent_tool_lists`（:522）。
- **红线**：配置写入永远走本模块，禁手拼 TOML 字符串。

### validation.rs

- `Severity`（:22）/ `ValidationError`（:31）/ `ValidationFinding`（:38）/ `ValidationResult`（:46）。
- `validate_config`（:76，错误清单）/ `validate_strict`（:389，Err 化）/ `validate_config_full`（:423，finding 清单）。

## 9. model_config.rs —— 别名解析（469 行）

- 常量：`CAP_MAIN`/`CAP_COMPACT`/`CAP_TITLE`（:20-24）+ `CAPABILITIES`（:27）。
- `capability_title`（:30）/ `capability_alias(config, cap) -> String`（:46，缺省回退 main→"main"、compact→"fast"、title→"fast"）。
- `ResolvedModel`（:61）：解析结果（model_id + 计价等）；`cost_spec()`（:86）→ `CostSpec`。
- `resolve_model(config, alias) -> Result<ResolvedModel, ConfigError>`（:111）：levels 映射优先，无 levels 段直接查 models（legacy 保持）。
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
- ContentBlock 处理（:120-137）：Resource/Image 出占位符；`#[non_exhaustive]` 兜底 warn。
- v1 决策：桌面端无热重载；连接/调用失败非致命（warn + skip）。mobile 端例外——运行期热重载（`MobileRuntime::reload_mcp` 复用 wiring 的 `connect_external_mcp`）。
- 工具注册名模板：`[mcp].tool_name_template`，**三占位符模型**——`{prefix}`=`[mcp.servers]` 键名（mobile「前缀」设置控制其值）、`{server}`=上游聚合名的 slug 段、`{tool}`=不带服务器前缀的裸工具名；缺省 `"{prefix}_{server}__{tool}"`（`DEFAULT_TOOL_NAME_TEMPLATE`，展开与历史行为逐字节一致）。`format_tool_name(template, prefix, upstream)`：上游含 `__` 才走模板（首 `__` 拆 server/tool，单趟展开、值不二次展开）；**不含 `__`（桌面直连形态）不走模板恒 `prefix_upstream`**（桌面零影响）。`McpTool::from_definition` 带 `Option<&str>` 模板，只改 `exposed_name`（`definition_name`/`namespace` 语义不变）。校验 `validate_mcp_tool_name_template`：必须含 `{tool}`、三占位符外余量限 `[A-Za-z0-9_.-]`（字面 `__` 合法——UI 改按模板正则提取）。落盘 `persist::set_mcp_tool_name_template`（空串删键回默认）。
- 前缀改名（= `{prefix}` 展开值）：`persist::rename_mcp_server(path, old, new)`（toml_edit 整子表搬移，transport/headers 随迁；单键直取、多键须精确命中 old）；`validate_mcp_server_name`：非空、`[A-Za-z0-9_.-]`、**不含 `__`**（键名会泄入 legacy `prefix_upstream` 路径与 UI 启发式——**刻意不接入 `validate_config`**，桌面手写配置不受 mobile 聚合约束）。mobile FFI `get/set_mcp_server_name` 读写，改名后经 reload 热生效。
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

## 16. approval.rs —— 审批闸门（585 行）

（概念语义见 [../01-architecture/domain-concepts.md](../01-architecture/domain-concepts.md) Approval 节。）

- `ApprovalPolicy`（:17，Default=Auto）/ `RiskLevel`（:26，Display 为 "low"/"medium"/"high"）/ `ApprovalRequest`（:47）/ `ApprovalDecision`（:60）/ `ApprovalCallback`（:74，同步阻塞设计）/ `ApprovalManager`（:81，Clone）。
- `needs_approval`（:114，AutoExcept 忽略大小写精确匹配）/ `assess_risk`（:131，子串启发式；`run_code` 硬编码 High + P2 deferral 注释）/ `create_request`（:204）/ **`check`**（:231，聚合闸门；决策全走 tracing target=`openslate_approval`，不写 store）/ `auto_approve`（:271）/ `deny`（:276）/ `policy()`（:280）。
- 模块头注释（:1-10）：**每个**工具调用（含 call_agent 与 run_code）都过 check，无绕过路径。

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

## 跨模块注意事项

1. `AgentRunner`/`RunManager` 用**所有权传递**注入关切（`with_*` builder 快照），不做共享引用。
2. `Message.reasoning_content` 多轮语义依赖 vendor 过的 genai（anthropic adapter 回传 thinking block；根 Cargo.toml `[patch.crates-io]`）。
3. 配置写入永远走 `config/persist.rs`（toml_edit 保格式）。
4. 改任何公开 API/数据模型/行为 → 按 `AGENTS.md` 同步更新文档。
