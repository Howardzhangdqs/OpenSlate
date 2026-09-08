//! AgentRunner — recursive child-agent orchestrator.
//!
//! Implements [`ToolExecutor`] so it can be handed to [`execute_run`] as the
//! tool executor. When the model emits a `call_agent` tool call, the runner
//! intercepts it and recursively runs the child agent. Non-`call_agent` tools
//! are delegated to the shared [`ToolRegistry`].
//!
//! Design rationale: [`execute_run`] is NOT modified. The delegation seam is
//! the [`ToolExecutor`] trait, which `execute_run` already invokes for every
//! tool call (`runtime.rs:execute_tool_safely`). Passing an `AgentRunner` as
//! that executor makes recursion happen naturally without touching the
//! single-agent loop. See `.slim/deepwork/subagent-recursive-delegation.md`.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::future::BoxFuture;

use crate::agent_tree::AgentTree;
use crate::callable::child_agent_definitions;
use crate::config::OpenSlateConfig;
use crate::context::{build_child_context, ContextIsolationConfig};
use crate::error::OpenSlateError;
use crate::execution::{ExecutionStatus, ExecutionTree};
use crate::model_config::resolve_model;
use crate::provider::{ModelProvider, ProgressCallback, ToolDefinition};
use crate::runtime::{check_limits, execute_run, RunConfig, RunResult, RuntimeLimits};
use crate::skills::SkillsCatalog;
use crate::tool::{ToolExecutor, ToolRegistry};
use crate::types::*;

/// Snapshot of the currently-executing agent pushed onto the runner's call
/// stack. `run_agent`/`run_root` push a frame on entry and pop it on exit
/// (via [`FrameGuard`]); [`handle_call_agent`](AgentRunner::handle_call_agent)
/// reads the top frame to learn who is calling and what context to inherit.
#[derive(Debug, Clone)]
struct CallerFrame {
    agent_id: AgentId,
    exec_id: ExecutionNodeId,
    depth: u32,
    /// Messages captured at frame entry — the parent-context source for
    /// children. This is a snapshot of the layer's *initial* messages; it
    /// does NOT reflect real-time conversation updates within the layer, so
    /// multi-level delegation passes each layer's initial context (not its
    /// current state) to its children. Accepted trade-off to keep
    /// `execute_run` unmodified.
    messages_snapshot: Vec<Message>,
}

/// Recursive child-agent orchestrator for a single run.
///
/// Constructed per-run on the stack of [`crate::run_manager::RunManager`] and
/// borrowed (`&self`) for the run's lifetime. Interior mutability (`Mutex` /
/// `Atomic*`) is required because [`ToolExecutor::execute`] takes `&self`
/// while the runner must track a growing execution tree, a call stack, and
/// token / call counters across recursion layers.
pub struct AgentRunner<'a> {
    provider: &'a dyn ModelProvider,
    agent_tree: &'a AgentTree,
    tool_registry: &'a ToolRegistry,
    config: &'a OpenSlateConfig,
    limits: RuntimeLimits,
    context_config: ContextIsolationConfig,
    run_id: RunId,
    /// Rendered tier-1 skills catalog section (None = no skills / nothing to
    /// inject), appended to every agent's system prompt.
    skills_section: Option<String>,

    // Interior-mutable shared state across the run and recursion layers.
    execution_tree: Mutex<ExecutionTree>,
    caller_stack: Mutex<Vec<CallerFrame>>,
    child_call_count: AtomicU32,
    total_input_tokens: AtomicU64,
    total_output_tokens: AtomicU64,
    /// Wall-clock deadline shared by the whole run. Each child layer gets
    /// `min(limits.timeout_ms, remaining_to_deadline)` as its timeout.
    root_deadline: tokio::time::Instant,
}

/// RAII guard that pops a caller frame when dropped, so the stack stays
/// balanced even if an inner `execute_run` returns early or errors.
struct FrameGuard<'r, 'a> {
    runner: &'r AgentRunner<'a>,
}

impl<'r, 'a> Drop for FrameGuard<'r, 'a> {
    fn drop(&mut self) {
        let mut stack = self
            .runner
            .caller_stack
            .lock()
            .expect("caller_stack poisoned");
        stack.pop();
    }
}

