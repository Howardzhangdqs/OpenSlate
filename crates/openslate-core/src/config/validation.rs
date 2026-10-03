//! Configuration validation for OpenSlate.
//!
//! Validates that `openslate.toml` + `agents/*.md` form a consistent,
//! complete configuration. Returns structured errors (and optional warnings)
//! instead of panicking.

use std::collections::HashSet;

use crate::config::{
    AgentsConfig, ApprovalPolicySetting, OpenSlateConfig, PtcConfig, TransportConfig,
};

/// Severity of a validation finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    /// A critical problem that prevents correct operation.
    Error,
    /// A non-critical issue that may indicate misconfiguration.
    Warning,
}

/// A single validation finding (error or warning).
#[derive(Debug, Clone)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

/// A validation finding with an attached severity level.
#[derive(Debug, Clone)]
pub struct ValidationFinding {
    pub severity: Severity,
    pub field: String,
    pub message: String,
}

/// Result of a full validation run containing both errors and warnings.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    pub errors: Vec<ValidationError>,
    pub warnings: Vec<ValidationError>,
}

impl ValidationResult {
    /// Returns `true` if there are no errors.
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    /// Convert into a flat list of findings with severity attached.
    pub fn into_findings(self) -> Vec<ValidationFinding> {
        let errors = self.errors.into_iter().map(|e| ValidationFinding {
            severity: Severity::Error,
            field: e.field,
            message: e.message,
        });
        let warnings = self.warnings.into_iter().map(|w| ValidationFinding {
            severity: Severity::Warning,
            field: w.field,
            message: w.message,
        });
        errors.chain(warnings).collect()
    }
}

/// Validate the complete configuration (`openslate.toml` + `agents/*.md`).
///
/// Returns a list of errors — empty means valid.
pub fn validate_config(config: &OpenSlateConfig, agents: &AgentsConfig) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // 1. model level `main` must be resolvable: either a `[models]` entry
    //    named `main` (legacy layout) or a `levels.main` mapping to any entry
    if !config.models.contains_key("main") && !config.levels.contains_key("main") {
        errors.push(ValidationError {
            field: "levels.main".into(),
            message: "Required model level 'main' is missing — define a [models.main] entry \
                      directly, or map it via [levels] main = \"<model entry>\""
                .into(),
        });
    }

    // 2. model level `fast` must be resolvable (same two ways as `main`)
    if !config.models.contains_key("fast") && !config.levels.contains_key("fast") {
        errors.push(ValidationError {
            field: "levels.fast".into(),
            message: "Required model level 'fast' is missing — define a [models.fast] entry \
                      directly, or map it via [levels] fast = \"<model entry>\""
                .into(),
        });
    }

    // 3. Every model alias must reference an existing provider
    for (alias, model) in &config.models {
        if !config.providers.contains_key(&model.provider) {
            errors.push(ValidationError {
                field: format!("models.{alias}.provider"),
                message: format!(
                    "Model '{}' references non-existent provider '{}'",
                    alias, model.provider
                ),
            });
        }
    }

    // 4. agent_id must be unique
    let mut seen_ids = HashSet::new();
    for agent in &agents.agents {
        if !seen_ids.insert(agent.id.clone()) {
            errors.push(ValidationError {
                field: format!("agents.{}", agent.id),
                message: format!("Duplicate agent id '{}'", agent.id),
            });
        }
    }

    // 5. Must have at least one root agent
    let all_children: HashSet<_> = agents
        .agents
        .iter()
        .flat_map(|a| a.children.iter())
        .collect();
    let root_agents: Vec<_> = agents
        .agents
        .iter()
        .filter(|a| !all_children.contains(&a.id))
        .collect();

    if root_agents.is_empty() {
        errors.push(ValidationError {
            field: "agents".into(),
            message: "No root agent found (every agent is listed as a child of another)".into(),
        });
    }

    // 6. Children references must exist as agent ids
    let agent_ids: HashSet<_> = agents.agents.iter().map(|a| &a.id).collect();
    for agent in &agents.agents {
        for child_id in &agent.children {
            if !agent_ids.contains(child_id) {
                errors.push(ValidationError {
                    field: format!("agents.{}.children", agent.id),
                    message: format!(
                        "Agent '{}' references non-existent child '{}'",
                        agent.id, child_id
                    ),
                });
            }
        }
    }

    // 7. Agent model references must exist — as a `[models]` entry or as a
    //    `[levels]` name (levels are what agents reference under the
    //    model-mgmt-1 two-layer schema).
    for agent in &agents.agents {
        if !config.models.contains_key(&agent.model) && !config.levels.contains_key(&agent.model) {
            errors.push(ValidationError {
                field: format!("agents.{}.model", agent.id),
                message: format!(
                    "Agent '{}' references non-existent model alias '{}'",
                    agent.id, agent.model
                ),
            });
        }
    }

    // 8. Limits validation (0 means unlimited for max_steps/max_tool_calls)
    if let Some(limits) = &config.limits {
        if limits.max_depth == 0 {
            errors.push(ValidationError {
                field: "limits.max_depth".into(),
                message: "max_depth must be > 0".into(),
            });
        }
        if limits.timeout_ms == 0 {
            errors.push(ValidationError {
                field: "limits.timeout_ms".into(),
                message: "timeout_ms must be > 0".into(),
            });
        }
    }

    // ── New validation rules (9–18) ──────────────────────────────────────

    // 9. Provider base_url must be a valid URL
    for (name, provider) in &config.providers {
        if provider.base_url.trim().is_empty() {
            errors.push(ValidationError {
                field: format!("providers.{name}.base_url"),
                message: format!("Provider '{}' has an empty base_url", name),
            });
        } else if !is_valid_url(&provider.base_url) {
            errors.push(ValidationError {
                field: format!("providers.{name}.base_url"),
                message: format!(
                    "Provider '{}' has an invalid base_url '{}'",
                    name, provider.base_url
                ),
            });
        }
    }

    // 10. Provider api_key_env must be non-empty
    for (name, provider) in &config.providers {
        if provider.api_key_env.trim().is_empty() {
            errors.push(ValidationError {
                field: format!("providers.{name}.api_key_env"),
                message: format!("Provider '{}' has an empty api_key_env", name),
            });
        }
    }

    // 11. Model model field must be non-empty
    for (alias, model) in &config.models {
        if model.model.trim().is_empty() {
            errors.push(ValidationError {
                field: format!("models.{alias}.model"),
                message: format!("Model alias '{}' has an empty model field", alias),
            });
        }
    }

    // 12. Agent id must be a valid identifier (alphanumeric + underscore + hyphen)
    for agent in &agents.agents {
        if !is_valid_agent_id(&agent.id.0) {
            errors.push(ValidationError {
                field: format!("agents.{}", agent.id),
                message: format!(
                    "Agent id '{}' is invalid: must contain only alphanumeric characters, underscores, and hyphens",
                    agent.id
                ),
            });
        }
    }

    // 13. Circular children references in agents
    errors.extend(detect_circular_children(agents));

    // 14. Database path must be valid (if specified)
    if let Some(db) = &config.database {
        if let Some(path) = &db.path {
            if path.trim().is_empty() {
                errors.push(ValidationError {
                    field: "database.path".into(),
                    message: "Database path is specified but empty".into(),
                });
            }
        }
    }

    // 15. MCP server transport fields must be valid (static checks only;
    //     reachability/collisions are handled at registry-build time).
    if let Some(mcp) = &config.mcp {
        for (name, server) in &mcp.servers {
            match &server.transport {
                TransportConfig::Stdio { command, .. } => {
                    if command.trim().is_empty() {
                        errors.push(ValidationError {
                            field: format!("mcp.servers.{name}.command"),
                            message: format!("MCP server '{}' has an empty command", name),
                        });
                    }
                }
                TransportConfig::Http { url, .. } => {
                    if url.trim().is_empty() {
                        errors.push(ValidationError {
                            field: format!("mcp.servers.{name}.url"),
                            message: format!("MCP server '{}' has an empty url", name),
                        });
                    } else if !is_valid_url(url) {
                        errors.push(ValidationError {
                            field: format!("mcp.servers.{name}.url"),
                            message: format!("MCP server '{}' has an invalid url '{}'", name, url),
                        });
                    }
                }
            }
        }
    }

    // 16. `[ptc]` numeric floors: zero/absurd values brick the feature
    //     (0 ms timeout, 0-byte heap, 0 tool calls per run), so they are
    //     rejected with the floor stated in the message. Enforced both
    //     here and at config load (`parse_openslate_toml`).
    errors.extend(ptc_floor_errors(&config.ptc));

    // 17. Every `[levels]` value must reference an existing `[models]` entry
    //     (the level would otherwise be unresolvable at use time).
    for (name, entry) in &config.levels {
        if !config.models.contains_key(entry) {
            errors.push(ValidationError {
                field: format!("levels.{name}"),
                message: format!("Level '{name}' points to non-existent model entry '{entry}'"),
            });
        }
    }

    errors
}

