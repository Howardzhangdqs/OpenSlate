//! Description assembly for the model-facing `run_code` tool.
//!
//! The tool description embeds a tool-surface block plus a fixed usage
//! template (`PTC_PLAN.md` §5.4). Three disclosure tiers decide how much of
//! the surface is inlined (`PTC_PLAN.md` §5.2):
//!
//! - [`Disclosure::Full`]: every PTC-visible tool as a full TS signature;
//! - [`Disclosure::Catalog`]: only `path: first sentence` catalog lines —
//!   the model browses full signatures inside the sandbox via
//!   `list_tools`/`describe_tool`;
//! - [`Disclosure::Auto`] (default): start from Full; over `max_list_chars`,
//!   demote `both`-mode tools (whose schemas already sit in the direct tool
//!   list) to catalog lines; still over, fall back to a pure catalog.
//!
//! A one-line helpers hint is appended to the tool block in every tier —
//! the discovery helpers are easy to miss otherwise (they live inside the
//! sandbox, not in the tool list). Blocks over the budget are truncated
//! with an explicit marker.

use crate::describe::catalog_line;
use crate::ts_types::{
    catalog_lines, generate_declarations, generate_declarations_mixed, tool_block,
};
use crate::{Disclosure, PtcToolInfo};

/// The `run_code` tool description template; `{{types}}` is replaced with
/// the tool-surface block.
const TEMPLATE: &str = "\
Execute JavaScript to orchestrate tool calls.

Available:
{{types}}