impl<'a> AgentRunner<'a> {
    /// Create a new runner bound to the given shared dependencies.
    ///
    /// Seeds the execution tree with a root node for the configured root agent
    /// and starts the shared wall-clock deadline.
    pub fn new(
        provider: &'a dyn ModelProvider,
        agent_tree: &'a AgentTree,
        tool_registry: &'a ToolRegistry,
        skills: &'a SkillsCatalog,
        config: &'a OpenSlateConfig,
        limits: RuntimeLimits,
        run_id: RunId,
    ) -> Self {
        let root_agent = agent_tree.get_root();
        let execution_tree = ExecutionTree::new(run_id.clone(), root_agent.id.clone());
        let root_deadline = tokio::time::Instant::now() + Duration::from_millis(limits.timeout_ms);
        let skills_section = skills.catalog_prompt(config.skills.max_list_chars);
        Self {
            provider,
            agent_tree,
            tool_registry,
            config,
            limits,
            context_config: ContextIsolationConfig::default(),
            run_id,
            skills_section,
            execution_tree: Mutex::new(execution_tree),
            caller_stack: Mutex::new(Vec::new()),
            child_call_count: AtomicU32::new(0),
            total_input_tokens: AtomicU64::new(0),
            total_output_tokens: AtomicU64::new(0),
            root_deadline,
        }
    }

    /// Total input tokens accumulated across the whole run (all layers).
    pub fn total_input_tokens(&self) -> u64 {
        self.total_input_tokens.load(Ordering::Relaxed)
    }

    /// Total output tokens accumulated across the whole run (all layers).
    pub fn total_output_tokens(&self) -> u64 {
        self.total_output_tokens.load(Ordering::Relaxed)
    }

    /// Take a snapshot of the execution tree built during the run.
    pub fn execution_tree(&self) -> ExecutionTree {
        self.execution_tree
            .lock()
            .expect("execution_tree poisoned")
            .clone()
    }

    /// Number of `call_agent` invocations that have been counted this run.
    pub fn child_call_count(&self) -> u32 {
        self.child_call_count.load(Ordering::Relaxed)
    }

    /// Append the skills catalog section (if any) to a system prompt.
    ///
    /// With no skills configured this is the identity function, so the
    /// system prompt is byte-identical to the agent's `default_prompt`.
    fn with_skills_section(&self, prompt: &str) -> String {
        match &self.skills_section {
            Some(section) => format!("{prompt}\n\n{section}"),
            None => prompt.to_owned(),
        }
    }

    /// Build the tool definitions exposed to a given agent: its `tools:`
    /// whitelist (or all registered tools when the whitelist is empty) plus,
    /// when the agent has children, the `call_agent` definitions that let the
    /// model delegate sub-tasks.
    fn tool_definitions_for(&self, agent_id: &AgentId) -> Vec<ToolDefinition> {
        let agent = match self.agent_tree.get_agent(agent_id) {
            Some(a) => a,
            None => return vec![],
        };
        let mut defs = if agent.tools.is_empty() {
            self.tool_registry.definitions()
        } else {
            self.tool_registry.definitions_for(&agent.tools)
        };
        if !agent.children.is_empty() {
            defs.extend(child_agent_definitions(&agent.children, self.agent_tree));
        }
        defs
    }

    // ── caller-stack helpers ───────────────────────────────────────────────

    fn push_frame(&self, frame: CallerFrame) {
        self.caller_stack
            .lock()
            .expect("caller_stack poisoned")
            .push(frame);
    }

    fn current_frame(&self) -> Option<CallerFrame> {
        self.caller_stack
            .lock()
            .expect("caller_stack poisoned")
            .last()
            .cloned()
    }

    fn accumulate_tokens(&self, result: &RunResult) {
        self.total_input_tokens
            .fetch_add(result.total_input_tokens, Ordering::Relaxed);
        self.total_output_tokens
            .fetch_add(result.total_output_tokens, Ordering::Relaxed);
    }

    fn update_exec_status(&self, exec_id: &ExecutionNodeId, status: ExecutionStatus) {
        let mut tree = self.execution_tree.lock().expect("execution_tree poisoned");
        tree.update_status(exec_id, status);
    }

    /// Remaining timeout for a child layer: the smaller of the per-layer cap
    /// and the time left until the shared run deadline. Returns 0 once the
    /// deadline has passed (which makes `execute_run` fail fast with Timeout).
    fn child_timeout_ms(&self) -> u64 {
        let now = tokio::time::Instant::now();
        if now >= self.root_deadline {
            return 0;
        }
        let remaining = (self.root_deadline - now).as_millis() as u64;
        remaining.min(self.limits.timeout_ms)
    }

