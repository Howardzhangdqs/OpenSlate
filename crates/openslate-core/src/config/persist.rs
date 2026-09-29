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
}
