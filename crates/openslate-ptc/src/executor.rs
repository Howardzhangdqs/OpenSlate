//! Sandboxed execution of PTC scripts via rquickjs (quickjs-ng).
//!
//! Execution model (see `PTC_PLAN.md` §6 and the P0 spike in
//! `/tmp/opencode/ptc-spike`):
//!
//! 1. [`run_code`] grabs the ambient tokio [`Handle`] (the tool bridge needs
//!    it to call back into the host async world) and moves the whole
//!    execution onto a `spawn_blocking` thread — the QuickJS engine must use
//!    the **sync** `Runtime`; nesting `block_on` inside an async runtime
//!    context would panic.
//! 2. A fresh `Runtime` + `Context` is created per call and dropped
//!    afterwards (one isolate per run, no state leaks between runs).
//! 3. Bound tools are exposed per [`crate::PtcBoundTool::namespace`]:
//!    namespace-less tools sit flat at `tools.<name>`; namespaced tools at
//!    `tools.<ns>.<method>` (a Proxy-backed namespace object), while every
//!    wrapper dispatches through the bridge under the **full registry
//!    name**. Accessing an unlisted tool routes through the bridge too, so
//!    the host-side allow-list produces the canonical "tool not available
//!    in code mode" error.
//! 4. The trigger expression evaluates `(CODE)()` inside an async IIFE that
//!    stores the outcome on `globalThis.__result/__err`; the returned
//!    promise is *not* awaited — the pending-job queue drives it to
//!    settlement. This sidesteps the rquickjs lifetime restriction on
//!    promise handles escaping `Context::with` closures.
//! 5. Resource limits: wall-clock timeout via interrupt handler
//!    (`Arc<AtomicBool>` + guard thread), heap cap via
//!    `Runtime::set_memory_limit`, a per-run tool-call budget counted in
//!    the host bridge, and a separate lookup budget for the discovery
//!    helpers (errors-as-data: budget/availability failures return values,
//!    never throw).
//! 6. Discovery helpers `list_tools(pattern)` / `describe_tool(name)` are
//!    host functions over the passed catalog: they bypass the bridge, do
//!    not count against the tool-call budget, and have their own
//!    `max_lookup_calls` limit. They are reachable both as top-level
//!    functions and as `tools.list_tools` / `tools.describe_tool` members
//!    (models naturally write the latter; both spellings share one budget).
//!
//! Host errors never cross the FFI as panics: the bridge call is wrapped in
//! `catch_unwind` and converted to an error envelope.