    /// Execute the root agent to completion.
    ///
    /// Runs the single-agent [`execute_run`] loop with `self` as the
    /// `ToolExecutor`, so any `call_agent` tool call is intercepted and
    /// recurses into the child agent.
    pub async fn run_root(
        &self,
        prior_messages: Vec<Message>,
        progress: Option<&mut dyn ProgressCallback>,
    ) -> Result<RunResult, OpenSlateError> {
        let root = self.agent_tree.get_root();
        let root_exec_id = self
            .execution_tree
            .lock()
            .expect("execution_tree poisoned")
            .root_id()
            .clone();

        // Resolve the root model BEFORE pushing a frame, so a resolution
        // failure returns cleanly without leaving a stale caller frame.
        let resolved = resolve_model(self.config, &root.model_alias)?;

        self.push_frame(CallerFrame {
            agent_id: root.id.clone(),
            exec_id: root_exec_id.clone(),
            depth: 0,
            messages_snapshot: prior_messages.clone(),
        });
        let _guard = FrameGuard { runner: self };

        let run_config = RunConfig {
            run_id: self.run_id.clone(),
            agent_id: root.id.clone(),
            model_alias: root.model_alias.clone(),
            system_prompt: Some(self.with_skills_section(&root.default_prompt)),
            initial_messages: prior_messages,
            max_steps: self.limits.max_steps,
            max_context_bytes: self.limits.max_context_bytes,
            max_output_bytes: self.limits.max_output_bytes,
            max_empty_turns: self.limits.max_empty_turns,
            tool_definitions: self.tool_definitions_for(&root.id),
            timeout_ms: self.limits.timeout_ms,
            depth: 0,
        };

        let result = execute_run(
            self.provider,
            run_config,
            &resolved.model_id,
            self,
            progress,
        )
        .await;

        // Update root status from the outcome, then accumulate tokens.
        match &result {
            Ok(r) => {
                self.accumulate_tokens(r);
                self.update_exec_status(&root_exec_id, ExecutionStatus::Completed);
            }
            Err(_) => {
                self.update_exec_status(&root_exec_id, ExecutionStatus::Failed);
            }
        }

        result
    }

