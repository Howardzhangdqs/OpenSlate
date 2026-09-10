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
use openslate_core::approval::{ApprovalCallback, ApprovalDecision, ApprovalRequest};
use openslate_core::context_manager::CompactResult;
use openslate_core::run_manager::RunManager;
use openslate_core::runtime::{CancellationToken, MessageSink};
use openslate_core::types::{Message, MessageRole, RunId, RunStatus, Usage};
use openslate_store_sqlite::recorder::RunRecorder;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::cmd::run::{build_provider_for_model, resolve_agent};
use crate::spinner::SpinnerCallback;
use crate::wiring::AppContext;

const PROMPT: &str = "openslate> ";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Readline prompt for the interactive approval callback.
const APPROVAL_PROMPT: &str = "允许执行? [y]es / [n]o / [a]lways: ";

/// System prompt for the LLM summarization behind /compact and
/// auto-compact. Instructs the (fast) model to keep exactly what a later
/// turn needs: decisions, file paths, unfinished work — and to stay brief.
const SUMMARY_SYSTEM_PROMPT: &str = "You summarize agent conversation transcripts \
for continued work. Produce a concise summary that preserves: \
(1) key decisions made and their rationale, \
(2) important file paths, commands, and code artifacts touched, \
(3) unfinished tasks, open questions, and next steps. \
Drop pleasantries and verbose tool output details. Be brief — only what is \
needed to continue the work effectively.";

/// One LLM summarization attempt for context compaction.
///
/// Returns `(summary, usage)`. A `None` summary means "fall back to the
/// mechanical concatenation" — provider errors and empty replies are
/// DEGRADED to the fallback, never surfaced as errors: any config may lack
/// a `fast` model, and compaction must keep working without one. Usage (when
/// the call did happen) is still returned so the caller can account for it
/// in session stats.
async fn generate_summary(
    provider: &dyn openslate_core::provider::ModelProvider,
    model_id: &str,
    conversation_text: &str,
) -> (Option<String>, Option<Usage>) {
    use openslate_core::provider::GenerateRequest;

    let request = GenerateRequest {
        model_id: model_id.to_owned(),
        system_prompt: Some(SUMMARY_SYSTEM_PROMPT.to_owned()),
        messages: vec![Message {
            role: MessageRole::User,
            content: format!(
                "Summarize the following conversation for continuation:\n\n{}",
                conversation_text
            ),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
    };

    match provider.generate(request).await {
        Ok(response) => {
            let usage = response.usage;
            // An empty/blank reply is treated as failure → mechanical
            // fallback (an empty summary message would be worse than none).
            let summary = response.content.filter(|c| !c.trim().is_empty());
            (summary, usage)
        }
        Err(e) => {
            tracing::warn!(
                "compact summary generation failed, using mechanical fallback: {}",
                e
            );
            (None, None)
        }
    }
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_owned()
    } else {
        // Walk back to a UTF-8 char boundary so multi-byte input (CJK, emoji)
        // cannot panic on slicing.
        let mut end = max_len.saturating_sub(1);
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
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
    /// Session-wide cost in USD (P2-3): every turn's run cost PLUS the
    /// compact summary calls — what /status displays.
    total_cost_usd: f64,
    /// The compact-summary subset of `total_cost_usd` (P2-3 口径): summary
    /// calls bill the SESSION but not the persisted run row, whose finish
    /// gets `run_cost_usd()` = total − compact.
    compact_cost_usd: f64,
}

/// The REPL session's backing persisted run (Phase 3).
///
/// One run row spans the whole session: every turn's messages land under
/// the same `run_id` (monotonic `seq`), which is exactly what `/resume`
/// needs to restore a session after exit or crash. `/new` drops it so the
/// next turn begins a fresh run.
#[derive(Clone)]
struct SessionRun {
    run_id: RunId,
    recorder: Arc<RunRecorder>,
}

impl SessionStats {
    fn new() -> Self {
        Self {
            total_steps: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            turns: 0,
            started_at: Instant::now(),
            total_cost_usd: 0.0,
            compact_cost_usd: 0.0,
        }
    }

    /// Cost attributable to the persisted run row (P2-3): everything the
    /// session spent EXCEPT the compact summary calls, which bill the
    /// session only.
    fn run_cost_usd(&self) -> f64 {
        self.total_cost_usd - self.compact_cost_usd
    }
}

/// One answer read at the approval prompt (from the shared readline editor
/// in production, injected by tests).
enum PromptAnswer {
    Line(String),
    /// Ctrl-C at the prompt.
    Interrupted,
    /// Any other readline failure.
    ReadError,
}

/// Source of approval-prompt answers; boxed so the callback (which is
/// `Send + Sync` and called with `&self`) can hold it.
type PromptAnswerReader = Box<dyn Fn() -> PromptAnswer + Send + Sync>;

/// Slot holding the CURRENT turn's cancellation token (Phase 4).
///
/// The token is per-turn (a `CancellationToken` cannot be reset), but the
/// approval callback is built once per session — so both share this slot:
/// each turn installs its fresh token, and a Ctrl-C at the approval prompt
/// cancels whatever turn is running.
#[derive(Clone)]
struct CancelSlot(Arc<Mutex<CancellationToken>>);

impl CancelSlot {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(CancellationToken::new())))
    }

    /// Install the token for the turn that is about to run.
    fn set(&self, token: CancellationToken) {
        *self.0.lock().expect("cancel slot poisoned") = token;
    }

    /// Cancel the current turn's token (no-op when no turn is running).
    fn cancel(&self) {
        self.0.lock().expect("cancel slot poisoned").cancel();
    }

    /// The current token (for assertions in tests).
    #[cfg(test)]
    fn current(&self) -> CancellationToken {
        self.0.lock().expect("cancel slot poisoned").clone()
    }
}

/// Interactive approval callback (REPL, Phase 1).
///
/// Prompts y/n/a on the shared readline editor, showing the requesting
/// agent, tool, assessed risk, and an arguments preview. `a` adds the tool
/// to a session-scoped allowlist so later calls to the same tool stop
/// prompting (session downgrade). Ctrl-C at the prompt CANCELS THE WHOLE
/// RUN (Phase 4): the current turn's token is cancelled (the runtime stops
/// at its next checkpoint with the partial transcript) and the call itself
/// is denied so the loop unwinds immediately.
struct InteractiveApproval {
    allowlist: Mutex<HashSet<String>>,
    /// Serializes the whole approval prompt (header print + answer read).
    /// The readline editor is already an `Arc<Mutex>` (naturally
    /// serialized), but the header prints used to happen OUTSIDE that
    /// lock — with parallel tool calls (P2-1) two approval requests could
    /// interleave their headers. This mutex wraps the ENTIRE prompting
    /// section so one request's header and answer complete before the
    /// next request begins. The allowlist fast path stays outside it.
    prompt: Mutex<()>,
    read_answer: PromptAnswerReader,
    cancel: CancelSlot,
}

impl InteractiveApproval {
    fn new(read_answer: PromptAnswerReader, cancel: CancelSlot) -> Self {
        Self {
            allowlist: Mutex::new(HashSet::new()),
            prompt: Mutex::new(()),
            read_answer,
            cancel,
        }
    }
}

