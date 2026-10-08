# 01-4 · 关键 trait 与抽象

> 本文收录贯穿架构的接缝（seam）型 trait：定义、语义约束、实现者清单。接缝的意义在于**让上层组合无需知道下层实现**（如 session 不知道事件去 WS 还是 FFI；runtime 不知道落盘是 SQLite 还是内存）。

## 1. ModelProvider —— 模型提供方

```rust
// crates/openslate-core/src/provider.rs:21
#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    async fn generate(&self, request: GenerateRequest) -> Result<ModelResponse, ProviderError>;
    async fn generate_stream(&self, request: GenerateRequest)
        -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>>;
    // generate_stream 有默认实现：包 generate()，发 Usage(如有) + Done 两个事件
    fn provider_name(&self) -> &str;
}
```

- `GenerateRequest`（provider.rs:12）：`model_id / system_prompt / messages / tools: Vec<ToolDefinition> / max_tokens / temperature`。
- `ModelStreamEvent`（types.rs:174）：`Delta(String)`（答案增量）/ `Reasoning(String)`（思维链增量）/ `Usage(Usage)` / `Done(ModelResponse)`（完整聚合响应）。
- **实现者**：
  - 生产唯一实现：`openslate-model-genai::GenaiProvider`（覆盖 openai/anthropic/gemini/ollama 四协议族）。
  - 测试桩：core 各测试里的 `ScriptedProvider`/`MockProvider`/`CountingProvider` 等；session/TUI 集成测试的 `ScriptedProvider`（经 `ProviderFactory` 注入）。
- 相关自由函数：`provider::list_remote_models(base_url, adapter, api_key)`（provider.rs:104）——adapter 感知的远端模型清单拉取（端点/鉴权矩阵见源码注释 ：96-103；Android 用 webpki 静态根证书）。

## 2. ProgressCallback —— 引擎 → UI 的同步进度回调

```rust
// crates/openslate-core/src/provider.rs:57
pub trait ProgressCallback: Send {
    fn on_request_start(&mut self, step: u32, model_id: &str);
    fn on_input_estimate(&mut self, _tokens: u32) {}   // 流式期 ↑N 估算（默认 no-op）
    fn on_first_token(&mut self);                       // TTFT
    fn on_delta(&mut self, text: &str);                 // 答案增量
    fn on_reasoning(&mut self, _text: &str) {}          // 思维链增量（默认 no-op）
    fn on_usage(&mut self, usage: Usage);
    fn on_request_end(&mut self);
    fn on_tool_start(&mut self, name: &str, args: &str);
    fn on_tool_end(&mut self, name: &str, bytes: usize, truncated: bool, preview: &str);
    fn on_step_end(&mut self) {}                        // 步进分隔（默认 no-op）
}
```

- **同步 trait**（`&mut self`、无 async）：引擎在循环里内联调用。这一约束向上传导成 session 的 std Mutex 决策（见 MsgSink 与锁序）。
- `on_tool_end` 的 `preview` 携带输出前若干字符（可能为空串）。
- **实现者**：CLI spinner；session 的 `SessionProgressBridge`（engine.rs:44，持 core 锁「先改镜像、再广播」）；runner 的 `ChildProgress`（子代理进度合并回父回调，runner.rs:1194）。

## 3. Tool / ToolExecutor —— 工具

```rust
// crates/openslate-core/src/tool.rs:17
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;                              // 模型可见名
    fn namespace(&self) -> Option<String> { None }       // PTC 沙箱暴露的命名空间（MCP server 别名）
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError>;
    fn to_definition(&self) -> ToolDefinition { /* name+description+schema */ }
}

// crates/openslate-core/src/tool.rs:54 —— runtime 消费的执行抽象
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolOutput;
    // 约定：内部错误转成 status=Error 的 ToolOutput 而非 Err，保证循环优雅继续
}
```

- **实现者（Tool）**：`McpTool`（外部/内置 MCP 工具的统一包装，mcp.rs:146）；`ChildAgentCallable`（`call_agent` 的**桩**——真正执行由 runner 拦截，callable.rs:50）；内置 server 后面的各工具；mobile 注入的 host 工具（`Box<dyn Tool>` 经 `register_boxed` 进注册表）。
- **实现者（ToolExecutor）**：`AgentRunner`（runner.rs:1248——拦截 `call_agent` 递归跑子代理，其余委托 `ToolRegistry`；这个接缝让 `execute_run` 保持通用）；测试里的各种 mock executor。
- `ToolRegistry`（tool.rs:66）：内部 `RwLock<HashMap<String, Arc<dyn Tool>>>`，装配后仍可运行期增删（mobile bash 工具热换名）。`try_register` 报 `ToolNameConflict`（启动期响亮失败优于静默遮蔽）；`register` 静默覆盖。

## 4. MessageSink —— 逐条消息持久化接缝

```rust
// crates/openslate-core/src/runtime.rs:47
#[async_trait::async_trait]
pub trait MessageSink: Send + Sync {
    async fn append(&self, message: &Message);   // 按会话顺序调用
}
```

