//! Agent runtime loop.
//!
//! Executes a single agent: sends messages to the model, processes the response
//! (text or tool_calls), and loops until completion or limits are hit.
//! Tool batches run CONCURRENTLY since P2-1 (`[limits].parallel_tool_calls`,
//! default on): every order-sensitive concern (progress emission, sink writes,
//! the transcript) is coordinated strictly in tool_call order around the join.

use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesUnordered, StreamExt};
use futures_util::FutureExt;
use openslate_ptc::RUN_CODE_TOOL;

use crate::error::{OpenSlateError, ProviderError, RuntimeError};
use crate::provider::{GenerateRequest, ModelProvider, ProgressCallback};
use crate::tool::ToolExecutor;
use crate::types::*;

/// Cooperative cancellation token (Phase 4), threaded into
/// [`execute_run`] the same way the [`MessageSink`] is: built by the CLI
/// layer, carried through the [`RunManager`](crate::run_manager::RunManager)
/// and [`AgentRunner`](crate::runner::AgentRunner), and observed at the
/// loop's checkpoints. Re-exported so downstream crates can construct and
/// cancel tokens without depending on `tokio-util` directly.
pub use tokio_util::sync::CancellationToken;

/// Default maximum number of consecutive empty turns before failing.
pub const DEFAULT_MAX_EMPTY_TURNS: u32 = 3;

/// Per-step persistence sink (Phase 3).
///
/// Implemented by the CLI/store layer (e.g. the SQLite `RunRecorder`) and
/// injected through the [`RunManager`](crate::run_manager::RunManager). The
/// runtime invokes it **inline in the execute_run loop** — directly awaited
/// right after an assistant message or a tool result is appended to the live
/// conversation, before the loop continues ("落盘后再 continue") — so a
/// crash, cancellation, or timeout always leaves a resumable partial
/// transcript. It deliberately does NOT interact with the run-root watchdog
/// select or spawn any task of its own.
///
/// Implementations must be cheap, non-failing from the runtime's point of
/// view (swallow and log errors — persistence must never kill a run), and
/// must preserve write order.
#[async_trait::async_trait]
pub trait MessageSink: Send + Sync {
    /// Persist `message` (called in conversation order).
    async fn append(&self, message: &Message);
}

/// Await the sink for a freshly appended message (no-op when unset).
async fn sink_append(sink: Option<&dyn MessageSink>, message: &Message) {
    if let Some(sink) = sink {
        sink.append(message).await;
    }
}

/// Build the graceful-cancellation result: the run stops with
/// [`RunStatus::Interrupted`] and the partial transcript accumulated so far
/// (the same semantics as the `max_steps` interruption). Everything already
/// appended has also gone through the sink, so the partial transcript is on
/// durable storage and the run stays resumable.
fn interrupted_result(
    config: &RunConfig,
    messages: Vec<Message>,
    total_steps: u32,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cost_usd: f64,
) -> RunResult {
    RunResult {
        run_id: config.run_id.clone(),
        status: RunStatus::Interrupted,
        messages,
        total_steps,
        total_input_tokens,
        total_output_tokens,
        total_cost_usd,
    }
}

/// Resolve a `cancelled()` future from an optional token into a concrete
/// future, so `select!` branches stay uniform (`pending()` when no token is
/// wired — cancellation is simply never observed).
async fn token_cancelled(cancel: Option<&CancellationToken>) {
    match cancel {
        Some(token) => token.cancelled().await,
        None => std::future::pending().await,
    }
}

/// The synthetic tool result standing in for a call that never got a real
/// answer (see [`append_cancelled_tool_results`]).
fn cancelled_tool_message(tc: &ToolCall) -> Message {
    Message {
        role: MessageRole::Tool,
        content: "[tool call cancelled before completion]".to_owned(),
        tool_call_id: Some(tc.id.clone()),
        name: Some(tc.name.clone()),
        tool_calls: None,
    }
}

/// Append synthetic tool results for the tool calls of the current step that
/// never got a real answer (cancellation hit before/between/inside their
/// executions). Without these, the partial transcript would end on an
/// assistant message with dangling `tool_calls` — providers reject that, so
/// neither the in-memory history nor a resumed run could continue.
async fn append_cancelled_tool_results(
    sink: Option<&dyn MessageSink>,
    messages: &mut Vec<Message>,
    tool_calls: &[ToolCall],
    from_index: usize,
) {
    for tc in &tool_calls[from_index..] {
        let msg = cancelled_tool_message(tc);
        sink_append(sink, &msg).await;
        messages.push(msg);
    }
}

/// Runtime limits for a single agent run.
#[derive(Debug, Clone)]
pub struct RuntimeLimits {
    pub max_steps: u32,
    pub max_depth: u32,
    pub max_tool_calls: u32,
    pub max_child_agent_calls: u32,
    /// Round-level budget for TOTAL LLM request time, in milliseconds
    /// (fix-21): only the time spent awaiting the provider (streaming or
    /// not) accumulates against it. Approval waits (decide() blocking)
    /// and tool execution pause the budget. When a round's accumulated
    /// provider-await time exhausts it, the run fails with
    /// `RuntimeError::Timeout`. The provider layer keeps its own
    /// per-request semantics (streaming idle / non-streaming total,
    /// fix-20).
    pub timeout_ms: u64,
    pub max_context_bytes: u32,
    pub max_output_bytes: u32,
    pub max_empty_turns: u32,
    /// P2-1: direct tool calls within one step execute concurrently.
    /// `false` restores the fully sequential tool loop (escape hatch for
    /// order-sensitive workloads). Batches containing `call_agent` or
    /// `run_code` always run sequentially regardless.
    pub parallel_tool_calls: bool,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_steps: 0,
            max_depth: 4,
            max_tool_calls: 20,
            max_child_agent_calls: 8,
            timeout_ms: 60_000,
            max_context_bytes: 64_000,
            max_output_bytes: 65_536,
            max_empty_turns: DEFAULT_MAX_EMPTY_TURNS,
            parallel_tool_calls: true,
        }
    }
}

/// Check if runtime limits are exceeded. Returns `Ok(())` if within limits.
pub fn check_limits(
    limits: &RuntimeLimits,
    current_steps: u32,
    current_depth: u32,
    current_tool_calls: u32,
    current_child_calls: u32,
) -> Result<(), RuntimeError> {
    if limits.max_steps > 0 && current_steps >= limits.max_steps {
        return Err(RuntimeError::MaxStepsExceeded {
            max: limits.max_steps,
        });
    }
    if current_depth >= limits.max_depth {
        return Err(RuntimeError::MaxDepthExceeded {
            max: limits.max_depth,
        });
    }
    if current_tool_calls >= limits.max_tool_calls {
        return Err(RuntimeError::MaxToolCallsExceeded {
            max: limits.max_tool_calls,
        });
    }
    if current_child_calls >= limits.max_child_agent_calls {
        return Err(RuntimeError::MaxChildAgentCallsExceeded {
            max: limits.max_child_agent_calls,
        });
    }
    Ok(())
}

/// Per-model pricing snapshot (P2-3), resolved from
/// `[models.X].input_price_per_mtok` / `output_price_per_mtok` when the
/// [`RunConfig`] is constructed (in the runner — the same source of truth
/// as model resolution), so `execute_run` itself stays config-unaware.
///
/// v1 scope: standard input/output pricing only. Cache-read discounts and
/// other usage buckets are a known limitation — `Usage` carries plain
/// input/output totals today, so there is nothing finer to price.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CostSpec {
    /// USD per million input tokens; `None` prices input usage at 0.
    pub input_price_per_mtok: Option<f64>,
    /// USD per million output tokens; `None` prices output usage at 0.
    pub output_price_per_mtok: Option<f64>,
}

impl CostSpec {
    /// Build from the two optional per-mtok prices.
    pub fn from_prices(
        input_price_per_mtok: Option<f64>,
        output_price_per_mtok: Option<f64>,
    ) -> Self {
        Self {
            input_price_per_mtok,
            output_price_per_mtok,
        }
    }

    /// Whether any price is configured (drives the "pricing not
    /// configured" display on the CLI side).
    pub fn is_configured(&self) -> bool {
        self.input_price_per_mtok.is_some() || self.output_price_per_mtok.is_some()
    }

    /// Cost of one usage record in USD. Absent prices contribute 0, so an
    /// unconfigured spec yields 0.0 for any usage (the "未配置→成本记 0"
    /// rule).
    pub fn cost_of(&self, usage: &Usage) -> f64 {
        let input =
            self.input_price_per_mtok.unwrap_or(0.0) * usage.input_tokens as f64 / 1_000_000.0;
        let output =
            self.output_price_per_mtok.unwrap_or(0.0) * usage.output_tokens as f64 / 1_000_000.0;
        input + output
    }
}

/// Configuration for a single agent run.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub run_id: RunId,
    pub agent_id: AgentId,
    pub model_alias: String,
    pub system_prompt: Option<String>,
    pub initial_messages: Vec<Message>,
    pub max_steps: u32,
    pub max_context_bytes: u32,
    pub max_output_bytes: u32,
    pub max_empty_turns: u32,
    pub tool_definitions: Vec<crate::provider::ToolDefinition>,
    /// Round-level budget for TOTAL LLM request time in milliseconds
    /// (fix-21): the sum of every provider-await segment of the round
    /// (request dispatch through response/stream completion). Approval
    /// waits and tool execution do NOT consume it. If the accumulated
    /// LLM time exceeds it, the run returns `RuntimeError::Timeout`;
    /// `0` times out immediately.
    pub timeout_ms: u64,
    /// Recursion depth of this agent run (0 = root). Used only to indent
    /// tracing logs so child-agent activity is visually nested under its
    /// parent during recursive delegation.
    pub depth: u32,
    /// P2-1: run the direct tool calls of each step concurrently (see
    /// [`RuntimeLimits::parallel_tool_calls`]).
    pub parallel_tool_calls: bool,
    /// P2-3: pricing for THIS layer's model alias, applied to every
    /// usage record the loop accumulates (child agents resolve their own
    /// spec at their own RunConfig construction, so mixed main/fast
    /// delegation prices each layer correctly).
    pub cost: CostSpec,
}

/// Result of a completed agent run.
#[derive(Debug, Clone)]
pub struct RunResult {
    pub run_id: RunId,
    pub status: RunStatus,
    pub messages: Vec<Message>,
    pub total_steps: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Accumulated cost in USD for THIS layer's model calls (P2-3):
    /// each provider response's usage × `RunConfig::cost`. 0.0 when the
    /// model has no pricing configured.
    pub total_cost_usd: f64,
}

/// A single step in the execution loop.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub step_number: u32,
    pub model_response: ModelResponse,
    pub tool_outputs: Vec<ToolOutput>,
}

