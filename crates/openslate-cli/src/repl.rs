//! REPL session — interactive chat loop with rustyline.
//!
//! Provides an interactive read-eval-print loop that:
//! - Displays a welcome message with version, profile, model, and agent info
//! - Reads user input via rustyline (UTF-8/CJK safe)
//! - Dispatches `/`-prefixed lines to slash-command handler (stub for Task 30)
//! - Sends normal text to the agent via RunManager
//! - Handles Ctrl+D as /exit, ignores empty input, supports `//literal` escape
//! - Accumulates conversation history across turns (basic multi-turn)

use anyhow::{Context, Result};
use openslate_core::types::{Message, MessageRole};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use crate::cmd::run::{build_provider_for_model, resolve_agent};
use crate::wiring::AppContext;

const PROMPT: &str = "openslate> ";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Result of dispatching a line of input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchResult {
    /// Continue the REPL loop.
    Continue,
    /// Exit the REPL.
    Exit,
}

/// An interactive REPL session.
pub struct ReplSession {
    ctx: AppContext,
    editor: DefaultEditor,
    profile: String,
    quiet: bool,
    /// Accumulated conversation history across turns.
    history: Vec<Message>,
}

impl ReplSession {
    /// Create a new REPL session.
    pub fn new(ctx: AppContext, profile: String, quiet: bool) -> Result<Self> {
        let editor = DefaultEditor::new().context("Failed to initialize readline editor")?;
        Ok(Self {
            ctx,
            editor,
            profile,
            quiet,
            history: Vec::new(),
        })
    }

    /// Run the REPL loop until the user exits.
    pub async fn run(&mut self) -> Result<()> {
        if !self.quiet {
            let welcome = self.format_welcome();
            println!("{}", welcome);
        }

        loop {
            let readline = self.editor.readline(PROMPT);
            match readline {
                Ok(line) => {
                    self.editor
                        .add_history_entry(line.as_str())
                        .ok(); // ignore history-write errors
                    let result = self.dispatch(&line).await?;
                    if result == DispatchResult::Exit {
                        break;
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    // Ctrl+C — ignore, continue
                    continue;
                }
                Err(ReadlineError::Eof) => {
                    // Ctrl+D — same as /exit
                    if !self.quiet {
                        println!("Goodbye!");
                    }
                    break;
                }
                Err(e) => {
                    return Err(e).context("Readline error");
                }
            }
        }

        Ok(())
    }

    /// Format the welcome message showing version, profile, model, and agents.
    pub fn format_welcome(&self) -> String {
        let root = self.ctx.agent_tree.get_root();
        let model_alias = &root.model_alias;
        let model_id = self.resolve_model_id(model_alias);
        let children: Vec<String> = root.children.iter().map(|c| c.0.clone()).collect();
        let agents_str = if children.is_empty() {
            root.id.0.clone()
        } else {
            format!("{} → [{}]", root.id.0, children.join(", "))
        };

        format!(
            "OpenSlate v{}\nprofile: {} | model: {} ({}) | agents: {}\ntype /help for commands",
            VERSION, self.profile, model_alias, model_id, agents_str
        )
    }

    /// Dispatch a line of input to the appropriate handler.
    async fn dispatch(&mut self, line: &str) -> Result<DispatchResult> {
        let trimmed = line.trim();

        // Empty input → skip
        if trimmed.is_empty() {
            return Ok(DispatchResult::Continue);
        }

        // Handle // prefix → strip one / and treat as normal input
        if trimmed.starts_with("//") {
            let text = &trimmed[1..]; // strip one leading /
            return self.handle_normal_input(text).await;
        }

        // Handle / prefix → slash command
        if trimmed.starts_with('/') {
            return self.handle_slash_command(trimmed);
        }

        // Normal input
        self.handle_normal_input(trimmed).await
    }

    /// Handle a slash command (stub — full implementation in Task 30).
    fn handle_slash_command(&mut self, input: &str) -> Result<DispatchResult> {
        let parts: Vec<&str> = input.splitn(2, char::is_whitespace).collect();
        let command = parts[0];

        match command {
            "/exit" | "/quit" => {
                if !self.quiet {
                    println!("Goodbye!");
                }
                Ok(DispatchResult::Exit)
            }
            "/help" => {
                println!("Available commands:");
                println!("  /exit, /quit  — Exit the REPL");
                println!("  /help         — Show this help message");
                Ok(DispatchResult::Continue)
            }
            _ => {
                println!(
                    "Unknown command: {}. Type /help for commands.",
                    command
                );
                Ok(DispatchResult::Continue)
            }
        }
    }

    /// Handle normal text input by sending to the agent.
    async fn handle_normal_input(&mut self, input: &str) -> Result<DispatchResult> {
        // Add user message to history
        self.history.push(Message {
            role: MessageRole::User,
            content: input.to_owned(),
            tool_call_id: None,
            name: None,
        });

        // Build provider and execute
        let agent = resolve_agent(&self.ctx.agent_tree, None)?;
        let model_alias = agent.model_alias.clone();
        let provider = build_provider_for_model(&self.ctx.config, &model_alias)?;

        let result = self
            .ctx
            .manager
            .execute(&provider, input)
            .await
            .map_err(|e| anyhow::anyhow!("Agent execution failed: {}", e))?;

        // Extract final assistant message
        let final_message = result
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, MessageRole::Assistant))
            .map(|m| m.content.clone())
            .unwrap_or_else(|| "(no assistant response)".to_owned());