impl ApprovalCallback for InteractiveApproval {
    fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision {
        if self
            .allowlist
            .lock()
            .expect("allowlist poisoned")
            .contains(&req.tool_name)
        {
            tracing::debug!(
                target: "openslate_approval",
                "session allowlist approved tool '{}'",
                req.tool_name
            );
            return ApprovalDecision::Approved;
        }
        // One prompt at a time (P2-1): held across the header print AND
        // the (blocking) answer read, so concurrent approval requests —
        // e.g. from a parallel tool batch — queue up instead of
        // interleaving their output.
        let _prompt_guard = self.prompt.lock().expect("prompt lock poisoned");
        loop {
            println!(
                "⚠ 审批请求  agent: {} | tool: {} | risk: {}",
                req.agent_id, req.tool_name, req.risk_level
            );
            println!("  args: {}", truncate_json(&req.arguments.to_string(), 120));
            match (self.read_answer)() {
                PromptAnswer::Line(line) => match line.trim().chars().next() {
                    Some(c) if c.eq_ignore_ascii_case(&'y') => {
                        return ApprovalDecision::Approved;
                    }
                    Some(c) if c.eq_ignore_ascii_case(&'a') => {
                        self.allowlist
                            .lock()
                            .expect("allowlist poisoned")
                            .insert(req.tool_name.clone());
                        println!(
                            "已加入会话 allowlist:'{}' 的后续调用将不再询问",
                            req.tool_name
                        );
                        return ApprovalDecision::Approved;
                    }
                    Some(c) if c.eq_ignore_ascii_case(&'n') => {
                        return ApprovalDecision::Denied("用户在审批提示上选择了拒绝".to_owned());
                    }
                    _ => {
                        println!("请回答 y / n / a(Ctrl-C 取消本次运行)");
                    }
                },
                PromptAnswer::Interrupted => {
                    println!("Ctrl-C:已取消本次运行(工具调用已拒绝,运行将在下一个检查点停止)");
                    self.cancel.cancel();
                    return ApprovalDecision::Denied(
                        "在审批提示上收到 Ctrl-C,本次运行已取消".to_owned(),
                    );
                }
                PromptAnswer::ReadError => {
                    println!("无法读取审批输入:已拒绝本次工具调用");
                    return ApprovalDecision::Denied("无法读取审批输入,已拒绝本次调用".to_owned());
                }
            }
        }
    }
}

pub struct ReplSession {
    ctx: AppContext,
    /// Readline editor shared with the approval callback (Arc<Mutex>): the
    /// prompt callback reads answers while the main loop is awaiting the
    /// run, never concurrently with the main prompt.
    editor: Arc<Mutex<DefaultEditor>>,
    profile: String,
    quiet: bool,
    history: Vec<Message>,
    verbose: bool,
    model_override: Option<String>,
    stats: SessionStats,
    /// Backing persisted run for this session (Phase 3), created lazily on
    /// the first turn (or adopted from `/resume`). `None` while there is no
    /// store or until the session has something to persist.
    session_run: Option<SessionRun>,
    /// Current turn's cancellation token slot (Phase 4), shared with the
    /// approval callback — see [`CancelSlot`].
    cancel_slot: CancelSlot,
}

