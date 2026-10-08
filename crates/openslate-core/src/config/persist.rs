//! Format-preserving config write-back (model-mgmt-1).
//!
//! Purely mechanical primitives over `toml_edit`: upsert/remove provider and
//! model entries, set/remove level mappings, and upsert `.env` key-values.
//! Everything else in the file — comments (including non-ASCII), whitespace,
//! ordering, unrelated tables — round-trips byte-for-byte; only the upserted
//! table body is rebuilt. Config structs stay `Deserialize`-only by design;
//! these helpers hand-construct the TOML rows.
//!
//! Idempotency: calling any upsert twice produces the same file content.
//!
//! Removal semantics: `remove_*` targets (or their whole section) that do
//! not exist are a **silent success** — removal is idempotent and callers
//! (the future TUI layer) own reference-integrity checks.
//!
//! Provider/model upsert semantics: the sub-table body is rebuilt from the
//! struct — required fields always written; defaulted optional fields are
//! omitted (`adapter` when `None`, `max_attempts`/`retry_base_ms` at their
//! serde defaults 3/500, `supports_tool_call` when `true`,
//! `supports_vision`/`supports_reasoning` when `false`, `Option` fields when
//! `None`). Comments inside the upserted table body are therefore not
//! preserved; comments everywhere else are.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use toml_edit::{value, DocumentMut, Item, Table};

/// serde defaults for `ProviderConfig` (config/mod.rs). Optional retry
/// fields are only written back when they differ from these.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;
const DEFAULT_RETRY_BASE_MS: u64 = 500;

/// Comment placed above a newly created `[levels]` table.
const LEVELS_HEADER_COMMENT: &str =
    "# Model levels: level name -> model library entry ([models] key)\n";

// ── TOML helpers ─────────────────────────────────────────────────────────────

/// Load the document at `path`, or an empty document when the file does not
/// exist yet (the upsert then creates a minimal file holding just the new
/// table).
fn load_document(path: &Path) -> Result<DocumentMut> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file '{}'", path.display()))?;
    content
        .parse::<DocumentMut>()
        .map_err(|e| anyhow::anyhow!("Failed to parse TOML '{}': {e}", path.display()))
}

/// Write the document back, creating parent directories as needed.
fn save_document(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create config directory '{}'", parent.display())
            })?;
        }
    }
    fs::write(path, doc.to_string())
        .with_context(|| format!("Failed to write config file '{}'", path.display()))
}

/// Whether the serialized document currently has any content (used to decide
/// whether a newly created table needs a separating newline before it).
fn doc_has_content(doc: &DocumentMut) -> bool {
    !doc.to_string().is_empty()
}

/// Peek whether `section[name]` already holds a standard table (as opposed
/// to being absent or a non-table value).
fn sub_table_exists(doc: &DocumentMut, section: &str, name: &str) -> bool {
    doc.as_table()
        .get(section)
        .and_then(|item| item.as_table())
        .and_then(|table| table.get(name))
        .is_some_and(|item| item.is_table())
}

/// Ensure `section` (and `section[name]`) are standard tables and hand the
/// sub-table to `fill`, which rebuilds its body. A pre-existing table keeps
/// its own decor (comments above the header) and position; only its
/// key-values are replaced. A newly created section is marked implicit so a
/// fresh file renders just `[section.name]`; a newly created sub-table gets
/// a blank-line prefix when the document already has content.
///
/// Tables are built explicitly with `Table::new()` — the `IndexMut`
/// auto-creation path yields dotted-key style tables, which would rewrite
/// the whole document shape instead of appending a `[section.name]` header.
/// An existing section/entry that is NOT a standard table (e.g. hand-written
/// inline syntax) is refused with a clear error rather than overwritten.
fn upsert_sub_table<F>(doc: &mut DocumentMut, section: &str, name: &str, fill: F) -> Result<()>
where
    F: FnOnce(&mut Table),
{
    let existed = sub_table_exists(doc, section, name);
    let has_content = doc_has_content(doc);

    match doc.as_table().get(section) {
        None => {}
        Some(item) if item.is_table() => {}
        Some(_) => {
            anyhow::bail!(
                "config section [{section}] is not a standard TOML table; \
                 refusing to rewrite it"
            )
        }
    }
    if let Some(item) = doc
        .as_table()
        .get(section)
        .and_then(|table| table.get(name))
    {
        if !item.is_table() {
            anyhow::bail!(
                "config entry '{name}' in [{section}] is not a standard TOML \
                 table; refusing to overwrite it"
            )
        }
    }

    let root = doc.as_table_mut();
    if !root.get(section).is_some_and(|item| item.is_table()) {
        let mut fresh = Table::new();
        fresh.set_implicit(true);
        root.insert(section, Item::Table(fresh));
    }
    let section_table = root
        .get_mut(section)
        .and_then(|item| item.as_table_mut())
        .expect("section was just ensured to be a table");
    if !section_table.get(name).is_some_and(|item| item.is_table()) {
        section_table.insert(name, Item::Table(Table::new()));
    }
    let table = section_table
        .get_mut(name)
        .and_then(|item| item.as_table_mut())
        .expect("sub-table was just ensured to be a table");

    if !existed && has_content {
        table.decor_mut().set_prefix("\n");
    }

    // Rebuild the body wholesale (stale optional fields must not linger),
    // preserving the table's own header decor.
    let keys: Vec<String> = table.iter().map(|(k, _)| k.to_owned()).collect();
    for key in keys {
        table.remove(&key);
    }
    fill(table);
    Ok(())
}

// ── Provider / model upserts ─────────────────────────────────────────────────

/// Insert or replace provider `name` in the `[providers]` table of the
/// config file at `path` (created when missing). See the module docs for
/// preservation and idempotency guarantees.
pub fn upsert_provider(path: &Path, name: &str, cfg: &super::ProviderConfig) -> Result<()> {
    let mut doc = load_document(path)?;
    upsert_sub_table(&mut doc, "providers", name, |table| {
        table["base_url"] = value(cfg.base_url.clone());
        table["api_key_env"] = value(cfg.api_key_env.clone());
        if let Some(adapter) = &cfg.adapter {
            table["adapter"] = value(adapter.clone());
        }
        if let Some(title) = &cfg.title {
            table["title"] = value(title.clone());
        }
        if cfg.max_attempts != DEFAULT_MAX_ATTEMPTS {
            table["max_attempts"] = value(i64::from(cfg.max_attempts));
        }
        if cfg.retry_base_ms != DEFAULT_RETRY_BASE_MS {
            table["retry_base_ms"] = value(cfg.retry_base_ms as i64);
        }
    })?;
    save_document(path, &doc)
}

/// Insert or replace model entry `entry` in the `[models]` table of the
/// config file at `path` (created when missing). See the module docs for
/// preservation and idempotency guarantees.
pub fn upsert_model(path: &Path, entry: &str, cfg: &super::ModelConfig) -> Result<()> {
    let mut doc = load_document(path)?;
    upsert_sub_table(&mut doc, "models", entry, |table| {
        table["provider"] = value(cfg.provider.clone());
        table["model"] = value(cfg.model.clone());
        if let Some(tokens) = cfg.max_context_tokens {
            table["max_context_tokens"] = value(i64::from(tokens));
        }
        if let Some(tokens) = cfg.max_output_tokens {
            table["max_output_tokens"] = value(i64::from(tokens));
        }
        // supports_tool_call defaults to true — only the non-default is
        // written.
        if !cfg.supports_tool_call {
            table["supports_tool_call"] = value(false);
        }
        if cfg.supports_vision {
            table["supports_vision"] = value(true);
        }
        if cfg.supports_reasoning {
            table["supports_reasoning"] = value(true);
        }
        if let Some(price) = cfg.input_price_per_mtok {
            table["input_price_per_mtok"] = value(price);
        }
        if let Some(price) = cfg.output_price_per_mtok {
            table["output_price_per_mtok"] = value(price);
        }
    })?;
    save_document(path, &doc)
}