Write an async arrow function. Do NOT use TypeScript syntax - no type
annotations, interfaces, or generics. Do NOT define named functions.
Example: async () => { const r = await tools.read_file({ path: \"x\" }); return r; }
Tool errors throw inside code - use try/catch when needed.
Use console.log for intermediate diagnostics; only the final return value
and logs are shown back to you.";

/// Helpers hint appended after the tool block in EVERY disclosure tier.
/// Real-model runs showed the discovery helpers are undiscoverable without
/// it: models naturally tried `tools.list_tools(...)` and Full-tier
/// descriptions never mentioned the helpers at all.
const HELPERS_HINT: &str = "Helpers: list_tools(pattern) and describe_tool(name) — also available as tools.list_tools / tools.describe_tool — browse tool signatures from inside code.";

/// Build the `run_code` tool description.
///
/// - `tools`: all PTC-visible tools (for signature/catalog rendering);
/// - `both_mode_names`: registry names of tools also exposed as direct tool
///   calls — the first candidates for auto-tier demotion, since their
///   schemas are already paid for in the direct tool list;
/// - `mode`: the disclosure tier;
/// - `max_list_chars`: character budget for the tool block (0 = unlimited).
pub fn run_code_description(
    tools: &[PtcToolInfo],
    both_mode_names: &[String],
    mode: Disclosure,
    max_list_chars: usize,
) -> String {
    let demote: std::collections::HashSet<&str> =
        both_mode_names.iter().map(String::as_str).collect();

    let types = match mode {
        Disclosure::Full => generate_declarations(tools),
        Disclosure::Catalog => catalog_lines(tools),
        Disclosure::Auto => {
            let full = generate_declarations(tools);
            if over_budget(&full, max_list_chars) {
                let (mixed, _demoted) = generate_declarations_mixed(tools, &demote);
                if over_budget(&mixed, max_list_chars) {
                    catalog_lines(tools)
                } else {
                    mixed
                }
            } else {
                full
            }
        }
    };

    let types = match over_budget(&types, max_list_chars) {
        // Final safety net (e.g. a pure catalog still over budget).
        true => truncate_with_marker(&types, tools, max_list_chars),
        false => types,
    };
    // The helpers hint is unconditional — without it the sandbox-side
    // discovery helpers (and their `tools.*` aliases) are undiscoverable,
    // especially in the Full tier where nothing else mentions them.
    let types = format!("{types}\n\n{HELPERS_HINT}");
    TEMPLATE.replace("{{types}}", &types)
}

/// Whether a rendered block exceeds the budget (a budget of 0 disables the
/// check).
fn over_budget(block: &str, max_list_chars: usize) -> bool {
    max_list_chars > 0 && block.chars().count() > max_list_chars
}

/// Cut the block to the budget (char boundary safe) and append a marker
/// noting the original size and the number of tools whose entry (full
/// signature or catalog line) did not survive the cut.
fn truncate_with_marker(block: &str, tools: &[PtcToolInfo], max_list_chars: usize) -> String {
    let total_chars = block.chars().count();
    let prefix: String = block.chars().take(max_list_chars).collect();
    let included = tools
        .iter()
        .filter(|t| {
            let indent = if t.namespace.is_some() { 4 } else { 2 };
            let full_block = tool_block(t, indent);
            let line = catalog_line(t);
            prefix.contains(full_block.as_str()) || prefix.contains(line.as_str())
        })
        .count();
    let omitted = tools.len().saturating_sub(included);
    format!(
        "{prefix}\n// --- truncated (original {total_chars} chars, {omitted} tools omitted) ---"
    )
}

/// The JSON Schema for the `run_code` tool parameters: a single required
/// `code` string.
pub fn run_code_parameters_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "code": {
                "type": "string",
                "description": "JavaScript async arrow function to execute"
            }
        },
        "required": ["code"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str) -> PtcToolInfo {
        PtcToolInfo {
            name: name.to_string(),
            namespace: None,
            description: description.to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        }
    }

    fn github_tool(method: &str, description: &str) -> PtcToolInfo {
        PtcToolInfo {
            name: format!("github_{method}"),
            namespace: Some("github".to_string()),
            description: description.to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "state": { "type": "string", "enum": ["open", "closed", "all"] },
                    "limit": { "type": "integer" }
                }
            }),
        }
    }

    fn both(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn full_mode_inlines_all_signatures_with_helpers_hint() {
        let tools = vec![
            tool("read_file", "Read a file"),
            github_tool("list_prs", "List pull requests"),
        ];
        let desc = run_code_description(&tools, &[], Disclosure::Full, 8000);
        assert!(desc.starts_with(
            "Execute JavaScript to orchestrate tool calls.\n\nAvailable:\ndeclare const tools: {\n"
        ));
        assert!(desc.contains("read_file: (input: { path: string }) => Promise<any>;"));
        assert!(desc.contains("github: {\n    /** List pull requests */"));
        // The helpers hint is present even without demotion (discoverability):
        assert!(desc.contains(&format!(
            "}};\n\n{HELPERS_HINT}\n\nWrite an async arrow function."
        )));
        assert!(desc.ends_with("and logs are shown back to you."));
    }

    #[test]
    fn catalog_mode_uses_lines_and_hint() {
        let tools = vec![
            tool("read_file", "Read a file. Really."),
            github_tool("list_prs", "List pull requests"),
        ];
        let desc = run_code_description(&tools, &[], Disclosure::Catalog, 8000);
        assert!(desc.contains(
            "Available:\ngithub.list_prs: List pull requests\nread_file: Read a file\n\n"
        ));
        assert!(!desc.contains("declare const"));
        assert!(!desc.contains("Promise<any>"));
        assert!(desc.contains(&format!(
            "\n\n{HELPERS_HINT}\n\nWrite an async arrow function."
        )));
    }

    #[test]
    fn auto_within_budget_keeps_full_render() {
        let tools = vec![
            tool("read_file", "Read a file"),
            github_tool("list_prs", "List pull requests"),
        ];
        // Budget comfortably above the full render size:
        let desc = run_code_description(&tools, &both(&["read_file"]), Disclosure::Auto, 8000);
        assert!(desc.contains("declare const tools: {"));
        assert!(desc.contains("(input: { path: string }) => Promise<any>;"));
        assert!(desc.contains(HELPERS_HINT));
        assert!(!desc.contains("// github.list_prs"));
    }

    #[test]
    fn auto_over_budget_demotes_both_tools() {
        let tools = vec![
            tool("read_file", "Read a file"),
            github_tool("list_prs", "List pull requests"),
            github_tool("get_file", "Get a file"),
        ];
        let full = generate_declarations(&tools);
        // Keep the ptc-only tool full but demote both github tools:
        let github_only: Vec<PtcToolInfo> = tools[1..].to_vec();
        let only_github = generate_declarations(&github_only);
        // Budget between "everything demoted but read_file" and "full".
        let budget = only_github.chars().count() + 10;
        assert!(
            budget < full.chars().count(),
            "test premise: mixed must be smaller than full"
        );

        let desc = run_code_description(
            &tools,
            &both(&["github_list_prs", "github_get_file"]),
            Disclosure::Auto,
            budget,
        );
        // read_file stays a full signature:
        assert!(desc.contains("read_file: (input: { path: string }) => Promise<any>;"));
        // github tools become comment catalog lines:
        assert!(desc.contains("  // github.list_prs: List pull requests\n"));
        assert!(desc.contains("  // github.get_file: Get a file\n"));
        assert!(!desc.contains("list_prs: (input:"));
        assert!(desc.contains(HELPERS_HINT));
    }

    #[test]
    fn auto_extreme_budget_falls_back_to_full_catalog() {
        let tools = vec![
            tool("read_file", "Read a file"),
            github_tool("list_prs", "List pull requests"),
        ];
        // Budget small enough that even the demoted render overflows, but
        // large enough for the pure catalog (no truncation marker):
        // full ~259 > 100; mixed (read_file + comment) ~142 > 100;
        // catalog ~58 <= 100.
        let desc = run_code_description(&tools, &both(&["github_list_prs"]), Disclosure::Auto, 100);
        assert!(!desc.contains("declare const"));
        assert!(desc.contains("github.list_prs: List pull requests"));
        assert!(desc.contains("read_file: Read a file"));
        assert!(desc.contains(HELPERS_HINT));
        assert!(!desc.contains("truncated"));
    }

    #[test]
    fn overlong_catalog_block_is_truncated_with_marker() {
        let tools: Vec<PtcToolInfo> = (0..30)
            .map(|i| {
                tool(
                    &format!("tool_{i:02}"),
                    &format!("Tool number {i} does things"),
                )
            })
            .collect();
        let budget = 400;
        let full = catalog_lines(&tools);
        let prefix: String = full.chars().take(budget).collect();
        let included = tools
            .iter()
            .filter(|t| prefix.contains(&catalog_line(t)))
            .count();
        let marker = format!(
            "\n// --- truncated (original {} chars, {} tools omitted) ---",
            full.chars().count(),
            30 - included
        );
        let desc = run_code_description(&tools, &[], Disclosure::Catalog, budget);
        assert!(desc.contains(&marker), "desc: {desc}");
        assert!(!desc.contains("tool_29"));
        // Hint sits after the marker and the template tail survives:
        assert!(desc.contains(&format!("{marker}\n\n{HELPERS_HINT}")));
        assert!(desc.ends_with("and logs are shown back to you."));
    }

    #[test]
    fn zero_budget_disables_truncation_and_demotion() {
        let tools: Vec<PtcToolInfo> = (0..30)
            .map(|i| tool(&format!("tool_{i:02}"), "d"))
            .collect();
        let desc = run_code_description(&tools, &both(&["tool_00"]), Disclosure::Auto, 0);
        assert!(desc.contains("tool_29"));
        assert!(!desc.contains("truncated"));
        // No demotion happened (no comment lines), but the hint is always on:
        assert!(!desc.contains("// tool_"));
        assert!(desc.contains(HELPERS_HINT));
    }

    #[test]
    fn parameters_schema_exact() {
        assert_eq!(
            run_code_parameters_schema(),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "JavaScript async arrow function to execute"
                    }
                },
                "required": ["code"],
                "additionalProperties": false
            })
        );
    }
}
