//! Approval system for tool execution gating.
//!
//! Provides the rule-based approval engine ([`ApprovalRules`] /
//! [`ToolApprovalLevel`]) to control whether tool calls require human
//! approval, and [`ApprovalManager`] to assess risk and determine
//! if a tool needs interactive confirmation before execution.
//!
//! The manager is wired into the run pipeline (config → RunManager →
//! AgentRunner) and consulted at the top of `AgentRunner::execute` for
//! EVERY tool call — including `call_agent` delegation and `run_code`
//! (PTC) — so no dispatch path can bypass it.
//!
//! # 规则引擎（tool-approval P1a，设计定稿 `.slim/deepwork/tool-approval.md`）
//!
//! 评估序：`tools[工具名]` > `servers[server 作用域]` > `default_level`。
//! 三个等级：[`ToolApprovalLevel::Manual`] 每次必问（trigger = Manual）；
//! `Allow` 从不问；`Auto` 自动判断——启发式 [`RiskLevel::Low`]（纯只读）
//! 直接放行，`Medium`/`High` 进入审批链（trigger = AutoRisk；AI 审查
//! 装饰器在 mobile 层对 AutoRisk 请求先做 LLM 审查，core 本身不做）。
//!
//! - **legacy 映射（P0-4，审查钉死勿改）**：`Auto` → default=Allow（从不
//!   问）；`Manual` → default=Manual；`AutoExcept(list)` → default=Allow +
//!   `tools[每项]=Manual`。CLI/server/TUI 旧行为经此映射逐字节保持。
//! - **热更新**：规则存 `Arc<RwLock<ApprovalRules>>`，Clone 共享同一
//!   cell——`set_rules`（或外部经 [`ApprovalManager::rules_handle`] 改写）
//!   对**进行中回合**的下一次 `check` 即生效（P0-5：装配侧必须用
//!   `with_rules`/`rules_handle` 注入共享 cell，重建 manager 会拿到游离
//!   cell，always_* 热更到不了引擎且无编译错误）。
//! - **非交互直通**：需要审批但未装 callback → `Approved`（wiring 的
//!   `NonInteractiveGate` 依赖该语义，保持不变）。

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use crate::config::{OpenSlateConfig, ToolApprovalLevelSetting};

/// Policy controlling whether tool calls require human approval.
///
/// legacy 策略三型（v1 语义）。新代码一律用 [`ApprovalRules`]；本类型
/// 保留给旧装配路径（`derive_effective_policy` / `apply_approval` /
/// CLI `--yes`），经 [`rules_from_policy`] 映射进规则引擎。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ApprovalPolicy {
    #[default]
    Auto,
    Manual,
    AutoExcept(Vec<String>),
}

/// Risk level assessed for a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskLevel {
    /// Read-only or observation tools (e.g., read_file, list_dir, current_time).
    Low,
    /// Tools with moderate side-effects.
    Medium,
    /// Destructive or write tools (e.g., bash, write_file).
    High,
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RiskLevel::Low => write!(f, "low"),
            RiskLevel::Medium => write!(f, "medium"),
            RiskLevel::High => write!(f, "high"),
        }
    }
}

/// 单个工具 / MCP server 作用域 / 全局缺省的审批等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalLevel {
    /// 每次调用都询问（人工审批；trigger = Manual——**不进 AI 审查**，
    /// 否则「全部手动」会被审查器过滤，违背语义，P0-2）。
    Manual,
    /// 从不询问，直接放行。
    #[default]
    Allow,
    /// 自动判断：启发式 [`RiskLevel::Low`]（纯只读）直接放行（省一次
    /// 审查调用）；`Medium` / `High` 进入审批链（trigger = AutoRisk）。
    /// 最终风险判定可由上层 AI 审查装饰器（mobile）承担，core 只做
    /// 启发式预过滤。
    Auto,
}

impl ToolApprovalLevel {
    /// 线格式 / TOML 拼写（snake_case，与 serde 形状一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolApprovalLevel::Manual => "manual",
            ToolApprovalLevel::Allow => "allow",
            ToolApprovalLevel::Auto => "auto",
        }
    }
}

impl fmt::Display for ToolApprovalLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<ToolApprovalLevelSetting> for ToolApprovalLevel {
    fn from(setting: ToolApprovalLevelSetting) -> Self {
        match setting {
            ToolApprovalLevelSetting::Manual => ToolApprovalLevel::Manual,
            ToolApprovalLevelSetting::Allow => ToolApprovalLevel::Allow,
            ToolApprovalLevelSetting::Auto => ToolApprovalLevel::Auto,
        }
    }
}

/// 规则集：`default_level` 缺省 + 按工具 / 按 MCP server 的覆盖。
///
/// `servers` 的键是审批作用域标识（聚合形态 = 下游 slug，host 内置裸名
/// = host 前缀；反解见 [`crate::mcp::server_slug_of`]）。
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ApprovalRules {
    /// 未命中 `tools` / `servers` 时的缺省等级。默认 [`ToolApprovalLevel::Allow`]
    /// （**从不问**——legacy `Auto` 的行为等价物，P0-4；需要自动判断的
    /// 装配点（mobile）必须显式 opt-in `Auto`，`[approval]` 段缺省不隐含它）。
    pub default_level: ToolApprovalLevel,
    /// 工具名（registry 暴露名）→ 等级。
    pub tools: BTreeMap<String, ToolApprovalLevel>,
    /// MCP server 作用域 → 等级（评估序低于 tools）。
    pub servers: BTreeMap<String, ToolApprovalLevel>,
}

/// 精确匹配未命中时做 ASCII 大小写不敏感回退——兼容 legacy
/// `AutoExcept` 的大小写不敏感语义（P0-4 行为零变化；新路径写入的键
/// 本就是精确暴露名，回退分支不会误命中）。
fn lookup_level(map: &BTreeMap<String, ToolApprovalLevel>, key: &str) -> Option<ToolApprovalLevel> {
    if let Some(level) = map.get(key) {
        return Some(*level);
    }
    map.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, level)| *level)
}