- 调用时机：**execute_run 循环内联**——assistant 消息 / 工具结果一进对话就 await 落盘（"落盘后再 continue"），崩溃/取消/超时总留可恢复的部分转写。不与 watchdog select 交互、不 spawn 任务。
- 实现约束（trait 文档钉死）：实现必须廉价、从 runtime 视角**永不失败**（吞错 + log）、保持写序。
- **实现者**：store 的 `RunRecorder`（recorder.rs:246——`append` 吞错 warn；要报错的调用方用 fallible 孪生 `write_message`）。测试里的 `RecordingSink`。
- 只有 **root 层**收到 sink（child 层传 None——子层消息混入同一 run 行会破坏 resume 顺序，runner.rs:107-112 注释）。

## 5. ApprovalCallback —— 审批回调（阻塞式）

```rust
// crates/openslate-core/src/approval.rs:74
pub trait ApprovalCallback: Send + Sync {
    fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision;
}
```

- **同步阻塞设计**：`decide` 在运行时线程内联调用，可以阻塞（REPL 等用户 y/n/a；session 桥阻塞在 mpsc recv）。这一约束是 session 引擎任务用 `spawn_blocking` 的根因。
- `ApprovalRequest`（:47）：tool_name / arguments / agent_id / risk_level。`ApprovalDecision`（:60）：`Approved | Denied(String)`。
- **实现者**：session 的 `SessionApprovalBridge`；CLI 的交互 prompt / 非交互高风险闸门；测试的 `DenyAll`/`DenyTool`/`DenyFirstN`/`BlockingApprover`。

## 6. MsgSink —— 传输无关事件出口

```rust
// crates/openslate-session/src/state.rs:51
pub trait MsgSink: Send + Sync {
    fn broadcast(&self, msg: ServerMsg);                    // 必须非阻塞（入队即返回）
    fn send_to(&self, conn_id: u64, msg: ServerMsg) -> bool; // false = 连接已不在
    fn conn_ids(&self) -> Vec<u64>;
    fn count(&self) -> usize;      // 审批桥「无人可问即拒绝」的判据
    fn close_all(&self);           // 优雅停机
    fn as_any(&self) -> &dyn std::any::Any;  // 传输侧下沉具体类型（session 核心自身不调用）
}
```

- **实现必须非阻塞**：broadcast 会被引擎线程在 core 锁释放后同步调用，阻塞在此拖慢整个回合（trait 文档 :49-50 钉死）。
- **实现者**：server 的 `ConnectionHub`（fan-out 多连接，每连接一个队列 + 写任务——队列天然给出同一连接内事件全序）；mobile 的 `EventSink`（单队列 + 泵线程 → JSON 回调宿主）。

## 7. Callable —— 工具与子代理的公共基座

```rust
// crates/openslate-core/src/callable.rs:21
pub trait Callable: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    fn kind(&self) -> CallableKind;   // Tool | ChildAgent
}
```

- 设计意图：工具与子代理在"可被模型调用的东西"这一层同构。当前主要消费者是元数据层；实际执行走 `Tool`/`ToolExecutor`。
- `ChildAgentCallable`（:50）实现 `Tool` 但 `execute` 是桩——真正执行由 runner 在 `ToolExecutor::execute` 里拦截 `call_agent` 名字完成（schema 的 enum 字段钉死只能调自己的 agent_id，:82-98）。

## 8. ProviderFactory —— 测试接缝（类型别名）

```rust
// crates/openslate-session/src/state.rs:40
pub type ProviderFactory =
    Arc<dyn Fn(&OpenSlateConfig, &str) -> anyhow::Result<Box<dyn ModelProvider>> + Send + Sync>;
```

- 生产实现 = `build_provider_for_model`（app/model-genai 侧）；集成测试注入 ScriptedProvider（与 TUI 同构）。每回合 submit 时调用（config, model_alias）→ 新 provider。

## 9. 编排职责链（谁组装谁）

```text
openslate-app::wiring::AppContext
  config 加载（分层合并）→ 校验 → SqliteStore → AgentTree
  → ToolRegistry（内置 MCP + 外部 MCP 连接 + host 注入）
  → RunManager{config, agent_tree, tool_registry, skills, limits, approval, message_sink}
  装配一次，多回合复用（session 的 Option 槽位持有/取走）

core::run_manager::RunManager（run 编排入口）
  execute_with_run_id(run_id, provider, prior_messages, cancel, progress)
  → 组 AgentRunner（快照 approval/sink/cancel_token）

core::runner::AgentRunner（递归子代理编排器，impl ToolExecutor）
  拦截 call_agent → 递归执行子代理（CallerFrame 栈 + 子上下文隔离）
  其余工具 → 委托 ToolRegistry

core::runtime::execute_run（单 agent 主循环）
  组装上下文（system prompt + skills 注入 + history 窗口管理）
  → ModelProvider::generate_stream → 处理 Delta/Reasoning/Usage/Done
  → 模型给 tool_calls → ApprovalManager.check → 工具批并发执行
    （parallel_tool_calls 默认开；含 call_agent/run_code 的批强制串行）
  → 结果经 MessageSink 即时落盘 → 循环至无 tool_calls / 限额 / 取消
```

要点：

- `RunManager`/`AgentRunner` 用**所有权传递**而非共享引用注入关切（approval/message_sink/cancel 经 builder 方法 `with_*` 快照进来）。
- `execute_run` 保持单 agent 通用性靠的是 ToolExecutor 接缝——递归逻辑全在 runner。
- `ManagedRunResult`（run_manager.rs:46）：run_id/status/messages/total_steps/tokens/cost_usd/execution_tree/model/trace——`TurnSummaryDto::From` 的数据源。
