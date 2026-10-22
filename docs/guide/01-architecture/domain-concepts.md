# 01-3 · 核心领域概念详解

> 每个概念给出：定义文件与类型名、生命周期、边界条件。**以实际代码为准**（本文全部条目已对照源码核验；行号为当前快照，漂移时以类型名 grep 为准）。

## 概念速查表

| 概念 | 定义位置 | 一句话 |
|------|----------|--------|
| Provider | `core/src/config/mod.rs:197` `ProviderConfig` | 一个 LLM 接入端点 |
| Model | `core/src/config/mod.rs:223` `ModelConfig` | provider + 具体 model id 的绑定条目 |
| Level | `OpenSlateConfig.levels`（`config/mod.rs:78`） | 别名 → 模型条目的间接层 |
| Capability | `core/src/model_config.rs:27` `CAPABILITIES` | 固定功能 id → 代号 |
| Agent | `core/src/types.rs:87` `AgentConfig` | 静态代理定义 |
| AgentTree | `core/src/agent_tree.rs:23` | 由 AgentConfig 列表构建的静态配置树 |
| Run | `core/src/types.rs:47` `RunId` + `run_manager.rs` | 一次完整执行 |
| Step | `core/src/types.rs:116` `StepKind` | run 内的一步 |
| ExecutionTree | `core/src/execution.rs:30` | 运行时实际执行树 |
| Tool | `core/src/tool.rs:17` `Tool` trait | 工具统一抽象 |
| MCP | `core/src/mcp.rs` | 工具供给协议（外部 + 内置） |
| PTC | `openslate-ptc` + core 的 `run_code` | QuickJS 沙箱内程序化工具调用 |
| Skill | `core/src/skills.rs:31` `Skill` | SKILL.md 渐进披露 |
| Approval | `core/src/approval.rs`（`ToolApprovalLevel`/`ApprovalRules`/`ApprovalManager`） | 工具执行闸门（三级规则引擎） |
| Session | `openslate-session/src/state.rs:93` `CoreInner` | 跨回合共享状态 |

---

## Provider（接入端点）

```rust
// crates/openslate-core/src/config/mod.rs:197（字段见 197-221 区域）
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key_env: String,      // 环境变量名（值从 .env / 进程环境读）
    pub adapter: Option<String>,  // "openai"|"anthropic"|"gemini"|"ollama"；None = 按 base_url 猜（旧行为）
    pub title: Option<String>,    // 显示名；键名恒为内部 ID
    pub max_attempts: u32,        // 重试
    pub retry_base_ms: u64,       // 指数退避基值
}
```

- **键名即内部 ID**（`[providers.<name>]` 的 `<name>`）：模型引用、env 派生（`<NAME>_API_KEY`）、Android Keystore 均按 ID。
- 生命周期：解析自 TOML → 存在于 `OpenSlateConfig.providers: HashMap<String, ProviderConfig>` → `resolve_model` 时被查到 → `build_provider_for_model`（model-genai / app）构造 `GenaiProvider`。
- 注意：`ProviderConfig` 只 derive **Deserialize**（core 不加 Serialize 耦合），协议侧有镜像 `ProviderDto` + `From` 双向转换（protocol/lib.rs:259-283）。

## Model（模型库条目）

```rust
// crates/openslate-core/src/config/mod.rs:223
pub struct ModelConfig {
    pub provider: String,                    // 指向 [providers] 键名
    pub model: String,                       // 具体 model id（如 "glm-4.7"）
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub supports_tool_call: bool,
    pub supports_vision: bool,
    pub supports_reasoning: bool,
    pub input_price_per_mtok: Option<f64>,   // 计价 → CostSpec
    pub output_price_per_mtok: Option<f64>,
}
```

`[models.<entry>]` 是**共享库**；agent 不直接引用它（除非 legacy 无 levels 模式，见 Level）。

## Level（级别 / 别名）

`[levels]`：level 名 → 模型条目名（`levels: HashMap<String, String>`，config/mod.rs:78）。agent 的 `model` 字段写的是**别名**。解析优先级（`resolve_model`，model_config.rs:111）：

1. 名字在 `levels` 里 → 映射到条目名再查 `models`；
2. 不在 `levels` → 直接查 `models`（**legacy 行为**：无 `[levels]` 段的旧配置行为完全不变）；
3. 都失败 → `ConfigError::InvalidLevelRef`（松耦合：删除被引用条目在 config_ops 层放行，悬空合法，用时报错）。

## Capability（功能）

```rust
// crates/openslate-core/src/model_config.rs:20-27
pub const CAP_MAIN: &str = "main";
pub const CAP_COMPACT: &str = "compact";
pub const CAP_TITLE: &str = "title";
pub const CAPABILITIES: &[&str] = &[CAP_MAIN, CAP_COMPACT, CAP_TITLE];
```

