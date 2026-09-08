//! JSON Schema to TypeScript declaration generation.
//!
//! Generates the `declare const tools: { ... }` block injected into the
//! `run_code` tool description (see `PTC_PLAN.md` §5.1). Ported in spirit
//! from Cloudflare's code-mode `json-schema-types.ts`, with OpenSlate
//! adjustments:
//! - parameter objects are inlined into the method signature (no
//!   intermediate `XInput` types);
//! - every method returns `Promise<any>` (tool results are JSON/text and
//!   MCP tools rarely declare output schemas);
//! - unknown or over-nested schema constructs degrade to `any` instead of
//!   failing, so a weird schema never breaks the whole tool list.
//!
//! Output is fully deterministic: same input orderings (tools sorted by
//! name, namespace groups sorted by name) produce byte-identical strings —
//! tests use exact-string ("snapshot") assertions on purpose.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::PtcToolInfo;

/// Maximum inline object-nesting depth; deeper composites become `any`.
const MAX_DEPTH: usize = 3;

/// JavaScript reserved words that are legal property names in JSON Schemas
/// but need escaping when used as identifiers in a declaration.
const RESERVED: &[&str] = &[
    "if",
    "for",
    "delete",
    "new",
    "class",
    "return",
    "function",
    "var",
    "let",
    "const",
    "do",
    "while",
    "switch",
    "case",
    "in",
    "of",
    "this",
    "null",
    "true",
    "false",
    "void",
    "typeof",
    "instanceof",
    "default",
    "export",
    "import",
    "try",
    "catch",
    "finally",
    "throw",
    "break",
    "continue",
];

// ── Public API ───────────────────────────────────────────────────────────────

/// Generate the TypeScript declarations for the given tools.
///
/// Tools with a namespace become a nested group (`tools.github.list_prs`),
/// namespace-less tools sit at the root level. Both levels are sorted by
/// name for deterministic output.
pub fn generate_declarations(tools: &[PtcToolInfo]) -> String {
    let mut root: Vec<&PtcToolInfo> = Vec::new();
    let mut groups: BTreeMap<String, Vec<&PtcToolInfo>> = BTreeMap::new();
    for tool in tools {
        match &tool.namespace {
            None => root.push(tool),
            Some(ns) => groups.entry(sanitize_ident(ns)).or_default().push(tool),
        }
    }
    root.sort_by_key(|t| method_name(t));
    for members in groups.values_mut() {
        members.sort_by_key(|t| method_name(t));
    }

    let mut out = String::from("declare const tools: {\n");
    for tool in root {
        out.push_str(&tool_block(tool, 2));
    }
    for (ns, members) in &groups {
        out.push_str(&format!("  {ns}: {{\n"));
        for tool in members {
            out.push_str(&tool_block(tool, 4));
        }
        out.push_str("  };\n");
    }
    out.push_str("};");
    out
}

/// Generate a declaration block for a single tool. For a namespaced tool
/// this nests the method inside its namespace group — byte-identical to
/// what [`generate_declarations`] would render for a one-tool catalog.
pub fn single_declaration(info: &PtcToolInfo) -> String {
    let mut out = String::from("declare const tools: {\n");
    match &info.namespace {
        None => out.push_str(&tool_block(info, 2)),
        Some(ns) => {
            out.push_str(&format!("  {}: {{\n", sanitize_ident(ns)));
            out.push_str(&tool_block(info, 4));
            out.push_str("  };\n");
        }
    }
    out.push_str("};");
    out
}

