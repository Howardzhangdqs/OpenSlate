//! openslate-ptc — Programmatic Tool Calling (PTC) support for OpenSlate.
//!
//! Lets the model orchestrate tool calls by writing a JavaScript async arrow
//! function executed in a sandboxed QuickJS isolate (one isolate per run,
//! discarded afterwards), instead of emitting individual JSON tool calls.
//!
//! Design doc: `PTC_PLAN.md` (repo root). Key properties:
//! - sandbox = rquickjs (quickjs-ng): no network, hard timeout via interrupt
//!   handler, memory limit, per-run tool-call budget;
//! - host bridge follows the "errors-as-data" protocol: the bridge returns a
//!   JSON envelope `{"result": ...}` or `{"error": "..."}`; the sandbox-side
//!   wrapper turns `error` envelopes into JS exceptions the model's code can
//!   try/catch;
//! - credentials never enter the sandbox (bridge calls stay host-side).
//!
//! This crate must not depend on openslate-core (core depends on this crate);
//! keep the public API self-contained over plain data types.

pub mod describe;
pub mod executor;
pub mod normalize;
pub mod prompt;
pub mod ts_types;

pub use executor::run_code;
pub use normalize::prepare;
pub use prompt::{run_code_description, run_code_parameters_schema};
pub use ts_types::generate_declarations;

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

/// Name of the model-facing tool that executes PTC code.
pub const RUN_CODE_TOOL: &str = "run_code";

// ── Tool call modes ──────────────────────────────────────────────────────────

/// How a tool may be invoked: normal (direct) tool calls, PTC code, or both.
///
/// Configured per-tool via `[ptc.tool_modes]` in `openslate.toml`
/// (glob patterns allowed; see [`resolve_tool_mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ToolCallMode {
    /// Only normal (direct) tool calls; not callable from PTC code.
    #[serde(rename = "direct")]
    DirectOnly,
    /// Only callable from PTC code; hidden from the model's direct tool list.
    #[serde(rename = "ptc")]
    PtcOnly,
    /// Callable both ways (default when PTC is enabled).
    #[default]
    #[serde(rename = "both")]
    Both,
}

impl ToolCallMode {
    /// Whether the tool's schema may appear in the model's direct tool list.
    pub fn direct_visible(self) -> bool {
        !matches!(self, ToolCallMode::PtcOnly)
    }

    /// Whether the tool may be bound into the PTC sandbox.
    pub fn ptc_callable(self) -> bool {
        !matches!(self, ToolCallMode::DirectOnly)
    }
}

/// Resolve the effective [`ToolCallMode`] for a tool name from the
/// `[ptc.tool_modes]` pattern map.
///
/// The longest matching glob pattern wins (ties broken lexicographically for
/// determinism). Names matching no pattern default to [`ToolCallMode::Both`]
/// when PTC is enabled, and [`ToolCallMode::DirectOnly`] when disabled.
pub fn resolve_tool_mode(
    patterns: &BTreeMap<String, ToolCallMode>,
    tool_name: &str,
    ptc_enabled: bool,
) -> ToolCallMode {
    if !ptc_enabled {
        return ToolCallMode::DirectOnly;
    }
    let mut best: Option<(&str, ToolCallMode)> = None;
    for (pattern, mode) in patterns {
        if wildcard_match(tool_name, pattern) {
            match best {
                // Later candidate only replaces the incumbent when strictly
                // longer, or equal length and lexicographically smaller —
                // BTreeMap iteration order makes equal-length deterministic.
                Some((bp, _)) if bp.len() > pattern.len() => {}
                Some((bp, _)) if bp.len() == pattern.len() && bp < pattern.as_str() => {}
                _ => best = Some((pattern.as_str(), *mode)),
            }
        }
    }
    best.map(|(_, mode)| mode).unwrap_or(ToolCallMode::Both)
}

/// Glob match with the same semantics as core's tool whitelist matching
/// (`tool.rs::tool_name_matches`): plain patterns (no metacharacters) require
/// an exact match; otherwise `*` matches any run of characters and `?`
/// matches exactly one. Keep in sync with the core implementation.
pub(crate) fn wildcard_match(text: &str, pattern: &str) -> bool {
    if !pattern.contains('*') && !pattern.contains('?') {
        return text == pattern;
    }
    fn rec(t: &[char], p: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') => (0..=t.len()).any(|i| rec(&t[i..], &p[1..])),
            Some('?') => !t.is_empty() && rec(&t[1..], &p[1..]),
            Some(&c) => !t.is_empty() && t[0] == c && rec(&t[1..], &p[1..]),
        }
    }
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    rec(&t, &p)
}

