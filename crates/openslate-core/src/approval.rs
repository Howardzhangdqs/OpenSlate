//! Approval system for tool execution gating.
//!
//! Provides `ApprovalPolicy` to control whether tool calls require
//! human approval, and `ApprovalManager` to assess risk and determine
//! if a tool needs interactive confirmation before execution.

use std::fmt;

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
    /// Tool execution is approved with modified arguments.
    /// Not fully implemented yet — treated as denied with a message.
    Modified(serde_json::Value),
}

/// Manages approval checks for tool executions.
pub struct ApprovalManager {
    policy: ApprovalPolicy,
}

impl ApprovalManager {
    /// Create a new approval manager with the given policy.
    pub fn new(policy: ApprovalPolicy) -> Self {
        Self { policy }
    }

    /// Create an approval manager with the default `Auto` policy.
    pub fn auto() -> Self {
        Self::new(ApprovalPolicy::Auto)
    }

    /// Create an approval manager with the `Manual` policy.
    pub fn manual() -> Self {
        Self::new(ApprovalPolicy::Manual)
    }

    /// Check whether a tool needs approval under the current policy.
    pub fn needs_approval(&self, tool_name: &str) -> bool {
        match &self.policy {
            ApprovalPolicy::Auto => false,
            ApprovalPolicy::Manual => true,
            ApprovalPolicy::AutoExcept(tool_list) => tool_list
                .iter()
                .any(|t| t.eq_ignore_ascii_case(tool_name)),
        }
    }

    /// Assess the risk level of a tool call based on tool name and arguments.
    pub fn assess_risk(&self, tool_name: &str, _args: &serde_json::Value) -> RiskLevel {
        let name_lower = tool_name.to_ascii_lowercase();

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
        let mgr = ApprovalManager::new(ApprovalPolicy::AutoExcept(vec![
            "Bash".to_owned(),
        ]));
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
        assert_eq!(mgr.assess_risk("read_file", &serde_json::json!({})), RiskLevel::Low);
        assert_eq!(mgr.assess_risk("list_dir", &serde_json::json!({})), RiskLevel::Low);
        assert_eq!(mgr.assess_risk("current_time", &serde_json::json!({})), RiskLevel::Low);
    }

    #[test]
    fn risk_assessment_write_tools_are_high() {
        let mgr = ApprovalManager::auto();
        assert_eq!(mgr.assess_risk("bash", &serde_json::json!({})), RiskLevel::High);
        assert_eq!(mgr.assess_risk("write_file", &serde_json::json!({})), RiskLevel::High);
        assert_eq!(mgr.assess_risk("exec_command", &serde_json::json!({})), RiskLevel::High);
    }

    #[test]
    fn risk_assessment_unknown_tools_are_medium() {
        let mgr = ApprovalManager::auto();
        assert_eq!(mgr.assess_risk("custom_tool", &serde_json::json!({})), RiskLevel::Medium);
        assert_eq!(mgr.assess_risk("transform", &serde_json::json!({})), RiskLevel::Medium);
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
        assert_eq!(decision, ApprovalDecision::Denied("User rejected".to_owned()));
    }

    // ── ApprovalDecision equality tests ──

    #[test]
    fn approval_decision_variants_compare() {
        assert_eq!(ApprovalDecision::Approved, ApprovalDecision::Approved);
        assert_ne!(ApprovalDecision::Approved, ApprovalDecision::Denied("no".to_owned()));
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
}