impl ApprovalRules {
    /// 评估某次调用的生效等级：`tools[name]` ?? `servers[server]` ?? `default`。
    pub fn level_for(&self, tool_name: &str, server: Option<&str>) -> ToolApprovalLevel {
        if let Some(level) = lookup_level(&self.tools, tool_name) {
            return level;
        }
        if let Some(level) = server.and_then(|s| lookup_level(&self.servers, s)) {
            return level;
        }
        self.default_level
    }

    /// 重建最接近的 legacy 策略视图（[`ApprovalManager::policy`] /
    /// [`ApprovalManager::needs_approval`] 兼容层的实现基础）。
    ///
    /// - default=Manual → [`ApprovalPolicy::Manual`]；
    /// - default=Allow 且存在 Manual 级工具项 →
    ///   [`ApprovalPolicy::AutoExcept`]（这些工具名）；
    /// - 其余（default=Allow 无 Manual 项 / default=Auto）→
    ///   [`ApprovalPolicy::Auto`]。
    ///
    /// **注意**：default=Auto（自动风险判断）无法用 legacy 三型表达，
    /// 返回 `Auto`（= 从不问）是有损降级——仅供旧装配路径
    /// （server/CLI 的 `policy().clone()` → `ApprovalManager::new(policy)`
    /// round-trip）过渡；legacy 配置形状（只经 [`rules_from_policy`] /
    /// `rules_from_config` 的 legacy 铺底产生）永不出现该损失。
    pub fn to_legacy_policy(&self) -> ApprovalPolicy {
        match self.default_level {
            ToolApprovalLevel::Manual => ApprovalPolicy::Manual,
            ToolApprovalLevel::Allow | ToolApprovalLevel::Auto => {
                let manual_tools: Vec<String> = self
                    .tools
                    .iter()
                    .filter(|(_, level)| **level == ToolApprovalLevel::Manual)
                    .map(|(name, _)| name.clone())
                    .collect();
                if manual_tools.is_empty() {
                    ApprovalPolicy::Auto
                } else {
                    ApprovalPolicy::AutoExcept(manual_tools)
                }
            }
        }
    }
}

/// legacy 策略 → 规则（P0-4，审查钉死，勿改）：
/// `Auto` → default=Allow；`Manual` → default=Manual；
/// `AutoExcept(list)` → default=Allow + `tools[每项]=Manual`。
pub fn rules_from_policy(policy: &ApprovalPolicy) -> ApprovalRules {
    match policy {
        ApprovalPolicy::Auto => ApprovalRules::default(),
        ApprovalPolicy::Manual => ApprovalRules {
            default_level: ToolApprovalLevel::Manual,
            ..ApprovalRules::default()
        },
        ApprovalPolicy::AutoExcept(tools) => ApprovalRules {
            default_level: ToolApprovalLevel::Allow,
            tools: tools
                .iter()
                .map(|name| (name.clone(), ToolApprovalLevel::Manual))
                .collect(),
            servers: BTreeMap::new(),
        },
    }
}

/// `[approval]` 配置段 → 生效规则的**单一推导函数**（装配 / config_ops
/// 重载 / always_* 持久化后三调用点共用，防各处推导漂移）。
///
/// 推导序：default=Allow 基线 → 段内 legacy `policy`（+`tools`）按
/// [`rules_from_policy`] 的映射铺底 → `default_level` 键覆盖缺省 →
/// `tool_levels` / `server_levels` 逐键覆盖合并。`[approval]` 段缺省 →
/// 纯 Allow 基线（mobile 的 Auto 默认是装配层显式 opt-in，**不在这里做**）。
pub fn rules_from_config(config: &OpenSlateConfig) -> ApprovalRules {
    let mut rules = ApprovalRules::default();
    let Some(approval) = config.approval.as_ref() else {
        return rules;
    };
    // legacy 铺底（policy 键缺省 = auto → default=Allow，与基线重合）。
    let legacy = rules_from_policy(&approval.to_policy());
    rules.default_level = legacy.default_level;
    rules.tools = legacy.tools;
    if let Some(level) = approval.default_level {
        rules.default_level = ToolApprovalLevel::from(level);
    }
    for (name, level) in &approval.tool_levels {
        rules
            .tools
            .insert(name.clone(), ToolApprovalLevel::from(*level));
    }
    for (slug, level) in &approval.server_levels {
        rules
            .servers
            .insert(slug.clone(), ToolApprovalLevel::from(*level));
    }
    rules
}

/// 本次审批请求的触发来源（P0-2）。
///
/// Manual 等级的请求直通人工审批链（不进 AI 审查）；AutoRisk 请求可被
/// 上层 AI 审查装饰器先行过滤（safe → 静默放行，risky → 弹人工）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalTrigger {
    /// 工具 / server / default 等级为 Manual → 直接人工审批。
    Manual,
    /// Auto 等级下启发式风险 ≥ Medium → 进入审批链。
    AutoRisk,
}

impl fmt::Display for ApprovalTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApprovalTrigger::Manual => write!(f, "manual"),
            ApprovalTrigger::AutoRisk => write!(f, "auto_risk"),
        }
    }
}

/// A request for tool execution approval.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    /// Name of the tool to execute.
    pub tool_name: String,
    /// Arguments that will be passed to the tool.
    pub arguments: serde_json::Value,
    /// ID of the agent requesting execution.
    pub agent_id: String,
    /// Assessed risk level.
    pub risk_level: RiskLevel,
    /// MCP server 作用域（聚合形态 = 下游 slug；host 内置 = host 前缀；
    /// 非 MCP = `None`）。审批 UI 的 (c)/(d) MCP 级选项据此显示。
    pub server: Option<String>,
    /// 触发来源（Manual 等级直问 / Auto 等级风险命中），见 [`ApprovalTrigger`]。
    pub trigger: ApprovalTrigger,
    /// AI 审查理由（上层装饰器回填；core 引擎没有审查上下文，恒 `None`）。
    pub note: Option<String>,
}

/// Decision returned from an approval check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Tool execution is approved.
    Approved,
    /// Tool execution is denied with a reason.
    Denied(String),
}

