//! Configuration parsing for OpenSlate.
//!
//! Supports TOML for main config (`openslate.toml`) and Markdown with YAML
//! frontmatter for agent definitions (`agents/*.md`). All structs implement
//! `serde::Deserialize`.
//!
//! # Schema Overview
//!
//! ## `openslate.toml` (TOML)
//!
//! | Section     | Type                | Required | Description                        |
//! |-------------|---------------------|----------|------------------------------------|
//! | `project`   | `ProjectConfig`     | no       | Project metadata                   |
//! | `database`  | `DatabaseConfig`    | no       | SQLite database settings           |
//! | `prompts`   | `PromptsConfig`     | no       | Prompt template paths              |
//! | `limits`    | `LimitsConfig`      | no       | Execution limit defaults           |
//! | `providers` | `Map<String, ProviderConfig>` | yes | LLM provider endpoints |
//! | `models`    | `Map<String, ModelConfig>`    | yes | Model library entries (each binds provider + model id) |
//! | `levels`    | `Map<String, String>`         | no  | Model level → model library entry name (legacy configs without `[levels]` treat each `models` key directly as an alias) |
//! | `capabilities` | `Map<String, String>`      | no  | Capability id (`main`/`compact`/`title`) → level name; defaults via `capability_alias` |
//! | `trace`     | `TraceConfig`       | no       | Observability settings             |
//! | `builtin_tools` | `BuiltinToolsConfig` | no   | In-process builtin tool toggles    |
//! | `skills`    | `SkillsConfig`      | no       | Skill discovery / injection        |
//! | `ptc`       | `PtcConfig`         | no       | Programmatic Tool Calling settings |
//! | `approval`  | `Option<ApprovalConfig>` | no  | Tool approval gating (legacy `policy`/`tools` + new `default_level`/`tool_levels`/`server_levels`) |
//! | `tui`       | `TuiConfig`         | no       | TUI frontend (icon overrides)      |
//! | `ask_user`  | `AskUserConfig`     | no       | ask_user tool behavior (timeout/masking) |
//! | `notify`    | `NotifyConfig`      | no       | notify tool behavior (default wait timeout, S1b) |
//! | `attachments` | `AttachmentsConfig` | no     | Multimodal attachment ingest / send limits (multimodal input design §6.6) |
//!
//! ## `agents/*.md` (Markdown + YAML frontmatter)
//!
//! Each `.md` file defines one [`AgentConfig`](crate::types::AgentConfig).
//! The YAML frontmatter requires: `name`, `model`. Optional: `id` (defaults
//! to filename), `children` (list of agent ids), `tools` (list of tool names).
//! The body after the closing `---` becomes `default_prompt`.
//!
//! # Validation
//!
//! Use [`validation::validate_config`] for error-only checks,
//! [`validation::validate_strict`] for errors + warnings, or
//! [`validation::validate_config_full`] for a structured result.

pub mod merge;
pub mod persist;
pub mod validation;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::Deserialize;

use crate::error::ConfigError;
use crate::types::{AgentConfig, AgentId};

// ── Config structs ───────────────────────────────────────────────────────────

/// Top-level configuration parsed from `openslate.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenSlateConfig {
    #[serde(default)]
    pub project: Option<ProjectConfig>,
    #[serde(default)]
    pub database: Option<DatabaseConfig>,
    #[serde(default)]
    pub prompts: Option<PromptsConfig>,
    #[serde(default)]
    pub limits: Option<LimitsConfig>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
    /// Model levels (`[levels]`): level name → model library entry name
    /// (model-mgmt-1). Levels are what agents reference (`main`, `fast`, …);
    /// an entry in `[models]` binds a provider + concrete model id and is the
    /// shared library. Resolution precedence: a name found in `levels` is
    /// mapped to its entry first; names absent from `levels` fall back to the
    /// legacy direct `[models]` lookup, so configs without a `[levels]`
    /// section keep their exact pre-existing behavior.
    #[serde(default)]
    pub levels: HashMap<String, String>,
    /// Capabilities (`[capabilities]`): 固定功能 id → 代号（`[levels]` key
    /// 或 legacy `[models]` 条目名）。功能 id 见 [`crate::model_config`]
    /// 的 `CAP_*` 常量（`main` 主对话 / `compact` 上下文压缩 / `title`
    /// 会话标题生成）。缺省回退：`main`→"main"、`title`→"fast"
    /// （[`crate::model_config::capability_alias`]）；`compact` 未绑定
    /// 时由调用方决定——session 引擎继承会话当前模型
    /// （`model_alias`），不使用该函数的 "fast" 缺省。
    /// levels 是用户自由命名的"代号"；capabilities 决定每个功能用哪个
    /// 代号——两层解耦：换模型只动 levels 绑定，换功能分工只动
    /// capabilities。
    #[serde(default)]
    pub capabilities: HashMap<String, String>,
    #[serde(default)]
    pub trace: Option<TraceConfig>,
    /// MCP (Model Context Protocol) client servers.
    #[serde(default)]
    pub mcp: Option<McpConfig>,
    /// In-process builtin tool servers (`read_file` / `write_file` / `shell` /
    /// `edit_file`). Everything defaults to enabled.
    #[serde(default)]
    pub builtin_tools: BuiltinToolsConfig,
    /// Skills (`SKILL.md`) discovery and prompt-injection settings.
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Programmatic Tool Calling (`run_code` sandbox) settings.
    #[serde(default)]
    pub ptc: PtcConfig,
    /// Tool approval gating (`[approval]`). `None` when the section is
    /// absent — the CLI layer then derives the effective policy from
    /// interactivity and `--yes`.
    #[serde(default)]
    pub approval: Option<ApprovalConfig>,
    /// TUI frontend settings (`[tui]` — icon overrides). The section
    /// is TUI-only; other binaries ignore it.
    #[serde(default)]
    pub tui: TuiConfig,
    /// ask_user（Agent 向用户提问）工具行为（`[ask_user]`，ask-user
    /// 设计 r4：默认超时用户可配）。Deserialize-only：写回由 persist.rs
    /// 手拼 TOML（模块头约定），本 lane 未接写回。
    #[serde(default)]
    pub ask_user: AskUserConfig,
    /// notify（Agent 主动系统通知）工具行为（`[notify]`，notify 设计
    /// S1b：wait 模式默认等待时限）。Deserialize-only：写回由 persist.rs
    /// 手拼 TOML（模块头约定），本 lane 未接写回（文件配置 + 代码默认）。
    #[serde(default)]
    pub notify: NotifyConfig,
    /// 附件（多模态输入）限额与实验开关（`[attachments]`，存储 v5 设计
    /// §6.6；图片处理用户可配置：dim/处理后大小双旋钮）。validation
    /// 边界：512 ≤ dim ≤ 6000、1 ≤ max_image_mb ≤ 5、其余 mb ≥ 1。
    #[serde(default)]
    pub attachments: AttachmentsConfig,
}

/// Project metadata.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub name: Option<String>,
}

/// Database connection settings.
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default = "default_true")]
    pub wal: bool,
    #[serde(default = "default_busy_timeout")]
    pub busy_timeout_ms: u64,
}

/// Prompt template settings.
#[derive(Debug, Clone, Deserialize)]
pub struct PromptsConfig {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default = "default_profile")]
    pub default_profile: String,
    #[serde(default)]
    pub hot_reload: bool,
}

/// Execution limit defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    pub max_steps: u32,
    pub max_depth: u32,
    pub max_tool_calls: u32,
    pub max_child_agent_calls: u32,
    /// Model-request timeout budget, in milliseconds (default 60000). Dual
    /// semantics (fix-20): for NON-streaming model requests it is the TOTAL
    /// per-attempt budget (whole request/response); for STREAMING requests
    /// it is the IDLE budget — the maximum silence while waiting for
    /// response headers or between stream events, so a long answer whose
    /// tokens keep flowing is never cut off (connection establishment is
    /// separately bounded by a fixed 15s connect timeout). At the runtime
    /// layer (fix-21) it is the round-level budget for TOTAL LLM request
    /// time: approval waits (decide() blocking) and tool execution do not
    /// consume it; only the sum of the round's provider-await segments is
    /// checked against it.
    pub timeout_ms: u64,
    pub max_context_messages: u32,
    pub max_context_bytes: u32,
    pub max_output_bytes: u32,
    /// 长输出落盘阈值（字节，output_query 配套）。工具输出超过该值
    /// 且宿主注册了 `ToolOutputStorer` 时，全文（脱敏后）落盘、对话内
    /// 替换为头尾预览，模型可用 `output_query` 取回全文。`0` = 关闭
    /// （走旧截断路径）。默认 16384。未注册 storer 的宿主
    /// （CLI/TUI/server）行为完全不变。
    pub output_store_threshold: u32,
    /// Auto-compact the conversation history (LLM summary via the `fast`
    /// model, mechanical-concatenation fallback) once it crosses the
    /// context limits, before a turn is dispatched. Default: on. The
    /// manual `/compact` command works regardless of this flag.
    pub auto_compact: bool,
    /// P2-1: direct tool calls within one step run concurrently (batches
    /// containing `call_agent`/`run_code` stay sequential regardless).
    /// `false` restores the fully sequential tool loop. Default: on.
    pub parallel_tool_calls: bool,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_steps: 0,
            max_depth: 4,
            max_tool_calls: 20,
            max_child_agent_calls: 8,
            timeout_ms: 60_000,
            max_context_messages: 16,
            max_context_bytes: 64_000,
            max_output_bytes: 65_536,
            output_store_threshold: 16_384,
            auto_compact: true,
            parallel_tool_calls: true,
        }
    }
}