/// Numeric floors for `[ptc]` values (PTC_PLAN.md §6.2). `max_list_chars`
/// is exempt: 0 is its documented "unlimited" meaning (same convention as
/// `limits.max_steps`). `pub(crate)` so config parsing can fail fast at
/// load time with the same messages.
pub(crate) fn ptc_floor_errors(ptc: &PtcConfig) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    if ptc.timeout_ms < 100 {
        errors.push(ValidationError {
            field: "ptc.timeout_ms".into(),
            message: format!(
                "timeout_ms must be >= 100 ms (got {}); smaller budgets fire \
                 before the sandbox can even start",
                ptc.timeout_ms
            ),
        });
    }
    if ptc.memory_limit_bytes < 1_048_576 {
        errors.push(ValidationError {
            field: "ptc.memory_limit_bytes".into(),
            message: format!(
                "memory_limit_bytes must be >= 1048576 (1 MiB) (got {}); \
                 the QuickJS isolate cannot start below that",
                ptc.memory_limit_bytes
            ),
        });
    }
    if ptc.max_output_bytes < 1024 {
        errors.push(ValidationError {
            field: "ptc.max_output_bytes".into(),
            message: format!(
                "max_output_bytes must be >= 1024 (got {})",
                ptc.max_output_bytes
            ),
        });
    }
    if ptc.max_tool_calls_per_run < 1 {
        errors.push(ValidationError {
            field: "ptc.max_tool_calls_per_run".into(),
            message: format!(
                "max_tool_calls_per_run must be >= 1 (got {}); 0 would make \
                 every run_code script useless",
                ptc.max_tool_calls_per_run
            ),
        });
    }
    if ptc.max_lookup_calls < 1 {
        errors.push(ValidationError {
            field: "ptc.max_lookup_calls".into(),
            message: format!(
                "max_lookup_calls must be >= 1 (got {}); 0 would make \
                 list_tools/describe_tool useless",
                ptc.max_lookup_calls
            ),
        });
    }
    errors
}

/// Strict validation that also warns about non-critical issues.
///
/// Returns `(errors, warnings)`.
pub fn validate_strict(
    config: &OpenSlateConfig,
    agents: &AgentsConfig,
) -> (Vec<ValidationError>, Vec<ValidationError>) {
    let errors = validate_config(config, agents);
    let mut warnings = Vec::new();

    // Check for unused models (models not referenced by any agent)
    let used_models: HashSet<_> = agents.agents.iter().map(|a| &a.model).collect();
    for alias in config.models.keys() {
        if !used_models.contains(alias) {
            warnings.push(ValidationError {
                field: format!("models.{alias}"),
                message: format!("Model alias '{}' is not used by any agent", alias),
            });
        }
    }

    warnings.extend(disabled_builtin_tool_warnings(config, agents));
    warnings.extend(approval_warnings(config, agents));

    (errors, warnings)
}