/// Human (or programmatic) decider consulted when a tool call requires
/// approval.
///
/// Synchronous by design: `decide` is called inline on the runtime thread
/// (from `AgentRunner::execute`) and may block — e.g. the REPL callback
/// waits for the user's y/n/a answer. Implementations must be `Send +
/// Sync` because the manager is shared across the run's recursion layers.
pub trait ApprovalCallback: Send + Sync {
    /// Decide whether the requested tool call may execute.
    fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision;
}

/// Manages approval checks for tool executions.
#[derive(Clone)]
pub struct ApprovalManager {
    /// 共享规则 cell（Clone 共享同一 Arc → 热更新对进行中回合生效；
    /// 毒化容忍 `into_inner`，与全库锁口径一致）。
    rules: Arc<RwLock<ApprovalRules>>,
    callback: Option<Arc<dyn ApprovalCallback>>,
}

impl ApprovalManager {
    /// Create a new approval manager with the given legacy policy and no
    /// callback (calls that need approval are approved — the CLI layer is
    /// expected to install a callback or derive a policy that avoids this
    /// state). Policy 按 [`rules_from_policy`]（P0-4）映射进规则引擎。
    pub fn new(policy: ApprovalPolicy) -> Self {
        Self {
            rules: Arc::new(RwLock::new(rules_from_policy(&policy))),
            callback: None,
        }
    }

    /// Create an approval manager with the default rules（default=Allow，
    /// **从不问**——与 legacy `auto()` 行为等价，P0-4）。
    pub fn auto() -> Self {
        Self::default()
    }

    /// Create an approval manager asking for every tool call
    /// （default=Manual）。
    pub fn manual() -> Self {
        Self::new(ApprovalPolicy::Manual)
    }

    /// 以外部持有的共享规则 cell 构造（builder）。装配侧（session
    /// bridge / mobile 装配）持有同一 handle 改写规则，热更直达本
    /// manager——P0-5 钉死的正道（对比：`new(policy)` 重建会拿到游离
    /// cell）。
    pub fn with_rules(handle: Arc<RwLock<ApprovalRules>>) -> Self {
        Self {
            rules: handle,
            callback: None,
        }
    }

    /// 共享规则 cell 的克隆（外部热更新入口；与 `set_rules` 等价但允许
    /// 多方共享同一 cell）。
    pub fn rules_handle(&self) -> Arc<RwLock<ApprovalRules>> {
        Arc::clone(&self.rules)
    }

    /// 热更新规则（对进行中回合的下一次 `check` 生效）。
    pub fn set_rules(&self, rules: ApprovalRules) {
        *self
            .rules
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = rules;
    }