/// Generate catalog lines for the given tools (one per line, sorted by
/// path): `github.list_prs: List pull requests`. Used by the `catalog`
/// disclosure tier; line format shared with [`crate::describe::list`].
pub fn catalog_lines(tools: &[PtcToolInfo]) -> String {
    let mut sorted: Vec<&PtcToolInfo> = tools.iter().collect();
    sorted.sort_by_key(|t| tool_path(t));
    sorted
        .iter()
        .map(|t| crate::describe::catalog_line(t))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render declarations where the tools named in `demote` (registry names)
/// appear as comment catalog lines instead of full signatures. Returns the
/// block and the number of tools actually demoted. `pub(crate)` for the
/// `auto` disclosure tier in `prompt.rs`.
pub(crate) fn generate_declarations_mixed(
    tools: &[PtcToolInfo],
    demote: &std::collections::HashSet<&str>,
) -> (String, usize) {
    let mut root: Vec<&PtcToolInfo> = Vec::new();
    let mut groups: BTreeMap<String, Vec<&PtcToolInfo>> = BTreeMap::new();
    for tool in tools {
        match &tool.namespace {
            None => root.push(tool),
            Some(ns) => groups.entry(sanitize_ident(ns)).or_default().push(tool),
        }
    }
    root.sort_by_key(|t| method_name(t));
    for members in groups.values_mut() {
        members.sort_by_key(|t| method_name(t));
    }

    let mut out = String::from("declare const tools: {\n");
    let mut demoted_lines: Vec<String> = Vec::new();
    let mut demoted_count = 0usize;
    for tool in root {
        if demote.contains(tool.name.as_str()) {
            demoted_lines.push(format!("  // {}", crate::describe::catalog_line(tool)));
            demoted_count += 1;
        } else {
            out.push_str(&tool_block(tool, 2));
        }
    }
    for (ns, members) in &groups {
        let full: Vec<&&PtcToolInfo> = members
            .iter()
            .filter(|t| !demote.contains(t.name.as_str()))
            .collect();
        for tool in members {
            if demote.contains(tool.name.as_str()) {
                demoted_lines.push(format!("  // {}", crate::describe::catalog_line(tool)));
                demoted_count += 1;
            }
        }
        if full.is_empty() {
            continue; // whole group demoted: no empty braces
        }
        out.push_str(&format!("  {ns}: {{\n"));
        for tool in full {
            out.push_str(&tool_block(tool, 4));
        }
        out.push_str("  };\n");
    }
    for line in demoted_lines {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("};");
    (out, demoted_count)
}

/// Render the method block (JSDoc + signature) for a single tool at the
/// given indent width. `pub(crate)` so `prompt.rs` can count how many tools
/// survive declaration truncation.
pub(crate) fn tool_block(tool: &PtcToolInfo, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut out = String::new();
    if let Some(doc) = jsdoc(&tool.description) {
        out.push_str(&format!("{pad}/** {doc} */\n"));
    }
    let input = render_type(
        &tool.parameters,
        &tool.parameters,
        1,
        indent + 2,
        &mut Vec::new(),
    );
    out.push_str(&format!(
        "{pad}{}: (input: {input}) => Promise<any>;\n",
        method_name(tool)
    ));
    out
}

/// The method name a tool is exposed under: for namespaced tools the
/// `{namespace}_` prefix is stripped from the registry name, then the result
/// is sanitized into a valid TS identifier.
fn method_name(tool: &PtcToolInfo) -> String {
    let raw = match &tool.namespace {
        Some(ns) => tool
            .name
            .strip_prefix(&format!("{ns}_"))
            .unwrap_or(tool.name.as_str()),
        None => tool.name.as_str(),
    };
    sanitize_ident(raw)
}

/// The dotted access path a tool is exposed under inside the sandbox and in
/// catalog lines: `tools.<path>` — `github.list_prs` for namespaced tools,
/// the sanitized registry name for root tools.
pub(crate) fn tool_path(tool: &PtcToolInfo) -> String {
    match &tool.namespace {
        Some(ns) => format!("{}.{}", sanitize_ident(ns), method_name(tool)),
        None => sanitize_ident(&tool.name),
    }
}

// ── Identifier and JSDoc helpers ─────────────────────────────────────────────

/// Sanitize a name into a valid TS identifier: `[-. ]` become `_`, other
/// non-identifier characters are dropped, a leading digit gets a `_` prefix
/// and reserved words get a `_` suffix.
pub(crate) fn sanitize_ident(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '-' | '.' | ' ' => '_',
            _ => c,
        })
        .collect();
    s.retain(|c| c.is_ascii_alphanumeric() || c == '_');
    if s.is_empty() {
        return "_".to_string();
    }
    if s.starts_with(|c: char| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    if RESERVED.contains(&s.as_str()) {
        s.push('_');
    }
    s
}

/// Prepare a description for use inside a JSDoc comment: flatten newlines
/// to spaces and escape `*/` so the comment cannot be broken out of.
/// Returns `None` for empty descriptions (comment omitted entirely).
fn jsdoc(text: &str) -> Option<String> {
    let flattened: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.is_empty() {
        return None;
    }
    Some(flattened.replace("*/", "*\\/"))
}

// ── Schema rendering ─────────────────────────────────────────────────────────

/// Render a JSON Schema (or schema-like value) as a TS type expression.
///
/// `root` is the tool's full parameter schema used to resolve internal
/// `$ref` pointers; `depth` is the 1-based composite nesting depth;
/// `indent` is the indentation (in spaces) for members of multiline object
/// literals at this depth; `refs` tracks `$ref` pointers currently being
/// expanded for cycle detection.
fn render_type(
    schema: &Value,
    root: &Value,
    depth: usize,
    indent: usize,
    refs: &mut Vec<String>,
) -> String {
    match schema {
        // A `true` schema (or a missing/null one) accepts anything.
        Value::Bool(true) | Value::Null => "any".to_string(),
        Value::Bool(false) => "never".to_string(),
        Value::Object(obj) => render_schema_object(obj, root, depth, indent, refs),
        _ => "any".to_string(),
    }
}

fn render_schema_object(
    obj: &Map<String, Value>,
    root: &Value,
    depth: usize,
    indent: usize,
    refs: &mut Vec<String>,
) -> String {
    // Internal JSON Pointer $ref (`#/$defs/x`); anything else -> any.
    if let Some(Value::String(pointer)) = obj.get("$ref") {
        if !pointer.starts_with("#/") || depth > MAX_DEPTH {
            return "any".to_string();
        }
        if refs.iter().any(|r| r == pointer) {
            return "any".to_string(); // cyclic reference
        }
        let Some(target) = resolve_pointer(root, pointer) else {
            return "any".to_string();
        };
        refs.push(pointer.clone());
        let rendered = render_type(target, root, depth + 1, indent, refs);
        refs.pop();
        return rendered;
    }

    if let Some(variants) = union_variants(obj, "anyOf").or_else(|| union_variants(obj, "oneOf")) {
        if variants.is_empty() {
            return "any".to_string();
        }
        let parts: Vec<String> = variants
            .iter()
            .map(|s| render_type(s, root, depth, indent, refs))
            .collect();
        return format!("({})", parts.join(" | "));
    }
    if let Some(variants) = union_variants(obj, "allOf") {
        if variants.is_empty() {
            return "any".to_string();
        }
        let parts: Vec<String> = variants
            .iter()
            .map(|s| render_type(s, root, depth, indent, refs))
            .collect();
        return format!("({})", parts.join(" & "));
    }

    if let Some(literals) = enum_literals(obj.get("enum")) {
        return literals.join(" | ");
    }
    if let Some(literal) = obj.get("const").and_then(literal) {
        return literal;
    }
    match obj.get("type") {
        Some(Value::String(t)) => match t.as_str() {
            "string" => "string".to_string(),
            "integer" | "number" => "number".to_string(),
            "boolean" => "boolean".to_string(),
            "null" => "null".to_string(),
            "object" => render_object_type(obj, root, depth, indent, refs),
            "array" => render_array_type(obj, root, depth, indent, refs),
            _ => "any".to_string(),
        },
        // `["string", "null"]` style type lists -> parenthesized union.
        Some(Value::Array(types)) if !types.is_empty() => {
            let parts: Vec<String> = types
                .iter()
                .filter_map(|t| t.as_str())
                .map(|t| match t {
                    "string" => "string".to_string(),
                    "integer" | "number" => "number".to_string(),
                    "boolean" => "boolean".to_string(),
                    "null" => "null".to_string(),
                    _ => "any".to_string(),
                })
                .collect();
            if parts.is_empty() {
                "any".to_string()
            } else if parts.len() == 1 {
                parts.join(" | ")
            } else {
                format!("({})", parts.join(" | "))
            }
        }
        // No explicit type but properties/additionalProperties: treat as object.
        _ if obj.contains_key("properties") || obj.contains_key("additionalProperties") => {
            render_object_type(obj, root, depth, indent, refs)
        }
        _ => "any".to_string(),
    }
}

fn union_variants<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a [Value]> {
    match obj.get(key) {
        Some(Value::Array(items)) if !items.is_empty() => Some(items),
        _ => None,
    }
}

/// Render enum members as a literal union; `None` (caller falls back to
/// `any`) when any member is not a primitive.
fn enum_literals(value: Option<&Value>) -> Option<Vec<String>> {
    let items = value?.as_array()?;
    if items.is_empty() {
        return None;
    }
    items.iter().map(literal).collect()
}

/// A single enum/const literal: strings become JSON-quoted (and escaped)
/// string literals, numbers/booleans/null are printed verbatim.
/// `pub(crate)` for `describe.rs` example synthesis.
pub(crate) fn literal(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => serde_json::to_string(s).ok(),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some("null".to_string()),
        _ => None,
    }
}