use std::collections::{BTreeMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use rquickjs::{CatchResultExt, CaughtError, Context, Function, Runtime, Value};

use crate::ts_types::{sanitize_ident, tool_path};
use crate::{
    describe, envelope_error, normalize, PtcBoundTool, PtcLimits, PtcOutcome, PtcToolInfo,
    ToolBridge,
};

/// Static start of the sandbox prelude: console capture into `__logs`
/// (warn/error get prefixes) plus the wrapper factories. `__mkTool(display,
/// registry)` builds the `(input) => …` callable that talks to
/// `__call_tool_raw` and turns error envelopes into JS exceptions (the
/// errors-as-data protocol); `__mkNs(entries)` builds a namespace object
/// whose unknown members fall through to the bridge (so unlisted tools get
/// the canonical "not available" envelope instead of a bare `undefined`).
const PRELUDE_HEAD: &str = r#"
globalThis.__logs = [];
globalThis.console = {
  log:   (...a) => __logs.push(a.map(x => String(x)).join(" ")),
  warn:  (...a) => __logs.push("[warn] " + a.map(x => String(x)).join(" ")),
  error: (...a) => __logs.push("[error] " + a.map(x => String(x)).join(" ")),
};
globalThis.__mkTool = (display, registry) => (input) => {
  const out = JSON.parse(__call_tool_raw(registry, JSON.stringify(input ?? {})));
  if (out && out.error !== undefined) {
    throw new Error("tool " + display + " failed: " + out.error);
  }
  return out.result;
};
globalThis.__mkNs = (entries, nsDisplay) => new Proxy({}, {
  get: (_, name) => {
    const k = String(name);
    if (Object.prototype.hasOwnProperty.call(entries, k)) return entries[k];
    return globalThis.__mkTool(nsDisplay + "." + k, k);
  },
});
"#;

/// Static end of the prelude: the `tools` proxy over the bound entries and
/// the top-level discovery-helper aliases (`list_tools` / `describe_tool`).
const PRELUDE_TAIL: &str = r#"
globalThis.tools = new Proxy({}, {
  get: (_, name) => {
    const k = String(name);
    if (Object.prototype.hasOwnProperty.call(globalThis.__tools, k)) return globalThis.__tools[k];
    return globalThis.__mkTool(k, k);
  },
});
// Discovery helpers, callable both as top-level functions and as members of
// `tools` (models naturally write `tools.list_tools(...)`); they do NOT go
// through the tool bridge and do not consume the tool-call budget:
//   list_tools(pattern)   -> catalog lines, e.g. "github.list_prs: List pull requests"
//   describe_tool(name)   -> full TS declaration + example call; accepts the
//                            flat registry name ("github_list_prs") or the
//                            dotted path ("github.list_prs")
// The `tools.*` aliases are registered in the dynamic `__tools` literal
// (see build_tools_prelude), so they count as listed names.
globalThis.list_tools = __list_tools;
globalThis.describe_tool = __describe_tool;
"#;

/// Execute `code` (an async arrow function, or statements to be wrapped by
/// [`normalize::prepare`]) in a fresh sandbox with the given bound `tools`,
/// the `catalog` powering the discovery helpers, `limits`, and the host
/// `bridge`.
///
/// Errors are data ([`PtcOutcome::error`]), never `Err` — the caller turns
/// them into tool results so the model can self-heal.
pub async fn run_code(
    code: &str,
    tools: &[PtcBoundTool],
    catalog: &[PtcToolInfo],
    limits: &PtcLimits,
    bridge: ToolBridge,
) -> PtcOutcome {
    // The tool bridge must be able to re-enter the host async runtime from
    // the blocking thread; without an ambient handle there is nothing to
    // re-enter.
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return PtcOutcome {
            error: Some("no tokio runtime".to_string()),
            ..Default::default()
        };
    };

    let code = code.to_string();
    let tools: Vec<PtcBoundTool> = tools.to_vec();
    let catalog: Vec<PtcToolInfo> = catalog.to_vec();
    let limits = limits.clone();
    let bridge = bridge.clone();
    match tokio::task::spawn_blocking(move || {
        execute_blocking(&code, &tools, &catalog, &limits, bridge, handle)
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(join_err) => PtcOutcome {
            error: Some(format!("executor task failed: {join_err}")),
            ..Default::default()
        },
    }
}

/// Result of installing the prelude and triggering the (possibly retried)
/// script evaluation inside a `Context::with` closure.
enum TriggerResult {
    /// The trigger promise is pending; drive the job queue.
    Triggered,
    /// Interrupt handler fired during evaluation.
    TimedOut,
    /// Fatal setup or evaluation error (message included).
    Failed(String),
}

/// Synchronous executor body running on the `spawn_blocking` thread.
fn execute_blocking(
    code: &str,
    tools: &[PtcBoundTool],
    catalog: &[PtcToolInfo],
    limits: &PtcLimits,
    bridge: ToolBridge,
    handle: tokio::runtime::Handle,
) -> PtcOutcome {
    let allowed: HashSet<String> = tools.iter().map(|t| t.name.clone()).collect();
    let catalog = Arc::new(catalog.to_vec());
    let tools_prelude = build_tools_prelude(tools);
    let interrupt = Arc::new(AtomicBool::new(false));
    let tool_call_count = Arc::new(AtomicUsize::new(0));
    let lookup_count = Arc::new(AtomicUsize::new(0));

    // Timeout guard thread: sleeps for `timeout_ms`, then raises the
    // interrupt flag. A `done` signal lets us join the thread immediately
    // after execution instead of waiting out the full timeout.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let guard_flag = interrupt.clone();
    let timeout = limits.timeout_ms;
    let guard = std::thread::spawn(move || {
        if matches!(
            done_rx.recv_timeout(Duration::from_millis(timeout)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            guard_flag.store(true, Ordering::Relaxed);
        }
    });

    let outcome = sandboxed_run(
        code,
        &Setup {
            allowed: &allowed,
            catalog: &catalog,
            tools_prelude: &tools_prelude,
            limits,
            bridge: &bridge,
            handle: &handle,
            interrupt: &interrupt,
            tool_call_count: &tool_call_count,
            lookup_count: &lookup_count,
        },
    );

    // Reap the guard thread (it either received `done` or already fired).
    let _ = done_tx.send(());
    let _ = guard.join();
    outcome
}

/// Shared per-execution state handed to the sandbox setup phase.
struct Setup<'a> {
    allowed: &'a HashSet<String>,
    catalog: &'a Arc<Vec<PtcToolInfo>>,
    tools_prelude: &'a str,
    limits: &'a PtcLimits,
    bridge: &'a ToolBridge,
    handle: &'a tokio::runtime::Handle,
    interrupt: &'a Arc<AtomicBool>,
    tool_call_count: &'a Arc<AtomicUsize>,
    lookup_count: &'a Arc<AtomicUsize>,
}

/// Build the dynamic middle of the prelude: the `__tools` object literal
/// binding every tool to its wrapper. Namespace-less tools sit at the root;
/// namespaced tools nest under `__mkNs(...)` groups keyed by the sanitized
/// namespace. Display names use the composed access path
/// (`github.list_prs`); dispatch names are the full registry names
/// (`github_list_prs`). The two discovery helpers are registered last as
/// plain members, so `tools.list_tools` / `tools.describe_tool` resolve to
/// them instead of the "tool not available" fallback (and win over any
/// real tool unlucky enough to share those names).
fn build_tools_prelude(tools: &[PtcBoundTool]) -> String {
    let mut roots: Vec<&PtcBoundTool> = Vec::new();
    let mut groups: BTreeMap<String, (String, Vec<&PtcBoundTool>)> = BTreeMap::new();
    for tool in tools {
        match &tool.namespace {
            None => roots.push(tool),
            Some(ns) => groups
                .entry(sanitize_ident(ns))
                .or_insert_with(|| (ns.clone(), Vec::new()))
                .1
                .push(tool),
        }
    }

    let mut out = String::from("globalThis.__tools = {");
    for tool in roots {
        let key = sanitize_ident(&tool.name);
        out.push_str(&format!(
            "\n  {}: __mkTool({}, {}),",
            js_str(&key),
            js_str(&key),
            js_str(&tool.name)
        ));
    }
    for (ns_key, (raw_ns, members)) in &groups {
        out.push_str(&format!("\n  {}: __mkNs({{", js_str(ns_key)));
        for tool in members {
            let method = sanitize_ident(
                tool.name
                    .strip_prefix(&format!("{raw_ns}_"))
                    .unwrap_or(tool.name.as_str()),
            );
            let display = format!("{ns_key}.{method}");
            out.push_str(&format!(
                "\n    {}: __mkTool({}, {}),",
                js_str(&method),
                js_str(&display),
                js_str(&tool.name)
            ));
        }
        out.push_str(&format!("\n  }}, {}),", js_str(ns_key)));
    }
    // Discovery helper aliases on `tools` (same host functions as the
    // top-level globals; registered last so they take precedence).
    out.push_str("\n  \"list_tools\": __list_tools,");
    out.push_str("\n  \"describe_tool\": __describe_tool,");
    out.push_str("\n};\n");
    out
}

/// Embed a Rust string as a JSON (and thus valid JS) string literal.
fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"?\"".to_string())
}