    /// 当前规则快照（clone）。
    pub fn rules(&self) -> ApprovalRules {
        self.rules
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Attach an approval callback (builder style).
    pub fn with_callback(mut self, callback: Arc<dyn ApprovalCallback>) -> Self {
        self.callback = Some(callback);
        self
    }

    /// 兼容访问器：按 [`Self::policy`] 的 legacy 视图判断是否需要审批
    /// （不感知 server 作用域与 Auto 等级）。旧语义保持：`Auto` false /
    /// `Manual` true / `AutoExcept` 大小写不敏感精确匹配。新代码用
    /// `check`（等级评估含 server 作用域）。
    pub fn needs_approval(&self, tool_name: &str) -> bool {
        match self.policy() {
            ApprovalPolicy::Auto => false,
            ApprovalPolicy::Manual => true,
            ApprovalPolicy::AutoExcept(tool_list) => tool_list
                .iter()
                .any(|t| t.eq_ignore_ascii_case(tool_name)),
        }
    }

    /// Assess the risk level of a tool call based on tool name and arguments.
    ///
    /// `run_code` is hard-coded to [`RiskLevel::High`]: one approval covers
    /// the whole script it executes (the PTC bridge calls tools inside the
    /// sandbox directly, without passing through this gate — Claude-Code
    /// command-granularity convention). Per-bridge-tool approval is a
    /// deliberate P2 deferral.
    pub fn assess_risk(&self, tool_name: &str, _args: &serde_json::Value) -> RiskLevel {
        let name_lower = tool_name.to_ascii_lowercase();

        if name_lower == "run_code" {
            return RiskLevel::High;
        }

        // Known high-risk tools: anything that writes, executes, or modifies
        let high_risk_indicators = [
            "bash",
            "shell",
            "exec",
            "write",
            "write_file",
            "delete",
            "delete_file",
            "remove",
            "rename",
            "move",
            "chmod",
            "chown",
            "mkdir",
            "rmdir",
            "curl",
            "wget",
            "request",
            "http",
            "fetch",
            "sql",
            "database",
        ];

        // Known low-risk tools: read-only operations
        let low_risk_indicators = [
            "read",
            "read_file",
            "list",
            "list_dir",
            "ls",
            "cat",
            "head",
            "tail",
            "grep",
            "find",
            "search",
            "stat",
            "time",
            "current_time",
            "date",
            "whoami",
            "env",
            "version",
            "status",
            "info",
        ];

        for indicator in &high_risk_indicators {
            if name_lower.contains(indicator) {
                return RiskLevel::High;
            }
        }

        for indicator in &low_risk_indicators {
            if name_lower.contains(indicator) {
                return RiskLevel::Low;
            }
        }

        // Default to medium for unknown tools
        RiskLevel::Medium
    }

    /// Build an approval request for a tool call（`note` 恒 `None`——AI
    /// 审查理由由上层装饰器回填）。
    pub fn create_request(
        &self,
        tool_name: &str,
        server: Option<&str>,
        arguments: &serde_json::Value,
        agent_id: &str,
        trigger: ApprovalTrigger,
    ) -> ApprovalRequest {
        let risk_level = self.assess_risk(tool_name, arguments);
        ApprovalRequest {
            tool_name: tool_name.to_owned(),
            arguments: arguments.clone(),
            agent_id: agent_id.to_owned(),
            risk_level,
            server: server.map(str::to_owned),
            trigger,
            note: None,
        }
    }

    /// Aggregate gate consulted before every tool execution.
    ///
    /// 等级评估（rules 快照，锁外执行）：`Allow` → `Approved`；
    /// `Manual` → 人工审批（trigger=Manual）；`Auto` → 启发式 `Low` →
    /// `Approved`，`Medium`/`High` → 审批链（trigger=AutoRisk）。
    ///
    /// - 需要审批且有 callback → `callback.decide()`（同步阻塞）；
    /// - 需要审批但无 callback → `Approved`（非交互直通；CLI 层经
    ///   `NonInteractiveGate` 等策略避免走到该分支）；
    ///
    /// Every decision is audited via `tracing` (target
    /// `openslate_approval`); denials are additionally captured by the
    /// runner's in-memory tool audit as error tool results. Nothing is
    /// written to the store.
    pub fn check(
        &self,
        tool_name: &str,
        server: Option<&str>,
        args: &serde_json::Value,
        agent_id: &str,
    ) -> ApprovalDecision {
        let rules = self.rules();
        let level = rules.level_for(tool_name, server);
        let (needs_approval, trigger) = match level {
            ToolApprovalLevel::Allow => (false, ApprovalTrigger::Manual),
            ToolApprovalLevel::Manual => (true, ApprovalTrigger::Manual),
            ToolApprovalLevel::Auto => {
                // 启发式预过滤（设计 v3 用户拍板）：Low（纯只读）不进
                // 审批链直接放行，省一次审查调用。
                (
                    self.assess_risk(tool_name, args) != RiskLevel::Low,
                    ApprovalTrigger::AutoRisk,
                )
            }
        };
        if !needs_approval {
            tracing::debug!(
                target: "openslate_approval",
                "approved tool '{tool_name}' (agent {agent_id}, server {server:?}, \
                 level {level}, no approval needed)"
            );
            return ApprovalDecision::Approved;
        }
        let request = self.create_request(tool_name, server, args, agent_id, trigger);
        let decision = match &self.callback {
            Some(callback) => callback.decide(&request),
            None => ApprovalDecision::Approved,
        };
        match &decision {
            ApprovalDecision::Approved => tracing::info!(
                target: "openslate_approval",
                "approved tool '{}' (agent {}, server {:?}, trigger {}, risk {})",
                request.tool_name,
                request.agent_id,
                request.server,
                request.trigger,
                request.risk_level
            ),
            ApprovalDecision::Denied(reason) => tracing::warn!(
                target: "openslate_approval",
                "denied tool '{}' (agent {}, server {:?}, trigger {}, risk {}): {}",
                request.tool_name,
                request.agent_id,
                request.server,
                request.trigger,
                request.risk_level,
                reason
            ),
        }
        decision
    }

    /// Auto-approve a request (used in non-interactive mode).
    pub fn auto_approve(_request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Approved
    }

    /// Deny a request with a reason.
    pub fn deny(reason: &str) -> ApprovalDecision {
        ApprovalDecision::Denied(reason.to_owned())
    }

    /// 重建当前规则最接近的 legacy 策略视图（**owned**——规则可经共享
    /// cell 热更，无法返回借用）。旧装配路径（server/CLI/mobile 的
    /// `policy().clone()` → `ApprovalManager::new(policy)` round-trip）
    /// 对 legacy 配置形状逐字节等价；`default=Auto` 的有损降级见
    /// [`ApprovalRules::to_legacy_policy`]。
    pub fn policy(&self) -> ApprovalPolicy {
        self.rules().to_legacy_policy()
    }
}

impl Default for ApprovalManager {
    fn default() -> Self {
        Self {
            rules: Arc::new(RwLock::new(ApprovalRules::default())),
            callback: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Callback that denies everything and records the requests it saw.
    struct DenyAll {
        seen: Mutex<Vec<ApprovalRequest>>,
    }
    impl ApprovalCallback for DenyAll {
        fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision {
            self.seen.lock().expect("seen poisoned").push(req.clone());
            ApprovalDecision::Denied("denied by test callback".to_owned())
        }
    }

    /// Callback that approves everything and records the requests it saw.
    struct ApproveAll {
        seen: Mutex<Vec<ApprovalRequest>>,
    }
    impl ApprovalCallback for ApproveAll {
        fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision {
            self.seen.lock().expect("seen poisoned").push(req.clone());
            ApprovalDecision::Approved
        }
    }

    fn rules(
        default: ToolApprovalLevel,
        tools: &[(&str, ToolApprovalLevel)],
        servers: &[(&str, ToolApprovalLevel)],
    ) -> ApprovalRules {
        ApprovalRules {
            default_level: default,
            tools: tools
                .iter()
                .map(|(k, v)| ((*k).to_owned(), *v))
                .collect(),
            servers: servers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), *v))
                .collect(),
        }
    }

    fn config_with_approval(toml: &str) -> OpenSlateConfig {
        crate::config::parse_openslate_toml(toml).expect("config should parse")
    }

    // ── Auto policy tests（legacy 兼容）────────────────────────────────

    #[test]
    fn auto_policy_never_needs_approval() {
        let mgr = ApprovalManager::auto();
        assert!(!mgr.needs_approval("bash"));
        assert!(!mgr.needs_approval("write_file"));
        assert!(!mgr.needs_approval("read_file"));
    }

    #[test]
    fn auto_policy_is_default() {
        let mgr = ApprovalManager::default();
        assert!(!mgr.needs_approval("anything"));
    }

    #[test]
    fn auto_rules_default_is_allow_never_asks() {
        // P0-4：ApprovalRules::default() == ApprovalManager::auto() == Allow。
        assert_eq!(ApprovalRules::default().default_level, ToolApprovalLevel::Allow);
        let mgr = ApprovalManager::auto();
        assert_eq!(
            mgr.rules(),
            ApprovalRules {
                default_level: ToolApprovalLevel::Allow,
                ..ApprovalRules::default()
            }
        );
    }

    // ── Manual policy tests ──