fn render_object_type(
    obj: &Map<String, Value>,
    root: &Value,
    depth: usize,
    indent: usize,
    refs: &mut Vec<String>,
) -> String {
    if depth > MAX_DEPTH {
        return "any".to_string();
    }
    let properties = obj
        .get("properties")
        .and_then(Value::as_object)
        .map(|m| m.iter().collect::<Vec<_>>())
        .unwrap_or_default();
    let required: BTreeSet<&str> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    struct Member {
        doc: Option<String>,
        line: String,
        multiline: bool,
    }

    let mut members: Vec<Member> = Vec::new();
    for (key, prop) in &properties {
        let ty = render_type(prop, root, depth + 1, indent + 2, refs);
        let multiline = ty.contains('\n');
        let doc = prop
            .get("description")
            .and_then(Value::as_str)
            .and_then(jsdoc);
        let has_doc = doc.is_some();
        let optional = if required.contains(key.as_str()) {
            ""
        } else {
            "?"
        };
        let name = sanitize_ident(key);
        members.push(Member {
            doc,
            line: format!("{name}{optional}: {ty};"),
            multiline: multiline || has_doc,
        });
    }

    // `additionalProperties: <schema>` -> index signature member.
    match obj.get("additionalProperties") {
        Some(Value::Bool(true)) | None => {}
        Some(Value::Bool(false)) => {}
        Some(ap) => {
            let ty = render_type(ap, root, depth + 1, indent + 2, refs);
            members.push(Member {
                doc: None,
                line: format!("[key: string]: {ty};"),
                multiline: ty.contains('\n'),
            });
        }
    }

    if properties.is_empty()
        && obj
            .get("additionalProperties")
            .is_none_or(Value::is_boolean)
    {
        // No declared properties: a plain string map.
        return "Record<string, any>".to_string();
    }

    let inline = members.len() <= 1 && members.iter().all(|m| !m.multiline);
    if inline {
        let body = members
            .iter()
            .map(|m| m.line.trim_end_matches(';'))
            .collect::<Vec<_>>()
            .join(", ");
        return format!("{{ {body} }}");
    }
    let pad = " ".repeat(indent);
    let mut out = String::from("{\n");
    for m in &members {
        if let Some(doc) = &m.doc {
            out.push_str(&format!("{pad}/** {doc} */\n"));
        }
        out.push_str(&format!("{pad}{}\n", m.line));
    }
    out.push_str(&format!("{}}}", " ".repeat(indent.saturating_sub(2))));
    out
}