    /// Recursively run a (child) agent. Returns a pinned future so the async
    /// recursion compiles (a raw `async fn` recursing through itself yields an
    /// infinitely-sized future).
    ///
    /// `progress` is always `None` for children: child-layer streaming would
    /// require sharing the parent's `&mut ProgressCallback`, which is not
    /// sound across the recursion. The parent still observes each delegation
    /// via the normal `on_tool_start`/`on_tool_end` callbacks for
    /// `call_agent`.
    fn run_agent(
        &self,
        agent_id: AgentId,
        exec_id: ExecutionNodeId,
        depth: u32,
        initial_messages: Vec<Message>,
    ) -> BoxFuture<'_, Result<RunResult, OpenSlateError>> {
        Box::pin(async move {
            self.push_frame(CallerFrame {
                agent_id: agent_id.clone(),
                exec_id: exec_id.clone(),
                depth,
                messages_snapshot: initial_messages.clone(),
            });
            let _guard = FrameGuard { runner: self };

            let agent = match self.agent_tree.get_agent(&agent_id) {
                Some(a) => a,
                None => {
                    return Err(OpenSlateError::Runtime(
                        crate::error::RuntimeError::UnknownTool {
                            // Reuse a runtime error variant for a missing agent.
                            tool_name: format!("agent '{}' not found", agent_id.0),
                            step: 0,
                            agent_id: agent_id.0.clone(),
                        },
                    ));
                }
            };

            let resolved = resolve_model(self.config, &agent.model_alias)?;

            // build_child_context embeds the child's system prompt (and any
            // parent-summary) as leading system-role messages. Some providers
            // (e.g. the internlm openai-compatible endpoint via the genai
            // adapter) reject a system-role message inside the messages array
            // and return an empty/error response in ~100ms. Pull all leading
            // system messages into the dedicated `system_prompt` field — the
            // same field the root agent uses — and pass only the task onward.
            let (system_prompt, init_msgs) = split_leading_system(&initial_messages);

            let run_config = RunConfig {
                run_id: self.run_id.clone(),
                agent_id: agent_id.clone(),
                model_alias: agent.model_alias.clone(),
                system_prompt,
                initial_messages: init_msgs,
                max_steps: self.limits.max_steps,
                max_context_bytes: self.limits.max_context_bytes,
                max_output_bytes: self.limits.max_output_bytes,
                max_empty_turns: self.limits.max_empty_turns,
                tool_definitions: self.tool_definitions_for(&agent_id),
                timeout_ms: self.child_timeout_ms(),
                depth,
            };

            // Drive the child with the same streaming progress path the root
            // uses, but via a text-mode callback (ChildProgress) that emits
            // indented tracing lines — so reasoning, tool calls, answer, and
            // per-step stats all show up nested under the parent, matching the
            // root's output format minus the spinner TUI.
            let mut child_progress = ChildProgress::new(depth, agent_id.0.clone());
            let result = execute_run(
                self.provider,
                run_config,
                &resolved.model_id,
                self,
                Some(&mut child_progress),
            )
            .await;

            match &result {
                Ok(r) => {
                    self.accumulate_tokens(r);
                    self.update_exec_status(&exec_id, ExecutionStatus::Completed);
                }
                Err(_) => {
                    self.update_exec_status(&exec_id, ExecutionStatus::Failed);
                }
            }

            result
        })
    }

    /// Handle an intercepted `call_agent` tool call: validate, enforce limits,
    /// create a child execution node, build the child's isolated context,
    /// recurse, and translate the child's outcome into a tool output.
    async fn handle_call_agent(&self, args: &serde_json::Value) -> ToolOutput {
        let child_id_str = match args["agent_id"].as_str() {
            Some(s) => s,
            None => {
                return self.error_output("call_agent requires an 'agent_id' string argument");
            }
        };
        let task = match args["task"].as_str() {
            Some(s) => s.to_owned(),
            None => {
                return self.error_output("call_agent requires a 'task' string argument");
            }
        };

        let frame = match self.current_frame() {
            Some(f) => f,
            None => {
                return self.error_output("call_agent invoked outside of an agent run context");
            }
        };
        let caller_id = frame.agent_id.clone();
        let child_id = AgentId(child_id_str.to_owned());

        // Security: the requested child must be a configured child of the
        // caller. This blocks delegating to siblings, the root, or unknown ids.
        let is_child = self
            .agent_tree
            .get_children(&caller_id)
            .iter()
            .any(|c| c.id == child_id);
        if !is_child {
            return self.error_output(format!(
                "agent '{}' is not a child of '{}'",
                child_id.0, caller_id.0
            ));
        }

        let child_depth = frame.depth + 1;

        // Pre-check BEFORE reserving the budget slot: `max_child_agent_calls`
        // is "how many child calls are allowed", so a run that has already
        // made `current` calls may proceed only while current < max
        // (check_limits rejects when current >= max). The slot is reserved
        // only after the check passes.
        let current_calls = self.child_call_count.load(Ordering::Relaxed);
        if let Err(e) = check_limits(&self.limits, 0, child_depth, 0, current_calls) {
            return self.error_output(format!("child agent call denied: {}", e));
        }
        self.child_call_count.fetch_add(1, Ordering::Relaxed);

        let child_agent = match self.agent_tree.get_agent(&child_id) {
            Some(a) => a,
            None => {
                self.child_call_count.fetch_sub(1, Ordering::Relaxed);
                return self.error_output(format!("agent '{}' not found in tree", child_id.0));
            }
        };

        // Create the child execution node (short lock, not held across await).
        let child_exec_id = {
            let mut tree = self.execution_tree.lock().expect("execution_tree poisoned");
            tree.create_child(
                self.run_id.clone(),
                child_id.clone(),
                frame.exec_id.clone(),
                None, // parent_call_id: tool_call id is not visible here
            )
        };

        // Build the child's isolated context from the caller's snapshot.
        // Bind the augmented system prompt first so the borrow does not
        // span the recursive `run_agent` call below.
        let child_system = self.with_skills_section(&child_agent.default_prompt);
        let child_messages = build_child_context(
            &self.context_config,
            Some(&child_system),
            &task,
            &frame.messages_snapshot,
        );

        // Recurse.
        let outcome = self
            .run_agent(
                child_id.clone(),
                child_exec_id.clone(),
                child_depth,
                child_messages,
            )
            .await;

        match outcome {
            Ok(result) => {
                let text = last_assistant_text(&result.messages);
                match result.status {
                    RunStatus::Completed => self.success_output(text.unwrap_or_default()),
                    RunStatus::Interrupted => self.success_output(format!(
                        "[child agent '{}' did not finish: interrupted]\n{}",
                        child_id.0,
                        text.unwrap_or_default()
                    )),
                    // execute_run only yields Completed/Interrupted on Ok, but
                    // cover the remaining variants defensively.
                    other => self.success_output(format!(
                        "[child agent '{}' ended in state {:?}]\n{}",
                        child_id.0,
                        other,
                        text.unwrap_or_default()
                    )),
                }
            }
            Err(e) => {
                // run_agent already marked the child node Failed on its Err
                // path, so here we only translate the error for the parent.
                self.error_output(format!("child agent '{}' failed: {}", child_id.0, e))
            }
        }
    }

    fn success_output(&self, content: String) -> ToolOutput {
        let bytes = content.len();
        ToolOutput {
            content,
            bytes,
            duration_ms: 0,
            status: ToolOutputStatus::Success,
        }
    }

    fn error_output(&self, content: impl Into<String>) -> ToolOutput {
        let content = content.into();
        ToolOutput {
            content,
            bytes: 0,
            duration_ms: 0,
            status: ToolOutputStatus::Error,
        }
    }
}

/// Pull all leading system-role messages out of a message list and join them
/// into a single system prompt. Used by `run_agent` to move a child's system
/// prompt (placed at the front by `build_child_context`) into the dedicated
/// `system_prompt` field that providers read — mirroring how the root agent is
/// configured. Messages after the first non-system one are left untouched.
fn split_leading_system(msgs: &[Message]) -> (Option<String>, Vec<Message>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut rest = Vec::new();
    let mut seen_non_system = false;
    for m in msgs {
        if m.role == MessageRole::System && !seen_non_system {
            if !m.content.is_empty() {
                system_parts.push(m.content.clone());
            }
        } else {
            seen_non_system = true;
            rest.push(m.clone());
        }
    }
    let system_prompt = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (system_prompt, rest)
}