impl ReplSession {
    pub fn new(mut ctx: AppContext, profile: String, quiet: bool) -> Result<Self> {
        let editor = Arc::new(Mutex::new(
            DefaultEditor::new().context("Failed to initialize readline editor")?,
        ));

        // Approval wiring (Phase 1): interactive sessions get y/n/a prompts
        // on the shared editor (`a` allowlists the tool for the session).
        // Effective policy: `[approval].policy` > interactive default
        // auto_except(["shell", "run_code"]) when the section is absent —
        // locked product decision, announced with one startup line.
        let approval_section_absent = ctx.config.approval.is_none();
        let configured = ctx.config.approval.as_ref().map(|a| a.to_policy());
        let effective = crate::wiring::derive_effective_policy(configured, true, false);
        if approval_section_absent && !quiet {
            println!(
                "提示: 未配置 [approval] 节,REPL 默认审批策略为 \
                 auto_except([shell, run_code]) —— 高危工具执行前会询问"
            );
        }
        let prompt_editor = Arc::clone(&editor);
        let read_answer: PromptAnswerReader = Box::new(move || {
            match prompt_editor
                .lock()
                .expect("editor poisoned")
                .readline(APPROVAL_PROMPT)
            {
                Ok(line) => PromptAnswer::Line(line),
                Err(ReadlineError::Interrupted) => PromptAnswer::Interrupted,
                Err(_) => PromptAnswer::ReadError,
            }
        });
        let cancel_slot = CancelSlot::new();
        ctx.manager.approval =
            openslate_core::approval::ApprovalManager::new(effective).with_callback(Arc::new(
                InteractiveApproval::new(read_answer, cancel_slot.clone()),
            ));

        Ok(Self {
            ctx,
            editor,
            profile,
            quiet,
            history: Vec::new(),
            verbose: false,
            model_override: None,
            stats: SessionStats::new(),
            session_run: None,
            cancel_slot,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        if !self.quiet {
            let welcome = self.format_welcome();
            println!("{}", welcome);
        }

        loop {
            let readline = {
                let mut editor = self.editor.lock().expect("editor poisoned");
                let line = editor.readline(PROMPT);
                if let Ok(ref l) = line {
                    editor.add_history_entry(l.as_str()).ok();
                }
                line
            };
            match readline {
                Ok(line) => {
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

        // Clean exit (EOF / /exit): the session run completes. Error paths
        // above return early WITHOUT this update, deliberately leaving the
        // run at `running` — a crashed session stays resumable via /resume.
        if let Some(ref run) = self.session_run {
            if let Err(e) = run
                .recorder
                .finish("completed", None, self.stats.run_cost_usd())
                .await
            {
                tracing::warn!("Failed to persist session completion: {}", e);
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
                println!("  /resume, /continue    — Restore the most recent run's conversation");
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
                // Snapshot the run-only cost BEFORE resetting stats — the
                // backing run below is closed with this value, and reading
                // it after the reset would always persist 0.0.
                let closed_run_cost = self.stats.run_cost_usd();
                self.stats = SessionStats::new();
                // Close the backing run (its transcript stays resumable);
                // the next turn opens a fresh one.
                if let Some(run) = self.session_run.take() {
                    if let Err(e) = run
                        .recorder
                        .finish("completed", None, closed_run_cost)
                        .await
                    {
                        tracing::warn!("Failed to persist previous session run: {}", e);
                    }
                }
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
                println!("  cost:        {}", self.format_session_cost(&model_alias));
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
                    println!("    {} — base_url={}", name, provider.base_url);
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

                let result = self.run_compact().await;

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

    /// Lazily open (or return the existing) persisted session run.
    ///
    /// The first turn inserts the run row (`status = "running"`); every
    /// later turn reuses it so the whole session shares one `run_id` and
    /// one monotonic `seq` sequence — the property `/resume` relies on.
    /// Returns `None` when there is no store or the insert failed (the
    /// session then runs unpersisted, never blocked by store trouble).
    async fn current_run(&mut self) -> Option<SessionRun> {
        if let Some(run) = &self.session_run {
            return Some(run.clone());
        }
        let store = self.ctx.store.clone()?;
        let root_agent_id = self.ctx.agent_tree.get_root().id.0.clone();
        let run_id = RunManager::new_run_id();
        match RunRecorder::begin(
            store,
            run_id.clone(),
            &root_agent_id,
            Some("repl session"),
            r#"{"kind":"repl"}"#,
        )
        .await
        {
            Ok(recorder) => {
                let run = SessionRun {
                    run_id,
                    recorder: Arc::new(recorder),
                };
                self.session_run = Some(run.clone());
                Some(run)
            }
            Err(e) => {
                if !self.quiet {
                    println!("警告: 会话持久化不可用({}),本轮起不落盘", e);
                }
                None
            }
        }
    }

    /// Effective context limits for compaction decisions, from `[limits]`
    /// (or the same defaults the REPL always fell back to).
    fn context_limits(&self) -> (usize, usize) {
        (
            self.ctx
                .config
                .limits
                .as_ref()
                .map(|l| l.max_context_messages as usize)
                .unwrap_or(16),
            self.ctx
                .config
                .limits
                .as_ref()
                .map(|l| l.max_context_bytes as usize)
                .unwrap_or(64_000),
        )
    }

    /// Whether auto-compact is enabled (`[limits].auto_compact`, default on
    /// — including when the whole `[limits]` section is absent).
    fn auto_compact_enabled(&self) -> bool {
        self.ctx
            .config
            .limits
            .as_ref()
            .map(|l| l.auto_compact)
            .unwrap_or(true)
    }

    /// Compact the in-memory history, summarizing older messages through the
    /// `fast` model when one is available.
    ///
    /// The summary callback resolves to `None` (→ mechanical-concatenation
    /// fallback inside `compact`) whenever the `fast` alias is missing,
    /// resolution/provider construction fails, or the generate call errors —
    /// a degraded path, not an error: any config may legitimately not define
    /// a `fast` model. After compacting, the in-memory history (summarized)
    /// intentionally diverges from the persisted transcript (full): /resume
    /// restores the uncompressed history by design.
    ///
    /// When the summary call does happen, its usage is funneled back through
    /// an `Arc<Mutex<Option<Usage>>>` side channel (the callback's return
    /// type stays `Option<String>`) and credited to the session stats here.
    async fn run_compact(&mut self) -> CompactResult {
        // Resolve the fast alias up front so the closure below owns its plan
        // and borrows nothing from `self` (the history is borrowed &mut for
        // the duration of the compact call). The plan also carries the
        // fast model's pricing (P2-3) so the summary call bills the session
        // with the same CostSpec helper the runtime loop uses.
        let summary_plan: Option<(
            String,
            Box<dyn openslate_core::provider::ModelProvider>,
            openslate_core::runtime::CostSpec,
        )> = match openslate_core::model_config::resolve_model(&self.ctx.config, "fast") {
            Ok(resolved) => match build_provider_for_model(&self.ctx.config, "fast") {
                Ok(provider) => {
                    let pricing = resolved.cost_spec();
                    Some((resolved.model_id, provider, pricing))
                }
                Err(e) => {
                    tracing::debug!(
                        "no usable provider for 'fast' — compacting with mechanical fallback: {}",
                        e
                    );
                    None
                }
            },
            Err(e) => {
                tracing::debug!(
                    "no 'fast' model alias — compacting with mechanical fallback: {}",
                    e
                );
                None
            }
        };
        // Snapshot the pricing before the closure moves the plan.
        let summary_pricing = summary_plan
            .as_ref()
            .map(|(_, _, pricing)| *pricing)
            .unwrap_or_default();

        let usage_slot = Arc::new(Mutex::new(None::<Usage>));
        let slot = Arc::clone(&usage_slot);
        let (max_messages, max_bytes) = self.context_limits();

        let result = openslate_core::context_manager::compact(
            &mut self.history,
            None,
            max_messages,
            max_bytes,
            move |text: &str| {
                // Copy first: the returned future must not borrow from the
                // `&str` argument (nor from `messages`).
                let text = text.to_owned();
                async move {
                    let (model_id, provider, _pricing) = summary_plan?;
                    let (summary, usage) =
                        generate_summary(provider.as_ref(), &model_id, &text).await;
                    if let Some(u) = usage {
                        *slot.lock().expect("compact usage slot poisoned") = Some(u);
                    }
                    summary
                }
            },
        )
        .await;

        // Credit the summary call's tokens to the session stats (it is a
        // real model call the user paid for, even though it belongs to no
        // turn's run).
        if let Some(u) = usage_slot
            .lock()
            .expect("compact usage slot poisoned")
            .take()
        {
            self.credit_compact_usage(&u, summary_pricing);
        }

        result
    }

    /// Bill a compact summary call to the session (P2-3 口径): tokens AND
    /// cost — the cost priced with the fast model's CostSpec via the same
    /// helper the runtime uses. It lands in `total_cost_usd` (/status) and
    /// in `compact_cost_usd`, keeping the persisted run row's cost
    /// (`run_cost_usd()`) turn-only.
    fn credit_compact_usage(&mut self, usage: &Usage, pricing: openslate_core::runtime::CostSpec) {
        self.stats.total_input_tokens += usage.input_tokens as u64;
        self.stats.total_output_tokens += usage.output_tokens as u64;
        let cost = pricing.cost_of(usage);
        self.stats.total_cost_usd += cost;
        self.stats.compact_cost_usd += cost;
    }

    async fn handle_normal_input(&mut self, input: &str) -> Result<DispatchResult> {
        let user_message = Message {
            role: MessageRole::User,
            content: input.to_owned(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        };
        self.history.push(user_message.clone());

        // Every turn lands in the store: the user message is written
        // directly, and the runtime's message sink persists each assistant
        // message / tool result the moment it is produced.
        let session_run = self.current_run().await;
        if let Some(ref run) = session_run {
            if let Err(e) = run.recorder.write_message(&user_message).await {
                tracing::warn!("Failed to persist user message: {}", e);
            }
        }
        self.ctx.manager.message_sink = session_run
            .as_ref()
            .map(|r| r.recorder.clone() as Arc<dyn MessageSink>);

        // Auto-compact (P2-2): compress the history BEFORE the turn runs —
        // the model then sees the summarized context, and the just-pushed
        // user message survives as one of the kept recent messages. The
        // persisted transcript keeps the full history (intentional fork:
        // /resume restores the uncompressed version). No signal listener is
        // installed for the summary call: Ctrl-C during compaction gets
        // default SIGINT semantics (process exits; the run row stays
        // `running` and remains resumable), consistent with crash behavior.
        let (max_msgs, max_bytes) = self.context_limits();
        if self.auto_compact_enabled()
            && openslate_core::context_manager::needs_compact(&self.history, max_msgs, max_bytes, 0)
        {
            let result = self.run_compact().await;
            if !self.quiet {
                println!(
                    "上下文接近上限,已自动压缩历史: {} → {} 条消息",
                    result.messages_before, result.messages_after
                );
            }
        }

        let _agent = resolve_agent(&self.ctx.agent_tree, None)?;
        let model_alias = self.effective_model_alias();
        let provider = build_provider_for_model(&self.ctx.config, &model_alias)?;

        let run_id = session_run
            .as_ref()
            .map(|r| r.run_id.clone())
            .unwrap_or_else(RunManager::new_run_id);

        let start = Instant::now();
        let mut callback = SpinnerCallback::new(&model_alias, self.quiet);

        // Per-turn cancellation (Phase 4): Ctrl-C cancels the token, the
        // runtime's checkpoints observe it and the run returns
        // Ok(Interrupted) with the partial transcript — the session (and its
        // persisted run) survive. The signal listener lives only for the
        // turn; at the main prompt, rustyline's own Interrupted handling
        // (skip the input line) applies.
        let token = CancellationToken::new();
        self.cancel_slot.set(token.clone());
        let signal_token = token.clone();
        let ctrl_c = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal_token.cancel();
            }
        });
        let run_result = self
            .ctx
            .manager
            .execute_with_run_id(
                run_id,
                &*provider,
                &self.history,
                token,
                Some(&mut callback),
            )
            .await;
        ctrl_c.abort();
        let result = match run_result {
            Ok(r) => {
                // finish() MUST run on the cancellation path too — it tears
                // down the spinner renderer and restores the terminal.
                callback.finish();
                r
            }
            Err(e) => {
                let msg = e.to_string();
                callback.finish_with_error(&msg);
                // The turn failed but the session run stays `running` (and
                // the partial transcript is already persisted). Reload the
                // persisted transcript so in-memory history matches what a
                // later /resume would restore, instead of diverging from disk.
                if let (Some(run), Some(store)) = (&self.session_run, self.ctx.store.clone()) {
                    match RunRecorder::load_messages(&store, &run.run_id.0).await {
                        Ok(msgs) if !msgs.is_empty() => self.history = msgs,
                        Ok(_) => {}
                        Err(load_err) => {
                            tracing::warn!("Failed to reload persisted history: {}", load_err);
                        }
                    }
                }
                return Err(anyhow::anyhow!("Agent execution failed: {}", e));
            }
        };
        let _elapsed = start.elapsed();

        self.stats.total_steps += result.total_steps;
        self.stats.total_input_tokens += result.total_input_tokens as u64;
        self.stats.total_output_tokens += result.total_output_tokens as u64;
        self.stats.total_cost_usd += result.total_cost_usd;
        self.stats.turns += 1;

        if self.verbose {
            println!(
                "[verbose] steps={}, tokens_in={}, tokens_out={}, elapsed={:?}",
                result.total_steps, result.total_input_tokens, result.total_output_tokens, _elapsed
            );
        }

        let final_message = result
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, MessageRole::Assistant))
            .map(|m| m.content.clone())
            .unwrap_or_else(|| "(no assistant response)".to_owned());

        // Full-transcript replacement (Phase 3): keep the ENTIRE message
        // list the model saw — tool_calls and tool results included — not
        // just the final assistant text. Multi-turn quality and /resume
        // consistency both depend on the tool transcript surviving. A
        // cancelled turn keeps its partial transcript the same way.
        self.history = result.messages;

        if result.status == RunStatus::Interrupted {
            // Ctrl-C: brief notice, then back to the prompt — the session
            // stays alive with its history (and the persisted run) intact.
            if !self.quiet {
                println!();
                println!(
                    "已取消:本轮在第 {} 步中断,已完成部分已保留在会话中",
                    result.total_steps
                );
                if self.session_run.is_some() {
                    println!("(部分输出已持久化,之后可用 /resume 恢复;直接继续输入即可接着聊)");
                }
            }
            return Ok(DispatchResult::Continue);
        }

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

    /// The /status cost cell (P2-3): the session's accumulated cost
    /// (turns + compact summaries), or "pricing not configured" when the
    /// effective model carries no prices and nothing was ever billed.
    /// Same decision rule as the `run` end line (cmd/run.rs).
    fn format_session_cost(&self, model_alias: &str) -> String {
        let configured = crate::cmd::run::model_pricing_configured(&self.ctx.config, model_alias);
        if !configured && self.stats.total_cost_usd == 0.0 {
            "pricing not configured".to_owned()
        } else {
            format!("${:.4}", self.stats.total_cost_usd)
        }
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
            Some(ref s) => s.clone(),
            None => {
                println!("Store not available");
                return Ok(DispatchResult::Continue);
            }
        };

        // Pick the most recent resumable run: interrupted / cancelled /
        // crashed (`running`) one-shot runs and completed REPL sessions
        // alike — failed runs are skipped.
        let run = match store.get_last_resumable_run().await {
            Ok(Some(run)) => run,
            Ok(None) => {
                println!("No resumable runs found");
                return Ok(DispatchResult::Continue);
            }
            Err(e) => {
                println!("Store query failed: {}", e);
                return Ok(DispatchResult::Continue);
            }
        };

        let messages = match RunRecorder::load_messages(&store, &run.id).await {
            Ok(msgs) => msgs,
            Err(e) => {
                println!("Failed to load messages for run {}: {}", run.id, e);
                return Ok(DispatchResult::Continue);
            }
        };
        if messages.is_empty() {
            println!("Run {} has no persisted messages", run.id);
            return Ok(DispatchResult::Continue);
        }

        let root_agent_id = self.ctx.agent_tree.get_root().id.0.clone();
        match RunRecorder::resume(store, RunId(run.id.clone()), &root_agent_id).await {
            Ok(recorder) => {
                // Rebuild the conversation context from the persisted
                // transcript and adopt the run for this session: subsequent
                // turns continue its message sequence.
                let restored = messages.len();
                // Close the previous session run before adopting the resumed
                // one — otherwise it stays "running" forever and shadows later
                // no-arg /resume lookups (transcript stays resumable either way).
                if let Some(run) = self.session_run.take() {
                    if let Err(e) = run
                        .recorder
                        .finish("interrupted", None, self.stats.run_cost_usd())
                        .await
                    {
                        tracing::warn!("Failed to persist previous session run: {}", e);
                    }
                }
                self.history = messages;
                self.session_run = Some(SessionRun {
                    run_id: RunId(run.id.clone()),
                    recorder: Arc::new(recorder),
                });
                let short_id = run.id.get(..8).unwrap_or(&run.id);
                println!(
                    "Resumed run {} (status: {}, {} messages restored) — continue chatting",
                    short_id, run.status, restored
                );
            }
            Err(e) => {
                println!("Failed to adopt run {}: {}", run.id, e);
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
                    "{:<4} {:<20} {:<14} {:<12} {:<16} {:<10} Input",
                    "#", "Run ID", "Status", "Title", "Started", "Cost"
                );
                println!("{}", "-".repeat(100));
                for (i, run) in runs.iter().enumerate() {
                    let title = run.title.as_deref().unwrap_or("-");
                    let input_preview = truncate_json(&run.input_json, 40);
                    println!(
                        "{:<4} {:<20} {:<14} {:<12} {:<16} {:<10} {}",
                        i + 1,
                        truncate_str(&run.id, 18),
                        truncate_str(&run.status, 12),
                        truncate_str(title, 10),
                        run.started_at,
                        format!("${:.4}", run.cost_usd),
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

    #[cfg(test)]
    pub fn session_run_id(&self) -> Option<&str> {
        self.session_run.as_ref().map(|r| r.run_id.0.as_str())
    }

    #[cfg(test)]
    pub fn approval_policy(&self) -> openslate_core::approval::ApprovalPolicy {
        self.ctx.manager.approval.policy().clone()
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
        let agent_md = "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - read_file\n---\nYou are the root agent.\n";
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
        fs::write(
            agents_dir.join("researcher.md"),
            "---\nid: researcher\nname: Researcher\nmodel: fast\n---\nYou are a researcher.\n",
        )
        .expect("write researcher.md");
        fs::write(
            agents_dir.join("writer.md"),
            "---\nid: writer\nname: Writer\nmodel: fast\n---\nYou are a writer.\n",
        )
        .expect("write writer.md");
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
        let agent_md = "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - read_file\n---\nYou are the root agent.\n";
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
            openslate_core::tool::ToolRegistry::new(),
            openslate_core::skills::SkillsCatalog::default(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            manager,
            skills: openslate_core::skills::SkillsCatalog::default(),
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
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
            openslate_core::tool::ToolRegistry::new(),
            openslate_core::skills::SkillsCatalog::default(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            manager,
            skills: openslate_core::skills::SkillsCatalog::default(),
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
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
            openslate_core::tool::ToolRegistry::new(),
            openslate_core::skills::SkillsCatalog::default(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            manager,
            skills: openslate_core::skills::SkillsCatalog::default(),
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
            mcp_connections: openslate_core::mcp::McpConnectionGuard::default(),
        };

        ReplSession::new(ctx, "default".into(), true).unwrap()
    }

    fn make_ctx_helper(
        tmp: &tempfile::TempDir,
    ) -> (
        openslate_core::config::OpenSlateConfig,
        openslate_core::config::AgentsConfig,
        openslate_core::agent_tree::AgentTree,
        openslate_core::run_manager::RunManager,
    ) {
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents")).unwrap();
        let agent_tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let manager = openslate_core::run_manager::RunManager::new(
            config.clone(),
            agent_tree.clone(),
            openslate_core::tool::ToolRegistry::new(),
            openslate_core::skills::SkillsCatalog::default(),
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
            skills: openslate_core::skills::SkillsCatalog::default(),
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
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
            skills: openslate_core::skills::SkillsCatalog::default(),
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
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
            skills: openslate_core::skills::SkillsCatalog::default(),
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
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
        assert!(session.history().is_empty(), "/clear should clear history");
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
            session.stats().total_steps,
            0,
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
            openslate_core::tool::ToolRegistry::new(),
            openslate_core::skills::SkillsCatalog::default(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: Some(store),
            agent_tree,
            manager,
            skills: openslate_core::skills::SkillsCatalog::default(),
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
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
            openslate_core::tool::ToolRegistry::new(),
            openslate_core::skills::SkillsCatalog::default(),
        );

        let ctx = wiring::AppContext {
            config,
            agents,
            store: Some(store),
            agent_tree,
            manager,
            skills: openslate_core::skills::SkillsCatalog::default(),
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
            mcp_connections: openslate_core::mcp::McpConnectionGuard::default(),
        };

        let mut session = ReplSession::new(ctx, "default".into(), true).unwrap();
        let result = session.handle_sessions().await.unwrap();
        assert_eq!(result, DispatchResult::Continue);
    }

    // ── /resume: real restore (Phase 3) ────────────────────────────────

    use openslate_core::types::{ToolCall, ToolCallId};
    use openslate_store_sqlite::recorder::RunRecorder;

    async fn make_store() -> openslate_store_sqlite::store::SqliteStore {
        let store = openslate_store_sqlite::store::SqliteStore::new_in_memory()
            .await
            .expect("store");
        store.run_migrations().await.expect("migrations");
        store
    }

    async fn make_session_with_store(
        store: openslate_store_sqlite::store::SqliteStore,
    ) -> ReplSession {
        let tmp = temp_project();
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents")).unwrap();
        let agent_tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let manager = openslate_core::run_manager::RunManager::new(
            config.clone(),
            agent_tree.clone(),
            openslate_core::tool::ToolRegistry::new(),
            openslate_core::skills::SkillsCatalog::default(),
        );
        let ctx = wiring::AppContext {
            config,
            agents,
            store: Some(store),
            agent_tree,
            manager,
            skills: openslate_core::skills::SkillsCatalog::default(),
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
            mcp_connections: openslate_core::mcp::McpConnectionGuard::default(),
        };
        ReplSession::new(ctx, "default".into(), true).unwrap()
    }

    fn assistant_tool_call_msg() -> Message {
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: ToolCallId("tc-1".into()),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "ls"}),
            }]),
        }
    }

    fn tool_result_msg() -> Message {
        Message {
            role: MessageRole::Tool,
            content: "a.txt".into(),
            tool_call_id: Some(ToolCallId("tc-1".into())),
            name: Some("shell".into()),
            tool_calls: None,
        }
    }

    #[tokio::test]
    async fn test_resume_restores_history_and_adopts_run() {
        let store = make_store().await;
        // Seed an interrupted run with a partial tool transcript.
        let rec = RunRecorder::begin(
            store.clone(),
            openslate_core::types::RunId("seeded-run".into()),
            "root",
            None,
            "{}",
        )
        .await
        .expect("begin");
        for m in [
            Message {
                role: MessageRole::User,
                content: "list files".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
            assistant_tool_call_msg(),
            tool_result_msg(),
        ] {
            rec.write_message(&m).await.expect("seed message");
        }
        rec.finish("interrupted", None, 0.0).await.expect("finish");

        let mut session = make_session_with_store(store.clone()).await;
        assert!(session.history().is_empty());
        assert!(session.session_run_id().is_none());

        let result = session.handle_resume().await.unwrap();
        assert_eq!(result, DispatchResult::Continue);

        // History rebuilt from the persisted transcript, tool pairing intact.
        assert_eq!(session.history().len(), 3);
        assert_eq!(session.history()[0].content, "list files");
        assert_eq!(
            session.history()[1]
                .tool_calls
                .as_ref()
                .expect("tool_calls restored")
                .len(),
            1
        );
        assert_eq!(session.history()[2].role, MessageRole::Tool);

        // The session adopted the run: subsequent turns continue its seq.
        assert_eq!(session.session_run_id(), Some("seeded-run"));

        // Writing the next user message continues the same sequence (no
        // clobbering, monotonic seq).
        let run = session.current_run().await.expect("session run");
        assert_eq!(run.run_id.0, "seeded-run");
        run.recorder
            .write_message(&Message {
                role: MessageRole::User,
                content: "next turn".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            })
            .await
            .expect("write after adoption");
        assert_eq!(store.max_message_seq("seeded-run").await.unwrap(), 4);
    }

    #[tokio::test]
    async fn test_resume_skips_failed_runs() {
        let store = make_store().await;
        let rec = RunRecorder::begin(
            store.clone(),
            openslate_core::types::RunId("failed-run".into()),
            "root",
            None,
            "{}",
        )
        .await
        .expect("begin");
        rec.write_message(&Message {
            role: MessageRole::User,
            content: "boom".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        })
        .await
        .expect("write");
        rec.finish("failed", None, 0.0).await.expect("finish");

        let mut session = make_session_with_store(store).await;
        let result = session.handle_resume().await.unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert!(
            session.history().is_empty(),
            "a failed run must not be restored"
        );
        assert!(session.session_run_id().is_none());
    }

    #[tokio::test]
    async fn test_new_closes_session_run_and_starts_fresh() {
        let store = make_store().await;
        let rec = RunRecorder::begin(
            store.clone(),
            openslate_core::types::RunId("old-run".into()),
            "root",
            None,
            "{}",
        )
        .await
        .expect("begin");
        rec.write_message(&Message {
            role: MessageRole::User,
            content: "old".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        })
        .await
        .expect("write");
        drop(rec);

        let mut session = make_session_with_store(store.clone()).await;
        session.handle_resume().await.unwrap();
        assert_eq!(session.session_run_id(), Some("old-run"));

        // /new closes the old run and detaches: next turn opens a fresh one.
        let result = session.handle_slash_command("/new").await.unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert!(session.history().is_empty());
        assert!(session.session_run_id().is_none());

        let old = store.get_run("old-run").await.unwrap().expect("run");
        assert_eq!(old.status, "completed", "/new closes the backing run");

        // The next opened run is a different id.
        let run = session.current_run().await.expect("new run");
        assert_ne!(run.run_id.0, "old-run");
    }

    #[tokio::test]
    async fn test_new_persists_run_cost_snapshot_before_stats_reset() {
        let store = make_store().await;
        let rec = RunRecorder::begin(
            store.clone(),
            openslate_core::types::RunId("cost-run".into()),
            "root",
            None,
            "{}",
        )
        .await
        .expect("begin");
        rec.write_message(&Message {
            role: MessageRole::User,
            content: "spent".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        })
        .await
        .expect("write");
        drop(rec);

        let mut session = make_session_with_store(store.clone()).await;
        session.handle_resume().await.unwrap();
        assert_eq!(session.session_run_id(), Some("cost-run"));

        // A session that accrued run cost plus a compact-summary cost: the
        // run row must persist the run-only portion, snapshotted BEFORE the
        // stats reset inside /new. Regression: it used to read run_cost_usd()
        // after the reset and always persisted 0.0.
        session.stats.total_cost_usd = 0.0125;
        session.stats.compact_cost_usd = 0.0025;

        session.handle_slash_command("/new").await.unwrap();

        let run = store.get_run("cost-run").await.unwrap().expect("run");
        assert_eq!(run.status, "completed");
        assert!(
            (run.cost_usd - 0.0100).abs() < 1e-9,
            "/new must persist the pre-reset run-cost snapshot, got {}",
            run.cost_usd
        );
    }

    // ── /compact + auto-compact (P2-2) ─────────────────────────────────

    use async_trait::async_trait;
    use openslate_core::error::ProviderError;
    use openslate_core::provider::{GenerateRequest, ModelProvider};
    use openslate_core::types::ModelResponse;

    /// Temp project with a tiny message limit (max_context_messages = 4) so
    /// the 80%-threshold is easy to cross. No `auto_compact` field → the
    /// default (on) applies.
    fn temp_project_small_context() -> tempfile::TempDir {
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
max_context_messages = 4
max_context_bytes = 64000
"#;
        let agents_dir = openslate_dir.join("agents");
        fs::create_dir(&agents_dir).expect("create agents dir");
        let agent_md = "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - read_file\n---\nYou are the root agent.\n";
        fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        fs::write(agents_dir.join("root.md"), agent_md).expect("write root.md");
        tmp
    }

    /// Same tiny limits, but auto-compact explicitly disabled.
    fn temp_project_auto_compact_off() -> tempfile::TempDir {
        let tmp = temp_project_small_context();
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let toml = fs::read_to_string(&config_path).expect("read toml");
        let toml = format!("{}auto_compact = false\n", toml);
        fs::write(&config_path, toml).expect("write toml");
        tmp
    }

    /// Project whose `fast` model carries pricing (P2-3): $0.4/M in,
    /// $1.2/M out — so compact summary calls bill the session.
    fn temp_project_priced_fast() -> tempfile::TempDir {
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
input_price_per_mtok = 0.4
output_price_per_mtok = 1.2

[limits]
max_steps = 10
max_depth = 3
max_tool_calls = 20
"#;
        let agents_dir = openslate_dir.join("agents");
        fs::create_dir(&agents_dir).expect("create agents dir");
        fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        fs::write(
            agents_dir.join("root.md"),
            "---\nid: root\nname: Root Agent\nmodel: main\n---\nYou are the root agent.\n",
        )
        .expect("write root.md");
        tmp
    }

    fn make_session_from_dir(tmp: &tempfile::TempDir) -> ReplSession {
        let config = wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = wiring::load_agents(&tmp.path().join(".openslate/agents")).unwrap();
        let agent_tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let manager = openslate_core::run_manager::RunManager::new(
            config.clone(),
            agent_tree.clone(),
            openslate_core::tool::ToolRegistry::new(),
            openslate_core::skills::SkillsCatalog::default(),
        );
        let ctx = wiring::AppContext {
            config,
            agents,
            store: None,
            agent_tree,
            manager,
            skills: openslate_core::skills::SkillsCatalog::default(),
            config_path: tmp.path().join(".openslate/openslate.toml"),
            agents_path: tmp.path().join(".openslate/agents"),
            mcp_connections: openslate_core::mcp::McpConnectionGuard::default(),
        };
        ReplSession::new(ctx, "default".into(), true).unwrap()
    }

    fn seed_history(session: &mut ReplSession) {
        for i in 0..3 {
            session.history.push(Message {
                role: MessageRole::User,
                content: format!("user {}", i),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            });
            session.history.push(Message {
                role: MessageRole::Assistant,
                content: format!("assistant {}", i),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            });
        }
    }

    /// Scripted provider standing in for the `fast` model in
    /// `generate_summary` unit tests (the real REPL path builds the
    /// provider from config; these tests inject responses directly).
    struct ScriptedSummary {
        response: Result<ModelResponse, ProviderError>,
    }

    #[async_trait]
    impl ModelProvider for ScriptedSummary {
        async fn generate(
            &self,
            _request: GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            // ProviderError is not Clone: rebuild it instead of cloning.
            match &self.response {
                Ok(resp) => Ok(resp.clone()),
                Err(_) => Err(ProviderError::ServerError(500)),
            }
        }
        fn provider_name(&self) -> &str {
            "scripted-summary"
        }
    }

    #[test]
    fn test_compact_command_empty_history_is_noop() {
        let mut session = make_session();
        let result = block_on(session.handle_slash_command("/compact")).unwrap();
        assert_eq!(result, DispatchResult::Continue);
        assert!(session.history().is_empty(), "empty history stays empty");
    }

    #[test]
    fn test_compact_command_compresses_history_mechanical_fallback() {
        // temp_project defines no `fast` alias → the summarize callback
        // resolves to None and compact falls back to mechanical
        // concatenation of the older messages.
        let mut session = make_session();
        seed_history(&mut session);
        assert_eq!(session.history().len(), 6);

        let result = block_on(session.handle_slash_command("/compact")).unwrap();
        assert_eq!(result, DispatchResult::Continue);

        assert_eq!(session.history().len(), 3);
        assert_eq!(session.history()[0].role, MessageRole::System);
        assert_eq!(session.history()[0].name.as_deref(), Some("compact"));
        assert!(
            session.history()[0].content.contains("User: user 0"),
            "mechanical fallback text expected: {}",
            session.history()[0].content
        );
        // KEEP_RECENT_COUNT most recent messages survive verbatim.
        assert_eq!(session.history()[1].content, "user 2");
        assert_eq!(session.history()[2].content, "assistant 2");
    }

    #[test]
    fn test_auto_compact_triggers_when_over_threshold() {
        // Default-on (no auto_compact field) + max_context_messages = 4:
        // 7 messages after the turn's push → 7/4 > 0.8 → auto-compact
        // BEFORE the turn executes (the turn itself still fails on the
        // missing API key, but compaction has already happened).
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let tmp = temp_project_small_context();
        let mut session = make_session_from_dir(&tmp);
        seed_history(&mut session);

        let result = rt.block_on(session.dispatch("hi"));
        assert!(result.is_err(), "turn fails without API key");

        assert_eq!(session.history().len(), 3);
        assert_eq!(session.history()[0].role, MessageRole::System);
        assert_eq!(session.history()[0].name.as_deref(), Some("compact"));
        // The just-pushed user message survives as a kept recent message.
        assert_eq!(session.history()[2].content, "hi");
    }

    #[test]
    fn test_auto_compact_disabled_via_config() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let tmp = temp_project_auto_compact_off();
        let mut session = make_session_from_dir(&tmp);
        seed_history(&mut session);

        let result = rt.block_on(session.dispatch("hi"));
        assert!(result.is_err(), "turn fails without API key");

        // No compaction: raw 6 seeded + 1 pushed message remain.
        assert_eq!(session.history().len(), 7);
        assert!(
            !session
                .history()
                .iter()
                .any(|m| m.name.as_deref() == Some("compact")),
            "auto_compact = false must not compact"
        );
    }

    #[test]
    fn test_auto_compact_not_triggered_below_threshold() {
        // Default limits (max_context_messages = 16): 6 messages stay well
        // under the 80% threshold — no compaction even though auto_compact
        // defaults to on.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let mut session = make_session();
        seed_history(&mut session);

        let result = rt.block_on(session.dispatch("hi"));
        assert!(result.is_err(), "turn fails without API key");
        assert_eq!(session.history().len(), 7);
        assert_eq!(session.history()[0].content, "user 0");
    }

    #[tokio::test]
    async fn test_generate_summary_success_returns_content_and_usage() {
        let provider = ScriptedSummary {
            response: Ok(ModelResponse {
                content: Some("kept decisions, paths, todos".to_owned()),
                tool_calls: Vec::new(),
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                }),
                finish_reason: Some("stop".into()),
            }),
        };
        let (summary, usage) = generate_summary(&provider, "fast-model", "User: hello").await;
        assert_eq!(summary.as_deref(), Some("kept decisions, paths, todos"));
        let usage = usage.expect("usage recorded");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
    }

    // ── Compact usage → cost → session stats chain (P2-3) ────────────────

    #[test]
    fn test_compact_usage_costs_flow_into_session_stats() {
        // Fast model priced at $0.4/M in + $1.2/M out. One scripted summary
        // call with 2000 in / 500 out usage costs
        // 2000*0.4e-6 + 500*1.2e-6 = 0.0014 — and must bill the SESSION
        // (/status) without touching the persisted run row's cost.
        let tmp = temp_project_priced_fast();
        let mut session = make_session_from_dir(&tmp);

        let provider = ScriptedSummary {
            response: Ok(ModelResponse {
                content: Some("kept decisions".to_owned()),
                tool_calls: Vec::new(),
                usage: Some(Usage {
                    input_tokens: 2_000,
                    output_tokens: 500,
                }),
                finish_reason: Some("stop".into()),
            }),
        };
        let (_summary, usage) = block_on(generate_summary(&provider, "fast-model-v1", "User: hi"));
        let usage = usage.expect("scripted usage");
        let pricing = openslate_core::model_config::resolve_model(&session.ctx.config, "fast")
            .expect("fast resolves")
            .cost_spec();
        assert!(pricing.is_configured(), "fixture fast model is priced");

        session.credit_compact_usage(&usage, pricing);

        assert_eq!(session.stats().total_input_tokens, 2_000);
        assert_eq!(session.stats().total_output_tokens, 500);
        assert!(
            (session.stats().total_cost_usd - 0.0014f64).abs() < 1e-12,
            "usage must price into session cost, got {}",
            session.stats().total_cost_usd
        );
        assert!(
            (session.stats().compact_cost_usd - 0.0014f64).abs() < 1e-12,
            "compact spend is tracked as its own bucket"
        );
        assert_eq!(
            session.stats().run_cost_usd(),
            0.0,
            "compact cost must NOT bill the persisted run row"
        );
        // /status shows the accumulated dollar figure for a priced model.
        assert_eq!(session.format_session_cost("fast"), "$0.0014");
    }

    #[test]
    fn test_status_cost_shows_not_configured_without_pricing() {
        // temp_project defines an unpriced main → the /status cost cell
        // says so instead of a misleading $0.0000.
        let session = make_session();
        assert_eq!(
            session.format_session_cost("main"),
            "pricing not configured"
        );
    }

    #[tokio::test]
    async fn test_generate_summary_provider_error_falls_back() {
        let provider = ScriptedSummary {
            response: Err(ProviderError::ServerError(500)),
        };
        let (summary, usage) = generate_summary(&provider, "fast-model", "User: hello").await;
        assert!(summary.is_none(), "provider error ⇒ fallback");
        assert!(usage.is_none(), "no usage without a completed call");
    }

    #[tokio::test]
    async fn test_generate_summary_empty_content_falls_back() {
        let provider = ScriptedSummary {
            response: Ok(ModelResponse {
                content: Some("   ".to_owned()),
                tool_calls: Vec::new(),
                usage: Some(Usage {
                    input_tokens: 3,
                    output_tokens: 0,
                }),
                finish_reason: Some("stop".into()),
            }),
        };
        let (summary, usage) = generate_summary(&provider, "fast-model", "User: hello").await;
        assert!(summary.is_none(), "blank reply is treated as failure");
        // The call did happen (and cost tokens) — usage is still reported.
        assert!(usage.is_some());
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
    fn test_truncate_str_cjk_does_not_panic() {
        // Multi-byte chars must not panic on slice boundaries; truncate to a
        // byte budget that lands mid-character.
        let result = truncate_str("你好世界这是一个很长的字符串", 10);
        // 10 bytes fit 3 CJK chars (9 bytes); the 4th would straddle the bound.
        assert_eq!(result, "你好世…");
        assert!(result.len() <= 10 + "…".len());
    }

    #[test]
    fn test_truncate_str_multibyte_exact_boundary() {
        // Budget lands exactly on a char boundary — no character lost.
        let result = truncate_str("你好世界", 8);
        assert_eq!(result, "你好…");
    }

    #[test]
    fn test_truncate_json_strips_quotes() {
        assert_eq!(truncate_json(r#""hello""#, 20), "hello");
    }

    #[test]
    fn test_truncate_json_plain() {
        assert_eq!(truncate_json("hello world", 20), "hello world");
    }

    // ── Interactive approval callback (Phase 1) ──────────────────────────

    use openslate_core::approval::RiskLevel;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn approval_req(tool: &str, agent: &str, risk: RiskLevel) -> ApprovalRequest {
        ApprovalRequest {
            tool_name: tool.to_owned(),
            arguments: serde_json::json!({"cmd": "ls"}),
            agent_id: agent.to_owned(),
            risk_level: risk,
        }
    }

    /// Answers pulled one-by-one from a script; every read is counted so
    /// tests can prove whether the callback consulted the prompt at all.
    /// A read past the script's end yields "n" (deny).
    fn scripted_answers(script: &[&str]) -> (PromptAnswerReader, Arc<AtomicUsize>) {
        let script: Vec<String> = script.iter().map(|s| (*s).to_owned()).collect();
        let reads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&reads);
        let reader: PromptAnswerReader = Box::new(move || {
            let idx = counter.fetch_add(1, Ordering::SeqCst);
            match script.get(idx) {
                Some(answer) => PromptAnswer::Line(answer.clone()),
                None => PromptAnswer::Line("n".to_owned()),
            }
        });
        (reader, reads)
    }

    #[test]
    fn interactive_approval_prompts_are_serialized() {
        // P2-1: with parallel tool calls, two approval requests can race.
        // The prompt mutex wraps the WHOLE decide (header print + answer
        // read), so one request's header/read pair completes before the
        // next begins. Proven by concurrent readers: the reader bodies
        // never overlap (max active == 1) even with four threads
        // prompting at once.
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let reader: PromptAnswerReader = {
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            let reads = Arc::clone(&reads);
            Box::new(move || {
                reads.fetch_add(1, Ordering::SeqCst);
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(30));
                active.fetch_sub(1, Ordering::SeqCst);
                PromptAnswer::Line("y".to_owned())
            })
        };
        let cb = Arc::new(InteractiveApproval::new(reader, CancelSlot::new()));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let cb = Arc::clone(&cb);
            handles.push(std::thread::spawn(move || {
                assert_eq!(
                    cb.decide(&approval_req("shell", "root", RiskLevel::High)),
                    ApprovalDecision::Approved
                );
            }));
        }
        for h in handles {
            h.join().expect("approval thread panicked");
        }

        assert_eq!(reads.load(Ordering::SeqCst), 4, "every request prompted");
        assert_eq!(
            max_active.load(Ordering::SeqCst),
            1,
            "answer reads must never overlap — the prompt mutex serializes whole decide"
        );
    }

    #[test]
    fn interactive_approval_yes_approves_each_call() {
        // 'y' approves only the current call: the next call prompts again.
        let (reader, reads) = scripted_answers(&["y", "y"]);
        let cb = InteractiveApproval::new(reader, CancelSlot::new());
        assert_eq!(
            cb.decide(&approval_req("shell", "root", RiskLevel::High)),
            ApprovalDecision::Approved
        );
        assert_eq!(
            cb.decide(&approval_req("shell", "root", RiskLevel::High)),
            ApprovalDecision::Approved
        );
        assert_eq!(reads.load(Ordering::SeqCst), 2, "both calls prompted");
    }

    #[test]
    fn interactive_approval_always_downgrades_for_the_session() {
        // 'a' approves and allowlists the tool: the second call to the SAME
        // tool is approved WITHOUT reading another answer (session
        // downgrade); a different tool still prompts.
        let (reader, reads) = scripted_answers(&["a", "y"]);
        let cb = InteractiveApproval::new(reader, CancelSlot::new());
        assert_eq!(
            cb.decide(&approval_req("shell", "root", RiskLevel::High)),
            ApprovalDecision::Approved
        );
        assert_eq!(
            cb.decide(&approval_req("shell", "root", RiskLevel::High)),
            ApprovalDecision::Approved
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "second shell call must not prompt"
        );
        assert_eq!(
            cb.decide(&approval_req("write_file", "root", RiskLevel::High)),
            ApprovalDecision::Approved
        );
        assert_eq!(reads.load(Ordering::SeqCst), 2, "other tool prompts");
    }

    #[test]
    fn interactive_approval_no_denies() {
        let (reader, _reads) = scripted_answers(&["n"]);
        let cb = InteractiveApproval::new(reader, CancelSlot::new());
        match cb.decide(&approval_req("shell", "root", RiskLevel::High)) {
            ApprovalDecision::Denied(reason) => {
                assert!(reason.contains("拒绝"), "reason: {reason}")
            }
            other => panic!("expected denial, got {other:?}"),
        }
    }

    #[test]
    fn interactive_approval_invalid_answer_reprompts() {
        // Garbage input asks again; the following 'y' approves.
        let (reader, reads) = scripted_answers(&["maybe", "y"]);
        let cb = InteractiveApproval::new(reader, CancelSlot::new());
        assert_eq!(
            cb.decide(&approval_req("run_code", "root", RiskLevel::High)),
            ApprovalDecision::Approved
        );
        assert_eq!(reads.load(Ordering::SeqCst), 2, "re-prompted once");
    }

    #[test]
    fn interactive_approval_ctrl_c_denies_current_call() {
        let reader: PromptAnswerReader = Box::new(|| PromptAnswer::Interrupted);
        let cb = InteractiveApproval::new(reader, CancelSlot::new());
        match cb.decide(&approval_req("shell", "root", RiskLevel::High)) {
            ApprovalDecision::Denied(reason) => {
                assert!(reason.contains("Ctrl-C"), "reason: {reason}")
            }
            other => panic!("expected denial, got {other:?}"),
        }
    }

    #[test]
    fn interactive_approval_ctrl_c_cancels_current_run() {
        // Phase 4: Ctrl-C at the approval prompt cancels the current turn's
        // token (the run stops at its next checkpoint) — an upgrade from the
        // Phase 1 "deny this call only" behavior. The call is still denied
        // so the model loop unwinds immediately.
        let slot = CancelSlot::new();
        let token = CancellationToken::new();
        slot.set(token.clone());
        assert!(!slot.current().is_cancelled());
        let reader: PromptAnswerReader = Box::new(|| PromptAnswer::Interrupted);
        let cb = InteractiveApproval::new(reader, slot.clone());
        match cb.decide(&approval_req("shell", "root", RiskLevel::High)) {
            ApprovalDecision::Denied(reason) => {
                assert!(reason.contains("Ctrl-C"), "reason: {reason}");
                assert!(
                    reason.contains("取消") && !reason.contains("拒绝本次调用"),
                    "must state the run (not just the call) is cancelled: {reason}"
                );
            }
            other => panic!("expected denial, got {other:?}"),
        }
        assert!(
            slot.current().is_cancelled(),
            "the current turn's token must be cancelled"
        );
    }

    #[test]
    fn cancel_slot_swaps_tokens_per_turn() {
        let slot = CancelSlot::new();
        let first = CancellationToken::new();
        slot.set(first.clone());
        first.cancel();
        assert!(slot.current().is_cancelled(), "first turn cancelled");

        // A new turn installs a fresh token; the old cancellation does not
        // leak into it (CancellationToken cannot be reset, hence the slot).
        let second = CancellationToken::new();
        slot.set(second.clone());
        assert!(!slot.current().is_cancelled(), "fresh turn not cancelled");
        slot.cancel();
        assert!(slot.current().is_cancelled());
    }

    #[test]
    fn interactive_approval_read_error_denies_current_call() {
        let reader: PromptAnswerReader = Box::new(|| PromptAnswer::ReadError);
        let cb = InteractiveApproval::new(reader, CancelSlot::new());
        assert!(matches!(
            cb.decide(&approval_req("shell", "root", RiskLevel::High)),
            ApprovalDecision::Denied(_)
        ));
    }

    #[test]
    fn repl_session_without_approval_section_derives_shell_run_code_default() {
        // temp_project's config has no [approval] section → the locked REPL
        // default auto_except(["shell", "run_code"]) applies.
        let session = make_session();
        assert_eq!(
            session.approval_policy(),
            openslate_core::approval::ApprovalPolicy::AutoExcept(vec![
                "shell".to_owned(),
                "run_code".to_owned()
            ])
        );
    }
}