/// Create the isolate, run the script, drive pending jobs, collect results.
/// One `Runtime` per call, discarded on return.
fn sandboxed_run(code: &str, setup: &Setup<'_>) -> PtcOutcome {
    let tool_call_count = setup.tool_call_count;
    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            return error_outcome(format!("failed to create JS runtime: {e}"), tool_call_count)
        }
    };
    rt.set_memory_limit(setup.limits.memory_limit_bytes);
    let interrupt_flag: Arc<AtomicBool> = Arc::clone(setup.interrupt);
    rt.set_interrupt_handler(Some(Box::new(move || {
        interrupt_flag.load(Ordering::Relaxed)
    })));
    let ctx = match Context::full(&rt) {
        Ok(ctx) => ctx,
        Err(e) => {
            return error_outcome(format!("failed to create JS context: {e}"), tool_call_count)
        }
    };

    // Install prelude + host bridge and evaluate the trigger expression.
    let trigger = ctx.with(|c| install_and_trigger(c, code, setup));

    // Drive the QuickJS job queue until all promises settle (or a limit hits).
    let interrupt = setup.interrupt;
    let mut timed_out = false;
    let mut failure: Option<String> = None;
    match trigger {
        TriggerResult::Triggered => loop {
            if interrupt.load(Ordering::Relaxed) {
                timed_out = true;
                break;
            }
            // Ok(true): a job ran; Ok(false): queue empty, all settled.
            match rt.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => break,
                Err(job_exception) => {
                    if interrupt.load(Ordering::Relaxed) {
                        timed_out = true;
                    } else {
                        // JobException is not publicly nameable in rquickjs;
                        // its exception is still pending on the job context.
                        failure = Some(job_exception.0.with(|c| {
                            let value = c.catch();
                            match value.as_object() {
                                Some(obj) => rquickjs::Exception::from_object(obj.clone())
                                    .and_then(|ex| ex.message())
                                    .unwrap_or_else(|| "job raised an exception".into()),
                                None => format!("{value:?}"),
                            }
                        }));
                    }
                    break;
                }
            }
        },
        TriggerResult::TimedOut => timed_out = true,
        TriggerResult::Failed(msg) => failure = Some(msg),
    }

    // Collect __result / __err / __logs from the isolate.
    let (result, script_err, logs_json) = ctx.with(|c| {
        let result: Option<String> = c.globals().get("__result").unwrap_or(None);
        let err: Option<String> = c.globals().get("__err").unwrap_or(None);
        let logs_json: String = c
            .eval("JSON.stringify(__logs)")
            .unwrap_or_else(|_| "[]".into());
        (result, err, logs_json)
    });
    let logs: Vec<String> = serde_json::from_str(&logs_json).unwrap_or_default();

    let mut outcome = PtcOutcome {
        tool_calls: tool_call_count.load(Ordering::Relaxed),
        logs,
        ..Default::default()
    };
    if timed_out {
        outcome.timed_out = true;
        outcome.error = Some(format!(
            "execution timed out after {} ms",
            setup.limits.timeout_ms
        ));
    } else if let Some(failure) = failure {
        outcome.error = Some(failure);
    } else if let Some(script_err) = script_err {
        outcome.error = Some(script_err);
    } else {
        match result {
            Some(r) => {
                outcome.result = Some(truncate_bytes(&r, setup.limits.max_output_bytes));
            }
            None => {
                // `JSON.stringify(undefined)` leaves __result undefined: the
                // script ran but produced no value. Report it as an actionable
                // error so models can self-heal — a bare "[result] null"
                // success (the old behavior) made weak models conclude the
                // sandbox itself was broken.
                outcome.error = Some(
                    "script returned no value (undefined) — the async arrow function \
                     must RETURN its result, e.g. async () => { const r = await \
                     tools.read_file({ path: \"x\" }); return r; }"
                        .to_string(),
                );
            }
        }
    }
    if !outcome.logs.is_empty() {
        let joined = outcome.logs.join("\n");
        outcome.logs = truncate_bytes(&joined, setup.limits.max_output_bytes)
            .split('\n')
            .map(str::to_string)
            .collect();
    }
    outcome
}

