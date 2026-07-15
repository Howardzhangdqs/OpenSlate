//! Configuration validation for OpenSlate.
//!
//! Validates that `openslate.toml` + `agents/*.md` form a consistent,
//! complete configuration. Returns structured errors (and optional warnings)
//! instead of panicking.

use std::collections::HashSet;

use crate::config::{AgentsConfig, OpenSlateConfig, TransportConfig};

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
pub fn validate_config(
    config: &OpenSlateConfig,
    agents: &AgentsConfig,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // 1. models.main must exist
    if !config.models.contains_key("main") {
        errors.push(ValidationError {
            field: "models.main".into(),
            message: "Required model alias 'main' is missing".into(),
        });
    }

    // 2. models.fast must exist
    if !config.models.contains_key("fast") {
        errors.push(ValidationError {
            field: "models.fast".into(),
            message: "Required model alias 'fast' is missing".into(),
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
    let all_children: HashSet<_> = agents.agents.iter().flat_map(|a| a.children.iter()).collect();
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

    // 7. Agent model references must exist as model aliases
    for agent in &agents.agents {
        if !config.models.contains_key(&agent.model) {
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
                TransportConfig::Http { url } => {
                    if url.trim().is_empty() {
                        errors.push(ValidationError {
                            field: format!("mcp.servers.{name}.url"),
                            message: format!("MCP server '{}' has an empty url", name),
                        });
                    } else if !is_valid_url(url) {
                        errors.push(ValidationError {
                            field: format!("mcp.servers.{name}.url"),
                            message: format!(
                                "MCP server '{}' has an invalid url '{}'",
                                name, url
                            ),
                        });
                    }
                }
            }
        }
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

    (errors, warnings)
}

/// Full validation returning a structured `ValidationResult` with both errors
/// and warnings, suitable for UI display or logging.
///
/// This runs all error-level checks from [`validate_config`] plus the extended
/// warning checks from [`validate_strict`] and additional warning rules.
pub fn validate_config_full(
    config: &OpenSlateConfig,
    agents: &AgentsConfig,
) -> ValidationResult {
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
    let used_providers: HashSet<_> = config
        .models
        .values()
        .map(|m| &m.provider)
        .collect();
    for name in config.providers.keys() {
        if !used_providers.contains(&name) {
            warnings.push(ValidationError {
                field: format!("providers.{name}"),
                message: format!(
                    "Provider '{}' is not referenced by any model",
                    name
                ),
            });
        }
    }

    // ── Warning: agents with no tools ────────────────────────────────────
    for agent in &agents.agents {
        if agent.tools.is_empty() {
            warnings.push(ValidationError {
                field: format!("agents.{}.tools", agent.id),
                message: format!(
                    "Agent '{}' has no tools configured",
                    agent.id
                ),
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

    ValidationResult { errors, warnings }
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Check if a string is a valid HTTP(S) URL.
fn is_valid_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Check if an agent id contains only valid characters.
fn is_valid_agent_id(id: &str) -> bool {
    if id.is_empty() {
        return false;
    }
    id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Detect circular children references using DFS cycle detection.
fn detect_circular_children(agents: &AgentsConfig) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // Build adjacency map: agent_id → children ids
    let adj: std::collections::HashMap<String, Vec<String>> = agents
        .agents
        .iter()
        .map(|a| (a.id.0.clone(), a.children.iter().map(|c| c.0.clone()).collect()))
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
    fn single_agent(id: &str, name: &str, model: &str, tools: Vec<&str>, children: Vec<&str>, prompt: &str) -> AgentsConfig {
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
        assert!(errors.iter().any(|e| e.field == "models.main"), "{errors:?}");
    }

    #[test]
    fn missing_models_fast() {
        let mut config = valid_config();
        config.models.remove("fast");
        let errors = validate_config(&config, &valid_agents());
        assert!(errors.iter().any(|e| e.field == "models.fast"), "{errors:?}");
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
            errors.iter().any(|e| e.field == "agents.root" && e.message.contains("Duplicate")),
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
            errors.iter().any(|e| e.field == "agents" && e.message.contains("root")),
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
        let agents = parse_agents_dir(Path::new(agents_dir))
            .expect("example agents dir should parse");

        let errors = validate_config(&config, &agents);
        assert!(errors.is_empty(), "example config should be valid: {errors:?}");
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
        let agents = single_agent("has spaces", "Bad", "main", vec![], vec![], "prompt here for testing");
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
        let agents = single_agent("bad@id!", "Bad", "main", vec![], vec![], "prompt here for testing");
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
        let agents = single_agent("my-agent_v2", "Good", "main", vec!["read_file"], vec![], "prompt here for testing");
        let errors = validate_config(&valid_config(), &agents);
        assert!(errors.is_empty(), "hyphens and underscores are valid: {errors:?}");
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
        assert!(result.errors.is_empty(), "no errors expected: {:?}", result.errors);
        assert!(result.warnings.is_empty(), "no warnings expected: {:?}", result.warnings);
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
            },
        );
        let result = validate_config_full(&config, &valid_agents());
        assert!(result.is_valid(), "unused provider is a warning, not an error");
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
        let agents = single_agent("root", "Root", "main", vec![], vec![], "this is a reasonably long prompt");
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
            },
        );
        let result = validate_config_full(&config, &valid_agents());
        assert!(result.is_valid());
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.field == "models.extra"),
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

    // ── Rule 15: MCP server transport validation ───────────────────────

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
        let server = config
            .mcp
            .as_ref()
            .unwrap()
            .servers
            .get("remote")
            .unwrap();
        assert!(!server.enabled, "enabled can be overridden to false");
        match &server.transport {
            TransportConfig::Http { url } => assert_eq!(url, "http://localhost:8000/mcp"),
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
}
