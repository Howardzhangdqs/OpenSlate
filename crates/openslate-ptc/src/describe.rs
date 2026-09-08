//! Sandbox-side tool discovery formatting (`list_tools` / `describe_tool`,
//! `PTC_PLAN.md` §5.3).
//!
//! The catalog tier injects only `path: first sentence` lines into the
//! `run_code` description; the full signatures stay available inside the
//! sandbox through these helpers, so a weak model can look a tool up before
//! calling it without burning prompt budget on every schema.

use serde_json::Value;

use crate::ts_types::{effective_paths, literal, sanitize_ident, single_declaration_at, tool_path};
use crate::{wildcard_match, PtcToolInfo};

/// Render catalog lines for the catalog entries matching `pattern` (glob,
/// same semantics as the agent tool whitelist), sorted by effective path and
/// joined with newlines. A tool matches when either its composed access path
/// (`github.list_prs`) or its registry name (`github_list_prs`) matches.
/// Paths come from a full-set binding plan, so collision renames are
/// consistent with what the sandbox actually binds.
///
/// Each line is `path: first sentence of the description` (empty string
/// when nothing matches).
pub fn list(catalog: &[PtcToolInfo], pattern: &str) -> String {
    let paths = effective_paths(catalog);
    let mut matched: Vec<(&PtcToolInfo, &String)> = catalog
        .iter()
        .filter(|t| {
            paths
                .get(&t.name)
                .is_some_and(|p| wildcard_match(p, pattern) || wildcard_match(&t.name, pattern))
        })
        .map(|t| (t, paths.get(&t.name).expect("path for tool")))
        .collect();
    matched.sort_by(|a, b| a.1.cmp(b.1));
    matched
        .iter()
        .map(|(t, p)| catalog_line_at(t, p))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One catalog line: `github.list_prs: List pull requests`. The description
/// is cut at the first sentence boundary (first `.` or newline) and
/// newlines are collapsed, so the line stays a single line. Tools without
/// a description render as just the path. Single-tool path; prefer
/// [`catalog_line_at`] when a full-set plan is available.
pub(crate) fn catalog_line(info: &PtcToolInfo) -> String {
    catalog_line_at(info, &tool_path(info))
}

/// [`catalog_line`] with an explicit effective access path.
pub(crate) fn catalog_line_at(info: &PtcToolInfo, path: &str) -> String {
    let sentence = first_sentence(&info.description);
    if sentence.is_empty() {
        path.to_string()
    } else {
        format!("{}: {}", path, sentence)
    }
}

/// Full description of a single tool: its TypeScript declaration block,
/// a blank line, and a synthesized example call. Self-contained — suitable
/// for returning from `describe_tool` without any surrounding context.
/// Uses the tool's single-tool path; pass a full-set path via
/// [`describe_at`] when the binding set is known.
pub fn describe(info: &PtcToolInfo) -> String {
    describe_at(info, &tool_path(info))
}

/// [`describe`] with an explicit effective access path (e.g. from
/// [`crate::ts_types::effective_paths`]) so the shown path matches the
/// sandbox binding even under collision renames. `pub(crate)` for the
/// sandbox `describe_tool` host function.
pub(crate) fn describe_at(info: &PtcToolInfo, path: &str) -> String {
    format!(
        "{}\n\n{}",
        single_declaration_at(info, path),
        synthesize_example_at(info, path)
    )
}

/// First sentence of a description: cut at the first `.` or newline
/// (whitespace-trimmed). The newline cut also guarantees no embedded
/// newlines survive into single-line output.
fn first_sentence(description: &str) -> String {
    let trimmed = description.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match trimmed.find(['.', '\n', '\r']) {
        Some(idx) => trimmed[..idx].trim().to_string(),
        None => trimmed.to_string(),
    }
}

/// Synthesize an example call from the first level of the tool's parameter
/// schema: string → `"..."`, number/integer → `0`, boolean → `true`,
/// enum → first value (quoted when a string), array → `[]`, object → `{}`,
/// anything unrecognized → `"..."`. Rendered as
/// `// Example: await tools.github.list_prs({ state: "open", limit: 0 })`
/// using the composed (or flat) access path. Property keys are sanitized
/// into identifiers (same rule as the TS declarations) so a key containing
/// newlines/quotes cannot break out of the comment line.
fn synthesize_example_at(info: &PtcToolInfo, path: &str) -> String {
    let args: Vec<String> = info
        .parameters
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| {
            props
                .iter()
                .map(|(key, schema)| format!("{}: {}", sanitize_ident(key), example_value(schema)))
                .collect()
        })
        .unwrap_or_default();
    // `{}` for no args, `{ a: 0, b: "x" }` otherwise.
    let body = if args.is_empty() {
        String::new()
    } else {
        format!(" {} ", args.join(", "))
    };
    format!("// Example: await tools.{path}({{{}}})", body)
}

