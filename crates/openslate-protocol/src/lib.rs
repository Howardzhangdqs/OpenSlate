//! openslate server/client 共享协议（web-1）。
//!
//! 本 crate 是 server 与 client（TUI）两侧共用的**唯一**协议真相源：
//! WebSocket 上下行消息（`ClientMsg` / `ServerMsg`）与全部 DTO。
//! 纯数据层 —— 不依赖 tokio / axum，server 与 client 各自装配传输。
//!
//! 线格式：每条 WS text frame = 一个 JSON 对象，tagged enum
//! （`{"type":"...","...字段}` / EntryDto 用 `"kind"`），字段名 snake_case。
//! 契约测试（tests/contract.rs）逐变体钉死样例字节，**任何字段增删改名
//! 都必须同步改契约测试并升 [`PROTOCOL_VERSION`]**。
//!
//! DTO 原则（spec §2）：core 已有 serde derive 的类型
//! （`Message` / `Usage` / `RunId` / `RunStatus`）直接嵌入；core 仅
//! Deserialize 的 config 类型（`ProviderConfig` / `ModelConfig`）在协议侧
//! 定义镜像 DTO + `From` 转换，避免给 core 加 Serialize 耦合。

use std::collections::BTreeMap;

use openslate_core::agent_tree::AgentTree;
use openslate_core::approval::ApprovalRequest;
use openslate_core::config::{LimitsConfig, ModelConfig, ProviderConfig};
use openslate_core::types::{Message, RunId, RunStatus, Usage};

/// 当前协议版本。`hello.proto` 不等即拒连。
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// DTO：审批
// ---------------------------------------------------------------------------

/// 审批横幅摘要（TUI ApprovalSummary 的协议镜像）。`arguments` 为
/// 单行化 JSON，超过 120 字符截断加 `…`（与 TUI 展示约束一致）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApprovalSummaryDto {
    pub tool_name: String,
    pub arguments: String,
    pub agent_id: String,
    /// `"low"` / `"medium"` / `"high"`。
    pub risk_level: String,
}

/// 审批摘要的参数截断上限（字符，非字节；CJK 安全）。
pub const APPROVAL_SUMMARY_CLIP_CHARS: usize = 120;

/// 单行化并按字符截断到 [`APPROVAL_SUMMARY_CLIP_CHARS`]。
fn clip_oneline(s: &str, limit: usize) -> String {
    let oneline: String = s
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if oneline.chars().count() <= limit {
        oneline
    } else {
        let head: String = oneline.chars().take(limit).collect();
        format!("{head}…")
    }
}

impl From<&ApprovalRequest> for ApprovalSummaryDto {
    fn from(req: &ApprovalRequest) -> Self {
        // 与 TUI ApprovalSummary 相同的单行化：紧凑 JSON，失败回退原文。
        let args =
            serde_json::to_string(&req.arguments).unwrap_or_else(|_| req.arguments.to_string());
        Self {
            tool_name: req.tool_name.clone(),
            arguments: clip_oneline(&args, APPROVAL_SUMMARY_CLIP_CHARS),
            agent_id: req.agent_id.clone(),
            risk_level: req.risk_level.to_string(),
        }
    }
}

/// 待审批条目（snapshot 内嵌）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingApprovalDto {
    pub id: u64,
    pub summary: ApprovalSummaryDto,
}

/// 客户端审批应答的三种选择（对应 TUI 的 y / n / a）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAnswerChoice {
    Approve,
    Deny,
    /// 本会话后续审批自动放行（写工具白名单）。
    ApproveAll,
}

// ---------------------------------------------------------------------------
// DTO：转写条目（TUI TranscriptEntry 的协议镜像）
// ---------------------------------------------------------------------------

/// 工具条目状态（EntryDto 专用；`tool_end` 事件本身不携带状态——
/// 与 TuiEvent::ToolEnd 一致，失败由回合结束的 Tool 消息折叠判定）。
/// Live 迁移仅 Running → Done；`failed` 只由折叠启发式产生。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ToolStatusDto {
    Running,
    Done {
        bytes: usize,
        /// 输出留存被截断时为 true（args/输出留存上限）。
        truncated: bool,
        /// live 完成路径的实测时长；折叠路径（rebuild 后）为 `None`。
        elapsed_ms: Option<u64>,
    },
    Failed {
        summary: String,
    },
}