`[capabilities]`：功能 id → 代号（levels key 或 models 条目名）。缺省回退（`capability_alias`，model_config.rs:46）：`main`→"main"、`title`→"fast"；`compact` 的 "fast" 缺省仅为函数兼容值——session 引擎未绑定 compact 时**继承会话当前模型**（`model_alias`，engine.rs `run_auto_compact` 自行取值，不走 `capability_alias` 缺省）。

- `main` = 会话主模型；`compact` = auto-compact 摘要用（显式绑定优先，未绑定 = 会话模型）；`title` = 会话标题生成用。
- 两层解耦设计：换模型只动 levels 绑定，换功能分工只动 capabilities。
- `capability == "main"` 的 SetCapability 落盘后同时热切换会话模型并广播 `model_changed`（session/config_ops.rs）。

## Agent（静态代理定义）

```rust
// crates/openslate-core/src/types.rs:87
pub struct AgentConfig {
    pub id: AgentId,
    pub name: String,
    pub model: String,            // 别名（levels key / 条目名）
    pub children: Vec<AgentId>,   // 可委派的子代理
    pub tools: Vec<String>,       // glob 白名单；空 = 全部注册工具
    pub hidden_tools: Vec<String>,// glob 黑名单；生效集合 = 白名单匹配 - 黑名单匹配
    pub default_prompt: String,   // 无提示词文件时的兜底 system prompt
}
```

来源：`agents/*.md`（YAML frontmatter + 正文=default_prompt，`parse_agent_markdown` config/mod.rs:591；id 缺省从文件名派生 `derive_id_from_filename` config/mod.rs:562）。生命周期：解析 → `AgentTree::from_configs` 建树 → run 期间只读。

## AgentTree（静态配置树）

`core/src/agent_tree.rs`：

- `AgentTree::from_configs(&[AgentConfig]) -> Result<Self, TreeError>`（:45）——校验 children 引用（悬空/环/无根都会报 `TreeError`）。
- `get_agent(&AgentId)` / `get_children` / `get_root` / `root_id` / `all_agents`。
- **静态**：run 期间不可变；会话层工具可见性热替换是换掉整个 `inner.agent_tree`（clone 给回家的 manager，engine.rs:218），而非原地改。

## Run（一次完整执行）

- `RunId`（types.rs:47，newtype String）；`RunManager::new_run_id()`（run_manager.rs:94）铸 **UUID v4**。
- 一个 run = 会话的一个回合，落 `runs` 表一行（`RunRecorder::begin` 先插 running 行——**run 在任何 LLM 调用前就已存在**）。
- 状态机：`RunStatus`（types.rs:104）= `running → completed | failed | interrupted | cancelled`（store 侧 CHECK 约束还含预留值 `error`）。
- REPL 特例：多回合共享同一 run id（一个 recorder 跨 turn）。
- 编排入口：`RunManager::execute_with_run_id(run_id, provider, prior_messages, cancel, progress) -> ManagedRunResult`（run_manager.rs:118）。

## Step（run 内的一步）

```rust
// crates/openslate-core/src/types.rs:116
pub struct StepKind(pub String);
impl StepKind {
    pub const MODEL_CALL: &'static str = "model_call";       // 一次 LLM 调用
    pub const TOOL_CALL: &'static str = "tool_call";         // 一次工具调用
    pub const CHILD_AGENT_CALL: &'static str = "child_agent_call"; // 一次子代理调用
}
```

`StepId`（types.rs:48）标识。steps 表按 `(run_id, seq)` 存储，run 结束后由 CLI 侧整树写入（runtime 的 `StepResult` 家族，runtime.rs:294）。

## ExecutionTree / ExecutionNode（运行时执行树）

`core/src/execution.rs`：

- `ExecutionTree::new(run_id, root_agent_id)`（:37）建根节点；`create_child(parent_exec_id, parent_call_id, agent_id, input)`（:56）为每次 `call_agent` 递归加节点——记录 `parent_execution_id + parent_call_id`。
- `ExecutionStatus`（:10）跟踪节点状态；`depth(id)` 计算深度（配合 `max_depth` 限额）。
- 与静态 AgentTree 对应但**含运行时事实**（哪些子代理真的被调了、输入输出是什么）。
- 持久化：run 结束后整树写 `execution_nodes` 表（CLI 侧）；**注意** recorder 的消息挂在一个派生存储节点 `{run_id}-persist` 上（见 [../02-core/openslate-store-sqlite.md](../02-core/openslate-store-sqlite.md) recorder 节），与 runtime 的 `en-{uuid}` 树互不干扰。

## Tool（工具）

`core/src/tool.rs:17` 的 trait（详见 [traits-and-abstractions.md](traits-and-abstractions.md)）。要点：

