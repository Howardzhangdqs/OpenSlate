//! `openslate run` command — execute a single agent run.
//!
//! Uses the `AppContext` wiring to assemble all components and execute
//! a complete end-to-end run: config → validation → SQLite store → agent tree →
//! tool registry → provider → RunManager → execute → result output.

use anyhow::{Context, Result};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use openslate_core::agent_tree::AgentTree;
use openslate_core::provider::ModelProvider;
use openslate_core::run_manager::RunManager;
use openslate_core::runtime::{CancellationToken, MessageSink};
use openslate_core::types::{Message, MessageRole, RunId, RunStatus};
use openslate_store_sqlite::recorder::RunRecorder;

use crate::input::{expand_at_files, read_stdin_if_pipe, WorkspaceRoot};
use crate::spinner::SpinnerCallback;
use crate::wiring as app_wiring;

/// Output format for run results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Jsonl,
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "text" => Ok(OutputFormat::Text),
            "jsonl" => Ok(OutputFormat::Jsonl),
            _ => Err(format!(
                "unsupported format '{}'; supported: text, jsonl",
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
    pub trace_path: Option<String>,
    /// Auto-approve every tool call (`--yes`): forces the approval policy
    /// to `auto` for this run, overriding `[approval].policy`.
    pub yes: bool,
    /// Resume a previous run by ID (Phase 3): the run's persisted messages
    /// are loaded as the prior conversation and execution continues under
    /// the same run id. Interrupted / cancelled / crashed (`running`) runs
    /// are all resumable, including partial tool transcripts.
    pub resume: Option<String>,
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
    duration_ms: u64,
    step_output: u64,
    tps: u64,
    input_tokens: Option<u32>,
    llm_elapsed_secs: f64,
) -> Result<()> {
    // LLM outputs (especially right after a reasoning/thinking block) often
    // carry leading blank lines; trim them so the answer isn't pushed down by
    // empty rows between the thinking and the reply.
    let content = content.trim();
    match format {
        OutputFormat::Text => {
            if let Some(path) = output_path {
                fs::write(path, content)
                    .with_context(|| format!("Failed to write output to '{}'", path))?;
                if !quiet {
                    tracing::info!("Result written to {}", path);
                }
            } else {
                crate::markdown::print_markdown(content);
            }
        }
        OutputFormat::Jsonl => {
            let lines = generate_jsonl_events(result);
            let output = lines.join("\n");
            if let Some(path) = output_path {
                fs::write(path, &output)
                    .with_context(|| format!("Failed to write output to '{}'", path))?;
                if !quiet {
                    tracing::info!("Result written to {}", path);
                }
            } else {
                println!("{}", output);
            }
        }
    }

    // Final-step stats line, printed AFTER the assistant content and BEFORE
    // "Run done" (so: content → stats → Run done). Tool-call steps already got
    // their stats via on_step_end inside the runtime loop (after `-> / <-`).
    if !quiet {
        let in_seg = input_tokens.map(|i| format!("↑{} ", i)).unwrap_or_default();
        let tps_seg = if tps > 0 {
            format!(" · {}tok/s", tps)
        } else {
            String::new()
        };
        tracing::info!(
            target: "openslate_runtime",
            "{:.1}s · {}↓{}{}",
            llm_elapsed_secs,
            in_seg,
            step_output,
            tps_seg
        );
    }

    if !quiet {
        let dur_str = if duration_ms >= 1000 {
            format!("{:.1}s", duration_ms as f64 / 1000.0)
        } else {
            format!("{}ms", duration_ms)
        };
        let run_id_str = result.run_id.to_string();
        let short_id = run_id_str.get(..8).unwrap_or(&run_id_str);
        // "Run done" reflects the WHOLE run: total_output_tokens is the
        // accumulated completion_tokens across all steps (accurate, includes
        // reasoning + content + tool_call). Throughput is over the full run
        // wall-clock.
        let total_output = result.total_output_tokens;
        let run_tps = if duration_ms > 0 {
            (total_output as f64 * 1000.0 / duration_ms as f64).round() as u64
        } else {
            0
        };
        let tps_seg = if run_tps > 0 {
            format!(" · {}tok/s", run_tps)
        } else {
            String::new()
        };
        tracing::info!(
            "Run done · {} · {} step · {} · ↑{} ↓{}{}",
            short_id,
            result.total_steps,
            dur_str,
            result.total_input_tokens,
            total_output,
            tps_seg,
        );
    }

    Ok(())
}

/// Generate JSONL event lines from a managed run result.
fn generate_jsonl_events(result: &openslate_core::run_manager::ManagedRunResult) -> Vec<String> {
    use openslate_core::types::MessageRole;

    let mut lines = Vec::new();

    // run_start event
    let run_start = serde_json::json!({
        "type": "run_start",
        "run_id": result.run_id.to_string(),
        "agent_id": result.execution_tree.root().agent_id.to_string(),
        "model": result.model,
    });
    lines.push(run_start.to_string());

    // step and tool_result events from messages
    let mut step_count = 0u32;
    for msg in &result.messages {
        match msg.role {
            MessageRole::Assistant => {
                step_count += 1;
                // tool_calls carried by the assistant turn (empty when the
                // turn was plain text). Round-tripped from the live Message,
                // so id / name / arguments are complete — not a stub.
                let tool_calls: Vec<serde_json::Value> = msg
                    .tool_calls
                    .as_ref()
                    .map(|tcs| {
                        tcs.iter()
                            .map(|tc| {
                                serde_json::json!({
                                    "id": tc.id.to_string(),
                                    "name": tc.name,
                                    "arguments": tc.arguments,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let step_event = serde_json::json!({
                    "type": "step",
                    "step": step_count,
                    "role": "assistant",
                    "content": msg.content,
                    "tool_calls": tool_calls,
                });
                lines.push(step_event.to_string());
            }
            MessageRole::Tool => {
                let tool_name = msg.name.as_deref().unwrap_or("unknown");
                let tool_result = serde_json::json!({
                    "type": "tool_result",
                    "tool": tool_name,
                    "output": msg.content,
                });
                lines.push(tool_result.to_string());
            }
            // User and System messages are not emitted as separate events
            // in the JSONL format (they're included in step events conceptually)
            MessageRole::User | MessageRole::System => {}
        }
    }

    // run_end event
    let run_end = serde_json::json!({
        "type": "run_end",
        "run_id": result.run_id.to_string(),
        "status": serde_json::to_string(&result.status).unwrap().trim_matches('"'),
        "steps": result.total_steps,
        "input_tokens": result.total_input_tokens,
        "output_tokens": result.total_output_tokens,
        // P2-3: run-wide model spend in USD (0.0 when no pricing is
        // configured — same "未配置→记 0" rule as the store column).
        "cost_usd": result.total_cost_usd,
    });
    lines.push(run_end.to_string());

    lines
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

/// Map a run status to the store's lowercase vocabulary.
fn status_str(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "running",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Interrupted => "interrupted",
        RunStatus::Cancelled => "cancelled",
    }
}

/// Whether the given model alias has any pricing configured (P2-3) —
/// drives the "pricing not configured" variant of the cost line.
pub(crate) fn model_pricing_configured(
    config: &openslate_core::config::OpenSlateConfig,
    model_alias: &str,
) -> bool {
    config
        .models
        .get(model_alias)
        .map(|m| {
            openslate_core::runtime::CostSpec::from_prices(
                m.input_price_per_mtok,
                m.output_price_per_mtok,
            )
            .is_configured()
        })
        .unwrap_or(false)
}

/// Format a token count as `N.Nk` (e.g. 1234 → "1.2k", 0 → "0.0k").
fn fmt_k_tokens(tokens: u64) -> String {
    format!("{:.1}k", tokens as f64 / 1000.0)
}

/// The run-end cost line (P2-3): `cost: $X.XXXX (in Nk/out Nk tok)`, or
/// `cost: pricing not configured` when the model carries no prices (and
/// nothing was ever priced — a mid-run model switch with prices still
/// shows the number).
pub(crate) fn format_cost_line(
    pricing_configured: bool,
    cost_usd: f64,
    input_tokens: u64,
    output_tokens: u64,
) -> String {
    if !pricing_configured && cost_usd == 0.0 {
        "cost: pricing not configured".to_owned()
    } else {
        format!(
            "cost: ${:.4} (in {}/out {} tok)",
            cost_usd,
            fmt_k_tokens(input_tokens),
            fmt_k_tokens(output_tokens)
        )
    }
}

/// Derive a short run title from the prompt (char-boundary safe for CJK).
fn title_from_prompt(prompt: &str) -> Option<String> {
    let mut end = prompt.len().min(60);
    while end > 0 && !prompt.is_char_boundary(end) {
        end -= 1;
    }
    Some(prompt[..end].to_owned()).filter(|s| !s.is_empty())
}

/// Run the `openslate run` command.
pub async fn run_run_command(params: RunParams) -> Result<()> {
    // 1. Build the full app context (config → validation → store → agent tree → registry)
    let mut ctx = app_wiring::build_app_context(params.config_path.as_deref()).await?;

    // 1.5 Approval wiring (Phase 1): `--yes` > `[approval].policy` > default
    //     auto. Non-interactive runs with a non-auto effective policy get
    //     the high-risk gate (deny) + a one-line WARN; `--yes` skips all of
    //     it. Applied before execution so every layer of the run is covered.
    app_wiring::apply_approval(&mut ctx.manager, &ctx.config, false, params.yes);

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

    // 4. Conversation setup (Phase 3): either resume a persisted run
    //    (`--resume <run_id>`, optionally with a new `--prompt` appended)
    //    or start fresh from the prompt/stdin. Either way the run row is
    //    inserted (`status = "running"`) and the initial user message is
    //    persisted BEFORE the first model call, so even a crash there
    //    leaves a resumable run behind.
    let (prior_messages, run_id, recorder) = match params.resume.as_deref() {
        Some(run_id_str) => {
            let store = ctx.store.as_ref().ok_or_else(|| {
                anyhow::anyhow!("--resume requires a SQLite store, but none is configured")
            })?;
            let run_id = RunId(run_id_str.to_owned());
            let recorder = RunRecorder::resume(store.clone(), run_id.clone(), &agent.id.0)
                .await
                .with_context(|| format!("Failed to resume run '{}'", run_id_str))?;
            let mut prior = RunRecorder::load_messages(store, run_id_str)
                .await
                .with_context(|| format!("Failed to load messages for run '{}'", run_id_str))?;
            if prior.is_empty() {
                anyhow::bail!("Run '{}' has no persisted messages to resume", run_id_str);
            }
            // An optional --prompt becomes the next user turn on top of the
            // restored conversation.
            if let Some(ref p) = params.prompt {
                let user_message = Message {
                    role: MessageRole::User,
                    content: p.clone(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: None,
                };
                if let Err(e) = recorder.write_message(&user_message).await {
                    tracing::warn!("Failed to persist resume prompt: {}", e);
                }
                prior.push(user_message);
            }
            tracing::info!(
                "Resuming run {} ({} messages restored)",
                run_id_str,
                prior.len()
            );
            (prior, run_id, Some(Arc::new(recorder)))
        }
        None => {
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
            let user_message = Message {
                role: MessageRole::User,
                content: prompt.clone(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            };
            let run_id = RunManager::new_run_id();
            let recorder = match ctx.store {
                Some(ref store) => {
                    let input_json = serde_json::json!({ "prompt": &prompt }).to_string();
                    match RunRecorder::begin(
                        store.clone(),
                        run_id.clone(),
                        &agent.id.0,
                        title_from_prompt(&prompt).as_deref(),
                        &input_json,
                    )
                    .await
                    {
                        Ok(rec) => {
                            if let Err(e) = rec.write_message(&user_message).await {
                                tracing::warn!("Failed to persist initial user message: {}", e);
                            }
                            Some(Arc::new(rec))
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Run persistence unavailable (continuing without): {}",
                                e
                            );
                            None
                        }
                    }
                }
                None => None,
            };
            (vec![user_message], run_id, recorder)
        }
    };

    // 5. Per-step persistence (Phase 3): the recorder becomes the runtime's
    //    message sink; assistant messages and tool results hit SQLite the
    //    moment they enter the conversation.
    ctx.manager.message_sink = recorder.clone().map(|r| r as Arc<dyn MessageSink>);

    // 6. Execute via RunManager under the pre-allocated run id
    let (result, run_elapsed_ms, step_output, tps, input_tokens, llm_elapsed, cancelled_by_signal) = {
        // Spinner provides real-time progress via streaming callbacks.
        // In quiet mode, the spinner is hidden.
        // `run` mode always hides the spinner animation line (`⠙ main`) — it's
        // noise for a one-shot command. Streaming reasoning/tool lines still
        // print above; the per-request "Step N" log + final "Run done" carry
        // status. (params.quiet still governs the Run done line + result only.)
        let mut callback = SpinnerCallback::new(&model_alias, true);
        let exec_start = Instant::now();
        // Ctrl-C cancellation (Phase 4): the token is threaded through the
        // runtime, whose checkpoints return Ok(Interrupted) with the partial
        // transcript — the run future is NEVER dropped here (that would lose
        // everything); the signal listener is the only belt-and-suspenders.
        let cancel = CancellationToken::new();
        let cancelled_by_signal = Arc::new(AtomicBool::new(false));
        let signal_token = cancel.clone();
        let cancel_flag = Arc::clone(&cancelled_by_signal);
        let ctrl_c = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel_flag.store(true, Ordering::SeqCst);
                signal_token.cancel();
            }
        });
        let run_result = ctx
            .manager
            .execute_with_run_id(
                run_id,
                &*provider,
                &prior_messages,
                cancel,
                Some(&mut callback),
            )
            .await;
        ctrl_c.abort();
        let cancelled_by_signal = cancelled_by_signal.load(Ordering::SeqCst);
        match run_result {
            Ok(r) => {
                // Capture the streamed content/reasoning split before finish() consumes
                // the callback (used for the "Run done" log line).
                // step_output is the provider's accurate output_tokens for the final
                // step (includes reasoning + content + tool_call). Used for the
                // final-step stats line.
                let step_output = callback.real_output().unwrap_or(0) as u64;
                let tps = callback.tps();
                let input_tokens = callback.input_tokens();
                let llm_elapsed = callback.elapsed();
                let elapsed = exec_start.elapsed();
                // Skip the per-run `✓ model ...` spinner summary — its tok/s folds into
                // the "Run done" line below, so we avoid a redundant stats row.
                callback.finish_silent();
                (
                    r,
                    elapsed.as_millis() as u64,
                    step_output,
                    tps,
                    input_tokens,
                    llm_elapsed,
                    cancelled_by_signal,
                )
            }
            Err(e) => {
                callback.finish_with_error(&e.to_string());
                // A token cancelled before the run started surfaces
                // RuntimeError::Cancelled — same Ctrl-C exit path as a
                // mid-run cancellation (partial state, exit 130).
                if matches!(
                    e,
                    openslate_core::error::OpenSlateError::Runtime(
                        openslate_core::error::RuntimeError::Cancelled
                    )
                ) {
                    if let Some(ref rec) = recorder {
                        if let Err(pe) = rec.finish("cancelled", None, 0.0).await {
                            tracing::warn!("Failed to persist cancelled status: {}", pe);
                        }
                    }
                    println!("Run cancelled before it started");
                    std::process::exit(130);
                }
                // Failure path must be on disk too (Phase 3): the run row
                // already exists (inserted upfront), flip it to `failed`.
                if let Some(ref rec) = recorder {
                    let error_json = serde_json::json!({ "error": e.to_string() }).to_string();
                    if let Err(pe) = rec.finish("failed", Some(&error_json), 0.0).await {
                        tracing::warn!("Failed to persist run failure status: {}", pe);
                    }
                }
                return Err(anyhow::anyhow!("Run failed: {}", e));
            }
        }
    };

    // 6.4 Ctrl-C cancellation (Phase 4): print the completed part, persist
    //     the run row as `cancelled` (its partial transcript is already on
    //     disk via the Phase 3 sink, so `--resume <run_id>` continues it),
    //     and exit 130 (128 + SIGINT convention).
    if cancelled_by_signal && result.status == RunStatus::Interrupted {
        let partial = extract_final_assistant_message(&result.messages);
        match params.format {
            OutputFormat::Text => {
                if let Some(content) = partial.as_deref() {
                    if !content.trim().is_empty() {
                        crate::markdown::print_markdown(content.trim());
                    }
                }
            }
            OutputFormat::Jsonl => {
                println!("{}", generate_jsonl_events(&result).join("\n"));
            }
        }
        if let Some(ref rec) = recorder {
            let output_json = serde_json::json!({ "output": partial }).to_string();
            if let Err(e) = rec
                .finish("cancelled", Some(&output_json), result.total_cost_usd)
                .await
            {
                tracing::warn!("Failed to persist cancelled status: {}", e);
            }
            let run_id_str = result.run_id.to_string();
            let short_id = run_id_str.get(..8).unwrap_or(&run_id_str);
            println!("Run cancelled — 已保存已完成部分,可用 --resume {short_id} 续跑");
        } else {
            println!("Run cancelled");
        }
        std::process::exit(130);
    }

    // 6.5 Run lifecycle finish (Phase 3): completed / interrupted / cancelled
    //     states land on the run row with the final answer payload.
    if let Some(ref rec) = recorder {
        let final_message = extract_final_assistant_message(&result.messages);
        let output_json = serde_json::json!({ "output": final_message }).to_string();
        if let Err(e) = rec
            .finish(
                status_str(result.status),
                Some(&output_json),
                result.total_cost_usd,
            )
            .await
        {
            tracing::warn!("Failed to persist run completion status: {}", e);
        }
    }

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
        run_elapsed_ms,
        step_output,
        tps,
        input_tokens,
        llm_elapsed.as_secs_f64(),
    )?;

    // 8.5 Final cost line (P2-3), after "Run done": the run's token spend
    //     priced with the agent model's [models] pricing. "pricing not
    //     configured" when that model carries no prices (and no cost was
    //     ever accumulated — e.g. by a priced delegated child).
    if !params.quiet {
        let configured = model_pricing_configured(&ctx.config, &model_alias);
        tracing::info!(
            "{}",
            format_cost_line(
                configured,
                result.total_cost_usd,
                result.total_input_tokens,
                result.total_output_tokens
            )
        );
    }

    // 9. Export trace to file if requested
    if let Some(ref trace_path) = params.trace_path {
        let path = std::path::Path::new(trace_path);
        result
            .trace
            .export_to_file(path)
            .with_context(|| format!("Failed to export trace to '{}'", trace_path))?;
        if !params.quiet {
            tracing::info!("Trace exported to {}", trace_path);
        }
    }

    // 10. Persist execution tree + trace events to SQLite (if store available).
    //     The run row itself was inserted upfront by the recorder (Phase 3);
    //     this only stores the delegation nodes and trace spans.
    if let Some(ref store) = ctx.store {
        if let Err(e) = persist_trace_to_store(store, &result).await {
            tracing::warn!("Failed to persist trace events to SQLite: {}", e);
        }
    }

    Ok(())
}

/// Build a provider for a specific model alias.
///
/// All providers are routed through the genai adapter — the sole provider
/// implementation. `ProviderConfig.kind` is retained as an informational hint
/// (e.g. `"openai_compatible"`, `"genai"`) but no longer selects an
/// implementation. The genai `adapter` protocol (openai/anthropic/gemini/ollama)
/// is taken from `ProviderConfig.adapter`, defaulting to `"openai"` when unset
/// (the common case for OpenAI-compatible endpoints) to avoid genai's silent
/// Ollama fallthrough for unrecognized model names.
pub(crate) fn build_provider_for_model(
    config: &openslate_core::config::OpenSlateConfig,
    model_alias: &str,
) -> Result<Box<dyn ModelProvider>> {
    let resolved = openslate_core::model_config::resolve_model(config, model_alias)
        .with_context(|| format!("Failed to resolve model alias '{}'", model_alias))?;

    let api_key = std::env::var(&resolved.provider.api_key_env).with_context(|| {
        format!(
            "API key not found: set environment variable '{}'",
            resolved.provider.api_key_env
        )
    })?;

    // Default to the OpenAI adapter when unset: most OpenAI-compatible
    // providers (zhipu, minimax, internlm, …) don't set `adapter` explicitly,
    // and genai would otherwise infer Ollama from the model name.
    let adapter = resolved
        .provider
        .adapter
        .clone()
        .or_else(|| Some("openai".to_owned()));

    let cfg = openslate_model_genai::GenaiConfig {
        provider_name: resolved.provider_name.clone(),
        model: resolved.model_id.clone(),
        api_key: Some(api_key),
        base_url: Some(resolved.provider.base_url.clone()),
        adapter,
        timeout_secs: 60,
        max_attempts: resolved.provider.max_attempts,
        retry_base_ms: resolved.provider.retry_base_ms,
    };

    let provider = openslate_model_genai::GenaiProvider::new(cfg).map_err(|e| {
        anyhow::anyhow!(
            "Failed to build genai provider for '{}': {}",
            resolved.provider_name,
            e
        )
    })?;

    Ok(Box::new(provider))
}

async fn persist_trace_to_store(
    store: &openslate_store_sqlite::store::SqliteStore,
    result: &openslate_core::run_manager::ManagedRunResult,
) -> Result<()> {
    use openslate_core::trace::TraceEvent;

    let run_id_str = result.run_id.to_string();

    // NOTE (Phase 3): the run row itself is inserted BEFORE execution by the
    // RunRecorder (`status = "running"`) and finalized via update_run_status
    // on every terminal path — including failures. This function only
    // persists the delegation tree and trace spans afterwards; the run row
    // already satisfies the foreign keys below.

    // Persist every execution node (root + delegated children) so the full
    // delegation tree is queryable later. `parent_execution_id` links each
    // child back to the node that spawned it.
    for node in result.execution_tree.all_nodes() {
        let node_status = match node.status {
            openslate_core::execution::ExecutionStatus::Running => "running",
            openslate_core::execution::ExecutionStatus::Completed => "completed",
            openslate_core::execution::ExecutionStatus::Failed => "failed",
        };
        let parent_exec = node.parent_execution_id.as_ref().map(|id| id.to_string());
        store
            .insert_execution_node(
                &node.id.to_string(),
                &node.run_id.to_string(),
                &node.agent_id.to_string(),
                parent_exec.as_deref(),
                node.parent_call_id.as_deref(),
                node_status,
                "{}",
                0,
            )
            .await
            .map_err(|e| anyhow::anyhow!("Failed to insert execution node: {}", e))?;
    }

    let mut idx: usize = 0;

    for event in result.trace.events() {
        idx += 1;
        let event_id = format!("trace-{}-{}", run_id_str, idx);
        let (event_name, event_kind, ts_ns, dur_ns, track, args_json) = match event {
            TraceEvent::DurationBegin { name, ts, args, .. } => {
                let args_str = if args.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(args).unwrap_or_default())
                };
                (
                    name.clone(),
                    "duration_begin".to_owned(),
                    (*ts as i64) * 1000,
                    None,
                    "main".to_owned(),
                    args_str,
                )
            }
            TraceEvent::DurationEnd { name, ts, .. } => (
                name.clone(),
                "duration_end".to_owned(),
                (*ts as i64) * 1000,
                None,
                "main".to_owned(),
                None,
            ),
            TraceEvent::Complete {
                name,
                ts,
                dur,
                args,
                ..
            } => {
                let args_str = if args.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(args).unwrap_or_default())
                };
                (
                    name.clone(),
                    "complete".to_owned(),
                    (*ts as i64) * 1000,
                    Some((*dur as i64) * 1000),
                    "main".to_owned(),
                    args_str,
                )
            }
            TraceEvent::Instant { name, ts, .. } => (
                name.clone(),
                "instant".to_owned(),
                (*ts as i64) * 1000,
                None,
                "main".to_owned(),
                None,
            ),
            TraceEvent::Counter {
                name, ts, values, ..
            } => {
                let args_str = serde_json::to_string(values).unwrap_or_default();
                (
                    name.clone(),
                    "counter".to_owned(),
                    (*ts as i64) * 1000,
                    None,
                    "main".to_owned(),
                    Some(args_str),
                )
            }
        };

        store
            .insert_trace_event(
                &event_id,
                &run_id_str,
                None,
                None,
                None,
                &event_name,
                &event_kind,
                ts_ns,
                dur_ns,
                &track,
                args_json.as_deref(),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Failed to insert trace event: {}", e))?;
    }

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
        fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        let agents_dir = openslate_dir.join("agents");
        fs::create_dir(&agents_dir).expect("create agents dir");
        let agent_md = "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - read_file\n---\nYou are the root agent.\n";
        fs::write(agents_dir.join("root.md"), agent_md).expect("write root.md");
        tmp
    }

    #[test]
    fn test_output_format_parse_text() {
        assert_eq!("text".parse::<OutputFormat>(), Ok(OutputFormat::Text));
    }

    #[test]
    fn test_output_format_parse_jsonl() {
        assert_eq!("jsonl".parse::<OutputFormat>(), Ok(OutputFormat::Jsonl));
    }

    #[test]
    fn test_output_format_parse_unsupported() {
        assert!("csv".parse::<OutputFormat>().is_err());
        assert!("xml".parse::<OutputFormat>().is_err());
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
                tool_calls: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "response".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
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
            tool_calls: None,
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

    /// A genai-backed provider config must construct a `GenaiProvider` successfully.
    #[test]
    fn test_genai_provider_constructs_with_feature() {
        // Unique env var name to avoid races with parallel tests.
        // SAFETY of env mutation: this var is not read by any other test.
        std::env::set_var("GENAI_TEST_KEY", "sk-test");
        let toml = r#"
[providers.anthropic_prod]
base_url = "https://api.anthropic.com"
api_key_env = "GENAI_TEST_KEY"
adapter = "anthropic"

[models.main]
provider = "anthropic_prod"
model = "claude-sonnet-4-5"
"#;
        let config = openslate_core::config::parse_openslate_toml(toml).unwrap();
        let provider = build_provider_for_model(&config, "main").expect("genai provider builds");
        assert_eq!(provider.provider_name(), "anthropic_prod");
    }

    #[test]
    fn test_resolve_agent_root_default() {
        let tmp = temp_project();
        let agents = crate::wiring::load_agents(&tmp.path().join(".openslate/agents")).unwrap();
        let tree = openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
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
base_url = "https://example.com"
api_key_env = "TEST_API_KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"
"#;
        fs::write(dir.join("openslate.toml"), toml).expect("write");
        let agents_dir = dir.join("agents");
        fs::create_dir(&agents_dir).expect("create agents dir");
        let root_md = "---\nid: root\nname: Root\nmodel: main\nchildren:\n  - worker\n---\nRoot.\n";
        let worker_md = "---\nid: worker\nname: Worker\nmodel: fast\n---\nWorker.\n";
        fs::write(agents_dir.join("root.md"), root_md).expect("write root.md");
        fs::write(agents_dir.join("worker.md"), worker_md).expect("write worker.md");

        let agents_cfg = crate::wiring::load_agents(&dir.join("agents")).unwrap();
        let tree = openslate_core::agent_tree::AgentTree::from_configs(&agents_cfg.agents).unwrap();

        let worker = resolve_agent(&tree, Some("worker")).unwrap();
        assert_eq!(worker.id.0, "worker");
        assert_eq!(worker.model_alias, "fast");
    }

    #[test]
    fn test_resolve_agent_not_found() {
        let tmp = temp_project();
        let agents = crate::wiring::load_agents(&tmp.path().join(".openslate/agents")).unwrap();
        let tree = openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let result = resolve_agent(&tree, Some("nonexistent"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    // ── JSONL tool_calls schema (Phase 3) ─────────────────────────────

    use openslate_core::execution::ExecutionTree;
    use openslate_core::run_manager::ManagedRunResult;
    use openslate_core::trace::TraceCollector;
    use openslate_core::types::{AgentId, Message, MessageRole, RunId, ToolCall, ToolCallId};

    fn synth_result(messages: Vec<Message>) -> ManagedRunResult {
        let run_id = RunId("test-run".into());
        ManagedRunResult {
            run_id: run_id.clone(),
            status: openslate_core::types::RunStatus::Completed,
            messages,
            total_steps: 1,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
            execution_tree: ExecutionTree::new(run_id, AgentId("root".into())),
            model: "test-model".into(),
            trace: TraceCollector::new(1, 1),
        }
    }

    fn parse_lines(lines: &[String]) -> Vec<serde_json::Value> {
        lines
            .iter()
            .map(|l| serde_json::from_str(l).expect("each JSONL line parses"))
            .collect()
    }

    #[test]
    fn test_jsonl_step_event_carries_tool_calls() {
        let result = synth_result(vec![
            Message {
                role: MessageRole::User,
                content: "list files".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                name: None,
                tool_calls: Some(vec![ToolCall {
                    id: ToolCallId("call-7".into()),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "ls"}),
                }]),
            },
            Message {
                role: MessageRole::Tool,
                content: "a.txt".into(),
                tool_call_id: Some(ToolCallId("call-7".into())),
                name: Some("shell".into()),
                tool_calls: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "here is the list".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
        ]);

        let events = parse_lines(&generate_jsonl_events(&result));

        let step1 = events
            .iter()
            .find(|e| e["type"] == "step" && e["step"] == 1)
            .expect("first step event");
        assert_eq!(step1["role"], "assistant");
        let tcs = step1["tool_calls"].as_array().expect("tool_calls array");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["id"], "call-7");
        assert_eq!(tcs[0]["name"], "shell");
        assert_eq!(tcs[0]["arguments"]["command"], "ls");

        let tool_event = events
            .iter()
            .find(|e| e["type"] == "tool_result")
            .expect("tool_result event");
        assert_eq!(tool_event["tool"], "shell");
        assert_eq!(tool_event["output"], "a.txt");

        let step2 = events
            .iter()
            .find(|e| e["type"] == "step" && e["step"] == 2)
            .expect("second step event");
        assert_eq!(
            step2["tool_calls"].as_array().map(Vec::len),
            Some(0),
            "plain assistant turn → empty tool_calls array"
        );

        let run_end = events
            .iter()
            .find(|e| e["type"] == "run_end")
            .expect("run_end event");
        assert_eq!(run_end["status"], "completed");
        assert_eq!(
            run_end["cost_usd"], 0.0,
            "run_end carries the cost field (0.0 when unpriced)"
        );
    }

    #[test]
    fn test_jsonl_step_event_without_tool_calls_is_empty_array() {
        let result = synth_result(vec![
            Message {
                role: MessageRole::User,
                content: "hi".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "hello".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
        ]);

        let events = parse_lines(&generate_jsonl_events(&result));
        let step = events
            .iter()
            .find(|e| e["type"] == "step")
            .expect("step event");
        assert_eq!(step["content"], "hello");
        assert_eq!(step["tool_calls"], serde_json::json!([]));
    }

    #[test]
    fn test_title_from_prompt_char_boundary_safe() {
        assert_eq!(
            title_from_prompt("hello world, this is a long prompt"),
            Some("hello world, this is a long prompt".into())
        );
        // CJK: 60 bytes lands mid-character; must back off, never panic.
        let cjk = "你好".repeat(50);
        let title = title_from_prompt(&cjk).expect("title");
        assert!(title.chars().all(|c| !c.is_whitespace()));
        assert!(title.chars().count() <= 20);
        assert!(cjk.starts_with(&title));
        assert_eq!(title_from_prompt(""), None);
    }

    // ── Cost line + JSONL cost (P2-3) ───────────────────────────────────

    #[test]
    fn test_jsonl_run_end_carries_cost_value() {
        let mut result = synth_result(vec![Message {
            role: MessageRole::User,
            content: "hi".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        }]);
        result.total_input_tokens = 12_345;
        result.total_output_tokens = 6_789;
        result.total_cost_usd = 0.0123;

        let events = parse_lines(&generate_jsonl_events(&result));
        let run_end = events
            .iter()
            .find(|e| e["type"] == "run_end")
            .expect("run_end event");
        assert_eq!(run_end["input_tokens"], 12_345);
        assert_eq!(run_end["output_tokens"], 6_789);
        assert_eq!(run_end["cost_usd"], 0.0123);
    }

    #[test]
    fn test_format_cost_line_priced() {
        assert_eq!(
            format_cost_line(true, 0.0123, 1234, 567),
            "cost: $0.0123 (in 1.2k/out 0.6k tok)"
        );
        // Configured pricing with zero spend still shows the number.
        assert_eq!(
            format_cost_line(true, 0.0, 0, 0),
            "cost: $0.0000 (in 0.0k/out 0.0k tok)"
        );
    }

    #[test]
    fn test_format_cost_line_unpriced() {
        assert_eq!(
            format_cost_line(false, 0.0, 1000, 500),
            "cost: pricing not configured"
        );
        // Cost accumulated anyway (e.g. a priced delegated child under an
        // unpriced root): the number outranks the "not configured" notice.
        assert_eq!(
            format_cost_line(false, 0.0004, 1000, 500),
            "cost: $0.0004 (in 1.0k/out 0.5k tok)"
        );
    }

    #[test]
    fn test_model_pricing_configured_from_config() {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "TEST_API_KEY"

[models.main]
provider = "zhipu"
model = "m1"
input_price_per_mtok = 0.5
output_price_per_mtok = 2.0

[models.fast]
provider = "zhipu"
model = "m2"
input_price_per_mtok = 0.1

[models.bare]
provider = "zhipu"
model = "m3"
"#;
        let config = openslate_core::config::parse_openslate_toml(toml).unwrap();
        assert!(model_pricing_configured(&config, "main"));
        assert!(
            model_pricing_configured(&config, "fast"),
            "partial pricing (input only) still counts as configured"
        );
        assert!(!model_pricing_configured(&config, "bare"));
        assert!(
            !model_pricing_configured(&config, "nonexistent"),
            "unknown alias is not configured"
        );
    }

    // ── Ctrl-C cancellation persistence contract (Phase 4) ──────────────

    use openslate_store_sqlite::store::SqliteStore;

    #[tokio::test]
    async fn cancelled_run_is_persisted_and_resumable() {
        // The Phase 4 cancel path (`recorder.finish("cancelled", …)` +
        // exit 130) must leave a run that `--resume` (and REPL /resume,
        // via get_last_resumable_run) can pick up, partial transcript
        // included.
        let store = SqliteStore::new_in_memory().await.expect("store");
        store.run_migrations().await.expect("migrations");

        let run_id = RunId("cancelled-e2e".into());
        let rec = RunRecorder::begin(
            store.clone(),
            run_id.clone(),
            "root",
            None,
            r#"{"prompt":"go"}"#,
        )
        .await
        .expect("begin run");

        // Partial transcript as the Phase 3 sink would have written it.
        for m in [
            Message {
                role: MessageRole::User,
                content: "go".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
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
            },
            Message {
                role: MessageRole::Tool,
                content: "a.txt".into(),
                tool_call_id: Some(ToolCallId("tc-1".into())),
                name: Some("shell".into()),
                tool_calls: None,
            },
        ] {
            rec.write_message(&m).await.expect("seed message");
        }
        // The cancel path's terminal update.
        rec.finish("cancelled", Some(r#"{"output":null}"#), 0.0)
            .await
            .expect("finish cancelled");

        // Status assertion: the row landed as `cancelled` with a finish ts.
        let run = store
            .get_run("cancelled-e2e")
            .await
            .expect("get")
            .expect("row");
        assert_eq!(run.status, "cancelled");
        assert!(run.finished_at.is_some(), "cancelled run is finished");

        // Resumable discovery includes it (failed runs only are excluded).
        let resumable = store
            .get_last_resumable_run()
            .await
            .expect("query")
            .expect("cancelled run is resumable");
        assert_eq!(resumable.id, "cancelled-e2e");

        // Resume: adoption continues the seq sequence, transcript intact.
        let rec2 = RunRecorder::resume(store.clone(), run_id, "root")
            .await
            .expect("resume cancelled run");
        let loaded = RunRecorder::load_messages(&store, "cancelled-e2e")
            .await
            .expect("load");
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[2].role, MessageRole::Tool);
        rec2.write_message(&Message {
            role: MessageRole::User,
            content: "continue".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        })
        .await
        .expect("write after resume");
        assert_eq!(store.max_message_seq("cancelled-e2e").await.unwrap(), 4);
    }
}