/// Full validation returning a structured `ValidationResult` with both errors
/// and warnings, suitable for UI display or logging.
///
/// This runs all error-level checks from [`validate_config`] plus the extended
/// warning checks from [`validate_strict`] and additional warning rules.
pub fn validate_config_full(config: &OpenSlateConfig, agents: &AgentsConfig) -> ValidationResult {
    let errors = validate_config(config, agents);
    let mut warnings = Vec::new();

    // ── Warning: unused model aliases ────────────────────────────────────
    let used_models: HashSet<_> = agents.agents.iter().map(|a| &a.model).collect();
    for alias in config.models.keys() {
        if !used_models.contains(alias) {
            warnings.push(ValidationError {
                field: format!("models.{alias}"),
                message: format!("Model alias '{}' is not used by any agent", alias),
            });
        }
    }

    // ── Warning: unused provider configurations ──────────────────────────
    let used_providers: HashSet<_> = config.models.values().map(|m| &m.provider).collect();
    for name in config.providers.keys() {
        if !used_providers.contains(&name) {
            warnings.push(ValidationError {
                field: format!("providers.{name}"),
                message: format!("Provider '{}' is not referenced by any model", name),
            });
        }
    }

    // ── Warning: agents with no tools ────────────────────────────────────
    for agent in &agents.agents {
        if agent.tools.is_empty() {
            warnings.push(ValidationError {
                field: format!("agents.{}.tools", agent.id),
                message: format!("Agent '{}' has no tools configured", agent.id),
            });
        }
    }

    // ── Warning: default prompt is very short (< 10 chars) ───────────────
    for agent in &agents.agents {
        if agent.default_prompt.trim().len() < 10 {
            warnings.push(ValidationError {
                field: format!("agents.{}.default_prompt", agent.id),
                message: format!(
                    "Agent '{}' has a very short default_prompt ({} chars)",
                    agent.id,
                    agent.default_prompt.len()
                ),
            });
        }
    }

    // ── Warning: agent whitelists reference disabled builtin tools ───────
    warnings.extend(disabled_builtin_tool_warnings(config, agents));
    warnings.extend(approval_warnings(config, agents));

    ValidationResult { errors, warnings }
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Warnings for agents whose `tools` whitelist references a builtin tool that
/// `[builtin_tools]` disables. Not an error — whitelists may legitimately
/// reference external MCP tools unknown at validation time — but the agent
/// silently loses a capability it was configured to use, which is worth a
/// warning. Only exact name references are checked (globs like `*` are skipped
/// because they also match external tools).
fn disabled_builtin_tool_warnings(
    config: &OpenSlateConfig,
    agents: &AgentsConfig,
) -> Vec<ValidationError> {
    let bt = &config.builtin_tools;
    // (tool name, still available?) — unavailable when the master switch is
    // off or the specific tool flag is off.
    let availability: [(&str, bool); 4] = [
        ("read_file", bt.enabled && bt.read_file),
        ("write_file", bt.enabled && bt.write_file),
        ("shell", bt.enabled && bt.shell),
        ("edit_file", bt.enabled && bt.edit_file),
    ];

    let mut warnings = Vec::new();
    for agent in &agents.agents {
        for (name, available) in availability {
            if !available && agent.tools.iter().any(|t| t == name) {
                warnings.push(ValidationError {
                    field: format!("agents.{}.tools", agent.id),
                    message: format!(
                        "Agent '{}' whitelists builtin tool '{}' but it is disabled in [builtin_tools]",
                        agent.id, name
                    ),
                });
            }
        }
    }
    warnings
}

/// Warnings for `[approval]` drift (never fatal, read_skill-style exact-name
/// comparisons):
///
/// - `auto_except` with an empty `tools` list never asks for anything —
///   equivalent to `auto` and almost certainly a misconfiguration;
/// - a `tools` entry that matches no known tool name can never gate
///   anything. Known names are the builtins (`read_file` / `write_file` /
///   `shell` / `edit_file` / `read_skill`), the special tools (`run_code`,
///   `call_agent`), and every entry of every agent's `tools:` whitelist —
///   external MCP tool names are not knowable at validation time, so such
///   entries may be false positives (the message says so).
fn approval_warnings(config: &OpenSlateConfig, agents: &AgentsConfig) -> Vec<ValidationError> {
    let Some(approval) = config.approval.as_ref() else {
        return Vec::new();
    };
    let mut warnings = Vec::new();

    if approval.policy == ApprovalPolicySetting::AutoExcept && approval.tools.is_empty() {
        warnings.push(ValidationError {
            field: "approval.tools".into(),
            message: "policy is 'auto_except' but the tools list is empty — nothing will \
                      require approval (equivalent to 'auto')"
                .into(),
        });
    }

    let mut known: HashSet<String> = [
        "read_file",
        "write_file",
        "shell",
        "edit_file",
        "read_skill",
        "run_code",
        "call_agent",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    for agent in &agents.agents {
        known.extend(agent.tools.iter().cloned());
    }
    for tool in &approval.tools {
        if !known.iter().any(|k| k.eq_ignore_ascii_case(tool)) {
            warnings.push(ValidationError {
                field: "approval.tools".into(),
                message: format!(
                    "tool '{tool}' is not a known tool (builtin, run_code/call_agent, or in \
                     any agent whitelist) — approval can never match it; if it is an external \
                     MCP tool this warning may be a false positive"
                ),
            });
        }
    }
    warnings
}

/// Check if a string is a valid HTTP(S) URL.
fn is_valid_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Check if an agent id contains only valid characters.
fn is_valid_agent_id(id: &str) -> bool {
    if id.is_empty() {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Detect circular children references using DFS cycle detection.
fn detect_circular_children(agents: &AgentsConfig) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // Build adjacency map: agent_id → children ids
    let adj: std::collections::HashMap<String, Vec<String>> = agents
        .agents
        .iter()
        .map(|a| {
            (
                a.id.0.clone(),
                a.children.iter().map(|c| c.0.clone()).collect(),
            )
        })
        .collect();

    let mut visited = HashSet::new();
    let mut in_stack = HashSet::new();

    for agent in &agents.agents {
        let id_str = agent.id.0.clone();
        if !visited.contains(&id_str) {
            if let Some(cycle) = dfs_cycle(&id_str, &adj, &mut visited, &mut in_stack) {
                errors.push(ValidationError {
                    field: format!("agents.{}", cycle),
                    message: format!(
                        "Circular children reference detected involving agent '{}'",
                        cycle
                    ),
                });
            }
        }
    }

    errors
}

/// DFS-based cycle detection. Returns the first agent id in a cycle if found.
fn dfs_cycle(
    node: &str,
    adj: &std::collections::HashMap<String, Vec<String>>,
    visited: &mut HashSet<String>,
    in_stack: &mut HashSet<String>,
) -> Option<String> {
    visited.insert(node.to_owned());
    in_stack.insert(node.to_owned());

    if let Some(children) = adj.get(node) {
        for child in children {
            if in_stack.contains(child) {
                // Found a cycle — return the node where we detected it
                in_stack.remove(node);
                return Some(child.clone());
            }
            if !visited.contains(child) {
                if let Some(cycle) = dfs_cycle(child, adj, visited, in_stack) {
                    in_stack.remove(node);
                    return Some(cycle);
                }
            }
        }
    }

    in_stack.remove(node);
    None
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{parse_agents_dir, parse_openslate_toml};
    use crate::types::{AgentConfig, AgentId};
    use std::path::Path;

    // ── Helpers ───────────────────────────────────────────────────────────

    /// Build a minimal valid config for mutation in tests.
    fn valid_config() -> OpenSlateConfig {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"

[limits]
max_steps = 10
max_depth = 3
max_tool_calls = 20
max_child_agent_calls = 5
timeout_ms = 30000
max_context_messages = 16
max_context_bytes = 64000
max_output_bytes = 65536
"#;
        parse_openslate_toml(toml).expect("fixture should parse")
    }

    fn valid_agents() -> AgentsConfig {
        AgentsConfig {
            agents: vec![
                AgentConfig {
                    id: AgentId("root".into()),
                    name: "Root".into(),
                    model: "main".into(),
                    children: vec![AgentId("worker".into())],
                    tools: vec!["read_file".into()],
                    default_prompt: "root prompt here".into(),
                },
                AgentConfig {
                    id: AgentId("worker".into()),
                    name: "Worker".into(),
                    model: "fast".into(),
                    tools: vec!["write_file".into()],
                    default_prompt: "worker prompt here".into(),
                    children: vec![],
                },
            ],
        }
    }

    /// Build an `AgentsConfig` with a single agent.
    fn single_agent(
        id: &str,
        name: &str,
        model: &str,
        tools: Vec<&str>,
        children: Vec<&str>,
        prompt: &str,
    ) -> AgentsConfig {
        AgentsConfig {
            agents: vec![AgentConfig {
                id: AgentId(id.into()),
                name: name.into(),
                model: model.into(),
                tools: tools.into_iter().map(String::from).collect(),
                children: children.into_iter().map(|c| AgentId(c.into())).collect(),
                default_prompt: prompt.into(),
            }],
        }
    }

    // ── Rule 1 & 2: required model aliases ───────────────────────────────

    #[test]
    fn valid_config_produces_zero_errors() {
        let errors = validate_config(&valid_config(), &valid_agents());
        assert!(errors.is_empty(), "expected no errors: {errors:?}");
    }

    #[test]
    fn missing_models_main() {
        let mut config = valid_config();
        config.models.remove("main");
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors.iter().any(|e| e.field == "levels.main"
                && e.message.contains("[models.main]")
                && e.message.contains("[levels]")),
            "field is levels.main and message explains both setups: {errors:?}"
        );
    }

    #[test]
    fn missing_models_fast() {
        let mut config = valid_config();
        config.models.remove("fast");
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors.iter().any(|e| e.field == "levels.fast"
                && e.message.contains("[models.fast]")
                && e.message.contains("[levels]")),
            "field is levels.fast and message explains both setups: {errors:?}"
        );
    }

    // ── Rule 1/2 (new schema): a [levels] mapping satisfies the required
    // ── levels without a same-named [models] entry.

    #[test]
    fn level_mapping_satisfies_required_main_and_fast() {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.glm5]
provider = "zhipu"
model = "m1"

[models.mini]
provider = "zhipu"
model = "m2"

[levels]
main = "glm5"
fast = "mini"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors.is_empty(),
            "level-mapped main/fast are valid: {errors:?}"
        );
    }

    // ── Rule 17: [levels] values must reference existing [models] entries ─

    #[test]
    fn levels_unknown_entry_rejected() {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"

[levels]
deep = "ghost"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors.iter().any(|e| e.field == "levels.deep"
                && e.message.contains("ghost")
                && e.message.contains("non-existent")),
            "{errors:?}"
        );
    }

    #[test]
    fn levels_self_pointing_entries_valid() {
        // Self-referencing levels (entry name == level name) — the backward
        // compatible layout — must produce no rule-17 errors.
        let mut config = valid_config();
        config.levels.insert("main".into(), "main".into());
        config.levels.insert("fast".into(), "fast".into());
        let errors = validate_config(&config, &valid_agents());
        assert!(errors.is_empty(), "self-pointing levels valid: {errors:?}");
    }

    // ── Rule 3: model → provider reference ───────────────────────────────

    #[test]
    fn model_references_nonexistent_provider() {
        let mut config = valid_config();
        if let Some(m) = config.models.get_mut("main") {
            m.provider = "ghost".into();
        }
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors
                .iter()
                .any(|e| e.field == "models.main.provider" && e.message.contains("ghost")),
            "{errors:?}"
        );
    }

    // ── Rule 4: duplicate agent id ───────────────────────────────────────

    #[test]
    fn duplicate_agent_id() {
        let agents = AgentsConfig {
            agents: vec![
                AgentConfig {
                    id: AgentId("root".into()),
                    name: "Root".into(),
                    model: "main".into(),
                    tools: vec![],
                    children: vec![],
                    default_prompt: "p1".into(),
                },
                AgentConfig {
                    id: AgentId("root".into()),
                    name: "Root Dupe".into(),
                    model: "fast".into(),
                    tools: vec![],
                    children: vec![],
                    default_prompt: "p2".into(),
                },
            ],
        };
        let errors = validate_config(&valid_config(), &agents);
        assert!(
            errors
                .iter()
                .any(|e| e.field == "agents.root" && e.message.contains("Duplicate")),
            "{errors:?}"
        );
    }

    // ── Rule 5: at least one root agent ──────────────────────────────────

    #[test]
    fn no_root_agent() {
        // Every agent is a child of another → no root
        let agents = AgentsConfig {
            agents: vec![
                AgentConfig {
                    id: AgentId("a".into()),
                    name: "A".into(),
                    model: "main".into(),
                    tools: vec![],
                    children: vec![AgentId("b".into())],
                    default_prompt: "p".into(),
                },
                AgentConfig {
                    id: AgentId("b".into()),
                    name: "B".into(),
                    model: "fast".into(),
                    tools: vec![],
                    children: vec![AgentId("a".into())],
                    default_prompt: "p".into(),
                },
            ],
        };
        let errors = validate_config(&valid_config(), &agents);
        assert!(
            errors
                .iter()
                .any(|e| e.field == "agents" && e.message.contains("root")),
            "{errors:?}"
        );
    }

    // ── Rule 6: child references must exist ──────────────────────────────

    #[test]
    fn child_references_nonexistent_agent() {
        let agents = single_agent("root", "Root", "main", vec![], vec!["phantom"], "p");
        let errors = validate_config(&valid_config(), &agents);
        assert!(
            errors
                .iter()
                .any(|e| e.field == "agents.root.children" && e.message.contains("phantom")),
            "{errors:?}"
        );
    }

    // ── Rule 7: agent model must exist as alias ──────────────────────────

    #[test]
    fn agent_references_nonexistent_model() {
        let agents = single_agent("root", "Root", "nonexistent", vec![], vec![], "p");
        let errors = validate_config(&valid_config(), &agents);
        assert!(
            errors
                .iter()
                .any(|e| e.field == "agents.root.model" && e.message.contains("nonexistent")),
            "{errors:?}"
        );
    }

    #[test]
    fn agent_may_reference_a_level_only_name() {
        // "deep" exists only as a level (mapped to entry "main") — agents
        // reference levels, so this must pass rule 7.
        let mut config = valid_config();
        config.levels.insert("deep".into(), "main".into());
        let agents = single_agent("root", "Root", "deep", vec![], vec![], "p");
        let errors = validate_config(&config, &agents);
        assert!(
            !errors.iter().any(|e| e.field == "agents.root.model"),
            "level-only agent model reference is valid: {errors:?}"
        );
    }

    // ── Rule 8: limits validation ──────────────────────────────────────

    #[test]
    fn limits_max_steps_zero_means_unlimited() {
        let mut config = valid_config();
        if let Some(l) = config.limits.as_mut() {
            l.max_steps = 0;
        }
        let errors = validate_config(&config, &valid_agents());
        assert!(
            !errors.iter().any(|e| e.field == "limits.max_steps"),
            "max_steps=0 should be valid (unlimited), got errors: {errors:?}"
        );
    }

    #[test]
    fn limits_max_depth_zero() {
        let mut config = valid_config();
        if let Some(l) = config.limits.as_mut() {
            l.max_depth = 0;
        }
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors.iter().any(|e| e.field == "limits.max_depth"),
            "{errors:?}"
        );
    }

    #[test]
    fn limits_timeout_ms_zero() {
        let mut config = valid_config();
        if let Some(l) = config.limits.as_mut() {
            l.timeout_ms = 0;
        }
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors.iter().any(|e| e.field == "limits.timeout_ms"),
            "{errors:?}"
        );
    }

    // ── Strict mode: unused model warning ────────────────────────────────

    #[test]
    fn strict_mode_unused_model_warning() {
        let mut config = valid_config();
        config.models.insert(
            "unused".into(),
            crate::config::ModelConfig {
                provider: "zhipu".into(),
                model: "unused-model".into(),
                max_context_tokens: None,
                max_output_tokens: None,
                supports_tool_call: true,
                supports_vision: false,
                supports_reasoning: false,
                input_price_per_mtok: None,
                output_price_per_mtok: None,
            },
        );
        let (errors, warnings) = validate_strict(&config, &valid_agents());
        assert!(errors.is_empty(), "no errors expected: {errors:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.field == "models.unused" && w.message.contains("not used")),
            "{warnings:?}"
        );
    }

    // ── Full example files ───────────────────────────────────────────────

    #[test]
    fn example_config_is_valid() {
        let config = parse_openslate_toml(include_str!("../../fixtures/openslate.toml"))
            .expect("example toml should parse");
        let agents_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/agents");
        let agents =
            parse_agents_dir(Path::new(agents_dir)).expect("example agents dir should parse");

        let errors = validate_config(&config, &agents);
        assert!(
            errors.is_empty(),
            "example config should be valid: {errors:?}"
        );
    }

    // ── No limits section is fine (limits is optional) ───────────────────

    #[test]
    fn no_limits_section_is_valid() {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert!(config.limits.is_none());
        let errors = validate_config(&config, &valid_agents());
        assert!(errors.is_empty(), "no limits section is ok: {errors:?}");
    }

    // ════════════════════════════════════════════════════════════════════════
    // ── New tests for enhanced validation rules ──────────────────────────
    // ════════════════════════════════════════════════════════════════════════

    // ── Rule 9: provider base_url must be valid URL ──────────────────────

    #[test]
    fn provider_base_url_invalid() {
        let toml = r#"
[providers.bad]
base_url = "not-a-url"
api_key_env = "KEY"

[models.main]
provider = "bad"
model = "m1"

[models.fast]
provider = "bad"
model = "m2"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors
                .iter()
                .any(|e| e.field == "providers.bad.base_url" && e.message.contains("invalid")),
            "{errors:?}"
        );
    }

    #[test]
    fn provider_base_url_ftp_rejected() {
        let toml = r#"
[providers.ftp]
base_url = "ftp://example.com"
api_key_env = "KEY"

[models.main]
provider = "ftp"
model = "m1"

[models.fast]
provider = "ftp"
model = "m2"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors
                .iter()
                .any(|e| e.field == "providers.ftp.base_url" && e.message.contains("invalid")),
            "{errors:?}"
        );
    }

    // ── Rule 10: provider api_key_env must be non-empty ──────────────────

    #[test]
    fn provider_api_key_env_empty() {
        let toml = r#"
[providers.empty]
base_url = "https://example.com"
api_key_env = ""

[models.main]
provider = "empty"
model = "m1"

[models.fast]
provider = "empty"
model = "m2"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors
                .iter()
                .any(|e| e.field == "providers.empty.api_key_env" && e.message.contains("empty")),
            "{errors:?}"
        );
    }

    // ── Rule 11: model model field must be non-empty ─────────────────────

    #[test]
    fn model_model_field_empty() {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = ""
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors
                .iter()
                .any(|e| e.field == "models.fast.model" && e.message.contains("empty")),
            "{errors:?}"
        );
    }

    // ── Rule 12: agent id must be valid identifier ───────────────────────

    #[test]
    fn agent_id_with_spaces_rejected() {
        let agents = single_agent(
            "has spaces",
            "Bad",
            "main",
            vec![],
            vec![],
            "prompt here for testing",
        );
        let errors = validate_config(&valid_config(), &agents);
        assert!(
            errors
                .iter()
                .any(|e| e.field == "agents.has spaces" && e.message.contains("invalid")),
            "{errors:?}"
        );
    }

    #[test]
    fn agent_id_with_special_chars_rejected() {
        let agents = single_agent(
            "bad@id!",
            "Bad",
            "main",
            vec![],
            vec![],
            "prompt here for testing",
        );
        let errors = validate_config(&valid_config(), &agents);
        assert!(
            errors
                .iter()
                .any(|e| e.field == "agents.bad@id!" && e.message.contains("invalid")),
            "{errors:?}"
        );
    }

    #[test]
    fn agent_id_with_hyphen_and_underscore_accepted() {
        let agents = single_agent(
            "my-agent_v2",
            "Good",
            "main",
            vec!["read_file"],
            vec![],
            "prompt here for testing",
        );
        let errors = validate_config(&valid_config(), &agents);
        assert!(
            errors.is_empty(),
            "hyphens and underscores are valid: {errors:?}"
        );
    }

    // ── Rule 13: circular children detection ────────────────────────────

    #[test]
    fn circular_children_detected() {
        let agents = AgentsConfig {
            agents: vec![
                AgentConfig {
                    id: AgentId("root".into()),
                    name: "Root".into(),
                    model: "main".into(),
                    tools: vec!["read_file".into()],
                    children: vec![AgentId("loop_a".into())],
                    default_prompt: "prompt here for testing".into(),
                },
                AgentConfig {
                    id: AgentId("loop_a".into()),
                    name: "LoopA".into(),
                    model: "fast".into(),
                    tools: vec![],
                    children: vec![AgentId("loop_b".into())],
                    default_prompt: "prompt here for testing".into(),
                },
                AgentConfig {
                    id: AgentId("loop_b".into()),
                    name: "LoopB".into(),
                    model: "fast".into(),
                    tools: vec![],
                    children: vec![AgentId("loop_a".into())],
                    default_prompt: "prompt here for testing".into(),
                },
            ],
        };
        let errors = validate_config(&valid_config(), &agents);
        assert!(
            errors.iter().any(|e| e.message.contains("Circular")),
            "should detect circular reference: {errors:?}"
        );
    }

    // ── Rule 14: database path must be valid ─────────────────────────────

    #[test]
    fn database_path_empty_string_rejected() {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"

[database]
path = ""
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors
                .iter()
                .any(|e| e.field == "database.path" && e.message.contains("empty")),
            "{errors:?}"
        );
    }

    // ── validate_config_full tests ───────────────────────────────────────

    #[test]
    fn full_valid_config_has_no_errors_or_warnings() {
        let result = validate_config_full(&valid_config(), &valid_agents());
        assert!(
            result.errors.is_empty(),
            "no errors expected: {:?}",
            result.errors
        );
        assert!(
            result.warnings.is_empty(),
            "no warnings expected: {:?}",
            result.warnings
        );
        assert!(result.is_valid());
    }

    #[test]
    fn full_validation_catches_errors_and_warnings() {
        let mut config = valid_config();
        // Add an unused provider
        config.providers.insert(
            "orphan".into(),
            crate::config::ProviderConfig {
                base_url: "https://orphan.example.com".into(),
                api_key_env: "ORPHAN_KEY".into(),
                adapter: None,
                title: None,
                max_attempts: 3,
                retry_base_ms: 500,
            },
        );
        let result = validate_config_full(&config, &valid_agents());
        assert!(
            result.is_valid(),
            "unused provider is a warning, not an error"
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.field == "providers.orphan" && w.message.contains("not referenced")),
            "{:?}",
            result.warnings
        );
    }

    #[test]
    fn full_validation_warns_agent_no_tools() {
        let agents = single_agent(
            "root",
            "Root",
            "main",
            vec![],
            vec![],
            "this is a reasonably long prompt",
        );
        let result = validate_config_full(&valid_config(), &agents);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.field == "agents.root.tools" && w.message.contains("no tools")),
            "{:?}",
            result.warnings
        );
    }

    #[test]
    fn full_validation_warns_short_default_prompt() {
        let agents = single_agent("root", "Root", "main", vec!["read_file"], vec![], "hi");
        let result = validate_config_full(&valid_config(), &agents);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.field == "agents.root.default_prompt" && w.message.contains("short")),
            "{:?}",
            result.warnings
        );
    }

    #[test]
    fn full_validation_unused_model_warning() {
        let mut config = valid_config();
        config.models.insert(
            "extra".into(),
            crate::config::ModelConfig {
                provider: "zhipu".into(),
                model: "extra-model".into(),
                max_context_tokens: None,
                max_output_tokens: None,
                supports_tool_call: true,
                supports_vision: false,
                supports_reasoning: false,
                input_price_per_mtok: None,
                output_price_per_mtok: None,
            },
        );
        let result = validate_config_full(&config, &valid_agents());
        assert!(result.is_valid());
        assert!(
            result.warnings.iter().any(|w| w.field == "models.extra"),
            "{:?}",
            result.warnings
        );
    }

    // ── ValidationResult::into_findings tests ────────────────────────────

    #[test]
    fn validation_result_into_findings_combines() {
        let result = ValidationResult {
            errors: vec![ValidationError {
                field: "test".into(),
                message: "err".into(),
            }],
            warnings: vec![ValidationError {
                field: "test2".into(),
                message: "warn".into(),
            }],
        };
        let findings = result.into_findings();
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].severity, Severity::Error);
        assert_eq!(findings[1].severity, Severity::Warning);
    }

    #[test]
    fn validation_result_is_valid() {
        let result = ValidationResult {
            errors: vec![],
            warnings: vec![],
        };
        assert!(result.is_valid());

        let result_with_error = ValidationResult {
            errors: vec![ValidationError {
                field: "x".into(),
                message: "bad".into(),
            }],
            warnings: vec![],
        };
        assert!(!result_with_error.is_valid());
    }

    // ── Rule 16: [ptc] numeric floors ──────────────────────────────────

    #[test]
    fn ptc_zero_values_are_validation_errors() {
        let mut config = valid_config();
        config.ptc.timeout_ms = 0;
        config.ptc.max_output_bytes = 0;
        config.ptc.max_tool_calls_per_run = 0;
        config.ptc.max_lookup_calls = 0;
        config.ptc.memory_limit_bytes = 0;
        let errors = validate_config(&config, &valid_agents());
        for field in [
            "ptc.timeout_ms",
            "ptc.max_output_bytes",
            "ptc.max_tool_calls_per_run",
            "ptc.max_lookup_calls",
            "ptc.memory_limit_bytes",
        ] {
            assert!(
                errors.iter().any(|e| e.field == field),
                "expected error for {field}: {errors:?}"
            );
        }
    }

    #[test]
    fn ptc_floor_boundary_values_pass_validation() {
        let mut config = valid_config();
        config.ptc.enabled = true;
        config.ptc.timeout_ms = 100;
        config.ptc.memory_limit_bytes = 1_048_576;
        config.ptc.max_output_bytes = 1024;
        config.ptc.max_tool_calls_per_run = 1;
        config.ptc.max_lookup_calls = 1;
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors.iter().all(|e| !e.field.starts_with("ptc.")),
            "floor-boundary values must pass: {errors:?}"
        );
    }

    #[test]
    fn ptc_sub_floor_memory_limit_error_names_the_floor() {
        let mut config = valid_config();
        config.ptc.memory_limit_bytes = 1024;
        let errors = validate_config(&config, &valid_agents());
        let err = errors
            .iter()
            .find(|e| e.field == "ptc.memory_limit_bytes")
            .expect("memory floor error");
        assert!(err.message.contains("1048576"), "message: {}", err.message);
    }

    // ── Rule 15: MCP server transport validation ────────────────────────

    #[test]
    fn mcp_stdio_server_parses() {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"

[mcp.servers.fs]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let mcp = config.mcp.as_ref().expect("mcp section present");
        let server = mcp.servers.get("fs").expect("fs server present");
        assert!(server.enabled, "enabled defaults to true");
        match &server.transport {
            TransportConfig::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args.len(), 3);
                assert!(env.is_none());
            }
            TransportConfig::Http { .. } => panic!("expected stdio"),
        }
        let errors = validate_config(&config, &valid_agents());
        assert!(errors.is_empty(), "valid stdio server: {errors:?}");
    }

    #[test]
    fn mcp_http_server_disabled_parses() {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"

[mcp.servers.remote]
enabled = false
transport = "http"
url = "http://localhost:8000/mcp"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let server = config.mcp.as_ref().unwrap().servers.get("remote").unwrap();
        assert!(!server.enabled, "enabled can be overridden to false");
        match &server.transport {
            TransportConfig::Http { url, .. } => assert_eq!(url, "http://localhost:8000/mcp"),
            TransportConfig::Stdio { .. } => panic!("expected http"),
        }
        // A disabled server is still statically valid (connection is skipped later).
        let errors = validate_config(&config, &valid_agents());
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn mcp_stdio_with_env_parses() {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"

[mcp.servers.git]
transport = "stdio"
command = "uvx"
args = ["mcp-server-git"]

[mcp.servers.git.env]
GIT_AUTHOR_NAME = "openslate"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let server = config.mcp.unwrap().servers.into_values().next().unwrap();
        match server.transport {
            TransportConfig::Stdio { env, .. } => {
                let env = env.expect("env present");
                assert_eq!(env.get("GIT_AUTHOR_NAME").unwrap(), "openslate");
            }
            TransportConfig::Http { .. } => panic!("expected stdio"),
        }
    }

    #[test]
    fn mcp_stdio_empty_command_rejected() {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"

[mcp.servers.bad]
transport = "stdio"
command = ""
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors
                .iter()
                .any(|e| e.field == "mcp.servers.bad.command" && e.message.contains("empty")),
            "{errors:?}"
        );
    }

    #[test]
    fn mcp_http_invalid_url_rejected() {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"

[mcp.servers.bad]
transport = "http"
url = "not-a-url"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors
                .iter()
                .any(|e| e.field == "mcp.servers.bad.url" && e.message.contains("invalid")),
            "{errors:?}"
        );
    }

    #[test]
    fn mcp_unknown_transport_rejected_by_serde() {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"

[mcp.servers.weird]
transport = "carrier-pigeon"
"#;
        // Unknown transport variant → serde parse error (fail fast at load time).
        assert!(parse_openslate_toml(toml).is_err());
    }

    // ── Warning: agents referencing disabled builtin tools ──────────────

    #[test]
    fn warns_when_agent_whitelists_disabled_builtin_tool() {
        let mut config = valid_config();
        config.builtin_tools.read_file = false;
        let result = validate_config_full(&config, &valid_agents());
        assert!(
            result.is_valid(),
            "disabled builtins are a warning, not an error"
        );
        // root whitelists read_file (disabled); worker whitelists write_file (still on).
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.field == "agents.root.tools" && w.message.contains("'read_file'")),
            "{:?}",
            result.warnings
        );
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.field == "agents.worker.tools" && w.message.contains("builtin")),
            "worker's write_file is still enabled: {:?}",
            result.warnings
        );
    }

    #[test]
    fn warns_when_builtins_fully_disabled_and_referenced() {
        let mut config = valid_config();
        config.builtin_tools.enabled = false;
        let (errors, warnings) = validate_strict(&config, &valid_agents());
        assert!(errors.is_empty(), "{errors:?}");
        // Both root (read_file) and worker (write_file) reference disabled builtins.
        assert!(
            warnings
                .iter()
                .any(|w| w.field == "agents.root.tools" && w.message.contains("[builtin_tools]")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.field == "agents.worker.tools" && w.message.contains("[builtin_tools]")),
            "{warnings:?}"
        );
    }

    #[test]
    fn no_builtin_tool_warnings_when_all_enabled() {
        let result = validate_config_full(&valid_config(), &valid_agents());
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.message.contains("[builtin_tools]")),
            "everything enabled → no builtin warnings: {:?}",
            result.warnings
        );
    }

    #[test]
    fn glob_tool_patterns_never_trigger_builtin_warnings() {
        // A glob like "read_*" also matches external MCP tools, so it must not
        // warn even when the builtin read_file is disabled. Guards the exact
        // `t == name` comparison — a glob-aware match would make `--strict`
        // exit 1 on legitimate configs.
        let agents = single_agent(
            "root",
            "Root",
            "main",
            vec!["read_*"],
            vec![],
            "prompt long enough",
        );
        let mut config = valid_config();
        config.builtin_tools.read_file = false;

        let result = validate_config_full(&config, &agents);
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.message.contains("[builtin_tools]")),
            "glob patterns must not warn: {:?}",
            result.warnings
        );
        let (_errors, warnings) = validate_strict(&config, &agents);
        assert!(
            !warnings
                .iter()
                .any(|w| w.message.contains("[builtin_tools]")),
            "glob patterns must not warn (strict): {:?}",
            warnings
        );
    }

    #[test]
    fn master_switch_off_warns_even_with_tool_flag_on() {
        // Contradictory combo: enabled=false but read_file=true. The master
        // switch wins, so a whitelisted read_file is still unavailable and
        // must warn — locks the `bt.enabled && bt.flag` conjunction.
        let agents = single_agent(
            "root",
            "Root",
            "main",
            vec!["read_file"],
            vec![],
            "prompt long enough",
        );
        let mut config = valid_config();
        config.builtin_tools.enabled = false;
        config.builtin_tools.read_file = true;

        let result = validate_config_full(&config, &agents);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.field == "agents.root.tools"
                    && w.message.contains("'read_file'")
                    && w.message.contains("[builtin_tools]")),
            "master switch off must warn regardless of per-tool flag: {:?}",
            result.warnings
        );
    }

    // ── Warning: [approval] drift ─────────────────────────────────────────

    #[test]
    fn warns_when_auto_except_has_empty_tools() {
        let mut config = valid_config();
        config.approval = Some(crate::config::ApprovalConfig {
            policy: ApprovalPolicySetting::AutoExcept,
            tools: vec![],
        });
        let (_errors, warnings) = validate_strict(&config, &valid_agents());
        assert!(
            warnings
                .iter()
                .any(|w| w.field == "approval.tools" && w.message.contains("empty")),
            "{warnings:?}"
        );
    }

    #[test]
    fn warns_when_approval_tools_reference_unknown_tool() {
        let mut config = valid_config();
        config.approval = Some(crate::config::ApprovalConfig {
            policy: ApprovalPolicySetting::AutoExcept,
            tools: vec!["teleport".to_owned()],
        });
        let result = validate_config_full(&config, &valid_agents());
        assert!(
            result.is_valid(),
            "unknown approval tools are a warning, not an error"
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.field == "approval.tools" && w.message.contains("'teleport'")),
            "{:?}",
            result.warnings
        );
    }

    #[test]
    fn no_approval_warnings_for_known_tool_names() {
        // `shell` / `run_code` are known specials; `github_list_prs` appears
        // in an agent whitelist (the external-MCP stand-in).
        let agents = single_agent(
            "root",
            "Root",
            "main",
            vec!["github_list_prs"],
            vec![],
            "prompt long enough",
        );
        let mut config = valid_config();
        config.approval = Some(crate::config::ApprovalConfig {
            policy: ApprovalPolicySetting::AutoExcept,
            tools: vec![
                "shell".to_owned(),
                "run_code".to_owned(),
                "github_list_prs".to_owned(),
            ],
        });
        let result = validate_config_full(&config, &agents);
        assert!(
            !result.warnings.iter().any(|w| w.field == "approval.tools"),
            "known tool names must not warn: {:?}",
            result.warnings
        );
    }

    #[test]
    fn no_approval_warnings_when_section_absent() {
        let config = valid_config();
        assert!(config.approval.is_none());
        let result = validate_config_full(&config, &valid_agents());
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.field.starts_with("approval.")),
            "absent [approval] must not warn: {:?}",
            result.warnings
        );
    }
}