        // Add assistant response to history
        self.history.push(Message {
            role: MessageRole::Assistant,
            content: final_message.clone(),
            tool_call_id: None,
            name: None,
        });

        if !self.quiet {
            println!("{}", final_message);
        }

        Ok(DispatchResult::Continue)
    }

    /// Resolve the model ID for a given alias (for display purposes).
    fn resolve_model_id(&self, model_alias: &str) -> String {
        openslate_core::model_config::resolve_model(&self.ctx.config, model_alias)
            .map(|r| r.model_id)
            .unwrap_or_else(|_| model_alias.to_owned())
    }

    /// Get the conversation history (for testing).
    #[cfg(test)]
    pub fn history(&self) -> &[Message] {
        &self.history
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiring;
    use std::fs;

    /// Create a temp project with valid config + agents for testing.
    fn temp_project() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
kind = "openai_compatible"
base_url = "https://example.com"
api_key_env = "TEST_API_KEY"

[models.main]
provider = "zhipu"
model = "test-model-v1"
supports_tool_call = true

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

    fn temp_project_with_children() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
kind = "openai_compatible"
base_url = "https://example.com"
api_key_env = "TEST_API_KEY"

[models.main]
provider = "zhipu"
model = "glm-5.1"

[models.fast]
provider = "zhipu"
model = "fast-model"

[limits]
max_steps = 10
max_depth = 3
max_tool_calls = 20
"#;
        let agents = r#"
agents:
  - id: root
    name: Root Agent
    model: main
    children:
      - researcher
      - writer
    default_prompt: "You are the root agent."
  - id: researcher
    name: Researcher
    model: fast
    default_prompt: "You are a researcher."
  - id: writer
    name: Writer
    model: fast
    default_prompt: "You are a writer."
"#;
        fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        fs::write(openslate_dir.join("agents.yaml"), agents).expect("write agents.yaml");
        tmp
    }

    // ── Welcome message tests ──

    #[test]
    fn test_welcome_contains_version() {
        let tmp = temp_project();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents.yaml")).unwrap();
        let agent_tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let manager = openslate_core::run_manager::RunManager::new(
            config.clone(),
            agent_tree.clone(),
            openslate_core::tool::builtin_registry(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            provider: openslate_model_openai::client::OpenAICompatibleProvider::new(
                openslate_model_openai::client::OpenAIProviderConfig {
                    provider_name: "zhipu".into(),
                    base_url: "https://example.com".into(),
                    api_key: "test-key".into(),
                    timeout_secs: 60,
                },
            ),
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents.yaml"),
        };

        let session = ReplSession::new(ctx, "default".into(), false).unwrap();
        let welcome = session.format_welcome();

        assert!(
            welcome.contains("OpenSlate v0.1.0"),
            "welcome should contain version: {}",
            welcome
        );
    }

    #[test]
    fn test_welcome_contains_profile() {
        let tmp = temp_project();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents.yaml")).unwrap();
        let agent_tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let manager = openslate_core::run_manager::RunManager::new(
            config.clone(),
            agent_tree.clone(),
            openslate_core::tool::builtin_registry(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            provider: openslate_model_openai::client::OpenAICompatibleProvider::new(
                openslate_model_openai::client::OpenAIProviderConfig {
                    provider_name: "zhipu".into(),
                    base_url: "https://example.com".into(),
                    api_key: "test-key".into(),
                    timeout_secs: 60,
                },
            ),
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents.yaml"),
        };

        let session = ReplSession::new(ctx, "custom-profile".into(), false).unwrap();
        let welcome = session.format_welcome();

        assert!(
            welcome.contains("profile: custom-profile"),
            "welcome should contain profile: {}",
            welcome
        );
    }

    #[test]
    fn test_welcome_contains_model() {
        let tmp = temp_project();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents.yaml")).unwrap();
        let agent_tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let manager = openslate_core::run_manager::RunManager::new(
            config.clone(),
            agent_tree.clone(),
            openslate_core::tool::builtin_registry(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            provider: openslate_model_openai::client::OpenAICompatibleProvider::new(
                openslate_model_openai::client::OpenAIProviderConfig {
                    provider_name: "zhipu".into(),
                    base_url: "https://example.com".into(),
                    api_key: "test-key".into(),
                    timeout_secs: 60,
                },
            ),
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents.yaml"),
        };

        let session = ReplSession::new(ctx, "default".into(), false).unwrap();
        let welcome = session.format_welcome();

        assert!(
            welcome.contains("model: main (test-model-v1)"),
            "welcome should contain model alias and id: {}",
            welcome
        );
    }

    #[test]
    fn test_welcome_shows_children_agents() {
        let tmp = temp_project_with_children();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents.yaml")).unwrap();
        let agent_tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let manager = openslate_core::run_manager::RunManager::new(
            config.clone(),
            agent_tree.clone(),
            openslate_core::tool::builtin_registry(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            provider: openslate_model_openai::client::OpenAICompatibleProvider::new(
                openslate_model_openai::client::OpenAIProviderConfig {
                    provider_name: "zhipu".into(),
                    base_url: "https://example.com".into(),
                    api_key: "test-key".into(),
                    timeout_secs: 60,
                },
            ),
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents.yaml"),
        };

        let session = ReplSession::new(ctx, "default".into(), false).unwrap();
        let welcome = session.format_welcome();

        assert!(
            welcome.contains("agents: root → [researcher, writer]"),
            "welcome should show agent tree: {}",
            welcome
        );
    }

    #[test]
    fn test_welcome_shows_single_agent_no_children() {
        let tmp = temp_project();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents.yaml")).unwrap();
        let agent_tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let manager = openslate_core::run_manager::RunManager::new(
            config.clone(),
            agent_tree.clone(),
            openslate_core::tool::builtin_registry(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            provider: openslate_model_openai::client::OpenAICompatibleProvider::new(
                openslate_model_openai::client::OpenAIProviderConfig {
                    provider_name: "zhipu".into(),
                    base_url: "https://example.com".into(),
                    api_key: "test-key".into(),
                    timeout_secs: 60,
                },
            ),
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents.yaml"),
        };

        let session = ReplSession::new(ctx, "default".into(), false).unwrap();
        let welcome = session.format_welcome();

        assert!(
            welcome.contains("agents: root"),
            "welcome should show root agent: {}",
            welcome
        );
        assert!(
            !welcome.contains("→"),
            "single agent should not show arrow: {}",
            welcome
        );
    }

    #[test]
    fn test_welcome_contains_help_hint() {
        let tmp = temp_project();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents.yaml")).unwrap();
        let agent_tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let manager = openslate_core::run_manager::RunManager::new(
            config.clone(),
            agent_tree.clone(),
            openslate_core::tool::builtin_registry(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            provider: openslate_model_openai::client::OpenAICompatibleProvider::new(
                openslate_model_openai::client::OpenAIProviderConfig {
                    provider_name: "zhipu".into(),
                    base_url: "https://example.com".into(),
                    api_key: "test-key".into(),
                    timeout_secs: 60,
                },
            ),
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents.yaml"),
        };

        let session = ReplSession::new(ctx, "default".into(), false).unwrap();
        let welcome = session.format_welcome();

        assert!(
            welcome.contains("type /help for commands"),
            "welcome should contain help hint: {}",
            welcome
        );
    }

    // ── Slash command dispatch tests ──

    #[test]
    fn test_slash_exit_returns_exit() {
        let tmp = temp_project();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents.yaml")).unwrap();
        let agent_tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let manager = openslate_core::run_manager::RunManager::new(
            config.clone(),
            agent_tree.clone(),
            openslate_core::tool::builtin_registry(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            provider: openslate_model_openai::client::OpenAICompatibleProvider::new(
                openslate_model_openai::client::OpenAIProviderConfig {
                    provider_name: "zhipu".into(),
                    base_url: "https://example.com".into(),
                    api_key: "test-key".into(),
                    timeout_secs: 60,
                },
            ),
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents.yaml"),
        };

        let mut session = ReplSession::new(ctx, "default".into(), true).unwrap();

        // /exit should return Exit
        let result = session.handle_slash_command("/exit").unwrap();
        assert_eq!(result, DispatchResult::Exit);

        // /quit should also return Exit
        let result2 = session.handle_slash_command("/quit").unwrap();
        assert_eq!(result2, DispatchResult::Exit);
    }

    #[test]
    fn test_unknown_slash_command_returns_continue() {
        let tmp = temp_project();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents.yaml")).unwrap();
        let agent_tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let manager = openslate_core::run_manager::RunManager::new(
            config.clone(),
            agent_tree.clone(),
            openslate_core::tool::builtin_registry(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            provider: openslate_model_openai::client::OpenAICompatibleProvider::new(
                openslate_model_openai::client::OpenAIProviderConfig {
                    provider_name: "zhipu".into(),
                    base_url: "https://example.com".into(),
                    api_key: "test-key".into(),
                    timeout_secs: 60,
                },
            ),
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents.yaml"),
        };

        let mut session = ReplSession::new(ctx, "default".into(), true).unwrap();

        let result = session.handle_slash_command("/unknown").unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    // ── Dispatch logic tests (sync portions) ──

    /// Helper: build a minimal ReplSession for testing sync dispatch logic.
    fn make_session() -> ReplSession {
        let tmp = temp_project();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents.yaml")).unwrap();
        let agent_tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let manager = openslate_core::run_manager::RunManager::new(
            config.clone(),
            agent_tree.clone(),
            openslate_core::tool::builtin_registry(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            provider: openslate_model_openai::client::OpenAICompatibleProvider::new(
                openslate_model_openai::client::OpenAIProviderConfig {
                    provider_name: "zhipu".into(),
                    base_url: "https://example.com".into(),
                    api_key: "test-key".into(),
                    timeout_secs: 60,
                },
            ),
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents.yaml"),
        };

        ReplSession::new(ctx, "default".into(), true).unwrap()
    }

    #[test]
    fn test_empty_input_returns_continue() {
        // Test that dispatching empty strings returns Continue without modifying history.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        let result = rt.block_on(session.dispatch("")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert!(session.history().is_empty(), "empty input should not add to history");
    }

    #[test]
    fn test_whitespace_only_input_returns_continue() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        let result = rt.block_on(session.dispatch("   ")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert!(session.history().is_empty(), "whitespace input should not add to history");
    }

    #[test]
    fn test_slash_exit_dispatch() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        let result = rt.block_on(session.dispatch("/exit")).unwrap();
        assert_eq!(result, DispatchResult::Exit);
    }

    #[test]
    fn test_double_slash_strips_prefix() {
        // `//hello` should be treated as `/hello` (normal input)
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        // This will attempt to execute the agent, which will fail because there's
        // no API key set. That's OK — we just verify the dispatch didn't treat it
        // as a slash command (i.e., it tried normal input path).
        let result = rt.block_on(session.dispatch("//hello"));
        // It will error because the API key env var is not set, but the dispatch
        // should have added the user message to history first.
        assert!(
            session.history().len() == 1,
            "double-slash input should add user message to history"
        );
        assert_eq!(session.history()[0].content, "/hello");
        // The result is an error because no API key, but dispatch was attempted
        assert!(result.is_err(), "should fail due to missing API key");
    }

    #[test]
    fn test_unknown_slash_command_returns_continue_with_message() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        let result = rt.block_on(session.dispatch("/foobar")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert!(
            session.history().is_empty(),
            "unknown slash commands should not add to history"
        );
    }

    #[test]
    fn test_help_command_returns_continue() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        let result = rt.block_on(session.dispatch("/help")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[tokio::test]
    async fn test_normal_input_adds_to_history() {
        let mut session = make_session();

        // This will fail because no API key, but it should still add user message
        let result = session.dispatch("hello world").await;
        assert!(result.is_err(), "should fail due to missing API key");
        assert_eq!(session.history().len(), 1);
        assert_eq!(session.history()[0].role, MessageRole::User);
        assert_eq!(session.history()[0].content, "hello world");
    }

    #[test]
    fn test_quiet_mode_flag() {
        let session = make_session();
        assert!(session.quiet, "session should be in quiet mode");
    }
}