/// Execute an agent run to completion.
///
/// The loop:
/// 1. Build `GenerateRequest` from conversation history
/// 2. Call `ModelProvider::generate()`
/// 3. If response has content -> add assistant message, check for `tool_calls`
/// 4. If `tool_calls` -> execute each tool (via callback), add tool results as messages
/// 5. If no `tool_calls` and content present -> done (`Completed`)
/// 6. If `max_steps` reached -> `Interrupted`
/// 7. If `finish_reason` is `"stop"` -> `Completed`
///
/// Edge cases handled:
/// - Empty model response (no content, no tool_calls) counts toward max empty turns
/// - Unknown tool names produce a clear error message in tool output
/// - Malformed tool arguments (non-object) produce an error message in tool output
/// - Context exceeding max bytes is truncated to continue
/// - Tool execution panics are caught and reported as errors
/// - Multiple consecutive empty responses stop after max_empty_turns
///
/// When `sink` is set, every appended assistant message and tool result is
/// persisted through it before the loop continues (see [`MessageSink`]).
///
/// Tool batches (P2-1): when `config.parallel_tool_calls` is set (default),
/// the direct tool calls of one step execute concurrently; batches that
/// contain `call_agent` or `run_code` always run sequentially (name-based
/// dispatch decision, made at the top of the tool branch). Progress
/// callbacks fire in tool_call order around the join, and results enter the
/// conversation — and the sink — strictly in tool_call order, regardless of
/// completion order.
///
/// Cancellation (Phase 4): when `cancel` is set, the token is observed at
/// checkpoints — before every provider call, between stream events, and
/// around every tool execution (for a parallel batch: before it, and as a
/// branch raced against the whole batch). A cancelled token stops the run
/// gracefully with `Ok(RunStatus::Interrupted)` plus the partial
/// transcript; futures in flight (a provider request, a tool) are dropped
/// at the checkpoint, never the run itself — for a parallel batch the
/// already-completed tool outputs are drained and kept, and only the truly
/// unanswered calls get synthetic cancelled results. This is why the CLI
/// layer must NOT race the run future in a `select!` and drop it — that
/// would throw away everything.
///
/// Time budget (fix-21): `timeout_ms` bounds the round's TOTAL
/// provider-await time, not its wall clock. The loop accumulates only
/// the request segments (streaming: dispatch + event receive loop;
/// non-streaming: the `generate` await) into an LLM-time account and
/// wraps each request with the remaining `timeout_ms − accumulated`
/// budget. Approval waits (the decide() block inside tool execution) and
/// tool runs happen outside those segments, so a round with healthy
/// requests survives arbitrarily long approval queues and tool runs;
/// conversely, genuinely slow LLM time still trips `RuntimeError::Timeout`
/// once the accumulated total crosses the budget. The provider layer has
/// its own independent per-request semantics (streaming idle /
/// non-streaming total, fix-20). The runner (fix-22) delegates a FULL
/// independent budget to every recursion layer, so each `execute_run`
/// starts with a fresh LLM-time account.
///
/// Returns `RunResult` with final status and full conversation.
pub async fn execute_run(
    provider: &dyn ModelProvider,
    config: RunConfig,
    model_id: &str,
    tool_executor: &dyn ToolExecutor,
    sink: Option<&dyn MessageSink>,
    mut progress: Option<&mut dyn ProgressCallback>,
    cancel: Option<&CancellationToken>,
) -> Result<RunResult, OpenSlateError> {
    let mut messages = config.initial_messages.clone();
    let mut total_steps = 0u32;
    let mut total_input_tokens = 0u64;
    let mut total_output_tokens = 0u64;
    // P2-3: accumulated cost of this layer's model calls, priced with the
    // RunConfig's CostSpec at every usage record (below). f64 addition is
    // exact enough at these magnitudes; unconfigured pricing keeps it 0.0.
    let mut total_cost_usd = 0.0f64;
    let mut consecutive_empty_turns = 0u32;

    // fix-21 LLM-time budget: `config.timeout_ms` bounds the TOTAL time
    // this round spends AWAITING the provider (every step's request
    // segments combined), NOT the round's wall clock. Approval waits
    // (the decide() block inside tool execution) and tool runs pause the
    // budget — they happen outside the request segments below and never
    // touch `llm_time_accumulated`. Before each request the remainder is
    // `timeout_ms − accumulated`; an exhausted remainder fails the round
    // with `RuntimeError::Timeout` (same error and format as before).
    let mut llm_time_accumulated = Duration::ZERO;

    // Indent per recursion depth so child-agent logs nest under their parent,
    // and tag every line with the agent id so it's clear who is acting.
    let log_indent = "  ".repeat(config.depth as usize);
    let agent_tag = config.agent_id.0.clone();

    loop {
        // Cancellation checkpoint (Phase 4): a cancelled token stops the run
        // BEFORE the next provider call, returning the partial transcript
        // (aligned with the max_steps interruption semantics — the persisted
        // transcript keeps the run resumable).
        if cancel.is_some_and(|t| t.is_cancelled()) {
            return Ok(interrupted_result(
                &config,
                messages,
                total_steps,
                total_input_tokens,
                total_output_tokens,
                total_cost_usd,
            ));
        }

        if config.max_steps > 0 && total_steps >= config.max_steps {
            return Ok(RunResult {
                run_id: config.run_id.clone(),
                status: RunStatus::Interrupted,
                messages,
                total_steps,
                total_input_tokens,
                total_output_tokens,
                total_cost_usd,
            });
        }

        // Remaining pure-LLM budget for this request segment (fix-21):
        // `timeout_ms` minus the provider-await time already spent on
        // earlier steps. Exhausted (including `timeout_ms = 0`, the
        // immediate-timeout convention pinned by tests) → Timeout.
        let remaining_llm =
            Duration::from_millis(config.timeout_ms).saturating_sub(llm_time_accumulated);
        if remaining_llm.is_zero() {
            return Err(OpenSlateError::Runtime(RuntimeError::Timeout {
                timeout_ms: config.timeout_ms,
            }));
        }

        truncate_context_if_needed(&mut messages, config.max_context_bytes);

        let request = GenerateRequest {
            model_id: model_id.to_owned(),
            system_prompt: config.system_prompt.clone(),
            messages: messages.clone(),
            tools: config.tool_definitions.clone(),
            max_tokens: None,
            temperature: None,
        };

        // Log every LLM request — emitted once here so both the streaming and
        // non-streaming paths surface it (previously only the non-streaming
        // branch did, so `openslate run` with spinner showed no per-request line).
        if config.max_steps > 0 {
            tracing::info!(
                "{}Step {}/{} [{}]: requesting {}...",
                log_indent,
                total_steps + 1,
                config.max_steps,
                agent_tag,
                model_id
            );
        } else {
            tracing::info!(
                "{}Step {} [{}]: requesting {}...",
                log_indent,
                total_steps + 1,
                agent_tag,
                model_id
            );
        }

        let response = if let Some(cb) = progress.as_mut() {
            // --- Streaming path with progress callbacks ---
            cb.on_request_start(total_steps + 1, model_id);
            // Early input-token estimate so the UI can show ↑N during streaming,
            // before the provider's real usage arrives at stream end.
            cb.on_input_estimate(estimate_input_tokens(&request));

            // The LLM-time segment starts at request dispatch (fix-21):
            // default-trait providers run the whole request inline inside
            // `generate_stream` (it awaits `generate()`), real adapters
            // spawn and stream via the channel — both are provider
            // awaiting, so both belong to the budgeted segment. The
            // timeout wraps dispatch AND the receive loop together
            // (fix-22): with the dispatch outside, an inline default
            // `generate_stream` (exactly what child layers use via
            // ChildProgress) ran the whole request unbounded and the
            // timeout only guarded the already-drained channel. Real
            // adapters are unaffected — their dispatch returns a
            // receiver immediately, so the bound is the same recv loop
            // it always was.
            let segment_start = Instant::now();
            let mut assembled: Option<ModelResponse> = None;
            // Set when the cancel token fires mid-stream; the partial turn is
            // discarded (no assistant message is appended for it) and the run
            // returns Interrupted with everything accumulated so far.
            let mut stream_cancelled = false;

            let timeout_result = tokio::time::timeout(remaining_llm, async {
                let mut rx = provider.generate_stream(request).await;
                let mut first_token = true;
                loop {
                    tokio::select! {
                        // Cancellation checkpoint between stream events
                        // (Phase 4): `biased` so an already-cancelled token
                        // wins even against a ready event.
                        _ = token_cancelled(cancel) => {
                            stream_cancelled = true;
                            break;
                        }
                        event = rx.recv() => {
                            let Some(event) = event else { break };
                            match event {
                                Ok(ModelStreamEvent::Delta(text)) => {
                                    if first_token {
                                        first_token = false;
                                        cb.on_first_token();
                                    }
                                    cb.on_delta(&text);
                                }
                                Ok(ModelStreamEvent::Reasoning(text)) => {
                                    cb.on_reasoning(&text);
                                }
                                Ok(ModelStreamEvent::Usage(usage)) => {
                                    cb.on_usage(usage);
                                }
                                Ok(ModelStreamEvent::Done(resp)) => {
                                    assembled = Some(resp);
                                }
                                Err(e) => return Err(e),
                            }
                        }
                    }
                }
                Ok::<_, ProviderError>(())
            })
            .await;

            // Bill the segment (dispatch + stream receive loop) against
            // the round's LLM budget (fix-21). Error outcomes return
            // below, so the accumulation only matters on the paths that
            // continue the loop.
            llm_time_accumulated += segment_start.elapsed();

            cb.on_request_end();

            match timeout_result {
                Ok(Ok(())) => {
                    if stream_cancelled {
                        return Ok(interrupted_result(
                            &config,
                            messages,
                            total_steps,
                            total_input_tokens,
                            total_output_tokens,
                            total_cost_usd,
                        ));
                    }
                    assembled.unwrap_or(ModelResponse {
                        content: None,
                        tool_calls: vec![],
                        usage: None,
                        finish_reason: None,
                    })
                }
                Ok(Err(e)) => return Err(OpenSlateError::from(e)),
                Err(_) => {
                    return Err(OpenSlateError::Runtime(RuntimeError::Timeout {
                        timeout_ms: config.timeout_ms,
                    }))
                }
            }
        } else {
            // --- Non-streaming path (the per-request "Step N" log is emitted
            //     above, before the streaming/non-streaming split) ---
            let call_start = Instant::now();
            // Cancellation checkpoint around the provider call (Phase 4):
            // the request future is dropped at the checkpoint — the run
            // itself (and its partial transcript) is not.
            let resp = tokio::select! {
                biased;
                _ = token_cancelled(cancel) => {
                    return Ok(interrupted_result(
                        &config,
                        messages,
                        total_steps,
                        total_input_tokens,
                        total_output_tokens,
                        total_cost_usd,
                    ));
                }
                resp = tokio::time::timeout(remaining_llm, provider.generate(request)) => {
                    // Bill the request segment against the LLM budget
                    // before any error propagation (fix-21) — on success
                    // the loop continues with the shrunken remainder.
                    llm_time_accumulated += call_start.elapsed();
                    resp.map_err(|_| {
                        OpenSlateError::Runtime(RuntimeError::Timeout {
                            timeout_ms: config.timeout_ms,
                        })
                    })??
                }
            };
            let call_elapsed = call_start.elapsed();

            if let Some(usage) = &resp.usage {
                tracing::info!(
                    "{}  [{}ms · {}in/{}out]",
                    log_indent,
                    call_elapsed.as_millis(),
                    usage.input_tokens,
                    usage.output_tokens
                );
            } else {
                tracing::info!("{}  [{}ms]", log_indent, call_elapsed.as_millis());
            }
            resp
        };

        if let Some(usage) = &response.usage {
            total_input_tokens += usage.input_tokens as u64;
            total_output_tokens += usage.output_tokens as u64;
            // P2-3: price this response's usage with the layer's CostSpec.
            total_cost_usd += config.cost.cost_of(usage);
        }

        total_steps += 1;

        let has_tool_calls = !response.tool_calls.is_empty();
        let assistant_content = if has_tool_calls {
            String::new()
        } else {
            response.content.clone().unwrap_or_default()
        };
        let has_content = !assistant_content.is_empty();

        let assistant_msg = Message {
            role: MessageRole::Assistant,
            content: assistant_content.clone(),
            tool_call_id: None,
            name: None,
            tool_calls: if has_tool_calls {
                Some(response.tool_calls.clone())
            } else {
                None
            },
        };
        // Per-step incremental persistence: the assistant message is on
        // durable storage before any tool runs (crash mid-step leaves a
        // resumable partial transcript).
        sink_append(sink, &assistant_msg).await;
        messages.push(assistant_msg);

        if has_tool_calls {
            consecutive_empty_turns = 0;
            // P2-1 serial-vs-parallel dispatch, decided at the TOP of the
            // branch and purely by NAME: any `call_agent` (recursive
            // delegation owns frame/ordering invariants) or `run_code`
            // (PTC seam; one approval covers its whole sandbox script) in
            // the batch forces the sequential loop — with ptc disabled,
            // run_code lands as an unknown-tool error and serial stays the
            // conservative choice. `[limits].parallel_tool_calls = false`
            // forces the sequential loop for EVERY batch (escape hatch:
            // restores full write→read ordering within a step).
            let serial_batch = !config.parallel_tool_calls
                || response
                    .tool_calls
                    .iter()
                    .any(|tc| tc.name == "call_agent" || tc.name == RUN_CODE_TOOL);
            if serial_batch {
                for (tool_index, tc) in response.tool_calls.iter().enumerate() {
                    // Cancellation checkpoint before each tool (Phase 4): once
                    // cancelled, no further tool runs — the unanswered calls of
                    // this step get synthetic cancelled results so the partial
                    // transcript stays provider-valid.
                    if cancel.is_some_and(|t| t.is_cancelled()) {
                        append_cancelled_tool_results(
                            sink,
                            &mut messages,
                            &response.tool_calls,
                            tool_index,
                        )
                        .await;
                        return Ok(interrupted_result(
                            &config,
                            messages,
                            total_steps,
                            total_input_tokens,
                            total_output_tokens,
                            total_cost_usd,
                        ));
                    }

                    let args_str = tc.arguments.to_string();
                    let args_display = truncate_str(&args_str, 120);

                    if let Some(cb) = progress.as_mut() {
                        cb.on_tool_start(&tc.name, args_display);
                    } else {
                        tracing::info!("{}  -> {}({})", log_indent, tc.name, args_display);
                    }

                    validate_tool_arguments(&tc.arguments)?;

                    // Cancellation checkpoint around the tool execution (Phase 4):
                    // on cancel the tool future is dropped (the tool aborts and
                    // produces no result); a synthetic cancelled result fills the
                    // call so the transcript keeps its tool_call/tool pairing.
                    let output = tokio::select! {
                        biased;
                        _ = token_cancelled(cancel) => {
                            append_cancelled_tool_results(
                                sink,
                                &mut messages,
                                &response.tool_calls,
                                tool_index,
                            )
                            .await;
                            return Ok(interrupted_result(
                                &config,
                                messages,
                                total_steps,
                                total_input_tokens,
                                total_output_tokens,
                                total_cost_usd,
                            ));
                        }
                        out = execute_tool_safely(tool_executor, &tc.name, &tc.arguments) => out,
                    };

                    // Global output-size backstop: cap the tool output at
                    // `max_output_bytes` before it enters the conversation, so a
                    // runaway tool (typically an external MCP server without its
                    // own truncation) cannot flood the context. Builtin tools that
                    // already truncate (e.g. shell's 64KB cap) simply end up with
                    // the smaller of the two limits. `0` disables the cap
                    // (same convention as `max_steps`).
                    let output = if config.max_output_bytes > 0 {
                        crate::tool::limit_tool_output(output, config.max_output_bytes as usize)
                    } else {
                        output
                    };

                    let truncated = output.bytes > 80;
                    if let Some(cb) = progress.as_mut() {
                        cb.on_tool_end(&tc.name, output.bytes, truncated);
                    } else {
                        let result_preview = truncate_str(&output.content, 80);
                        tracing::info!(
                            "{}  <- {} [{} bytes] {}",
                            log_indent,
                            tc.name,
                            output.bytes,
                            if truncated {
                                format!("\"{}\"...", result_preview)
                            } else {
                                format!("\"{}\"", result_preview)
                            }
                        );
                    }

                    let tool_msg = Message {
                        role: MessageRole::Tool,
                        content: output.content,
                        tool_call_id: Some(tc.id.clone()),
                        name: Some(tc.name.clone()),
                        tool_calls: None,
                    };
                    // Persist the tool result before continuing (same per-step
                    // contract as the assistant message above).
                    sink_append(sink, &tool_msg).await;
                    messages.push(tool_msg);
                }
            } else {
                // ── Parallel batch (P2-1) ──────────────────────────────
                //
                // All direct tool calls of this step execute concurrently;
                // every order-sensitive concern (progress emission, sink
                // writes, the transcript) is coordinated by this loop,
                // strictly in tool_call order, before/after the join.

                // Argument validation is hoisted over the WHOLE batch: the
                // first malformed call aborts the run before anything is
                // dispatched — the same "bad arguments kill the run"
                // semantics the sequential loop enforces per tool.
                for tc in &response.tool_calls {
                    validate_tool_arguments(&tc.arguments)?;
                }

                // Cancellation checkpoint before the batch (Phase 4, batch
                // flavor): once cancelled, nothing is dispatched — every
                // call of this step gets a synthetic cancelled result so
                // the partial transcript stays provider-valid.
                if cancel.is_some_and(|t| t.is_cancelled()) {
                    append_cancelled_tool_results(sink, &mut messages, &response.tool_calls, 0)
                        .await;
                    return Ok(interrupted_result(
                        &config,
                        messages,
                        total_steps,
                        total_input_tokens,
                        total_output_tokens,
                        total_cost_usd,
                    ));
                }

                // Batch progress emission: `ProgressCallback` is `&mut dyn`
                // and cannot be shared into the concurrent futures (an
                // `Arc<Mutex<callback>>` bypass is deliberately forbidden) —
                // the coordinator emits `on_tool_start` for the whole batch,
                // in tool_call order, BEFORE dispatch, and `on_tool_end` in
                // tool_call order AFTER the join (below). ChildProgress
                // (child-agent layers) rides the same code path.
                for tc in &response.tool_calls {
                    let args_str = tc.arguments.to_string();
                    let args_display = truncate_str(&args_str, 120);
                    if let Some(cb) = progress.as_mut() {
                        cb.on_tool_start(&tc.name, args_display);
                    } else {
                        tracing::info!("{}  -> {}({})", log_indent, tc.name, args_display);
                    }
                }

                // Concurrent dispatch. `tokio::spawn`/`JoinSet` cannot be
                // used here: the executor is a shared borrow
                // (`&dyn ToolExecutor` — the AgentRunner, itself borrowing
                // the run context) and not `'static`, so the futures are
                // polled concurrently ON THIS TASK via `FuturesUnordered`
                // instead. For the async I/O tools that dominate real runs
                // this already makes a batch's wall time ≈ max(durations),
                // not the sum.
                //
                // Results land in per-tool_call SLOTS ("spawn + result
                // slots" pattern): a cancellation mid-batch drops only the
                // futures still in flight, while every already-completed
                // output is preserved and drained below — a bare
                // `join_all` + `select!` would lose them all (banned by
                // design). The execution still funnels through the single
                // `AgentRunner::execute` choke point (approval gate,
                // budgets, audit — all interior-mutable shared state).
                //
                // KNOWN BEHAVIOR (deliberate, bounded): the
                // `max_tool_calls` budget is check-then-add on a shared
                // atomic inside the choke point, so a parallel batch
                // racing the boundary may overshoot by at most
                // batch_size - 1 calls.
                // KNOWN BEHAVIOR (deliberate): ordering between a write
                // and a dependent read/shell call in the SAME batch is not
                // guaranteed — models place dependent calls in separate
                // steps; `parallel_tool_calls = false` restores full
                // ordering.
                let mut slots: Vec<Option<ToolOutput>> =
                    (0..response.tool_calls.len()).map(|_| None).collect();
                let mut inflight = FuturesUnordered::new();
                for (idx, tc) in response.tool_calls.iter().enumerate() {
                    inflight.push(async move {
                        // catch_unwind is per-future (execute_tool_safely):
                        // one tool's panic becomes its own error output and
                        // never disturbs its siblings.
                        let out = execute_tool_safely(tool_executor, &tc.name, &tc.arguments).await;
                        (idx, out)
                    });
                }
                let mut batch_cancelled = false;
                loop {
                    tokio::select! {
                        // Results-first ordering (deliberate, unlike the
                        // stream checkpoint): a ready tool result is
                        // always collected before the cancellation branch
                        // is consulted, so a batch whose tools raced a
                        // Ctrl-C to completion keeps their real outputs —
                        // only futures still pending when nothing else is
                        // ready are dropped. Cancellation stays prompt:
                        // the token branch is observed the moment the
                        // in-flight set goes pending.
                        biased;
                        item = inflight.next() => {
                            match item {
                                Some((idx, out)) => slots[idx] = Some(out),
                                None => break, // every future resolved
                            }
                        }
                        _ = token_cancelled(cancel) => {
                            batch_cancelled = true;
                            break;
                        }
                    }
                }

                // Ordered drain: exactly one post-pass in tool_call order —
                // emitting `on_tool_end`, applying the output-size
                // backstop, and writing the sink sequentially (the
                // single-writer order contract is preserved). A call whose
                // slot is still empty was dropped in flight by a batch
                // cancellation and gets the synthetic cancelled result,
                // keeping the tool_call/tool pairing (Phase 4 fidelity).
                for (tc, slot) in response.tool_calls.iter().zip(slots.iter_mut()) {
                    match slot.take() {
                        Some(output) => {
                            let output = if config.max_output_bytes > 0 {
                                crate::tool::limit_tool_output(
                                    output,
                                    config.max_output_bytes as usize,
                                )
                            } else {
                                output
                            };
                            let truncated = output.bytes > 80;
                            if let Some(cb) = progress.as_mut() {
                                cb.on_tool_end(&tc.name, output.bytes, truncated);
                            } else {
                                let result_preview = truncate_str(&output.content, 80);
                                tracing::info!(
                                    "{}  <- {} [{} bytes] {}",
                                    log_indent,
                                    tc.name,
                                    output.bytes,
                                    if truncated {
                                        format!("\"{}\"...", result_preview)
                                    } else {
                                        format!("\"{}\"", result_preview)
                                    }
                                );
                            }
                            let tool_msg = Message {
                                role: MessageRole::Tool,
                                content: output.content,
                                tool_call_id: Some(tc.id.clone()),
                                name: Some(tc.name.clone()),
                                tool_calls: None,
                            };
                            // Persist the tool result before continuing
                            // (same per-step contract as the sequential
                            // loop).
                            sink_append(sink, &tool_msg).await;
                            messages.push(tool_msg);
                        }
                        None => {
                            // Never answered (cancellation dropped the
                            // future in flight) — synthetic cancelled
                            // result fills the call.
                            let msg = cancelled_tool_message(tc);
                            sink_append(sink, &msg).await;
                            messages.push(msg);
                        }
                    }
                }
                if batch_cancelled {
                    return Ok(interrupted_result(
                        &config,
                        messages,
                        total_steps,
                        total_input_tokens,
                        total_output_tokens,
                        total_cost_usd,
                    ));
                }
            }
            if let Some(cb) = progress.as_mut() {
                cb.on_step_end();
            }
            continue;
        }

        if has_content {
            // Non-spinner path (i.e. child agents running with progress=None):
            // print the child's actual answer so the delegation output is
            // visible in the log. The root agent's content is streamed live by
            // the spinner (on_delta), so we skip duplicating it here.
            if progress.is_none() {
                tracing::info!("{}  ┃ [{}] {}", log_indent, agent_tag, assistant_content);
            }
            // Note: no on_step_end here. The final step's content is printed by
            // `run` after the runtime returns, so the stats line would appear
            // BEFORE the content. Instead `run` emits it after printing content
            // (order: content → stats → Run done).
            return Ok(RunResult {
                run_id: config.run_id.clone(),
                status: RunStatus::Completed,
                messages,
                total_steps,
                total_input_tokens,
                total_output_tokens,
                total_cost_usd,
            });
        }

        let is_done = response.finish_reason.as_deref() == Some("stop")
            || response.finish_reason.as_deref() == Some("end_turn");

        if is_done {
            return Ok(RunResult {
                run_id: config.run_id.clone(),
                status: RunStatus::Completed,
                messages,
                total_steps,
                total_input_tokens,
                total_output_tokens,
                total_cost_usd,
            });
        }

        consecutive_empty_turns += 1;
        if consecutive_empty_turns >= config.max_empty_turns {
            return Err(OpenSlateError::Runtime(
                RuntimeError::MaxEmptyTurnsExceeded {
                    count: consecutive_empty_turns,
                    step: total_steps,
                    agent_id: config.agent_id.0.clone(),
                    model_alias: config.model_alias.clone(),
                },
            ));
        }
    }
}

