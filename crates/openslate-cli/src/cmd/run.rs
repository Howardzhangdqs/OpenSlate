//! `openslate run` command — execute a single agent run.
//!
//! Loads config, validates, creates RunManager, executes with provider,
//! and prints result to stdout.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use openslate_core::agent_tree::AgentTree;
use openslate_core::config::validation::validate_config;
use openslate_core::config::{parse_agents_yaml, parse_openslate_toml, AgentsConfig, OpenSlateConfig};
use openslate_core::model_config::resolve_model;
use openslate_core::run_manager::RunManager;
use openslate_core::tool::builtin_registry;
use openslate_model_openai::client::{OpenAICompatibleProvider, OpenAIProviderConfig};

/// Output format for run results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "text" => Ok(OutputFormat::Text),
            _ => Err(format!(
                "unsupported format '{}'; supported: text",
                s
            )),
        }
    }
}

/// Parameters for the run command.
#[allow(dead_code)] // agent and profile used by Task 28 wiring
pub struct RunParams {
    pub config_path: Option<String>,
    pub agent: Option<String>,
    pub prompt: Option<String>,
    pub profile: String,
    pub format: OutputFormat,
    pub output: Option<String>,
    pub root_agent: Option<String>,
    pub quiet: bool,
}

/// Load and parse `openslate.toml` from the given path.
fn load_config(config_path: &Path) -> Result<OpenSlateConfig> {
    let content = fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read config file '{}'", config_path.display()))?;
    parse_openslate_toml(&content)
        .with_context(|| format!("Failed to parse config file '{}'", config_path.display()))
}

/// Load and parse `agents.yaml` from the given path.
fn load_agents(agents_path: &Path) -> Result<AgentsConfig> {
    let content = fs::read_to_string(agents_path)
        .with_context(|| format!("Failed to read agents file '{}'", agents_path.display()))?;
    parse_agents_yaml(&content)
        .with_context(|| format!("Failed to parse agents file '{}'", agents_path.display()))
}

/// Resolve config file path from CLI `--config` flag or default XDG resolution.
fn resolve_config_file(config_flag: Option<&str>) -> Result<std::path::PathBuf> {
    if let Some(flag) = config_flag {
        let path = Path::new(flag);
        if !path.exists() {
            anyhow::bail!("Config file not found: {}", path.display());
        }
        return Ok(path.to_path_buf());
    }

    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let paths = openslate_core::paths::resolve_paths(&cwd);
    if !paths.config_file.exists() {
        anyhow::bail!(
            "No openslate.toml found. Expected at {}. Run `openslate init` to create one.",
            paths.config_file.display()
        );
    }
    Ok(paths.config_file)
}

/// Resolve agents.yaml path from the config file's parent directory.
fn resolve_agents_file(config_path: &Path) -> std::path::PathBuf {
    config_path
        .parent()
        .map(|p| p.join("agents.yaml"))
        .unwrap_or_else(|| Path::new("agents.yaml").to_path_buf())
}

/// Build an OpenAI-compatible provider from the root agent's model config.
fn build_provider(config: &OpenSlateConfig, model_alias: &str) -> Result<OpenAICompatibleProvider> {
    let resolved = resolve_model(config, model_alias).with_context(|| {
        format!("Failed to resolve model alias '{}'", model_alias)
    })?;

    let api_key = std::env::var(&resolved.provider.api_key_env).with_context(|| {
        format!(
            "API key not found: set environment variable '{}'",
            resolved.provider.api_key_env
        )
    })?;

    let provider_config = OpenAIProviderConfig {
        provider_name: resolved.provider_name,
        base_url: resolved.provider.base_url,
        api_key,
        timeout_secs: 60,
    };

    Ok(OpenAICompatibleProvider::new(provider_config))
}

/// Extract the final assistant message from the run result.
fn extract_final_assistant_message(messages: &[openslate_core::types::Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, openslate_core::types::MessageRole::Assistant))
        .map(|m| m.content.clone())
}

