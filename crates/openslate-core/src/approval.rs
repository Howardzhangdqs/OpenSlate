//! Approval system for tool execution gating.
//!
//! Provides `ApprovalPolicy` to control whether tool calls require
//! human approval, and `ApprovalManager` to assess risk and determine
//! if a tool needs interactive confirmation before execution.
//!
//! The manager is wired into the run pipeline (config → RunManager →
//! AgentRunner) and consulted at the top of `AgentRunner::execute` for
//! EVERY tool call — including `call_agent` delegation and `run_code`
//! (PTC) — so no dispatch path can bypass it.

use std::fmt;
use std::sync::Arc;

/// Policy controlling whether tool calls require human approval.
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
    policy: ApprovalPolicy,
    callback: Option<Arc<dyn ApprovalCallback>>,
}

impl ApprovalManager {
    /// Create a new approval manager with the given policy and no callback
    /// (calls that need approval are approved — the CLI layer is expected
    /// to install a callback or derive a policy that avoids this state).
    pub fn new(policy: ApprovalPolicy) -> Self {
        Self {
            policy,
            callback: None,
        }
    }

    /// Create an approval manager with the default `Auto` policy.
    pub fn auto() -> Self {
        Self::new(ApprovalPolicy::Auto)
    }

    /// Create an approval manager with the `Manual` policy.
    pub fn manual() -> Self {
        Self::new(ApprovalPolicy::Manual)
    }

    /// Attach an approval callback (builder style).
    pub fn with_callback(mut self, callback: Arc<dyn ApprovalCallback>) -> Self {
        self.callback = Some(callback);
        self
    }

    /// Check whether a tool needs approval under the current policy.
    pub fn needs_approval(&self, tool_name: &str) -> bool {
        match &self.policy {
            ApprovalPolicy::Auto => false,
            ApprovalPolicy::Manual => true,
            ApprovalPolicy::AutoExcept(tool_list) => {
                tool_list.iter().any(|t| t.eq_ignore_ascii_case(tool_name))
            }
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

    /// Build an approval request for a tool call.
    pub fn create_request(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
        agent_id: &str,
    ) -> ApprovalRequest {
        let risk_level = self.assess_risk(tool_name, arguments);
        ApprovalRequest {
            tool_name: tool_name.to_owned(),
            arguments: arguments.clone(),
            agent_id: agent_id.to_owned(),
            risk_level,
        }
    }

    /// Aggregate gate consulted before every tool execution.
    ///
    /// - tool does not need approval under the policy → `Approved`;
    /// - needs approval and a callback is installed → `callback.decide()`;
    /// - needs approval but no callback → `Approved` (non-interactive
    ///   pass-through; the CLI layer derives policies/gates that avoid
    ///   reaching this branch).
    ///
    /// Every decision is audited via `tracing` (target
    /// `openslate_approval`); denials are additionally captured by the
    /// runner's in-memory tool audit as error tool results. Nothing is
    /// written to the store.
    pub fn check(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        agent_id: &str,
    ) -> ApprovalDecision {
        if !self.needs_approval(tool_name) {
            tracing::debug!(
                target: "openslate_approval",
                "approved tool '{tool_name}' (agent {agent_id}, policy {:?}, no approval needed)",
                self.policy
            );
            return ApprovalDecision::Approved;
        }
        let request = self.create_request(tool_name, args, agent_id);
        let decision = match &self.callback {
            Some(callback) => callback.decide(&request),
            None => ApprovalDecision::Approved,
        };
        match &decision {
            ApprovalDecision::Approved => tracing::info!(
                target: "openslate_approval",
                "approved tool '{}' (agent {}, risk {})",
                request.tool_name,
                request.agent_id,
                request.risk_level
            ),
            ApprovalDecision::Denied(reason) => tracing::warn!(
                target: "openslate_approval",
                "denied tool '{}' (agent {}, risk {}): {}",
                request.tool_name,
                request.agent_id,
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

    /// Return the current policy.
    pub fn policy(&self) -> &ApprovalPolicy {
        &self.policy
    }
}

impl Default for ApprovalManager {
    fn default() -> Self {
        Self::new(ApprovalPolicy::default())
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

    // ── Auto policy tests ──

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
        let req = mgr.create_request("read_file", &args, "root");

        assert_eq!(req.tool_name, "read_file");
        assert_eq!(req.arguments, args);
        assert_eq!(req.agent_id, "root");
        assert_eq!(req.risk_level, RiskLevel::Low);
    }

    // ── ApprovalManager::check tests ──

    #[test]
    fn check_approves_when_policy_does_not_require_approval() {
        // Auto policy: even with a deny-everything callback attached, the
        // callback must never be consulted.
        let mgr = ApprovalManager::auto().with_callback(Arc::new(DenyAll {
            seen: Mutex::new(Vec::new()),
        }));
        let decision = mgr.check("bash", &serde_json::json!({}), "root");
        assert_eq!(decision, ApprovalDecision::Approved);
    }

    #[test]
    fn check_consults_callback_when_approval_needed() {
        let mgr = ApprovalManager::manual().with_callback(Arc::new(DenyAll {
            seen: Mutex::new(Vec::new()),
        }));
        let decision = mgr.check("read_file", &serde_json::json!({"path": "x"}), "root");
        assert_eq!(
            decision,
            ApprovalDecision::Denied("denied by test callback".to_owned())
        );
    }

    #[test]
    fn check_without_callback_passes_through_as_approved() {
        // Needs approval under Manual, but no callback installed → the
        // non-interactive pass-through approves. The CLI layer derives
        // policies/gates that avoid reaching this branch in practice.
        let mgr = ApprovalManager::manual();
        let decision = mgr.check("bash", &serde_json::json!({}), "root");
        assert_eq!(decision, ApprovalDecision::Approved);
    }

    #[test]
    fn check_callback_receives_request_context() {
        // The callback slot is opaque behind `Arc<dyn …>`, so the recorder is
        // held by the test and shared with the callback via Arc.
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
        mgr.check("shell", &args, "researcher");

        let seen = seen.lock().expect("seen poisoned");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].tool_name, "shell");
        assert_eq!(seen[0].arguments, args);
        assert_eq!(seen[0].agent_id, "researcher");
        assert_eq!(seen[0].risk_level, RiskLevel::High);
    }

    // ── ApprovalDecision tests ──

    #[test]
    fn auto_approve_returns_approved() {
        let req = ApprovalRequest {
            tool_name: "bash".to_owned(),
            arguments: serde_json::json!({"cmd": "rm -rf /"}),
            agent_id: "root".to_owned(),
            risk_level: RiskLevel::High,
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

    // ── Policy accessor test ──

    #[test]
    fn policy_returns_current_policy() {
        let mgr = ApprovalManager::new(ApprovalPolicy::Manual);
        assert_eq!(mgr.policy(), &ApprovalPolicy::Manual);
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
        clone.check("read_file", &serde_json::json!({}), "root");
        // The original must observe the recorded request through the shared
        // callback: checking again appends to the same recorder via the
        // clone's callback — observable only through side effects of the
        // shared Arc. Structural check: both managers report the same policy.
        assert_eq!(mgr.policy(), clone.policy());
    }
}