/// 工具条目展开详情。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolEntryDetailDto {
    /// 完整调用参数（pretty JSON，与 TUI DisplayArgs 一致）。
    pub args: String,
    /// 工具输出全文；live 期为 `None`（回合结束从 Tool 消息回填）。
    pub output: Option<String>,
}

/// 转写条目。字段名与 TUI `TranscriptEntry` 一一对应；不可序列化的
/// `DisplayArgs` / `ToolEntryDetail` 降为 String / [`ToolEntryDetailDto`]。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryDto {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        name: String,
        args: String,
        /// live 条目恒为 `None`（core 的 on_tool_start 不传 id）；
        /// 客户端从 TurnSummaryDto.messages 重建折叠时可回填。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        status: ToolStatusDto,
        detail: ToolEntryDetailDto,
    },
    Approval {
        tool_name: String,
        /// `"approved"` / `"denied"` / `"approve-all"`（与 TUI 决议行一致）。
        decision: String,
    },
    Delegate {
        agent: String,
        done: bool,
    },
    StepBreak,
    Meta {
        text: String,
    },
}

// ---------------------------------------------------------------------------
// DTO：回合摘要 / 快照
// ---------------------------------------------------------------------------

/// 回合终态摘要（TUI TurnSummary 的可序列化投影；execution_tree 不进
/// 协议——客户端用它重建折叠 Tool 消息，不需要执行树）。
///
/// 不 derive `PartialEq`：内嵌的 core `Message` 未实现它。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnSummaryDto {
    pub run_id: RunId,
    pub status: RunStatus,
    pub total_steps: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    pub model: String,
    pub messages: Vec<Message>,
}

impl From<openslate_core::run_manager::ManagedRunResult> for TurnSummaryDto {
    fn from(r: openslate_core::run_manager::ManagedRunResult) -> Self {
        Self {
            run_id: r.run_id,
            status: r.status,
            total_steps: r.total_steps,
            total_input_tokens: r.total_input_tokens,
            total_output_tokens: r.total_output_tokens,
            total_cost_usd: r.total_cost_usd,
            model: r.model,
            messages: r.messages,
        }
    }
}

/// 全量快照：新连接 hello 成功后定向发送；`new_session` 重置后逐连接补发。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SnapshotDto {
    pub proto: u32,
    pub session_id: String,
    pub session_label: String,
    /// 转写条目镜像（字段名 `transcript`，同 spec §3）。
    pub transcript: Vec<EntryDto>,
    pub running: bool,
    pub depth_cur: u32,
    pub agents_running: u32,
    pub tool_calls_cur: u32,
    pub model_alias: String,
    pub pending_approval: Option<PendingApprovalDto>,
    pub config: ConfigViewDto,
    /// 会话累计统计（GAP-3）：server 是真相源，重连/新客户端 attach
    /// 不清零；`turn_ok` 后由 server 更新。`new_session` 归零。
    pub session_stats: SessionStatsDto,
}

/// 会话累计统计（server 侧维护）。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionStatsDto {
    pub turns: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
}