/// Extract the final assistant text (the last assistant message with content)
/// from a run's message history — used as the child's returned result.
fn last_assistant_text(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Assistant && !m.content.is_empty())
        .map(|m| m.content.clone())
}

/// A text-mode [`ProgressCallback`] for child agents.
///
/// The root agent displays progress via the spinner TUI (streaming reasoning,
/// live token counts, `-> tool`, `<- tool [bytes]`, per-step
/// `{elapsed}s · ↑in ↓out · tok/s`). Child agents cannot nest another spinner
/// cleanly, so this callback replays the same event information as indented
/// `tracing` lines — giving the user a consistent view of every layer's model
/// activity, nested by depth.
struct ChildProgress {
    indent: String,
    agent: String,
    step_start: Instant,
    input_tokens: u32,
    output_tokens: u32,
    reasoning_buf: String,
    content_buf: String,
}

impl ChildProgress {
    fn new(depth: u32, agent: String) -> Self {
        Self {
            indent: "  ".repeat(depth as usize),
            agent,
            step_start: Instant::now(),
            input_tokens: 0,
            output_tokens: 0,
            reasoning_buf: String::new(),
            content_buf: String::new(),
        }
    }

    fn flush_reasoning(&mut self) {
        if !self.reasoning_buf.is_empty() {
            let r = self.reasoning_buf.trim_end();
            if !r.is_empty() {
                tracing::info!("{}  ┊ {}", self.indent, r);
            }
            self.reasoning_buf.clear();
        }
    }

    fn emit_stats(&self) {
        let elapsed = self.step_start.elapsed().as_secs_f64();
        let tps = if elapsed > 0.0 {
            (self.output_tokens as f64 / elapsed).round() as u32
        } else {
            0
        };
        let tps_seg = if tps > 0 {
            format!(" · {}tok/s", tps)
        } else {
            String::new()
        };
        tracing::info!(
            "{}{:.1}s · ↑{} ↓{}{}",
            self.indent,
            elapsed,
            self.input_tokens,
            self.output_tokens,
            tps_seg
        );
    }
}

impl ProgressCallback for ChildProgress {
    fn on_request_start(&mut self, _step: u32, _model_id: &str) {
        self.step_start = Instant::now();
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.reasoning_buf.clear();
        self.content_buf.clear();
    }

    fn on_first_token(&mut self) {}

    fn on_delta(&mut self, text: &str) {
        self.content_buf.push_str(text);
    }

    fn on_reasoning(&mut self, text: &str) {
        self.reasoning_buf.push_str(text);
    }

    fn on_usage(&mut self, usage: Usage) {
        self.input_tokens = usage.input_tokens;
        self.output_tokens = usage.output_tokens;
    }

    fn on_request_end(&mut self) {
        // Always flush reasoning first so it appears before any tool line on
        // tool steps, and before the answer on content steps.
        self.flush_reasoning();
        if !self.content_buf.is_empty() {
            tracing::info!(
                "{}  ┃ [{}] {}",
                self.indent,
                self.agent,
                self.content_buf.trim()
            );
            self.emit_stats();
        }
    }

    fn on_tool_start(&mut self, name: &str, args: &str) {
        tracing::info!("{}  -> {}({})", self.indent, name, args);
    }

    fn on_tool_end(&mut self, name: &str, bytes: usize, _truncated: bool) {
        tracing::info!("{}  <- {} [{} bytes]", self.indent, name, bytes);
    }

    fn on_step_end(&mut self) {
        // Tool-step stats. Content-step stats are emitted in on_request_end.
        self.emit_stats();
    }
}