// ── Disclosure tiers ─────────────────────────────────────────────────────────

/// How much of the PTC tool surface is injected into the `run_code` tool
/// description (`[ptc] disclosure`, see `PTC_PLAN.md` §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Disclosure {
    /// All PTC-visible tools as full TypeScript signatures.
    Full,
    /// Only catalog lines (name + first sentence of the description); the
    /// full signatures are fetched inside the sandbox via
    /// `list_tools`/`describe_tool`.
    Catalog,
    /// Full signatures when they fit `max_list_chars`; over budget, first
    /// demote `both`-mode tools (whose schemas already sit in the direct
    /// tool list) to catalog lines, then fall back to a pure catalog.
    #[default]
    Auto,
}

// ── Shared data types ────────────────────────────────────────────────────────

/// Tool metadata used to generate the TypeScript declarations injected into
/// the `run_code` tool description. Plain data; no core dependency.
#[derive(Debug, Clone)]
pub struct PtcToolInfo {
    /// Registry name (dispatch key), e.g. `github_list_prs` or `read_file`.
    pub name: String,
    /// Optional namespace (e.g. the MCP server alias `github`). `None` puts
    /// the tool at the root level of the `tools` object; `Some(ns)` exposes
    /// it under the composed path `tools.<ns>.<method>`, where `method` is
    /// the registry `name` with the `{ns}_` prefix stripped (falling back
    /// to the full name), sanitized into a valid identifier. Both the
    /// generated TypeScript declarations and the sandbox bindings follow
    /// this rule; dispatch always uses the full registry `name`.
    pub namespace: Option<String>,
    /// Tool description (becomes the JSDoc comment).
    pub description: String,
    /// JSON Schema of the tool parameters (may be `Value::Null`).
    pub parameters: serde_json::Value,
}

/// A tool bound into a PTC execution.
///
/// `name` is the registry name used for dispatch through the bridge.
/// `namespace` controls the sandbox exposure path: `None` binds the tool
/// flat at `tools.<name>`; `Some(ns)` binds it under the composed path
/// `tools.<ns>.<method>` (see [`PtcToolInfo::namespace`] for the method
/// name rule) while dispatch still uses the full `name`.
#[derive(Debug, Clone)]
pub struct PtcBoundTool {
    /// Registry name (exact dispatch key passed to the bridge).
    pub name: String,
    /// Optional namespace for composed-path exposure inside the sandbox.
    pub namespace: Option<String>,
}

/// Resource limits for a single `run_code` execution.
#[derive(Debug, Clone)]
pub struct PtcLimits {
    /// Hard wall-clock timeout per execution (interrupt handler based).
    pub timeout_ms: u64,
    /// QuickJS heap limit in bytes.
    pub memory_limit_bytes: usize,
    /// Truncation budget for the combined result+logs output, in bytes.
    pub max_output_bytes: usize,
    /// Max tool calls a single script may make through the bridge.
    pub max_tool_calls_per_run: usize,
    /// Max `list_tools`/`describe_tool` lookup calls per run (separate,
    /// more generous budget than [`PtcLimits::max_tool_calls_per_run`]).
    pub max_lookup_calls: usize,
}

impl Default for PtcLimits {
    fn default() -> Self {
        Self {
            timeout_ms: 60_000,
            memory_limit_bytes: 64 * 1024 * 1024,
            max_output_bytes: 64 * 1024,
            max_tool_calls_per_run: 16,
            max_lookup_calls: 50,
        }
    }
}

/// Outcome of a `run_code` execution. Errors are data, not `Err` — the caller
/// formats them into the tool result so the model can self-heal.
#[derive(Debug, Clone, Default)]
pub struct PtcOutcome {
    /// Final return value of the script, JSON-encoded (`None` on error).
    pub result: Option<String>,
    /// Error message (script exception, timeout, memory limit, …).
    pub error: Option<String>,
    /// Captured `console.log/warn/error` lines.
    pub logs: Vec<String>,
    /// Number of bridge tool calls actually attempted (envelope errors
    /// included, budget-exceeded calls included).
    pub tool_calls: usize,
    /// Whether execution was aborted by the wall-clock timeout.
    pub timed_out: bool,
}