fn render_array_type(
    obj: &Map<String, Value>,
    root: &Value,
    depth: usize,
    indent: usize,
    refs: &mut Vec<String>,
) -> String {
    if depth > MAX_DEPTH {
        return "any".to_string();
    }
    let prefix: Option<Vec<String>> = obj
        .get("prefixItems")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .map(|items| {
            items
                .iter()
                .map(|s| render_type(s, root, depth + 1, indent + 2, refs))
                .collect()
        });
    let items = obj
        .get("items")
        .filter(|v| !matches!(v, Value::Bool(false)));

    match (prefix, items) {
        (Some(prefix), Some(items)) => {
            let rest = render_type(items, root, depth + 1, indent + 2, refs);
            format!("[{}, ...{}[]]", prefix.join(", "), array_wrap(&rest))
        }
        (Some(prefix), None) => format!("[{}]", prefix.join(", ")),
        (None, Some(items)) => {
            let ty = render_type(items, root, depth + 1, indent + 2, refs);
            format!("{}[]", array_wrap(&ty))
        }
        (None, None) => "any[]".to_string(),
    }
}

/// Wrap an element type in parentheses when needed before `[]`
/// (unions/intersections/optional members would bind wrongly otherwise).
fn array_wrap(ty: &str) -> String {
    if ty.contains(' ') || ty.contains('|') || ty.contains('&') {
        format!("({ty})")
    } else {
        ty.to_string()
    }
}