#[async_trait]
impl<'a> ToolExecutor for AgentRunner<'a> {
    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolOutput {
        if name == "call_agent" {
            return self.handle_call_agent(args).await;
        }
        // Delegate all other tools to the shared registry, converting errors
        // into error ToolOutputs exactly as ToolRegistry's own ToolExecutor
        // impl does (so the runtime loop keeps going gracefully).
        match self.tool_registry.execute(name, args).await {
            Ok(output) => output,
            Err(e) => ToolOutput {
                content: format!("Error: {}", e),
                bytes: 0,
                duration_ms: 0,
                status: ToolOutputStatus::Error,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderError;
    use crate::provider::GenerateRequest;
    use std::sync::atomic::AtomicUsize;

    // ── Test doubles ───────────────────────────────────────────────────────

    struct NoopProvider;
    #[async_trait]
    impl ModelProvider for NoopProvider {
        async fn generate(
            &self,
            _request: GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            unreachable!("this test does not call the provider")
        }
        fn provider_name(&self) -> &str {
            "noop"
        }
    }

    /// Scripted provider: returns responses in order by call index.
    struct ScriptedProvider {
        responses: Vec<ModelResponse>,
        call_count: AtomicUsize,
    }
    impl ScriptedProvider {
        fn new(responses: Vec<ModelResponse>) -> Self {
            Self {
                responses,
                call_count: AtomicUsize::new(0),
            }
        }
    }
    #[async_trait]
    impl ModelProvider for ScriptedProvider {
        async fn generate(
            &self,
            _request: GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            self.responses
                .get(idx)
                .cloned()
                .ok_or(ProviderError::ServerError(500))
        }
        fn provider_name(&self) -> &str {
            "scripted"
        }
    }

    fn test_config() -> OpenSlateConfig {
        let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "mock-model"

[models.fast]
provider = "mock"
model = "mock-fast"
"#;
        crate::config::parse_openslate_toml(toml).expect("config should parse")
    }

    fn user_message(content: &str) -> Message {
        Message {
            role: MessageRole::User,
            content: content.into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        }
    }

    fn root_only_tree() -> AgentTree {
        let agents = vec![AgentConfig {
            id: AgentId("root".into()),
            name: "Root".into(),
            model: "main".into(),
            children: vec![],
            tools: vec![],
            default_prompt: "root prompt".into(),
        }];
        AgentTree::from_configs(&agents).expect("tree should build")
    }

    /// root → child (child is a leaf using the "fast" model).
    fn root_child_tree() -> AgentTree {
        let agents = vec![
            AgentConfig {
                id: AgentId("root".into()),
                name: "Root".into(),
                model: "main".into(),
                children: vec![AgentId("child".into())],
                tools: vec![],
                default_prompt: "root prompt".into(),
            },
            AgentConfig {
                id: AgentId("child".into()),
                name: "Child".into(),
                model: "fast".into(),
                children: vec![],
                tools: vec![],
                default_prompt: "child prompt".into(),
            },
        ];
        AgentTree::from_configs(&agents).expect("tree should build")
    }

    struct EchoTool;
    #[async_trait]
    impl crate::tool::Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            args: &serde_json::Value,
        ) -> Result<ToolOutput, crate::error::ToolError> {
            Ok(ToolOutput {
                content: args.to_string(),
                bytes: 0,
                duration_ms: 0,
                status: ToolOutputStatus::Success,
            })
        }
    }

    // ── Phase 1: delegation / interception mechanics ───────────────────────

    #[tokio::test]
    async fn runner_delegates_normal_tool_to_registry() {
        let config = test_config();
        let tree = root_only_tree();
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &NoopProvider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        );

        let out = runner.execute("echo", &serde_json::json!({"x": 1})).await;
        assert_eq!(out.status, ToolOutputStatus::Success);
        assert!(out.content.contains("x"));
    }

    #[tokio::test]
    async fn runner_unknown_tool_returns_error_output() {
        let config = test_config();
        let tree = root_only_tree();
        let registry = ToolRegistry::new();
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &NoopProvider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        );