// ── Level mappings ───────────────────────────────────────────────────────────

/// Set `levels.<level> = <entry>` in the config file at `path`. The
/// `[levels]` table is created (with a comment header) when missing. An
/// existing `[levels]` that is not a standard table is refused rather than
/// overwritten.
pub fn set_level(path: &Path, level: &str, entry: &str) -> Result<()> {
    let mut doc = load_document(path)?;
    let created = match doc.as_table().get("levels") {
        None => true,
        Some(item) if item.is_table() => false,
        Some(_) => {
            anyhow::bail!(
                "config section [levels] is not a standard TOML table; \
                 refusing to rewrite it"
            )
        }
    };
    if created {
        let mut fresh = Table::new();
        fresh.decor_mut().set_prefix(LEVELS_HEADER_COMMENT);
        doc.as_table_mut().insert("levels", Item::Table(fresh));
    }
    // An existing [levels] table keeps its decor untouched (zero disturbance).
    doc.as_table_mut()
        .get_mut("levels")
        .and_then(|item| item.as_table_mut())
        .expect("levels table was just ensured")
        .insert(level, value(entry));
    save_document(path, &doc)
}

/// Remove `levels.<level>`. Missing level or missing `[levels]` section is a
/// silent success — the file is left untouched (idempotent, no stray empty
/// files).
pub fn remove_level(path: &Path, level: &str) -> Result<()> {
    let mut doc = load_document(path)?;
    let Some(levels) = doc
        .as_table_mut()
        .get_mut("levels")
        .and_then(|item| item.as_table_mut())
    else {
        return Ok(());
    };
    if levels.remove(level).is_none() {
        return Ok(());
    }
    save_document(path, &doc)
}

// ── Capabilities（功能 → 代号）──────────────────────────────────────────

/// Comment placed above a newly created `[capabilities]` table.
const CAPABILITIES_HEADER_COMMENT: &str =
    "# Capability -> level mapping: main (主对话) / compact (上下文压缩) / title (标题生成)\n";

/// Set `capabilities.<cap> = <alias>` in the config file at `path`. The
/// `[capabilities]` table is created (with a comment header) when missing.
/// An existing `[capabilities]` that is not a standard table is refused
/// rather than overwritten. Semantics mirror [`set_level`].
pub fn set_capability(path: &Path, cap: &str, alias: &str) -> Result<()> {
    let mut doc = load_document(path)?;
    let created = match doc.as_table().get("capabilities") {
        None => true,
        Some(item) if item.is_table() => false,
        Some(_) => {
            anyhow::bail!(
                "config section [capabilities] is not a standard TOML table; \
                 refusing to rewrite it"
            )
        }
    };
    if created {
        let mut fresh = Table::new();
        fresh.decor_mut().set_prefix(CAPABILITIES_HEADER_COMMENT);
        doc.as_table_mut().insert("capabilities", Item::Table(fresh));
    }
    // An existing [capabilities] table keeps its decor untouched.
    doc.as_table_mut()
        .get_mut("capabilities")
        .and_then(|item| item.as_table_mut())
        .expect("capabilities table was just ensured")
        .insert(cap, value(alias));
    save_document(path, &doc)
}

/// Remove model entry `entry` from `[models]`. Missing entry or section is a
/// silent success (idempotent). Reference integrity (levels/agents pointing
/// at the entry) is the caller's responsibility.
pub fn remove_model_entry(path: &Path, entry: &str) -> Result<()> {
    remove_sub_key(path, "models", entry)
}

/// Remove provider `name` from `[providers]`. Missing provider or section is
/// a silent success (idempotent). Reference integrity (models pointing at
/// the provider) is the caller's responsibility.
pub fn remove_provider(path: &Path, name: &str) -> Result<()> {
    remove_sub_key(path, "providers", name)
}

// ── MCP servers（两层嵌套 [mcp.servers.<name>]）─────────────────────────────

/// Insert or replace an HTTP MCP server entry in `[mcp.servers.<name>]`
/// (created when missing; parent `[mcp]`/`[mcp.servers]` created as needed).
/// Semantics mirror the provider upsert: the sub-table body is rebuilt from
/// the arguments, everything else round-trips. Idempotent.
///
/// Writes `transport = "http"` + `url` (+ optional `[mcp.servers.<name>.headers]`
/// sub-table), matching `TransportConfig::Http`'s internally-tagged serde
/// shape. The connection itself is established on next runtime start (MCP
/// tools are listed at connect time — no hot-reload by design).
pub fn upsert_mcp_server(
    path: &Path,
    name: &str,
    url: &str,
    headers: Option<&std::collections::HashMap<String, String>>,
) -> Result<()> {
    let mut doc = load_document(path)?;
    // 拒绝把非表节点踩掉（与 upsert_sub_table 同守卫语义）。
    for section in ["mcp", "servers"] {
        if let Some(item) = doc.as_table().get(section) {
            anyhow::ensure!(
                item.is_table(),
                "config entry '{section}' is not a standard TOML table"
            );
        }
    }
    if let Some(item) = doc.as_table().get("mcp").and_then(|t| t.get("servers")).and_then(|t| t.get(name)) {
        anyhow::ensure!(
            item.is_table(),
            "config entry 'mcp.servers.{name}' is not a standard TOML table"
        );
    }

    let mcp = doc
        .as_table_mut()
        .entry("mcp")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .context("[mcp] is not a table")?;
    let servers = mcp
        .entry("servers")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .context("[mcp.servers] is not a table")?;
    let entry = servers
        .entry(name)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .context("[mcp.servers.{name}] is not a table")?;
    entry["transport"] = value("http");
    entry["url"] = value(url.to_owned());
    entry.remove("headers");
    if let Some(map) = headers {
        if !map.is_empty() {
            let mut headers_table = Table::new();
            for (k, v) in map {
                headers_table[k.as_str()] = value(v.clone());
            }
            entry["headers"] = Item::Table(headers_table);
        }
    }
    save_document(path, &doc)
}

/// Remove `[mcp.servers.<name>]`. Missing targets are a silent success
/// (idempotent); refuses to touch non-table `mcp`/`servers` nodes.
pub fn remove_mcp_server(path: &Path, name: &str) -> Result<()> {
    let mut doc = load_document(path)?;
    let Some(mcp) = doc.as_table_mut().get_mut("mcp").and_then(|i| i.as_table_mut()) else {
        return Ok(());
    };
    let Some(servers) = mcp.get_mut("servers").and_then(|i| i.as_table_mut()) else {
        return Ok(());
    };
    if servers.remove(name).is_none() {
        return Ok(());
    }
    save_document(path, &doc)
}

/// Write/clear `[mcp].tool_name_template`: an empty `template` removes the
/// key (falling back to the built-in `{prefix}_{server}__{tool}`, via
/// [`remove_sub_key`]
/// semantics — missing key/section is a silent success and no stray file is
/// created); a `[mcp]` section that does not exist yet is created on write.
/// Value validation is the caller's job (core
/// `validation::validate_mcp_tool_name_template` — the FFI write path runs it
/// before persisting, and startup validation would catch anything hand-written
/// into the file). Refuses to step on a non-table `mcp` node (same guard as
/// [`upsert_mcp_server`]).
pub fn set_mcp_tool_name_template(path: &Path, template: &str) -> Result<()> {
    if template.is_empty() {
        return remove_sub_key(path, "mcp", "tool_name_template");
    }
    let mut doc = load_document(path)?;
    if let Some(item) = doc.as_table().get("mcp") {
        anyhow::ensure!(
            item.is_table(),
            "config entry 'mcp' is not a standard TOML table"
        );
    }
    let mcp = doc
        .as_table_mut()
        .entry("mcp")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .context("[mcp] is not a table")?;
    mcp.insert("tool_name_template", value(template.to_owned()));
    save_document(path, &doc)
}

