//! REPL session — interactive chat loop with rustyline.
//!
//! Provides an interactive read-eval-print loop that:
//! - Displays a welcome message with version, profile, model, and agent info
//! - Reads user input via rustyline (UTF-8/CJK safe)
//! - Dispatches `/`-prefixed lines to slash-command handler
//! - Sends normal text to the agent via RunManager
//! - Handles Ctrl+D as /exit, ignores empty input, supports `//literal` escape
//! - Accumulates conversation history across turns (basic multi-turn)

use anyhow::{Context, Result};
use openslate_core::types::{Message, MessageRole};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::time::Instant;

use crate::cmd::run::{build_provider_for_model, resolve_agent};
use crate::spinner::SpinnerCallback;
use crate::wiring::AppContext;

const PROMPT: &str = "openslate> ";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_owned()
    } else {
        let end = max_len.saturating_sub(1);
        format!("{}…", &s[..end])
    }
}

fn truncate_json(json: &str, max_len: usize) -> String {
    let cleaned = json.trim().trim_start_matches('"').trim_end_matches('"');
    truncate_str(cleaned, max_len)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchResult {
    Continue,
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SlashCommand {
    Help,
    Exit,
    New,
    Verbose { on: bool },
    Status,
    Config,
    Agents,
    Model { alias: String },
    Profile { name: String },
    Compact,
    Resume,
    Sessions,
    Unknown { raw: String },
}

impl SlashCommand {
    fn parse(input: &str) -> Self {
        let parts: Vec<&str> = input.splitn(2, char::is_whitespace).collect();
        let command = parts[0];
        let arg = parts.get(1).map(|s| s.trim()).filter(|s| !s.is_empty());

        match command {
            "/help" => SlashCommand::Help,
            "/exit" | "/quit" | "/q" => SlashCommand::Exit,
            "/new" | "/clear" => SlashCommand::New,
            "/verbose" => match arg {
                Some("on") => SlashCommand::Verbose { on: true },
                Some("off") => SlashCommand::Verbose { on: false },
                _ => SlashCommand::Unknown {
                    raw: input.to_owned(),
                },
            },
            "/status" => SlashCommand::Status,
            "/config" => SlashCommand::Config,
            "/agents" => SlashCommand::Agents,
            "/model" => match arg {
                Some(alias) => SlashCommand::Model {
                    alias: alias.to_owned(),
                },
                None => SlashCommand::Unknown {
                    raw: input.to_owned(),
                },
            },
            "/profile" => match arg {
                Some(name) => SlashCommand::Profile {
                    name: name.to_owned(),
                },
                None => SlashCommand::Unknown {
                    raw: input.to_owned(),
                },
            },
            "/compact" => SlashCommand::Compact,
            "/resume" | "/continue" => SlashCommand::Resume,
            "/session" | "/sessions" => SlashCommand::Sessions,
            _ => SlashCommand::Unknown {
                raw: command.to_owned(),
            },
        }
    }
}

#[derive(Debug, Clone)]
struct SessionStats {
    total_steps: u32,
    total_input_tokens: u64,
    total_output_tokens: u64,
    turns: u32,
    started_at: Instant,
}

impl SessionStats {
    fn new() -> Self {
        Self {
            total_steps: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            turns: 0,
            started_at: Instant::now(),
        }
    }
}

pub struct ReplSession {
    ctx: AppContext,
    editor: DefaultEditor,
    profile: String,
    quiet: bool,
    history: Vec<Message>,
    verbose: bool,
    model_override: Option<String>,
    stats: SessionStats,
}

impl ReplSession {
    pub fn new(ctx: AppContext, profile: String, quiet: bool) -> Result<Self> {
        let editor = DefaultEditor::new().context("Failed to initialize readline editor")?;
        Ok(Self {
            ctx,
            editor,
            profile,
            quiet,
            history: Vec::new(),
            verbose: false,
            model_override: None,
            stats: SessionStats::new(),
        })
    }

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
                        .ok();
                    let result = self.dispatch(&line).await?;
                    if result == DispatchResult::Exit {
                        break;
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    continue;
                }
                Err(ReadlineError::Eof) => {
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

    pub fn format_welcome(&self) -> String {
        let root = self.ctx.agent_tree.get_root();
        let model_alias = self.effective_model_alias();
        let model_id = self.resolve_model_id(&model_alias);
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

    async fn dispatch(&mut self, line: &str) -> Result<DispatchResult> {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            return Ok(DispatchResult::Continue);
        }

        if trimmed.starts_with("//") {
            let text = &trimmed[1..];
            return self.handle_normal_input(text).await;
        }

        if trimmed.starts_with('/') {
            return self.handle_slash_command(trimmed).await;
        }

        self.handle_normal_input(trimmed).await
    }

    async fn handle_slash_command(&mut self, input: &str) -> Result<DispatchResult> {
        let cmd = SlashCommand::parse(input);

        match cmd {
            SlashCommand::Help => {
                println!("Available commands:");
                println!("  /help                 — Show this help message");
                println!("  /exit, /quit, /q      — Exit the REPL");
                println!("  /new, /clear          — Clear conversation history");
                println!("  /verbose on|off       — Toggle verbose mode");
                println!("  /status               — Show session statistics");
                println!("  /config               — Display effective configuration");
                println!("  /agents               — Display agent tree structure");
                println!("  /model <alias>        — Switch active model");
                println!("  /profile <name>       — Switch active profile");
                println!("  /compact              — Compress conversation history");
                println!("  /resume, /continue    — Resume last interrupted run");
                println!("  /session, /sessions   — List recent runs from store");
                println!("  //text                — Escape: treat /text as normal input");
                Ok(DispatchResult::Continue)
            }
            SlashCommand::Exit => {
                if !self.quiet {
                    println!("Goodbye!");
                }
                Ok(DispatchResult::Exit)
            }
            SlashCommand::New => {
                self.history.clear();
                self.stats = SessionStats::new();
                if !self.quiet {
                    println!("Conversation cleared.");
                }
                Ok(DispatchResult::Continue)
            }
            SlashCommand::Verbose { on } => {
                self.verbose = on;
                if !self.quiet {
                    println!("Verbose mode {}", if on { "on" } else { "off" });
                }
                Ok(DispatchResult::Continue)
            }
            SlashCommand::Status => {
                let model_alias = self.effective_model_alias();
                let model_id = self.resolve_model_id(&model_alias);
                let elapsed = self.stats.started_at.elapsed();
                let secs = elapsed.as_secs();
                let mins = secs / 60;
                let secs_rem = secs % 60;

                println!("Session status:");
                println!("  profile:     {}", self.profile);
                println!("  model:       {} ({})", model_alias, model_id);
                println!("  turns:       {}", self.stats.turns);
                println!("  steps:       {}", self.stats.total_steps);
                println!(
                    "  tokens:      {} in / {} out",
                    self.stats.total_input_tokens, self.stats.total_output_tokens
                );
                println!("  messages:    {} in history", self.history.len());
                println!("  verbose:     {}", if self.verbose { "on" } else { "off" });
                if mins > 0 {
                    println!("  elapsed:     {}m {}s", mins, secs_rem);
                } else {
                    println!("  elapsed:     {}s", secs);
                }
                Ok(DispatchResult::Continue)
            }
            SlashCommand::Config => {
                println!("Configuration:");
                println!("  config path: {}", self.ctx.config_path.display());
                println!("  agents path: {}", self.ctx.agents_path.display());

                println!("  providers:");
                for (name, provider) in &self.ctx.config.providers {
                    println!(
                        "    {} — base_url={}",
                        name, provider.base_url
                    );
                }

                println!("  models:");
                for (alias, model) in &self.ctx.config.models {
                    let resolved_id = self.resolve_model_id(alias);
                    println!(
                        "    {} → {} (provider={}, tool_call={}, vision={}, reasoning={})",
                        alias,
                        resolved_id,
                        model.provider,
                        model.supports_tool_call,
                        model.supports_vision,
                        model.supports_reasoning
                    );
                }

                if let Some(ref limits) = self.ctx.config.limits {
                    println!("  limits:");
                    println!("    max_steps={}", limits.max_steps);
                    println!("    max_depth={}", limits.max_depth);
                    println!("    max_tool_calls={}", limits.max_tool_calls);
                    println!("    max_child_agent_calls={}", limits.max_child_agent_calls);
                    println!("    timeout_ms={}", limits.timeout_ms);
                    println!("    max_context_messages={}", limits.max_context_messages);
                    println!("    max_context_bytes={}", limits.max_context_bytes);
                    println!("    max_output_bytes={}", limits.max_output_bytes);
                } else {
                    println!("  limits: (using defaults)");
                }

                Ok(DispatchResult::Continue)
            }
            SlashCommand::Agents => {
                self.print_agent_tree();
                Ok(DispatchResult::Continue)
            }
            SlashCommand::Model { alias } => {
                if self.resolve_model_id(&alias) == alias
                    && !self.ctx.config.models.contains_key(&alias)
                {
                    println!(
                        "Unknown model alias: '{}'. Available: {}",
                        alias,
                        self.ctx
                            .config
                            .models
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    return Ok(DispatchResult::Continue);
                }

                let model_id = self.resolve_model_id(&alias);
                self.model_override = Some(alias.clone());
                if !self.quiet {
                    println!("Model switched to {} ({})", alias, model_id);
                }
                Ok(DispatchResult::Continue)
            }
            SlashCommand::Profile { name } => {
                self.profile = name.clone();
                if !self.quiet {
                    println!("Profile switched to '{}'", name);
                }
                Ok(DispatchResult::Continue)
            }
            SlashCommand::Compact => {
                let before = self.history.len();
                if before == 0 {
                    if !self.quiet {
                        println!("Nothing to compact — history is empty.");
                    }
                    return Ok(DispatchResult::Continue);
                }

                let result = openslate_core::context_manager::compact(
                    &mut self.history,
                    None,
                    self.ctx.config.limits.as_ref().map(|l| l.max_context_messages as usize).unwrap_or(16),
                    self.ctx.config.limits.as_ref().map(|l| l.max_context_bytes as usize).unwrap_or(64_000),
                    |_text| None,
                );

                if !self.quiet {
                    println!(
                        "Context compressed: {} messages → {} messages",
                        result.messages_before, result.messages_after
                    );
                }
                Ok(DispatchResult::Continue)
            }
            SlashCommand::Resume => {
                if self.ctx.store.is_none() {
                    println!("Store not available");
                    return Ok(DispatchResult::Continue);
                }
                self.handle_resume().await
            }
            SlashCommand::Sessions => {
                if self.ctx.store.is_none() {
                    println!("Store not available");
                    return Ok(DispatchResult::Continue);
                }
                self.handle_sessions().await
            }
            SlashCommand::Unknown { ref raw } => {
                let cmd_part = raw.split_whitespace().next().unwrap_or(raw);
                println!(
                    "Unknown command: {}. Type /help for available commands.",
                    cmd_part
                );
                Ok(DispatchResult::Continue)
            }
        }
    }

    async fn handle_normal_input(&mut self, input: &str) -> Result<DispatchResult> {
        self.history.push(Message {
            role: MessageRole::User,
            content: input.to_owned(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        });

        let _agent = resolve_agent(&self.ctx.agent_tree, None)?;
        let model_alias = self.effective_model_alias();
        let provider = build_provider_for_model(&self.ctx.config, &model_alias)?;

        let start = Instant::now();
        let mut callback = SpinnerCallback::new(&model_alias, self.quiet);
        let result = match self
            .ctx
            .manager
            .execute_with_history(&*provider, &self.history, Some(&mut callback))
            .await
        {
            Ok(r) => {
                callback.finish();
                r
            }
            Err(e) => {
                let msg = e.to_string();
                callback.finish_with_error(&msg);
                return Err(anyhow::anyhow!("Agent execution failed: {}", e));
            }
        };
        let _elapsed = start.elapsed();

        self.stats.total_steps += result.total_steps;
        self.stats.total_input_tokens += result.total_input_tokens as u64;
        self.stats.total_output_tokens += result.total_output_tokens as u64;
        self.stats.turns += 1;

        if self.verbose {
            println!(
                "[verbose] steps={}, tokens_in={}, tokens_out={}, elapsed={:?}",
                result.total_steps,
                result.total_input_tokens,
                result.total_output_tokens,
                _elapsed
            );
        }

        let final_message = result
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, MessageRole::Assistant))
            .map(|m| m.content.clone())
            .unwrap_or_else(|| "(no assistant response)".to_owned());

        self.history.push(Message {
            role: MessageRole::Assistant,
            content: final_message.clone(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        });

        if !self.quiet {
            crate::markdown::print_markdown(&final_message);
        }

        Ok(DispatchResult::Continue)
    }

    fn effective_model_alias(&self) -> String {
        self.model_override
            .clone()
            .unwrap_or_else(|| self.ctx.agent_tree.get_root().model_alias.clone())
    }

    fn resolve_model_id(&self, model_alias: &str) -> String {
        openslate_core::model_config::resolve_model(&self.ctx.config, model_alias)
            .map(|r| r.model_id)
            .unwrap_or_else(|_| model_alias.to_owned())
    }

    fn print_agent_tree(&self) {
        let root = self.ctx.agent_tree.get_root();
        println!("Agent tree:");
        self.print_agent_node(root, 0);
    }

    fn print_agent_node(&self, node: &openslate_core::agent_tree::AgentNode, depth: usize) {
        let indent = "  ".repeat(depth);
        let model_id = self.resolve_model_id(&node.model_alias);
        let tools_str = if node.tools.is_empty() {
            String::new()
        } else {
            format!(" [tools: {}]", node.tools.join(", "))
        };
        println!(
            "{}{} ({}) — model: {} ({}){}",
            indent, node.id.0, node.name, node.model_alias, model_id, tools_str
        );
        for child_id in &node.children {
            if let Some(child) = self.ctx.agent_tree.get_agent(child_id) {
                self.print_agent_node(child, depth + 1);
            }
        }
    }

    async fn handle_resume(&mut self) -> Result<DispatchResult> {
        let store = match self.ctx.store {
            Some(ref s) => s,
            None => {
                println!("Store not available");
                return Ok(DispatchResult::Continue);
            }
        };

        match store.get_last_interrupted_run().await {
            Ok(Some(run)) => {
                let input_preview = truncate_json(&run.input_json, 80);
                println!("Found interrupted run:");
                println!("  ID:        {}", run.id);
                println!("  Status:    {}", run.status);
                println!("  Started:   {}", run.started_at);
                println!("  Input:     {}", input_preview);
                println!("(Resume display only — full replay not yet implemented)");
            }
            Ok(None) => {
                println!("No interrupted runs found");
            }
            Err(e) => {
                println!("Store query failed: {}", e);
            }
        }

        Ok(DispatchResult::Continue)
    }

    async fn handle_sessions(&mut self) -> Result<DispatchResult> {
        let store = match self.ctx.store {
            Some(ref s) => s,
            None => {
                println!("Store not available");
                return Ok(DispatchResult::Continue);
            }
        };

        match store.list_runs(10, 0).await {
            Ok(runs) => {
                if runs.is_empty() {
                    println!("No runs found");
                    return Ok(DispatchResult::Continue);
                }

                println!(
                    "{:<4} {:<20} {:<14} {:<12} {:<16} Input",
                    "#", "Run ID", "Status", "Title", "Started"
                );
                println!("{}", "-".repeat(90));
                for (i, run) in runs.iter().enumerate() {
                    let title = run.title.as_deref().unwrap_or("-");
                    let input_preview = truncate_json(&run.input_json, 40);
                    println!(
                        "{:<4} {:<20} {:<14} {:<12} {:<16} {}",
                        i + 1,
                        truncate_str(&run.id, 18),
                        truncate_str(&run.status, 12),
                        truncate_str(title, 10),
                        run.started_at,
                        input_preview
                    );
                }
            }
            Err(e) => {
                println!("Store query failed: {}", e);
            }
        }

        Ok(DispatchResult::Continue)
    }

    #[cfg(test)]
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    #[cfg(test)]
    pub fn verbose(&self) -> bool {
        self.verbose
    }

    #[cfg(test)]
    pub fn model_override(&self) -> Option<&str> {
        self.model_override.as_deref()
    }

    #[cfg(test)]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    #[cfg(test)]
    pub fn stats(&self) -> &SessionStats {
        &self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiring;
    use std::fs;

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(future)
    }

    fn temp_project() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
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
        let agents_dir = openslate_dir.join("agents");
        fs::create_dir(&agents_dir).expect("create agents dir");
        let agent_md = "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - current_time\n---\nYou are the root agent.\n";
        fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        fs::write(agents_dir.join("root.md"), agent_md).expect("write root.md");
        tmp
    }

    fn temp_project_with_children() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
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
        let agents_dir = openslate_dir.join("agents");
        fs::create_dir(&agents_dir).expect("create agents dir");
        fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        fs::write(agents_dir.join("root.md"), "---\nid: root\nname: Root Agent\nmodel: main\nchildren:\n  - researcher\n  - writer\n---\nYou are the root agent.\n").expect("write root.md");
        fs::write(agents_dir.join("researcher.md"), "---\nid: researcher\nname: Researcher\nmodel: fast\n---\nYou are a researcher.\n").expect("write researcher.md");
        fs::write(agents_dir.join("writer.md"), "---\nid: writer\nname: Writer\nmodel: fast\n---\nYou are a writer.\n").expect("write writer.md");
        tmp
    }

    fn temp_project_multi_model() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "TEST_API_KEY"

[models.main]
provider = "zhipu"
model = "test-model-v1"

[models.fast]
provider = "zhipu"
model = "fast-model-v1"

[models.deep]
provider = "zhipu"
model = "deep-model-v1"

[limits]
max_steps = 10
max_depth = 3
max_tool_calls = 20
"#;
        let agents_dir = openslate_dir.join("agents");
        fs::create_dir(&agents_dir).expect("create agents dir");
        let agent_md = "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - current_time\n---\nYou are the root agent.\n";
        fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        fs::write(agents_dir.join("root.md"), agent_md).expect("write root.md");
        tmp
    }

    fn make_session() -> ReplSession {
        let tmp = temp_project();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents")).unwrap();
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
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
            #[cfg(feature = "mcp")]
            mcp_connections: openslate_core::mcp::McpConnectionGuard::default(),
        };

        ReplSession::new(ctx, "default".into(), true).unwrap()
    }

    fn make_session_with_children() -> ReplSession {
        let tmp = temp_project_with_children();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents")).unwrap();
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
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
            #[cfg(feature = "mcp")]
            mcp_connections: openslate_core::mcp::McpConnectionGuard::default(),
        };

        ReplSession::new(ctx, "default".into(), true).unwrap()
    }

    fn make_session_multi_model() -> ReplSession {
        let tmp = temp_project_multi_model();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents")).unwrap();
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
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
            #[cfg(feature = "mcp")]
            mcp_connections: openslate_core::mcp::McpConnectionGuard::default(),
        };

        ReplSession::new(ctx, "default".into(), true).unwrap()
    }

    fn make_ctx_helper(
        tmp: &tempfile::TempDir,
    ) -> (openslate_core::config::OpenSlateConfig, openslate_core::config::AgentsConfig, openslate_core::agent_tree::AgentTree, openslate_core::run_manager::RunManager) {
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents")).unwrap();
        let agent_tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let manager = openslate_core::run_manager::RunManager::new(
            config.clone(),
            agent_tree.clone(),
            openslate_core::tool::builtin_registry(),
        );
        (config, agents, agent_tree, manager)
    }

    fn make_non_quiet_session() -> ReplSession {
        let tmp = temp_project();
        let (config, agents, agent_tree, manager) = make_ctx_helper(&tmp);
        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
            #[cfg(feature = "mcp")]
            mcp_connections: openslate_core::mcp::McpConnectionGuard::default(),
        };
        ReplSession::new(ctx, "default".into(), false).unwrap()
    }

    fn make_non_quiet_session_profile(profile: &str) -> ReplSession {
        let tmp = temp_project();
        let (config, agents, agent_tree, manager) = make_ctx_helper(&tmp);
        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
            #[cfg(feature = "mcp")]
            mcp_connections: openslate_core::mcp::McpConnectionGuard::default(),
        };
        ReplSession::new(ctx, profile.into(), false).unwrap()
    }

    fn make_non_quiet_session_with_children() -> ReplSession {
        let tmp = temp_project_with_children();
        let (config, agents, agent_tree, manager) = make_ctx_helper(&tmp);
        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
            #[cfg(feature = "mcp")]
            mcp_connections: openslate_core::mcp::McpConnectionGuard::default(),
        };
        ReplSession::new(ctx, "default".into(), false).unwrap()
    }

    // ── SlashCommand parsing tests ──

    #[test]
    fn test_parse_help() {
        assert_eq!(SlashCommand::parse("/help"), SlashCommand::Help);
    }

    #[test]
    fn test_parse_exit_variants() {
        assert_eq!(SlashCommand::parse("/exit"), SlashCommand::Exit);
        assert_eq!(SlashCommand::parse("/quit"), SlashCommand::Exit);
        assert_eq!(SlashCommand::parse("/q"), SlashCommand::Exit);
    }

    #[test]
    fn test_parse_new_and_clear() {
        assert_eq!(SlashCommand::parse("/new"), SlashCommand::New);
        assert_eq!(SlashCommand::parse("/clear"), SlashCommand::New);
    }

    #[test]
    fn test_parse_verbose() {
        assert_eq!(
            SlashCommand::parse("/verbose on"),
            SlashCommand::Verbose { on: true }
        );
        assert_eq!(
            SlashCommand::parse("/verbose off"),
            SlashCommand::Verbose { on: false }
        );
    }

    #[test]
    fn test_parse_verbose_without_arg_is_unknown() {
        match SlashCommand::parse("/verbose") {
            SlashCommand::Unknown { .. } => {}
            other => panic!("expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_status() {
        assert_eq!(SlashCommand::parse("/status"), SlashCommand::Status);
    }

    #[test]
    fn test_parse_config() {
        assert_eq!(SlashCommand::parse("/config"), SlashCommand::Config);
    }

    #[test]
    fn test_parse_agents() {
        assert_eq!(SlashCommand::parse("/agents"), SlashCommand::Agents);
    }

    #[test]
    fn test_parse_model() {
        assert_eq!(
            SlashCommand::parse("/model fast"),
            SlashCommand::Model {
                alias: "fast".to_owned()
            }
        );
    }

    #[test]
    fn test_parse_model_without_arg_is_unknown() {
        match SlashCommand::parse("/model") {
            SlashCommand::Unknown { .. } => {}
            other => panic!("expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_profile() {
        assert_eq!(
            SlashCommand::parse("/profile myprofile"),
            SlashCommand::Profile {
                name: "myprofile".to_owned()
            }
        );
    }

    #[test]
    fn test_parse_profile_without_arg_is_unknown() {
        match SlashCommand::parse("/profile") {
            SlashCommand::Unknown { .. } => {}
            other => panic!("expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_unknown_command() {
        match SlashCommand::parse("/foobar") {
            SlashCommand::Unknown { ref raw } if raw == "/foobar" => {}
            other => panic!("expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_resume() {
        assert_eq!(SlashCommand::parse("/resume"), SlashCommand::Resume);
    }

    #[test]
    fn test_parse_continue_is_resume() {
        assert_eq!(SlashCommand::parse("/continue"), SlashCommand::Resume);
    }

    #[test]
    fn test_parse_session() {
        assert_eq!(SlashCommand::parse("/session"), SlashCommand::Sessions);
    }

    #[test]
    fn test_parse_sessions() {
        assert_eq!(SlashCommand::parse("/sessions"), SlashCommand::Sessions);
    }

    // ── Welcome message tests ──

    #[test]
    fn test_welcome_contains_version() {
        let session = make_non_quiet_session();
        let welcome = session.format_welcome();
        assert!(
            welcome.contains("OpenSlate v0.1.0"),
            "welcome should contain version: {}",
            welcome
        );
    }

    #[test]
    fn test_welcome_contains_profile() {
        let session = make_non_quiet_session_profile("custom-profile");
        let welcome = session.format_welcome();
        assert!(
            welcome.contains("profile: custom-profile"),
            "welcome should contain profile: {}",
            welcome
        );
    }

    #[test]
    fn test_welcome_contains_model() {
        let session = make_non_quiet_session();
        let welcome = session.format_welcome();
        assert!(
            welcome.contains("model: main (test-model-v1)"),
            "welcome should contain model alias and id: {}",
            welcome
        );
    }

    #[test]
    fn test_welcome_shows_children_agents() {
        let session = make_non_quiet_session_with_children();
        let welcome = session.format_welcome();
        assert!(
            welcome.contains("agents: root → [researcher, writer]"),
            "welcome should show agent tree: {}",
            welcome
        );
    }

    #[test]
    fn test_welcome_shows_single_agent_no_children() {
        let session = make_non_quiet_session();
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
        let session = make_non_quiet_session();
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
        let mut session = make_session();
        let result = block_on(session.handle_slash_command("/exit")).unwrap();
        assert_eq!(result, DispatchResult::Exit);

        let result2 = block_on(session.handle_slash_command("/quit")).unwrap();
        assert_eq!(result2, DispatchResult::Exit);
    }

    #[test]
    fn test_slash_q_returns_exit() {
        let mut session = make_session();
        let result = block_on(session.handle_slash_command("/q")).unwrap();
        assert_eq!(result, DispatchResult::Exit);
    }

    #[test]
    fn test_unknown_slash_command_returns_continue() {
        let mut session = make_session();
        let result = block_on(session.handle_slash_command("/unknown")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[test]
    fn test_help_command_lists_all_commands() {
        let mut session = make_session();
        let result = block_on(session.handle_slash_command("/help")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[test]
    fn test_new_clears_history() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        session.history.push(Message {
            role: MessageRole::User,
            content: "test".to_owned(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        });
        assert_eq!(session.history().len(), 1);

        let result = rt.block_on(session.dispatch("/new")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert!(session.history().is_empty(), "/new should clear history");
    }

    #[test]
    fn test_clear_clears_history() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        session.history.push(Message {
            role: MessageRole::User,
            content: "test".to_owned(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        });

        let result = rt.block_on(session.dispatch("/clear")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert!(
            session.history().is_empty(),
            "/clear should clear history"
        );
    }

    #[test]
    fn test_verbose_on() {
        let mut session = make_session();
        assert!(!session.verbose(), "verbose should start off");

        block_on(session.handle_slash_command("/verbose on")).unwrap();
        assert!(session.verbose(), "verbose should be on");
    }

    #[test]
    fn test_verbose_off() {
        let mut session = make_session();
        block_on(session.handle_slash_command("/verbose on")).unwrap();
        assert!(session.verbose());

        block_on(session.handle_slash_command("/verbose off")).unwrap();
        assert!(!session.verbose(), "verbose should be off");
    }

    #[test]
    fn test_verbose_without_arg_is_unknown() {
        let mut session = make_session();
        let result = block_on(session.handle_slash_command("/verbose")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert!(
            !session.verbose(),
            "verbose should remain off for invalid arg"
        );
    }

    #[test]
    fn test_status_command() {
        let mut session = make_session();
        let result = block_on(session.handle_slash_command("/status")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert_eq!(session.stats().turns, 0);
        assert_eq!(session.stats().total_steps, 0);
        assert_eq!(session.stats().total_input_tokens, 0);
        assert_eq!(session.stats().total_output_tokens, 0);
    }

    #[test]
    fn test_config_command() {
        let mut session = make_session();
        let result = block_on(session.handle_slash_command("/config")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[test]
    fn test_agents_command() {
        let mut session = make_session();
        let result = block_on(session.handle_slash_command("/agents")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[test]
    fn test_agents_command_with_children() {
        let mut session = make_session_with_children();
        let result = block_on(session.handle_slash_command("/agents")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[test]
    fn test_model_switches_model() {
        let mut session = make_session_multi_model();
        assert!(session.model_override().is_none());

        block_on(session.handle_slash_command("/model fast")).unwrap();
        assert_eq!(session.model_override(), Some("fast"));

        block_on(session.handle_slash_command("/model deep")).unwrap();
        assert_eq!(session.model_override(), Some("deep"));
    }

    #[test]
    fn test_model_unknown_alias_still_switches() {
        let mut session = make_session();
        let result = block_on(session.handle_slash_command("/model nonexistent")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[test]
    fn test_profile_switches_profile() {
        let mut session = make_session();
        assert_eq!(session.profile(), "default");

        block_on(session.handle_slash_command("/profile custom")).unwrap();
        assert_eq!(session.profile(), "custom");

        block_on(session.handle_slash_command("/profile another")).unwrap();
        assert_eq!(session.profile(), "another");
    }

    // ── Dispatch logic tests ──

    #[test]
    fn test_empty_input_returns_continue() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        let result = rt.block_on(session.dispatch("")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert!(
            session.history().is_empty(),
            "empty input should not add to history"
        );
    }

    #[test]
    fn test_whitespace_only_input_returns_continue() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        let result = rt.block_on(session.dispatch("   ")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert!(
            session.history().is_empty(),
            "whitespace input should not add to history"
        );
    }

    #[test]
    fn test_slash_exit_dispatch() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        let result = rt.block_on(session.dispatch("/exit")).unwrap();
        assert_eq!(result, DispatchResult::Exit);
    }

    #[test]
    fn test_slash_q_dispatch() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        let result = rt.block_on(session.dispatch("/q")).unwrap();
        assert_eq!(result, DispatchResult::Exit);
    }

    #[test]
    fn test_double_slash_strips_prefix() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        let result = rt.block_on(session.dispatch("//hello"));
        assert!(
            session.history().len() == 1,
            "double-slash input should add user message to history"
        );
        assert_eq!(session.history()[0].content, "/hello");
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

    #[test]
    fn test_verbose_toggles_via_dispatch() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        let result = rt.block_on(session.dispatch("/verbose on")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert!(session.verbose());

        let result = rt.block_on(session.dispatch("/verbose off")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert!(!session.verbose());
    }

    #[test]
    fn test_model_switches_via_dispatch() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session_multi_model();

        let result = rt.block_on(session.dispatch("/model fast")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert_eq!(session.model_override(), Some("fast"));
    }

    #[test]
    fn test_profile_switches_via_dispatch() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();

        let result = rt.block_on(session.dispatch("/profile myprofile")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert_eq!(session.profile(), "myprofile");
    }

    #[test]
    fn test_new_resets_stats() {
        let mut session = make_session();

        session.stats.turns = 5;
        session.stats.total_steps = 10;

        block_on(session.handle_slash_command("/new")).unwrap();
        assert_eq!(session.stats().turns, 0, "/new should reset turns");
        assert_eq!(
            session.stats().total_steps, 0,
            "/new should reset total_steps"
        );
    }

    #[test]
    fn test_config_command_multi_model() {
        let mut session = make_session_multi_model();
        let result = block_on(session.handle_slash_command("/config")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    // ── /resume and /session tests ──

    #[test]
    fn test_resume_no_store() {
        let mut session = make_session();
        let result = block_on(session.handle_slash_command("/resume")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[test]
    fn test_sessions_no_store() {
        let mut session = make_session();
        let result = block_on(session.handle_slash_command("/sessions")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[test]
    fn test_resume_via_continue_alias() {
        let mut session = make_session();
        let result = block_on(session.handle_slash_command("/continue")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[test]
    fn test_resume_via_dispatch() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();
        let result = rt.block_on(session.dispatch("/resume")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[test]
    fn test_sessions_via_dispatch() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();
        let result = rt.block_on(session.dispatch("/sessions")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[tokio::test]
    async fn test_resume_with_store_no_interrupted_runs() {
        let store = openslate_store_sqlite::store::SqliteStore::new_in_memory()
            .await
            .expect("store");
        store.run_migrations().await.expect("migrations");

        let tmp = temp_project();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents")).unwrap();
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
            store: Some(store),
            agent_tree,
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
            #[cfg(feature = "mcp")]
            mcp_connections: openslate_core::mcp::McpConnectionGuard::default(),
        };

        let mut session = ReplSession::new(ctx, "default".into(), true).unwrap();
        let result = session.handle_resume().await.unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[tokio::test]
    async fn test_sessions_with_store_empty() {
        let store = openslate_store_sqlite::store::SqliteStore::new_in_memory()
            .await
            .expect("store");
        store.run_migrations().await.expect("migrations");

        let tmp = temp_project();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents")).unwrap();
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
            store: Some(store),
            agent_tree,
            manager,
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
            #[cfg(feature = "mcp")]
            mcp_connections: openslate_core::mcp::McpConnectionGuard::default(),
        };

        let mut session = ReplSession::new(ctx, "default".into(), true).unwrap();
        let result = session.handle_sessions().await.unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    #[test]
    fn test_truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_str_long() {
        let result = truncate_str("hello world this is a long string", 10);
        assert_eq!(result, "hello wor…");
    }

    #[test]
    fn test_truncate_json_strips_quotes() {
        assert_eq!(truncate_json(r#""hello""#, 20), "hello");
    }

    #[test]
    fn test_truncate_json_plain() {
        assert_eq!(truncate_json("hello world", 20), "hello world");
    }
}