        let out = runner.execute("nope", &serde_json::json!({})).await;
        assert_eq!(out.status, ToolOutputStatus::Error);
    }

    #[tokio::test]
    async fn runner_call_agent_outside_run_is_error() {
        let config = test_config();
        let tree = root_child_tree();
        let registry = ToolRegistry::new();
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &NoopProvider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        );

        // No caller frame on the stack → handle_call_agent refuses.
        let out = runner
            .execute(
                "call_agent",
                &serde_json::json!({"agent_id": "child", "task": "x"}),
            )
            .await;
        assert_eq!(out.status, ToolOutputStatus::Error);
        assert!(out.content.contains("outside"));
    }

    #[test]
    fn runner_seeds_execution_tree_with_root() {
        let config = test_config();
        let tree = root_only_tree();
        let registry = ToolRegistry::new();
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &NoopProvider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        );
        let t = runner.execution_tree();
        assert_eq!(t.root().agent_id.0, "root");
        assert_eq!(t.root().depth, 0);
    }

    #[test]
    fn runner_tool_definitions_for_root_with_children_includes_call_agent() {
        let config = test_config();
        let tree = root_child_tree();
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &NoopProvider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        );

        let defs = runner.tool_definitions_for(&AgentId("root".into()));
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"call_agent"));
    }

    // ── Phase 2+3: real recursive delegation ───────────────────────────────

    fn call_agent_tool_call(id: &str, child: &str, task: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId(id.into()),
            name: "call_agent".into(),
            arguments: serde_json::json!({"agent_id": child, "task": task}),
        }
    }

    fn assistant_text(content: &str) -> ModelResponse {
        ModelResponse {
            content: Some(content.into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("stop".into()),
        }
    }

    #[tokio::test]
    async fn runner_recurses_root_to_child() {
        // root step 1 → call_agent(child, "greet")
        // child step 1 → "Hello from child"
        // root step 2 → final summary
        let provider = ScriptedProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![call_agent_tool_call("ca-1", "child", "greet")],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            assistant_text("Hello from child"),
            assistant_text("Delegation done: got child reply"),
        ]);

        let config = test_config();
        let tree = root_child_tree();
        let registry = ToolRegistry::new();
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        );

        let messages = vec![user_message("delegate please")];
        let result = runner.run_root(messages, None).await.expect("run ok");

        assert_eq!(result.status, RunStatus::Completed);
        // The child's reply must have surfaced back to the root as a tool msg.
        let tool_msg = result
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("tool message present");
        assert!(
            tool_msg.content.contains("Hello from child"),
            "child reply should reach root, got: {}",
            tool_msg.content
        );

        // Execution tree grew a child node at depth 1.
        let exec = runner.execution_tree();
        let node_count = exec.node_count();
        assert_eq!(node_count, 2, "root + child execution nodes");
        assert_eq!(runner.child_call_count(), 1);
    }

    #[tokio::test]
    async fn runner_depth_limit_blocks_recursion() {
        // max_depth = 1 → root (depth 0) may not spawn a depth-1 child.
        let provider = ScriptedProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![call_agent_tool_call("ca-1", "child", "greet")],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            // After the denied call the root produces a final answer.
            assistant_text("could not delegate"),
        ]);

        let config = test_config();
        let tree = root_child_tree();
        let registry = ToolRegistry::new();
        let limits = RuntimeLimits {
            max_depth: 1,
            ..RuntimeLimits::default()
        };
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            limits,
            RunId("t".into()),
        );

        let messages = vec![user_message("delegate please")];
        let result = runner.run_root(messages, None).await.expect("run ok");

        assert_eq!(result.status, RunStatus::Completed);
        let tool_msg = result
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("tool message present");
        assert!(
            tool_msg.content.contains("denied") || tool_msg.content.contains("depth"),
            "expected depth-limit denial in tool output, got: {}",
            tool_msg.content
        );
        // No child node created.
        assert_eq!(runner.execution_tree().node_count(), 1);
    }

    #[tokio::test]
    async fn runner_child_call_limit_blocks_second_call() {
        // max_child_agent_calls = 1 → first call succeeds, second is denied.
        let provider = ScriptedProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![call_agent_tool_call("ca-1", "child", "first")],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            assistant_text("child reply 1"),
            ModelResponse {
                content: None,
                tool_calls: vec![call_agent_tool_call("ca-2", "child", "second")],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            assistant_text("child reply 2"),
            assistant_text("done"),
        ]);

        let config = test_config();
        let tree = root_child_tree();
        let registry = ToolRegistry::new();
        let limits = RuntimeLimits {
            max_child_agent_calls: 1,
            ..RuntimeLimits::default()
        };
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            limits,
            RunId("t".into()),
        );

        let messages = vec![user_message("delegate twice")];
        let result = runner.run_root(messages, None).await.expect("run ok");

        assert_eq!(result.status, RunStatus::Completed);
        let tool_msgs: Vec<&Message> = result
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 2, "two call_agent invocations");
        // First succeeded, second denied.
        assert!(tool_msgs[0].content.contains("child reply 1"));
        assert!(
            tool_msgs[1].content.contains("denied") || tool_msgs[1].content.contains("call"),
            "second call should be denied, got: {}",
            tool_msgs[1].content
        );
    }

    #[tokio::test]
    async fn runner_rejects_non_child_agent_id() {
        // root has only "child"; requesting "ghost" must be rejected.
        let provider = ScriptedProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![call_agent_tool_call("ca-1", "ghost", "x")],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            assistant_text("ok"),
        ]);

        let config = test_config();
        let tree = root_child_tree();
        let registry = ToolRegistry::new();
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        );

        let messages = vec![user_message("go")];
        let result = runner.run_root(messages, None).await.expect("run ok");

        let tool_msg = result
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("tool message");
        assert!(
            tool_msg.content.contains("not a child") || tool_msg.content.contains("ghost"),
            "expected non-child rejection, got: {}",
            tool_msg.content
        );
        assert_eq!(runner.execution_tree().node_count(), 1);
    }

    // ── Multi-level recursion (oracle follow-up) ──────────────────────────

    fn root_child_grandchild_tree() -> AgentTree {
        let agents = vec![
            AgentConfig {
                id: AgentId("root".into()),
                name: "Root".into(),
                model: "main".into(),
                children: vec![AgentId("child".into())],
                tools: vec![],
                default_prompt: "root".into(),
            },
            AgentConfig {
                id: AgentId("child".into()),
                name: "Child".into(),
                model: "main".into(),
                children: vec![AgentId("grandchild".into())],
                tools: vec![],
                default_prompt: "child".into(),
            },
            AgentConfig {
                id: AgentId("grandchild".into()),
                name: "Grandchild".into(),
                model: "main".into(),
                children: vec![],
                tools: vec![],
                default_prompt: "grandchild".into(),
            },
        ];
        AgentTree::from_configs(&agents).expect("three-level tree should build")
    }

    /// Three-level delegation root -> child -> grandchild, verifying that
    /// depth stays consistent between the caller-frame and the execution tree,
    /// that a child with its own children gets a `call_agent` tool definition,
    /// and that a grandchild's answer propagates back up two layers.
    #[tokio::test]
    async fn runner_recurses_three_levels() {
        let provider = ScriptedProvider::new(vec![
            // root: delegate to child
            ModelResponse {
                content: None,
                tool_calls: vec![call_agent_tool_call("ca-1", "child", "do it")],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            // child: delegate to grandchild
            ModelResponse {
                content: None,
                tool_calls: vec![call_agent_tool_call("ca-2", "grandchild", "sub-task")],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            // grandchild: answer
            assistant_text("deep result"),
            // child: summarize grandchild's reply
            assistant_text("child aggregated: deep result"),
            // root: summarize child's reply
            assistant_text("root done"),
        ]);

        let config = test_config();
        let tree = root_child_grandchild_tree();
        let registry = ToolRegistry::new();
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        );

        let messages = vec![user_message("go")];
        let result = runner.run_root(messages, None).await.expect("run ok");

        assert_eq!(result.status, RunStatus::Completed);
        // Three execution nodes: root + child + grandchild.
        assert_eq!(runner.execution_tree().node_count(), 3);
        assert_eq!(runner.child_call_count(), 2);
        // The root's own conversation only shows its single call_agent hop
        // (to child); the child's call to grandchild lives in the child's own
        // isolated context, not the root's messages. The grandchild's reply
        // nonetheless surfaces because the child's returned summary embeds it.
        let tool_msgs: Vec<&Message> = result
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 1, "root has one call_agent hop");
        assert!(
            tool_msgs[0].content.contains("deep result"),
            "grandchild reply should reach root via child's summary, got: {}",
            tool_msgs[0].content
        );
    }

    // ── Skills catalog injection ──────────────────────────────────────────

    /// Provider that records the system prompt of every request and answers
    /// with a fixed final response.
    struct CapturingProvider {
        system_prompts: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl ModelProvider for CapturingProvider {
        async fn generate(&self, request: GenerateRequest) -> Result<ModelResponse, ProviderError> {
            self.system_prompts
                .lock()
                .expect("system_prompts poisoned")
                .push(request.system_prompt.unwrap_or_default());
            Ok(assistant_text("ok"))
        }
        fn provider_name(&self) -> &str {
            "capturing"
        }
    }

    /// Build a one-skill catalog from a real temp SKILL.md directory.
    fn skills_catalog_with(name: &str, description: &str) -> crate::skills::SkillsCatalog {
        let dir = tempfile::TempDir::new().unwrap();
        let skill_dir = dir.path().join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nSkill body.\n"),
        )
        .unwrap();
        let (catalog, warnings) = crate::skills::discover_skills(&[dir.path().to_path_buf()]);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        catalog
    }

    #[tokio::test]
    async fn runner_root_system_prompt_includes_skills_section() {
        let provider = CapturingProvider {
            system_prompts: Mutex::new(Vec::new()),
        };
        let config = test_config();
        let tree = root_only_tree();
        let registry = ToolRegistry::new();
        let skills = skills_catalog_with("pdf-processing", "Handle PDF files");
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        );

        let messages = vec![user_message("hi")];
        runner.run_root(messages, None).await.expect("run ok");

        let prompts = provider.system_prompts.lock().expect("prompts");
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].starts_with("root prompt\n\n# Skills"),
            "skills section must follow the agent prompt, got: {}",
            prompts[0]
        );
        assert!(prompts[0].contains("- name: pdf-processing"));
        assert!(prompts[0].contains("  description: Handle PDF files"));
    }

    #[tokio::test]
    async fn runner_root_system_prompt_unchanged_when_catalog_empty() {
        let provider = CapturingProvider {
            system_prompts: Mutex::new(Vec::new()),
        };
        let config = test_config();
        let tree = root_only_tree();
        let registry = ToolRegistry::new();
        let skills = crate::skills::SkillsCatalog::default();
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        );

        let messages = vec![user_message("hi")];
        runner.run_root(messages, None).await.expect("run ok");

        let prompts = provider.system_prompts.lock().expect("prompts");
        assert_eq!(prompts.len(), 1);
        // Byte-identical to the agent's default_prompt: empty catalog must
        // not alter existing behavior.
        assert_eq!(prompts[0], "root prompt");
    }
}