/// Rename a key inside `[mcp.servers]` (the mobile aggregated-host scenario
/// has exactly one server, whose key is also the `{prefix}` placeholder's
/// expansion value). Semantics:
///
/// - exactly one key → that key is renamed (the caller's `old` may be stale
///   and is ignored — single-server is the invariant this targets);
/// - multiple keys → the key literally named `old` is renamed; missing `old`
///   is an error (refuse to guess which entry to touch);
/// - empty (or missing) `[mcp.servers]` → error (upsert first);
/// - `new ==` resolved source key → silent no-op (idempotent);
/// - `new` already exists as another key → error (refuse to clobber);
/// - the entry must be a standard table; anything else is refused.
///
/// The move is whole-`Item`: transport/headers values and their decor
/// (comments, blank lines) travel to the new key untouched — the render
/// walks the key path, so `[mcp.servers.old]` (and a nested
/// `[mcp.servers.old.headers]`) automatically come out as `...new...`.
/// Position note: `insert` appends at the table's end, so among multiple
/// servers the renamed one moves last — semantically irrelevant (TOML
/// tables are unordered). Server-name legality is the caller's job (FFI
/// runs `validation::validate_mcp_server_name` first).
pub fn rename_mcp_server(path: &Path, old: &str, new: &str) -> Result<()> {
    let mut doc = load_document(path)?;
    let Some(mcp) = doc.as_table_mut().get_mut("mcp").and_then(|i| i.as_table_mut()) else {
        anyhow::bail!(
            "[mcp] section is missing (or not a table): nothing to rename — \
             create the server first via upsert_mcp_server"
        );
    };
    let Some(servers) = mcp.get_mut("servers").and_then(|i| i.as_table_mut()) else {
        anyhow::bail!(
            "[mcp.servers] section is missing (or not a table): nothing to \
             rename — create the server first via upsert_mcp_server"
        );
    };
    // 解析要搬移的源键：单键直取（mobile 聚合场景的常态，调用方的
    // old 可能过期）；多键必须精确命中 old，改错无辜条目不如报错。
    let source = match servers.len() {
        0 => anyhow::bail!(
            "[mcp.servers] is empty: nothing to rename — create the server \
             first via upsert_mcp_server"
        ),
        1 => servers
            .iter()
            .next()
            // Table::iter() 的键即 &str（非 toml_edit::Key），直接 to_owned。
            .map(|(k, _)| k.to_owned())
            .expect("len == 1 guarantees a key"),
        _ => {
            if !servers.contains_key(old) {
                anyhow::bail!(
                    "[mcp.servers] has multiple keys and none is named '{old}': \
                     refusing to guess which one to rename"
                );
            }
            old.to_owned()
        }
    };
    // 幂等：目标名即源名（含调用方显式 old == new）→ 原样成功，不落盘。
    if source == new {
        return Ok(());
    }
    // 目标键已被占（多键场景才可能）→ 拒绝覆盖：静默合并会吞掉一个
    // server 的配置，且无法从改名操作里恢复。
    if servers.contains_key(new) {
        anyhow::bail!(
            "[mcp.servers.{new}] already exists: refusing to overwrite it \
             while renaming '{source}' (remove one of them first)"
        );
    }
    let item = servers
        .remove(&source)
        .expect("source key was just confirmed present");
    anyhow::ensure!(
        item.is_table(),
        "[mcp.servers.{source}] is not a standard TOML table; refusing to move it"
    );
    servers.insert(new, item);
    save_document(path, &doc)
}

/// Shared removal for `[<section>] <key>`. A missing section or key is a
/// silent success leaving the file untouched (idempotent, no stray empty
/// files); a section that is not a standard table is refused.
fn remove_sub_key(path: &Path, section: &str, key: &str) -> Result<()> {
    let mut doc = load_document(path)?;
    let Some(table) = doc
        .as_table_mut()
        .get_mut(section)
        .and_then(|item| item.as_table_mut())
    else {
        return Ok(());
    };
    if table.remove(key).is_none() {
        return Ok(());
    }
    save_document(path, &doc)
}

// ── .env upsert ──────────────────────────────────────────────────────────────

/// Insert or update `VAR=value` in `{config_dir}/.env`, creating the file
/// (and the directory) when missing. On Unix a newly created file gets mode
/// `0600`; an existing file keeps its permissions. When a `VAR=...` line
/// exists, that line is replaced in place (first occurrence); otherwise the
/// pair is appended (adding the missing trailing newline first). All other
/// bytes of the file are left untouched. Repeated calls with the same
/// arguments are idempotent.
///
/// `value` is written verbatim (no quoting) — callers must pass single-line
/// values. `var` must be a non-empty name without `=` or newlines.
pub fn upsert_env_key(config_dir: &Path, var: &str, value: &str) -> Result<()> {
    if var.is_empty() || var.contains('=') || var.contains('\n') {
        anyhow::bail!("invalid env var name: '{var}'");
    }
    if value.contains('\n') {
        anyhow::bail!("env value for '{var}' must be a single line");
    }

    let env_path = config_dir.join(".env");
    let content = if env_path.is_file() {
        fs::read_to_string(&env_path)
            .with_context(|| format!("Failed to read env file '{}'", env_path.display()))?
    } else {
        String::new()
    };

    let pair = format!("{var}={value}");
    let prefix = format!("{var}=");
    let mut out = String::with_capacity(content.len() + pair.len() + 2);
    let mut replaced = false;
    for line in content.split_inclusive('\n') {
        let bare = line.strip_suffix('\n').unwrap_or(line);
        let bare = bare.strip_suffix('\r').unwrap_or(bare);
        if !replaced && bare.starts_with(&prefix) {
            out.push_str(&pair);
            out.push('\n');
            replaced = true;
        } else {
            out.push_str(line);
        }
    }
    if !replaced {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&pair);
        out.push('\n');
    }

    fs::create_dir_all(config_dir)
        .with_context(|| format!("Failed to create config dir '{}'", config_dir.display()))?;
    write_env_file(&env_path, out.as_bytes())
}