/// Host-side tool bridge: `(registry_name, args_json)` → envelope JSON string
/// (`{"result": ...}` or `{"error": "..."}`, see [`envelope_result`] /
/// [`envelope_error`]). Always returns a value; never panics across the
/// boundary (errors-as-data protocol).
pub type ToolBridge = Arc<dyn Fn(&str, &str) -> BoxFuture<'static, String> + Send + Sync>;

// ── Envelope helpers ─────────────────────────────────────────────────────────

/// Build a success envelope for the sandbox bridge.
pub fn envelope_result(value: serde_json::Value) -> String {
    serde_json::json!({ "result": value }).to_string()
}

/// Build an error envelope for the sandbox bridge (becomes a JS exception
/// inside the sandbox).
pub fn envelope_error(message: impl std::fmt::Display) -> String {
    serde_json::json!({ "error": message.to_string() }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes(pairs: &[(&str, ToolCallMode)]) -> BTreeMap<String, ToolCallMode> {
        pairs.iter().map(|(p, m)| (p.to_string(), *m)).collect()
    }

    #[test]
    fn resolve_defaults() {
        let empty = BTreeMap::new();
        assert_eq!(resolve_tool_mode(&empty, "x", true), ToolCallMode::Both);
        assert_eq!(
            resolve_tool_mode(&empty, "x", false),
            ToolCallMode::DirectOnly
        );
    }

    #[test]
    fn resolve_longest_glob_wins() {
        let m = modes(&[
            ("*", ToolCallMode::Both),
            ("github_*", ToolCallMode::PtcOnly),
            ("github_search_*", ToolCallMode::DirectOnly),
        ]);
        assert_eq!(
            resolve_tool_mode(&m, "github_search_code", true),
            ToolCallMode::DirectOnly
        );
        assert_eq!(
            resolve_tool_mode(&m, "github_get_file", true),
            ToolCallMode::PtcOnly
        );
        assert_eq!(resolve_tool_mode(&m, "shell", true), ToolCallMode::Both);
    }

    #[test]
    fn resolve_exact_before_glob() {
        let m = modes(&[
            ("*_file", ToolCallMode::PtcOnly),
            ("read_file", ToolCallMode::Both),
        ]);
        // "read_file" is longer than "*_file" (8 > 6) so it wins.
        assert_eq!(resolve_tool_mode(&m, "read_file", true), ToolCallMode::Both);
        assert_eq!(
            resolve_tool_mode(&m, "write_file", true),
            ToolCallMode::PtcOnly
        );
    }

    #[test]
    fn wildcard_semantics_match_core() {
        assert!(wildcard_match("github_list_prs", "github_*"));
        assert!(wildcard_match("read_file", "read_file"));
        assert!(wildcard_match("read_file", "read_*")); // has metachar → glob
        assert!(!wildcard_match("read_files", "read_file")); // plain → exact
        assert!(!wildcard_match("write_file", "read_*"));
        assert!(wildcard_match("read_fi_e", "read_fi?e"));
        assert!(wildcard_match("aXBc", "*X*"));
    }

    #[test]
    fn disclosure_serde_roundtrip() {
        assert_eq!(
            serde_json::from_str::<Disclosure>("\"full\"").unwrap(),
            Disclosure::Full
        );
        assert_eq!(
            serde_json::from_str::<Disclosure>("\"catalog\"").unwrap(),
            Disclosure::Catalog
        );
        assert_eq!(
            serde_json::from_str::<Disclosure>("\"auto\"").unwrap(),
            Disclosure::Auto
        );
        assert_eq!(Disclosure::default(), Disclosure::Auto);
        // Unknown tiers are rejected (config validation relies on serde).
        assert!(serde_json::from_str::<Disclosure>("\"everything\"").is_err());
        assert_eq!(
            serde_json::to_value(Disclosure::Catalog).unwrap(),
            serde_json::json!("catalog")
        );
    }

    #[test]
    fn limits_default_includes_lookup_budget() {
        let limits = crate::PtcLimits::default();
        assert_eq!(limits.max_lookup_calls, 50);
        assert_eq!(limits.max_tool_calls_per_run, 16);
    }
}