- 注册进 `ToolRegistry` 后按名字调度；`definitions_visible(allow, hidden)` 是 agent 的 `tools`/`hidden_tools` 与模型可见列表的**唯一交汇点**（tool.rs:174）。
- 名字匹配语义：模式无 `*`/`?` 精确匹配，否则 glob（`tool_name_matches`，tool.rs:242）。
- 工具来源三系：内置 MCP（裸名）、外部 MCP（`{server}_{tool}`，`McpTool::from_definition` mcp.rs:166）、宿主注入（mobile host tools，`register_boxed` tool.rs:93）。

## MCP（工具供给协议）

- **外部 server**（`[mcp.servers]`，stdio / Streamable HTTP）：`connect_mcp_server`（mcp.rs:358）建立连接，工具命名 `{server}_{tool}` 防跨 server 冲突。
- **内置 in-process server**：`connect_builtin_servers`（mcp.rs:578）用 tuple transport 直连（无子进程、无序列化），工具用裸名（`read_file`/`write_file`/`shell`/`edit_file`/`read_skill`…）。
- `McpConnectionGuard`（mcp.rs:272）：连接保活容器。**必须活得比 ToolRegistry 久**——`McpTool` 持有 `ServerSink`（连接的克隆句柄），连接先死则工具调用悬空。`AppContext` 用字段声明序保证 drop 序（wiring.rs:43-47 有注释钉死此约束）。
- v1 范围决策：无热重载；连接/调用失败非致命（warn + skip）；Resource/Image 内容出占位符（mcp.rs:120-135）。
- 协议版本：rmcp 2025-11-25。

## PTC（Programmatic Tool Calling）

- 模型不逐个发 tool_call，而是写一个 **JS async 箭头函数**（`run_code` 工具的参数），在 `openslate-ptc` 的 QuickJS 沙箱内执行：脚本内经 `tools.<name>` / `tools.<server>.<method>`（MCP namespace，由 `Tool::namespace()` 驱动）调用宿主工具，自己控制编排/重试/分支。
- 沙箱约束：每 run 一个 isolate、无网络、硬超时、errors-as-data（脚本错误作为工具结果回给模型而非炸 run）。
- 审批语义：`run_code` 硬编码 `RiskLevel::High`（approval.rs:131-135）——**一次审批盖住整个脚本**（沙箱内桥接调用不过审批闸门，Claude-Code 命令粒度惯例）；per-bridge-tool 审批是明确的 P2 deferral。
- 配置：`PtcConfig`（config/mod.rs:403）+ `ToolCallMode` 枚举（ptc crate，core 的 `[ptc]` 与沙箱执行器共享）。
- 工具调用计数：`run_code` 本身 + 脚本内的每个桥接调用都计入 `max_tool_calls`（runner.rs:126-132 注释）。

## Skill（SKILL.md 技能）

`core/src/skills.rs`：

- `discover_skills(sources: &[PathBuf]) -> (SkillsCatalog, Vec<SkillWarning>)`（:169）——多源目录**低→高优先级**扫描，重名后盖前；坏 skill 只产出 `SkillWarning` 不阻断启动。
- **渐进披露**两层：tier-1 = 启动时只把 name + description 注入 system prompt（`SkillsCatalog::catalog_prompt`，:339，有 max_list_chars 上限）；tier-2 = 正文由 `read_skill` 工具按需读（内置 MCP server 提供）。
- `Skill`（:31）= name/description/body/path；`skill_name_warnings`（:130）校验名字规范性。
- 约定格式：agentskills.io 的 SKILL.md。

## Approval（审批闸门 / 规则引擎）

`core/src/approval.rs`（tool-approval 重构后的规则引擎语义；线格式与审批卡交互见 protocol / mobile 章）：