impl Default for SessionStatsDto {
    fn default() -> Self {
        Self {
            turns: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// DTO：配置视图（model-mgmt / REST /api/config 共用）
// ---------------------------------------------------------------------------

/// provider 条目（`[providers.<name>]` 的协议镜像）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderDto {
    pub base_url: String,
    pub api_key_env: String,
    /// `"openai"` / `"anthropic"` / `"gemini"` / `"ollama"`；`None` =
    /// 未配置（按 base_url 猜测的旧行为）。
    #[serde(default)]
    pub adapter: Option<String>,
    /// 人类可读显示名（中文/空格/大小写均可）；`None` = 显示用键名。
    /// 键名（`name`）恒为内部 ID：模型引用、env 派生、Keystore 均按 ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub max_attempts: u32,
    pub retry_base_ms: u64,
}

impl From<&ProviderConfig> for ProviderDto {
    fn from(p: &ProviderConfig) -> Self {
        Self {
            base_url: p.base_url.clone(),
            api_key_env: p.api_key_env.clone(),
            adapter: p.adapter.clone(),
            title: p.title.clone(),
            max_attempts: p.max_attempts,
            retry_base_ms: p.retry_base_ms,
        }
    }
}

impl From<ProviderDto> for ProviderConfig {
    fn from(p: ProviderDto) -> Self {
        ProviderConfig {
            base_url: p.base_url,
            api_key_env: p.api_key_env,
            adapter: p.adapter,
            title: p.title,
            max_attempts: p.max_attempts,
            retry_base_ms: p.retry_base_ms,
        }
    }
}

/// 模型库条目（`[models.<entry>]` 的协议镜像）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelDto {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub max_context_tokens: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub supports_tool_call: bool,
    #[serde(default)]
    pub supports_vision: bool,
    #[serde(default)]
    pub supports_reasoning: bool,
    #[serde(default)]
    pub input_price_per_mtok: Option<f64>,
    #[serde(default)]
    pub output_price_per_mtok: Option<f64>,
}

impl From<&ModelConfig> for ModelDto {
    fn from(m: &ModelConfig) -> Self {
        Self {
            provider: m.provider.clone(),
            model: m.model.clone(),
            max_context_tokens: m.max_context_tokens,
            max_output_tokens: m.max_output_tokens,
            supports_tool_call: m.supports_tool_call,
            supports_vision: m.supports_vision,
            supports_reasoning: m.supports_reasoning,
            input_price_per_mtok: m.input_price_per_mtok,
            output_price_per_mtok: m.output_price_per_mtok,
        }
    }
}

impl From<ModelDto> for ModelConfig {
    fn from(m: ModelDto) -> Self {
        ModelConfig {
            provider: m.provider,
            model: m.model,
            max_context_tokens: m.max_context_tokens,
            max_output_tokens: m.max_output_tokens,
            supports_tool_call: m.supports_tool_call,
            supports_vision: m.supports_vision,
            supports_reasoning: m.supports_reasoning,
            input_price_per_mtok: m.input_price_per_mtok,
            output_price_per_mtok: m.output_price_per_mtok,
        }
    }
}

/// skill 摘要（name + description，渐进披露第一层）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillInfoDto {
    pub name: String,
    pub description: String,
}

/// 执行限额（`[limits]` 的协议镜像，GAP-1：客户端 make_ctx 与上下文
/// 仪表需要；`Option` 语义在 server 侧已用默认值解掉）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LimitsDto {
    pub max_steps: u32,
    pub max_depth: u32,
    pub max_tool_calls: u32,
    pub max_child_agent_calls: u32,
    pub timeout_ms: u64,
    pub max_context_messages: u32,
    pub max_context_bytes: u32,
    pub max_output_bytes: u32,
    pub auto_compact: bool,
    pub parallel_tool_calls: bool,
}

impl From<&LimitsConfig> for LimitsDto {
    fn from(l: &LimitsConfig) -> Self {
        Self {
            max_steps: l.max_steps,
            max_depth: l.max_depth,
            max_tool_calls: l.max_tool_calls,
            max_child_agent_calls: l.max_child_agent_calls,
            timeout_ms: l.timeout_ms,
            max_context_messages: l.max_context_messages,
            max_context_bytes: l.max_context_bytes,
            max_output_bytes: l.max_output_bytes,
            auto_compact: l.auto_compact,
            parallel_tool_calls: l.parallel_tool_calls,
        }
    }
}

/// agents 树节点（GAP-1：agents 浮层 set_root 与 models_change_guard
/// 的引用检查需要；`children` 递归展开，顺序 = 配置 children 声明序）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentNodeDto {
    pub id: String,
    pub name: String,
    pub model: String,
    pub children: Vec<AgentNodeDto>,
}