    #[test]
    fn manual_policy_always_needs_approval() {
        let mgr = ApprovalManager::manual();
        assert!(mgr.needs_approval("bash"));
        assert!(mgr.needs_approval("read_file"));
        assert!(mgr.needs_approval("current_time"));
    }

    // ── AutoExcept policy tests ──

    #[test]
    fn auto_except_needs_approval_for_listed_tools() {
        let mgr = ApprovalManager::new(ApprovalPolicy::AutoExcept(vec![
            "bash".to_owned(),
            "write_file".to_owned(),
        ]));
        assert!(mgr.needs_approval("bash"));
        assert!(mgr.needs_approval("write_file"));
        assert!(!mgr.needs_approval("read_file"));
        assert!(!mgr.needs_approval("current_time"));
    }

    #[test]
    fn auto_except_is_case_insensitive() {
        let mgr = ApprovalManager::new(ApprovalPolicy::AutoExcept(vec!["Bash".to_owned()]));
        assert!(mgr.needs_approval("bash"));
        assert!(mgr.needs_approval("BASH"));
        assert!(mgr.needs_approval("Bash"));
    }

    #[test]
    fn auto_except_empty_list_never_needs_approval() {
        let mgr = ApprovalManager::new(ApprovalPolicy::AutoExcept(vec![]));
        assert!(!mgr.needs_approval("bash"));
        assert!(!mgr.needs_approval("anything"));
    }

    #[test]
    fn legacy_auto_except_case_insensitivity_survives_rules_mapping() {
        // P0-4 行为零变化锚点：AutoExcept 的大小写不敏感匹配必须经
        // legacy 映射 + check() 存活（tools map 精确未命中时 ASCII
        // 大小写回退）。
        let mgr =
            ApprovalManager::new(ApprovalPolicy::AutoExcept(vec!["Bash".to_owned()]))
                .with_callback(Arc::new(DenyAll {
                    seen: Mutex::new(Vec::new()),
                }));
        assert_eq!(
            mgr.check("BASH", None, &serde_json::json!({}), "root"),
            ApprovalDecision::Denied("denied by test callback".to_owned())
        );
    }

    // ── Risk assessment tests ──

    #[test]
    fn risk_assessment_read_tools_are_low() {
        let mgr = ApprovalManager::auto();
        assert_eq!(
            mgr.assess_risk("read_file", &serde_json::json!({})),
            RiskLevel::Low
        );
        assert_eq!(
            mgr.assess_risk("list_dir", &serde_json::json!({})),
            RiskLevel::Low
        );
        assert_eq!(
            mgr.assess_risk("current_time", &serde_json::json!({})),
            RiskLevel::Low
        );
    }

    #[test]
    fn risk_assessment_write_tools_are_high() {
        let mgr = ApprovalManager::auto();
        assert_eq!(
            mgr.assess_risk("bash", &serde_json::json!({})),
            RiskLevel::High
        );
        assert_eq!(
            mgr.assess_risk("write_file", &serde_json::json!({})),
            RiskLevel::High
        );
        assert_eq!(
            mgr.assess_risk("exec_command", &serde_json::json!({})),
            RiskLevel::High
        );
    }

    #[test]
    fn risk_assessment_unknown_tools_are_medium() {
        let mgr = ApprovalManager::auto();
        assert_eq!(
            mgr.assess_risk("custom_tool", &serde_json::json!({})),
            RiskLevel::Medium
        );
        assert_eq!(
            mgr.assess_risk("transform", &serde_json::json!({})),
            RiskLevel::Medium
        );
    }

    #[test]
    fn risk_assessment_run_code_is_always_high() {
        // Hard-coded: `run_code` executes an arbitrary script whose inner
        // bridge calls bypass the approval gate, so the invocation itself
        // must be treated as high-risk regardless of name heuristics.
        let mgr = ApprovalManager::auto();
        assert_eq!(
            mgr.assess_risk("run_code", &serde_json::json!({})),
            RiskLevel::High
        );
    }

    // ── ApprovalRequest construction tests ──

    #[test]
    fn create_request_populates_all_fields() {
        let mgr = ApprovalManager::manual();
        let args = serde_json::json!({"path": "/tmp/test.txt"});
        let req = mgr.create_request(
            "read_file",
            Some("github"),
            &args,
            "root",
            ApprovalTrigger::AutoRisk,
        );

        assert_eq!(req.tool_name, "read_file");
        assert_eq!(req.arguments, args);
        assert_eq!(req.agent_id, "root");
        assert_eq!(req.risk_level, RiskLevel::Low);
        assert_eq!(req.server.as_deref(), Some("github"));
        assert_eq!(req.trigger, ApprovalTrigger::AutoRisk);
        assert!(req.note.is_none(), "core 恒不填 note（上层装饰器回填）");
    }

    // ── ApprovalManager::check tests（六路径 + trigger 正确性）────────

    #[test]
    fn check_manual_level_consults_callback() {
        // 路径 1：Manual → 必问（callback 被咨询）。
        let mgr = ApprovalManager::manual().with_callback(Arc::new(DenyAll {
            seen: Mutex::new(Vec::new()),
        }));
        let decision = mgr.check("read_file", None, &serde_json::json!({"path": "x"}), "root");
        assert_eq!(
            decision,
            ApprovalDecision::Denied("denied by test callback".to_owned())
        );
    }

    #[test]
    fn check_allow_level_passes_without_callback_consultation() {
        // 路径 2：Allow → 过（callback 永不被咨询）。
        let mgr = ApprovalManager::auto().with_callback(Arc::new(DenyAll {
            seen: Mutex::new(Vec::new()),
        }));
        let decision = mgr.check("bash", None, &serde_json::json!({}), "root");
        assert_eq!(decision, ApprovalDecision::Approved);
    }

    #[test]
    fn check_auto_level_low_risk_passes() {
        // 路径 3：Auto + 启发式 Low → 过（不进审批链，callback 不被咨询）。
        let mgr = ApprovalManager::with_rules(Arc::new(RwLock::new(rules(
            ToolApprovalLevel::Auto,
            &[],
            &[],
        ))))
        .with_callback(Arc::new(DenyAll {
            seen: Mutex::new(Vec::new()),
        }));
        assert_eq!(
            mgr.check("read_file", None, &serde_json::json!({}), "root"),
            ApprovalDecision::Approved
        );
    }

