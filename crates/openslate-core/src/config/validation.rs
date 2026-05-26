//! Configuration validation for OpenSlate.
//!
//! Validates that `openslate.toml` + `agents.yaml` form a consistent,
//! complete configuration. Returns structured errors (and optional warnings)
//! instead of panicking.

use std::collections::HashSet;

use crate::config::{AgentsConfig, OpenSlateConfig};

/// A single validation finding (error or warning).
#[derive(Debug, Clone)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

/// Validate the complete configuration (`openslate.toml` + `agents.yaml`).
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

    // 8. Limits must have reasonable values (> 0)
    if let Some(limits) = &config.limits {
        if limits.max_steps == 0 {
            errors.push(ValidationError {
                field: "limits.max_steps".into(),
                message: "max_steps must be > 0".into(),
            });
        }
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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{parse_agents_yaml, parse_openslate_toml};

    // ── Helpers ───────────────────────────────────────────────────────────

    /// Build a minimal valid config for mutation in tests.
    fn valid_config() -> OpenSlateConfig {
        let toml = r#"
[providers.zhipu]
kind = "openai_compatible"
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
        let yaml = r#"
agents:
  - id: root
    name: Root
    model: main
    children:
      - worker
    default_prompt: "root prompt"
  - id: worker
    name: Worker
    model: fast
    default_prompt: "worker prompt"
"#;
        parse_agents_yaml(yaml).expect("fixture should parse")
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
        let yaml = r#"
agents:
  - id: root
    name: Root
    model: main
    default_prompt: "p1"
  - id: root
    name: Root Dupe
    model: fast
    default_prompt: "p2"
"#;
        let agents = parse_agents_yaml(yaml).expect("should parse");
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
        let yaml = r#"
agents:
  - id: a
    name: A
    model: main
    children:
      - b
    default_prompt: "p"
  - id: b
    name: B
    model: fast
    children:
      - a
    default_prompt: "p"
"#;
        let agents = parse_agents_yaml(yaml).expect("should parse");
        let errors = validate_config(&valid_config(), &agents);
        assert!(
            errors.iter().any(|e| e.field == "agents" && e.message.contains("root")),
            "{errors:?}"
        );
    }

    // ── Rule 6: child references must exist ──────────────────────────────

    #[test]
    fn child_references_nonexistent_agent() {
        let yaml = r#"
agents:
  - id: root
    name: Root
    model: main
    children:
      - phantom
    default_prompt: "p"
"#;
        let agents = parse_agents_yaml(yaml).expect("should parse");
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
        let yaml = r#"
agents:
  - id: root
    name: Root
    model: nonexistent
    default_prompt: "p"
"#;
        let agents = parse_agents_yaml(yaml).expect("should parse");
        let errors = validate_config(&valid_config(), &agents);
        assert!(
            errors
                .iter()
                .any(|e| e.field == "agents.root.model" && e.message.contains("nonexistent")),
            "{errors:?}"
        );
    }

    // ── Rule 8: limits must be > 0 ───────────────────────────────────────

    #[test]
    fn limits_max_steps_zero() {
        let mut config = valid_config();
        if let Some(l) = config.limits.as_mut() {
            l.max_steps = 0;
        }
        let errors = validate_config(&config, &valid_agents());
        assert!(
            errors.iter().any(|e| e.field == "limits.max_steps"),
            "{errors:?}"
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
        let config = parse_openslate_toml(include_str!(
            "../../../../../openslate-project-plan/plan/examples/openslate.toml"
        ))
        .expect("example toml should parse");
        let agents = parse_agents_yaml(include_str!(
            "../../../../../openslate-project-plan/plan/examples/agents.yaml"
        ))
        .expect("example yaml should parse");

        let errors = validate_config(&config, &agents);
        assert!(errors.is_empty(), "example config should be valid: {errors:?}");
    }

    // ── No limits section is fine (limits is optional) ───────────────────

    #[test]
    fn no_limits_section_is_valid() {
        let toml = r#"
[providers.zhipu]
kind = "openai_compatible"
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
}