/// Map a first-level property schema to a literal example value.
fn example_value(schema: &Value) -> String {
    if let Some(first) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
    {
        if let Some(literal) = literal(first) {
            return literal;
        }
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => "\"...\"".to_string(),
        Some("integer") | Some("number") => "0".to_string(),
        Some("boolean") => "true".to_string(),
        Some("array") => "[]".to_string(),
        Some("object") => "{}".to_string(),
        _ => "\"...\"".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str, parameters: Value) -> PtcToolInfo {
        PtcToolInfo {
            name: name.to_string(),
            namespace: None,
            description: description.to_string(),
            parameters,
        }
    }

    fn namespaced(ns: &str, name: &str, description: &str) -> PtcToolInfo {
        PtcToolInfo {
            name: name.to_string(),
            namespace: Some(ns.to_string()),
            description: description.to_string(),
            parameters: Value::Null,
        }
    }

    #[test]
    fn list_filters_by_path_and_registry_name() {
        let catalog = vec![
            namespaced(
                "github",
                "github_list_prs",
                "List pull requests. Extra detail.",
            ),
            namespaced("github", "github_get_file", "Get file contents"),
            tool("read_file", "Read a file from the workspace.", Value::Null),
        ];
        // Glob on composed path:
        assert_eq!(
            list(&catalog, "github.*"),
            "github.get_file: Get file contents\ngithub.list_prs: List pull requests"
        );
        // Plain pattern = exact match; both path and registry name work:
        assert_eq!(
            list(&catalog, "read_file"),
            "read_file: Read a file from the workspace"
        );
        assert_eq!(
            list(&catalog, "github_list_prs"),
            "github.list_prs: List pull requests"
        );
        // Wildcard catches everything:
        assert_eq!(list(&catalog, "*").lines().count(), 3);
        // No match -> empty:
        assert_eq!(list(&catalog, "gitlab.*"), "");
    }

    #[test]
    fn list_first_sentence_truncation() {
        let catalog = vec![
            tool(
                "a",
                "One sentence. Two sentences\nOn new lines.",
                Value::Null,
            ),
            tool("b", "No terminal punctuation", Value::Null),
            tool("c", "  ", Value::Null),
        ];
        assert_eq!(
            list(&catalog, "*"),
            "a: One sentence\nb: No terminal punctuation\nc"
        );
    }

    #[test]
    fn describe_renders_declaration_plus_example() {
        let info = PtcToolInfo {
            name: "github_list_prs".to_string(),
            namespace: Some("github".to_string()),
            description: "List pull requests".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "state": { "type": "string", "enum": ["open", "closed", "all"] },
                    "limit": { "type": "integer" }
                }
            }),
        };
        assert_eq!(
            describe(&info),
            "declare const tools: {\n  \
github: {\n    \
/** List pull requests */\n    \
list_prs: (input: {\n      \
limit?: number;\n      \
state?: \"open\" | \"closed\" | \"all\";\n    \
}) => Promise<any>;\n  \
};\n\
};\n\n\
// Example: await tools.github.list_prs({ limit: 0, state: \"open\" })"
        );
    }

    #[test]
    fn example_value_type_mapping() {
        let props = serde_json::json!({
            "type": "object",
            "properties": {
                "s": { "type": "string" },
                "n": { "type": "number" },
                "i": { "type": "integer" },
                "b": { "type": "boolean" },
                "e": { "enum": ["x", "y"] },
                "en": { "enum": [1, 2] },
                "a": { "type": "array", "items": { "type": "string" } },
                "o": { "type": "object", "properties": { "k": { "type": "string" } } },
                "u": { "type": "weird" },
                "m": {},
                "t": true
            }
        });
        let info = tool("t", "", props);
        assert_eq!(
            describe(&info),
            "declare const tools: {\n  t: (input: {\n    \
a?: string[];\n    \
b?: boolean;\n    \
e?: \"x\" | \"y\";\n    \
en?: 1 | 2;\n    \
i?: number;\n    \
m?: any;\n    \
n?: number;\n    \
o?: { k?: string };\n    \
s?: string;\n    \
t?: any;\n    \
u?: any;\n  \
}) => Promise<any>;\n\
};\n\n\
// Example: await tools.t({ a: [], b: true, e: \"x\", en: 1, i: 0, m: \"...\", n: 0, o: {}, s: \"...\", t: \"...\", u: \"...\" })"
        );
    }

    #[test]
    fn example_without_properties_is_empty_object() {
        let info = tool("ping", "Ping something", Value::Null);
        assert_eq!(
            synthesize_example_at(&info, &tool_path(&info)),
            "// Example: await tools.ping({})"
        );
    }

    #[test]
    fn example_sanitizes_hostile_property_keys() {
        // A key containing newlines/quotes must not break out of the
        // `// Example:` comment line (keys are sanitized into identifiers,
        // same rule as the TS declarations).
        let info = tool(
            "t",
            "hostile schema",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "evil\nkey\"q": { "type": "string" },
                    "ok": { "type": "integer" }
                }
            }),
        );
        let rendered = describe(&info);
        let example_line = rendered.lines().last().unwrap();
        // Exact match: the key is sanitized into an identifier and the
        // example stays a single comment line.
        assert_eq!(
            example_line,
            "// Example: await tools.t({ evilkeyq: \"...\", ok: 0 })"
        );
    }
}