/// Install the host bridge function, the discovery helpers and the prelude,
/// then evaluate the trigger expression for each normalization candidate.
/// Only syntax/compile errors retry with the next candidate; runtime errors
/// surface immediately.
fn install_and_trigger<'js>(c: rquickjs::Ctx<'js>, code: &str, setup: &Setup<'_>) -> TriggerResult {
    let Setup {
        allowed,
        catalog,
        tools_prelude,
        limits,
        bridge,
        handle,
        interrupt,
        tool_call_count,
        lookup_count,
    } = setup;
    let host_fn = {
        // Fully-qualified clones: `.clone()` on a `&T` would clone the
        // reference and borrow from `setup`, which cannot escape the
        // `Context::with` closure (lifetime `'js`).
        let allowed: HashSet<String> = HashSet::clone(allowed);
        let bridge: ToolBridge = Arc::clone(bridge);
        let handle: tokio::runtime::Handle = tokio::runtime::Handle::clone(handle);
        let counter: Arc<AtomicUsize> = Arc::clone(tool_call_count);
        let budget = limits.max_tool_calls_per_run;
        match Function::new(c.clone(), move |name: String, args: String| -> String {
            let attempt = counter.fetch_add(1, Ordering::Relaxed) + 1;
            if attempt > budget {
                return envelope_error(format!("tool call budget exceeded ({attempt}/{budget})"));
            }
            if !allowed.contains(&name) {
                return envelope_error(format!("tool not available in code mode: {name}"));
            }
            let future = (bridge)(&name, &args);
            match catch_unwind(AssertUnwindSafe(|| handle.block_on(future))) {
                Ok(envelope) => envelope,
                Err(_) => envelope_error("bridge panicked"),
            }
        }) {
            Ok(f) => f,
            Err(e) => return TriggerResult::Failed(format!("failed to bind host function: {e}")),
        }
    };
    if let Err(e) = c.globals().set("__call_tool_raw", host_fn) {
        return TriggerResult::Failed(format!("failed to install host function: {e}"));
    }

    // Discovery helpers: host functions over the catalog; they bypass the
    // bridge entirely and carry their own lookup budget.
    let lookup_fn = {
        let catalog: Arc<Vec<PtcToolInfo>> = Arc::clone(catalog);
        let lookups: Arc<AtomicUsize> = Arc::clone(lookup_count);
        let budget = limits.max_lookup_calls;
        match Function::new(c.clone(), move |pattern: String| -> String {
            let attempt = lookups.fetch_add(1, Ordering::Relaxed) + 1;
            if attempt > budget {
                return format!("lookup budget exceeded ({attempt}/{budget})");
            }
            describe::list(&catalog, &pattern)
        }) {
            Ok(f) => f,
            Err(e) => return TriggerResult::Failed(format!("failed to bind list_tools: {e}")),
        }
    };
    if let Err(e) = c.globals().set("__list_tools", lookup_fn) {
        return TriggerResult::Failed(format!("failed to install list_tools: {e}"));
    }
    let describe_fn = {
        let catalog: Arc<Vec<PtcToolInfo>> = Arc::clone(catalog);
        let lookups: Arc<AtomicUsize> = Arc::clone(lookup_count);
        let budget = limits.max_lookup_calls;
        match Function::new(c.clone(), move |name: String| -> String {
            let attempt = lookups.fetch_add(1, Ordering::Relaxed) + 1;
            if attempt > budget {
                return format!("lookup budget exceeded ({attempt}/{budget})");
            }
            match catalog
                .iter()
                .find(|t| t.name == name || tool_path(t) == name)
            {
                Some(info) => describe::describe(info),
                None => format!("unknown tool: {name}"),
            }
        }) {
            Ok(f) => f,
            Err(e) => return TriggerResult::Failed(format!("failed to bind describe_tool: {e}")),
        }
    };
    if let Err(e) = c.globals().set("__describe_tool", describe_fn) {
        return TriggerResult::Failed(format!("failed to install describe_tool: {e}"));
    }

    let prelude = format!("{PRELUDE_HEAD}{tools_prelude}{PRELUDE_TAIL}");
    if let Err(caught) = c.eval::<(), _>(prelude.as_str()).catch(&c) {
        if interrupt.load(Ordering::Relaxed) {
            return TriggerResult::TimedOut;
        }
        return TriggerResult::Failed(format!("prelude failed: {}", caught_message(caught)));
    }

    let mut last_syntax_error: Option<String> = None;
    for candidate in normalize::prepare(code) {
        let trigger_expr = format!(
            "(async () => {{ globalThis.__result = null; globalThis.__err = null; \
             try {{ globalThis.__result = JSON.stringify(await ({candidate})()); }} \
             catch (e) {{ globalThis.__err = String(e && e.message || e); }} }})()"
        );
        match c.eval::<Value, _>(trigger_expr.as_str()).catch(&c) {
            Ok(_) => return TriggerResult::Triggered,
            Err(CaughtError::Exception(ex)) if is_syntax_error(&c, &ex) => {
                last_syntax_error = Some(ex.message().unwrap_or_else(|| "syntax error".into()));
            }
            Err(caught) => {
                // Runtime error during the synchronous segment, or the
                // uncatchable interrupt exception.
                if interrupt.load(Ordering::Relaxed) {
                    return TriggerResult::TimedOut;
                }
                return TriggerResult::Failed(caught_message(caught));
            }
        }
    }
    TriggerResult::Failed(format!(
        "syntax error: {}",
        last_syntax_error.unwrap_or_else(|| "invalid code".into())
    ))
}

