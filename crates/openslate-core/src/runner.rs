//! AgentRunner — recursive child-agent orchestrator.
//!
//! Implements [`ToolExecutor`] so it can be handed to [`execute_run`] as the
//! tool executor. When the model emits a `call_agent` tool call, the runner
//! intercepts it and recursively runs the child agent. Non-`call_agent` tools
//! are delegated to the shared [`ToolRegistry`].
//!
//! Design rationale: [`execute_run`] stays generic — the delegation seam is
//! the [`ToolExecutor`] trait, which `execute_run` already invokes for every
//! tool call (`runtime.rs:execute_tool_safely`). Passing an `AgentRunner` as
//! that executor makes recursion happen naturally. Run-scoped concerns (the
//! Phase 3 persistence sink, the Phase 4 cancellation token) are injected via
//! builder methods and forwarded from `run_root`. See
//! `.slim/deepwork/subagent-recursive-delegation.md`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use openslate_ptc::{
    resolve_tool_mode, run_code_description, run_code_parameters_schema, PtcBoundTool, PtcLimits,
    PtcToolInfo, ToolBridge, ToolCallMode, RUN_CODE_TOOL,
};

use crate::agent_tree::AgentTree;
use crate::approval::{ApprovalDecision, ApprovalManager};
use crate::callable::child_agent_definitions;
use crate::config::{OpenSlateConfig, PtcConfig};
use crate::context::{build_child_context, ContextIsolationConfig};
use crate::error::OpenSlateError;
use crate::execution::{ExecutionStatus, ExecutionTree};
use crate::model_config::resolve_model;
use crate::provider::{ModelProvider, ProgressCallback, ToolDefinition};
use crate::runtime::{
    check_limits, execute_run, CancellationToken, MessageSink, RunConfig, RunResult, RuntimeLimits,
};
use crate::skills::SkillsCatalog;
use crate::tool::{
    create_audit_record, limit_tool_output, Tool, ToolAuditRecord, ToolExecutor, ToolRegistry,
    TOOL_CALLER_DIRECT, TOOL_CALLER_RUN_CODE,
};
use crate::types::*;