/// Write the `.env` file, assigning mode `0600` when the file is created
/// (Unix); an existing file keeps its mode.
fn write_env_file(env_path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(env_path)
            .with_context(|| format!("Failed to open env file '{}'", env_path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("Failed to write env file '{}'", env_path.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(env_path, bytes)
            .with_context(|| format!("Failed to write env file '{}'", env_path.display()))?;
    }
    Ok(())
}

// ── Agent markdown frontmatter（工具可见性页）────────────────────────────────

/// Rewrite ONLY the `tools:` / `hidden_tools:` keys in an agent markdown
/// file's YAML frontmatter, leaving every other byte — other frontmatter
/// keys (order, spelling, comments), the closing `---`, and the whole
/// markdown body — untouched.
///
/// - An existing key (whatever shape: dash list, inline `[a, b]`, scalar) is
///   replaced wholesale by the new serialization: a 4-space-indented dash
///   list, or `key: []` for an empty list (bootstrap `agents/root.md` style).
/// - A missing `hidden_tools` key is inserted right after `tools`; a missing
///   `tools` key is inserted at the top of the frontmatter.
/// - List items are written unquoted when they match the plain-scalar-safe
///   tool-name/pattern charset, double-quoted otherwise (a bare `*glob`/`"`
///   would corrupt the YAML).
/// - A file without (or with unclosed) frontmatter is an error — this helper
///   never invents structure, it edits an existing agent file.
/// - Idempotent: calling it twice with the same arguments yields the same
///   file.
///
/// Callers own the reference-integrity questions (unknown tool names are
/// persisted as-is; the runner simply matches nothing for them).
pub fn set_agent_tool_lists(path: &Path, tools: &[String], hidden_tools: &[String]) -> Result<()> {
    for (kind, list) in [("tools", tools), ("hidden_tools", hidden_tools)] {
        for item in list {
            if item.contains('\n') || item.contains('\r') {
                anyhow::bail!("{kind} entry must be a single line: {item:?}");
            }
        }
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read agent file '{}'", path.display()))?;

    // Same frontmatter split as `parse_agent_markdown` (BOM tolerated,
    // preserved on write-back).
    let (bom, rest) = match content.strip_prefix('\u{feff}') {
        Some(r) => ("\u{feff}", r),
        None => ("", content.as_str()),
    };
    let after_open = rest.strip_prefix("---").ok_or_else(|| {
        anyhow::anyhow!("agent file '{}': no frontmatter delimiter", path.display())
    })?;
    let close = after_open.find("\n---").ok_or_else(|| {
        anyhow::anyhow!("agent file '{}': unclosed frontmatter (missing closing ---)", path.display())
    })?;
    let frontmatter = &after_open[..close];
    let body = &after_open[close + "\n---".len()..];

    let lines: Vec<&str> = frontmatter.split_inclusive('\n').collect();
    // Locate each top-level key block: the key line plus every following
    // indented / blank line (nested list items, nested tables). A column-0
    // comment stops the block so comments above the next key survive.
    let mut blocks: Vec<(&str, usize, usize)> = Vec::new(); // (key, start, end_exclusive)
    let mut i = 0usize;
    while i < lines.len() {
        if let Some(key) = top_level_key(lines[i]) {
            let start = i;
            i += 1;
            while i < lines.len() {
                let line = lines[i].trim_end_matches(['\n', '\r']);
                if line.starts_with(' ')
                    || line.starts_with('\t')
                    || line.is_empty()
                    || line.starts_with("- ")
                    || line == "-"
                {
                    // 缩进续行 / 空行 / 列 0 序列项（YAML 允许列表与父键同
                    // 缩进——`tools:\n- a\n- b`）。列 0 注释不属于块（保留
                    // 在原位，通常是下一个键的说明）。
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push((key, start, i));
        } else {
            i += 1;
        }
    }

    let tools_block = yaml_key_block("tools", tools);
    let hidden_block = yaml_key_block("hidden_tools", hidden_tools);

    // Ranges to drop = the old tools/hidden blocks; the fresh pair is written
    // at the start of the first dropped block so `hidden_tools` always ends
    // up right after `tools`. Lines outside every block (leading blank line,
    // column-0 comments between keys) are copied verbatim.
    let drop_ranges: Vec<(usize, usize)> = blocks
        .iter()
        .filter(|(k, ..)| *k == "tools" || *k == "hidden_tools")
        .map(|(_, s, e)| (*s, *e))
        .collect();
    let insert_at = drop_ranges.first().map(|(s, _)| *s).or_else(|| {
        // Neither key existed: anchor at the first recognized key (keeping
        // any leading blank/comment lines in place), or the very top when
        // the frontmatter holds no keys at all.
        blocks.first().map(|(_, s, _)| *s)
    });

    let mut out =
        String::with_capacity(frontmatter.len() + tools_block.len() + hidden_block.len());
    for (i, line) in lines.iter().enumerate() {
        if insert_at == Some(i) {
            out.push_str(&tools_block);
            out.push_str(&hidden_block);
        }
        if !drop_ranges.iter().any(|(s, e)| *s <= i && i < *e) {
            out.push_str(line);
        }
    }
    if insert_at.is_none() {
        // Keyless frontmatter: the pair becomes its whole content.
        out.push_str(&tools_block);
        out.push_str(&hidden_block);
    }

    let mut file = String::with_capacity(bom.len() + 4 + out.len() + 4 + body.len());
    file.push_str(bom);
    file.push_str("---");
    file.push_str(&out);
    file.push_str("\n---");
    file.push_str(body);
    fs::write(path, file)
        .with_context(|| format!("Failed to write agent file '{}'", path.display()))
}

/// Top-level YAML key of a frontmatter line (`name: …`), or `None` for
/// indented continuation lines, comments, blank lines, and anything that is
/// not a `key:` at column 0.
fn top_level_key(line: &str) -> Option<&str> {
    let bare = line.trim_end_matches(['\n', '\r']);
    let first = bare.chars().next()?;
    if first.is_whitespace() || first == '#' || first == '-' {
        return None;
    }
    let colon = bare.find(':')?;
    let key = &bare[..colon];
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    Some(key)
}

/// Serialize one frontmatter key as a dash list (`key:\n    - item`) or an
/// empty inline list (`key: []`), bootstrap-`root.md` style.
fn yaml_key_block(key: &str, items: &[String]) -> String {
    if items.is_empty() {
        return format!("{key}: []\n");
    }
    let mut out = format!("{key}:\n");
    for item in items {
        out.push_str("    - ");
        out.push_str(&yaml_scalar(item));
        out.push('\n');
    }
    out
}

/// Plain scalar when the value is unambiguous YAML in a list-item position
/// (tool names and most globs), double-quoted otherwise — a bare `*alias`,
/// `?`, `"` etc. would corrupt the document.
fn yaml_scalar(value: &str) -> String {
    let plain_safe = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@' | '?' | '*' | '/');
    let first_ok = value
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    if first_ok && !value.is_empty() && value.chars().all(plain_safe) {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_config() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("openslate.toml");
        (dir, path)
    }

    fn provider(base_url: &str, env: &str) -> super::super::ProviderConfig {
        super::super::ProviderConfig {
            base_url: base_url.to_owned(),
            api_key_env: env.to_owned(),
            adapter: None,
            title: None,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            retry_base_ms: DEFAULT_RETRY_BASE_MS,
        }
    }

    fn model(provider: &str, id: &str) -> super::super::ModelConfig {
        super::super::ModelConfig {
            provider: provider.to_owned(),
            model: id.to_owned(),
            max_context_tokens: None,
            max_output_tokens: None,
            supports_tool_call: true,
            supports_vision: false,
            supports_reasoning: false,
            input_price_per_mtok: None,
            output_price_per_mtok: None,
        }
    }

    fn parse_at(path: &Path) -> DocumentMut {
        fs::read_to_string(path)
            .expect("config file should exist")
            .parse::<DocumentMut>()
            .expect("written config should parse")
    }

    fn text_at(path: &Path) -> String {
        fs::read_to_string(path).expect("file should exist")
    }

    // ── upsert_provider ──────────────────────────────────────────────────

    #[test]
    // ── MCP servers ────────────────────────────────────────────────────

    #[test]
    fn upsert_mcp_server_creates_nested_table_with_headers() {
        let (_dir, path) = temp_config();
        let mut headers = std::collections::HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer tok-1".to_string());
        upsert_mcp_server(
            &path,
            "termux",
            "http://127.0.0.1:8765/mcp",
            Some(&headers),
        )
        .expect("upsert");

        let text = text_at(&path);
        assert!(text.contains("[mcp.servers.termux]"), "{text}");
        assert!(text.contains(r#"transport = "http""#), "{text}");
        assert!(text.contains("127.0.0.1:8765"), "{text}");
        assert!(text.contains("Authorization = \"Bearer tok-1\""), "{text}");

        // 幂等 + 无 headers 时清掉旧 headers 子表。
        upsert_mcp_server(&path, "termux", "http://127.0.0.1:8766/mcp", None).expect("re-upsert");
        let text = text_at(&path);
        assert!(!text.contains("Authorization"), "{text}");
        let doc = parse_at(&path);
        let entry = &doc["mcp"]["servers"]["termux"];
        assert_eq!(entry["url"].as_str(), Some("http://127.0.0.1:8766/mcp"));
    }

    #[test]
    fn remove_mcp_server_is_idempotent() {
        let (_dir, path) = temp_config();
        upsert_mcp_server(&path, "a", "http://x/mcp", None).expect("upsert");
        upsert_mcp_server(&path, "b", "http://y/mcp", None).expect("upsert");
        remove_mcp_server(&path, "a").expect("remove");
        let text = text_at(&path);
        assert!(!text.contains("[mcp.servers.a]"), "{text}");
        assert!(text.contains("[mcp.servers.b]"), "{text}");
        // 不存在 → 静默成功；无 [mcp] 节点也一样。
        remove_mcp_server(&path, "zz").expect("remove missing ok");
        let (_dir2, path2) = temp_config();
        remove_mcp_server(&path2, "any").expect("no mcp section ok");
    }

    // ── [mcp].tool_name_template ───────────────────────────────────────

    #[test]
    fn set_mcp_tool_name_template_roundtrip() {
        let (_dir, path) = temp_config();
        // 写入（[mcp] 节不存在则创建）→ 读回。
        set_mcp_tool_name_template(&path, "{server}.{tool}").expect("write");
        let doc = parse_at(&path);
        assert_eq!(
            doc["mcp"]["tool_name_template"].as_str(),
            Some("{server}.{tool}")
        );
        // 覆写。
        set_mcp_tool_name_template(&path, "{tool}").expect("overwrite");
        let doc = parse_at(&path);
        assert_eq!(doc["mcp"]["tool_name_template"].as_str(), Some("{tool}"));
        // 空串 → 键消失（回默认）。
        set_mcp_tool_name_template(&path, "").expect("clear");
        let doc = parse_at(&path);
        let gone = doc["mcp"]
            .as_table()
            .map(|t| t.get("tool_name_template").is_none())
            .unwrap_or(true);
        assert!(gone, "cleared key must be absent: {}", text_at(&path));
        // 再清一次（幂等）。
        set_mcp_tool_name_template(&path, "").expect("clear again ok");
    }

    #[test]
    fn set_mcp_tool_name_template_coexists_with_servers() {
        let (_dir, path) = temp_config();
        upsert_mcp_server(&path, "termux", "http://127.0.0.1:8765/mcp", None).expect("upsert");
        set_mcp_tool_name_template(&path, "{server}_{tool}").expect("write");
        let text = text_at(&path);
        assert!(text.contains("[mcp.servers.termux]"), "{text}");
        assert!(
            text.contains(r#"tool_name_template = "{server}_{tool}""#),
            "{text}"
        );
        // 清模板不动 servers；写出的文件仍可解析回同样的 server 条目。
        set_mcp_tool_name_template(&path, "").expect("clear");
        let text = text_at(&path);
        assert!(!text.contains("tool_name_template"), "{text}");
        assert!(text.contains("[mcp.servers.termux]"), "{text}");
        let doc = parse_at(&path);
        assert_eq!(
            doc["mcp"]["servers"]["termux"]["url"].as_str(),
            Some("http://127.0.0.1:8765/mcp")
        );
    }

    #[test]
    fn set_mcp_tool_name_template_clear_missing_is_ok() {
        // 无 [mcp] 节的文件上清空 → 静默成功，不创建文件。
        let (_dir, path) = temp_config();
        set_mcp_tool_name_template(&path, "").expect("clear on empty doc ok");
        assert!(!path.exists(), "no file should be created on no-op clear");
    }

    // ── rename_mcp_server ──────────────────────────────────────────────

    #[test]
    fn rename_mcp_server_roundtrip() {
        let (_dir, path) = temp_config();
        upsert_mcp_server(&path, "termux", "http://127.0.0.1:8765/mcp", None).expect("upsert");
        rename_mcp_server(&path, "termux", "phone").expect("rename");

        // 展开头随键路径变为 [mcp.servers.phone]，旧头消失，值原样。
        let text = text_at(&path);
        assert!(text.contains("[mcp.servers.phone]"), "{text}");
        assert!(!text.contains("[mcp.servers.termux]"), "{text}");
        assert!(text.contains(r#"transport = "http""#), "{text}");
        assert!(text.contains("127.0.0.1:8765"), "{text}");
        let doc = parse_at(&path);
        assert_eq!(
            doc["mcp"]["servers"]["phone"]["url"].as_str(),
            Some("http://127.0.0.1:8765/mcp")
        );
    }

    #[test]
    fn rename_mcp_server_moves_headers_subtable() {
        // headers 子表随整个 Item 搬移：渲染为 [mcp.servers.<new>.headers]，
        // Authorization 条目原样保留。
        let (_dir, path) = temp_config();
        let mut headers = std::collections::HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer tok-1".to_string());
        upsert_mcp_server(&path, "termux", "http://127.0.0.1:8765/mcp", Some(&headers))
            .expect("upsert");
        rename_mcp_server(&path, "termux", "phone").expect("rename");

        let text = text_at(&path);
        assert!(text.contains("[mcp.servers.phone.headers]"), "{text}");
        assert!(text.contains("Authorization = \"Bearer tok-1\""), "{text}");
        assert!(!text.contains("[mcp.servers.termux"), "{text}");
        let doc = parse_at(&path);
        assert_eq!(
            doc["mcp"]["servers"]["phone"]["headers"]["Authorization"].as_str(),
            Some("Bearer tok-1")
        );
    }

    #[test]
    fn rename_mcp_server_coexists_with_tool_name_template() {
        let (_dir, path) = temp_config();
        upsert_mcp_server(&path, "termux", "http://127.0.0.1:8765/mcp", None).expect("upsert");
        set_mcp_tool_name_template(&path, "{server}.{tool}").expect("template");
        rename_mcp_server(&path, "termux", "phone").expect("rename");

        let text = text_at(&path);
        assert!(text.contains("[mcp.servers.phone]"), "{text}");
        assert!(
            text.contains(r#"tool_name_template = "{server}.{tool}""#),
            "{text}"
        );
    }

    #[test]
    fn rename_mcp_server_same_name_is_idempotent() {
        let (_dir, path) = temp_config();
        upsert_mcp_server(&path, "termux", "http://127.0.0.1:8765/mcp", None).expect("upsert");
        // 显式 old == new → ok，键不动。
        rename_mcp_server(&path, "termux", "termux").expect("old == new ok");
        assert!(text_at(&path).contains("[mcp.servers.termux]"));
        // 单键场景下 old 过期、new 即现名 → 同样幂等 ok。
        rename_mcp_server(&path, "stale", "termux").expect("stale old, new == current ok");
        assert!(text_at(&path).contains("[mcp.servers.termux]"));
    }

    #[test]
    fn rename_mcp_server_multi_key_targets_old() {
        let (_dir, path) = temp_config();
        upsert_mcp_server(&path, "a", "http://x/mcp", None).expect("upsert a");
        upsert_mcp_server(&path, "b", "http://y/mcp", None).expect("upsert b");
        rename_mcp_server(&path, "a", "c").expect("rename a -> c");
        let text = text_at(&path);
        assert!(text.contains("[mcp.servers.c]"), "{text}");
        assert!(text.contains("[mcp.servers.b]"), "{text}");
        assert!(!text.contains("[mcp.servers.a]"), "{text}");
        let doc = parse_at(&path);
        assert_eq!(doc["mcp"]["servers"]["c"]["url"].as_str(), Some("http://x/mcp"));
        assert_eq!(doc["mcp"]["servers"]["b"]["url"].as_str(), Some("http://y/mcp"));

        // 多键但 old 不存在 → 报错，不动文件。
        let (_dir2, path2) = temp_config();
        upsert_mcp_server(&path2, "a", "http://x/mcp", None).expect("upsert a");
        upsert_mcp_server(&path2, "b", "http://y/mcp", None).expect("upsert b");
        assert!(rename_mcp_server(&path2, "zz", "c").is_err());
        assert!(text_at(&path2).contains("[mcp.servers.a]"));

        // 目标键已被另一 server 占用 → 拒绝覆盖。
        assert!(rename_mcp_server(&path2, "a", "b").is_err());
        assert!(text_at(&path2).contains("[mcp.servers.a]"));
        assert!(text_at(&path2).contains("[mcp.servers.b]"));
    }

    #[test]
    fn rename_mcp_server_empty_or_missing_errors() {
        // 无 [mcp] 节 → 报错。
        let (_dir, path) = temp_config();
        assert!(rename_mcp_server(&path, "termux", "phone").is_err());
        assert!(!path.exists(), "failed rename must not create a file");
        // [mcp.servers] 存在但为空表 → 同样报错。
        let (_dir2, path2) = temp_config();
        fs::write(&path2, "[mcp.servers]\n").expect("write empty servers");
        assert!(rename_mcp_server(&path2, "termux", "phone").is_err());
    }

    fn upsert_provider_creates_new_file() {
        let (_dir, path) = temp_config();
        upsert_provider(
            &path,
            "zhipu",
            &provider("https://api.example.com", "ZHIPU_API_KEY"),
        )
        .expect("upsert");

        let doc = parse_at(&path);
        let text = text_at(&path);
        assert!(text.contains("[providers.zhipu]"), "{text}");
        assert_eq!(
            doc["providers"]["zhipu"]["base_url"].as_str(),
            Some("https://api.example.com")
        );
        assert_eq!(
            doc["providers"]["zhipu"]["api_key_env"].as_str(),
            Some("ZHIPU_API_KEY")
        );
        assert!(
            doc["providers"]["zhipu"].get("max_attempts").is_none(),
            "default retry fields are omitted"
        );
    }

    #[test]
    fn upsert_provider_writes_and_drops_title() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("openslate.toml");

        let mut with_title = provider("https://x", "K");
        with_title.title = Some("智谱 开放平台".into());
        upsert_provider(&path, "zhipu", &with_title).expect("write title");
        let doc = load_document(&path).unwrap();
        assert_eq!(
            doc["providers"]["zhipu"]["title"].as_str(),
            Some("智谱 开放平台")
        );

        // 清空 title（None）→ 重建表体时该键被移除，不残留。
        upsert_provider(&path, "zhipu", &provider("https://x", "K")).expect("drop title");
        let doc = load_document(&path).unwrap();
        assert!(doc["providers"]["zhipu"].get("title").is_none());
    }

    #[test]
    fn upsert_provider_writes_non_default_retry_fields() {
        let (_dir, path) = temp_config();
        let mut cfg = provider("https://api.example.com", "K");
        cfg.adapter = Some("anthropic".to_owned());
        cfg.max_attempts = 5;
        cfg.retry_base_ms = 250;
        upsert_provider(&path, "p", &cfg).expect("upsert");

        let doc = parse_at(&path);
        assert_eq!(doc["providers"]["p"]["adapter"].as_str(), Some("anthropic"));
        assert_eq!(doc["providers"]["p"]["max_attempts"].as_integer(), Some(5));
        assert_eq!(
            doc["providers"]["p"]["retry_base_ms"].as_integer(),
            Some(250)
        );
    }

    #[test]
    fn upsert_provider_rebuilds_body_dropping_stale_fields() {
        let (_dir, path) = temp_config();
        fs::write(
            &path,
            "[providers.p]\nbase_url = \"https://old\"\napi_key_env = \"OLD\"\nmax_attempts = 9\n",
        )
        .expect("seed");
        upsert_provider(&path, "p", &provider("https://new", "NEW")).expect("upsert");

        let text = text_at(&path);
        assert!(text.contains("https://new"), "{text}");
        assert!(
            !text.contains("max_attempts"),
            "stale non-default field is dropped on rebuild: {text}"
        );
    }

    #[test]
    fn upsert_provider_preserves_chinese_comments_and_other_bytes() {
        // The canonical acceptance sample: a file with Chinese comments
        // (matching the real user config) must round-trip byte-for-byte
        // outside the upserted table.
        let (_dir, path) = temp_config();
        let original = [
            "# 主配置文件（示例）",
            "# 手写注释：智谱为默认供应商",
            "[providers.zhipu]",
            "base_url = \"https://old.example.com\"",
            "api_key_env = \"OLD_KEY\"",
            "",
            "# 模型条目：主力模型",
            "[models.main]",
            "provider = \"zhipu\"  # 指向智谱",
            "model = \"glm-5.1\"   # 行内注释也保留",
            "",
        ]
        .join("\n");
        fs::write(&path, &original).expect("seed");

        // Everything up to and including the upserted table's header must be
        // byte-identical (header decor preserved)…
        let marker = "[providers.zhipu]\n";
        let header_end = original.find(marker).expect("marker") + marker.len();
        let before = &original[..header_end];

        // …and the trailing [models.main] region (with its Chinese comments)
        // must reappear verbatim after the rebuilt provider body.
        let tail_marker = "[models.main]";
        let tail_idx = original.find(tail_marker).expect("tail marker");
        let tail = &original[tail_idx..];

        upsert_provider(
            &path,
            "zhipu",
            &provider("https://new.example.com", "NEW_KEY"),
        )
        .expect("upsert");
        let updated = text_at(&path);

        assert!(
            updated.starts_with(before),
            "bytes before the upserted table body changed:\n--- before ---\n{before}\n--- updated ---\n{updated}"
        );
        assert!(
            updated.contains(tail),
            "trailing region with Chinese comments was disturbed:\n{updated}"
        );
        assert!(updated.contains("https://new.example.com"));

        // Still parses as valid config.
        let parsed = crate::config::parse_openslate_toml(&updated).expect("still valid TOML");
        assert_eq!(
            parsed.providers.get("zhipu").expect("provider").base_url,
            "https://new.example.com"
        );
        assert_eq!(parsed.models.get("main").expect("model").model, "glm-5.1");
    }

    #[test]
    fn upsert_provider_is_idempotent() {
        let (_dir, path) = temp_config();
        let cfg = provider("https://api.example.com", "K");
        upsert_provider(&path, "p", &cfg).expect("first");
        let first = text_at(&path);
        upsert_provider(&path, "p", &cfg).expect("second");
        let second = text_at(&path);
        assert_eq!(first, second, "repeated upsert must not change the file");
    }

    // ── upsert_model ─────────────────────────────────────────────────────

    #[test]
    fn upsert_model_writes_required_and_skips_default_optionals() {
        let (_dir, path) = temp_config();
        upsert_model(&path, "main", &model("zhipu", "glm-5.1")).expect("upsert");

        let text = text_at(&path);
        let doc = parse_at(&path);
        assert!(text.contains("[models.main]"), "{text}");
        assert_eq!(doc["models"]["main"]["provider"].as_str(), Some("zhipu"));
        assert_eq!(doc["models"]["main"]["model"].as_str(), Some("glm-5.1"));
        for absent in [
            "max_context_tokens",
            "max_output_tokens",
            "supports_tool_call",
            "supports_vision",
            "supports_reasoning",
            "input_price_per_mtok",
            "output_price_per_mtok",
        ] {
            assert!(
                doc["models"]["main"].get(absent).is_none(),
                "default optional `{absent}` must be omitted: {text}"
            );
        }
    }

    #[test]
    fn upsert_model_writes_non_default_optionals() {
        let (_dir, path) = temp_config();
        let mut cfg = model("zhipu", "glm-4v");
        cfg.max_context_tokens = Some(128_000);
        cfg.max_output_tokens = Some(8_192);
        cfg.supports_tool_call = false;
        cfg.supports_vision = true;
        cfg.supports_reasoning = true;
        cfg.input_price_per_mtok = Some(0.5);
        cfg.output_price_per_mtok = Some(2.0);
        upsert_model(&path, "vision", &cfg).expect("upsert");

        let doc = parse_at(&path);
        let entry = &doc["models"]["vision"];
        assert_eq!(entry["max_context_tokens"].as_integer(), Some(128_000));
        assert_eq!(entry["max_output_tokens"].as_integer(), Some(8_192));
        assert_eq!(entry["supports_tool_call"].as_bool(), Some(false));
        assert_eq!(entry["supports_vision"].as_bool(), Some(true));
        assert_eq!(entry["supports_reasoning"].as_bool(), Some(true));
        assert_eq!(entry["input_price_per_mtok"].as_float(), Some(0.5));
        assert_eq!(entry["output_price_per_mtok"].as_float(), Some(2.0));
    }

    #[test]
    fn upsert_model_appends_into_existing_models_table() {
        let (_dir, path) = temp_config();
        upsert_model(&path, "main", &model("zhipu", "glm-5.1")).expect("first");
        upsert_model(&path, "fast", &model("minimax", "mini")).expect("second");

        let text = text_at(&path);
        assert!(text.contains("[models.main]"), "{text}");
        assert!(text.contains("[models.fast]"), "{text}");
        // Round-trips through the real parser.
        let parsed = crate::config::parse_openslate_toml(&text).expect("valid");
        assert_eq!(parsed.models.len(), 2);
    }

    #[test]
    fn upsert_model_is_idempotent() {
        let (_dir, path) = temp_config();
        let cfg = model("zhipu", "glm-5.1");
        upsert_model(&path, "main", &cfg).expect("first");
        let first = text_at(&path);
        upsert_model(&path, "main", &cfg).expect("second");
        assert_eq!(first, text_at(&path));
    }

    // ── set_level / remove_level ─────────────────────────────────────────

    #[test]
    fn set_level_creates_commented_levels_table() {
        let (_dir, path) = temp_config();
        set_level(&path, "main", "glm5").expect("set");

        let text = text_at(&path);
        assert!(text.contains("[levels]"), "{text}");
        assert!(
            text.contains("Model levels"),
            "new [levels] table carries the comment header: {text}"
        );
        let parsed = crate::config::parse_openslate_toml(&text).expect("valid");
        assert_eq!(parsed.levels.get("main").map(String::as_str), Some("glm5"));
    }

    #[test]
    fn set_level_updates_existing_mapping_and_stays_idempotent() {
        let (_dir, path) = temp_config();
        set_level(&path, "main", "glm5").expect("set 1");
        set_level(&path, "fast", "mini").expect("set 2");
        set_level(&path, "main", "other").expect("overwrite");
        let once = text_at(&path);
        set_level(&path, "main", "other").expect("repeat");
        assert_eq!(once, text_at(&path), "idempotent");

        let parsed = crate::config::parse_openslate_toml(&once).expect("valid");
        assert_eq!(parsed.levels.get("main").map(String::as_str), Some("other"));
        assert_eq!(parsed.levels.get("fast").map(String::as_str), Some("mini"));
    }

    // ── set_capability ──────────────────────────────────────────────────

    #[test]
    fn set_capability_creates_commented_table_and_updates() {
        let (_dir, path) = temp_config();
        set_capability(&path, "compact", "fast").expect("set 1");

        let text = text_at(&path);
        assert!(text.contains("[capabilities]"), "{text}");
        assert!(
            text.contains("Capability -> level mapping"),
            "new [capabilities] table carries the comment header: {text}"
        );
        let parsed = crate::config::parse_openslate_toml(&text).expect("valid");
        assert_eq!(
            parsed.capabilities.get("compact").map(String::as_str),
            Some("fast")
        );

        set_capability(&path, "compact", "vision").expect("overwrite");
        set_capability(&path, "main", "main").expect("second key");
        let once = text_at(&path);
        set_capability(&path, "compact", "vision").expect("repeat");
        assert_eq!(once, text_at(&path), "idempotent");
        let parsed = crate::config::parse_openslate_toml(&once).expect("valid");
        assert_eq!(
            parsed.capabilities.get("compact").map(String::as_str),
            Some("vision")
        );
        assert_eq!(
            parsed.capabilities.get("main").map(String::as_str),
            Some("main")
        );
    }

    #[test]
    fn remove_level_removes_and_is_silent_when_missing() {
        let (_dir, path) = temp_config();
        // Missing [levels] section entirely → silent OK.
        remove_level(&path, "main").expect("silent ok");
        // Now with a section.
        set_level(&path, "main", "glm5").expect("set");
        remove_level(&path, "main").expect("remove");
        let parsed = crate::config::parse_openslate_toml(&text_at(&path)).expect("valid");
        assert!(parsed.levels.is_empty(), "{:?}", parsed.levels);
        // Removing again is still OK.
        remove_level(&path, "main").expect("idempotent remove");
    }

    // ── remove_model_entry / remove_provider ─────────────────────────────

    #[test]
    fn remove_model_entry_and_provider_work_and_are_silent_when_missing() {
        let (_dir, path) = temp_config();
        // No sections at all → silent OK.
        remove_model_entry(&path, "main").expect("silent ok");
        remove_provider(&path, "zhipu").expect("silent ok");

        upsert_provider(&path, "zhipu", &provider("https://x", "K")).expect("provider");
        upsert_model(&path, "main", &model("zhipu", "glm-5.1")).expect("model");

        remove_model_entry(&path, "main").expect("remove model");
        remove_provider(&path, "zhipu").expect("remove provider");

        let parsed = crate::config::parse_openslate_toml(&text_at(&path)).expect("valid");
        assert!(parsed.models.is_empty());
        assert!(parsed.providers.is_empty());

        // Repeat removals are silent successes.
        remove_model_entry(&path, "main").expect("idempotent");
        remove_provider(&path, "zhipu").expect("idempotent");
    }

    #[test]
    fn remove_operations_leave_other_entries_untouched() {
        let (_dir, path) = temp_config();
        upsert_provider(&path, "keep", &provider("https://keep", "KEEP")).expect("p");
        upsert_provider(&path, "drop", &provider("https://drop", "DROP")).expect("p");
        upsert_model(&path, "keep", &model("keep", "m1")).expect("m");
        upsert_model(&path, "drop", &model("keep", "m2")).expect("m");

        remove_provider(&path, "drop").expect("remove");
        remove_model_entry(&path, "drop").expect("remove");

        let parsed = crate::config::parse_openslate_toml(&text_at(&path)).expect("valid");
        assert!(parsed.providers.contains_key("keep"));
        assert!(!parsed.providers.contains_key("drop"));
        assert!(parsed.models.contains_key("keep"));
        assert!(!parsed.models.contains_key("drop"));
    }

    // ── upsert_env_key ───────────────────────────────────────────────────

    #[test]
    fn upsert_env_key_creates_new_file() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let config_dir = dir.path().join("openslate");
        upsert_env_key(&config_dir, "ZHIPU_API_KEY", "sk-123").expect("upsert");

        let content = fs::read_to_string(config_dir.join(".env")).expect("file");
        assert_eq!(content, "ZHIPU_API_KEY=sk-123\n");
    }

    #[test]
    #[cfg(unix)]
    fn upsert_env_key_new_file_has_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().expect("temp dir");
        let config_dir = dir.path().join("openslate");
        upsert_env_key(&config_dir, "K", "v").expect("upsert");

        let meta = fs::metadata(config_dir.join(".env")).expect("file");
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "newly created .env must be 0600"
        );
    }

    #[test]
    fn upsert_env_key_updates_existing_line_in_place() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let config_dir = dir.path();
        fs::write(
            config_dir.join(".env"),
            "# 注释保留\nOTHER=1\nZHIPU_API_KEY=old\nTAIL=2\n",
        )
        .expect("seed");

        upsert_env_key(config_dir, "ZHIPU_API_KEY", "new").expect("upsert");

        let content = fs::read_to_string(config_dir.join(".env")).expect("file");
        assert_eq!(
            content, "# 注释保留\nOTHER=1\nZHIPU_API_KEY=new\nTAIL=2\n",
            "only the matched line is replaced, everything else byte-identical"
        );
    }

    #[test]
    fn upsert_env_key_appends_when_absent() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let config_dir = dir.path();
        // No trailing newline on purpose: the append must add one.
        fs::write(config_dir.join(".env"), "A=1").expect("seed");

        upsert_env_key(config_dir, "B", "2").expect("upsert");

        let content = fs::read_to_string(config_dir.join(".env")).expect("file");
        assert_eq!(content, "A=1\nB=2\n");
    }

    #[test]
    fn upsert_env_key_is_idempotent() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let config_dir = dir.path();
        fs::write(config_dir.join(".env"), "A=1\nK=old\n").expect("seed");

        upsert_env_key(config_dir, "K", "new").expect("first");
        let once = fs::read_to_string(config_dir.join(".env")).expect("file");
        upsert_env_key(config_dir, "K", "new").expect("second");
        assert_eq!(
            once,
            fs::read_to_string(config_dir.join(".env")).expect("file"),
            "repeated upsert must not change the file"
        );
    }

    #[test]
    fn upsert_env_key_matches_only_exact_var_prefix() {
        // `KEY=...` must not match a line starting with `KEYWORD=...`.
        let dir = tempfile::TempDir::new().expect("temp dir");
        let config_dir = dir.path();
        fs::write(config_dir.join(".env"), "KEYWORD=1\n").expect("seed");

        upsert_env_key(config_dir, "KEY", "v").expect("upsert");

        let content = fs::read_to_string(config_dir.join(".env")).expect("file");
        assert_eq!(content, "KEYWORD=1\nKEY=v\n");
    }

    #[test]
    fn upsert_env_key_rejects_bad_names_and_values() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        assert!(upsert_env_key(dir.path(), "", "v").is_err());
        assert!(upsert_env_key(dir.path(), "A=B", "v").is_err());
        assert!(upsert_env_key(dir.path(), "A", "line1\nline2").is_err());
    }

    // ── set_agent_tool_lists（agent md frontmatter 外科手术）───────────

    // 注意：字符串续行（`\`+换行）会吞掉下一行前导空格，列表缩进必须
    // 用原始字符串字面量保真。
    const ROOT_MD: &str = r#"---
id: root
name: OpenSlate
# 主对话模型
model: main
tools:
    - mobile.ping
    - bash
children: []
---
You are a helpful AI assistant.
"#;

    fn agent_file() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("root.md");
        fs::write(&path, ROOT_MD).expect("seed agent md");
        (dir, path)
    }

    #[test]
    fn set_agent_tool_lists_rewrites_only_the_two_keys() {
        let (dir, path) = agent_file();
        set_agent_tool_lists(
            &path,
            &["mobile.ping".into(), "bash".into()],
            &["termux_bash".into()],
        )
        .expect("rewrite");

        let content = text_at(&path);
        // 其余 frontmatter 键（含注释）与正文逐字节保留。
        assert!(content.contains("id: root\n"));
        assert!(content.contains("# 主对话模型\nmodel: main\n"));
        assert!(content.contains("children: []\n"));
        assert!(content.ends_with("---\nYou are a helpful AI assistant.\n"));
        // 两个键按新值重写，hidden_tools 紧随 tools。
        assert!(content.contains("tools:\n    - mobile.ping\n    - bash\n"));
        assert!(content.contains("hidden_tools:\n    - termux_bash\n"));
        assert_eq!(content.matches("tools:").count(), 2); // tools + hidden_tools

        // 回读语义正确（markdown 解析器视角）。
        let agent = super::super::parse_agent_markdown(&content, "root.md").expect("reparse");
        assert_eq!(agent.tools, ["mobile.ping", "bash"]);
        assert_eq!(agent.hidden_tools, ["termux_bash"]);
        let _ = dir;
    }

    #[test]
    fn set_agent_tool_lists_is_idempotent() {
        let (dir, path) = agent_file();
        set_agent_tool_lists(&path, &["bash".into()], &["mcp_*".into()]).expect("first");
        let once = text_at(&path);
        set_agent_tool_lists(&path, &["bash".into()], &["mcp_*".into()]).expect("second");
        assert_eq!(once, text_at(&path), "second call must not change the file");
        let _ = dir;
    }

    #[test]
    fn set_agent_tool_lists_empty_list_writes_inline_empty() {
        let (dir, path) = agent_file();
        set_agent_tool_lists(&path, &[], &[]).expect("rewrite");
        let content = text_at(&path);
        assert!(content.contains("tools: []\n"));
        assert!(content.contains("hidden_tools: []\n"));
        let agent = super::super::parse_agent_markdown(&content, "root.md").expect("reparse");
        assert!(agent.tools.is_empty());
        assert!(agent.hidden_tools.is_empty());
        let _ = dir;
    }

    #[test]
    fn set_agent_tool_lists_quotes_glob_only_patterns() {
        let (dir, path) = agent_file();
        // 前导 `*` 的裸模式在 YAML 里是 alias 语法，必须引号包裹。
        set_agent_tool_lists(&path, &["*".into(), "fs_*".into()], &["*_echo".into()])
            .expect("rewrite");
        let content = text_at(&path);
        assert!(content.contains("    - \"*\"\n"));
        assert!(content.contains("    - fs_*\n"));
        assert!(content.contains("    - \"*_echo\"\n"));
        let agent = super::super::parse_agent_markdown(&content, "root.md").expect("reparse");
        assert_eq!(agent.tools, ["*", "fs_*"]);
        assert_eq!(agent.hidden_tools, ["*_echo"]);
        let _ = dir;
    }

    #[test]
    fn set_agent_tool_lists_relocates_stray_hidden_tools() {
        // hidden_tools 在文件别处（甚至 tools 之前）也会被收编到 tools 之后。
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("root.md");
        fs::write(
            &path,
            "---\nhidden_tools:\n    - old\nname: OpenSlate\nmodel: main\ntools: [bash]\n---\nbody\n",
        )
        .expect("seed");

        set_agent_tool_lists(&path, &["read_file".into()], &[]).expect("rewrite");

        let content = text_at(&path);
        let tools_at = content.find("tools:\n    - read_file\n").expect("fresh tools");
        let hidden_at = content
            .find("hidden_tools: []\n")
            .expect("fresh hidden follows tools");
        assert!(hidden_at > tools_at);
        assert_eq!(content.matches("hidden_tools").count(), 1);
        assert!(content.ends_with("---\nbody\n"));
        let _ = dir;
    }

    #[test]
    fn set_agent_tool_lists_inserts_into_file_without_the_keys() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("child.md");
        fs::write(&path, "---\nname: Child\nmodel: fast\n---\nDo things.\n").expect("seed");

        set_agent_tool_lists(&path, &["bash".into()], &["mcp_fs_*".into()]).expect("rewrite");

        let agent =
            super::super::parse_agent_markdown(&text_at(&path), "child.md").expect("reparse");
        assert_eq!(agent.tools, ["bash"]);
        assert_eq!(agent.hidden_tools, ["mcp_fs_*"]);
        assert!(text_at(&path).ends_with("---\nDo things.\n"));
        let _ = dir;
    }

    #[test]
    fn set_agent_tool_lists_rejects_bad_input() {
        let (dir, path) = agent_file();
        // 无 frontmatter / 未闭合 frontmatter → 报错不动文件。
        let bare = dir.path().join("bare.md");
        fs::write(&bare, "no frontmatter here\n").expect("seed");
        assert!(set_agent_tool_lists(&bare, &[], &[]).is_err());

        let unclosed = dir.path().join("unclosed.md");
        fs::write(&unclosed, "---\nname: X\n").expect("seed");
        assert!(set_agent_tool_lists(&unclosed, &[], &[]).is_err());

        // 多行条目 → 拒绝（会破坏 YAML 结构）。
        assert!(set_agent_tool_lists(&path, &["a\nb".into()], &[]).is_err());

        // 不存在的文件 → 报错。
        assert!(set_agent_tool_lists(&dir.path().join("nope.md"), &[], &[]).is_err());
    }
}