/// Truncate a string to at most `max_chars` characters, appending "..." if truncated.
fn truncate_str(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        return s;
    }
    let mut end = max_chars;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Rough estimate of input tokens for a request (~4 chars/token), so the UI can
/// show an `↑N` hint during streaming before the provider's real usage arrives.
fn estimate_input_tokens(req: &GenerateRequest) -> u32 {
    let mut chars: usize = req.system_prompt.as_ref().map_or(0, String::len);
    for m in &req.messages {
        chars += m.content.len();
        if let Some(name) = m.name.as_ref() {
            chars += name.len();
        }
        if let Some(tcs) = m.tool_calls.as_ref() {
            for tc in tcs {
                chars += tc.name.len() + tc.arguments.to_string().len();
            }
        }
    }
    for t in &req.tools {
        chars += t.name.len() + t.description.len() + t.parameters.to_string().len();
    }
    (chars as u32).max(1) / 4
}

/// Validate that tool call arguments are a JSON object. Returns `Ok(())` if valid,
/// or a `RuntimeError::ToolArgumentError` if the arguments are malformed.
pub fn validate_tool_arguments(args: &serde_json::Value) -> Result<(), RuntimeError> {
    match args {
        serde_json::Value::Object(_) => Ok(()),
        serde_json::Value::Null => Ok(()),
        other => Err(RuntimeError::ToolArgumentError {
            tool_name: String::new(),
            step: 0,
            agent_id: String::new(),
            details: format!("expected object or null, got {}", json_type_name(other)),
        }),
    }
}