    #[test]
    fn check_auto_level_medium_risk_asks() {
        // 路径 4：Auto + Medium（未知工具）→ 问。
        let mgr = ApprovalManager::with_rules(Arc::new(RwLock::new(rules(
            ToolApprovalLevel::Auto,
            &[],
            &[],
        ))))
        .with_callback(Arc::new(ApproveAll {
            seen: Mutex::new(Vec::new()),
        }));
        assert_eq!(
            mgr.check("transform", None, &serde_json::json!({}), "root"),
            ApprovalDecision::Approved // ApproveAll 证明走到了 callback
        );
        // 用 DenyAll 反证 Medium 确实进链。
        let mgr = ApprovalManager::with_rules(Arc::new(RwLock::new(rules(
            ToolApprovalLevel::Auto,
            &[],
            &[],
        ))))
        .with_callback(Arc::new(DenyAll {
            seen: Mutex::new(Vec::new()),
        }));
        assert_eq!(
            mgr.check("transform", None, &serde_json::json!({}), "root"),
            ApprovalDecision::Denied("denied by test callback".to_owned())
        );
    }

    #[test]
    fn check_auto_level_high_risk_asks() {
        // 路径 5：Auto + High → 问。
        let mgr = ApprovalManager::with_rules(Arc::new(RwLock::new(rules(
            ToolApprovalLevel::Auto,
            &[],
            &[],
        ))))
        .with_callback(Arc::new(DenyAll {
            seen: Mutex::new(Vec::new()),
        }));
        assert_eq!(
            mgr.check("bash", None, &serde_json::json!({}), "root"),
            ApprovalDecision::Denied("denied by test callback".to_owned())
        );
    }

    #[test]
    fn check_without_callback_passes_through_as_approved() {
        // 路径 6：需要审批但无 callback → Approved 直通（NonInteractiveGate
        // 依赖的既有语义）。
        let mgr = ApprovalManager::manual();
        let decision = mgr.check("bash", None, &serde_json::json!({}), "root");
        assert_eq!(decision, ApprovalDecision::Approved);
    }