/// A single LLM provider endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key_env: String,
    /// 人类可读显示名（中文/空格/大小写均可）；`None` = 显示用键名。
    /// toml 键恒为内部 ID：模型引用（`models.<entry>.provider`）、env 派生、
    /// Android Keystore 均按 ID 关联，改名（title）不影响它们。
    #[serde(default)]
    pub title: Option<String>,
    /// The genai adapter protocol (e.g. `"anthropic"`, `"gemini"`, `"openai"`,
    /// `"ollama"`). When omitted, defaults to `"openai"` (the common case for
    /// OpenAI-compatible endpoints) to avoid genai's silent Ollama fallthrough
    /// for unrecognized model names.
    #[serde(default)]
    pub adapter: Option<String>,
    /// Total attempts per call for transient failures (HTTP 429/5xx,
    /// network errors), INCLUDING the first attempt. `0` is clamped to `1`.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Exponential backoff base in milliseconds between attempts:
    /// `retry_base_ms * 2^(attempt-1) + jitter`, single sleep capped at 10s.
    #[serde(default = "default_retry_base_ms")]
    pub retry_base_ms: u64,
}

/// A named model alias referencing a provider.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub max_context_tokens: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default = "default_true")]
    pub supports_tool_call: bool,
    #[serde(default)]
    pub supports_vision: bool,
    #[serde(default)]
    pub supports_reasoning: bool,
    /// Standard input price, USD per million tokens (P2-3 cost tracking).
    /// Absent → the model's input usage is priced at 0.
    #[serde(default)]
    pub input_price_per_mtok: Option<f64>,
    /// Standard output price, USD per million tokens (P2-3 cost tracking).
    /// Absent → the model's output usage is priced at 0.
    #[serde(default)]
    pub output_price_per_mtok: Option<f64>,
    /// Thinking effort（推理力度）：七档全集 `"none" | "minimal" | "low"
    /// | "medium" | "high" | "xhigh" | "max"`（各 provider 实际档位不一，
    /// 如 GLM-5.3 仅 low/high/max、OpenAI/Claude 有 xhigh/max——存全集、
    /// 由 UI 按模型能力裁剪）。`None` = Auto：不传该字段，跟随 provider
    /// 默认；`"none"` = 显式硬关闭思考（wire 发
    /// `reasoning_effort:"none"`，仅支持的家族有效，如 GLM-5.2 /
    /// Gemini 2.5；GLM-5.3 / DeepSeek 不支持）——两种语义互不等价。
    /// 其他非法值（手改 TOML 拼错等）在消费端（`resolve_model`）容忍
    /// 处理：warn 后按 None，不 panic。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// Tracing / observability settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TraceConfig {
    pub enabled: bool,
    pub store_sqlite: bool,
    pub default_export_format: String,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            store_sqlite: true,
            default_export_format: "chrome-json".to_owned(),
        }
    }
}

/// MCP (Model Context Protocol) client configuration.
///
/// Declares external MCP servers whose tools are registered into the
/// `ToolRegistry` at startup. See [`TransportConfig`] for connection options.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: HashMap<String, McpServerConfig>,
    /// 工具注册名模板（大模板，三占位符）：占位符 {prefix}（[mcp.servers]
    /// 键名，App 侧「服务名/前缀」设置）、{server}（上游 MCP 服务器名 =
    /// 聚合名的 `{slug}` 段，如 Server_Memory）与 {tool}（不带前缀的
    /// 工具名）。缺省 = "{prefix}_{server}__{tool}"（与历史
    /// `前缀_上游全名` 展开逐字节一致）；None/空白同缺省。除占位符外仅
    /// 允许 [A-Za-z0-9_.-]（校验见 validation.rs）。**仅作用于含 `__` 的
    /// 聚合上游名**——裸名上游（host-builtin / 桌面直连）走
    /// [`Self::builtin_tool_name_template`] 小模板。只影响注册名：
    /// tools/call 路由（definition_name）与 PTC namespace 语义不变。保持
    /// `Deserialize`-only：写回由 persist.rs 手拼 TOML（模块头约定）。
    #[serde(default)]
    pub tool_name_template: Option<String>,
    /// builtin 工具名模板（小模板，双模板模型的另一半）：作用于**不含
    /// `__` 的裸上游名**——聚合 host 内置 server 列出的裸名（bash /
    /// read_file…）与桌面直连 server 的裸名共用同一条展开分支，改这一
    /// 个字段同时影响两者。占位符仅 {prefix}/{tool}（裸名没有 server
    /// slug 段，`{server}` 不允许出现）；缺省 = "{prefix}_{tool}"（与
    /// legacy `前缀_上游全名` 逐字节一致，None/空白同缺省）；除占位符外
    /// 仅 [A-Za-z0-9_.-] 且**禁字面 `__`**——裸名注册必须与聚合形态
    /// `slug__tool` 保持可区分（校验见 validation.rs）。生效值统一经
    /// [`Self::builtin_template_or_default`] 解析（空白回落默认），装配 /
    /// 热重载 / FFI 共用，勿各处自写 blank-filter。写回由 persist.rs
    /// `set_mcp_builtin_tool_name_template`（空串删键回默认）。
    #[serde(default)]
    pub builtin_tool_name_template: Option<String>,
}

impl McpConfig {
    /// 生效的 builtin 工具名模板（跨 crate 接缝契约）：None/空白视同
    /// 未设置，回落 [`crate::mcp::DEFAULT_BUILTIN_TOOL_NAME_TEMPLATE`]
    /// （`{prefix}_{tool}`）。装配期（wiring）、mobile 热重载、FFI 回显
    /// 统一走这里解析——空白语义只此一份，避免各处自写 blank-filter 后
    /// 对「空串 = 默认」的口径漂移。
    pub fn builtin_template_or_default(&self) -> &str {
        self.builtin_tool_name_template
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(crate::mcp::DEFAULT_BUILTIN_TOOL_NAME_TEMPLATE)
    }
}

/// A single MCP server connection.
///
/// `enabled` defaults to `true`. Each tool this server exposes is registered
/// under the template-pair expansion (`[mcp].tool_name_template` for
/// aggregated `slug__tool` names, `[mcp].builtin_tool_name_template` for
/// bare names; defaults `{prefix}_{server}__{tool}` / `{prefix}_{tool}`,
/// where `{prefix}` is this server's key in `[mcp.servers]`), so collisions
/// across servers (and with builtins) are avoided without any per-server
/// config.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub transport: TransportConfig,
}

/// How to reach an MCP server.
///
/// Internally-tagged by the `transport` field. This enum intentionally does NOT
/// use `#[serde(deny_unknown_fields)]` — that is incompatible with serde's
/// internally-tagged enum representation.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "transport")]
pub enum TransportConfig {
    /// Spawn a local subprocess speaking MCP over stdio.
    #[serde(rename = "stdio")]
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Option<HashMap<String, String>>,
    },
    /// Connect to a remote MCP server via Streamable HTTP.
    ///
    /// `headers`：随每个请求原样注入的自定义头（如 mcp-host 的
    /// `Authorization = "Bearer <token>"`；rmcp 仅保留 accept/session/
    /// protocol-version/last-event-id，Authorization 可安全透传）。
    #[serde(rename = "http")]
    Http {
        url: String,
        #[serde(default)]
        headers: Option<HashMap<String, String>>,
    },
}

/// Wrapper for the agents YAML file.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentsConfig {
    pub agents: Vec<AgentConfig>,
}

/// Toggles for the in-process builtin MCP tool servers.
///
/// `enabled` is the master switch; the per-tool flags gate individual tools
/// (the server owning a disabled tool is still started if any of its siblings
/// is enabled — e.g. `read_file` off, `write_file` on keeps the fs server up).
/// All flags default to `true`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuiltinToolsConfig {
    /// Master switch: `false` disables every builtin tool server.
    pub enabled: bool,
    /// Expose `read_file` (workspace-sandboxed file reading).
    pub read_file: bool,
    /// Expose `write_file` (workspace-sandboxed file writing).
    pub write_file: bool,
    /// Expose `shell` (run a command in the workspace root).
    pub shell: bool,
    /// Expose `edit_file` (context-patch editing of existing files).
    pub edit_file: bool,
}

impl Default for BuiltinToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            read_file: true,
            write_file: true,
            shell: true,
            edit_file: true,
        }
    }
}

/// Skills (SKILL.md) discovery and prompt-injection settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    /// Master switch: `false` disables skill discovery and injection.
    pub enabled: bool,
    /// Max characters for the skills catalog section in the system prompt.
    /// 0 = unlimited.
    pub max_list_chars: usize,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_list_chars: 8000,
        }
    }
}

/// TUI frontend settings (`[tui]`). Only `openslate-tui` consumes this
/// section; every other binary ignores it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiConfig {
    /// Icon glyph overrides.
    pub icons: TuiIconsConfig,
}