/// Get a human-readable type name for a JSON value.
fn json_type_name(val: &serde_json::Value) -> &'static str {
    match val {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Execute a tool with panic protection.
///
/// Wraps the tool executor call in `catch_unwind` to catch panics
/// and convert them into error `ToolOutput`s.
async fn execute_tool_safely(
    executor: &dyn ToolExecutor,
    name: &str,
    args: &serde_json::Value,
) -> ToolOutput {
    let name_owned = name.to_owned();
    let args_owned = args.clone();

    let result = AssertUnwindSafe(executor.execute(&name_owned, &args_owned))
        .catch_unwind()
        .await;

    match result {
        Ok(output) => output,
        Err(panic_payload) => {
            let reason = match panic_payload.downcast_ref::<&str>() {
                Some(s) => s.to_string(),
                None => match panic_payload.downcast_ref::<String>() {
                    Some(s) => s.clone(),
                    None => "unknown panic".to_string(),
                },
            };
            ToolOutput {
                content: format!("Tool '{}' panicked: {}", name, reason),
                bytes: 0,
                duration_ms: 0,
                status: ToolOutputStatus::Error,
            }
        }
    }
}

/// Estimate the byte size of the message context.
fn estimate_context_bytes(messages: &[Message]) -> usize {
    messages.iter().map(|m| m.content.len()).sum()
}

/// Truncate conversation context if it exceeds the maximum byte limit.
///
/// Preserves the first message (typically the user's input) and the most recent
/// messages. Middle messages are dropped to bring the total under the limit.
fn truncate_context_if_needed(messages: &mut Vec<Message>, max_bytes: u32) {
    let max = max_bytes as usize;
    if estimate_context_bytes(messages) <= max {
        return;
    }

    if messages.len() <= 2 {
        return;
    }

    let keep_recent = 2.min(messages.len());
    let first = messages.first().cloned();

    let mut trimmed = Vec::new();
    if let Some(first_msg) = first {
        trimmed.push(first_msg);
    }

    let notice = Message {
        role: MessageRole::System,
        content: "[Context truncated: older messages removed to stay within limit]".into(),
        tool_call_id: None,
        name: None,
        tool_calls: None,
    };
    trimmed.push(notice);

    let recent_start = messages.len().saturating_sub(keep_recent);
    for msg in messages.iter().skip(recent_start) {
        trimmed.push(msg.clone());
    }

    while estimate_context_bytes(&trimmed) > max && trimmed.len() > 3 {
        let mid = 1 + (trimmed.len() - 1) / 2;
        trimmed.remove(mid.min(trimmed.len() - 2).max(1));
    }

    *messages = trimmed;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderError;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // -- Mock provider --

    struct MockProvider {
        responses: Vec<ModelResponse>,
        call_count: AtomicUsize,
    }

    impl MockProvider {
        fn new(responses: Vec<ModelResponse>) -> Self {
            Self {
                responses,
                call_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for MockProvider {
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
            "mock"
        }
    }

    // -- Mock tool executor --

    struct MockToolExecutor;

    #[async_trait::async_trait]
    impl crate::tool::ToolExecutor for MockToolExecutor {
        async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolOutput {
            ToolOutput {
                content: format!("executed {name} with {args:?}"),
                bytes: 20,
                duration_ms: 10,
                status: ToolOutputStatus::Success,
            }
        }
    }

    fn default_config() -> RunConfig {
        RunConfig {
            run_id: RunId("test-run".into()),
            agent_id: AgentId("test-agent".into()),
            model_alias: "test-model".into(),
            system_prompt: None,
            initial_messages: vec![Message {
                role: MessageRole::User,
                content: "hello".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            }],
            max_steps: 10,
            max_context_bytes: 100_000,
            max_output_bytes: 10_000,
            max_empty_turns: DEFAULT_MAX_EMPTY_TURNS,
            tool_definitions: vec![],
            timeout_ms: 60_000,
            depth: 0,
            parallel_tool_calls: true,
            cost: CostSpec::default(),
        }
    }

    // -- Tests --

    #[tokio::test]
    async fn test_simple_single_turn() {
        let provider = MockProvider::new(vec![ModelResponse {
            content: Some("Hello!".into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("stop".into()),
        }]);

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &MockToolExecutor,
            None,
            None,
            None,
        )
        .await
        .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 1);
        assert_eq!(result.run_id, RunId("test-run".into()));

        // 1 user + 1 assistant
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.messages[1].role, MessageRole::Assistant);
        assert_eq!(result.messages[1].content, "Hello!");
    }

    #[tokio::test]
    async fn test_multi_turn_with_tools() {
        let provider = MockProvider::new(vec![
            // Step 1: model requests a tool call
            ModelResponse {
                content: Some("Let me check that.".into()),
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "ls"}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            // Step 2: model returns final text
            ModelResponse {
                content: Some("Here are the files.".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &MockToolExecutor,
            None,
            None,
            None,
        )
        .await
        .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 2);

        // 1 user + 1 assistant(1st) + 1 tool + 1 assistant(2nd)
        assert_eq!(result.messages.len(), 4);
        assert_eq!(result.messages[2].role, MessageRole::Tool);
        assert_eq!(
            result.messages[2].tool_call_id,
            Some(ToolCallId("tc-1".into()))
        );
        assert_eq!(result.messages[3].role, MessageRole::Assistant);
        assert_eq!(result.messages[3].content, "Here are the files.");
    }

    #[tokio::test]
    async fn test_max_steps_interrupted() {
        // Provider always returns tool_calls — never terminates naturally
        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "loop".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            };
            5 // more than enough
        ]);

        let mut config = default_config();
        config.max_steps = 2;

        let result = execute_run(&provider, config, "m1", &MockToolExecutor, None, None, None)
            .await
            .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Interrupted);
        assert_eq!(result.total_steps, 2);
    }

    #[tokio::test]
    async fn test_provider_error() {
        struct ErrorProvider;

        #[async_trait::async_trait]
        impl ModelProvider for ErrorProvider {
            async fn generate(
                &self,
                _request: GenerateRequest,
            ) -> Result<ModelResponse, ProviderError> {
                Err(ProviderError::ServerError(503))
            }

            fn provider_name(&self) -> &str {
                "error"
            }
        }

        let result = execute_run(
            &ErrorProvider,
            default_config(),
            "m1",
            &MockToolExecutor,
            None,
            None,
            None,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                OpenSlateError::Provider(ProviderError::ServerError(503))
            ),
            "expected ProviderError::ServerError(503), got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_usage_tracking() {
        let provider = MockProvider::new(vec![
            // Step 1: tool call
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "calc".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cached_input_tokens: None,
                }),
                finish_reason: Some("tool_calls".into()),
            },
            // Step 2: final answer
            ModelResponse {
                content: Some("42".into()),
                tool_calls: vec![],
                usage: Some(Usage {
                    input_tokens: 150,
                    output_tokens: 5,
                    cached_input_tokens: None,
                }),
                finish_reason: Some("stop".into()),
            },
        ]);

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &MockToolExecutor,
            None,
            None,
            None,
        )
        .await
        .expect("run should succeed");

        assert_eq!(result.total_input_tokens, 250); // 100 + 150
        assert_eq!(result.total_output_tokens, 25); // 20 + 5
        assert_eq!(result.total_steps, 2);
    }

    #[tokio::test]
    async fn test_empty_response_counts_toward_max_empty_turns() {
        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![],
                usage: None,
                finish_reason: None,
            };
            5
        ]);

        let mut config = default_config();
        config.max_empty_turns = 3;

        let result =
            execute_run(&provider, config, "m1", &MockToolExecutor, None, None, None).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        match &err {
            OpenSlateError::Runtime(RuntimeError::MaxEmptyTurnsExceeded {
                count,
                step,
                agent_id,
                model_alias,
            }) => {
                assert_eq!(*count, 3);
                assert_eq!(*step, 3);
                assert_eq!(agent_id, "test-agent");
                assert_eq!(model_alias, "test-model");
            }
            other => panic!("expected MaxEmptyTurnsExceeded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_single_empty_response_then_content() {
        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![],
                usage: None,
                finish_reason: None,
            },
            ModelResponse {
                content: Some("Finally!".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        let mut config = default_config();
        config.max_empty_turns = 3;

        let result = execute_run(&provider, config, "m1", &MockToolExecutor, None, None, None)
            .await
            .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 2);
    }

    #[tokio::test]
    async fn test_unknown_tool_produces_error_output() {
        struct ErrorToolExecutor;

        #[async_trait::async_trait]
        impl crate::tool::ToolExecutor for ErrorToolExecutor {
            async fn execute(&self, name: &str, _args: &serde_json::Value) -> ToolOutput {
                ToolOutput {
                    content: format!("Error: tool '{}' not found", name),
                    bytes: 0,
                    duration_ms: 0,
                    status: ToolOutputStatus::Error,
                }
            }
        }

        let provider = MockProvider::new(vec![
            ModelResponse {
                content: Some("calling unknown tool".into()),
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "nonexistent_tool".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("Done after error".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &ErrorToolExecutor,
            None,
            None,
            None,
        )
        .await
        .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 2);
        assert_eq!(result.messages[2].role, MessageRole::Tool);
        assert!(result.messages[2].content.contains("nonexistent_tool"));
        assert!(result.messages[2].content.contains("not found"));
    }

    #[tokio::test]
    async fn test_malformed_tool_arguments() {
        let result = validate_tool_arguments(&serde_json::json!({"key": "value"}));
        assert!(result.is_ok());

        let result = validate_tool_arguments(&serde_json::Value::Null);
        assert!(result.is_ok());

        let result = validate_tool_arguments(&serde_json::json!("not an object"));
        assert!(result.is_err());
        match result.unwrap_err() {
            RuntimeError::ToolArgumentError { details, .. } => {
                assert!(details.contains("expected object or null"));
                assert!(details.contains("string"));
            }
            other => panic!("expected ToolArgumentError, got {other:?}"),
        }

        let result = validate_tool_arguments(&serde_json::json!(42));
        assert!(result.is_err());

        let result = validate_tool_arguments(&serde_json::json!([1, 2, 3]));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_tool_panic_caught() {
        struct PanickingExecutor;

        #[async_trait::async_trait]
        impl crate::tool::ToolExecutor for PanickingExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ToolOutput {
                panic!("intentional test panic");
            }
        }

        let output =
            execute_tool_safely(&PanickingExecutor, "test_tool", &serde_json::json!({})).await;
        assert_eq!(output.status, ToolOutputStatus::Error);
        assert!(output.content.contains("test_tool"));
        assert!(output.content.contains("panicked"));
        assert!(output.content.contains("intentional test panic"));
    }

    #[tokio::test]
    async fn test_tool_panic_with_string_message() {
        struct StringPanicExecutor;

        #[async_trait::async_trait]
        impl crate::tool::ToolExecutor for StringPanicExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ToolOutput {
                panic!("string panic message");
            }
        }

        let output =
            execute_tool_safely(&StringPanicExecutor, "my_tool", &serde_json::json!({})).await;
        assert_eq!(output.status, ToolOutputStatus::Error);
        assert!(output.content.contains("my_tool"));
        assert!(output.content.contains("string panic message"));
    }

    #[tokio::test]
    async fn test_tool_panic_with_unknown_payload() {
        struct UnknownPanicExecutor;

        #[async_trait::async_trait]
        impl crate::tool::ToolExecutor for UnknownPanicExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ToolOutput {
                std::panic::panic_any(42i32); // non-string payload
            }
        }

        let output =
            execute_tool_safely(&UnknownPanicExecutor, "weird_tool", &serde_json::json!({})).await;
        assert_eq!(output.status, ToolOutputStatus::Error);
        assert!(output.content.contains("weird_tool"));
        assert!(output.content.contains("unknown panic"));
    }

    #[test]
    fn test_context_truncation() {
        let mut messages = vec![
            Message {
                role: MessageRole::User,
                content: "hello".repeat(1000),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "response1".repeat(1000),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::User,
                content: "followup".repeat(1000),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "response2".repeat(1000),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::User,
                content: "final".repeat(100),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
        ];

        let total_before: usize = messages.iter().map(|m| m.content.len()).sum();
        assert!(total_before > 2000);

        truncate_context_if_needed(&mut messages, 2000);

        let total_after: usize = messages.iter().map(|m| m.content.len()).sum();
        assert!(total_after < total_before);
        assert!(messages.len() >= 2);
        assert_eq!(messages[0].role, MessageRole::User);
    }

    #[test]
    fn test_context_no_truncation_when_under_limit() {
        let mut messages = vec![
            Message {
                role: MessageRole::User,
                content: "hello".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "hi".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
        ];

        let original_len = messages.len();
        truncate_context_if_needed(&mut messages, 100_000);
        assert_eq!(messages.len(), original_len);
    }

    #[test]
    fn test_error_messages_include_context() {
        let err = RuntimeError::EmptyResponse {
            step: 5,
            agent_id: "my-agent".into(),
            model_alias: "gpt-4o".into(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("step 5"));
        assert!(msg.contains("my-agent"));
        assert!(msg.contains("gpt-4o"));

        let err = RuntimeError::UnknownTool {
            tool_name: "bad_tool".into(),
            step: 3,
            agent_id: "root".into(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("bad_tool"));
        assert!(msg.contains("step 3"));
        assert!(msg.contains("root"));

        let err = RuntimeError::ToolExecutionError {
            tool_name: "bash".into(),
            step: 7,
            agent_id: "worker".into(),
            reason: "segfault".into(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("bash"));
        assert!(msg.contains("step 7"));
        assert!(msg.contains("worker"));
        assert!(msg.contains("segfault"));
    }

    // -- RuntimeLimits + check_limits tests --

    #[test]
    fn test_max_steps_exceeded() {
        let limits = RuntimeLimits {
            max_steps: 5,
            ..Default::default()
        };
        let err = check_limits(&limits, 5, 0, 0, 0).unwrap_err();
        assert!(
            matches!(err, RuntimeError::MaxStepsExceeded { max: 5 }),
            "expected MaxStepsExceeded, got {err:?}"
        );
    }

    #[test]
    fn test_max_steps_zero_means_unlimited() {
        let limits = RuntimeLimits {
            max_steps: 0,
            ..Default::default()
        };
        // Should NOT trigger MaxStepsExceeded even with a huge step count
        check_limits(&limits, 999_999, 0, 0, 0).expect("max_steps=0 should be unlimited");
    }

    #[test]
    fn test_max_depth_exceeded() {
        let limits = RuntimeLimits::default();
        let err = check_limits(&limits, 0, limits.max_depth, 0, 0).unwrap_err();
        assert!(
            matches!(err, RuntimeError::MaxDepthExceeded { max: 4 }),
            "expected MaxDepthExceeded, got {err:?}"
        );
    }

    #[test]
    fn test_max_tool_calls_exceeded() {
        let limits = RuntimeLimits::default();
        let err = check_limits(&limits, 0, 0, limits.max_tool_calls, 0).unwrap_err();
        assert!(
            matches!(err, RuntimeError::MaxToolCallsExceeded { max: 20 }),
            "expected MaxToolCallsExceeded, got {err:?}"
        );
    }

    #[test]
    fn test_within_limits() {
        let limits = RuntimeLimits::default();
        check_limits(&limits, 0, 0, 0, 0).expect("should be within limits");
        check_limits(&limits, 11, 3, 19, 7).expect("should be within limits (one under each)");
    }

    #[test]
    fn test_default_limits() {
        let limits = RuntimeLimits::default();
        assert_eq!(limits.max_steps, 0); // 0 = unlimited
        assert_eq!(limits.max_depth, 4);
        assert_eq!(limits.max_tool_calls, 20);
        assert_eq!(limits.max_child_agent_calls, 8);
        assert_eq!(limits.timeout_ms, 60_000);
        assert_eq!(limits.max_context_bytes, 64_000);
        assert_eq!(limits.max_output_bytes, 65_536);
        assert_eq!(limits.max_empty_turns, DEFAULT_MAX_EMPTY_TURNS);
    }

    // ── Timeout enforcement tests ──

    #[tokio::test]
    async fn test_timeout_fires_on_slow_provider() {
        struct SlowProvider;

        #[async_trait::async_trait]
        impl ModelProvider for SlowProvider {
            async fn generate(
                &self,
                _request: GenerateRequest,
            ) -> Result<ModelResponse, ProviderError> {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                Ok(ModelResponse {
                    content: Some("too slow".into()),
                    tool_calls: vec![],
                    usage: None,
                    finish_reason: Some("stop".into()),
                })
            }
            fn provider_name(&self) -> &str {
                "slow"
            }
        }

        let mut config = default_config();
        config.timeout_ms = 50; // 50 ms budget

        let result = execute_run(
            &SlowProvider,
            config,
            "m1",
            &MockToolExecutor,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OpenSlateError::Runtime(RuntimeError::Timeout { timeout_ms }) => {
                assert_eq!(timeout_ms, 50);
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_normal_execution_within_timeout() {
        let provider = MockProvider::new(vec![ModelResponse {
            content: Some("fast enough".into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("stop".into()),
        }]);

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &MockToolExecutor,
            None,
            None,
            None,
        )
        .await
        .expect("should complete well within timeout");
        assert_eq!(result.status, RunStatus::Completed);
    }

    #[tokio::test]
    async fn test_timeout_allows_multi_step_within_budget() {
        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "step1".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("done".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        // Generous timeout — should complete both steps.
        let mut config = default_config();
        config.timeout_ms = 5_000;

        let result = execute_run(&provider, config, "m1", &MockToolExecutor, None, None, None)
            .await
            .expect("should complete within timeout");
        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 2);
    }

    #[tokio::test]
    async fn test_timeout_zero_ms_immediately_fires() {
        let provider = MockProvider::new(vec![ModelResponse {
            content: Some("never".into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("stop".into()),
        }]);

        let mut config = default_config();
        config.timeout_ms = 0; // zero → immediate timeout

        let result =
            execute_run(&provider, config, "m1", &MockToolExecutor, None, None, None).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OpenSlateError::Runtime(RuntimeError::Timeout { timeout_ms }) => {
                assert_eq!(timeout_ms, 0);
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    // ── LLM-time budget semantics (fix-21) ──────────────────────────────
    //
    // `[limits].timeout_ms` bounds the round's TOTAL provider-await time.
    // Approval waits (decide() blocking inside tool execution) and tool
    // runs pause the budget; pure LLM time spent across steps accumulates
    // and still kills the round when it exceeds the budget.

    /// Executor whose tool blocks far longer than the round budget — the
    /// shape of a manual-approval wait (decide() blocking on the user's
    /// y/n) plus the tool's own run: exactly the fix-21 repro.
    struct ApprovalBlockingExecutor {
        block_ms: u64,
    }

    #[async_trait::async_trait]
    impl crate::tool::ToolExecutor for ApprovalBlockingExecutor {
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ToolOutput {
            tokio::time::sleep(Duration::from_millis(self.block_ms)).await;
            ToolOutput {
                content: "approved and ran".into(),
                bytes: 18,
                duration_ms: self.block_ms,
                status: ToolOutputStatus::Success,
            }
        }
    }

    #[tokio::test]
    async fn test_timeout_budget_ignores_approval_and_tool_time() {
        // fix-21 repro nail (non-streaming path): step 1 answers with a
        // tool call; the tool's "approval" blocks 300ms — LONGER than the
        // whole 200ms budget; step 2's request must still get the full
        // budget and succeed. Under the old round wall clock the
        // pre-step-2 check found the deadline burnt by the approval and
        // killed the run with `timeout after 200ms`.
        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "ls"}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("tutorial draft done".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        let mut config = default_config();
        config.timeout_ms = 200;

        let result = execute_run(
            &provider,
            config,
            "m1",
            &ApprovalBlockingExecutor { block_ms: 300 },
            None,
            None,
            None,
        )
        .await
        .expect("approval/tool time must not burn the LLM budget");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 2);
    }

    /// Streaming provider with a spawned dispatch (the shape of the real
    /// genai adapter — `generate_stream` returns a receiver immediately
    /// and the response arrives as stream events): each call streams its
    /// scripted response as Delta(s) + Done.
    struct ScriptedStreamProvider {
        responses: Vec<ModelResponse>,
        call_count: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ModelProvider for ScriptedStreamProvider {
        async fn generate(
            &self,
            _request: GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            Err(ProviderError::ServerError(500)) // streaming path only
        }

        async fn generate_stream(
            &self,
            _request: GenerateRequest,
        ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            let resp = self.responses.get(idx).cloned();
            tokio::spawn(async move {
                match resp {
                    Some(resp) => {
                        if let Some(text) = &resp.content {
                            let _ = tx.send(Ok(ModelStreamEvent::Delta(text.clone()))).await;
                        }
                        let _ = tx.send(Ok(ModelStreamEvent::Done(resp))).await;
                    }
                    None => {
                        let _ = tx.send(Err(ProviderError::ServerError(500))).await;
                    }
                }
            });
            rx
        }

        fn provider_name(&self) -> &str {
            "scripted-stream"
        }
    }

    #[tokio::test]
    async fn test_timeout_budget_ignores_tool_time_streaming_path() {
        // The same nail on the STREAMING path (progress callback wired —
        // the production CLI/TUI shape): the tool block must not consume
        // the budget and the second stream must complete with its full
        // remainder.
        let provider = ScriptedStreamProvider {
            responses: vec![
                ModelResponse {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: ToolCallId("tc-1".into()),
                        name: "shell".into(),
                        arguments: serde_json::json!({"command": "ls"}),
                    }],
                    usage: None,
                    finish_reason: Some("tool_calls".into()),
                },
                ModelResponse {
                    content: Some("done".into()),
                    tool_calls: vec![],
                    usage: None,
                    finish_reason: Some("stop".into()),
                },
            ],
            call_count: AtomicUsize::new(0),
        };

        let mut config = default_config();
        config.timeout_ms = 200;
        let mut progress = NoopProgress;

        let result = execute_run(
            &provider,
            config,
            "m1",
            &ApprovalBlockingExecutor { block_ms: 300 },
            None,
            Some(&mut progress),
            None,
        )
        .await
        .expect("streaming run must survive the approval block");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 2);
    }

    #[tokio::test]
    async fn test_timeout_bounds_inline_stream_dispatch() {
        // fix-22 pin: a provider relying on the DEFAULT `generate_stream`
        // (which awaits `generate()` inline — the shape of child-agent
        // layers wired through ChildProgress) must have a single over-
        // budget request killed by the per-request budget. Previously the
        // timeout only wrapped the receive loop, so the inline dispatch
        // ran unbounded and the (already-drained) channel never tripped
        // it; the overrun only surfaced at the NEXT step boundary.
        struct InlineSleepyStreamProvider;

        #[async_trait::async_trait]
        impl ModelProvider for InlineSleepyStreamProvider {
            async fn generate(
                &self,
                _request: GenerateRequest,
            ) -> Result<ModelResponse, ProviderError> {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok(ModelResponse {
                    content: Some("never".into()),
                    tool_calls: vec![],
                    usage: None,
                    finish_reason: Some("stop".into()),
                })
            }
            fn provider_name(&self) -> &str {
                "inline-sleepy-stream"
            }
        }

        let mut config = default_config();
        config.timeout_ms = 200;
        let mut progress = NoopProgress;

        let result = execute_run(
            &InlineSleepyStreamProvider,
            config,
            "m1",
            &MockToolExecutor,
            None,
            Some(&mut progress),
            None,
        )
        .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OpenSlateError::Runtime(RuntimeError::Timeout { timeout_ms }) => {
                assert_eq!(timeout_ms, 200);
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_timeout_budget_accumulates_llm_time_across_steps() {
        // The budget is the round's TOTAL LLM time (fix-21): request 1
        // spends ~120ms of the 200ms budget and returns a tool call; the
        // tool itself is instant; request 2 then has only ~80ms of budget
        // left and its 500ms await must be killed by Timeout — pure-LLM
        // overrun still times out.
        struct SleepyProvider;

        #[async_trait::async_trait]
        impl ModelProvider for SleepyProvider {
            async fn generate(
                &self,
                request: GenerateRequest,
            ) -> Result<ModelResponse, ProviderError> {
                let is_first = !request.messages.iter().any(|m| m.role == MessageRole::Tool);
                if is_first {
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    Ok(ModelResponse {
                        content: None,
                        tool_calls: vec![ToolCall {
                            id: ToolCallId("tc-1".into()),
                            name: "fast".into(),
                            arguments: serde_json::json!({}),
                        }],
                        usage: None,
                        finish_reason: Some("tool_calls".into()),
                    })
                } else {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Ok(ModelResponse {
                        content: Some("never".into()),
                        tool_calls: vec![],
                        usage: None,
                        finish_reason: Some("stop".into()),
                    })
                }
            }
            fn provider_name(&self) -> &str {
                "sleepy"
            }
        }

        let mut config = default_config();
        config.timeout_ms = 200;

        let result = execute_run(
            &SleepyProvider,
            config,
            "m1",
            &MockToolExecutor,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OpenSlateError::Runtime(RuntimeError::Timeout { timeout_ms }) => {
                assert_eq!(timeout_ms, 200);
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    // ── Async tool execution tests ──

    #[tokio::test]
    async fn test_async_tool_executes_without_nested_runtime() {
        /// A tool executor that performs real async work (sleeps).
        /// This would deadlock/fail with the old nested-runtime approach
        /// when called from within a multi-threaded runtime context.
        struct AsyncSleepExecutor;

        #[async_trait::async_trait]
        impl crate::tool::ToolExecutor for AsyncSleepExecutor {
            async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolOutput {
                // Perform genuine async I/O to prove we're on a real async runtime.
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                ToolOutput {
                    content: format!("async tool {name} done, args={args}"),
                    bytes: 30,
                    duration_ms: 5,
                    status: ToolOutputStatus::Success,
                }
            }
        }

        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "async_op".into(),
                    arguments: serde_json::json!({"key": "val"}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("all done".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &AsyncSleepExecutor,
            None,
            None,
            None,
        )
        .await
        .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 2);
        assert_eq!(result.messages[2].role, MessageRole::Tool);
        assert!(result.messages[2].content.contains("async_op"));
    }

    // ── max_output_bytes enforcement tests ──

    /// Executor returning a configurable, potentially huge output.
    struct HugeOutputExecutor {
        content: String,
    }

    #[async_trait::async_trait]
    impl crate::tool::ToolExecutor for HugeOutputExecutor {
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ToolOutput {
            ToolOutput {
                bytes: self.content.len(),
                content: self.content.clone(),
                duration_ms: 1,
                status: ToolOutputStatus::Success,
            }
        }
    }

    #[tokio::test]
    async fn test_tool_output_truncated_to_max_output_bytes() {
        let executor = HugeOutputExecutor {
            content: "x".repeat(10_000),
        };
        let mut config = default_config();
        config.max_output_bytes = 200;

        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "big".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("done".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        let result = execute_run(&provider, config, "m1", &executor, None, None, None)
            .await
            .expect("run should succeed");

        // The Tool message that entered the conversation must carry the
        // truncation marker and stay far below the original 10_000 bytes
        // (limit_tool_output keeps ~max/4 chars + notice).
        let tool_msg = &result.messages[2];
        assert_eq!(tool_msg.role, MessageRole::Tool);
        assert!(tool_msg
            .content
            .contains("[TRUNCATED: original 10000 bytes"));
        assert!(
            tool_msg.content.len() < 1_000,
            "got {} bytes",
            tool_msg.content.len()
        );
    }

    #[tokio::test]
    async fn test_tool_output_under_limit_passes_through() {
        let executor = HugeOutputExecutor {
            content: "small".repeat(10), // 50 bytes, well under default 10_000
        };
        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "small".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("done".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &executor,
            None,
            None,
            None,
        )
        .await
        .expect("run should succeed");

        let tool_msg = &result.messages[2];
        assert_eq!(tool_msg.content, "small".repeat(10));
        assert!(!tool_msg.content.contains("[TRUNCATED"));
    }

    #[tokio::test]
    async fn test_max_output_bytes_zero_disables_cap() {
        let executor = HugeOutputExecutor {
            content: "y".repeat(5_000),
        };
        let mut config = default_config();
        config.max_output_bytes = 0; // 0 = unlimited (same convention as max_steps)

        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "big".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("done".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        let result = execute_run(&provider, config, "m1", &executor, None, None, None)
            .await
            .expect("run should succeed");

        let tool_msg = &result.messages[2];
        assert_eq!(tool_msg.content.len(), 5_000);
        assert!(!tool_msg.content.contains("[TRUNCATED"));
    }

    // ── Per-step persistence sink (Phase 3) ──────────────────────────────

    /// Message sink that records every appended message (with per-message
    /// monotonic observation order) behind a Mutex.
    struct RecordingSink(std::sync::Mutex<Vec<Message>>);

    #[async_trait::async_trait]
    impl MessageSink for RecordingSink {
        async fn append(&self, message: &Message) {
            self.0.lock().expect("sink poisoned").push(message.clone());
        }
    }

    #[tokio::test]
    async fn test_sink_receives_assistant_and_tool_messages_in_order() {
        let provider = MockProvider::new(vec![
            // Step 1: assistant asks for a tool
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "calc".into(),
                    arguments: serde_json::json!({"x": 1}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            // Step 2: final answer
            ModelResponse {
                content: Some("all done".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));
        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &MockToolExecutor,
            Some(&sink),
            None,
            None,
        )
        .await
        .expect("run should succeed");

        let observed = sink.0.lock().expect("sink poisoned").clone();
        assert_eq!(
            observed.len(),
            3,
            "assistant(tool_calls) + tool result + final assistant"
        );
        assert_eq!(observed[0].role, MessageRole::Assistant);
        assert_eq!(
            observed[0].tool_calls.as_ref().expect("tool_calls").len(),
            1,
            "assistant message must carry its tool_calls"
        );
        assert_eq!(observed[1].role, MessageRole::Tool);
        assert_eq!(
            observed[1].tool_call_id,
            Some(ToolCallId("tc-1".into())),
            "tool result must reference the call id"
        );
        assert_eq!(observed[2].role, MessageRole::Assistant);
        assert_eq!(observed[2].content, "all done");
        // The sink saw exactly the messages that entered the conversation
        // after the initial user turn.
        assert_eq!(observed.len() + 1, result.messages.len());
    }

    #[tokio::test]
    async fn test_sink_sees_message_before_loop_continues() {
        // Two tool calls back to back: the sink must have observed the first
        // step's messages by the time the second request is built. Proven by
        // a provider that snapshots the request history per call.
        use std::sync::Mutex as StdMutex;
        struct SnapshotProvider {
            responses: Vec<ModelResponse>,
            call_count: AtomicUsize,
            history_per_call: StdMutex<Vec<usize>>, // messages seen per call
        }
        #[async_trait::async_trait]
        impl ModelProvider for SnapshotProvider {
            async fn generate(
                &self,
                request: GenerateRequest,
            ) -> Result<ModelResponse, ProviderError> {
                self.history_per_call
                    .lock()
                    .expect("hist")
                    .push(request.messages.len());
                let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
                self.responses
                    .get(idx)
                    .cloned()
                    .ok_or(ProviderError::ServerError(500))
            }
            fn provider_name(&self) -> &str {
                "snapshot"
            }
        }

        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));
        let provider = SnapshotProvider {
            responses: vec![
                ModelResponse {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: ToolCallId("tc-1".into()),
                        name: "a".into(),
                        arguments: serde_json::json!({}),
                    }],
                    usage: None,
                    finish_reason: Some("tool_calls".into()),
                },
                ModelResponse {
                    content: Some("fin".into()),
                    tool_calls: vec![],
                    usage: None,
                    finish_reason: Some("stop".into()),
                },
            ],
            call_count: AtomicUsize::new(0),
            history_per_call: StdMutex::new(Vec::new()),
        };

        execute_run(
            &provider,
            default_config(),
            "m1",
            &MockToolExecutor,
            Some(&sink),
            None,
            None,
        )
        .await
        .expect("run should succeed");

        let hists = provider.history_per_call.lock().expect("hist");
        // Call 1 saw [user]; call 2 saw [user, assistant(tool_calls), tool]
        // — the first step's messages were appended (and thus persisted)
        // before the loop continued.
        assert_eq!(*hists, vec![1, 3]);
    }

    // ── Cancellation checkpoints (Phase 4) ────────────────────────────────

    /// No-op progress callback: forces execute_run down the STREAMING path
    /// (the cancel checkpoints there differ from the non-streaming ones).
    struct NoopProgress;

    impl crate::provider::ProgressCallback for NoopProgress {
        fn on_request_start(&mut self, _step: u32, _model_id: &str) {}
        fn on_first_token(&mut self) {}
        fn on_delta(&mut self, _text: &str) {}
        fn on_usage(&mut self, _usage: crate::types::Usage) {}
        fn on_request_end(&mut self) {}
        fn on_tool_start(&mut self, _name: &str, _args: &str) {}
        fn on_tool_end(&mut self, _name: &str, _bytes: usize, _truncated: bool) {}
    }

    /// Provider that streams a scripted first turn (Delta + Done with a
    /// tool_call), then on the second call notifies `second_started` and
    /// streams one Delta whose channel NEVER closes — a stream stuck
    /// mid-generation, exactly what a Ctrl-C interrupts.
    struct StreamThenStuckProvider {
        second_started: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl ModelProvider for StreamThenStuckProvider {
        async fn generate(
            &self,
            _request: GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            Err(ProviderError::ServerError(500))
        }

        async fn generate_stream(
            &self,
            request: GenerateRequest,
        ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let second_started = Arc::clone(&self.second_started);
            let is_second = !request.messages.is_empty()
                && request.messages.iter().any(|m| m.role == MessageRole::Tool);
            tokio::spawn(async move {
                if is_second {
                    second_started.notify_one();
                    // One delta, then the stream never ends and never closes.
                    let _ = tx.send(Ok(ModelStreamEvent::Delta("par".into()))).await;
                    std::future::pending::<()>().await;
                } else {
                    let _ = tx
                        .send(Ok(ModelStreamEvent::Delta("let me check".into())))
                        .await;
                    let _ = tx
                        .send(Ok(ModelStreamEvent::Done(ModelResponse {
                            content: None,
                            tool_calls: vec![ToolCall {
                                id: ToolCallId("tc-1".into()),
                                name: "calc".into(),
                                arguments: serde_json::json!({"x": 1}),
                            }],
                            usage: None,
                            finish_reason: Some("tool_calls".into()),
                        })))
                        .await;
                }
            });
            rx
        }

        fn provider_name(&self) -> &str {
            "stream-then-stuck"
        }
    }

    #[tokio::test]
    async fn test_cancel_during_stream_returns_interrupted_with_partial() {
        let provider = StreamThenStuckProvider {
            second_started: Arc::new(tokio::sync::Notify::new()),
        };
        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));
        let token = CancellationToken::new();

        // Cancel the token once the second (stuck) stream has started —
        // deterministic: Notify stores a permit if the waiter is late.
        let canceller = {
            let token = token.clone();
            let started = Arc::clone(&provider.second_started);
            tokio::spawn(async move {
                started.notified().await;
                token.cancel();
            })
        };

        let mut progress = NoopProgress;
        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &MockToolExecutor,
            Some(&sink),
            Some(&mut progress),
            Some(&token),
        )
        .await
        .expect("cancelled stream run still returns Ok");
        canceller.abort();

        // Graceful interruption: the completed step's transcript survives,
        // the half-streamed turn contributed nothing.
        assert_eq!(result.status, RunStatus::Interrupted);
        assert_eq!(result.total_steps, 1, "second step never completed");
        // user + assistant(tool_calls) + tool result — no second assistant.
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.messages[1].role, MessageRole::Assistant);
        assert_eq!(result.messages[2].role, MessageRole::Tool);

        // Sink linkage: exactly the partial transcript that the conversation
        // holds was persisted (Phase 3), keeping the run resumable.
        let observed = sink.0.lock().expect("sink poisoned").clone();
        assert_eq!(observed.len(), 2, "assistant + tool result persisted");
        assert_eq!(observed[0].role, MessageRole::Assistant);
        assert_eq!(observed[1].role, MessageRole::Tool);
    }

    /// Executor that signals `started`, then never completes (sets `finished`
    /// only after its await point — which never resumes when cancelled).
    struct StuckExecutor {
        started: Arc<tokio::sync::Notify>,
        finished: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl crate::tool::ToolExecutor for StuckExecutor {
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ToolOutput {
            self.started.notify_one();
            // Holds the tool future in-flight until cancelled.
            std::future::pending::<()>().await;
            self.finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
            ToolOutput {
                content: "never".into(),
                bytes: 0,
                duration_ms: 0,
                status: ToolOutputStatus::Success,
            }
        }
    }

    #[tokio::test]
    async fn test_cancel_during_tool_execution_aborts_tool() {
        // Step 1 requests a tool whose execution hangs; the token is
        // cancelled while the tool is in flight.
        let executor = StuckExecutor {
            started: Arc::new(tokio::sync::Notify::new()),
            finished: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "slow_tool".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("done".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);
        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));
        let token = CancellationToken::new();

        let canceller = {
            let token = token.clone();
            let started = Arc::clone(&executor.started);
            tokio::spawn(async move {
                started.notified().await;
                token.cancel();
            })
        };

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &executor,
            Some(&sink),
            None,
            Some(&token),
        )
        .await
        .expect("tool-cancel run still returns Ok");
        canceller.abort();

        assert_eq!(result.status, RunStatus::Interrupted);
        // The tool aborted: it never finished and its own output never
        // entered the conversation — instead a synthetic cancelled result
        // fills the call so the transcript keeps its pairing.
        assert!(
            !executor.finished.load(std::sync::atomic::Ordering::SeqCst),
            "tool future must have been dropped before completing"
        );
        assert_eq!(
            result.messages.len(),
            3,
            "user + assistant(tool_calls) + cancelled tool result"
        );
        assert_eq!(result.messages[1].role, MessageRole::Assistant);
        assert_eq!(result.messages[2].role, MessageRole::Tool);
        assert_eq!(
            result.messages[2].tool_call_id,
            Some(ToolCallId("tc-1".into()))
        );
        assert_eq!(
            result.messages[2].content, "[tool call cancelled before completion]",
            "the tool's own output must NOT be present"
        );
        let observed = sink.0.lock().expect("sink poisoned").clone();
        assert_eq!(
            observed.len(),
            2,
            "assistant + cancelled tool result persisted"
        );
        assert_eq!(observed[1].role, MessageRole::Tool);
        assert_eq!(
            observed[1].content,
            "[tool call cancelled before completion]"
        );
    }

    /// Provider that counts calls — proves a pre-cancelled token never
    /// reaches the provider.
    struct CountingProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ModelProvider for CountingProvider {
        async fn generate(
            &self,
            _request: GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ModelResponse {
                content: Some("should never be reached".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            })
        }
        fn provider_name(&self) -> &str {
            "counting"
        }
    }

    #[tokio::test]
    async fn test_pre_cancelled_token_returns_interrupted_without_provider_call() {
        let provider = CountingProvider {
            calls: AtomicUsize::new(0),
        };
        let token = CancellationToken::new();
        token.cancel();

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &MockToolExecutor,
            None,
            None,
            Some(&token),
        )
        .await
        .expect("pre-cancelled run still returns Ok");

        assert_eq!(result.status, RunStatus::Interrupted);
        assert_eq!(result.total_steps, 0);
        assert_eq!(
            result.messages.len(),
            1,
            "only the initial user message is returned"
        );
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            0,
            "the provider must not be called at all"
        );
    }

    #[tokio::test]
    async fn test_uncancelled_token_run_behaves_normally() {
        // A wired-but-never-cancelled token changes nothing: the run
        // completes exactly as without cancellation support.
        let provider = MockProvider::new(vec![ModelResponse {
            content: Some("all good".into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("stop".into()),
        }]);
        let token = CancellationToken::new();

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &MockToolExecutor,
            None,
            None,
            Some(&token),
        )
        .await
        .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(
            result.messages.last().map(|m| m.content.clone()),
            Some("all good".into())
        );
    }

    // ── Parallel tool batches (P2-1) ─────────────────────────────────────

    /// Executor for the parallel-batch tests: sleeps a per-name delay,
    /// tracks how many executions are active at once (overlap probe), and
    /// honors two special names — "boom" panics; "quick" sleeps briefly,
    /// THEN cancels the wired token right before answering (a
    /// deterministic "cancellation lands mid-batch, after this tool
    /// answered").
    struct BatchProbeExecutor {
        token: Option<CancellationToken>,
        active: AtomicUsize,
        max_active: Arc<AtomicUsize>,
        calls: AtomicUsize,
    }

    impl BatchProbeExecutor {
        fn new() -> Self {
            Self {
                token: None,
                active: AtomicUsize::new(0),
                max_active: Arc::new(AtomicUsize::new(0)),
                calls: AtomicUsize::new(0),
            }
        }

        fn with_token(token: CancellationToken) -> Self {
            Self {
                token: Some(token),
                ..Self::new()
            }
        }

        fn delay_for(name: &str) -> u64 {
            match name {
                "slow" | "slow2" => 220,
                "run_code" | "call_agent" => 120,
                "boom" | "quick" => 5,
                _ => 20,
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::tool::ToolExecutor for BatchProbeExecutor {
        async fn execute(&self, name: &str, _args: &serde_json::Value) -> ToolOutput {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if name == "boom" {
                panic!("kaboom");
            }
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now, Ordering::SeqCst);
            let delay = Self::delay_for(name);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            if name == "quick" {
                // Cancel only AFTER the answer is ready: the pending select
                // branch polling this future resolves it first, so the
                // result gets collected; the token is observed on the next
                // iteration.
                if let Some(t) = &self.token {
                    t.cancel();
                }
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            ToolOutput {
                content: format!("done:{name}"),
                bytes: 5 + name.len(),
                duration_ms: delay,
                status: ToolOutputStatus::Success,
            }
        }
    }

    /// Two direct tool calls in one step.
    fn two_call_response(first: &str, second: &str) -> ModelResponse {
        ModelResponse {
            content: None,
            tool_calls: vec![
                ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: first.into(),
                    arguments: serde_json::json!({}),
                },
                ToolCall {
                    id: ToolCallId("tc-2".into()),
                    name: second.into(),
                    arguments: serde_json::json!({}),
                },
            ],
            usage: None,
            finish_reason: Some("tool_calls".into()),
        }
    }

    fn final_text_response(text: &str) -> ModelResponse {
        ModelResponse {
            content: Some(text.into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("stop".into()),
        }
    }

    #[tokio::test]
    async fn parallel_batch_wall_time_is_max_not_sum() {
        // Two equally slow tools in one batch must overlap: the batch's
        // wall time is ~max(durations), not their sum (which the
        // sequential loop would take).
        let executor = BatchProbeExecutor::new();
        let provider = MockProvider::new(vec![
            two_call_response("slow", "slow2"),
            final_text_response("all done"),
        ]);

        let started = Instant::now();
        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &executor,
            None,
            None,
            None,
        )
        .await
        .expect("run should succeed");
        let elapsed = started.elapsed();

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            2,
            "both tools dispatched"
        );
        assert_eq!(
            executor.max_active.load(Ordering::SeqCst),
            2,
            "the two executions must be in flight at the same time"
        );
        // Sequential would need >= 220+220 = 440ms; parallel lands at
        // ~220ms. Generous margins on both sides to stay flake-free.
        assert!(
            elapsed >= Duration::from_millis(215),
            "tools must actually have run: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(430),
            "batch must take ~max, not the sum: {elapsed:?}"
        );
        // Results still land in tool_call order.
        assert_eq!(
            result.messages[2].tool_call_id,
            Some(ToolCallId("tc-1".into()))
        );
        assert_eq!(result.messages[2].content, "done:slow");
        assert_eq!(
            result.messages[3].tool_call_id,
            Some(ToolCallId("tc-2".into()))
        );
        assert_eq!(result.messages[3].content, "done:slow2");
    }

    /// Progress callback recording only the tool events (name-tagged), to
    /// prove the batch-emission order around the join.
    struct ToolEventProgress(std::sync::Mutex<Vec<String>>);

    impl crate::provider::ProgressCallback for ToolEventProgress {
        fn on_request_start(&mut self, _step: u32, _model_id: &str) {}
        fn on_first_token(&mut self) {}
        fn on_delta(&mut self, _text: &str) {}
        fn on_usage(&mut self, _usage: crate::types::Usage) {}
        fn on_request_end(&mut self) {}
        fn on_tool_start(&mut self, name: &str, _args: &str) {
            self.0
                .lock()
                .expect("progress poisoned")
                .push(format!("start:{name}"));
        }
        fn on_tool_end(&mut self, name: &str, _bytes: usize, _truncated: bool) {
            self.0
                .lock()
                .expect("progress poisoned")
                .push(format!("end:{name}"));
        }
    }

    #[tokio::test]
    async fn parallel_batch_results_and_events_stay_in_tool_call_order() {
        // A mixed read/write-style batch (write_file slow, shell fast):
        // the fast tool finishes first, but results, sink writes, and
        // progress events all keep tool_call order — and the transcript
        // stays provider-valid (assistant tool_calls followed by exactly
        // one paired Tool message per call, in order).
        let executor = BatchProbeExecutor::new();
        let provider = MockProvider::new(vec![
            two_call_response("write_file", "shell"),
            final_text_response("mixed batch done"),
        ]);
        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));
        let mut progress = ToolEventProgress(std::sync::Mutex::new(Vec::new()));

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &executor,
            Some(&sink),
            Some(&mut progress),
            None,
        )
        .await
        .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(
            executor.max_active.load(Ordering::SeqCst),
            2,
            "the batch really ran concurrently"
        );

        // Transcript: user + assistant(2 tool_calls) + tool + tool + assistant.
        assert_eq!(result.messages.len(), 5);
        let assistant = &result.messages[1];
        assert_eq!(assistant.role, MessageRole::Assistant);
        assert_eq!(assistant.tool_calls.as_ref().expect("tcs").len(), 2);
        let tool_a = &result.messages[2];
        let tool_b = &result.messages[3];
        assert_eq!(tool_a.role, MessageRole::Tool);
        assert_eq!(tool_a.tool_call_id, Some(ToolCallId("tc-1".into())));
        assert_eq!(tool_a.name.as_deref(), Some("write_file"));
        assert_eq!(tool_a.content, "done:write_file");
        assert_eq!(tool_b.role, MessageRole::Tool);
        assert_eq!(tool_b.tool_call_id, Some(ToolCallId("tc-2".into())));
        assert_eq!(tool_b.name.as_deref(), Some("shell"));
        assert_eq!(tool_b.content, "done:shell");

        // Sink wrote the same messages, in the same order, one by one.
        let observed = sink.0.lock().expect("sink poisoned").clone();
        assert_eq!(observed.len(), 4);
        assert_eq!(observed[1].tool_call_id, Some(ToolCallId("tc-1".into())));
        assert_eq!(observed[2].tool_call_id, Some(ToolCallId("tc-2".into())));

        // Progress emission: ALL starts fire before dispatch (in tool_call
        // order), ALL ends fire after the join (in tool_call order) — even
        // though `shell` completed long before `write_file`.
        let events = progress.0.lock().expect("progress poisoned").clone();
        assert_eq!(
            events,
            vec![
                "start:write_file",
                "start:shell",
                "end:write_file",
                "end:shell",
            ],
            "progress must be batch-emitted in tool_call order"
        );
    }

    #[tokio::test]
    async fn parallel_batch_cancel_drains_completed_and_synthesizes_unanswered() {
        // `quick` answers and cancels the token mid-batch; `slow` is still
        // in flight. The completed result must survive (drained), only the
        // truly unanswered call gets the synthetic cancelled result, and
        // everything lands on the sink — the partial transcript stays
        // resumable.
        let token = CancellationToken::new();
        let executor = BatchProbeExecutor::with_token(token.clone());
        let provider = MockProvider::new(vec![
            two_call_response("quick", "slow"),
            final_text_response("never reached"),
        ]);
        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &executor,
            Some(&sink),
            None,
            Some(&token),
        )
        .await
        .expect("cancelled batch run still returns Ok");

        assert_eq!(result.status, RunStatus::Interrupted);
        // user + assistant(2 tcs) + tool(real quick) + tool(synthetic slow)
        assert_eq!(result.messages.len(), 4);
        let done = &result.messages[2];
        assert_eq!(done.tool_call_id, Some(ToolCallId("tc-1".into())));
        assert_eq!(
            done.content, "done:quick",
            "the completed tool's REAL output must survive cancellation"
        );
        let cancelled = &result.messages[3];
        assert_eq!(cancelled.tool_call_id, Some(ToolCallId("tc-2".into())));
        assert_eq!(
            cancelled.content, "[tool call cancelled before completion]",
            "only the unanswered call gets the synthetic result"
        );

        // And exactly that partial transcript was persisted.
        let observed = sink.0.lock().expect("sink poisoned").clone();
        assert_eq!(observed.len(), 3, "assistant + drained + synthesised");
        assert_eq!(observed[0].role, MessageRole::Assistant);
        assert_eq!(observed[1].content, "done:quick");
        assert_eq!(
            observed[2].content,
            "[tool call cancelled before completion]"
        );
    }

    #[tokio::test]
    async fn parallel_batch_arguments_validated_before_any_dispatch() {
        // A malformed call anywhere in the batch aborts the run BEFORE any
        // tool is dispatched (the same "bad arguments kill the run"
        // semantics as the sequential loop, hoisted over the batch).
        let executor = BatchProbeExecutor::new();
        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![
                    ToolCall {
                        id: ToolCallId("tc-1".into()),
                        name: "fine".into(),
                        arguments: serde_json::json!({"ok": true}),
                    },
                    ToolCall {
                        id: ToolCallId("tc-2".into()),
                        name: "broken".into(),
                        arguments: serde_json::Value::String("not-an-object".into()),
                    },
                ],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            final_text_response("never reached"),
        ]);

        let err = execute_run(
            &provider,
            default_config(),
            "m1",
            &executor,
            None,
            None,
            None,
        )
        .await
        .expect_err("malformed arguments must kill the run");
        assert!(matches!(
            err,
            OpenSlateError::Runtime(RuntimeError::ToolArgumentError { .. })
        ));
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            0,
            "validation happens before ANY dispatch"
        );
    }

    #[tokio::test]
    async fn parallel_tool_calls_false_restores_sequential_execution() {
        // The escape hatch: with the flag off, a batch runs one tool at a
        // time (full ordering) — the pre-P2-1 behavior, exactly.
        let executor = BatchProbeExecutor::new();
        let provider = MockProvider::new(vec![
            two_call_response("slow", "slow2"),
            final_text_response("serial again"),
        ]);
        let mut config = default_config();
        config.parallel_tool_calls = false;

        let started = Instant::now();
        let result = execute_run(&provider, config, "m1", &executor, None, None, None)
            .await
            .expect("run should succeed");
        let elapsed = started.elapsed();

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(
            executor.max_active.load(Ordering::SeqCst),
            1,
            "sequential loop: never more than one execution in flight"
        );
        assert!(
            elapsed >= Duration::from_millis(440),
            "sequential takes the sum of durations: {elapsed:?}"
        );
        assert_eq!(result.messages[2].content, "done:slow");
        assert_eq!(result.messages[3].content, "done:slow2");
    }

    #[tokio::test]
    async fn batches_with_call_agent_or_run_code_run_sequentially() {
        // Name-based serial dispatch (P2-1): a batch containing call_agent
        // or run_code always takes the sequential loop, regardless of the
        // parallel flag — they own recursion/approval invariants.
        for serial_name in ["call_agent", "run_code"] {
            let executor = BatchProbeExecutor::new();
            let provider = MockProvider::new(vec![
                two_call_response(serial_name, "fast"),
                final_text_response("serial by name"),
            ]);

            let result = execute_run(
                &provider,
                default_config(),
                "m1",
                &executor,
                None,
                None,
                None,
            )
            .await
            .expect("run should succeed");

            assert_eq!(result.status, RunStatus::Completed);
            assert_eq!(
                executor.max_active.load(Ordering::SeqCst),
                1,
                "'{serial_name}' batches must dispatch one tool at a time"
            );
            // Both tools still executed, in order.
            assert_eq!(result.messages[2].content, format!("done:{serial_name}"));
            assert_eq!(result.messages[3].content, "done:fast");
        }
    }

    #[tokio::test]
    async fn parallel_batch_panic_is_isolated_to_its_own_tool() {
        // catch_unwind stays per-future: one panicking tool becomes an
        // error tool result; its siblings run and answer normally.
        let executor = BatchProbeExecutor::new();
        let provider = MockProvider::new(vec![
            two_call_response("boom", "steady"),
            final_text_response("survived the panic"),
        ]);

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &executor,
            None,
            None,
            None,
        )
        .await
        .expect("a panicking tool must not kill the run");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
        let boom = &result.messages[2];
        assert_eq!(boom.tool_call_id, Some(ToolCallId("tc-1".into())));
        assert!(
            boom.content.contains("panicked") && boom.content.contains("kaboom"),
            "panic captured as the tool's own error output: {}",
            boom.content
        );
        let steady = &result.messages[3];
        assert_eq!(steady.tool_call_id, Some(ToolCallId("tc-2".into())));
        assert_eq!(steady.content, "done:steady");
    }

    // ── Cost accumulation (P2-3) ─────────────────────────────────────────

    #[test]
    fn cost_spec_cost_of_prices_usage_per_mtok() {
        let spec = CostSpec::from_prices(Some(2.0), Some(6.0));
        assert!(
            (spec.cost_of(&Usage {
                input_tokens: 500_000,
                output_tokens: 100_000,
                cached_input_tokens: None
            }) - 1.6f64)
                .abs()
                < 1e-12
        );
        // Absent prices contribute 0 on that side only.
        let input_only = CostSpec::from_prices(Some(1.0), None);
        assert!(
            (input_only.cost_of(&Usage {
                input_tokens: 1_000_000,
                output_tokens: 999,
                cached_input_tokens: None
            }) - 1.0f64)
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn cost_spec_unconfigured_costs_zero_and_is_not_configured() {
        let spec = CostSpec::default();
        assert!(!spec.is_configured());
        assert_eq!(
            spec.cost_of(&Usage {
                input_tokens: 123_456,
                output_tokens: 65_432,
                cached_input_tokens: None
            }),
            0.0,
            "unconfigured pricing must record cost 0"
        );
        assert!(CostSpec::from_prices(None, Some(0.1)).is_configured());
        assert!(CostSpec::from_prices(Some(0.1), None).is_configured());
    }

    #[tokio::test]
    async fn test_cost_accumulates_across_multi_step_run() {
        // Two steps, each with usage: 1k/2k in then 3k/4k out tokens.
        // Pricing $2/M in, $5/M out →
        //   step1: 1000*2e-6 + 2000*5e-6 = 0.012
        //   step2: 3000*2e-6 + 4000*5e-6 = 0.026  → total 0.038
        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "bash".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: Some(Usage {
                    input_tokens: 1_000,
                    output_tokens: 2_000,
                    cached_input_tokens: None,
                }),
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("done".into()),
                tool_calls: vec![],
                usage: Some(Usage {
                    input_tokens: 3_000,
                    output_tokens: 4_000,
                    cached_input_tokens: None,
                }),
                finish_reason: Some("stop".into()),
            },
        ]);

        let mut config = default_config();
        config.cost = CostSpec::from_prices(Some(2.0), Some(5.0));

        let result = execute_run(&provider, config, "m1", &MockToolExecutor, None, None, None)
            .await
            .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_input_tokens, 4_000);
        assert_eq!(result.total_output_tokens, 6_000);
        assert!(
            (result.total_cost_usd - 0.038f64).abs() < 1e-12,
            "multi-step cost must accumulate, got {}",
            result.total_cost_usd
        );
    }

    #[tokio::test]
    async fn test_cost_stays_zero_without_pricing() {
        let provider = MockProvider::new(vec![ModelResponse {
            content: Some("answer".into()),
            tool_calls: vec![],
            usage: Some(Usage {
                input_tokens: 9_999,
                output_tokens: 1_111,
                cached_input_tokens: None,
            }),
            finish_reason: Some("stop".into()),
        }]);

        let result = execute_run(
            &provider,
            default_config(),
            "m1",
            &MockToolExecutor,
            None,
            None,
            None,
        )
        .await
        .expect("run should succeed");

        assert_eq!(result.total_cost_usd, 0.0);
    }
}
