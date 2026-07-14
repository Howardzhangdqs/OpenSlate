//! `openslate run` command — execute a single agent run.
//!
//! Uses the `AppContext` wiring to assemble all components and execute
//! a complete end-to-end run: config → validation → SQLite store → agent tree →
//! tool registry → provider → RunManager → execute → result output.

use anyhow::{Context, Result};
use std::fs;
use std::time::Instant;

use openslate_core::agent_tree::AgentTree;
use openslate_core::provider::ModelProvider;
use openslate_model_openai::client::{OpenAICompatibleProvider, OpenAIProviderConfig};

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
    content_tokens: u64,
    reasoning_tokens: u64,
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
        let in_seg = input_tokens
            .map(|i| format!("↑{} ", i))
            .unwrap_or_default();
        let out_seg = match (content_tokens, reasoning_tokens) {
            (0, 0) => "↓0".to_string(),
            (c, 0) => format!("↓{}", c),
            (0, r) => format!("↓r{}", r),
            (c, r) => format!("↓{}r{}", c, r),
        };
        let tps_seg = if tps > 0 {
            format!(" · {}tok/s", tps)
        } else {
            String::new()
        };
        tracing::info!(
            target: "openslate_runtime",
            "{:.1}s · {}{}{}",
            llm_elapsed_secs,
            in_seg,
            out_seg,
            tps_seg
        );
    }

    if !quiet {
        let dur_str = if duration_ms >= 1000 {
            format!("{:.1}s", duration_ms as f64 / 1000.0)
        } else {
            format!("{}ms", duration_ms)
        };
        // Short id (git-style) keeps the log compact; the full id stays in the
        // exported trace and the store. Token split matches the spinner: `↓{content}`
        // plus `r{reasoning}` when the model produced reasoning.
        let run_id_str = result.run_id.to_string();
        let short_id = run_id_str.get(..8).unwrap_or(&run_id_str);
        let token_seg = match (content_tokens, reasoning_tokens) {
            (0, 0) => "↓0".to_string(),
            (c, 0) => format!("↓{}", c),
            (0, r) => format!("↓r{}", r),
            (c, r) => format!("↓{}r{}", c, r),
        };
        let tps_seg = if tps > 0 {
            format!(" · {}tok/s", tps)
        } else {
            String::new()
        };
        tracing::info!(
            "Run done · {} · {} step · {} · ↑{} {}{}",
            short_id,
            result.total_steps,
            dur_str,
            result.total_input_tokens,
            token_seg,
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
                let step_event = serde_json::json!({
                    "type": "step",
                    "step": step_count,
                    "role": "assistant",
                    "content": msg.content,
                    "tool_calls": [],  // tool_calls not available in Message
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
    let (result, run_elapsed_ms, content_tokens, reasoning_tokens, tps, input_tokens, llm_elapsed) = {
        // Spinner provides real-time progress via streaming callbacks.
        // In quiet mode, the spinner is hidden.
        // `run` mode always hides the spinner animation line (`⠙ main`) — it's
        // noise for a one-shot command. Streaming reasoning/tool lines still
        // print above; the per-request "Step N" log + final "Run done" carry
        // status. (params.quiet still governs the Run done line + result only.)
        let mut callback = SpinnerCallback::new(&model_alias, true);
        let exec_start = Instant::now();
        let run_result = match ctx
            .manager
            .execute(&*provider, &prompt, Some(&mut callback))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                callback.finish_with_error(&e.to_string());
                return Err(anyhow::anyhow!("Run failed: {}", e));
            }
        };
        // Capture the streamed content/reasoning split before finish() consumes
        // the callback (used for the "Run done" log line).
        let content_tokens = callback.content_tokens();
        let reasoning_tokens = callback.reasoning_tokens();
        let tps = callback.tps();
        let input_tokens = callback.input_tokens();
        let llm_elapsed = callback.elapsed();
        let elapsed = exec_start.elapsed();
        // Skip the per-run `✓ model ...` spinner summary — its tok/s folds into
        // the "Run done" line below, so we avoid a redundant stats row.
        callback.finish_silent();
        (
            run_result,
            elapsed.as_millis() as u64,
            content_tokens,
            reasoning_tokens,
            tps,
            input_tokens,
            llm_elapsed,
        )
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
        run_elapsed_ms,
        content_tokens,
        reasoning_tokens,
        tps,
        input_tokens,
        llm_elapsed.as_secs_f64(),
    )?;

    // 9. Export trace to file if requested
    if let Some(ref trace_path) = params.trace_path {
        let path = std::path::Path::new(trace_path);
        result.trace.export_to_file(path)
            .with_context(|| format!("Failed to export trace to '{}'", trace_path))?;
        if !params.quiet {
            tracing::info!("Trace exported to {}", trace_path);
        }
    }

    // 10. Persist trace events to SQLite (if store available)
    if let Some(ref store) = ctx.store {
        if let Err(e) = persist_trace_to_store(store, &result).await {
            tracing::warn!("Failed to persist trace events to SQLite: {}", e);
        }
    }

    Ok(())
}

/// Build a provider for a specific model alias.
///
/// Dispatches on `ProviderConfig.kind`:
/// - `"openai_compatible"` → the hand-rolled OpenAI Chat Completions client.
/// - `"genai"` → the genai-backed multi-provider adapter (requires the `genai`
///   cargo feature).
pub(crate) fn build_provider_for_model(
    config: &openslate_core::config::OpenSlateConfig,
    model_alias: &str,
) -> Result<Box<dyn ModelProvider>> {
    let resolved =
        openslate_core::model_config::resolve_model(config, model_alias).with_context(|| {
            format!("Failed to resolve model alias '{}'", model_alias)
        })?;

    match resolved.provider.kind.as_str() {
        "openai_compatible" => build_openai_provider(&resolved),
        "genai" => build_genai_provider(&resolved),
        other => anyhow::bail!(
            "unknown provider kind '{}' for provider '{}'; expected 'openai_compatible' or 'genai'",
            other,
            resolved.provider_name
        ),
    }
}

/// Construct the OpenAI-compatible provider (always available, no extra deps).
fn build_openai_provider(
    resolved: &openslate_core::model_config::ResolvedModel,
) -> Result<Box<dyn ModelProvider>> {
    let api_key = std::env::var(&resolved.provider.api_key_env).with_context(|| {
        format!(
            "API key not found: set environment variable '{}'",
            resolved.provider.api_key_env
        )
    })?;

    let provider_config = OpenAIProviderConfig {
        provider_name: resolved.provider_name.clone(),
        base_url: resolved.provider.base_url.clone(),
        api_key,
        timeout_secs: 60,
    };

    Ok(Box::new(OpenAICompatibleProvider::new(provider_config)))
}

/// Construct the genai-backed multi-provider adapter.
#[cfg(feature = "genai")]
fn build_genai_provider(
    resolved: &openslate_core::model_config::ResolvedModel,
) -> Result<Box<dyn ModelProvider>> {
    let api_key = std::env::var(&resolved.provider.api_key_env).with_context(|| {
        format!(
            "API key not found: set environment variable '{}'",
            resolved.provider.api_key_env
        )
    })?;

    let cfg = openslate_model_genai::GenaiConfig {
        provider_name: resolved.provider_name.clone(),
        model: resolved.model_id.clone(),
        api_key: Some(api_key),
        base_url: Some(resolved.provider.base_url.clone()),
        adapter: resolved.provider.adapter.clone(),
        timeout_secs: 60,
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

/// Without the `genai` feature, a `kind = "genai"` config fails with a clear
/// rebuild instruction instead of a confusing link error.
#[cfg(not(feature = "genai"))]
fn build_genai_provider(
    resolved: &openslate_core::model_config::ResolvedModel,
) -> Result<Box<dyn ModelProvider>> {
    anyhow::bail!(
        "provider '{}' uses kind = \"genai\", but this openslate build was compiled without \
         the 'genai' feature. Rebuild with: cargo run --features openslate-cli/genai",
        resolved.provider_name
    );
}

async fn persist_trace_to_store(
    store: &openslate_store_sqlite::store::SqliteStore,
    result: &openslate_core::run_manager::ManagedRunResult,
) -> Result<()> {
    use openslate_core::trace::TraceEvent;

    let run_id_str = result.run_id.to_string();

    // Insert the run row first so that trace_events FK constraint is satisfied.
    let status_str = match result.status {
        openslate_core::types::RunStatus::Running => "running",
        openslate_core::types::RunStatus::Completed => "completed",
        openslate_core::types::RunStatus::Failed => "failed",
        openslate_core::types::RunStatus::Interrupted => "interrupted",
        openslate_core::types::RunStatus::Cancelled => "cancelled",
    };
    let root_agent_id = result.execution_tree.root().agent_id.to_string();

    store.insert_run(
        &run_id_str,
        None,
        &root_agent_id,
        status_str,
        "{}",
        0,
    ).await.map_err(|e| anyhow::anyhow!("Failed to insert run: {}", e))?;

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
                (name.clone(), "duration_begin".to_owned(), (*ts as i64) * 1000, None, "main".to_owned(), args_str)
            }
            TraceEvent::DurationEnd { name, ts, .. } => {
                (name.clone(), "duration_end".to_owned(), (*ts as i64) * 1000, None, "main".to_owned(), None)
            }
            TraceEvent::Complete { name, ts, dur, args, .. } => {
                let args_str = if args.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(args).unwrap_or_default())
                };
                (name.clone(), "complete".to_owned(), (*ts as i64) * 1000, Some((*dur as i64) * 1000), "main".to_owned(), args_str)
            }
            TraceEvent::Instant { name, ts, .. } => {
                (name.clone(), "instant".to_owned(), (*ts as i64) * 1000, None, "main".to_owned(), None)
            }
            TraceEvent::Counter { name, ts, values, .. } => {
                let args_str = serde_json::to_string(values).unwrap_or_default();
                (name.clone(), "counter".to_owned(), (*ts as i64) * 1000, None, "main".to_owned(), Some(args_str))
            }
        };

        store.insert_trace_event(
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
        ).await.map_err(|e| anyhow::anyhow!("Failed to insert trace event: {}", e))?;
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
        fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        let agents_dir = openslate_dir.join("agents");
        fs::create_dir(&agents_dir).expect("create agents dir");
        let agent_md = "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - current_time\n---\nYou are the root agent.\n";
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

    /// A `kind = "genai"` config in a build compiled WITHOUT the `genai` feature
    /// must fail with a clear rebuild instruction (not a confusing link error).
    #[cfg(not(feature = "genai"))]
    #[test]
    fn test_genai_provider_without_feature_errors_clearly() {
        let toml = r#"
[providers.anthropic_prod]
kind = "genai"
base_url = "https://api.anthropic.com"
api_key_env = "GENAI_TEST_KEY"
adapter = "anthropic"

[models.main]
provider = "anthropic_prod"
model = "claude-sonnet-4-5"
"#;
        let config = openslate_core::config::parse_openslate_toml(toml).unwrap();
        match build_provider_for_model(&config, "main") {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("--features") && msg.contains("genai"),
                    "error should tell the user how to enable the feature: {msg}"
                );
            }
            Ok(_) => panic!("expected an error without the genai feature"),
        }
    }

    /// A `kind = "genai"` config in a build compiled WITH the `genai` feature
    /// must construct a `GenaiProvider` successfully.
    #[cfg(feature = "genai")]
    #[test]
    fn test_genai_provider_constructs_with_feature() {
        // Unique env var name to avoid races with parallel tests.
        // SAFETY of env mutation: this var is not read by any other test.
        std::env::set_var("GENAI_TEST_KEY", "sk-test");
        let toml = r#"
[providers.anthropic_prod]
kind = "genai"
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

    /// An unknown `kind` must be rejected with a clear error.
    #[test]
    fn test_unknown_provider_kind_is_rejected() {
        let toml = r#"
[providers.weird]
kind = "something-new"
base_url = "https://example.com"
api_key_env = "SOME_KEY"

[models.main]
provider = "weird"
model = "m1"
"#;
        let config = openslate_core::config::parse_openslate_toml(toml).unwrap();
        match build_provider_for_model(&config, "main") {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("unknown provider kind") && msg.contains("something-new"),
                    "error should name the unknown kind: {msg}"
                );
            }
            Ok(_) => panic!("expected an error for an unknown provider kind"),
        }
    }

    #[test]
    fn test_resolve_agent_root_default() {
        let tmp = temp_project();
        let agents =
            crate::wiring::load_agents(&tmp.path().join(".openslate/agents")).unwrap();
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
        fs::write(dir.join("openslate.toml"), toml).expect("write");
        let agents_dir = dir.join("agents");
        fs::create_dir(&agents_dir).expect("create agents dir");
        let root_md = "---\nid: root\nname: Root\nmodel: main\nchildren:\n  - worker\n---\nRoot.\n";
        let worker_md = "---\nid: worker\nname: Worker\nmodel: fast\n---\nWorker.\n";
        fs::write(agents_dir.join("root.md"), root_md).expect("write root.md");
        fs::write(agents_dir.join("worker.md"), worker_md).expect("write worker.md");

        let agents_cfg =
            crate::wiring::load_agents(&dir.join("agents")).unwrap();
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
            crate::wiring::load_agents(&tmp.path().join(".openslate/agents")).unwrap();
        let tree =
            openslate_core::agent_tree::AgentTree::from_configs(&agents.agents).unwrap();
        let result = resolve_agent(&tree, Some("nonexistent"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

}