/// Whether the caught exception is a `SyntaxError` instance (i.e. a
/// compile-time error worth retrying with the next normalization candidate).
fn is_syntax_error<'js>(c: &rquickjs::Ctx<'js>, ex: &rquickjs::Exception<'js>) -> bool {
    c.globals()
        .get::<_, Value>("SyntaxError")
        .map(|ctor| ex.as_object().is_instance_of(ctor))
        .unwrap_or(false)
}

/// Human-readable message for a caught eval exception.
fn caught_message(caught: CaughtError<'_>) -> String {
    match caught {
        CaughtError::Exception(ex) => ex.message().unwrap_or_else(|| "exception".into()),
        CaughtError::Value(v) => format!("{v:?}"),
        CaughtError::Error(e) => e.to_string(),
    }
}

/// Build an error-only outcome (used for setup failures).
fn error_outcome(message: String, tool_call_count: &Arc<AtomicUsize>) -> PtcOutcome {
    PtcOutcome {
        error: Some(message),
        tool_calls: tool_call_count.load(Ordering::Relaxed),
        ..Default::default()
    }
}

/// Cut `s` to at most `max` bytes (on a UTF-8 char boundary), appending a
/// truncation marker noting the original size when anything was cut.
fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n--- TRUNCATED (original {} bytes) ---",
        &s[..cut],
        s.len()
    )
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serde_json::json;

    use super::*;
    use crate::envelope_result;

    /// Mock bridge mirroring the spike's `tool_dispatch`: add/echo/fail plus
    /// namespaced echo helpers that record the dispatch name they received.
    fn mock_bridge() -> ToolBridge {
        Arc::new(|name: &str, args: &str| {
            let value: serde_json::Value =
                serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
            let out = match name {
                "add" => {
                    let a = value.get("a").and_then(|x| x.as_i64()).unwrap_or(0);
                    let b = value.get("b").and_then(|x| x.as_i64()).unwrap_or(0);
                    envelope_result(json!(a + b))
                }
                "echo" => envelope_result(json!(value)),
                "fail" => crate::envelope_error("simulated tool failure"),
                "github_list_prs" => envelope_result(json!({
                    "dispatched_as": name,
                    "state": value.get("state"),
                })),
                "read_file" => envelope_result(json!("content")),
                _ => crate::envelope_error(format!("unknown tool: {name}")),
            };
            Box::pin(async move { out })
                as std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
        })
    }

    fn bound(names: &[&str]) -> Vec<PtcBoundTool> {
        names
            .iter()
            .map(|n| PtcBoundTool {
                name: (*n).to_string(),
                namespace: None,
            })
            .collect()
    }

    fn catalog() -> Vec<PtcToolInfo> {
        vec![
            PtcToolInfo {
                name: "read_file".to_string(),
                namespace: None,
                description: "Read a file from the workspace. Second sentence.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            },
            PtcToolInfo {
                name: "github_list_prs".to_string(),
                namespace: Some("github".to_string()),
                description: "List pull requests".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "state": { "type": "string", "enum": ["open", "closed", "all"] },
                        "limit": { "type": "integer" }
                    }
                }),
            },
        ]
    }

    #[tokio::test]
    async fn valueless_script_reports_actionable_error() {
        // Script-style code with no return used to surface as a bare
        // "[result] null" success; it must become a self-heal hint instead.
        let outcome = run_code(
            "async () => { const sum = await tools.add({ a: 1, b: 2 }); }",
            &bound(&["add"]),
            &[],
            &PtcLimits::default(),
            mock_bridge(),
        )
        .await;
        assert_eq!(outcome.result, None);
        let err = outcome.error.expect("valueless script must error");
        assert!(err.contains("returned no value"));
        assert_eq!(outcome.tool_calls, 1);
    }

    #[tokio::test]
    async fn orchestrates_tools_and_captures_logs() {
        let code = r#"async () => {
            console.log("starting");
            const sum = await tools.add({ a: 1, b: 2 });
            const both = await Promise.all([
                tools.add({ a: 10, b: 20 }),
                tools.echo({ x: "hi" }),
            ]);
            return { sum, both };
        }"#;
        let outcome = run_code(
            code,
            &bound(&["add", "echo"]),
            &[],
            &PtcLimits::default(),
            mock_bridge(),
        )
        .await;
        assert_eq!(outcome.error, None, "error: {outcome:?}");
        assert!(!outcome.timed_out);
        assert_eq!(outcome.tool_calls, 3);
        assert_eq!(outcome.logs, vec!["starting".to_string()]);
        let result: serde_json::Value = serde_json::from_str(&outcome.result.unwrap()).unwrap();
        assert_eq!(result["sum"], 3);
        assert_eq!(result["both"][0], 30);
        assert_eq!(result["both"][1]["x"], "hi");
    }

    #[tokio::test]
    async fn namespaced_tool_binds_composed_path() {
        // A Some(ns) tool is exposed as tools.<ns>.<method> but dispatched
        // through the bridge under its full registry name.
        let tools = vec![
            PtcBoundTool {
                name: "github_list_prs".to_string(),
                namespace: Some("github".to_string()),
            },
            PtcBoundTool {
                name: "read_file".to_string(),
                namespace: None,
            },
        ];
        let code = r#"async () => {
            const pr = await tools.github.list_prs({ state: "open" });
            const f = await tools.read_file({ path: "x" });
            return { pr, f };
        }"#;
        let outcome = run_code(code, &tools, &[], &PtcLimits::default(), mock_bridge()).await;
        assert_eq!(outcome.error, None, "outcome: {outcome:?}");
        assert_eq!(outcome.tool_calls, 2);
        let result: serde_json::Value = serde_json::from_str(&outcome.result.unwrap()).unwrap();
        assert_eq!(result["pr"]["dispatched_as"], "github_list_prs");
        assert_eq!(result["pr"]["state"], "open");
        assert_eq!(result["f"], "content");
    }

    #[tokio::test]
    async fn namespaced_unknown_member_keeps_not_available_semantics() {
        let tools = vec![PtcBoundTool {
            name: "github_list_prs".to_string(),
            namespace: Some("github".to_string()),
        }];
        let code = r#"async () => {
            let caught = null;
            try { await tools.github.nope({}); } catch (e) { caught = String(e.message); }
            return caught;
        }"#;
        let outcome = run_code(code, &tools, &[], &PtcLimits::default(), mock_bridge()).await;
        assert_eq!(outcome.error, None);
        let result: serde_json::Value = serde_json::from_str(&outcome.result.unwrap()).unwrap();
        assert_eq!(
            result.as_str().unwrap(),
            "tool github.nope failed: tool not available in code mode: nope"
        );
        assert_eq!(outcome.tool_calls, 1);
    }

    #[tokio::test]
    async fn discovery_helpers_do_not_use_the_bridge() {
        let code = r#"async () => {
            const listed = await list_tools("github.*");
            const d1 = await describe_tool("github_list_prs");
            const d2 = await describe_tool("github.list_prs");
            const d3 = await describe_tool("nope");
            const all = await list_tools("*");
            const sum = await tools.add({ a: 1, b: 2 });
            return { listed, d1, d2, d3, all, sum };
        }"#;
        let outcome = run_code(
            code,
            &bound(&["add"]),
            &catalog(),
            &PtcLimits::default(),
            mock_bridge(),
        )
        .await;
        assert_eq!(outcome.error, None, "outcome: {outcome:?}");
        let result: serde_json::Value = serde_json::from_str(&outcome.result.unwrap()).unwrap();
        // list_tools: only matching entries, first-sentence descriptions:
        assert_eq!(
            result["listed"].as_str().unwrap(),
            "github.list_prs: List pull requests"
        );
        assert_eq!(
            result["all"].as_str().unwrap(),
            "github.list_prs: List pull requests\nread_file: Read a file from the workspace"
        );
        // describe_tool accepts both the flat name and the dotted path and
        // renders the same self-contained block:
        let d1 = result["d1"].as_str().unwrap();
        let d2 = result["d2"].as_str().unwrap();
        assert_eq!(d1, d2);
        assert!(d1.contains("declare const tools: {"), "d1: {d1}");
        assert!(d1.contains("list_prs: (input: {"), "d1: {d1}");
        assert!(
            d1.contains("// Example: await tools.github.list_prs({ limit: 0, state: \"open\" })"),
            "d1: {d1}"
        );
        // Unknown lookups are data, not exceptions:
        assert_eq!(result["d3"].as_str().unwrap(), "unknown tool: nope");
        // Discovery calls bypass the bridge entirely: only add counted.
        assert_eq!(outcome.tool_calls, 1);
        assert_eq!(result["sum"], 3);
    }

    #[tokio::test]
    async fn lookup_budget_enforced() {
        let limits = PtcLimits {
            max_lookup_calls: 1,
            ..PtcLimits::default()
        };
        let code = r#"async () => {
            const first = await list_tools("*");
            const second = await list_tools("*");
            const third = await describe_tool("read_file");
            return { first, second, third };
        }"#;
        let outcome = run_code(code, &[], &catalog(), &limits, mock_bridge()).await;
        assert_eq!(outcome.error, None, "outcome: {outcome:?}");
        let result: serde_json::Value = serde_json::from_str(&outcome.result.unwrap()).unwrap();
        assert!(result["first"].as_str().unwrap().contains("read_file"));
        assert_eq!(
            result["second"].as_str().unwrap(),
            "lookup budget exceeded (2/1)"
        );
        assert_eq!(
            result["third"].as_str().unwrap(),
            "lookup budget exceeded (3/1)"
        );
        // Lookups never touch the tool-call budget:
        assert_eq!(outcome.tool_calls, 0);
    }

    #[tokio::test]
    async fn helpers_reachable_as_tools_members_with_shared_budget() {
        // Models naturally write `tools.list_tools(...)`; both spellings
        // must work and count against the same lookup budget.
        let limits = PtcLimits {
            max_lookup_calls: 2,
            ..PtcLimits::default()
        };
        let code = r#"async () => {
            const a = await tools.list_tools("github.*");
            const b = await list_tools("*");
            const c = await tools.describe_tool("github_list_prs");
            return { a, b, c };
        }"#;
        let outcome = run_code(code, &[], &catalog(), &limits, mock_bridge()).await;
        assert_eq!(outcome.error, None, "outcome: {outcome:?}");
        let result: serde_json::Value = serde_json::from_str(&outcome.result.unwrap()).unwrap();
        // tools.list_tools is the same host function as list_tools:
        assert_eq!(
            result["a"].as_str().unwrap(),
            "github.list_prs: List pull requests"
        );
        assert!(result["b"].as_str().unwrap().contains("read_file"));
        // Both spellings share one counter: the third lookup is over budget.
        assert_eq!(
            result["c"].as_str().unwrap(),
            "lookup budget exceeded (3/2)"
        );
        assert_eq!(outcome.tool_calls, 0);
    }

    #[tokio::test]
    async fn tool_errors_are_data_throwable_in_code() {
        let code = r#"async () => {
            let caught = null;
            try { await tools.fail({}); } catch (e) { caught = String(e.message); }
            return { caught };
        }"#;
        let outcome = run_code(
            code,
            &bound(&["fail"]),
            &[],
            &PtcLimits::default(),
            mock_bridge(),
        )
        .await;
        assert_eq!(outcome.error, None);
        let result: serde_json::Value = serde_json::from_str(&outcome.result.unwrap()).unwrap();
        assert!(
            result["caught"]
                .as_str()
                .unwrap_or("")
                .contains("simulated tool failure"),
            "result: {result}"
        );
    }

    #[tokio::test]
    async fn tool_call_budget_enforced() {
        let code = r#"async () => {
            await tools.add({ a: 1, b: 1 });
            await tools.add({ a: 2, b: 2 });
            let third = null;
            try { third = await tools.add({ a: 3, b: 3 }); } catch (e) { third = String(e.message); }
            return third;
        }"#;
        let limits = PtcLimits {
            max_tool_calls_per_run: 2,
            ..PtcLimits::default()
        };
        let outcome = run_code(code, &bound(&["add"]), &[], &limits, mock_bridge()).await;
        assert_eq!(outcome.error, None);
        let result: serde_json::Value = serde_json::from_str(&outcome.result.unwrap()).unwrap();
        assert_eq!(
            result.as_str().unwrap(),
            "tool add failed: tool call budget exceeded (3/2)"
        );
        assert_eq!(outcome.tool_calls, 3);
    }

    #[tokio::test]
    async fn hard_timeout_interrupts_infinite_loop() {
        let start = Instant::now();
        let limits = PtcLimits {
            timeout_ms: 150,
            ..PtcLimits::default()
        };
        let outcome = run_code(
            "async () => { while (true) {} }",
            &[],
            &[],
            &limits,
            mock_bridge(),
        )
        .await;
        assert!(
            start.elapsed().as_millis() < 2000,
            "took too long: {:?}",
            start.elapsed()
        );
        assert!(outcome.timed_out, "outcome: {outcome:?}");
        assert_eq!(
            outcome.error.as_deref(),
            Some("execution timed out after 150 ms")
        );
    }

    #[tokio::test]
    async fn hard_timeout_interrupts_loop_after_await() {
        // Infinite loop in a pending job (after the first await) must also
        // be interrupted via execute_pending_job + interrupt flag.
        let limits = PtcLimits {
            timeout_ms: 150,
            ..PtcLimits::default()
        };
        let code = r#"async () => {
            await tools.echo({});
            while (true) {}
        }"#;
        let start = Instant::now();
        let outcome = run_code(code, &bound(&["echo"]), &[], &limits, mock_bridge()).await;
        assert!(start.elapsed().as_millis() < 2000);
        assert!(outcome.timed_out, "outcome: {outcome:?}");
    }

    #[tokio::test]
    async fn memory_limit_rejects_huge_allocation() {
        let limits = PtcLimits {
            memory_limit_bytes: 4 * 1024 * 1024,
            ..PtcLimits::default()
        };
        let outcome = run_code(
            r#"async () => "x".repeat(50_000_000)"#,
            &[],
            &[],
            &limits,
            mock_bridge(),
        )
        .await;
        assert!(outcome.result.is_none());
        assert!(
            outcome
                .error
                .as_deref()
                .unwrap()
                .to_lowercase()
                .contains("memory"),
            "outcome: {outcome:?}"
        );
        assert!(!outcome.timed_out);
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_envelope() {
        let code = r#"async () => {
            let caught = null;
            try { await tools.nope({}); } catch (e) { caught = String(e.message); }
            return caught;
        }"#;
        let outcome = run_code(
            code,
            &bound(&["add"]),
            &[],
            &PtcLimits::default(),
            mock_bridge(),
        )
        .await;
        assert_eq!(outcome.error, None);
        let result: serde_json::Value = serde_json::from_str(&outcome.result.unwrap()).unwrap();
        assert_eq!(
            result.as_str().unwrap(),
            "tool nope failed: tool not available in code mode: nope"
        );
        assert_eq!(outcome.tool_calls, 1);
    }

    #[tokio::test]
    async fn result_truncated_over_budget() {
        let limits = PtcLimits {
            max_output_bytes: 20,
            ..PtcLimits::default()
        };
        let outcome = run_code(
            r#"async () => "a".repeat(500)"#,
            &[],
            &[],
            &limits,
            mock_bridge(),
        )
        .await;
        let result = outcome.result.unwrap();
        assert!(
            result.contains("\n--- TRUNCATED (original 502 bytes) ---"),
            "result: {result:?}"
        );
        // The JSON-encoded string is 502 bytes (500 chars + 2 quotes).
        assert!(result.starts_with("\"aaaaaaaaaa"));
    }

    #[tokio::test]
    async fn syntax_error_retries_with_wrapped_candidate() {
        // Bare statements are not a valid expression; normalize's wrap
        // candidate makes them work.
        let outcome = run_code(
            "const a = 40;\nreturn a + 2;",
            &[],
            &[],
            &PtcLimits::default(),
            mock_bridge(),
        )
        .await;
        assert_eq!(outcome.error, None, "outcome: {outcome:?}");
        assert_eq!(outcome.result.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn genuine_syntax_error_reports_parser_message() {
        let outcome = run_code(
            "async () => { this is not javascript }",
            &[],
            &[],
            &PtcLimits::default(),
            mock_bridge(),
        )
        .await;
        let error = outcome.error.unwrap();
        assert!(error.starts_with("syntax error:"), "error: {error}");
        assert!(!outcome.timed_out);
    }

    #[tokio::test]
    async fn script_runtime_error_is_data() {
        let outcome = run_code(
            r#"async () => { throw new Error("boom"); }"#,
            &[],
            &[],
            &PtcLimits::default(),
            mock_bridge(),
        )
        .await;
        assert_eq!(outcome.error.as_deref(), Some("boom"));
        assert!(outcome.result.is_none());
        assert!(!outcome.timed_out);
    }

    #[test]
    fn without_tokio_runtime_returns_error() {
        // futures::executor has no tokio Handle -> must not panic, must
        // return the "no tokio runtime" error outcome.
        let outcome = futures::executor::block_on(run_code(
            "async () => 1",
            &[],
            &[],
            &PtcLimits::default(),
            mock_bridge(),
        ));
        assert_eq!(outcome.error.as_deref(), Some("no tokio runtime"));
    }

    #[tokio::test]
    async fn bridge_panic_becomes_error_envelope() {
        let bridge: ToolBridge =
            Arc::new(|_name: &str, _args: &str| Box::pin(async { panic!("bridge exploded") }));
        let code = r#"async () => {
            let caught = null;
            try { await tools.add({}); } catch (e) { caught = String(e.message); }
            return caught;
        }"#;
        let outcome = run_code(code, &bound(&["add"]), &[], &PtcLimits::default(), bridge).await;
        assert_eq!(outcome.error, None, "outcome: {outcome:?}");
        let result: serde_json::Value = serde_json::from_str(&outcome.result.unwrap()).unwrap();
        assert_eq!(result.as_str().unwrap(), "tool add failed: bridge panicked");
    }

    #[tokio::test]
    async fn logs_truncated_over_budget() {
        let limits = PtcLimits {
            max_output_bytes: 15,
            ..PtcLimits::default()
        };
        let outcome = run_code(
            r#"async () => { console.log("0123456789abcdefghij"); console.log("second line"); return 1; }"#,
            &[],
            &[],
            &limits,
            mock_bridge(),
        )
        .await;
        let joined = outcome.logs.join("\n");
        assert!(
            joined.contains("--- TRUNCATED (original"),
            "logs: {:?}",
            outcome.logs
        );
        assert_eq!(outcome.result.as_deref(), Some("1"));
    }
}