/// Per-slot icon glyph overrides (icons-4). Any default codepoint
/// table bets against some user's font — per-slot overrides let
/// missing glyphs self-heal: the TUI patches each named slot of the
/// CLI-selected tier. Keys are `IconSet` field names (`branch`,
/// `brand`, `thinking`, …); unknown keys and empty values are ignored
/// with a startup warning. Values may be multi-char; TOML
/// `\uXXXX`/`\UXXXXXXXX` escapes or literal glyphs both work.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiIconsConfig {
    /// Slot name → replacement glyph (empty map by default).
    pub overrides: HashMap<String, String>,
}

/// Programmatic Tool Calling (PTC) settings — see PTC_PLAN.md.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PtcConfig {
    /// Master switch: `false` (default) disables PTC entirely; every tool
    /// behaves as direct-only.
    pub enabled: bool,
    /// Hard wall-clock timeout per run_code execution, in milliseconds.
    pub timeout_ms: u64,
    /// QuickJS sandbox heap limit in bytes.
    pub memory_limit_bytes: usize,
    /// Truncation budget for run_code result+logs output, in bytes.
    pub max_output_bytes: usize,
    /// Max tool calls a single run_code script may make.
    pub max_tool_calls_per_run: usize,
    /// Max characters for the TypeScript declarations injected into the
    /// run_code tool description. 0 = unlimited.
    pub max_list_chars: usize,
    /// Disclosure strategy for the run_code tool description
    /// (`full` / `catalog` / `auto`, PTC_PLAN.md §5.2). `auto` (default):
    /// full signatures; over `max_list_chars`, demote both-mode tools
    /// (whose schemas already sit in the direct tool list) to catalog
    /// lines; still over, a pure catalog.
    pub disclosure: openslate_ptc::Disclosure,
    /// Max `list_tools`/`describe_tool` calls a single run_code script may
    /// make (separate, more generous budget than the tool-call budget).
    pub max_lookup_calls: usize,
    /// Per-tool call modes: glob pattern -> mode (direct/ptc/both).
    /// Longest matching pattern wins; unmatched tools default to `both`
    /// when PTC is enabled.
    pub tool_modes: BTreeMap<String, openslate_ptc::ToolCallMode>,
}

impl Default for PtcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_ms: 60_000,
            memory_limit_bytes: 67_108_864,
            max_output_bytes: 65_536,
            max_tool_calls_per_run: 16,
            max_list_chars: 8000,
            disclosure: openslate_ptc::Disclosure::Auto,
            max_lookup_calls: 50,
            tool_modes: BTreeMap::new(),
        }
    }
}

/// Policy selector for `[approval].policy` (config-file spelling).
///
/// The runtime [`ApprovalPolicy`](crate::approval::ApprovalPolicy) is
/// derived from this plus the `tools` list via
/// [`ApprovalConfig::to_policy`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicySetting {
    /// Approve every tool call without asking (default).
    #[default]
    Auto,
    /// Ask before every tool call.
    Manual,
    /// Ask only for the tools listed in `tools`.
    AutoExcept,
}

/// `[approval]` 新 schema 的等级拼写（tool-approval P1a）。与运行时
/// [`crate::approval::ToolApprovalLevel`] 一一对应，serde 形状同为
/// snake_case（`"manual"` / `"allow"` / `"auto"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalLevelSetting {
    /// 每次调用都询问。
    Manual,
    /// 从不询问，直接放行。
    Allow,
    /// 自动判断：启发式低风险放行，中/高风险进入审批链。
    Auto,
}

/// Tool approval gating settings (`[approval]`).
///
/// Controls whether tool calls require human approval before execution.
/// Interactive sessions prompt via an approval callback; non-interactive
/// runs install a high-risk gate unless `--yes` forces `auto`.
///
/// 双 schema 共存（tool-approval P1a）：legacy `policy`（+`tools`）保留
/// 可解析——旧文件零破坏；新键 `default_level` / `tool_levels` /
/// `server_levels` 经 [`crate::approval::rules_from_config`] 统一推导
/// （推导序：legacy 映射铺底 → `default_level` 覆盖 → 两个 level map
/// 逐键合并）。写回侧（`persist::set_approval_section`）整段重写为新
/// schema 并删除 legacy 键。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalConfig {
    /// Approval policy. Defaults to `auto`.
    #[serde(default)]
    pub policy: ApprovalPolicySetting,
    /// Tools that require approval under the `auto_except` policy
    /// (case-insensitive exact names). Ignored under `auto` / `manual`.
    #[serde(default)]
    pub tools: Vec<String>,
    /// 新 schema：全局缺省等级（覆盖 legacy `policy` 的映射结果）。
    /// `None` = 未配置（沿用 legacy 映射；两者都没写 → 规则基线 allow）。
    #[serde(default)]
    pub default_level: Option<ToolApprovalLevelSetting>,
    /// 新 schema：工具名 → 等级（逐键覆盖 legacy `tools` 的映射结果）。
    #[serde(default)]
    pub tool_levels: BTreeMap<String, ToolApprovalLevelSetting>,
    /// 新 schema：MCP server 作用域 → 等级。键 = 审批作用域标识
    /// （聚合形态下游 slug / host 前缀，见 `mcp::server_slug_of`）。
    #[serde(default)]
    pub server_levels: BTreeMap<String, ToolApprovalLevelSetting>,
}

impl ApprovalConfig {
    /// Map the config section to the runtime approval policy.
    pub fn to_policy(&self) -> crate::approval::ApprovalPolicy {
        match self.policy {
            ApprovalPolicySetting::Auto => crate::approval::ApprovalPolicy::Auto,
            ApprovalPolicySetting::Manual => crate::approval::ApprovalPolicy::Manual,
            ApprovalPolicySetting::AutoExcept => {
                crate::approval::ApprovalPolicy::AutoExcept(self.tools.clone())
            }
        }
    }
}

/// ask_user（Agent 向用户提问）工具行为（`[ask_user]`，ask-user 设计）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AskUserConfig {
    /// 默认等待时限；工具入参 `timeout` 逐次覆盖（设计 §2）。
    pub default_timeout: AskTimeout,
    /// 工具输出对已知密钥值打码（secret 子系统预留，默认开）。
    pub mask_secrets: bool,
}

impl Default for AskUserConfig {
    fn default() -> Self {
        Self {
            default_timeout: AskTimeout::Never,
            mask_secrets: true,
        }
    }
}

/// notify（Agent 主动系统通知）工具行为（`[notify]`，notify 设计 S1b；
/// 本阶段仅文件配置 + 代码默认值，设置页 UI 后补——descope 拍板）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotifyConfig {
    /// `wait:true` 模式默认等待时限；工具入参 `timeout` 逐次覆盖
    /// （`-1` = 永不）。与 ask_user 的 App 内「永不」首装默认不同属
    /// 有意差异：通知场景用户大概率不在屏幕前，「永不」会让 turn
    /// 无限挂起（notify 设计 §10 决策 9）。
    pub default_wait_timeout: AskTimeout,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            default_wait_timeout: AskTimeout::Seconds(300),
        }
    }
}

/// ask_user 等待时限。TOML 写法：`default_timeout = "never"` 或
/// `default_timeout = { seconds = 300 }`（serde 外部标签枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskTimeout {
    /// 永不超时（默认；移动端等人场景，进程存活即保持 pending）。
    #[default]
    Never,
    /// 阈值秒数；超时回填 timeout 结果（模型收到「用户可能不在，
    /// 自行判断继续」后继续）。`0` 秒视为立即过期，由消费端容忍。
    Seconds(u64),
}

/// 附件（多模态输入）限额与实验开关（`[attachments]`，存储 v5 设计
/// §6.6 最终形态；2026-10-21 起图片处理用户可配置）。
///
/// 默认值即安全值：图像等比缩到 `max_image_dim` 后**迭代压缩**到达
/// `max_image_mb`（q85→70→55 降档、再 dim×0.85 缩边重试，见 mobile
/// attachments.rs），单请求总内联 25MB，实验视频默认关。
/// 校验边界（validation.rs）：`512 ≤ max_image_dim ≤ 6000`、
/// `1 ≤ max_image_mb ≤ 5`（= ingest 出口硬校验的 5MB 公共分母）、
/// 其余 `*_mb ≥ 1`。
/// Deserialize-only：写回由 persist.rs 手拼 TOML（模块头约定）；
/// mobile 侧 FFI setter（set_image_processing）经 toml_edit 原地改写。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AttachmentsConfig {
    /// 图像等比缩放目标：max(w,h) 上限（用户旋钮；只缩不放）。归一化
    /// 迭代压缩以它为起点，超大小上限时逐档 ×0.85 收缩（地板 1024）。
    pub max_image_dim: u32,
    /// **处理后**图像大小上限（MB；用户旋钮）：快速通道 passthrough 与
    /// 迭代压缩的达标线（q85→70→55 → 缩边重试）。语义自 2026-10-21
    /// 由「ingest 入口源文件上限」迁移——入口防呆改固定宽裕常量
    /// （mobile `MAX_SOURCE_IMAGE_BYTES` 50MB），归一化总能压到达标。
    pub max_image_mb: u64,
    /// 视频落盘上限（passthrough 不转码；文件先落盘可重发、跨 run
    /// 复用，实际发送另受 `max_inline_video_mb` 约束）。
    pub max_video_mb: u64,
    /// 实际发送的内联视频上限（data:URL base64 内联量级，对齐 Gemini
    /// inline 惯例；200MB 是智谱 URL 托管上限，不是内联该碰的）。
    pub max_inline_video_mb: u64,
    /// 单请求附件总字节上限（Submit 门控 fail-fast，报错列出实际总量）。
    pub max_inline_total_mb: u64,
    /// data:URL 视频实验开关（智谱端 base64 视频未文档化，设计 §3.5；
    /// 默认关，开启后 Submit 门控才放行视频附件）。
    pub experimental_video: bool,
}