/// Consecutive approval denials of the same tool that trip the watchdog and
/// abort the run (stops a model from hammering a denied tool forever; an
/// `Approved` decision resets the tool's counter).
const APPROVAL_DENIAL_LIMIT: u32 = 3;

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
    /// PTC (`run_code`) settings, snapshotted from `config.ptc` at
    /// construction (same pipeline as skills: wiring loads the config →
    /// RunManager holds it → AgentRunner snapshots the slice it needs).
    ptc: PtcConfig,
    /// Approval gate (Phase 1), snapshotted from the RunManager at
    /// construction — same pipeline as skills/ptc (wiring derives the
    /// effective policy → RunManager holds the ApprovalManager → the
    /// runner snapshots it per run). Defaults to `Auto` (approve
    /// everything); consult `with_approval` / RunManager wiring.
    approval: ApprovalManager,
    /// Per-run message persistence sink (Phase 3), cloned from the
    /// RunManager. Only the ROOT layer receives it (`run_root` passes it to
    /// `execute_run`; child layers pass `None`) — resume rebuilds the root
    /// conversation, and interleaving child-layer messages into the same
    /// run row would corrupt the restored ordering.
    message_sink: Option<Arc<dyn MessageSink>>,
    /// Cooperative cancellation token (Phase 4), cloned from the RunManager.
    /// Shared by every recursion layer — root AND children observe the same
    /// token, so cancelling it stops the whole delegation tree: each layer's
    /// `execute_run` returns `Ok(RunStatus::Interrupted)` with its partial
    /// transcript at its next checkpoint (children do not need their own
    /// trigger). A token already cancelled at `run_root` entry surfaces
    /// `RuntimeError::Cancelled` — the variant's real trigger point.
    cancel_token: CancellationToken,

    // Interior-mutable shared state across the run and recursion layers.
    execution_tree: Mutex<ExecutionTree>,
    caller_stack: Mutex<Vec<CallerFrame>>,
    child_call_count: AtomicU32,
    /// Global tool-call count for the whole run: every direct tool call the
    /// model makes (including `run_code` itself) plus every bridge call a
    /// `run_code` script makes. `call_agent` is excluded — it has its own
    /// budget (`max_child_agent_calls`). Consulted by `check_limits` at
    /// delegation boundaries and by `execute` before every tool call
    /// (main-loop enforcement of `max_tool_calls`).
    tool_call_count: AtomicU32,
    /// Audit records for every tool execution in this run (direct model
    /// calls and sandbox bridge calls alike), with caller attribution —
    /// PTC_PLAN.md §6.5. `Arc` so the `run_code` bridge closure (which must
    /// be 'static) can record into the same log.
    tool_audit_log: Arc<Mutex<Vec<ToolAuditRecord>>>,
    /// Approval watchdog: consecutive `Denied` counts per tool name; an
    /// `Approved` decision resets its tool's counter.
    denial_counts: Mutex<HashMap<String, u32>>,
    /// Tool that tripped the approval watchdog (reported in the abort
    /// error).
    watchdog_tool: Mutex<Option<String>>,
    /// Wakes `run_root`'s abort branch when the approval watchdog trips.
    abort_notify: tokio::sync::Notify,
    total_input_tokens: AtomicU64,
    total_output_tokens: AtomicU64,
    /// Accumulated cost in USD across all layers of the run (P2-3): every
    /// layer's `RunResult::total_cost_usd` (already priced with that
    /// layer's own model pricing) folds in here — the same aggregation
    /// path as the token counters. Mutex (not atomic) because f64 has no
    /// atomic ops; folds happen once per completed layer, never racing
    /// (`call_agent` batches run sequentially).
    total_cost_usd: Mutex<f64>,
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
            ptc: config.ptc.clone(),
            approval: ApprovalManager::auto(),
            message_sink: None,
            cancel_token: CancellationToken::new(),
            execution_tree: Mutex::new(execution_tree),
            caller_stack: Mutex::new(Vec::new()),
            child_call_count: AtomicU32::new(0),
            tool_call_count: AtomicU32::new(0),
            tool_audit_log: Arc::new(Mutex::new(Vec::new())),
            denial_counts: Mutex::new(HashMap::new()),
            watchdog_tool: Mutex::new(None),
            abort_notify: tokio::sync::Notify::new(),
            total_input_tokens: AtomicU64::new(0),
            total_output_tokens: AtomicU64::new(0),
            total_cost_usd: Mutex::new(0.0),
            root_deadline,
        }
    }

    /// Attach the approval manager (builder style).
    ///
    /// The manager is cloned from the RunManager (config → RunManager →
    /// AgentRunner snapshot, same pipeline as skills/ptc). The callback
    /// inside it is `Arc`-shared, so session-level state (e.g. a REPL
    /// allowlist) is observed by every recursion layer of the run.
    pub fn with_approval(mut self, approval: ApprovalManager) -> Self {
        self.approval = approval;
        self
    }

    /// Attach the per-run message persistence sink (builder style, Phase 3).
    ///
    /// Cloned from the RunManager's `message_sink`; the root layer forwards
    /// it to `execute_run` so every appended assistant message / tool result
    /// is persisted inline before the loop continues.
    pub fn with_message_sink(mut self, sink: Option<Arc<dyn MessageSink>>) -> Self {
        self.message_sink = sink;
        self
    }

    /// Attach the run's cancellation token (builder style, Phase 4).
    ///
    /// Cloned from the RunManager's token; both the root layer and every
    /// child layer forward it to their `execute_run` so Ctrl-C stops the
    /// whole delegation tree at the next checkpoint, gracefully, with
    /// partial transcripts.
    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = token;
        self
    }

    /// Total input tokens accumulated across the whole run (all layers).
    pub fn total_input_tokens(&self) -> u64 {
        self.total_input_tokens.load(Ordering::Relaxed)
    }

    /// Total output tokens accumulated across the whole run (all layers).
    pub fn total_output_tokens(&self) -> u64 {
        self.total_output_tokens.load(Ordering::Relaxed)
    }

    /// Total cost in USD accumulated across the whole run (all layers,
    /// each priced with its own model's pricing — P2-3).
    pub fn total_cost_usd(&self) -> f64 {
        *self.total_cost_usd.lock().expect("total_cost_usd poisoned")
    }

    /// Fold a completed layer's usage totals (tokens and cost) into the
    /// run-wide accumulators. Called for the root result AND every child
    /// result, which is how delegated spend aggregates into the root.
    fn accumulate_tokens(&self, result: &RunResult) {
        self.total_input_tokens
            .fetch_add(result.total_input_tokens, Ordering::Relaxed);
        self.total_output_tokens
            .fetch_add(result.total_output_tokens, Ordering::Relaxed);
        *self.total_cost_usd.lock().expect("total_cost_usd poisoned") += result.total_cost_usd;
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

    /// Number of tool calls counted this run: direct calls (including
    /// `run_code` invocations) plus the bridge calls each `run_code` script
    /// made. `call_agent` calls are counted separately.
    pub fn tool_call_count(&self) -> u32 {
        self.tool_call_count.load(Ordering::Relaxed)
    }

    /// Audit records for every tool execution this run, in execution order,
    /// with caller attribution (`direct` / `run_code`, PTC_PLAN.md §6.5).
    pub fn tool_audit_records(&self) -> Vec<ToolAuditRecord> {
        self.tool_audit_log
            .lock()
            .expect("tool_audit_log poisoned")
            .clone()
    }

    /// Append an audit record for a tool execution.
    fn record_tool_audit(
        &self,
        name: &str,
        caller: &str,
        args: &serde_json::Value,
        output: &ToolOutput,
    ) {
        self.tool_audit_log
            .lock()
            .expect("tool_audit_log poisoned")
            .push(create_audit_record(name, caller, args, output));
    }

    /// Agent whose loop emitted the current tool call (the top caller
    /// frame, or the root agent when `execute` is invoked outside a run —
    /// direct test invocation).
    fn current_agent_id(&self) -> AgentId {
        self.current_frame()
            .map(|f| f.agent_id)
            .unwrap_or_else(|| self.agent_tree.get_root().id.clone())
    }

    /// Record a consecutive denial for `name`; returns `true` when the
    /// watchdog limit is reached (the caller aborts the run).
    fn note_denial(&self, name: &str) -> bool {
        let mut counts = self.denial_counts.lock().expect("denial_counts poisoned");
        let count = counts.entry(name.to_owned()).or_insert(0);
        *count += 1;
        *count >= APPROVAL_DENIAL_LIMIT
    }

    /// Reset the consecutive-denial counter for `name` (approved call).
    fn clear_denials(&self, name: &str) {
        self.denial_counts
            .lock()
            .expect("denial_counts poisoned")
            .remove(name);
    }

    /// Build the watchdog abort error once the watchdog has tripped.
    fn watchdog_error(&self, agent_id: &AgentId) -> Option<OpenSlateError> {
        let tool = self
            .watchdog_tool
            .lock()
            .expect("watchdog_tool poisoned")
            .clone()?;
        Some(OpenSlateError::Runtime(
            crate::error::RuntimeError::ApprovalAbort {
                tool_name: tool,
                agent_id: agent_id.0.clone(),
                denials: APPROVAL_DENIAL_LIMIT,
            },
        ))
    }

    /// Translate a denied approval into an errors-as-data tool result and
    /// feed the consecutive-denial watchdog.
    ///
    /// The denial is audited into the in-memory tool log (as an error
    /// result attributed to the direct caller) and via `tracing`; nothing
    /// is written to the store.
    fn denied_output(&self, name: &str, args: &serde_json::Value, reason: String) -> ToolOutput {
        let tripped = self.note_denial(name);
        let out = self.error_output(format!(
            "approval denied tool '{name}': {reason}\n\
             不要重试同一调用,请换方案或直接作答。"
        ));
        self.record_tool_audit(name, TOOL_CALLER_DIRECT, args, &out);
        if tripped {
            tracing::warn!(
                target: "openslate_approval",
                "tool '{name}' denied {APPROVAL_DENIAL_LIMIT} consecutive times — aborting run"
            );
            *self.watchdog_tool.lock().expect("watchdog_tool poisoned") = Some(name.to_owned());
            self.abort_notify.notify_one();
        }
        out
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
    ///
    /// With PTC enabled, ptc-only tools are removed from the direct list and
    /// a `run_code` tool is appended when the agent has at least one
    /// code-callable tool (PTC_PLAN.md §3, 收口①).
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
        if self.ptc.enabled {
            // ptc-only tools must not reach the model's direct tool list.
            defs.retain(|d| {
                resolve_tool_mode(&self.ptc.tool_modes, &d.name, true).direct_visible()
            });
            let code_defs = self.ptc_code_tool_defs(agent_id);
            if !code_defs.is_empty() {
                let infos = self.ptc_infos(&code_defs);
                // both-mode tools are the first candidates for auto-tier
                // demotion — their schemas are already paid for in the
                // direct tool list. ptc-only tools never demote (the code
                // description is the only place their schema is exposed).
                let both_mode_names: Vec<String> = code_defs
                    .iter()
                    .filter(|d| {
                        resolve_tool_mode(&self.ptc.tool_modes, &d.name, true) == ToolCallMode::Both
                    })
                    .map(|d| d.name.clone())
                    .collect();
                defs.push(ToolDefinition {
                    name: RUN_CODE_TOOL.to_owned(),
                    description: run_code_description(
                        &infos,
                        &both_mode_names,
                        self.ptc.disclosure,
                        self.ptc.max_list_chars,
                    ),
                    parameters: run_code_parameters_schema(),
                });
            }
        }
        defs
    }

    /// Tools the given agent may call from inside `run_code` — the PTC
    /// binding set (PTC_PLAN.md §3, 收口②): its `tools:` whitelist (or every
    /// registered tool when the whitelist is empty) restricted to
    /// ptc-callable modes, excluding `run_code` itself and `call_agent` (no
    /// recursive delegation from code; `call_agent` is not a registry tool,
    /// so the registry-derived list would exclude it anyway — the explicit
    /// filter guards against future changes).
    ///
    /// Shared by [`Self::tool_definitions_for`] (which renders these defs
    /// into the `run_code` description) and
    /// [`Self::handle_run_code`](Self::handle_run_code) (which bridges them),
    /// so the model-facing list and the sandbox binding set can never drift.
    fn ptc_code_tool_defs(&self, agent_id: &AgentId) -> Vec<ToolDefinition> {
        let agent = match self.agent_tree.get_agent(agent_id) {
            Some(a) => a,
            None => return vec![],
        };
        let mut defs = if agent.tools.is_empty() {
            self.tool_registry.definitions()
        } else {
            self.tool_registry.definitions_for(&agent.tools)
        };
        defs.retain(|d| {
            d.name != RUN_CODE_TOOL
                && d.name != "call_agent"
                && resolve_tool_mode(&self.ptc.tool_modes, &d.name, true).ptc_callable()
        });
        // Registry iteration order is a HashMap artifact; sort so the
        // generated declarations (and the model's view) are deterministic.
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// Map code-tool defs to [`PtcToolInfo`], resolving each tool's PTC
    /// namespace from the registry: MCP tools report their server alias
    /// (sandbox path `tools.<server>.<method>`, declarations grouped);
    /// builtin tools report `None` (flat `tools.<name>`).
    fn ptc_infos(&self, defs: &[ToolDefinition]) -> Vec<PtcToolInfo> {
        defs.iter()
            .map(|def| PtcToolInfo {
                name: def.name.clone(),
                description: def.description.clone(),
                parameters: def.parameters.clone(),
                namespace: self
                    .tool_registry
                    .get(&def.name)
                    .and_then(|t| t.namespace()),
            })
            .collect()
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
        // Cancellation (Phase 4): a token already cancelled at entry means
        // the run was stopped before it started — there is no partial
        // transcript to return gracefully, so `RuntimeError::Cancelled` is
        // surfaced here (the variant's real trigger point; a cancellation
        // observed mid-loop instead returns `Ok(RunStatus::Interrupted)`
        // with the partial messages from execute_run's checkpoints).
        if self.cancel_token.is_cancelled() {
            return Err(OpenSlateError::Runtime(
                crate::error::RuntimeError::Cancelled,
            ));
        }

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
            parallel_tool_calls: self.limits.parallel_tool_calls,
            // P2-3: pricing resolved here, at RunConfig construction, from
            // the same config source as the model itself — execute_run
            // stays config-unaware.
            cost: resolved.cost_spec(),
        };

        // Drive the root loop, racing it against the approval watchdog: a
        // tripped watchdog (same tool denied APPROVAL_DENIAL_LIMIT times
        // consecutively) aborts the whole run — including any in-flight
        // child layers, since dropping this future cancels everything it
        // transitively awaits. `biased` polls the abort branch first, so a
        // stored permit wins even if the loop raced toward completion; the
        // post-select re-check then makes the abort deterministic even when
        // the loop never yields (e.g. an instantly-completing provider).
        //
        // Cancellation (Phase 4) deliberately has NO branch here: the token
        // is observed inside `execute_run` (checkpoints before/inside
        // provider calls and around tool execution), which returns
        // `Ok(RunStatus::Interrupted)` with the partial transcript — racing
        // the loop in this select would drop it and lose the context.
        let outcome = tokio::select! {
            biased;
            _ = self.abort_notify.notified() => None,
            r = execute_run(
                self.provider,
                run_config,
                &resolved.model_id,
                self,
                self.message_sink.as_deref(),
                progress,
                Some(&self.cancel_token),
            ) => Some(r),
        };
        let result = match self.watchdog_error(&root.id) {
            Some(err) => Err(err),
            None => match outcome {
                Some(r) => r,
                // The abort branch fired without a recorded watchdog trip
                // (spurious notify): the run was halted from outside its
                // loop, so its context cannot be recovered here — report it
                // as a cancellation of the whole run.
                None => Err(OpenSlateError::Runtime(
                    crate::error::RuntimeError::Cancelled,
                )),
            },
        };

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
                parallel_tool_calls: self.limits.parallel_tool_calls,
                // P2-3: the child prices its OWN model alias (e.g. `fast`),
                // so a mixed main/fast delegation accumulates each layer's
                // spend at the right rate.
                cost: resolved.cost_spec(),
            };

            // Drive the child with the same streaming progress path the root
            // uses, but via a text-mode callback (ChildProgress) that emits
            // indented tracing lines — so reasoning, tool calls, answer, and
            // per-step stats all show up nested under the parent, matching
            // the root's output format minus the spinner TUI.
            //
            // The child observes the SAME cancellation token as the root
            // (Phase 4): cancelling stops every layer at its next
            // checkpoint; the child's partial result flows back through the
            // Interrupted branch of handle_call_agent.
            let mut child_progress = ChildProgress::new(depth, agent_id.0.clone());
            let result = execute_run(
                self.provider,
                run_config,
                &resolved.model_id,
                self,
                None,
                Some(&mut child_progress),
                Some(&self.cancel_token),
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
        // only after the check passes. The global tool-call count feeds the
        // same check (best-effort `max_tool_calls` enforcement; the main
        // loop itself never increments it — exp-1 gap).
        let current_calls = self.child_call_count.load(Ordering::Relaxed);
        let current_tool_calls = self.tool_call_count.load(Ordering::Relaxed);
        if let Err(e) = check_limits(
            &self.limits,
            0,
            child_depth,
            current_tool_calls,
            current_calls,
        ) {
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

    /// Handle an intercepted `run_code` tool call (PTC): bind the calling
    /// agent's ptc-callable tools into a fresh QuickJS sandbox, execute the
    /// model's code through [`openslate_ptc::run_code`], and translate the
    /// outcome into a ToolOutput whose content follows PTC_PLAN.md §6.4
    /// (`[logs]`/`[result]` on success, `[error]` + logs on failure).
    ///
    /// Errors are data: script failures become an Error ToolOutput with the
    /// full message as content, so the model can self-heal and the run loop
    /// keeps going.
    async fn handle_run_code(&self, args: &serde_json::Value) -> ToolOutput {
        let start = Instant::now();
        let code = match args.get("code").and_then(|c| c.as_str()) {
            Some(c) => c.to_owned(),
            None => {
                return self.error_output(
                    "run_code requires a 'code' string argument: an async arrow function, \
                     e.g. async () => { return await tools.read_file({ path: \"x\" }); }",
                );
            }
        };

        // The binding set belongs to the agent whose loop emitted the call.
        // Outside a run (no caller frame — direct invocation in tests) the
        // root agent's set is used.
        let agent_id = self
            .current_frame()
            .map(|f| f.agent_id)
            .unwrap_or_else(|| self.agent_tree.get_root().id.clone());

        // Owned bridge map: the executor's `spawn_blocking` boundary requires
        // 'static data while `AgentRunner<'a>` borrows, so resolve the bound
        // tools to their `Arc<dyn Tool>` handles up front (the same objects
        // the registry dispatches through). Names missing from the registry
        // (should not happen — the defs came from it) are skipped.
        // The same infos double as the sandbox discovery catalog powering
        // list_tools/describe_tool.
        let catalog = self.ptc_infos(&self.ptc_code_tool_defs(&agent_id));
        let mut bound: Vec<PtcBoundTool> = Vec::new();
        let mut bridge_tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        for info in &catalog {
            if let Some(tool) = self.tool_registry.get(&info.name) {
                bridge_tools.insert(info.name.clone(), tool);
                bound.push(PtcBoundTool {
                    name: info.name.clone(),
                    namespace: info.namespace.clone(),
                });
            }
        }

        // Host-side bridge (errors-as-data protocol): every outcome — lookup
        // failure, bad arguments, tool Err, or an error-status ToolOutput —
        // comes back as an error envelope the sandbox turns into a JS
        // exception the code can try/catch. Every execution is audited with
        // `caller: run_code` attribution (PTC_PLAN.md §6.5) and truncated
        // through the same engine-level cap the direct path uses.
        let max_output = self.ptc.max_output_bytes;
        let audit_log = Arc::clone(&self.tool_audit_log);
        let bridge: ToolBridge = Arc::new(
            move |name: &str, args_json: &str| -> BoxFuture<'static, String> {
                let name = name.to_owned();
                let args_json = args_json.to_owned();
                let tool = bridge_tools.get(&name).cloned();
                let audit_log = Arc::clone(&audit_log);
                Box::pin(async move {
                    let tool = match tool {
                        Some(t) => t,
                        None => {
                            return openslate_ptc::envelope_error(format!(
                                "tool not available in code mode: {name}"
                            ));
                        }
                    };
                    let parsed: serde_json::Value = match serde_json::from_str(&args_json) {
                        Ok(v) => v,
                        Err(e) => {
                            return openslate_ptc::envelope_error(format!(
                                "invalid tool arguments JSON: {e}"
                            ));
                        }
                    };
                    if !parsed.is_object() && !parsed.is_null() {
                        return openslate_ptc::envelope_error(format!(
                            "tool arguments must be a JSON object, got: {args_json}"
                        ));
                    }
                    match tool.execute(&parsed).await {
                        Err(e) => {
                            // No ToolOutput on Err; audit a synthetic error
                            // record so the failed bridge call stays visible.
                            let failed = ToolOutput {
                                content: format!("Error: {e}"),
                                bytes: 0,
                                duration_ms: 0,
                                status: ToolOutputStatus::Error,
                            };
                            audit_log.lock().expect("tool_audit_log poisoned").push(
                                create_audit_record(&name, TOOL_CALLER_RUN_CODE, &parsed, &failed),
                            );
                            openslate_ptc::envelope_error(e)
                        }
                        Ok(out) => {
                            let limited = truncate_bridge_output(out, max_output);
                            audit_log.lock().expect("tool_audit_log poisoned").push(
                                create_audit_record(&name, TOOL_CALLER_RUN_CODE, &parsed, &limited),
                            );
                            match limited.status {
                                ToolOutputStatus::Error => {
                                    openslate_ptc::envelope_error(limited.content)
                                }
                                _ => openslate_ptc::envelope_result(serde_json::Value::String(
                                    limited.content,
                                )),
                            }
                        }
                    }
                })
            },
        );

        let limits = PtcLimits {
            timeout_ms: self.ptc.timeout_ms,
            memory_limit_bytes: self.ptc.memory_limit_bytes,
            max_output_bytes: self.ptc.max_output_bytes,
            max_tool_calls_per_run: self.ptc.max_tool_calls_per_run,
            max_lookup_calls: self.ptc.max_lookup_calls,
        };

        let outcome = openslate_ptc::run_code(&code, &bound, &catalog, &limits, bridge).await;

        // Fold the script's bridge calls into the global tool-call budget
        // (the run_code invocation itself is already counted in execute()).
        // usize → u32 with an explicit saturating fallback: a pathological
        // count beyond u32::MAX clamps instead of silently truncating via
        // `as` (unreachable in practice — the per-run budget is far smaller).
        let bridge_calls = u32::try_from(outcome.tool_calls).unwrap_or(u32::MAX);
        if bridge_calls > 0 {
            self.tool_call_count
                .fetch_add(bridge_calls, Ordering::Relaxed);
        }

        let logs = outcome.logs.join("\n");
        let content = if let Some(err) = &outcome.error {
            if logs.is_empty() {
                format!("[error] {err}")
            } else {
                format!("[error] {err}\n\n[logs]\n{logs}")
            }
        } else {
            let result = outcome.result.as_deref().unwrap_or("null");
            if logs.is_empty() {
                format!("[result]\n{result}")
            } else {
                format!("[logs]\n{logs}\n\n[result]\n{result}")
            }
        };
        let bytes = content.len();
        ToolOutput {
            content,
            bytes,
            duration_ms: start.elapsed().as_millis() as u64,
            status: if outcome.error.is_some() {
                ToolOutputStatus::Error
            } else {
                ToolOutputStatus::Success
            },
        }
    }
}

/// Apply the engine-level output cap ([`limit_tool_output`]) to a PTC
/// bridge tool output — the same truncation the direct path applies in the
/// runtime loop (PTC_PLAN.md §6.5), instead of an ad-hoc slice. The byte
/// count is taken from the actual content so tools that report
/// `bytes = 0` with a large payload are still capped.
fn truncate_bridge_output(output: ToolOutput, max: usize) -> ToolOutput {
    let bytes = output.content.len();
    limit_tool_output(ToolOutput { bytes, ..output }, max)
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
        // Approval gate — the single choke point every tool call passes
        // through, at the very top (BEFORE the call_agent branch and every
        // budget check), so child-agent layers (same runner) and `run_code`
        // (PTC) are covered too; the PTC bridge's inner sandbox calls are
        // deliberately covered by the one approval granted to `run_code`
        // (always assessed high-risk). A denial is errors-as-data: the
        // model sees the refusal with guidance and self-heals; the
        // consecutive-denial watchdog aborts runs that keep retrying.
        let agent_id = self.current_agent_id();
        if let ApprovalDecision::Denied(reason) = self.approval.check(name, args, &agent_id.0) {
            return self.denied_output(name, args, reason);
        }
        self.clear_denials(name);
        if name == "call_agent" {
            return self.handle_call_agent(args).await;
        }
        // Main-loop enforcement of the global `max_tool_calls` budget
        // (PTC_PLAN.md §6.2): this runs before every tool execution — i.e.
        // at each step boundary of the run loop — with the same
        // pre-check-then-reserve semantics as `check_limits`
        // (current >= max rejects). A run_code script folding its bridge
        // calls over the budget makes subsequent calls (direct or another
        // run_code) fail here too, so exceeding via the bridge also stops
        // the run coherently.
        let current_tool_calls = self.tool_call_count.load(Ordering::Relaxed);
        if self.limits.max_tool_calls > 0 && current_tool_calls >= self.limits.max_tool_calls {
            return self.error_output(format!(
                "max tool calls exceeded ({current_tool_calls}/{}): \
                 tool '{name}' not executed — finish with your final answer",
                self.limits.max_tool_calls
            ));
        }
        // Count every (attempted) direct tool call toward the global
        // max_tool_calls budget; a run_code script folds its inner bridge
        // calls in via handle_run_code. call_agent is excluded (own budget,
        // above).
        self.tool_call_count.fetch_add(1, Ordering::Relaxed);
        // PTC interception (PTC_PLAN.md §3, 收口②): run_code executes the
        // model's code in the sandbox instead of dispatching to the registry.
        // Only when PTC is enabled; disabled, it falls through to the
        // registry's unknown-tool error.
        if name == RUN_CODE_TOOL && self.ptc.enabled {
            let output = self.handle_run_code(args).await;
            self.record_tool_audit(RUN_CODE_TOOL, TOOL_CALLER_DIRECT, args, &output);
            return output;
        }
        // Hallucination guard (收口④): the model may still emit a direct
        // call to a ptc-only tool it saw in an earlier turn or invented —
        // reject it with an actionable message instead of executing. Only a
        // REGISTERED tool gets the ptc-only hint; an unregistered name that
        // merely matches a ptc-only glob must surface the unknown-tool
        // error below.
        if self.ptc.enabled
            && !resolve_tool_mode(&self.ptc.tool_modes, name, true).direct_visible()
            && self.tool_registry.contains(name)
        {
            return self.error_output(format!(
                "tool '{name}' is ptc-only: call it from code inside {RUN_CODE_TOOL}"
            ));
        }
        // Delegate all other tools to the shared registry, converting errors
        // into error ToolOutputs exactly as ToolRegistry's own ToolExecutor
        // impl does (so the runtime loop keeps going gracefully). Every
        // dispatched execution is audited with direct-caller attribution.
        let output = match self.tool_registry.execute(name, args).await {
            Ok(output) => output,
            Err(e) => ToolOutput {
                content: format!("Error: {}", e),
                bytes: 0,
                duration_ms: 0,
                status: ToolOutputStatus::Error,
            },
        };
        self.record_tool_audit(name, TOOL_CALLER_DIRECT, args, &output);
        output
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

    // ── Cost aggregation across delegation (P2-3) ────────────────────────

    /// Config where `main` and `fast` carry DIFFERENT prices, so a mixed
    /// delegation must price each layer at its own rate.
    fn priced_test_config() -> OpenSlateConfig {
        let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "mock-model"
input_price_per_mtok = 2.0
output_price_per_mtok = 4.0

[models.fast]
provider = "mock"
model = "mock-fast"
input_price_per_mtok = 0.5
output_price_per_mtok = 1.0
"#;
        crate::config::parse_openslate_toml(toml).expect("config should parse")
    }

    fn response_with_usage(
        content: Option<&str>,
        tool_calls: Vec<ToolCall>,
        usage: Usage,
    ) -> ModelResponse {
        let has_calls = !tool_calls.is_empty();
        ModelResponse {
            content: content.map(|c| c.to_owned()),
            tool_calls,
            usage: Some(usage),
            finish_reason: Some(if has_calls { "tool_calls" } else { "stop" }.into()),
        }
    }

    #[tokio::test]
    async fn runner_prices_mixed_model_delegation_per_layer() {
        // root step 1 (main, 1000 in / 100 out → $2e-3·1 + $4e-6·0.1k = 0.0024)
        //   → call_agent(child)
        // child step 1 (fast, 500 in / 50 out → 0.5e-6·500 + 1e-6·50 = 0.0003)
        // root step 2 (main, 2000 in / 200 out → 0.0048)
        // run total = 0.0024 + 0.0003 + 0.0048 = 0.0075
        let provider = ScriptedProvider::new(vec![
            response_with_usage(
                None,
                vec![call_agent_tool_call("ca-1", "child", "greet")],
                Usage {
                    input_tokens: 1_000,
                    output_tokens: 100,
                },
            ),
            response_with_usage(
                Some("Hello from child"),
                vec![],
                Usage {
                    input_tokens: 500,
                    output_tokens: 50,
                },
            ),
            response_with_usage(
                Some("Delegation done"),
                vec![],
                Usage {
                    input_tokens: 2_000,
                    output_tokens: 200,
                },
            ),
        ]);

        let config = priced_test_config();
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

        let result = runner
            .run_root(vec![user_message("delegate please")], None)
            .await
            .expect("run ok");
        assert_eq!(result.status, RunStatus::Completed);

        // The root's own RunResult prices only the root layer...
        assert!(
            (result.total_cost_usd - 0.0072f64).abs() < 1e-12,
            "root layer cost = 0.0024 + 0.0048, got {}",
            result.total_cost_usd
        );
        // ...while the runner aggregates the child's fast-priced spend in.
        assert!(
            (runner.total_cost_usd() - 0.0075f64).abs() < 1e-12,
            "run-wide cost must include the child layer, got {}",
            runner.total_cost_usd()
        );
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

    // ── PTC (run_code) ────────────────────────────────────────────────────

    /// A tool that gets configured ptc-only in tests.
    struct SearchWebTool;
    #[async_trait]
    impl crate::tool::Tool for SearchWebTool {
        fn name(&self) -> &str {
            "search_web"
        }
        fn description(&self) -> &str {
            "Search the web"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"q": {"type": "string"}}})
        }
        async fn execute(
            &self,
            _args: &serde_json::Value,
        ) -> Result<ToolOutput, crate::error::ToolError> {
            Ok(ToolOutput {
                content: "web results".into(),
                bytes: 0,
                duration_ms: 0,
                status: ToolOutputStatus::Success,
            })
        }
    }

    /// A tool that counts its invocations so tests can assert how many calls
    /// actually crossed the PTC bridge.
    struct CountingEchoTool {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl crate::tool::Tool for CountingEchoTool {
        fn name(&self) -> &str {
            "mock_echo"
        }
        fn description(&self) -> &str {
            "Echo its arguments back"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"x": {"type": "number"}}})
        }
        async fn execute(
            &self,
            args: &serde_json::Value,
        ) -> Result<ToolOutput, crate::error::ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput {
                content: format!("echo:{}", args["x"]),
                bytes: 0,
                duration_ms: 0,
                status: ToolOutputStatus::Success,
            })
        }
    }

    /// test_config + `[ptc] enabled = true` plus extra TOML appended inside
    /// the `[ptc]` table (must come before any `[ptc.tool_modes]` subtable).
    fn ptc_config(extra: &str) -> OpenSlateConfig {
        let toml = format!(
            r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "mock-model"

[models.fast]
provider = "mock"
model = "mock-fast"

[ptc]
enabled = true
{extra}"#
        );
        crate::config::parse_openslate_toml(&toml).expect("ptc config should parse")
    }

    #[test]
    fn ptc_disabled_by_default_definitions_unchanged() {
        // No [ptc] section → PtcConfig::default() → disabled: no run_code,
        // no mode filtering.
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

        let defs = runner.tool_definitions_for(&AgentId("root".into()));
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"echo"));
        assert!(
            !names.contains(&"run_code"),
            "run_code must not appear when ptc is disabled: {names:?}"
        );
    }

    #[test]
    fn ptc_enabled_filters_ptc_only_and_injects_run_code() {
        let config = ptc_config("\n[ptc.tool_modes]\nsearch_web = \"ptc\"\n");
        let tree = root_only_tree();
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        registry.register(SearchWebTool);
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
        assert!(
            names.contains(&"run_code"),
            "run_code injected when a code-callable tool exists: {names:?}"
        );
        assert!(
            !names.contains(&"search_web"),
            "ptc-only tool hidden from the direct list: {names:?}"
        );
        assert!(
            names.contains(&"echo"),
            "both-mode tool stays directly visible: {names:?}"
        );
        let run_code_def = defs
            .iter()
            .find(|d| d.name == "run_code")
            .expect("run_code definition");
        assert!(
            run_code_def.description.contains("declare const tools:"),
            "TS declarations must be inlined, got: {}",
            run_code_def.description
        );
        assert!(run_code_def.description.contains("echo"));
        assert!(
            run_code_def.description.contains("search_web"),
            "ptc-only tool is code-callable, so it MUST appear in the \
             code-mode declarations: {}",
            run_code_def.description
        );
    }

    #[tokio::test]
    async fn ptc_only_direct_call_is_rejected() {
        let config = ptc_config("\n[ptc.tool_modes]\nsearch_web = \"ptc\"\n");
        let tree = root_only_tree();
        let mut registry = ToolRegistry::new();
        registry.register(SearchWebTool);
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

        let out = runner
            .execute("search_web", &serde_json::json!({"q": "x"}))
            .await;
        assert_eq!(out.status, ToolOutputStatus::Error);
        assert!(
            out.content.contains("ptc-only"),
            "expected ptc-only guard message, got: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn run_code_executes_tool_through_bridge() {
        let config = ptc_config("");
        let tree = root_only_tree();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(CountingEchoTool {
            calls: calls.clone(),
        });
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

        let out = runner
            .execute(
                "run_code",
                &serde_json::json!({
                    "code": "async () => { const r = await tools.mock_echo({x:1}); return r; }"
                }),
            )
            .await;

        assert_eq!(
            out.status,
            ToolOutputStatus::Success,
            "run_code content: {}",
            out.content
        );
        assert!(
            out.content.contains("[result]"),
            "result section missing, got: {}",
            out.content
        );
        assert!(
            out.content.contains("echo:1"),
            "tool return value missing, got: {}",
            out.content
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one bridge call");
        // One model-facing run_code invocation + one inner bridge call.
        assert_eq!(runner.tool_call_count(), 2);
    }

    #[tokio::test]
    async fn run_code_enforces_tool_call_budget() {
        // Budget of 1 + code calling the tool twice: the second bridge call
        // gets a budget-exceeded envelope, which throws inside the sandbox
        // and surfaces as an error the model can self-heal from.
        let config = ptc_config("max_tool_calls_per_run = 1\n");
        let tree = root_only_tree();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(CountingEchoTool {
            calls: calls.clone(),
        });
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

        let out = runner
            .execute(
                "run_code",
                &serde_json::json!({
                    "code": "async () => { const a = await tools.mock_echo({x:1}); \
                             const b = await tools.mock_echo({x:2}); return b; }"
                }),
            )
            .await;

        assert_eq!(
            out.status,
            ToolOutputStatus::Error,
            "budget overrun must be an error output, got: {}",
            out.content
        );
        assert!(
            out.content.contains("budget exceeded"),
            "budget message missing, got: {}",
            out.content
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only the first call reached the tool"
        );
    }

    #[tokio::test]
    async fn run_code_without_code_argument_is_a_usage_error() {
        let config = ptc_config("");
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

        let out = runner.execute("run_code", &serde_json::json!({})).await;
        assert_eq!(out.status, ToolOutputStatus::Error);
        assert!(
            out.content.contains("'code'"),
            "usage hint missing, got: {}",
            out.content
        );
    }

    // ── PTC P2: namespaces, disclosure tiers, lookup budget ──────────────

    /// A namespaced mock tool: registry name `github_list_prs`, PTC sandbox
    /// path `tools.github.list_prs`.
    struct GithubListPrsTool {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl crate::tool::Tool for GithubListPrsTool {
        fn name(&self) -> &str {
            "github_list_prs"
        }
        fn namespace(&self) -> Option<String> {
            Some("github".to_string())
        }
        fn description(&self) -> &str {
            "List pull requests"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "state": {"type": "string", "enum": ["open", "closed", "all"]} },
                "required": ["state"]
            })
        }
        async fn execute(
            &self,
            args: &serde_json::Value,
        ) -> Result<ToolOutput, crate::error::ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let state = args["state"].as_str().unwrap_or("all");
            Ok(ToolOutput {
                content: format!("prs[{state}]=3"),
                bytes: 0,
                duration_ms: 0,
                status: ToolOutputStatus::Success,
            })
        }
    }

    /// Flat mock tool configured ptc-only via tool_modes.
    struct MockSearchTool;
    #[async_trait]
    impl crate::tool::Tool for MockSearchTool {
        fn name(&self) -> &str {
            "mock_search"
        }
        fn description(&self) -> &str {
            "Search the index"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "q": {"type": "string"}, "limit": {"type": "integer"} }
            })
        }
        async fn execute(
            &self,
            _args: &serde_json::Value,
        ) -> Result<ToolOutput, crate::error::ToolError> {
            Ok(ToolOutput {
                content: "index hits".into(),
                bytes: 0,
                duration_ms: 0,
                status: ToolOutputStatus::Success,
            })
        }
    }

    #[tokio::test]
    async fn run_code_namespaced_tool_dispatches_under_registry_name() {
        let config = ptc_config("");
        let tree = root_only_tree();
        let gh_calls = Arc::new(AtomicUsize::new(0));
        let echo_calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(GithubListPrsTool {
            calls: gh_calls.clone(),
        });
        registry.register(CountingEchoTool {
            calls: echo_calls.clone(),
        });
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

        // The namespaced tool is reached via the composed path
        // `tools.github.list_prs`; a wrong dispatch name would surface as
        // "tool not available in code mode" instead of the real result.
        let out = runner
            .execute(
                "run_code",
                &serde_json::json!({
                    "code": "async () => { const prs = await tools.github.list_prs({ state: \"open\" }); \
                             const echo = await tools.mock_echo({x: 7}); return prs + \" | \" + echo; }"
                }),
            )
            .await;

        assert_eq!(
            out.status,
            ToolOutputStatus::Success,
            "run_code content: {}",
            out.content
        );
        assert!(
            out.content.contains("prs[open]=3"),
            "namespaced call result missing, got: {}",
            out.content
        );
        assert!(
            out.content.contains("echo:7"),
            "flat call result missing, got: {}",
            out.content
        );
        assert_eq!(gh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(echo_calls.load(Ordering::SeqCst), 1);

        // The declarations group the namespaced tool under `github: {`.
        let defs = runner.tool_definitions_for(&AgentId("root".into()));
        let run_code_def = defs
            .iter()
            .find(|d| d.name == "run_code")
            .expect("run_code definition");
        assert!(
            run_code_def.description.contains("github: {"),
            "namespace group missing from declarations: {}",
            run_code_def.description
        );
    }

    #[test]
    fn ptc_disclosure_auto_demotes_both_tools_over_budget() {
        // Tool set: one ptc-only tool (stays full) + three both tools
        // (demote to comment catalog lines when over budget).
        let registry_tools = || {
            let echo_calls = Arc::new(AtomicUsize::new(0));
            let mut registry = ToolRegistry::new();
            registry.register(MockSearchTool);
            registry.register(EchoTool);
            registry.register(SearchWebTool);
            registry.register(CountingEchoTool { calls: echo_calls });
            registry
        };
        let tree = root_only_tree();
        let skills = SkillsCatalog::default();
        let agent_id = AgentId("root".into());
        let modes = "\n[ptc.tool_modes]\nmock_search = \"ptc\"\n";

        // Compute a budget that lands in the auto tier's MIXED branch using
        // the same renderer the runner uses: full render must overflow it,
        // while ptc-only-full + comment lines must fit.
        let probe_registry = registry_tools();
        let probe_config = ptc_config(modes);
        let probe = AgentRunner::new(
            &NoopProvider,
            &tree,
            &probe_registry,
            &skills,
            &probe_config,
            RuntimeLimits::default(),
            RunId("t".into()),
        );
        let infos = probe.ptc_infos(&probe.ptc_code_tool_defs(&agent_id));
        let full = openslate_ptc::generate_declarations(&infos);
        let ptc_only: Vec<PtcToolInfo> = infos
            .iter()
            .filter(|i| i.name == "mock_search")
            .cloned()
            .collect();
        let base = openslate_ptc::generate_declarations(&ptc_only);
        let budget = base.chars().count() + 150;
        assert!(
            budget < full.chars().count(),
            "test premise: both-tool signatures ({}) must exceed the comment-line \
             allowance (150); full={budget}",
            full.chars().count()
        );
        drop(probe);

        let config = ptc_config(&format!("max_list_chars = {budget}\n{modes}"));
        let registry = registry_tools();
        let runner = AgentRunner::new(
            &NoopProvider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        );
        let defs = runner.tool_definitions_for(&agent_id);
        let desc = &defs
            .iter()
            .find(|d| d.name == "run_code")
            .expect("run_code definition")
            .description;

        // ptc-only tool keeps its full signature...
        assert!(
            desc.contains("mock_search: (input:"),
            "ptc-only tool must stay a full signature, got: {desc}"
        );
        // ...while both tools demote to comment catalog lines...
        assert!(
            desc.contains("// search_web: Search the web"),
            "both tool not demoted to a catalog comment line, got: {desc}"
        );
        assert!(
            !desc.contains("search_web: (input:"),
            "demoted tool must not keep its full signature, got: {desc}"
        );
        // ...and the discovery hint tells the model how to look them up.
        assert!(
            desc.contains("describe_tool(name)"),
            "discovery hint missing, got: {desc}"
        );
    }

    #[test]
    fn ptc_disclosure_catalog_only_shows_catalog_and_hint() {
        let config = ptc_config("disclosure = \"catalog\"\n");
        let tree = root_only_tree();
        let mut registry = ToolRegistry::new();
        registry.register(MockSearchTool);
        registry.register(SearchWebTool);
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
        let desc = &defs
            .iter()
            .find(|d| d.name == "run_code")
            .expect("run_code definition")
            .description;
        assert!(
            desc.contains("mock_search: Search the index"),
            "catalog line missing, got: {desc}"
        );
        assert!(
            desc.contains("search_web: Search the web"),
            "catalog line missing, got: {desc}"
        );
        assert!(
            !desc.contains("declare const"),
            "catalog tier must not inline signatures, got: {desc}"
        );
        assert!(
            desc.contains("describe_tool(name)"),
            "discovery hint missing, got: {desc}"
        );
    }

    #[tokio::test]
    async fn run_code_lookup_budget_is_enforced() {
        // max_lookup_calls = 1: the second describe_tool returns the budget
        // string as data (it does not throw); lookups never touch the tool
        // bridge, so the mock tool's counter stays at zero.
        let config = ptc_config("max_lookup_calls = 1\n");
        let tree = root_only_tree();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(CountingEchoTool {
            calls: calls.clone(),
        });
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

        let out = runner
            .execute(
                "run_code",
                &serde_json::json!({
                    "code": "async () => { const first = describe_tool(\"mock_echo\"); \
                             const second = describe_tool(\"mock_echo\"); return second; }"
                }),
            )
            .await;

        assert_eq!(
            out.status,
            ToolOutputStatus::Success,
            "budget string is data, not an error, got: {}",
            out.content
        );
        assert!(
            out.content.contains("lookup budget exceeded (2/1)"),
            "budget message missing, got: {}",
            out.content
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "lookups must not consume the tool-call budget"
        );
    }

    // ── Global max_tool_calls enforcement in the main loop (A7) ──────────

    fn echo_tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId(id.into()),
            name: "mock_echo".into(),
            arguments: serde_json::json!({"x": 1}),
        }
    }

    #[tokio::test]
    async fn runner_max_tool_calls_enforced_in_main_loop() {
        // max_tool_calls = 2 while the model keeps calling tools: calls
        // beyond the budget must return the limit error (and not execute),
        // so the run finishes with the limit feedback long before
        // max_steps instead of burning the step budget.
        let provider = ScriptedProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![echo_tool_call("tc-1")],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: None,
                tool_calls: vec![echo_tool_call("tc-2")],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: None,
                tool_calls: vec![echo_tool_call("tc-3")],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: None,
                tool_calls: vec![echo_tool_call("tc-4")],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            assistant_text("done after limits"),
        ]);

        let config = test_config();
        let tree = root_only_tree();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(CountingEchoTool {
            calls: calls.clone(),
        });
        let skills = SkillsCatalog::default();
        let limits = RuntimeLimits {
            max_tool_calls: 2,
            max_steps: 50,
            ..RuntimeLimits::default()
        };
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            limits,
            RunId("t".into()),
        );

        let result = runner
            .run_root(vec![user_message("keep calling tools")], None)
            .await
            .expect("run ok");

        // The run terminated on the model's final answer (5 steps), far
        // below max_steps = 50 — not by running out of steps.
        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 5);

        let tool_msgs: Vec<&Message> = result
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 4, "four tool-call turns happened");
        assert!(tool_msgs[0].content.contains("echo:1"));
        assert!(tool_msgs[1].content.contains("echo:1"));
        // Calls 3 and 4 hit the limit error instead of executing.
        for msg in &tool_msgs[2..] {
            assert!(
                msg.content.contains("max tool calls exceeded"),
                "expected limit error, got: {}",
                msg.content
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2, "only 2 executions");
        assert_eq!(runner.tool_call_count(), 2);
    }

    // ── Audit records + shared bridge truncation (A8) ────────────────────

    #[tokio::test]
    async fn run_code_bridge_calls_are_audited_with_caller_attribution() {
        let config = ptc_config("");
        let tree = root_only_tree();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(CountingEchoTool {
            calls: calls.clone(),
        });
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

        let out = runner
            .execute(
                "run_code",
                &serde_json::json!({
                    "code": "async () => { const r = await tools.mock_echo({x:1}); return r; }"
                }),
            )
            .await;
        assert_eq!(out.status, ToolOutputStatus::Success, "{}", out.content);

        let records = runner.tool_audit_records();
        // The sandbox-originated tool call must produce an audit record
        // attributed to run_code…
        let bridge = records
            .iter()
            .find(|r| r.tool_name == "mock_echo")
            .expect("bridge call audited");
        assert_eq!(bridge.caller, "run_code");
        assert!(bridge.arguments_json.contains("\"x\":1"));
        assert_eq!(bridge.output_status, "success");
        // …and the model-facing run_code invocation is audited as direct.
        let direct = records
            .iter()
            .find(|r| r.tool_name == "run_code")
            .expect("run_code invocation audited");
        assert_eq!(direct.caller, "direct");
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn truncate_bridge_output_under_limit_passes_through() {
        let output = ToolOutput {
            content: "small".into(),
            bytes: 0, // tools that misreport bytes must not dodge the cap
            duration_ms: 7,
            status: ToolOutputStatus::Success,
        };
        let limited = truncate_bridge_output(output.clone(), 100);
        assert_eq!(limited.content, "small");
        assert_eq!(limited.status, ToolOutputStatus::Success);
        assert_eq!(limited.duration_ms, 7);
    }

    #[test]
    fn truncate_bridge_output_over_limit_shares_engine_truncation() {
        let output = ToolOutput {
            content: "z".repeat(10_000),
            bytes: 0,
            duration_ms: 3,
            status: ToolOutputStatus::Success,
        };
        let limited = truncate_bridge_output(output, 200);
        assert_eq!(limited.status, ToolOutputStatus::Truncated);
        assert!(
            limited.content.contains("[TRUNCATED: original 10000 bytes"),
            "marker: {}",
            limited.content
        );
        assert!(limited.content.len() < 1_000);
        assert_eq!(limited.status, ToolOutputStatus::Truncated);
    }

    // ── Hallucination guard: unknown tool vs ptc-only (A10) ──────────────

    #[tokio::test]
    async fn unregistered_tool_matching_ptc_glob_reports_unknown_tool() {
        // `github_*` is ptc-only, but `github_nope` is not registered: the
        // model must see the unknown-tool error, not a misleading
        // "is ptc-only" hint.
        let config = ptc_config("\n[ptc.tool_modes]\n\"github_*\" = \"ptc\"\n");
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

        let out = runner.execute("github_nope", &serde_json::json!({})).await;
        assert_eq!(out.status, ToolOutputStatus::Error);
        assert!(
            out.content.contains("not found") && !out.content.contains("ptc-only"),
            "expected unknown-tool error, got: {}",
            out.content
        );
    }

    // ── Approval gating (Phase 1) ─────────────────────────────────────────

    use crate::approval::{ApprovalCallback, ApprovalRequest};

    /// Denies every approval request.
    struct DenyAll;
    impl ApprovalCallback for DenyAll {
        fn decide(&self, _req: &ApprovalRequest) -> ApprovalDecision {
            ApprovalDecision::Denied("denied by test policy".to_owned())
        }
    }

    /// Denies only the named tool; approves everything else.
    struct DenyTool(&'static str);
    impl ApprovalCallback for DenyTool {
        fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision {
            if req.tool_name == self.0 {
                ApprovalDecision::Denied(format!("{} is not allowed here", self.0))
            } else {
                ApprovalDecision::Approved
            }
        }
    }

    /// Denies the first N approval requests, approves the rest.
    struct DenyFirstN {
        remaining: Mutex<u32>,
    }
    impl ApprovalCallback for DenyFirstN {
        fn decide(&self, _req: &ApprovalRequest) -> ApprovalDecision {
            let mut remaining = self.remaining.lock().expect("remaining poisoned");
            if *remaining > 0 {
                *remaining -= 1;
                ApprovalDecision::Denied("first call denied".to_owned())
            } else {
                ApprovalDecision::Approved
            }
        }
    }

    fn manual_with(callback: Arc<dyn ApprovalCallback>) -> ApprovalManager {
        ApprovalManager::manual().with_callback(callback)
    }

    fn echo_call(id: &str, x: i64) -> ToolCall {
        ToolCall {
            id: ToolCallId(id.into()),
            name: "mock_echo".into(),
            arguments: serde_json::json!({"x": x}),
        }
    }

    fn tool_call_response(calls: Vec<ToolCall>) -> ModelResponse {
        ModelResponse {
            content: None,
            tool_calls: calls,
            usage: None,
            finish_reason: Some("tool_calls".into()),
        }
    }

    #[tokio::test]
    async fn approval_manual_blocks_run_code_under_ptc() {
        // PTC enabled + manual policy: the run_code invocation itself is
        // gated (hard-coded high risk) — the sandbox must never start, so
        // the bound tool is never reached through the bridge either.
        let config = ptc_config("");
        let tree = root_only_tree();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(CountingEchoTool {
            calls: calls.clone(),
        });
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &NoopProvider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        )
        .with_approval(manual_with(Arc::new(DenyAll)));

        let out = runner
            .execute(
                "run_code",
                &serde_json::json!({
                    "code": "async () => { return await tools.mock_echo({x:1}); }"
                }),
            )
            .await;

        assert_eq!(out.status, ToolOutputStatus::Error);
        assert!(
            out.content.contains("approval denied tool 'run_code'"),
            "expected denial message, got: {}",
            out.content
        );
        assert!(
            out.content.contains("不要重试同一调用"),
            "denial must tell the model not to retry, got: {}",
            out.content
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "tool must not run");
        assert_eq!(
            runner.tool_call_count(),
            0,
            "denied calls do not consume the tool-call budget"
        );
        // The denial is audited (errors-as-data, direct attribution).
        let records = runner.tool_audit_records();
        let denied = records
            .iter()
            .find(|r| r.tool_name == "run_code")
            .expect("denial audited");
        assert_eq!(denied.output_status, "error");
        assert_eq!(denied.caller, "direct");
    }

    #[tokio::test]
    async fn approval_gate_covers_call_agent_calls() {
        // Manual + deny-all: even the call_agent dispatch itself is denied
        // before handle_call_agent runs (gate sits above the branch).
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
        )
        .with_approval(manual_with(Arc::new(DenyAll)));

        let out = runner
            .execute(
                "call_agent",
                &serde_json::json!({"agent_id": "child", "task": "x"}),
            )
            .await;
        assert_eq!(out.status, ToolOutputStatus::Error);
        assert!(
            out.content.contains("approval denied tool 'call_agent'"),
            "got: {}",
            out.content
        );
        assert_eq!(runner.child_call_count(), 0, "no child spawned");
        assert_eq!(runner.execution_tree().node_count(), 1);
    }

    #[tokio::test]
    async fn approval_propagates_to_child_agent_tool_calls() {
        // root delegates to child; the child's own tool call goes through
        // the SAME runner (and thus the same approval manager) and is
        // denied there. The child sees the refusal as data and finishes;
        // the root still completes.
        let provider = ScriptedProvider::new(vec![
            // root: delegate to child
            tool_call_response(vec![call_agent_tool_call("ca-1", "child", "do it")]),
            // child: calls mock_echo (denied)
            tool_call_response(vec![echo_call("tc-child", 1)]),
            // child: recovers with a final answer
            assistant_text("child could not echo"),
            // root: final answer
            assistant_text("root done"),
        ]);

        let config = test_config();
        let tree = root_child_tree();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(CountingEchoTool {
            calls: calls.clone(),
        });
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        )
        .with_approval(manual_with(Arc::new(DenyTool("mock_echo"))));

        let result = runner
            .run_root(vec![user_message("delegate")], None)
            .await
            .expect("run ok");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "child's tool call must not execute"
        );
        // The child layer ran (its denial happened inside the child's own
        // loop, via the shared runner).
        assert_eq!(runner.execution_tree().node_count(), 2);
        let records = runner.tool_audit_records();
        let denied = records
            .iter()
            .find(|r| r.tool_name == "mock_echo")
            .expect("child-layer denial audited");
        assert_eq!(denied.output_status, "error");
        assert_eq!(denied.caller, "direct");
    }

    #[tokio::test]
    async fn approval_watchdog_aborts_run_after_three_consecutive_denials() {
        // The model keeps requesting the same denied tool; on the third
        // consecutive denial the watchdog aborts the whole run with an
        // error instead of looping forever.
        let provider = ScriptedProvider::new(vec![
            tool_call_response(vec![echo_call("tc-1", 1)]),
            tool_call_response(vec![echo_call("tc-2", 1)]),
            tool_call_response(vec![echo_call("tc-3", 1)]),
            // Never reached: the run aborts right after the third denial.
            assistant_text("done"),
        ]);

        let config = test_config();
        let tree = root_only_tree();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(CountingEchoTool {
            calls: calls.clone(),
        });
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        )
        .with_approval(manual_with(Arc::new(DenyAll)));

        let err = runner
            .run_root(vec![user_message("keep trying the tool")], None)
            .await
            .expect_err("watchdog must abort the run");
        assert!(
            matches!(
                &err,
                OpenSlateError::Runtime(crate::error::RuntimeError::ApprovalAbort {
                    tool_name,
                    denials: 3,
                    ..
                }) if tool_name == "mock_echo"
            ),
            "expected ApprovalAbort for mock_echo after 3 denials, got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("watchdog") && msg.contains("mock_echo"),
            "error should name the tool and the watchdog, got: {msg}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "tool never ran");
        // Root execution node is marked failed by run_root's error path.
        assert_eq!(
            runner.execution_tree().root().status,
            crate::execution::ExecutionStatus::Failed
        );
    }

    #[tokio::test]
    async fn denied_tool_result_self_heals_and_counter_resets() {
        // First request is denied (errors-as-data back to the model with
        // retry guidance); the second identical request is approved — the
        // consecutive-denial counter reset on the approval proves the
        // watchdog only counts CONSECUTIVE denials.
        let provider = ScriptedProvider::new(vec![
            tool_call_response(vec![echo_call("tc-1", 1)]),
            tool_call_response(vec![echo_call("tc-2", 2)]),
            assistant_text("recovered"),
        ]);

        let config = test_config();
        let tree = root_only_tree();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(CountingEchoTool {
            calls: calls.clone(),
        });
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        )
        .with_approval(manual_with(Arc::new(DenyFirstN {
            remaining: Mutex::new(1),
        })));

        let result = runner
            .run_root(vec![user_message("try the tool")], None)
            .await
            .expect("run ok");

        assert_eq!(result.status, RunStatus::Completed);
        let tool_msgs: Vec<&Message> = result
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 2, "two tool-call turns");
        assert!(
            tool_msgs[0]
                .content
                .contains("approval denied tool 'mock_echo'"),
            "got: {}",
            tool_msgs[0].content
        );
        assert!(
            tool_msgs[0].content.contains("不要重试同一调用"),
            "denial guidance missing, got: {}",
            tool_msgs[0].content
        );
        assert!(
            tool_msgs[1].content.contains("echo:2"),
            "second call must execute, got: {}",
            tool_msgs[1].content
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // ── Parallel batches + approval (P2-1) ───────────────────────────────

    /// Second registry tool for mixed parallel batches ("mock_echo" is
    /// CountingEchoTool's fixed name).
    struct CountingOtherTool {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl crate::tool::Tool for CountingOtherTool {
        fn name(&self) -> &str {
            "mock_other"
        }
        fn description(&self) -> &str {
            "Second tool for parallel-batch approval tests"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _args: &serde_json::Value,
        ) -> Result<ToolOutput, crate::error::ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput {
                content: "other done".into(),
                bytes: 11,
                duration_ms: 1,
                status: ToolOutputStatus::Success,
            })
        }
    }

    fn other_call(id: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId(id.into()),
            name: "mock_other".into(),
            arguments: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn parallel_batch_denial_is_independent_errors_as_data() {
        // One parallel batch mixes a denied tool and an approved sibling
        // (no call_agent/run_code → the P2-1 concurrent path): the denial
        // is errors-as-data for ITS OWN call only — the sibling executes
        // normally and the run completes (a single denial never trips the
        // watchdog).
        let provider = ScriptedProvider::new(vec![
            tool_call_response(vec![echo_call("tc-d", 1), other_call("tc-o")]),
            assistant_text("mixed batch done"),
        ]);

        let config = test_config();
        let tree = root_only_tree();
        let echo_calls = Arc::new(AtomicUsize::new(0));
        let other_calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(CountingEchoTool {
            calls: echo_calls.clone(),
        });
        registry.register(CountingOtherTool {
            calls: other_calls.clone(),
        });
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        )
        .with_approval(manual_with(Arc::new(DenyTool("mock_echo"))));

        let result = runner
            .run_root(vec![user_message("mixed batch")], None)
            .await
            .expect("run ok");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(
            echo_calls.load(Ordering::SeqCst),
            0,
            "denied tool never ran"
        );
        assert_eq!(other_calls.load(Ordering::SeqCst), 1, "sibling executed");

        // Both calls got their tool results, in tool_call order.
        let tool_msgs: Vec<&Message> = result
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 2);
        assert_eq!(tool_msgs[0].tool_call_id, Some(ToolCallId("tc-d".into())));
        assert!(
            tool_msgs[0]
                .content
                .contains("approval denied tool 'mock_echo'"),
            "denial guidance, got: {}",
            tool_msgs[0].content
        );
        assert_eq!(tool_msgs[1].tool_call_id, Some(ToolCallId("tc-o".into())));
        assert_eq!(tool_msgs[1].content, "other done");
    }

    #[tokio::test]
    async fn approval_watchdog_trips_across_parallel_batches() {
        // Three steps each carry a REAL parallel batch (denied mock_echo +
        // approved mock_other): the third consecutive mock_echo denial
        // trips the watchdog and aborts the run — concurrent dispatch
        // does not bypass the AgentRunner::execute choke point.
        let provider = ScriptedProvider::new(vec![
            tool_call_response(vec![echo_call("tc-1", 1), other_call("tc-o1")]),
            tool_call_response(vec![echo_call("tc-2", 1), other_call("tc-o2")]),
            tool_call_response(vec![echo_call("tc-3", 1), other_call("tc-o3")]),
            // Never reached: the run aborts right after the third denial.
            assistant_text("done"),
        ]);

        let config = test_config();
        let tree = root_only_tree();
        let echo_calls = Arc::new(AtomicUsize::new(0));
        let other_calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(CountingEchoTool {
            calls: echo_calls.clone(),
        });
        registry.register(CountingOtherTool {
            calls: other_calls.clone(),
        });
        let skills = SkillsCatalog::default();
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        )
        .with_approval(manual_with(Arc::new(DenyTool("mock_echo"))));

        let err = runner
            .run_root(vec![user_message("keep trying the tool")], None)
            .await
            .expect_err("watchdog must abort the run");
        assert!(
            matches!(
                &err,
                OpenSlateError::Runtime(crate::error::RuntimeError::ApprovalAbort {
                    tool_name,
                    denials: 3,
                    ..
                }) if tool_name == "mock_echo"
            ),
            "expected ApprovalAbort for mock_echo after 3 denials, got {err:?}"
        );
        assert_eq!(
            echo_calls.load(Ordering::SeqCst),
            0,
            "denied tool never ran"
        );
        // The approved sibling ran in every batch (the parallel path
        // dispatches it alongside each denial).
        assert_eq!(other_calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            runner.execution_tree().root().status,
            crate::execution::ExecutionStatus::Failed
        );
    }

    // ── Cancellation (Phase 4) ────────────────────────────────────────────

    /// Provider whose `generate` counts calls (proving a pre-cancelled run
    /// never reaches the provider).
    struct CountingProvider {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl ModelProvider for CountingProvider {
        async fn generate(
            &self,
            _request: GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(assistant_text("never expected"))
        }
        fn provider_name(&self) -> &str {
            "counting"
        }
    }

    #[tokio::test]
    async fn runner_pre_cancelled_token_returns_cancelled_error() {
        // RuntimeError::Cancelled's real trigger point: a token already
        // cancelled at run_root entry means the run never started — there is
        // no partial transcript to return as Interrupted.
        let provider = CountingProvider {
            calls: AtomicUsize::new(0),
        };
        let config = test_config();
        let tree = root_only_tree();
        let registry = ToolRegistry::new();
        let skills = SkillsCatalog::default();
        let token = CancellationToken::new();
        token.cancel();
        let runner = AgentRunner::new(
            &provider,
            &tree,
            &registry,
            &skills,
            &config,
            RuntimeLimits::default(),
            RunId("t".into()),
        )
        .with_cancel_token(token);

        let err = runner
            .run_root(vec![user_message("go")], None)
            .await
            .expect_err("pre-cancelled run must fail with Cancelled");
        assert!(
            matches!(
                err,
                OpenSlateError::Runtime(crate::error::RuntimeError::Cancelled)
            ),
            "expected RuntimeError::Cancelled, got {err:?}"
        );
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            0,
            "provider must not be called"
        );
    }

    /// Root uses non-streaming `generate` (progress=None in tests); the
    /// child layer streams via `generate_stream`. This provider scripts the
    /// root's responses and makes the child's stream stick forever after
    /// announcing it started — the exact shape a Ctrl-C interrupts.
    struct ChildStuckProvider {
        responses: Vec<ModelResponse>,
        generate_calls: AtomicUsize,
        child_started: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl ModelProvider for ChildStuckProvider {
        async fn generate(
            &self,
            _request: GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            let idx = self.generate_calls.fetch_add(1, Ordering::SeqCst);
            self.responses
                .get(idx)
                .cloned()
                .ok_or(ProviderError::ServerError(500))
        }

        async fn generate_stream(
            &self,
            _request: GenerateRequest,
        ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            self.child_started.notify_one();
            tokio::spawn(async move {
                // Hold the sender open forever (a real binding — `let _ =`
                // would NOT capture it, the channel would close instantly).
                let _tx = tx;
                std::future::pending::<()>().await;
            });
            rx
        }

        fn provider_name(&self) -> &str {
            "child-stuck"
        }
    }

    #[tokio::test]
    async fn runner_cancel_during_child_delegation_returns_interrupted() {
        // root delegates to child → child's stream sticks → token cancelled
        // → child returns Interrupted (partial) → its "[did not finish]"
        // tool output lands in the root transcript → the root's next
        // checkpoint observes the same token and stops the whole run.
        let provider = ChildStuckProvider {
            responses: vec![
                tool_call_response(vec![call_agent_tool_call("ca-1", "child", "sub-task")]),
                assistant_text("root final (never reached)"),
            ],
            generate_calls: AtomicUsize::new(0),
            child_started: Arc::new(tokio::sync::Notify::new()),
        };
        let token = CancellationToken::new();

        let canceller = {
            let token = token.clone();
            let started = Arc::clone(&provider.child_started);
            tokio::spawn(async move {
                started.notified().await;
                token.cancel();
            })
        };

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
        )
        .with_cancel_token(token);

        let result = runner
            .run_root(vec![user_message("delegate please")], None)
            .await
            .expect("cancelled delegation run still returns Ok");
        canceller.abort();

        assert_eq!(result.status, RunStatus::Interrupted);
        // The delegation hop is in the partial transcript with its pairing
        // intact — filled by a synthetic cancelled result (the in-flight
        // child future was dropped at the checkpoint, aborting it).
        let tool_msg = result
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("call_agent hop present");
        assert!(
            tool_msg.content.contains("cancelled"),
            "expected cancelled marker for the aborted delegation, got: {}",
            tool_msg.content
        );
        assert_eq!(
            tool_msg.tool_call_id,
            Some(ToolCallId("ca-1".into())),
            "tool result must pair with the call_agent call"
        );
        // The root never issued its second request (loop-top checkpoint).
        assert_eq!(
            provider.generate_calls.load(Ordering::SeqCst),
            1,
            "root's second provider call must not happen"
        );
    }
}