impl From<&AgentTree> for AgentNodeDto {
    fn from(tree: &AgentTree) -> Self {
        fn build(tree: &AgentTree, id: &openslate_core::types::AgentId) -> AgentNodeDto {
            let node = tree
                .get_agent(id)
                .expect("tree walk only visits existing nodes");
            AgentNodeDto {
                id: node.id.0.clone(),
                name: node.name.clone(),
                model: node.model_alias.clone(),
                children: node.children.iter().map(|c| build(tree, c)).collect(),
            }
        }
        build(tree, tree.root_id())
    }
}

/// 配置只读视图：`/provider` 浮层、REST `/api/config` 与 snapshot 的
/// 同一份数据（GAP-1：客户端是零本地配置的纯消费者，本视图必须自足）。
/// BTreeMap 保证 key 有序（快照 diff / 契约测试稳定）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConfigViewDto {
    pub providers: BTreeMap<String, ProviderDto>,
    pub models: BTreeMap<String, ModelDto>,
    pub levels: BTreeMap<String, String>,
    /// 执行限额（server 侧已解析默认值，非 Option）。
    pub limits: LimitsDto,
    /// agents 树（root 起递归）。
    pub agents: AgentNodeDto,
    pub skills: Vec<SkillInfoDto>,
    /// 实际生效配置文件路径（`--config` 显式指定或本地发现链结果）。
    pub active_config: String,
    /// 全局库路径（`~/.config/openslate/openslate.toml`）；无全局库时
    /// `None`。
    pub global_config: Option<String>,
    /// 本地叠加配置路径（`./.openslate/openslate.toml`）；无本地配置时
    /// `None`（`--config` 显式指定时与 active 相同）。
    pub local_config: Option<String>,
}

// ---------------------------------------------------------------------------
// DTO：REST 辅助
// ---------------------------------------------------------------------------

/// store 里的历史 run 行（REST `/api/sessions`）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionListItemDto {
    pub id: String,
    pub title: Option<String>,
    pub root_agent_id: String,
    pub status: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub cost_usd: f64,
}

/// notice 语义级别（渲染仅影响着色，不影响顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

// ---------------------------------------------------------------------------
// server 自描述文件（auto-attach-1：`<config 目录>/server.json`）
// ---------------------------------------------------------------------------

/// server.json 文件名（server 侧落盘 / TUI 侧发现共用，放协议 crate
/// 保持单一真相源）。
pub const SERVER_INFO_FILE: &str = "server.json";

/// server 启动时落盘的自描述信息：TUI 无 `--server` 时读取它自动
/// attach（发现链 flag > env > server.json）。server 优雅停机时删除；
/// 防双启动靠对其 `url` 的 health 探测（不是 pid 校验——pid 会复用）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServerInfo {
    /// 协议版本；读侧与 [`PROTOCOL_VERSION`] 不符视为陈旧文件。
    pub proto: u32,
    /// 完整 WS 路径（如 `ws://127.0.0.1:7800/api/ws`）。
    pub url: String,
    pub pid: u32,
    pub port: u16,
    /// 实际 bind 地址（可能与 url host 不同——bind 0.0.0.0 时 url
    /// 仍写 127.0.0.1 供本机客户端）。
    pub bind: String,
    /// 启动时刻（ISO 8601，本地时区带偏移）。
    pub started_at: String,
    /// `--auth-token` 值；无鉴权为 `None`。文件权限 0600 的原因。
    #[serde(default)]
    pub token: Option<String>,
    /// server 进程工作目录（调试友好）。
    pub cwd: String,
}

// ---------------------------------------------------------------------------
// ClientMsg（上行）
// ---------------------------------------------------------------------------