/// Format and write the result to output.
fn write_result(
    content: &str,
    result: &openslate_core::run_manager::ManagedRunResult,
    format: &OutputFormat,
    output_path: Option<&str>,
    quiet: bool,
) -> Result<()> {
    let output = match format {
        OutputFormat::Text => content.to_owned(),
    };

    if let Some(path) = output_path {
        fs::write(path, &output)
            .with_context(|| format!("Failed to write output to '{}'", path))?;
        if !quiet {
            tracing::info!("Result written to {}", path);
        }
    } else {
        println!("{}", output);
    }

    if !quiet {
        tracing::info!(
            "Run completed: id={}, steps={}, input_tokens={}, output_tokens={}",
            result.run_id,
            result.total_steps,
            result.total_input_tokens,
            result.total_output_tokens,
        );
    }

    Ok(())
}

/// Run the `openslate run` command.
pub async fn run_run_command(params: RunParams) -> Result<()> {
    // 1. Resolve config path
    let config_path = resolve_config_file(params.config_path.as_deref())?;
    tracing::debug!("Using config: {}", config_path.display());

    let agents_path = resolve_agents_file(&config_path);
    tracing::debug!("Using agents: {}", agents_path.display());

    // 2. Load config
    let config = load_config(&config_path)?;

    // 3. Load agents
    let agents = load_agents(&agents_path)?;

    // 4. Validate config
    let errors = validate_config(&config, &agents);
    if !errors.is_empty() {
        for err in &errors {
            tracing::error!("Validation error: {} — {}", err.field, err.message);
        }
        anyhow::bail!(
            "Configuration validation failed with {} error(s)",
            errors.len()
        );
    }

    // 5. Build agent tree
    let agent_tree = AgentTree::from_configs(&agents.agents)
        .map_err(|e| anyhow::anyhow!("Failed to build agent tree: {}", e))?;

    // 6. Determine which agent to run (before moving agent_tree)
    let root_agent = if let Some(agent_id) = &params.root_agent {
        agent_tree
            .get_agent(&openslate_core::types::AgentId(agent_id.clone()))
            .ok_or_else(|| anyhow::anyhow!("Agent '{}' not found in configuration", agent_id))?
    } else {
        agent_tree.get_root()
    };

    let model_alias = root_agent.model_alias.clone();
    tracing::debug!("Running agent '{}' with model '{}'", root_agent.id, model_alias);

    // 7. Build tool registry
    let registry = builtin_registry();

    // 8. Build provider (before moving config into RunManager)
    let provider = build_provider(&config, &model_alias)?;

    // 9. Create RunManager
    let manager = RunManager::new(config, agent_tree, registry);

    // 10. Get prompt
    let prompt = params
        .prompt
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("No prompt provided. Use --prompt <text> to specify input."))?;

    // 11. Execute
    let result = manager
        .execute(&provider, prompt)
        .await
        .map_err(|e| anyhow::anyhow!("Run failed: {}", e))?;

    // 12. Extract final assistant message
    let final_message = extract_final_assistant_message(&result.messages)
        .unwrap_or_else(|| "(no assistant response)".to_owned());

    // 13. Write result
    write_result(&final_message, &result, &params.format, params.output.as_deref(), params.quiet)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create a temp dir with valid config + agents for testing.
    fn temp_project() -> TempDir {
        let tmp = TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
kind = "openai_compatible"
base_url = "https://example.com"
api_key_env = "TEST_API_KEY"

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
        let agents = r#"
agents:
  - id: root
    name: Root Agent
    model: main
    tools:
      - current_time
    default_prompt: "You are the root agent."
"#;
        fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        fs::write(openslate_dir.join("agents.yaml"), agents).expect("write agents.yaml");
        tmp
    }

    #[test]
    fn test_output_format_parse_text() {
        assert_eq!("text".parse::<OutputFormat>(), Ok(OutputFormat::Text));
    }

    #[test]
    fn test_output_format_parse_unsupported() {
        assert!("jsonl".parse::<OutputFormat>().is_err());
        assert!("csv".parse::<OutputFormat>().is_err());
    }

    #[test]
    fn test_load_config_valid() {
        let tmp = temp_project();
        let path = tmp.path().join(".openslate/openslate.toml");
        let config = load_config(&path).expect("should load");
        assert!(config.models.contains_key("main"));
    }

    #[test]
    fn test_load_config_missing_file() {
        let result = load_config(Path::new("/nonexistent/openslate.toml"));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Failed to read") || msg.contains("Failed to parse"));
    }

    #[test]
    fn test_load_agents_valid() {
        let tmp = temp_project();
        let path = tmp.path().join(".openslate/agents.yaml");
        let agents = load_agents(&path).expect("should load");
        assert_eq!(agents.agents.len(), 1);
        assert_eq!(agents.agents[0].id.0, "root");
    }

    #[test]
    fn test_load_agents_missing_file() {
        let result = load_agents(Path::new("/nonexistent/agents.yaml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_config_file_explicit() {
        let tmp = temp_project();
        let path = tmp.path().join(".openslate/openslate.toml");
        let resolved = resolve_config_file(Some(path.to_str().unwrap())).expect("should resolve");
        assert_eq!(resolved, path);
    }

    #[test]
    fn test_resolve_config_file_missing_explicit() {
        let result = resolve_config_file(Some("/nonexistent/openslate.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_agents_file() {
        let path = Path::new("/project/.openslate/openslate.toml");
        let agents = resolve_agents_file(path);
        assert_eq!(agents, Path::new("/project/.openslate/agents.yaml"));
    }

    #[test]
    fn test_extract_final_assistant_message() {
        use openslate_core::types::{Message, MessageRole};

        let messages = vec![
            Message {
                role: MessageRole::User,
                content: "hello".into(),
                tool_call_id: None,
                name: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "response".into(),
                tool_call_id: None,
                name: None,
            },
        ];
        assert_eq!(
            extract_final_assistant_message(&messages),
            Some("response".to_owned())
        );
    }

    #[test]
    fn test_extract_final_assistant_message_none() {
        use openslate_core::types::{Message, MessageRole};

        let messages = vec![Message {
            role: MessageRole::User,
            content: "hello".into(),
            tool_call_id: None,
            name: None,
        }];
        assert_eq!(extract_final_assistant_message(&messages), None);
    }

    #[test]
    fn test_build_provider_missing_env_var() {
        let tmp = temp_project();
        let config = load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        match build_provider(&config, "main") {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("TEST_API_KEY"),
                    "error should mention env var name: {msg}"
                );
            }
            Ok(_) => panic!("expected error when env var is not set"),
        }
    }

    #[test]
    fn test_validation_with_valid_config() {
        let tmp = temp_project();
        let config = load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = load_agents(&tmp.path().join(".openslate/agents.yaml")).unwrap();
        let errors = validate_config(&config, &agents);
        assert!(errors.is_empty(), "valid config should have no errors: {errors:?}");
    }

    #[test]
    fn test_validation_with_invalid_model_ref() {
        let tmp = TempDir::new().expect("create temp dir");
        let dir = tmp.path().join(".openslate");
        fs::create_dir(&dir).expect("create dir");

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
        let agents = r#"
agents:
  - id: root
    name: Root
    model: nonexistent_model
    default_prompt: "test"
"#;
        fs::write(dir.join("openslate.toml"), toml).expect("write");
        fs::write(dir.join("agents.yaml"), agents).expect("write");

        let config = load_config(&dir.join("openslate.toml")).unwrap();
        let agents_cfg = load_agents(&dir.join("agents.yaml")).unwrap();
        let errors = validate_config(&config, &agents_cfg);
        assert!(!errors.is_empty(), "should have validation errors");
    }
}
