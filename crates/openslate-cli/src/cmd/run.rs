//! `openslate run` command — execute a single agent run.
//!
//! Uses the `AppContext` wiring to assemble all components and execute
//! a complete end-to-end run: config → validation → SQLite store → agent tree →
//! tool registry → provider → RunManager → execute → result output.

use anyhow::{Context, Result};
use std::fs;

use openslate_core::agent_tree::AgentTree;
use openslate_model_openai::client::{OpenAICompatibleProvider, OpenAIProviderConfig};

use crate::input::{expand_at_files, read_stdin_if_pipe, WorkspaceRoot};
use crate::spinner::Spinner;
use crate::wiring as app_wiring;

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
#[allow(dead_code)]
pub struct RunParams {
    pub config_path: Option<String>,
    pub agent: Option<String>,
    pub prompt: Option<String>,
    pub profile: String,
    pub format: OutputFormat,
    pub output: Option<String>,
    #[allow(dead_code)]
    pub root_agent: Option<String>,
    pub quiet: bool,
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

/// Resolve a specific agent from the agent tree.
///
/// If `agent_id` is provided, look it up. Otherwise return the root agent.
pub(crate) fn resolve_agent<'a>(
    agent_tree: &'a AgentTree,
    agent_id: Option<&str>,
) -> Result<&'a openslate_core::agent_tree::AgentNode> {
    if let Some(id) = agent_id {
        agent_tree
            .get_agent(&openslate_core::types::AgentId(id.to_owned()))
            .ok_or_else(|| anyhow::anyhow!("Agent '{}' not found in configuration", id))
    } else {
        Ok(agent_tree.get_root())
    }
}

/// Run the `openslate run` command.
pub async fn run_run_command(params: RunParams) -> Result<()> {
    // 1. Build the full app context (config → validation → store → agent tree → registry)
    let ctx = app_wiring::build_app_context(params.config_path.as_deref()).await?;

    // 2. Resolve which agent to run (--agent flag or root)
    let agent = resolve_agent(&ctx.agent_tree, params.agent.as_deref())?;
    let model_alias = agent.model_alias.clone();
    tracing::info!(
        "Running agent '{}' (model='{}') with profile '{}'",
        agent.id,
        model_alias,
        params.profile
    );

    // 3. Build provider for the resolved agent's model
    let provider = build_provider_for_model(&ctx.config, &model_alias)?;

    let raw_prompt = if let Some(ref p) = params.prompt {
        p.clone()
    } else if let Some(stdin_content) = read_stdin_if_pipe() {
        stdin_content
    } else {
        anyhow::bail!(
            "No prompt provided. Use --prompt <text> or pipe input via stdin: echo 'hello' | openslate run"
        )
    };

    let workspace_root = WorkspaceRoot::from_config_path(&ctx.config_path);
    let prompt = expand_at_files(&raw_prompt, &workspace_root);

    // 5. Log store status
    if let Some(ref _store) = ctx.store {
        tracing::debug!("SQLite store initialized for this run");
    }

    // 6. Execute via RunManager
    let result = {
        let mut spinner = Spinner::new(&model_alias, params.quiet);
        let run_result = match ctx
            .manager
            .execute(&provider, &prompt)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                spinner.finish_with_error(&e.to_string());
                return Err(anyhow::anyhow!("Run failed: {}", e));
            }
        };
        if run_result.total_output_tokens > 0 {
            spinner.on_usage(run_result.total_output_tokens as u32);
        }
        spinner.finish();
        run_result
    };

    // 7. Extract final assistant message
    let final_message = extract_final_assistant_message(&result.messages)
        .unwrap_or_else(|| "(no assistant response)".to_owned());

    // 8. Write result
    write_result(
        &final_message,
        &result,
        &params.format,
        params.output.as_deref(),
        params.quiet,
    )?;

    Ok(())
}

/// Build an OpenAI-compatible provider for a specific model alias.
pub(crate) fn build_provider_for_model(
    config: &openslate_core::config::OpenSlateConfig,
    model_alias: &str,
) -> Result<OpenAICompatibleProvider> {
    let resolved =
        openslate_core::model_config::resolve_model(config, model_alias).with_context(|| {
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
    fn test_build_provider_for_model_missing_env_var() {
        let tmp = temp_project();
        let config =
            crate::wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        match build_provider_for_model(&config, "main") {
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
    fn test_resolve_agent_root_default() {
        let tmp = temp_project();
        let agents =
            crate::wiring::load_agents(&tmp.path().join(".openslate/agents.yaml")).unwrap();
        let tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let agent = resolve_agent(&tree, None).unwrap();
        assert_eq!(agent.id.0, "root");
    }

    #[test]
    fn test_resolve_agent_explicit_id() {
        let tmp = TempDir::new().expect("create temp dir");
        let dir = tmp.path().join(".openslate");
        fs::create_dir(&dir).expect("create dir");

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
"#;
        let agents = r#"
agents:
  - id: root
    name: Root
    model: main
    children:
      - worker
    default_prompt: "Root."
  - id: worker
    name: Worker
    model: fast
    default_prompt: "Worker."
"#;
        fs::write(dir.join("openslate.toml"), toml).expect("write");
        fs::write(dir.join("agents.yaml"), agents).expect("write");

        let agents_cfg =
            crate::wiring::load_agents(&dir.join("agents.yaml")).unwrap();
        let tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents_cfg.agents).unwrap();

        let worker = resolve_agent(&tree, Some("worker")).unwrap();
        assert_eq!(worker.id.0, "worker");
        assert_eq!(worker.model_alias, "fast");
    }

    #[test]
    fn test_resolve_agent_not_found() {
        let tmp = temp_project();
        let agents =
            crate::wiring::load_agents(&tmp.path().join(".openslate/agents.yaml")).unwrap();
        let tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let result = resolve_agent(&tree, Some("nonexistent"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }
}