/// 客户端 → server。首条必须是 [`ClientMsg::Hello`]。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// 握手：版本 + 可选 token。失败即被断开。
    Hello {
        proto: u32,
        #[serde(default)]
        token: Option<String>,
    },
    /// 提交一轮用户输入。
    Submit {
        text: String,
    },
    /// 审批应答。任一客户端首答生效；迟到应答收到定向 notice。
    ApprovalAnswer {
        id: u64,
        choice: ApprovalAnswerChoice,
    },
    /// 取消当前回合（复用 TUI Ctrl-C 语义：引擎侧 cancel）。
    Cancel,
    /// 新会话：历史/转写清空，session_id 更新，广播 session_reset。
    NewSession,
    /// 切模型级别（alias 必须可解析，server 广播 model_changed）。
    SetModel {
        alias: String,
    },
    UpsertProvider {
        name: String,
        provider: ProviderDto,
    },
    DeleteProvider {
        name: String,
    },
    UpsertModel {
        entry: String,
        model: ModelDto,
    },
    DeleteModel {
        entry: String,
    },
    SetLevel {
        level: String,
        entry: String,
    },
    DeleteLevel {
        level: String,
    },
    /// 新增/更新 MCP server（Streamable HTTP 条目；手机端 mcp-host 场景）。
    /// headers 如 `{ "Authorization": "Bearer <token>" }`，可选。
    UpsertMcpServer {
        name: String,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<std::collections::HashMap<String, String>>,
    },
    /// 删除 MCP server 条目（不存在时静默成功，幂等）。
    RemoveMcpServer {
        name: String,
    },
    /// 直接粘贴 API key：server 派生 `<NAME>_API_KEY` 写 `.env`（0600），
    /// 值不回显、不广播。
    SetApiKey {
        provider: String,
        value: String,
    },
}

// ---------------------------------------------------------------------------
// ServerMsg（下行）
// ---------------------------------------------------------------------------

/// server → 客户端。同一连接内事件全序（单发送队列）。
///
/// 不 derive `PartialEq`：`TurnOk` 内嵌 core `Message`（无 PartialEq）；
/// 测试侧用 `serde_json::to_value` 比对。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// 全量快照（hello 成功后 / session_reset 后，逐连接定向）。
    /// `Box` 仅为压 enum 变体尺寸（clippy large_enum_variant），
    /// serde 线格式与裸值完全一致。
    Snapshot {
        session: Box<SnapshotDto>,
    },
    RequestStart {
        step: u32,
        model: String,
    },
    FirstToken,
    /// 答案流式增量。
    Delta {
        text: String,
    },
    /// 思维链流式增量。
    Reasoning {
        text: String,
    },
    /// 输入 token 估算（流式期 `↑N`）。
    InputEstimate {
        tokens: u32,
    },
    /// 单次请求精确 usage。
    Usage {
        usage: Usage,
    },
    /// 请求结束（附带的 usage 行语义）。
    RequestEnd,
    /// 步进（step+1 / step=N）。
    StepEnd,
    /// 工具调用开始（镜像 TuiEvent::ToolStart；live 不带 call_id）。
    ToolStart {
        name: String,
        args: String,
    },
    /// 工具调用结束（镜像 TuiEvent::ToolEnd；失败与否由回合结束的
    /// Tool 消息折叠判定，事件本身只带字节数/截断位）。
    /// `preview` 为输出前若干字符（UI 投影兜底；None = 无输出或未携带）。
    ToolEnd {
        name: String,
        bytes: usize,
        truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    ApprovalRequested {
        id: u64,
        summary: ApprovalSummaryDto,
    },
    /// 审批已裁决（首答后广播；所有客户端清横幅）。`choice` 值域同
    /// [`ApprovalAnswerChoice`]（approve/deny/approve_all）。
    ApprovalResolved {
        id: u64,
        choice: String,
    },
    TurnOk {
        /// `Box` 同上，仅压尺寸。
        summary: Box<TurnSummaryDto>,
    },
    TurnError {
        message: String,
    },
    /// 配置 CRUD 成功后广播新视图（`Box` 同上，仅压尺寸）。
    ConfigChanged {
        config: Box<ConfigViewDto>,
    },
    ModelChanged {
        alias: String,
    },
    SessionReset,
    Notice {
        text: String,
        level: NoticeLevel,
    },
    /// 协议级错误（hello 版本不符 / token 校验失败 / 非法消息），
    /// 定向发送后随即断开。`code` 机器可读（`proto_mismatch` /
    /// `bad_token` / `bad_message`），`message` 人读中文。
    Error {
        code: String,
        message: String,
    },
}