impl Default for AttachmentsConfig {
    fn default() -> Self {
        Self {
            max_image_dim: 2048,
            max_image_mb: 5,
            max_video_mb: 50,
            max_inline_video_mb: 20,
            max_inline_total_mb: 25,
            experimental_video: false,
        }
    }
}

// ── Default helpers ──────────────────────────────────────────────────────────

fn default_true() -> bool {
    true
}

fn default_busy_timeout() -> u64 {
    5000
}

fn default_profile() -> String {
    "default".to_owned()
}

fn default_max_attempts() -> u32 {
    3
}

fn default_retry_base_ms() -> u64 {
    500
}

// ── Parsing functions ────────────────────────────────────────────────────────

/// Parse the main `openslate.toml` content into an `OpenSlateConfig`.
///
/// `[ptc]` numeric floors (see `validation::ptc_floor_errors`) are enforced
/// here too, so a value that would brick the `run_code` sandbox (e.g.
/// `timeout_ms = 0`) fails at load time instead of at first use.
pub fn parse_openslate_toml(content: &str) -> Result<OpenSlateConfig, ConfigError> {
    let config: OpenSlateConfig =
        toml::from_str(content).map_err(|e| ConfigError::ParseError(e.to_string()))?;
    if let Some(first) = validation::ptc_floor_errors(&config.ptc).first() {
        return Err(ConfigError::ParseError(format!(
            "invalid [ptc] settings: {}: {}",
            first.field, first.message
        )));
    }
    Ok(config)
}

// ── Markdown frontmatter parsing ─────────────────────────────────────────────

/// Frontmatter fields deserialized from the YAML header of an agent `.md` file.
///
/// Unlike [`AgentConfig`], the `id` is optional (falls back to the filename)
/// and `default_prompt` is absent (it comes from the markdown body).
#[derive(Debug, Clone, Deserialize)]
pub struct AgentFrontmatter {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub tools: Vec<String>,
    /// 工具隐藏名单（glob 可）：从生效集合中扣除（见 [`crate::types::AgentConfig::hidden_tools`]）。
    #[serde(default)]
    pub hidden_tools: Vec<String>,
    #[serde(default)]
    pub children: Vec<String>,
}

/// Derive an agent id from a markdown filename.
///
/// Strips the `.md` extension, keeps only `[a-zA-Z0-9_]`, replaces other
/// characters with `_`, and converts to lowercase.
pub fn derive_id_from_filename(filename: &str) -> String {
    let stem = filename.strip_suffix(".md").unwrap_or(filename);
    stem.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Parse a single agent markdown file (with YAML frontmatter) into an [`AgentConfig`].
///
/// Expected format:
/// ```markdown
/// ---
/// name: My Agent
/// model: main
/// tools: [bash, read_file]
/// ---
/// You are a helpful assistant.
/// ```
///
/// - The `id` field is optional in the frontmatter; if absent it is derived
///   from `filename` via [`derive_id_from_filename`].
/// - The body after the closing `---` becomes `default_prompt`.
/// - UTF-8 BOM (`\u{feff}`) at the start is stripped before parsing.
pub fn parse_agent_markdown(content: &str, filename: &str) -> Result<AgentConfig, ConfigError> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);

    let content = content
        .strip_prefix("---")
        .ok_or_else(|| ConfigError::ParseError(format!("{filename}: no frontmatter delimiter")))?;

    let (yaml_str, body) = match content.find("\n---") {
        Some(pos) => {
            let yaml = &content[..pos];
            let body = &content[pos + "\n---".len()..];
            (yaml, body)
        }
        None => {
            return Err(ConfigError::ParseError(format!(
                "{filename}: unclosed frontmatter (missing closing ---)"
            )));
        }
    };

    let fm: AgentFrontmatter = serde_norway::from_str(yaml_str).map_err(|e| {
        ConfigError::ParseError(format!("{filename}: invalid frontmatter YAML: {e}"))
    })?;

    let id_string = fm.id.unwrap_or_else(|| derive_id_from_filename(filename));
    let id = AgentId(id_string);
    let children = fm.children.into_iter().map(AgentId).collect();
    let default_prompt = body.trim().to_owned();

    Ok(AgentConfig {
        id,
        name: fm.name,
        model: fm.model,
        children,
        tools: fm.tools,
        hidden_tools: fm.hidden_tools,
        default_prompt,
    })
}