/// Resolve an internal JSON Pointer (`#/properties/a`) against the root
/// schema, unescaping `~1` (`/`) and `~0` (`~`).
fn resolve_pointer<'a>(root: &'a Value, pointer: &str) -> Option<&'a Value> {
    let mut current = root;
    for raw in pointer.trim_start_matches("#/").split('/') {
        let token = raw.replace("~1", "/").replace("~0", "~");
        current = current.as_object()?.get(&token)?;
    }
    Some(current)
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

    fn namespaced(ns: &str, name: &str, description: &str, parameters: Value) -> PtcToolInfo {
        PtcToolInfo {
            name: name.to_string(),
            namespace: Some(ns.to_string()),
            description: description.to_string(),
            parameters,
        }
    }

    fn obj(map: serde_json::Map<String, Value>) -> Value {
        Value::Object(map)
    }

    #[test]
    fn plan_sample_root_and_namespace() {
        let read_file = tool(
            "read_file",
            "Read a file from the workspace",
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        );
        let list_prs = namespaced(
            "github",
            "github_list_prs",
            "List pull requests",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "state": { "type": "string", "enum": ["open", "closed", "all"] },
                    "limit": { "type": "integer" }
                }
            }),
        );
        assert_eq!(
            generate_declarations(&[read_file, list_prs]),
            "declare const tools: {\n  \
/** Read a file from the workspace */\n  \
read_file: (input: { path: string }) => Promise<any>;\n  \
github: {\n    \
/** List pull requests */\n    \
list_prs: (input: {\n      \
limit?: number;\n      \
state?: \"open\" | \"closed\" | \"all\";\n    \
}) => Promise<any>;\n  \
};\n\
};"
        );
    }

    #[test]
    fn no_schema_or_true_schema_is_any() {
        let a = tool("a", "", Value::Null);
        let b = tool(
            "b",
            "",
            serde_json::json!({ "type": "object", "properties": { "x": true } }),
        );
        assert_eq!(
            generate_declarations(&[a, b]),
            "declare const tools: {\n  \
a: (input: any) => Promise<any>;\n  \
b: (input: { x?: any }) => Promise<any>;\n\
};"
        );
    }

    #[test]
    fn empty_object_is_record() {
        let t = tool(
            "t",
            "",
            serde_json::json!({ "type": "object", "properties": {} }),
        );
        assert_eq!(
            generate_declarations(&[t]),
            "declare const tools: {\n  t: (input: Record<string, any>) => Promise<any>;\n};"
        );
    }

    #[test]
    fn any_of_and_one_of_are_parenthesized_unions() {
        let t = tool(
            "t",
            "",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "target": { "anyOf": [{ "type": "string" }, { "type": "number" }] },
                    "mode": { "oneOf": [{ "const": "fast" }, { "const": "slow" }] }
                }
            }),
        );
        assert_eq!(
            generate_declarations(&[t]),
            "declare const tools: {\n  \
t: (input: {\n    \
mode?: (\"fast\" | \"slow\");\n    \
target?: (string | number);\n  \
}) => Promise<any>;\n\
};"
        );
    }

    #[test]
    fn nested_three_levels_render_fourth_becomes_any() {
        let three = tool(
            "three",
            "",
            serde_json::json!({ "type": "object", "properties": {
                "a": { "type": "object", "properties": {
                    "b": { "type": "object", "properties": {
                        "c": { "type": "string" } } } } } },
                "required": ["a"] }),
        );
        assert_eq!(
            generate_declarations(&[three]),
            "declare const tools: {\n  \
three: (input: { a: { b?: { c?: string } } }) => Promise<any>;\n\
};"
        );

        let four = tool(
            "four",
            "",
            serde_json::json!({ "type": "object", "properties": {
                "a": { "type": "object", "properties": {
                    "b": { "type": "object", "properties": {
                        "c": { "type": "object", "properties": {
                            "d": { "type": "string" } } } } } } } },
                "required": ["a"] }),
        );
        assert_eq!(
            generate_declarations(&[four]),
            "declare const tools: {\n  \
four: (input: { a: { b?: { c?: any } } }) => Promise<any>;\n\
};"
        );
    }

    #[test]
    fn ref_cycle_becomes_any() {
        let mut props = serde_json::Map::new();
        props.insert(
            "next".to_string(),
            serde_json::json!({ "$ref": "#/$defs/node" }),
        );
        let schema = obj(
            [
                ("type".to_string(), Value::String("object".into())),
                ("properties".to_string(), Value::Object(props)),
                (
                    "$defs".to_string(),
                    serde_json::json!({ "node": {
                        "type": "object",
                        "properties": { "next": { "$ref": "#/$defs/node" }, "name": { "type": "string" } },
                        "required": ["name"]
                    } }),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let t = tool("t", "", schema);
        // depth 1 input -> $defs/node (depth 2) -> node's `next` $ref at depth 4
        // (over the nest limit AND a cycle) -> any; `name` stays string.
        assert_eq!(
            generate_declarations(&[t]),
            "declare const tools: {\n  \
t: (input: {\n    \
next?: {\n      \
name: string;\n      \
next?: any;\n    \
};\n  \
}) => Promise<any>;\n\
};"
        );
    }

    #[test]
    fn escapes_jsdoc_and_flattens_newlines() {
        let t = tool("t", "evil */ comment\nsecond line", Value::Null);
        assert_eq!(
            generate_declarations(&[t]),
            "declare const tools: {\n  \
/** evil *\\/ comment second line */\n  \
t: (input: any) => Promise<any>;\n\
};"
        );
    }

    #[test]
    fn sanitizes_identifiers() {
        assert_eq!(sanitize_ident("fetch-agents"), "fetch_agents");
        assert_eq!(sanitize_ident("delete"), "delete_");
        assert_eq!(sanitize_ident("123abc"), "_123abc");
        assert_eq!(sanitize_ident("a.b c"), "a_b_c");
        assert_eq!(sanitize_ident("weird:name!"), "weirdname");
        assert_eq!(sanitize_ident("for"), "for_");
    }

    #[test]
    fn namespace_groups_sorted_members_sorted() {
        let z_first = namespaced("zeta", "zeta_a", "in zeta", Value::Null);
        let b_tool = namespaced("alpha", "alpha_b", "b tool", Value::Null);
        let a_tool = namespaced("alpha", "alpha_a", "a tool", Value::Null);
        let root = tool("zzz", "root tool", Value::Null);
        // The `{namespace}_` prefix is stripped from registry names inside
        // groups (github_list_prs -> list_prs, alpha_a -> a).
        assert_eq!(
            generate_declarations(&[z_first, b_tool, a_tool, root]),
            "declare const tools: {\n  \
/** root tool */\n  \
zzz: (input: any) => Promise<any>;\n  \
alpha: {\n    \
/** a tool */\n    \
a: (input: any) => Promise<any>;\n    \
/** b tool */\n    \
b: (input: any) => Promise<any>;\n  \
};\n  \
zeta: {\n    \
/** in zeta */\n    \
a: (input: any) => Promise<any>;\n  \
};\n\
};"
        );
    }

    #[test]
    fn field_descriptions_become_jsdoc() {
        let t = tool(
            "t",
            "",
            serde_json::json!({ "type": "object", "properties": {
                "path": { "type": "string", "description": "File path" }
            }, "required": ["path"] }),
        );
        assert_eq!(
            generate_declarations(&[t]),
            "declare const tools: {\n  \
t: (input: {\n    \
/** File path */\n    \
path: string;\n  \
}) => Promise<any>;\n\
};"
        );
    }

    #[test]
    fn additional_properties_index_signature() {
        let t = tool(
            "t",
            "",
            serde_json::json!({ "type": "object", "properties": {
                "kind": { "type": "string" }
            }, "additionalProperties": { "type": "number" } }),
        );
        assert_eq!(
            generate_declarations(&[t]),
            "declare const tools: {\n  \
t: (input: {\n    \
kind?: string;\n    \
[key: string]: number;\n  \
}) => Promise<any>;\n\
};"
        );
    }

    #[test]
    fn false_schema_is_never() {
        let t = tool(
            "t",
            "",
            serde_json::json!({ "type": "object", "properties": {
                "x": { "type": "string" }, "never_field": false
            }, "required": ["x", "never_field"] }),
        );
        assert_eq!(
            generate_declarations(&[t]),
            "declare const tools: {\n  \
t: (input: {\n    \
never_field: never;\n    \
x: string;\n  \
}) => Promise<any>;\n\
};"
        );
    }

    #[test]
    fn arrays_items_and_prefix_items() {
        let t = tool(
            "t",
            "",
            serde_json::json!({ "type": "object", "properties": {
                "tags": { "type": "array", "items": { "type": "string" } },
                "pair": { "type": "array", "prefixItems": [{ "type": "string" }, { "type": "number" }] },
                "mix": { "type": "array", "prefixItems": [{ "type": "string" }],
                         "items": { "type": "boolean" } },
                "unions": { "type": "array", "items": { "enum": ["a", "b"] } }
            }, "required": ["tags", "pair", "mix", "unions"] }),
        );
        assert_eq!(
            generate_declarations(&[t]),
            "declare const tools: {\n  \
t: (input: {\n    \
mix: [string, ...boolean[]];\n    \
pair: [string, number];\n    \
tags: string[];\n    \
unions: (\"a\" | \"b\")[];\n  \
}) => Promise<any>;\n\
};"
        );
    }

    #[test]
    fn external_ref_is_any() {
        let t = tool(
            "t",
            "",
            serde_json::json!({ "type": "object", "properties": {
                "x": { "$ref": "https://example.com/schema.json" } } }),
        );
        assert_eq!(
            generate_declarations(&[t]),
            "declare const tools: {\n  t: (input: { x?: any }) => Promise<any>;\n};"
        );
    }

    #[test]
    fn single_declaration_matches_grouped_rendering() {
        let list_prs = namespaced(
            "github",
            "github_list_prs",
            "List pull requests",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "state": { "type": "string", "enum": ["open", "closed", "all"] },
                    "limit": { "type": "integer" }
                }
            }),
        );
        assert_eq!(
            single_declaration(&list_prs),
            "declare const tools: {\n  \
github: {\n    \
/** List pull requests */\n    \
list_prs: (input: {\n      \
limit?: number;\n      \
state?: \"open\" | \"closed\" | \"all\";\n    \
}) => Promise<any>;\n  \
};\n\
};"
        );
        // Same bytes as a one-tool generate_declarations call:
        assert_eq!(
            single_declaration(&list_prs),
            generate_declarations(std::slice::from_ref(&list_prs))
        );

        let root = tool("read_file", "Read a file", Value::Null);
        assert_eq!(
            single_declaration(&root),
            "declare const tools: {\n  /** Read a file */\n  read_file: (input: any) => Promise<any>;\n};"
        );
    }

    #[test]
    fn catalog_lines_sorted_with_first_sentences() {
        let tools = vec![
            tool("read_file", "Read a file. More.", Value::Null),
            namespaced(
                "github",
                "github_list_prs",
                "List pull requests",
                Value::Null,
            ),
            namespaced("zeta", "zeta_a", "  ", Value::Null),
        ];
        assert_eq!(
            catalog_lines(&tools),
            "github.list_prs: List pull requests\nread_file: Read a file\nzeta.a"
        );
    }

    #[test]
    fn mixed_declarations_demote_to_comments() {
        let read_file = tool(
            "read_file",
            "Read a file",
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        );
        let list_prs = namespaced(
            "github",
            "github_list_prs",
            "List pull requests",
            Value::Null,
        );
        let get_file = namespaced("github", "github_get_file", "Get a file", Value::Null);
        let mut demote = std::collections::HashSet::new();
        demote.insert("github_list_prs");
        let (mixed, count) =
            generate_declarations_mixed(&[read_file.clone(), list_prs, get_file], &demote);
        assert_eq!(count, 1);
        assert_eq!(
            mixed,
            "declare const tools: {\n  \
/** Read a file */\n  \
read_file: (input: { path: string }) => Promise<any>;\n  \
github: {\n    \
/** Get a file */\n    \
get_file: (input: any) => Promise<any>;\n  \
};\n  \
// github.list_prs: List pull requests\n\
};"
        );
        // Nothing demoted -> identical to the full render.
        let (full, zero) = generate_declarations_mixed(
            &[
                read_file,
                namespaced(
                    "github",
                    "github_list_prs",
                    "List pull requests",
                    Value::Null,
                ),
                namespaced("github", "github_get_file", "Get a file", Value::Null),
            ],
            &std::collections::HashSet::new(),
        );
        assert_eq!(zero, 0);
        assert_eq!(
            full,
            generate_declarations(&[
                tool(
                    "read_file",
                    "Read a file",
                    serde_json::json!({
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                        "required": ["path"]
                    })
                ),
                namespaced(
                    "github",
                    "github_list_prs",
                    "List pull requests",
                    Value::Null
                ),
                namespaced("github", "github_get_file", "Get a file", Value::Null),
            ])
        );
    }
}