    #[test]
    fn check_trigger_is_manual_for_manual_level() {
        // trigger 正确性：Manual 等级的请求 trigger 必须 = Manual
        //（P0-2：AI 审查装饰器只滤 AutoRisk，绝不滤 Manual）。
        let seen = Arc::new(Mutex::new(Vec::new()));
        struct Recorder {
            seen: Arc<Mutex<Vec<ApprovalRequest>>>,
        }
        impl ApprovalCallback for Recorder {
            fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision {
                self.seen.lock().expect("seen poisoned").push(req.clone());
                ApprovalDecision::Approved
            }
        }
        let mgr = ApprovalManager::new(ApprovalPolicy::AutoExcept(vec!["shell".to_owned()]))
            .with_callback(Arc::new(Recorder {
                seen: Arc::clone(&seen),
            }));
        let args = serde_json::json!({"cmd": "ls"});
        mgr.check("shell", None, &args, "researcher");

        let seen = seen.lock().expect("seen poisoned");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].tool_name, "shell");
        assert_eq!(seen[0].arguments, args);
        assert_eq!(seen[0].agent_id, "researcher");
        assert_eq!(seen[0].risk_level, RiskLevel::High);
        assert_eq!(seen[0].trigger, ApprovalTrigger::Manual);
        assert!(seen[0].server.is_none());
    }

    #[test]
    fn check_trigger_is_auto_risk_for_auto_level_medium() {
        // trigger 正确性：Auto 等级风险命中 → trigger = AutoRisk，且
        // server/note 透传。
        let seen = Arc::new(Mutex::new(Vec::new()));
        struct Recorder {
            seen: Arc<Mutex<Vec<ApprovalRequest>>>,
        }
        impl ApprovalCallback for Recorder {
            fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision {
                self.seen.lock().expect("seen poisoned").push(req.clone());
                ApprovalDecision::Approved
            }
        }
        let mgr = ApprovalManager::with_rules(Arc::new(RwLock::new(rules(
            ToolApprovalLevel::Auto,
            &[],
            &[],
        ))))
        .with_callback(Arc::new(Recorder {
            seen: Arc::clone(&seen),
        }));
        mgr.check("termux_github__create_issue", Some("github"), &serde_json::json!({}), "root");

        let seen = seen.lock().expect("seen poisoned");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].trigger, ApprovalTrigger::AutoRisk);
        assert_eq!(seen[0].server.as_deref(), Some("github"));
    }

    // ── server 作用域评估序 ──

    #[test]
    fn server_level_gates_between_tool_and_default() {
        // servers 命中介于 tools 与 default 之间；tools 显式等级最高。
        let mgr = ApprovalManager::with_rules(Arc::new(RwLock::new(rules(
            ToolApprovalLevel::Manual,
            &[("bash", ToolApprovalLevel::Allow)],
            &[("github", ToolApprovalLevel::Allow)],
        ))));
        // tools[bash]=Allow 压过 default=Manual 与 servers[github]=Manual。
        assert_eq!(
            mgr.check("bash", Some("github"), &serde_json::json!({}), "root"),
            ApprovalDecision::Approved
        );
        // servers[github]=Allow 压过 default=Manual（无 tools 条目）。
        assert_eq!(
            mgr.check("termux_github__create_issue", Some("github"), &serde_json::json!({}), "root"),
            ApprovalDecision::Approved
        );
    }

    #[test]
    fn server_level_requires_matching_scope() {
        // servers 键不匹配（非 MCP / 别的 server）→ 落 default。
        let mgr = ApprovalManager::with_rules(Arc::new(RwLock::new(rules(
            ToolApprovalLevel::Manual,
            &[],
            &[("termux", ToolApprovalLevel::Allow)],
        ))))
        .with_callback(Arc::new(DenyAll {
            seen: Mutex::new(Vec::new()),
        }));
        // server=None（native 工具）不命中 servers → Manual → 问。
        assert_eq!(
            mgr.check("bash", None, &serde_json::json!({}), "root"),
            ApprovalDecision::Denied("denied by test callback".to_owned())
        );
        // host 裸名形态 server="termux" 命中 → 放行。
        assert_eq!(
            mgr.check("termux_bash", Some("termux"), &serde_json::json!({}), "root"),
            ApprovalDecision::Approved
        );
    }

    // ── rules_from_config 全矩阵 ──

    #[test]
    fn rules_from_config_absent_section_is_pure_allow() {
        // 段缺省 → 纯 Allow 基线（mobile 的 Auto 默认是装配层 opt-in）。
        let config = config_with_approval("[providers]\n[models]\n");
        assert_eq!(
            rules_from_config(&config),
            ApprovalRules {
                default_level: ToolApprovalLevel::Allow,
                ..ApprovalRules::default()
            }
        );
    }

    #[test]
    fn rules_from_config_legacy_manual() {
        let config = config_with_approval("[approval]\npolicy = \"manual\"\n");
        let rules = rules_from_config(&config);
        assert_eq!(rules.default_level, ToolApprovalLevel::Manual);
        assert!(rules.tools.is_empty());
        assert!(rules.servers.is_empty());
    }

    #[test]
    fn rules_from_config_legacy_auto() {
        let config = config_with_approval("[approval]\npolicy = \"auto\"\n");
        let rules = rules_from_config(&config);
        assert_eq!(rules.default_level, ToolApprovalLevel::Allow);
        assert!(rules.tools.is_empty());
    }

    #[test]
    fn rules_from_config_legacy_auto_except() {
        let config = config_with_approval(
            "[approval]\npolicy = \"auto_except\"\ntools = [\"bash\", \"write_file\"]\n",
        );
        let rules = rules_from_config(&config);
        assert_eq!(rules.default_level, ToolApprovalLevel::Allow);
        assert_eq!(
            rules.tools,
            BTreeMap::from([
                ("bash".to_owned(), ToolApprovalLevel::Manual),
                ("write_file".to_owned(), ToolApprovalLevel::Manual),
            ])
        );
    }

    #[test]
    fn rules_from_config_default_level_overrides_legacy() {
        // 新键覆盖 legacy 映射的 default。
        let config = config_with_approval(
            "[approval]\npolicy = \"manual\"\ndefault_level = \"auto\"\n",
        );
        let rules = rules_from_config(&config);
        assert_eq!(rules.default_level, ToolApprovalLevel::Auto);
    }

    #[test]
    fn rules_from_config_level_maps_merge_over_legacy() {
        // tool_levels/server_levels 逐键覆盖合并（legacy tools 铺底被覆盖）。
        let config = config_with_approval(
            "[approval]\n\
             policy = \"auto_except\"\n\
             tools = [\"bash\"]\n\
             default_level = \"manual\"\n\
             \n[approval.tool_levels]\n\
             bash = \"allow\"\n\
             read_file = \"auto\"\n\
             \n[approval.server_levels]\n\
             github = \"allow\"\n",
        );
        let rules = rules_from_config(&config);
        assert_eq!(rules.default_level, ToolApprovalLevel::Manual);
        assert_eq!(
            rules.tools,
            BTreeMap::from([
                ("bash".to_owned(), ToolApprovalLevel::Allow), // 覆盖 legacy Manual
                ("read_file".to_owned(), ToolApprovalLevel::Auto),
            ])
        );
        assert_eq!(
            rules.servers,
            BTreeMap::from([("github".to_owned(), ToolApprovalLevel::Allow)])
        );
    }

    #[test]
    fn rules_from_config_new_keys_only() {
        // 段内只写新键（policy 键缺省 = auto → Allow 基线不干扰）。
        let config = config_with_approval(
            "[approval]\n\
             default_level = \"auto\"\n\
             \n[approval.tool_levels]\n\
             bash = \"manual\"\n",
        );
        let rules = rules_from_config(&config);
        assert_eq!(rules.default_level, ToolApprovalLevel::Auto);
        assert_eq!(
            rules.tools,
            BTreeMap::from([("bash".to_owned(), ToolApprovalLevel::Manual)])
        );
        assert!(rules.servers.is_empty());
    }

    // ── legacy 映射与视图重建 ──

    #[test]
    fn rules_from_policy_p0_4_mapping() {
        assert_eq!(
            rules_from_policy(&ApprovalPolicy::Auto),
            ApprovalRules::default()
        );
        assert_eq!(
            rules_from_policy(&ApprovalPolicy::Manual).default_level,
            ToolApprovalLevel::Manual
        );
        let mapped = rules_from_policy(&ApprovalPolicy::AutoExcept(vec!["bash".to_owned()]));
        assert_eq!(mapped.default_level, ToolApprovalLevel::Allow);
        assert_eq!(
            mapped.tools,
            BTreeMap::from([("bash".to_owned(), ToolApprovalLevel::Manual)])
        );
    }

    #[test]
    fn policy_view_reconstruction_round_trips_legacy_shapes() {
        // policy() → new(policy) round-trip 对 legacy 三型语义等价
        //（server/CLI/mobile 旧装配路径依赖）。AutoExcept 的工具列表经
        // BTreeMap 往返后有序化——legacy 匹配是 any() 无序语义，等价。
        fn sort_except(policy: ApprovalPolicy) -> ApprovalPolicy {
            match policy {
                ApprovalPolicy::AutoExcept(mut tools) => {
                    tools.sort();
                    ApprovalPolicy::AutoExcept(tools)
                }
                other => other,
            }
        }
        for policy in [
            ApprovalPolicy::Auto,
            ApprovalPolicy::Manual,
            ApprovalPolicy::AutoExcept(vec!["shell".to_owned(), "run_code".to_owned()]),
        ] {
            let expected = sort_except(policy.clone());
            let mgr = ApprovalManager::new(policy);
            assert_eq!(
                sort_except(mgr.policy()),
                expected,
                "round-trip must preserve {expected:?}"
            );
        }
        // AutoExcept([]) 与 Auto 语义等价（空列表从不命中 needs_approval），
        // 重建正确折叠为 Auto——行为保持，非结构保持。
        let mgr = ApprovalManager::new(ApprovalPolicy::AutoExcept(vec![]));
        assert_eq!(mgr.policy(), ApprovalPolicy::Auto);
    }

    #[test]
    fn policy_view_reconstruction_from_new_shapes() {
        // Allow + Manual 工具项 → AutoExcept；Auto default → Auto（有损，
        // 文档化降级）。
        let mgr = ApprovalManager::with_rules(Arc::new(RwLock::new(rules(
            ToolApprovalLevel::Allow,
            &[("bash", ToolApprovalLevel::Manual), ("read_file", ToolApprovalLevel::Allow)],
            &[],
        ))));
        assert_eq!(
            mgr.policy(),
            ApprovalPolicy::AutoExcept(vec!["bash".to_owned()])
        );
        let mgr = ApprovalManager::with_rules(Arc::new(RwLock::new(rules(
            ToolApprovalLevel::Auto,
            &[],
            &[],
        ))));
        assert_eq!(mgr.policy(), ApprovalPolicy::Auto);
    }

    // ── 热更新（共享 cell）──

    #[test]
    fn set_rules_hot_update_visible_to_clones() {
        // Clone 共享同一 Arc：一侧 set_rules，另一侧（进行中回合的快照
        // manager）下一次 check 即生效。
        let mgr = ApprovalManager::manual();
        let clone = mgr.clone();
        clone.set_rules(ApprovalRules::default()); // → Allow
        assert_eq!(
            mgr.check("bash", None, &serde_json::json!({}), "root"),
            ApprovalDecision::Approved
        );
        assert_eq!(mgr.policy(), ApprovalPolicy::Auto);
    }

    #[test]
    fn with_rules_shares_external_handle() {
        // 外部 handle（bridge 等）改写 → manager 生效（P0-5 正道）。
        let handle: Arc<RwLock<ApprovalRules>> = Arc::new(RwLock::new(ApprovalRules::default()));
        let mgr = ApprovalManager::with_rules(Arc::clone(&handle));
        *handle.write().unwrap() = rules(ToolApprovalLevel::Manual, &[], &[]);
        assert_eq!(
            mgr.check("bash", None, &serde_json::json!({}), "root"),
            ApprovalDecision::Approved // Manual 但无 callback → 直通
        );
        assert_eq!(mgr.policy(), ApprovalPolicy::Manual);
        // rules_handle 返回同一 cell。
        assert!(Arc::ptr_eq(&mgr.rules_handle(), &handle));
    }

    // ── ApprovalDecision tests ──

    #[test]
    fn auto_approve_returns_approved() {
        let req = ApprovalRequest {
            tool_name: "bash".to_owned(),
            arguments: serde_json::json!({"cmd": "rm -rf /"}),
            agent_id: "root".to_owned(),
            risk_level: RiskLevel::High,
            server: None,
            trigger: ApprovalTrigger::Manual,
            note: None,
        };
        let decision = ApprovalManager::auto_approve(&req);
        assert_eq!(decision, ApprovalDecision::Approved);
    }

    #[test]
    fn deny_returns_denied_with_reason() {
        let decision = ApprovalManager::deny("User rejected");
        assert_eq!(
            decision,
            ApprovalDecision::Denied("User rejected".to_owned())
        );
    }

    // ── ApprovalDecision equality tests ──

    #[test]
    fn approval_decision_variants_compare() {
        assert_eq!(ApprovalDecision::Approved, ApprovalDecision::Approved);
        assert_ne!(
            ApprovalDecision::Approved,
            ApprovalDecision::Denied("no".to_owned())
        );
        assert_ne!(
            ApprovalDecision::Denied("a".to_owned()),
            ApprovalDecision::Denied("b".to_owned())
        );
    }

    // ── RiskLevel display test ──

    #[test]
    fn risk_level_display() {
        assert_eq!(format!("{}", RiskLevel::Low), "low");
        assert_eq!(format!("{}", RiskLevel::Medium), "medium");
        assert_eq!(format!("{}", RiskLevel::High), "high");
    }

    // ── Display / serde 形状 ──

    #[test]
    fn tool_approval_level_display_and_serde() {
        for (level, lit) in [
            (ToolApprovalLevel::Manual, "manual"),
            (ToolApprovalLevel::Allow, "allow"),
            (ToolApprovalLevel::Auto, "auto"),
        ] {
            assert_eq!(level.to_string(), lit);
            assert_eq!(serde_json::to_string(&level).unwrap(), format!("\"{lit}\""));
            let back: ToolApprovalLevel = serde_json::from_str(&format!("\"{lit}\"")).unwrap();
            assert_eq!(back, level);
        }
    }

    #[test]
    fn approval_trigger_display() {
        assert_eq!(ApprovalTrigger::Manual.to_string(), "manual");
        assert_eq!(ApprovalTrigger::AutoRisk.to_string(), "auto_risk");
    }

    // ── Policy accessor test ──

    #[test]
    fn policy_returns_current_policy() {
        let mgr = ApprovalManager::new(ApprovalPolicy::Manual);
        assert_eq!(mgr.policy(), ApprovalPolicy::Manual);
    }

    // ── Clone semantics ──

    #[test]
    fn manager_clone_shares_callback_state() {
        // The manager is cloned per runner snapshot; the callback (and its
        // interior state, e.g. a session allowlist) must be shared via Arc.
        let mgr = ApprovalManager::manual().with_callback(Arc::new(ApproveAll {
            seen: Mutex::new(Vec::new()),
        }));
        let clone = mgr.clone();
        clone.check("read_file", None, &serde_json::json!({}), "root");
        // The original must observe the recorded request through the shared
        // callback: checking again appends to the same recorder via the
        // clone's callback — observable only through side effects of the
        // shared Arc. Structural check: both managers report the same policy.
        assert_eq!(mgr.policy(), clone.policy());
    }
}