/// Parse all `.md` files in a directory into an [`AgentsConfig`].
///
/// Files are filtered by the `.md` extension, parsed individually via
/// [`parse_agent_markdown`], and the resulting agents are sorted by `id`
/// alphabetically for deterministic output.
pub fn parse_agents_dir(dir: &Path) -> Result<AgentsConfig, ConfigError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| ConfigError::FileNotFound(format!("{}: {e}", dir.display())))?;

    let mut agents = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            ConfigError::ParseError(format!("{}: read_dir entry: {e}", dir.display()))
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown.md");
            let content = std::fs::read_to_string(&path)
                .map_err(|e| ConfigError::FileNotFound(format!("{}: {e}", path.display())))?;
            agents.push(parse_agent_markdown(&content, filename)?);
        }
    }

    agents.sort_by(|a, b| a.id.0.cmp(&b.id.0));

    Ok(AgentsConfig { agents })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Default value tests ──────────────────────────────────────────────

    #[test]
    fn default_limits_config() {
        let limits = LimitsConfig::default();
        assert_eq!(limits.max_steps, 0); // 0 = unlimited
        assert_eq!(limits.max_depth, 4);
        assert_eq!(limits.max_tool_calls, 20);
        assert_eq!(limits.max_child_agent_calls, 8);
        assert_eq!(limits.timeout_ms, 60_000);
        assert_eq!(limits.max_context_messages, 16);
        assert_eq!(limits.max_context_bytes, 64_000);
        assert_eq!(limits.max_output_bytes, 65_536);
        assert_eq!(limits.output_store_threshold, 16_384);
        assert!(limits.auto_compact);
        assert!(limits.parallel_tool_calls);
    }

    // ── [limits].parallel_tool_calls ─────────────────────────────────────

    #[test]
    fn parse_limits_parallel_tool_calls_explicit_false() {
        let toml = r#"
[limits]
max_steps = 5
parallel_tool_calls = false
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert!(!config.limits.expect("limits").parallel_tool_calls);
    }

    #[test]
    fn parse_limits_parallel_tool_calls_defaults_to_true_when_absent() {
        // A [limits] section without the field still defaults to on
        // (container-level #[serde(default)] fills from Default).
        let toml = r#"
[limits]
max_steps = 5
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert!(config.limits.expect("limits").parallel_tool_calls);
    }

    // ── [limits].output_store_threshold（长输出落盘，C2）─────────────────

    #[test]
    fn parse_limits_output_store_threshold_defaults_when_absent() {
        // 存量配置（无该键）自动获得默认 16384；显式 0 = 关闭。
        let toml = r#"
[limits]
max_steps = 5
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert_eq!(
            config.limits.expect("limits").output_store_threshold,
            16_384
        );

        let toml = r#"
[limits]
max_steps = 5
output_store_threshold = 0
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert_eq!(config.limits.expect("limits").output_store_threshold, 0);
    }

    // ── [ask_user]（Agent 向用户提问；ask-user 设计 r4）─────────────────

    #[test]
    fn ask_user_defaults_when_section_absent() {
        let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "m"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert_eq!(config.ask_user.default_timeout, AskTimeout::Never);
        assert!(config.ask_user.mask_secrets);
    }

    // ── [notify]（notify 设计 S1b）──────────────────────────────────────

    #[test]
    fn notify_defaults_and_explicit_override() {
        // 缺省 = 300 秒（与 ask_user 的 Never 首装默认有意不同：通知
        // 场景用户大概率不在屏幕前，「永不」会让 turn 无限挂起）。
        let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "m"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert_eq!(
            config.notify.default_wait_timeout,
            AskTimeout::Seconds(300)
        );

        // 显式覆盖（含 -1 语义的 never 形态）。
        let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "m"

[notify]
default_wait_timeout = { seconds = 60 }
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert_eq!(config.notify.default_wait_timeout, AskTimeout::Seconds(60));

        // deny_unknown_fields 拒拼写错误键。
        let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "m"

[notify]
default_wait_timeout = "never"
tiemout = 60
"#;
        assert!(parse_openslate_toml(toml).is_err(), "deny_unknown_fields 拒拼写错误键");
    }

    // ── [attachments]（多模态输入设计 §6.6）─────────────────────────────

    #[test]
    fn attachments_defaults_are_the_safe_values() {
        let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "m"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert_eq!(config.attachments, AttachmentsConfig::default());
        assert_eq!(config.attachments.max_image_dim, 2048);
        // 2026-10-21 语义迁移：max_image_mb = 处理后大小上限（默认 5），
        // 不再是 ingest 入口源文件上限（入口防呆改固定常量）。
        assert_eq!(config.attachments.max_image_mb, 5);
        assert_eq!(config.attachments.max_video_mb, 50);
        assert_eq!(config.attachments.max_inline_video_mb, 20);
        assert_eq!(config.attachments.max_inline_total_mb, 25);
        assert!(!config.attachments.experimental_video);
    }

    #[test]
    fn attachments_partial_override_fills_the_rest_from_default() {
        // 容器级 #[serde(default)]：只写实验开关，其余字段回落默认值。
        let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "m"

[attachments]
experimental_video = true
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert!(config.attachments.experimental_video);
        assert_eq!(config.attachments.max_image_dim, 2048);
        assert_eq!(config.attachments.max_inline_total_mb, 25);
    }

    #[test]
    fn attachments_unknown_key_rejected() {
        let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "m"

[attachments]
max_imege_dim = 1024
"#;
        assert!(parse_openslate_toml(toml).is_err(), "deny_unknown_fields 拒拼写错误键");
    }

    #[test]
    fn ask_user_explicit_never_and_seconds() {
        let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "m"

[ask_user]
default_timeout = "never"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert_eq!(config.ask_user.default_timeout, AskTimeout::Never);

        let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "m"

[ask_user]
default_timeout = { seconds = 300 }
mask_secrets = false
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert_eq!(config.ask_user.default_timeout, AskTimeout::Seconds(300));
        assert!(!config.ask_user.mask_secrets);
    }

    #[test]
    fn ask_user_rejects_unknown_fields() {
        // deny_unknown_fields：拼写错误早失败（与 [skills]/[ptc] 同款）。
        let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "m"

[ask_user]
defaut_timeout = "never"
"#;
        assert!(parse_openslate_toml(toml).is_err());
    }

    // ── [limits].auto_compact ────────────────────────────────────────────

    #[test]
    fn parse_limits_auto_compact_explicit_false() {
        let toml = r#"
[limits]
max_steps = 5
auto_compact = false
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert!(!config.limits.expect("limits").auto_compact);
    }

    #[test]
    fn parse_limits_auto_compact_defaults_to_true_when_absent() {
        // A [limits] section without the field still defaults to on
        // (container-level #[serde(default)] fills from Default).
        let toml = r#"
[limits]
max_steps = 5
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert!(config.limits.expect("limits").auto_compact);
    }

    #[test]
    fn parse_limits_auto_compact_defaults_to_true_without_section() {
        let toml = r#"
[project]
name = "test"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert!(config.limits.is_none());
    }

    #[test]
    fn default_trace_config() {
        let trace = TraceConfig::default();
        assert!(trace.enabled);
        assert!(trace.store_sqlite);
        assert_eq!(trace.default_export_format, "chrome-json");
    }

    #[test]
    fn default_builtin_tools_config_all_enabled() {
        let bt = BuiltinToolsConfig::default();
        assert!(bt.enabled);
        assert!(bt.read_file);
        assert!(bt.write_file);
        assert!(bt.shell);
        assert!(bt.edit_file);
    }

    // ── TOML parsing tests ───────────────────────────────────────────────

    #[test]
    fn parse_minimal_toml() {
        let toml = "";
        let config = parse_openslate_toml(toml).expect("empty toml should parse");
        assert!(config.project.is_none());
        assert!(config.database.is_none());
        assert!(config.models.is_empty());
        assert!(config.providers.is_empty());
    }

    #[test]
    fn parse_missing_models_section_empty_hashmap() {
        let toml = r#"
[project]
name = "test"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert!(config.models.is_empty());
        assert!(config.providers.is_empty());
        assert!(
            config.capabilities.is_empty(),
            "no [capabilities] section → empty map (defaults applied at use site)"
        );
    }

    #[test]
    fn parse_capabilities_section() {
        let toml = r#"
[providers.zhipu]
base_url = "https://api.example.com"
api_key_env = "ZHIPU_API_KEY"

[models.big]
provider = "zhipu"
model = "big-m"

[models.small]
provider = "zhipu"
model = "small-m"

[levels]
main = "big"
fast = "small"
vision = "big"

[capabilities]
main = "main"
compact = "fast"
title = "fast"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert_eq!(config.capabilities.get("main").map(String::as_str), Some("main"));
        assert_eq!(config.capabilities.get("compact").map(String::as_str), Some("fast"));
        assert_eq!(config.capabilities.get("title").map(String::as_str), Some("fast"));
        // levels 仍是自由命名：vision 无 capability 引用也照常解析。
        assert!(crate::model_config::resolve_model(&config, "vision").is_ok());
    }

    #[test]
    fn parse_invalid_toml_syntax() {
        let toml = "this is [ not valid { toml";
        let result = parse_openslate_toml(toml);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ConfigError::ParseError(_)),
            "expected ParseError, got {err:?}"
        );
    }

    #[test]
    fn parse_provider_config() {
        let toml = r#"
[providers.zhipu]
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key_env = "ZHIPU_API_KEY"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let zhipu = config.providers.get("zhipu").expect("provider zhipu");
        assert_eq!(zhipu.base_url, "https://open.bigmodel.cn/api/paas/v4");
        assert_eq!(zhipu.api_key_env, "ZHIPU_API_KEY");
    }

    #[test]
    fn parse_provider_retry_defaults() {
        let toml = r#"
[providers.zhipu]
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key_env = "ZHIPU_API_KEY"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let zhipu = config.providers.get("zhipu").expect("provider zhipu");
        assert_eq!(zhipu.max_attempts, 3, "default total attempts");
        assert_eq!(zhipu.retry_base_ms, 500, "default backoff base");
    }

    #[test]
    fn parse_provider_retry_custom_values() {
        let toml = r#"
[providers.zhipu]
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key_env = "ZHIPU_API_KEY"
max_attempts = 5
retry_base_ms = 250
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let zhipu = config.providers.get("zhipu").expect("provider zhipu");
        assert_eq!(zhipu.max_attempts, 5);
        assert_eq!(zhipu.retry_base_ms, 250);
    }

    #[test]
    fn parse_model_config_with_optional_fields() {
        let toml = r#"
[models.main]
provider = "zhipu"
model = "glm-5.1"
max_context_tokens = 200000
max_output_tokens = 131072
supports_tool_call = true
supports_vision = false
supports_reasoning = true
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let main = config.models.get("main").expect("model main");
        assert_eq!(main.provider, "zhipu");
        assert_eq!(main.model, "glm-5.1");
        assert_eq!(main.max_context_tokens, Some(200_000));
        assert_eq!(main.max_output_tokens, Some(131_072));
        assert!(main.supports_tool_call);
        assert!(!main.supports_vision);
        assert!(main.supports_reasoning);
    }

    #[test]
    fn parse_model_config_optional_fields_missing() {
        let toml = r#"
[models.bare]
provider = "p"
model = "m"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let bare = config.models.get("bare").expect("model bare");
        assert_eq!(bare.provider, "p");
        assert_eq!(bare.model, "m");
        assert_eq!(bare.max_context_tokens, None);
        assert_eq!(bare.max_output_tokens, None);
        assert!(bare.supports_tool_call);
        assert!(!bare.supports_vision);
        assert!(!bare.supports_reasoning);
    }

    // ── [models].input_price_per_mtok / output_price_per_mtok (P2-3) ────

    #[test]
    fn parse_model_config_pricing_both_fields() {
        let toml = r#"
[models.main]
provider = "zhipu"
model = "glm-5.1"
input_price_per_mtok = 0.5
output_price_per_mtok = 2.0
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let main = config.models.get("main").expect("model main");
        assert_eq!(main.input_price_per_mtok, Some(0.5));
        assert_eq!(main.output_price_per_mtok, Some(2.0));
    }

    #[test]
    fn parse_model_config_pricing_partial_configuration() {
        // Only the input price is set: output usage prices at 0 while the
        // model still counts as "pricing configured".
        let toml = r#"
[models.fast]
provider = "zhipu"
model = "fast-m"
input_price_per_mtok = 0.15
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let fast = config.models.get("fast").expect("model fast");
        assert_eq!(fast.input_price_per_mtok, Some(0.15));
        assert_eq!(fast.output_price_per_mtok, None);
    }

    #[test]
    fn parse_model_config_pricing_defaults_to_none_when_absent() {
        let toml = r#"
[models.main]
provider = "zhipu"
model = "glm-5.1"
supports_tool_call = true
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let main = config.models.get("main").expect("model main");
        assert_eq!(main.input_price_per_mtok, None);
        assert_eq!(main.output_price_per_mtok, None);
    }

    #[test]
    fn parse_database_config_defaults() {
        let toml = r#"
[database]
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let db = config.database.as_ref().expect("database section");
        assert!(db.path.is_none());
        assert!(db.wal);
        assert_eq!(db.busy_timeout_ms, 5000);
    }

    #[test]
    fn parse_trace_config_defaults() {
        let toml = r#"
[trace]
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let trace = config.trace.as_ref().expect("trace section");
        assert!(trace.enabled);
        assert!(trace.store_sqlite);
        assert_eq!(trace.default_export_format, "chrome-json");
    }

    #[test]
    fn parse_builtin_tools_defaults_when_section_absent() {
        let config = parse_openslate_toml("").expect("empty toml should parse");
        assert!(config.builtin_tools.enabled);
        assert!(config.builtin_tools.read_file);
        assert!(config.builtin_tools.write_file);
        assert!(config.builtin_tools.shell);
        assert!(config.builtin_tools.edit_file);
    }

    #[test]
    fn parse_builtin_tools_partial_override() {
        let toml = r#"
[builtin_tools]
shell = false
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let bt = &config.builtin_tools;
        assert!(bt.enabled, "unspecified flags keep their defaults");
        assert!(bt.read_file);
        assert!(bt.write_file);
        assert!(!bt.shell);
        assert!(bt.edit_file);
    }

    #[test]
    fn parse_builtin_tools_disabled_entirely() {
        let toml = r#"
[builtin_tools]
enabled = false
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert!(!config.builtin_tools.enabled);
    }

    #[test]
    fn parse_builtin_tools_unknown_field_rejected() {
        let toml = r#"
[builtin_tools]
teleport = true
"#;
        assert!(parse_openslate_toml(toml).is_err());
    }

    #[test]
    fn parse_builtin_tools_rejects_legacy_tool_flags() {
        // Migration foot-gun: the old builtin tools (current_time, list_dir)
        // no longer exist. deny_unknown_fields must reject their flags with a
        // clear "unknown field" error instead of silently ignoring them.
        let toml = r#"
[builtin_tools]
current_time = false
"#;
        let err = parse_openslate_toml(toml).expect_err("legacy flag must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field"),
            "expected 'unknown field' in error, got: {msg}"
        );
        assert!(
            msg.contains("current_time"),
            "error should name the offending field, got: {msg}"
        );
    }

    #[test]
    fn default_skills_config() {
        let skills = SkillsConfig::default();
        assert!(skills.enabled);
        assert_eq!(skills.max_list_chars, 8000);
    }

    #[test]
    fn parse_skills_defaults_when_section_absent() {
        let config = parse_openslate_toml("").expect("empty toml should parse");
        assert!(config.skills.enabled);
        assert_eq!(config.skills.max_list_chars, 8000);
    }

    #[test]
    fn parse_skills_custom_values() {
        let toml = r#"
[skills]
enabled = false
max_list_chars = 1200
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert!(!config.skills.enabled);
        assert_eq!(config.skills.max_list_chars, 1200);
    }

    #[test]
    fn parse_skills_partial_override() {
        let toml = r#"
[skills]
max_list_chars = 42
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert!(
            config.skills.enabled,
            "unspecified flags keep their defaults"
        );
        assert_eq!(config.skills.max_list_chars, 42);
    }

    #[test]
    fn parse_skills_unknown_field_rejected() {
        let toml = r#"
[skills]
telepathy = true
"#;
        let err = parse_openslate_toml(toml).expect_err("unknown field must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field"),
            "expected 'unknown field' in error, got: {msg}"
        );
        assert!(
            msg.contains("telepathy"),
            "error should name the offending field, got: {msg}"
        );
    }

    // ── [tui] parse tests ────────────────────────────────────────────

    #[test]
    fn parse_tui_defaults_when_section_absent() {
        let config = parse_openslate_toml("").expect("empty toml should parse");
        assert!(
            config.tui.icons.overrides.is_empty(),
            "no [tui] section = empty override map"
        );
    }

    #[test]
    fn parse_tui_icon_overrides() {
        let toml = r#"
[tui.icons]
overrides = { branch = "\uF418", brand = "★" }
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert_eq!(config.tui.icons.overrides.len(), 2);
        assert_eq!(
            config.tui.icons.overrides.get("branch").map(String::as_str),
            Some("\u{F418}"),
            "TOML \\uXXXX escape decodes to the PUA codepoint"
        );
        assert_eq!(
            config.tui.icons.overrides.get("brand").map(String::as_str),
            Some("★"),
            "literal glyphs pass through unchanged"
        );
    }

    #[test]
    fn parse_tui_unknown_field_rejected() {
        let toml = r#"
[tui.icons]
foo = 1
"#;
        let err = parse_openslate_toml(toml).expect_err("unknown field must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field"),
            "expected 'unknown field' in error, got: {msg}"
        );
        assert!(
            msg.contains("foo"),
            "error should name the offending field, got: {msg}"
        );
    }

    // ── [approval] parse tests ────────────────────────────────────────────

    #[test]
    fn default_approval_config_is_auto() {
        let approval = ApprovalConfig::default();
        assert_eq!(approval.policy, ApprovalPolicySetting::Auto);
        assert!(approval.tools.is_empty());
        assert_eq!(approval.to_policy(), crate::approval::ApprovalPolicy::Auto);
    }

    #[test]
    fn parse_approval_absent_section_is_none() {
        let config = parse_openslate_toml("").expect("empty toml should parse");
        assert!(config.approval.is_none());
    }

    #[test]
    fn parse_approval_section_present_defaults_policy_auto() {
        let toml = r#"
[approval]
tools = ["shell"]
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let approval = config.approval.as_ref().expect("section present");
        assert_eq!(approval.policy, ApprovalPolicySetting::Auto);
        assert_eq!(approval.tools, vec!["shell".to_owned()]);
    }

    #[test]
    fn parse_approval_manual() {
        let toml = r#"
[approval]
policy = "manual"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let policy = config
            .approval
            .as_ref()
            .expect("section present")
            .to_policy();
        assert_eq!(policy, crate::approval::ApprovalPolicy::Manual);
    }

    #[test]
    fn parse_approval_auto_except_with_tools() {
        let toml = r#"
[approval]
policy = "auto_except"
tools = ["shell", "run_code"]
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let policy = config
            .approval
            .as_ref()
            .expect("section present")
            .to_policy();
        assert_eq!(
            policy,
            crate::approval::ApprovalPolicy::AutoExcept(vec![
                "shell".to_owned(),
                "run_code".to_owned()
            ])
        );
    }

    #[test]
    fn parse_approval_unknown_field_rejected() {
        let toml = r#"
[approval]
policy = "auto"
vibes = false
"#;
        let err = parse_openslate_toml(toml).expect_err("unknown field must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field"),
            "expected 'unknown field' in error, got: {msg}"
        );
        assert!(
            msg.contains("vibes"),
            "error should name the offending field, got: {msg}"
        );
    }

    #[test]
    fn parse_approval_unknown_policy_rejected() {
        let toml = r#"
[approval]
policy = "ask_sometimes"
"#;
        let err = parse_openslate_toml(toml).expect_err("unknown policy must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("ask_sometimes"),
            "error should name the bad variant, got: {msg}"
        );
    }

    // ── [approval] 新 schema 解析（tool-approval P1a）────────────────────

    #[test]
    fn parse_approval_new_schema_keys() {
        let toml = r#"
[approval]
default_level = "auto"

[approval.tool_levels]
bash = "manual"
read_file = "allow"

[approval.server_levels]
github = "allow"
termux = "manual"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let approval = config.approval.as_ref().expect("section present");
        assert_eq!(
            approval.default_level,
            Some(ToolApprovalLevelSetting::Auto)
        );
        assert_eq!(
            approval.tool_levels,
            BTreeMap::from([
                ("bash".to_owned(), ToolApprovalLevelSetting::Manual),
                ("read_file".to_owned(), ToolApprovalLevelSetting::Allow),
            ])
        );
        assert_eq!(
            approval.server_levels,
            BTreeMap::from([
                ("github".to_owned(), ToolApprovalLevelSetting::Allow),
                ("termux".to_owned(), ToolApprovalLevelSetting::Manual),
            ])
        );
        // legacy 键缺省仍可读（双 schema 共存）。
        assert_eq!(approval.policy, ApprovalPolicySetting::Auto);
        assert!(approval.tools.is_empty());
    }

    #[test]
    fn parse_approval_legacy_and_new_keys_coexist() {
        // 旧文件（只写 legacy 键）零破坏 + 新旧键同段混写可解析。
        let legacy = r#"
[approval]
policy = "auto_except"
tools = ["bash"]
"#;
        let config = parse_openslate_toml(legacy).expect("legacy file should parse");
        let approval = config.approval.as_ref().expect("section present");
        assert_eq!(approval.policy, ApprovalPolicySetting::AutoExcept);
        assert_eq!(approval.tools, vec!["bash".to_owned()]);
        assert_eq!(approval.default_level, None);
        assert!(approval.tool_levels.is_empty());
        assert!(approval.server_levels.is_empty());

        let mixed = r#"
[approval]
policy = "manual"
default_level = "auto"

[approval.tool_levels]
bash = "allow"
"#;
        let config = parse_openslate_toml(mixed).expect("mixed schema should parse");
        let approval = config.approval.as_ref().expect("section present");
        assert_eq!(approval.policy, ApprovalPolicySetting::Manual);
        assert_eq!(
            approval.default_level,
            Some(ToolApprovalLevelSetting::Auto)
        );
        assert_eq!(
            approval.tool_levels,
            BTreeMap::from([("bash".to_owned(), ToolApprovalLevelSetting::Allow)])
        );
    }

    #[test]
    fn parse_approval_unknown_level_rejected() {
        let toml = r#"
[approval]
default_level = "yolo"
"#;
        let err = parse_openslate_toml(toml).expect_err("unknown level must be rejected");
        assert!(
            err.to_string().contains("yolo"),
            "error should name the bad variant, got: {err}"
        );
    }

    #[test]
    fn default_ptc_config() {
        let ptc = PtcConfig::default();
        assert!(!ptc.enabled);
        assert_eq!(ptc.timeout_ms, 60_000);
        assert_eq!(ptc.memory_limit_bytes, 67_108_864); // 64 MB
        assert_eq!(ptc.max_output_bytes, 65_536);
        assert_eq!(ptc.max_tool_calls_per_run, 16);
        assert_eq!(ptc.max_list_chars, 8000);
        assert_eq!(ptc.disclosure, openslate_ptc::Disclosure::Auto);
        assert_eq!(ptc.max_lookup_calls, 50);
        assert!(ptc.tool_modes.is_empty());
    }

    #[test]
    fn parse_ptc_defaults_when_section_absent() {
        let config = parse_openslate_toml("").expect("empty toml should parse");
        assert!(!config.ptc.enabled);
        assert_eq!(config.ptc.timeout_ms, 60_000);
        assert_eq!(config.ptc.memory_limit_bytes, 67_108_864);
        assert_eq!(config.ptc.max_output_bytes, 65_536);
        assert_eq!(config.ptc.max_tool_calls_per_run, 16);
        assert_eq!(config.ptc.max_list_chars, 8000);
        assert_eq!(
            config.ptc.disclosure,
            openslate_ptc::Disclosure::Auto,
            "disclosure defaults to auto"
        );
        assert_eq!(config.ptc.max_lookup_calls, 50);
        assert!(config.ptc.tool_modes.is_empty());
    }

    #[test]
    fn parse_ptc_full_section_with_tool_modes() {
        let toml = r#"
[ptc]
enabled = true
timeout_ms = 5000
memory_limit_bytes = 33554432
max_output_bytes = 4096
max_tool_calls_per_run = 8
max_list_chars = 2000
disclosure = "catalog"
max_lookup_calls = 10

[ptc.tool_modes]
"*" = "both"
"shell" = "direct"
"github_*" = "ptc"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let ptc = &config.ptc;
        assert!(ptc.enabled);
        assert_eq!(ptc.timeout_ms, 5000);
        assert_eq!(ptc.memory_limit_bytes, 33_554_432);
        assert_eq!(ptc.max_output_bytes, 4096);
        assert_eq!(ptc.max_tool_calls_per_run, 8);
        assert_eq!(ptc.max_list_chars, 2000);
        assert_eq!(ptc.disclosure, openslate_ptc::Disclosure::Catalog);
        assert_eq!(ptc.max_lookup_calls, 10);
        assert_eq!(ptc.tool_modes.len(), 3);
        assert_eq!(
            ptc.tool_modes.get("*"),
            Some(&openslate_ptc::ToolCallMode::Both)
        );
        assert_eq!(
            ptc.tool_modes.get("shell"),
            Some(&openslate_ptc::ToolCallMode::DirectOnly)
        );
        assert_eq!(
            ptc.tool_modes.get("github_*"),
            Some(&openslate_ptc::ToolCallMode::PtcOnly)
        );
    }

    #[test]
    fn parse_ptc_partial_override_keeps_defaults() {
        let toml = r#"
[ptc]
enabled = true
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert!(config.ptc.enabled);
        assert_eq!(
            config.ptc.timeout_ms, 60_000,
            "unspecified fields keep their defaults"
        );
        assert!(config.ptc.tool_modes.is_empty());
    }

    #[test]
    fn parse_ptc_unknown_field_rejected() {
        let toml = r#"
[ptc]
bogus = 1
"#;
        let err = parse_openslate_toml(toml).expect_err("unknown field must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field"),
            "expected 'unknown field' in error, got: {msg}"
        );
        assert!(
            msg.contains("bogus"),
            "error should name the offending field, got: {msg}"
        );
    }

    #[test]
    fn parse_ptc_invalid_mode_rejected() {
        let toml = r#"
[ptc]
enabled = true

[ptc.tool_modes]
"read_file" = "wrong"
"#;
        assert!(parse_openslate_toml(toml).is_err());
    }

    #[test]
    fn parse_ptc_disclosure_tiers() {
        for (value, expected) in [
            ("full", openslate_ptc::Disclosure::Full),
            ("catalog", openslate_ptc::Disclosure::Catalog),
            ("auto", openslate_ptc::Disclosure::Auto),
        ] {
            let toml = format!("[ptc]\ndisclosure = \"{value}\"\n");
            let config = parse_openslate_toml(&toml).expect("valid tier should parse");
            assert_eq!(config.ptc.disclosure, expected);
        }
    }

    #[test]
    fn parse_ptc_invalid_disclosure_rejected() {
        let toml = r#"
[ptc]
disclosure = "everything"
"#;
        let err = parse_openslate_toml(toml).expect_err("unknown disclosure tier must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("disclosure"),
            "error should name the offending field, got: {msg}"
        );
    }

    #[test]
    fn parse_ptc_zero_timeout_ms_rejected() {
        // Zero/near-zero budgets brick the sandbox — fail at load time.
        let toml = r#"
[ptc]
enabled = true
timeout_ms = 0
"#;
        let err = parse_openslate_toml(toml).expect_err("timeout_ms = 0 must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("ptc.timeout_ms"), "msg: {msg}");
        assert!(msg.contains(">= 100"), "floor must be stated, msg: {msg}");
    }

    #[test]
    fn parse_ptc_zero_max_output_bytes_rejected() {
        let toml = r#"
[ptc]
enabled = true
max_output_bytes = 0
"#;
        let err = parse_openslate_toml(toml).expect_err("max_output_bytes = 0 must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("ptc.max_output_bytes"), "msg: {msg}");
        assert!(msg.contains(">= 1024"), "floor must be stated, msg: {msg}");
    }

    #[test]
    fn parse_ptc_floor_boundary_values_accepted() {
        // Happy path: every numeric exactly at its floor passes.
        let toml = r#"
[ptc]
enabled = true
timeout_ms = 100
memory_limit_bytes = 1048576
max_output_bytes = 1024
max_tool_calls_per_run = 1
max_lookup_calls = 1
max_list_chars = 0
"#;
        let config = parse_openslate_toml(toml).expect("floor values must parse");
        assert_eq!(config.ptc.timeout_ms, 100);
        assert_eq!(config.ptc.memory_limit_bytes, 1_048_576);
        assert_eq!(config.ptc.max_output_bytes, 1024);
        assert_eq!(config.ptc.max_tool_calls_per_run, 1);
        assert_eq!(config.ptc.max_lookup_calls, 1);
        // max_list_chars = 0 keeps its documented "unlimited" meaning.
        assert_eq!(config.ptc.max_list_chars, 0);
    }

    // ── Full example TOML ────────────────────────────────────────────────

    #[test]
    fn parse_full_example_toml() {
        let toml_content = include_str!("../../fixtures/openslate.toml");
        let config = parse_openslate_toml(toml_content).expect("example toml should parse");

        let project = config.project.as_ref().expect("project");
        assert_eq!(project.name.as_deref(), Some("example-openslate-project"));

        let db = config.database.as_ref().expect("database");
        assert_eq!(db.path, None);
        assert!(db.wal);
        assert_eq!(db.busy_timeout_ms, 5000);

        let prompts = config.prompts.as_ref().expect("prompts");
        assert_eq!(prompts.path.as_deref(), Some("./prompts"));
        assert_eq!(prompts.default_profile, "default");
        assert!(prompts.hot_reload);

        let limits = config.limits.as_ref().expect("limits");
        assert_eq!(limits.max_steps, 12);
        assert_eq!(limits.max_depth, 4);
        assert!(limits.auto_compact);
        assert!(limits.parallel_tool_calls);

        assert_eq!(config.providers.len(), 2);
        let zhipu = config.providers.get("zhipu").expect("zhipu provider");
        assert_eq!(zhipu.base_url, "https://open.bigmodel.cn/api/paas/v4");
        let minimax = config.providers.get("minimax").expect("minimax provider");
        assert_eq!(minimax.api_key_env, "MINIMAX_API_KEY");

        assert_eq!(config.models.len(), 4);
        let main = config.models.get("main").expect("main model");
        assert_eq!(main.provider, "zhipu");
        assert_eq!(main.model, "glm-5.1");
        assert_eq!(main.max_context_tokens, Some(200_000));
        assert!(main.supports_tool_call);
        assert!(main.supports_reasoning);

        let fast = config.models.get("fast").expect("fast model");
        assert_eq!(fast.provider, "minimax");
        assert!(!fast.supports_reasoning);

        let vision = config.models.get("vision").expect("vision model");
        assert!(vision.supports_vision);

        let trace = config.trace.as_ref().expect("trace");
        assert!(trace.enabled);
        assert!(trace.store_sqlite);
        assert_eq!(trace.default_export_format, "chrome-json");

        // The [ptc] example block is fully commented out, so PTC must stay
        // at its disabled-by-default state.
        assert!(!config.ptc.enabled);
        assert!(config.ptc.tool_modes.is_empty());
    }

    // ── Markdown agent parsing tests ─────────────────────────────────────

    #[test]
    fn md_parse_invalid_frontmatter() {
        let md = "---\nname: [broken\nmodel: main\n---\nbody\n";
        let result = parse_agent_markdown(md, "broken.md");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::ParseError(_)),);
    }

    #[test]
    fn md_parse_single_agent() {
        let md = "---\nid: root\nname: Root Agent\nmodel: main\n---\nYou are the root agent.\n";
        let agent = parse_agent_markdown(md, "root.md").expect("should parse");
        assert_eq!(agent.id.0, "root");
        assert_eq!(agent.name, "Root Agent");
        assert_eq!(agent.model, "main");
        assert!(agent.children.is_empty());
        assert!(agent.tools.is_empty());
        assert_eq!(agent.default_prompt, "You are the root agent.");
    }

    #[test]
    fn md_parse_children_and_tools() {
        let md = "---\nid: root\nname: Root Agent\nmodel: main\nchildren:\n  - researcher\n  - writer\ntools:\n  - current_time\n  - read_file\n---\nYou are the root coordinator agent.\n";
        let agent = parse_agent_markdown(md, "root.md").expect("should parse");
        assert_eq!(agent.children.len(), 2);
        assert_eq!(agent.children[0].0, "researcher");
        assert_eq!(agent.children[1].0, "writer");
        assert_eq!(agent.tools.len(), 2);
        assert_eq!(agent.tools[0], "current_time");
        assert_eq!(agent.tools[1], "read_file");
    }

    #[test]
    fn parse_full_example_agents_dir() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/agents");
        let config = parse_agents_dir(Path::new(dir)).expect("example agents dir should parse");

        assert_eq!(config.agents.len(), 6);

        let root = config
            .agents
            .iter()
            .find(|a| a.id.0 == "root")
            .expect("root agent");
        assert_eq!(root.name, "Root Agent");
        assert_eq!(root.model, "main");
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].0, "researcher");
        assert_eq!(root.children[1].0, "writer");
        // Assert the names, not just the count: these are the matching keys
        // for the [builtin_tools] warning rule and the runtime per-tool
        // filter, so a silent rename must not pass.
        assert_eq!(
            root.tools,
            vec!["read_file", "write_file", "edit_file", "shell"]
        );

        let researcher = config
            .agents
            .iter()
            .find(|a| a.id.0 == "researcher")
            .expect("researcher agent");
        assert_eq!(researcher.model, "fast");
        assert_eq!(researcher.children.len(), 3);
        assert_eq!(researcher.children[0].0, "verifier");
        assert_eq!(researcher.children[1].0, "deep-analyst");
        assert_eq!(researcher.children[2].0, "visual-inspector");

        let verifier = config
            .agents
            .iter()
            .find(|a| a.id.0 == "verifier")
            .expect("verifier agent");
        assert!(verifier.children.is_empty());

        let writer = config
            .agents
            .iter()
            .find(|a| a.id.0 == "writer")
            .expect("writer agent");
        assert!(writer.tools.is_empty());

        let analyst = config
            .agents
            .iter()
            .find(|a| a.id.0 == "deep-analyst")
            .expect("deep-analyst agent");
        assert_eq!(analyst.model, "deep-reasoner");

        let inspector = config
            .agents
            .iter()
            .find(|a| a.id.0 == "visual-inspector")
            .expect("visual-inspector agent");
        assert_eq!(inspector.model, "vision");
    }

    // ── Multiple providers and models ────────────────────────────────────

    #[test]
    fn parse_multiple_providers_and_models() {
        let toml = r#"
[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[providers.anthropic]
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"

[models.gpt4]
provider = "openai"
model = "gpt-4-turbo"
max_context_tokens = 128000

[models.claude]
provider = "anthropic"
model = "claude-3-opus"
supports_tool_call = true
supports_vision = true
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert_eq!(config.providers.len(), 2);
        assert_eq!(config.models.len(), 2);

        let gpt4 = config.models.get("gpt4").expect("gpt4");
        assert_eq!(gpt4.max_context_tokens, Some(128_000));
        assert_eq!(gpt4.max_output_tokens, None);

        let claude = config.models.get("claude").expect("claude");
        assert!(claude.supports_tool_call);
        assert!(claude.supports_vision);
    }

    // ── Markdown frontmatter parsing tests ───────────────────────────────

    #[test]
    fn md_parse_normal_frontmatter() {
        let md = "---\nname: Root Agent\nmodel: main\ntools:\n  - bash\n  - read_file\n---\nYou are the root agent.\n";
        let agent = parse_agent_markdown(md, "root.md").expect("should parse");
        assert_eq!(agent.id.0, "root");
        assert_eq!(agent.name, "Root Agent");
        assert_eq!(agent.model, "main");
        assert_eq!(agent.tools, vec!["bash", "read_file"]);
        assert!(agent.children.is_empty());
        assert!(agent.hidden_tools.is_empty(), "无 hidden_tools = 空名单（行为不变）");
        assert_eq!(agent.default_prompt, "You are the root agent.");
    }

    #[test]
    fn md_parse_hidden_tools() {
        let md = "---\nname: Root\nmodel: main\ntools: [bash, read_file]\nhidden_tools:\n  - mcp_*\n  - termux_bash\n---\nBody.\n";
        let agent = parse_agent_markdown(md, "root.md").expect("should parse");
        assert_eq!(agent.tools, vec!["bash", "read_file"]);
        assert_eq!(agent.hidden_tools, vec!["mcp_*", "termux_bash"]);
    }

    #[test]
    fn md_parse_no_id_falls_back_to_filename() {
        let md = "---\nname: Writer\nmodel: fast\n---\nWrite stuff.\n";
        let agent = parse_agent_markdown(md, "my-writer.md").expect("should parse");
        assert_eq!(agent.id.0, "my_writer");
        assert_eq!(agent.name, "Writer");
        assert_eq!(agent.model, "fast");
    }

    #[test]
    fn md_parse_empty_body() {
        let md = "---\nname: Empty\nmodel: main\n---\n";
        let agent = parse_agent_markdown(md, "empty.md").expect("should parse");
        assert_eq!(agent.id.0, "empty");
        assert_eq!(agent.default_prompt, "");
    }

    #[test]
    fn md_parse_no_frontmatter_delimiter_error() {
        let md = "Just some plain text without frontmatter.\n";
        let result = parse_agent_markdown(md, "plain.md");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ConfigError::ParseError(_)));
        assert!(err.to_string().contains("no frontmatter delimiter"));
    }

    #[test]
    fn md_parse_bad_yaml_error() {
        let md = "---\nname: [broken\nmodel: main\n---\nbody\n";
        let result = parse_agent_markdown(md, "bad.md");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ConfigError::ParseError(_)));
        assert!(err.to_string().contains("bad.md"));
    }

    #[test]
    fn md_parse_thematic_break_in_body() {
        let md = "---\nname: Agent\nmodel: main\n---\nSome prompt text.\n\n---\n\nMore text after thematic break.\n";
        let agent = parse_agent_markdown(md, "thematic.md").expect("should parse");
        assert_eq!(agent.id.0, "thematic");
        assert!(agent.default_prompt.contains("---"));
        assert!(agent
            .default_prompt
            .contains("More text after thematic break."));
    }

    #[test]
    fn md_parse_crlf_line_endings() {
        let md = "---\r\nname: CRLF Agent\r\nmodel: main\r\n---\r\nHello from Windows.\r\n";
        let agent = parse_agent_markdown(md, "crlf.md").expect("should parse");
        assert_eq!(agent.id.0, "crlf");
        assert_eq!(agent.name, "CRLF Agent");
    }

    #[test]
    fn md_parse_bom_prefix() {
        let bom = "\u{feff}";
        let md = format!("{bom}---\nname: BOM Agent\nmodel: main\n---\nBOM content.\n");
        let agent = parse_agent_markdown(&md, "bom.md").expect("should parse");
        assert_eq!(agent.id.0, "bom");
        assert_eq!(agent.name, "BOM Agent");
    }

    #[test]
    fn md_parse_explicit_id_overrides_filename() {
        let md = "---\nid: custom-id\nname: Custom\nmodel: fast\n---\nCustom prompt.\n";
        let agent = parse_agent_markdown(md, "some-file.md").expect("should parse");
        assert_eq!(agent.id.0, "custom-id");
    }

    #[test]
    fn md_parse_children_converted_to_agent_ids() {
        let md =
            "---\nname: Parent\nmodel: main\nchildren:\n  - researcher\n  - writer\n---\nPrompt.\n";
        let agent = parse_agent_markdown(md, "parent.md").expect("should parse");
        assert_eq!(agent.children.len(), 2);
        assert_eq!(agent.children[0].0, "researcher");
        assert_eq!(agent.children[1].0, "writer");
    }

    #[test]
    fn derive_id_basic() {
        assert_eq!(derive_id_from_filename("root.md"), "root");
    }

    #[test]
    fn derive_id_special_chars() {
        assert_eq!(derive_id_from_filename("my-cool agent.md"), "my_cool_agent");
    }

    #[test]
    fn derive_id_no_md_extension() {
        assert_eq!(derive_id_from_filename("agent"), "agent");
    }

    #[test]
    fn derive_id_uppercase() {
        assert_eq!(derive_id_from_filename("MyAgent.md"), "myagent");
    }

    #[test]
    fn parse_agents_dir_nonexistent() {
        let result = parse_agents_dir(Path::new("/nonexistent/path/xyz"));
        assert!(result.is_err());
    }
}