- **三级等级** `ToolApprovalLevel`（:75）= `Manual`（每次必问，trigger=Manual）/ `Allow`（从不问，**默认值**）/ `Auto`（自动判断：启发式 `RiskLevel::Low`（纯只读）直接放行，Medium/High 进审批链，trigger=AutoRisk——mobile 装配的 AI 审查装饰器只滤 AutoRisk 请求，Manual 直通人工，P0-2）。
- **规则集** `ApprovalRules`（:121）= `{default_level, tools: BTreeMap<工具名,等级>, servers: BTreeMap<server 作用域,等级>}`；评估序 **`tools[工具名]` > `servers[server]` > `default_level`**（`level_for` :146；精确未命中做 ASCII 大小写不敏感回退，兼容 legacy AutoExcept 语义）。`servers` 键 = 审批作用域标识（聚合形态 = 下游 slug、host 内置裸名 = host 前缀），由 `mcp::server_slug_of`（mcp.rs:217，权威反解）产出。
- **`ApprovalManager::check(name, server, args, agent_id)`**（:506）是**聚合闸门**，每次工具执行前被咨询（runner.rs `ToolExecutor::execute` 顶部唯一闸口：registry namespace 经 `server_slug_of` 反解注入 server 作用域）：等级评估 → 不需要 → Approved；需要且有 callback → `callback.decide()`（同步阻塞）；需要但无 callback → Approved（非交互直通，CLI `NonInteractiveGate` 依赖该语义）。**每个**工具调用都过它——含 `call_agent` 与 `run_code`，无绕过路径（模块头注释钉死）。
- **热更新**：规则存 `Arc<RwLock<ApprovalRules>>`（Clone 共享同一 cell），`set_rules` / 外部 `rules_handle()` 改写对**进行中回合**的下一次 check 即生效（P0-5 红线：装配侧必须 `ApprovalManager::with_rules(bridge.rules_handle())` 注入共享 cell——重建 manager 会拿到游离 cell，热更永远到不了引擎且无编译错误）。
- **legacy 映射（P0-4，钉死勿改）**：`ApprovalPolicy::Auto` → default=Allow；`Manual` → default=Manual；`AutoExcept(list)` → default=Allow + `tools[每项]=Manual`。`rules_from_config`（:218）是 `[approval]` 段 → 生效规则的**单一推导函数**（legacy 铺底 → `default_level` 覆盖 → `tool_levels`/`server_levels` 逐键合并；段缺省 = 纯 Allow 基线——mobile 的 Auto 默认是装配层显式 opt-in，不在这里做）。
- `ApprovalTrigger`（:248）= Manual / AutoRisk（本次询问由谁触发）；`ApprovalRequest`（:266）携带 `server: Option<String>` / `trigger` / `note`（AI 审查理由——core 恒 None，mobile 审查装饰器回填）。
- `RiskLevel`（:52）= Low / Medium / High；`assess_risk`（:398）按工具名子串启发式（高险：bash/shell/exec/write/delete/curl/sql…；低险：read/list/grep/find/info…；未知 Medium；`run_code` 硬编码 High——**一次审批盖住整个脚本**，沙箱内桥接调用不过闸门，Claude-Code 命令粒度惯例；会话级放行 `run_code` 亦盖全脚本）。Auto 等级下启发式是 AI 审查的**预过滤**（Low 不进审查直接放行，省一次 LLM 调用）。
- 审批 watchdog（runner.rs）：连续 Denied 按工具名计数中止 run——**只有 deny 计数**（无「一直禁止」），放行类 choice 一律不触发。
- 决策审计走 tracing（target `openslate_approval`），**不写 store**（AI 审查 safe 静默放行同样只进日志、不进转写；人工审批才进转写条目）。

## Session（会话）

`openslate-session/src/state.rs:93` `CoreInner`（由 `SessionCore` 的 std Mutex 包着）：

- 内容：`session_id`/`session_label`/`history: Vec<Message>`（真相源）/`transcript: Vec<EntryDto>`（展示镜像）/`running`/`compacting`/深度与工具计数/`model_alias`/`cancel`/`config`/`agent_tree`/`skills`/`store`/`session_run`/`manager: Option<RunManager>`（在家/引擎持有两态槽位）/`engine_task`/`paths`/`stats`/`stream`（流式缓冲）/`tool_timers`/`pending_step_meta`。
- 生命周期：进程内一条（server 单会话；mobile 一条）；`NewSession` 清空 history/transcript、换 session_id、关旧 run 行、stats 归零，并调 `approval.clear_session_scopes()` 清空会话作用域审批集合（`SessionApprovalBridge` 的 `allowed_tools`/`allowed_servers` 双集合——「当前会话」语义；持久化规则不随会话重置）。
- server 与 mobile 两侧同语义——这正是 session crate 抽取的目的。
- 锁与顺序不变量见 [../02-core/openslate-session.md](../02-core/openslate-session.md)。

## 概念间的从属关系图

```text
Session（会话，跨回合）
 └─ Run（回合）            runs 表一行
     ├─ Step×N             steps 表（model_call/tool_call/child_agent_call）
     ├─ ExecutionTree      execution_nodes 表（run 结束整树写）
     │   └─ 子代理递归节点  AgentTree 的静态 children 在运行时的实例化
     ├─ Messages×N         messages 表（seq 单调，逐条即时落）
     └─ 转写镜像           transcript: Vec<EntryDto>（展示层，session 持有）

配置侧（静态）：
 Providers ── Models ── Levels ── Capabilities     （四级解析链）
 Agents（AgentTree）──引用── Levels
 Tools：内置 MCP（裸名）+ 外部 MCP（{server}_{tool}）+ host 注入
 Skills：tier-1 注入 system prompt / tier-2 read_skill 按需读
```
