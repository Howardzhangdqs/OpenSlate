//! Integration wiring — assembles all components for the CLI run pipeline.
//!
//! Creates an `AppContext` that ties together:
//! config loading → validation → SQLite store → agent tree → tool registry → RunManager.
//!
//! NOTE: the LLM provider is no longer stored in `AppContext`; it is built per
//! run/turn via `cmd::run::build_provider_for_model` (so it can be dispatched on
//! `ProviderConfig.kind`, e.g. OpenAI-compatible vs. genai).

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use openslate_core::agent_tree::AgentTree;
use openslate_core::approval::{
    ApprovalCallback, ApprovalDecision, ApprovalManager, ApprovalPolicy, ApprovalRequest, RiskLevel,
};
use openslate_core::config::validation::validate_config;
use openslate_core::config::{
    parse_agents_dir, parse_openslate_toml, AgentsConfig, OpenSlateConfig,
};
use openslate_core::paths::{resolve_paths, OpenSlatePaths};
use openslate_core::run_manager::RunManager;
use openslate_core::skills::{discover_skills, SkillsCatalog};
use openslate_core::tool::ToolRegistry;
use openslate_store_sqlite::store::SqliteStore;

/// Fully assembled application context ready for running agents.
#[allow(dead_code)]
pub struct AppContext {
    pub config: OpenSlateConfig,
    pub agents: AgentsConfig,
    pub store: Option<SqliteStore>,
    pub agent_tree: AgentTree,
    pub manager: RunManager,
    /// Discovered skills catalog (empty when `[skills] enabled = false` or
    /// nothing was found). Inert data for diagnostics; the RunManager holds
    /// its own copy for prompt injection.
    pub skills: SkillsCatalog,
    /// Live MCP server connections (builtin in-process servers first, then
    /// external ones). Declared after `manager` so that on drop, the registry
    /// (and its `McpTool`s holding `ServerSink` clones) is dropped *before*
    /// the connections themselves are cancelled — avoiding any window where a
    /// tool could outlive its transport.
    pub mcp_connections: openslate_core::mcp::McpConnectionGuard,
    /// Resolved config file path (for diagnostics).
    pub config_path: std::path::PathBuf,
    /// Resolved agents file path.
    pub agents_path: std::path::PathBuf,
}

/// Load and parse `openslate.toml` from the given path.
pub(crate) fn load_config(config_path: &Path) -> Result<OpenSlateConfig> {
    let content = fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read config file '{}'", config_path.display()))?;
    parse_openslate_toml(&content)
        .with_context(|| format!("Failed to parse config file '{}'", config_path.display()))
}

/// Load and parse agents from the `agents/` directory.
pub(crate) fn load_agents(agents_dir: &Path) -> Result<AgentsConfig> {
    if !agents_dir.is_dir() {
        anyhow::bail!("Agents directory not found: {}", agents_dir.display());
    }
    parse_agents_dir(agents_dir).with_context(|| {
        format!(
            "Failed to parse agents directory '{}'",
            agents_dir.display()
        )
    })
}

/// Resolve config file path from CLI `--config` flag or default XDG resolution.
pub fn resolve_config_file(config_flag: Option<&str>) -> Result<std::path::PathBuf> {
    if let Some(flag) = config_flag {
        let path = Path::new(flag);
        if !path.exists() {
            anyhow::bail!("Config file not found: {}", path.display());
        }
        return Ok(path.to_path_buf());
    }

    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let paths = resolve_paths(&cwd);
    if !paths.config_file.exists() {
        anyhow::bail!(
            "No openslate.toml found. Expected at {}. Run `openslate init` to create one.",
            paths.config_file.display()
        );
    }
    Ok(paths.config_file)
}

/// Resolve agents directory path from the config file's parent directory.
pub fn resolve_agents_dir(config_path: &Path) -> std::path::PathBuf {
    config_path
        .parent()
        .map(|p| p.join("agents"))
        .unwrap_or_else(|| Path::new("agents").to_path_buf())
}

/// Resolve the skill discovery source directories, ordered LOW→HIGH
/// precedence:
///
/// 1. `~/.agents/skills` — user-global, agent-tool-agnostic location
///    (skipped silently when no home directory is resolvable);
/// 2. the XDG-aware user-native skills dir (`~/.config/openslate/skills`);
/// 3. `cwd/.agents/skills` — project-local, tool-agnostic location;
/// 4. `{config dir}/skills` — project-native (e.g. `.openslate/skills`).
///
/// Every source is normalized ([`canonical_source`]) BEFORE dedupe, so a
/// relative `--config` spelling (whose parent yields a relative source-4
/// path, or an empty parent for a bare `openslate.toml`) and symlinked
/// directories cannot evade dedupe and double-scan the same physical dir
/// (which would produce spurious "shadows" warnings). Source 4 equals
/// source 2 when running off the global config; exact duplicates are
/// removed keeping the LATER (higher-precedence) occurrence. Missing
/// directories are silently skipped by the discovery itself.
pub fn skills_sources(config_path: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut sources: Vec<PathBuf> = Vec::with_capacity(4);
    if let Some(home) = dirs::home_dir() {
        sources.push(home.join(".agents").join("skills"));
    }
    sources.push(resolve_paths(cwd).global_config_dir.join("skills"));
    sources.push(cwd.join(".agents").join("skills"));
    // A bare `--config openslate.toml` has an empty parent; the joined
    // `skills` stays relative here and is resolved against `cwd` below
    // (consistent with the empty-parent fallback in core's
    // `parse_skill_markdown`, which yields `.`).
    if let Some(parent) = config_path.parent() {
        sources.push(parent.join("skills"));
    }
    let sources: Vec<PathBuf> = sources.iter().map(|s| canonical_source(s, cwd)).collect();

    // Dedupe by normalized path equality, keeping the later occurrence (it
    // has higher precedence, and `discover_skills` lets later sources
    // shadow).
    let mut deduped: Vec<PathBuf> = Vec::with_capacity(sources.len());
    for source in sources {
        if let Some(pos) = deduped.iter().position(|s| s == &source) {
            deduped.remove(pos);
        }
        deduped.push(source);
    }
    deduped
}

/// Absolutize `path` against `cwd` (for relative spellings, e.g. a
/// `--config` flag value), then canonicalize when the directory exists so
/// symlinked paths to one physical dir compare equal. Non-existent dirs
/// (silently skipped by discovery anyway) fall back to the absolute path.
fn canonical_source(path: &Path, cwd: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    abs.canonicalize().unwrap_or(abs)
}

/// Discover skills per `config.skills` and log the outcome.
///
/// Enabled: runs [`discover_skills`] over [`skills_sources`], surfacing every
/// warning via `tracing::warn!` (a broken skill never blocks startup).
/// Disabled: returns the empty catalog and logs a debug line.
fn load_skills(config: &OpenSlateConfig, config_path: &Path, cwd: &Path) -> SkillsCatalog {
    if !config.skills.enabled {
        tracing::debug!(
            target: "openslate_skills",
            "skills disabled ([skills] enabled = false), skipping discovery"
        );
        return SkillsCatalog::default();
    }

    let sources = skills_sources(config_path, cwd);
    let (catalog, warnings) = discover_skills(&sources);
    for warning in &warnings {
        tracing::warn!(target: "openslate_skills", "skill warning: {warning}");
    }
    for skill in catalog.skills() {
        tracing::debug!(
            target: "openslate_skills",
            "skill '{}' from {}",
            skill.name,
            skill.path.display()
        );
    }
    tracing::info!(
        target: "openslate_skills",
        "skills loaded: {} (from {} sources)",
        catalog.skills().len(),
        sources.len()
    );
    catalog
}

// ── Approval wiring (Phase 1) ───────────────────────────────────────────────

/// Tools gated by the derived REPL default when no `[approval]` section is
/// configured (locked product decision: interactive sessions ask before the
/// dangerous tools, while non-interactive runs stay fully automatic).
pub(crate) const REPL_DEFAULT_APPROVAL_TOOLS: [&str; 2] = ["shell", "run_code"];

/// Derive the effective approval policy.
///
/// Priority (locked): `--yes` > `[approval].policy` > default. The default
/// itself depends on interactivity — an interactive session with no
/// `[approval]` section derives `auto_except(["shell", "run_code"])`; a
/// non-interactive run derives plain `auto`. Pure function; fully covered by
/// a matrix unit test.
pub(crate) fn derive_effective_policy(
    configured: Option<ApprovalPolicy>,
    interactive: bool,
    yes: bool,
) -> ApprovalPolicy {
    if yes {
        return ApprovalPolicy::Auto;
    }
    if let Some(policy) = configured {
        return policy;
    }
    if interactive {
        return ApprovalPolicy::AutoExcept(
            REPL_DEFAULT_APPROVAL_TOOLS
                .iter()
                .map(|t| (*t).to_owned())
                .collect(),
        );
    }
    ApprovalPolicy::Auto
}

/// Non-interactive safety gate: a non-interactive run has no human to ask,
/// so high-risk tool calls are denied with an actionable reason and
/// everything else is approved. Installed by [`apply_approval`] whenever the
/// effective policy is not `auto` in a non-interactive run without `--yes`
/// (the CLI-derived strategy that avoids "needs approval, no callback").
pub(crate) struct NonInteractiveGate;

impl ApprovalCallback for NonInteractiveGate {
    fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision {
        if req.risk_level == RiskLevel::High {
            ApprovalDecision::Denied("非交互 manual 模式拒绝高危工具,可用 --yes 覆盖".to_owned())
        } else {
            ApprovalDecision::Approved
        }
    }
}

/// Wire the effective approval manager into a RunManager.
///
/// Non-interactive runs with a non-`auto` effective policy and no `--yes`
/// get the [`NonInteractiveGate`] callback plus a one-line startup WARN.
/// Returns the effective policy for callers that want to report it.
pub(crate) fn apply_approval(
    manager: &mut RunManager,
    config: &OpenSlateConfig,
    interactive: bool,
    yes: bool,
) -> ApprovalPolicy {
    let configured = config.approval.as_ref().map(|a| a.to_policy());
    let effective = derive_effective_policy(configured, interactive, yes);
    let mut approval = ApprovalManager::new(effective.clone());
    if !interactive && !yes && effective != ApprovalPolicy::Auto {
        approval = approval.with_callback(Arc::new(NonInteractiveGate));
        tracing::warn!(
            target: "openslate_approval",
            "非交互运行且未加 --yes:审批策略 {:?} 下高危工具将被拒绝(可用 --yes 覆盖)",
            effective
        );
    }
    manager.approval = approval;
    effective
}

/// Initialize the SQLite store based on config.
///
/// Creates the database file (if file-based) and runs migrations.
/// Returns `None` if store initialization should be skipped (e.g. missing config).
async fn init_store(
    config: &OpenSlateConfig,
    paths: &OpenSlatePaths,
) -> Result<Option<SqliteStore>> {
    let db_path = config
        .database
        .as_ref()
        .and_then(|db| db.path.clone())
        .map(|p| {
            if Path::new(&p).is_absolute() {
                p
            } else {
                paths
                    .global_data_dir
                    .join(&p)
                    .to_str()
                    .map(|s| s.to_owned())
                    .unwrap_or(p)
            }
        })
        .unwrap_or_else(|| {
            paths
                .database_path
                .to_str()
                .expect("database_path should be valid UTF-8")
                .to_owned()
        });

    // Ensure parent directory exists
    if let Some(parent) = Path::new(&db_path).parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create database directory '{}'", parent.display())
            })?;
        }
    }

    tracing::debug!("Initializing SQLite store at: {}", db_path);

    let store = SqliteStore::new(&db_path)
        .await
        .with_context(|| format!("Failed to open SQLite database at '{}'", db_path))?;

    store
        .run_migrations()
        .await
        .with_context(|| "Failed to run database migrations")?;

    tracing::debug!("SQLite store initialized and migrations applied");
    Ok(Some(store))
}

/// Build the full application context from CLI parameters.
pub async fn build_app_context(config_flag: Option<&str>) -> Result<AppContext> {
    // 1. Resolve config file path
    let config_path = resolve_config_file(config_flag)?;
    tracing::debug!("Using config: {}", config_path.display());

    // 1.5. Auto-load .env from the config file's directory. Does NOT override
    //      already-set env vars (so explicit shell exports win). Lets users keep
    //      provider API keys out of the committed config without exporting them
    //      in every shell. Missing/malformed .env is a soft warning, not fatal.
    if let Some(dir) = config_path.parent() {
        let env_path = dir.join(".env");
        if env_path.is_file() {
            match dotenvy::from_path(&env_path) {
                Ok(()) => tracing::debug!("Loaded .env from {}", env_path.display()),
                Err(e) => tracing::warn!("Failed to load .env at {}: {}", env_path.display(), e),
            }
        }
    }

    let agents_path = resolve_agents_dir(&config_path);
    tracing::debug!("Using agents: {}", agents_path.display());

    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let paths = resolve_paths(&cwd);

    // 2. Load config
    let config = load_config(&config_path)?;

    // 3. Load agents
    let agents = load_agents(&agents_path)?;

    // 3.5 Discover skills ([skills] enabled gates discovery; warnings are
    //     logged, never fatal). The catalog feeds both the RunManager (system
    //     prompt injection) and the builtin `read_skill` MCP server below.
    let skills = load_skills(&config, &config_path, &cwd);
    let skill_infos: Vec<openslate_core::mcp::SkillInfo> =
        skills.skills().iter().map(From::from).collect();

    // 4. Validate config
    let errors = validate_config(&config, &agents);
    if !errors.is_empty() {
        for err in &errors {
            tracing::error!("Validation error: {} — {}", err.field, err.message);
        }
        anyhow::bail!(
            "Configuration validation failed with {} error(s)",
            errors.len()
        );
    }

    // 5. Initialize SQLite store
    let store = init_store(&config, &paths).await?;

    // 6. Build agent tree
    let agent_tree = AgentTree::from_configs(&agents.agents)
        .map_err(|e| anyhow::anyhow!("Failed to build agent tree: {}", e))?;

    // 7. Build tool registry.
    let mut registry = ToolRegistry::new();

    // 7.2 Builtin tool servers: in-process MCP (fs / shell / edit / skills),
    //     started without any config; `[builtin_tools]` toggles gate the
    //     individual fs/shell/edit tools, while the skill server is gated by
    //     the catalog being non-empty (driven by `[skills] enabled`).
    //     Tools register under bare names; a collision (only possible with an
    //     external MCP server claiming a builtin name) is a hard error —
    //     unlike external servers, a builtin failing to start is a bug, not an
    //     environment problem, so it aborts startup.
    let mut mcp_connections = openslate_core::mcp::McpConnectionGuard::new();
    {
        let (builtin_tools, builtin_services) =
            openslate_core::mcp::connect_builtin_servers(&cwd, &config.builtin_tools, skill_infos)
                .await?;
        let mut names = Vec::with_capacity(builtin_tools.len());
        for tool in builtin_tools {
            names.push(tool.exposed_name().to_owned());
            if let Err(e) = registry.try_register(tool) {
                anyhow::bail!(
                    "builtin tool name conflict: '{}' already registered (an external MCP \
                     server may claim a builtin tool name)",
                    e.0
                );
            }
        }
        if !names.is_empty() {
            tracing::info!(
                target: "openslate_mcp",
                "builtin tools registered: [{}]",
                names.join(", ")
            );
        }
        for service in builtin_services {
            mcp_connections.push(service);
        }
    }

    // 7.5 Connect external MCP servers and register their tools.
    //     Static config errors were already caught by validate_config; here we
    //     handle runtime failures (spawn/handshake/list) with warn+skip so one
    //     bad server cannot abort startup. Name collisions are a hard error.
    if let Some(mcp) = &config.mcp {
        use tokio::sync::mpsc;

        // Disabled servers: log and skip (no subprocess spawned).
        for (name, server_cfg) in &mcp.servers {
            if !server_cfg.enabled {
                tracing::info!(target: "openslate_mcp", "MCP server '{name}' disabled, skipping");
            }
        }

        // Spawn one task per enabled server to connect concurrently, and log +
        // register each one THE MOMENT it finishes (delivered in completion order
        // via the channel) — instead of waiting for all connects before logging.
        // Registration still happens on this task (&mut registry, sequential),
        // but it's pure in-memory HashMap inserts, trivially fast.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut pending = 0usize;
        for (name, cfg) in &mcp.servers {
            if !cfg.enabled {
                continue;
            }
            let name = name.clone();
            let cfg = cfg.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let start = std::time::Instant::now();
                let result = openslate_core::mcp::connect_mcp_server(&name, &cfg).await;
                // `elapsed` is captured here, at this server's connect completion
                // — independent of when sibling servers (or the registry loop) run.
                let _ = tx.send((name, start.elapsed(), result));
            });
            pending += 1;
        }
        drop(tx);

        for _ in 0..pending {
            let (name, elapsed, result) = match rx.recv().await {
                Some(msg) => msg,
                None => break, // all senders dropped unexpectedly (e.g. a task panicked)
            };
            match result {
                Ok((tools, service)) => {
                    let mut names = Vec::with_capacity(tools.len());
                    for tool in tools {
                        names.push(tool.exposed_name().to_owned());
                        match registry.try_register(tool) {
                            Ok(()) => {}
                            Err(e) => {
                                anyhow::bail!(
                                    "MCP tool name conflict: tool '{}' from server '{}' collides \
                                     with an existing tool (MCP tools are auto-namespaced as \
                                     '{name}_*'; a collision means two servers share a name)",
                                    e.0,
                                    name
                                );
                            }
                        }
                    }
                    tracing::info!(
                        target: "openslate_mcp",
                        "MCP server '{name}': {} tools registered in {:.2}s",
                        names.len(),
                        elapsed.as_secs_f32()
                    );
                    tracing::debug!(
                        target: "openslate_mcp",
                        "MCP server '{name}' tool list: [{}]",
                        names.join(", ")
                    );
                    mcp_connections.push(service);
                }
                Err(e) => {
                    tracing::warn!(
                        target: "openslate_mcp",
                        "MCP server '{name}' failed to connect after {:.2}s, skipped: {e}",
                        elapsed.as_secs_f32()
                    );
                }
            }
        }
    }

    // 7.6 PTC diagnostics: exact-name `[ptc.tool_modes]` entries that match
    //     no registered tool are almost certainly config drift (a renamed
    //     tool, a typo, or a not-yet-connected MCP server's tool) and would
    //     silently never apply. Glob patterns are skipped — matching nothing
    //     yet is legitimate for them. Never fatal (the same warning style as
    //     the skills diagnostics).
    if config.ptc.enabled {
        let registered = registry.tool_names();
        let invalid: Vec<&str> = config
            .ptc
            .tool_modes
            .keys()
            .filter(|pattern| {
                !pattern.contains('*')
                    && !pattern.contains('?')
                    && !registered.iter().any(|name| name == *pattern)
            })
            .map(String::as_str)
            .collect();
        if !invalid.is_empty() {
            tracing::warn!(
                target: "openslate_ptc",
                "ptc tool_modes reference unknown tool(s): [{}] — these entries will never \
                 match a registered tool (check for renames/typos)",
                invalid.join(", ")
            );
        }
    }

    // 8. Resolve the root agent to determine which model to use (informational;
    //    the provider itself is built per run/turn via build_provider_for_model).
    let root_agent = agent_tree.get_root();
    let model_alias = root_agent.model_alias.clone();
    tracing::debug!(
        "Root agent '{}' uses model '{}'",
        root_agent.id,
        model_alias
    );

    // 9. Create RunManager
    let manager = RunManager::new(config.clone(), agent_tree.clone(), registry, skills.clone());

    Ok(AppContext {
        config,
        agents,
        store,
        agent_tree,
        manager,
        skills,
        mcp_connections,
        config_path,
        agents_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a temp dir with valid config + agents for testing.
    fn temp_project() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().expect("create temp dir");
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
        let agents_dir = openslate_dir.join("agents");
        fs::create_dir(&agents_dir).expect("create agents dir");
        let agent_md = "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - read_file\n---\nYou are the root agent.\n";
        fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        fs::write(agents_dir.join("root.md"), agent_md).expect("write root.md");
        tmp
    }

    #[test]
    fn test_load_config_valid() {
        let tmp = temp_project();
        let path = tmp.path().join(".openslate/openslate.toml");
        let config = load_config(&path).expect("should load");
        assert!(config.models.contains_key("main"));
    }

    #[test]
    fn test_load_config_missing_file() {
        let result = load_config(Path::new("/nonexistent/openslate.toml"));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Failed to read") || msg.contains("Failed to parse"));
    }

    #[test]
    fn test_load_agents_valid() {
        let tmp = temp_project();
        let path = tmp.path().join(".openslate/agents");
        let agents = load_agents(&path).expect("should load");
        assert_eq!(agents.agents.len(), 1);
        assert_eq!(agents.agents[0].id.0, "root");
    }

    #[test]
    fn test_load_agents_missing_file() {
        let result = load_agents(Path::new("/nonexistent/agents"));
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_config_file_explicit() {
        let tmp = temp_project();
        let path = tmp.path().join(".openslate/openslate.toml");
        let resolved = resolve_config_file(Some(path.to_str().unwrap())).expect("should resolve");
        assert_eq!(resolved, path);
    }

    #[test]
    fn test_resolve_config_file_missing_explicit() {
        let result = resolve_config_file(Some("/nonexistent/openslate.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_agents_dir() {
        let path = Path::new("/project/.openslate/openslate.toml");
        let agents = resolve_agents_dir(path);
        assert_eq!(agents, Path::new("/project/.openslate/agents"));
    }

    #[tokio::test]
    async fn test_init_store_creates_database() {
        let tmp = tempfile::TempDir::new().expect("create temp dir");
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"
"#;
        let config = parse_openslate_toml(toml).expect("parse config");
        let paths = resolve_paths(tmp.path());
        let store = init_store(&config, &paths)
            .await
            .expect("store should init");
        assert!(store.is_some(), "store should be Some");
    }

    #[tokio::test]
    async fn test_init_store_with_explicit_relative_path() {
        let tmp = tempfile::TempDir::new().expect("create temp dir as data dir");
        let toml = r#"
[database]
path = "data/test.sqlite"
"#;
        let config = parse_openslate_toml(toml).expect("parse");
        let mut paths = resolve_paths(tmp.path());
        paths.global_data_dir = tmp.path().to_path_buf();
        paths.database_path = tmp.path().join("openslate.sqlite");
        let store = init_store(&config, &paths)
            .await
            .expect("store should init");
        assert!(store.is_some());
        assert!(tmp.path().join("data/test.sqlite").exists());
    }

    #[tokio::test]
    async fn test_init_store_with_absolute_path() {
        let tmp = tempfile::TempDir::new().expect("create temp dir");
        let db_file = tmp.path().join("custom.db");
        let toml = format!(
            r#"
[database]
path = "{}"
"#,
            db_file.display()
        );
        let config = parse_openslate_toml(&toml).expect("parse");
        let paths = resolve_paths(tmp.path());
        let store = init_store(&config, &paths)
            .await
            .expect("store should init");
        assert!(store.is_some());
        assert!(db_file.exists());
    }

    #[test]
    fn test_validation_with_valid_config() {
        let tmp = temp_project();
        let config = load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        let agents = load_agents(&tmp.path().join(".openslate/agents")).unwrap();
        let errors = validate_config(&config, &agents);
        assert!(
            errors.is_empty(),
            "valid config should have no errors: {errors:?}"
        );
    }

    // ── skills_sources ───────────────────────────────────────────────────

    #[test]
    fn test_skills_sources_orders_low_to_high() {
        let cwd = Path::new("/project");
        let config_path = Path::new("/project/.openslate/openslate.toml");
        let sources = skills_sources(config_path, cwd);
        let home = dirs::home_dir().expect("home dir");
        let global = resolve_paths(cwd).global_config_dir.join("skills");
        assert_eq!(sources[0], home.join(".agents").join("skills"));
        assert_eq!(sources[1], global);
        assert_eq!(sources[2], PathBuf::from("/project/.agents/skills"));
        assert_eq!(
            sources[3],
            PathBuf::from("/project/.openslate/skills"),
            "config-parent skills dir has the highest precedence"
        );
    }

    #[test]
    fn test_skills_sources_dedupes_global_config_duplicates() {
        // Running off the global config: source 2 (XDG global) and source 4
        // (config parent) are the same directory — the LATER occurrence must
        // be the one kept, and it appears only once.
        let cwd = Path::new("/nowhere/.openslate-absent"); // no local config
        let config_path = resolve_paths(cwd).global_config_dir.join("openslate.toml");
        let sources = skills_sources(&config_path, cwd);
        let global_skills =
            canonical_source(&resolve_paths(cwd).global_config_dir.join("skills"), cwd);
        assert_eq!(
            sources.iter().filter(|s| **s == global_skills).count(),
            1,
            "duplicate dir listed exactly once: {sources:?}"
        );
        assert_eq!(
            *sources.last().unwrap(),
            global_skills,
            "the later (higher-precedence) occurrence is kept"
        );
    }

    #[test]
    fn test_skills_sources_relative_config_is_absolutized() {
        // `--config .openslate/openslate.toml` typed from `cwd`: source 4
        // must come out absolute (and canonical), not `.openslate/skills`.
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let cwd = tmp.path();
        fs::create_dir_all(cwd.join(".openslate/skills")).expect("mkdir");

        let sources = skills_sources(Path::new(".openslate/openslate.toml"), cwd);
        assert!(
            sources.iter().all(|s| s.is_absolute()),
            "all sources absolute: {sources:?}"
        );
        assert_eq!(
            *sources.last().unwrap(),
            cwd.join(".openslate").join("skills"),
            "relative config parent resolves against cwd"
        );
    }

    #[test]
    fn test_skills_sources_bare_config_filename_resolves_against_cwd() {
        // `--config openslate.toml` (bare filename, empty parent) must not
        // push a bare relative `skills` source.
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let cwd = tmp.path();

        let sources = skills_sources(Path::new("openslate.toml"), cwd);
        assert!(
            sources.iter().all(|s| s.is_absolute()),
            "no relative sources: {sources:?}"
        );
        assert_eq!(
            *sources.last().unwrap(),
            cwd.join("skills"),
            "empty config parent resolves against cwd"
        );
    }

    #[test]
    fn test_skills_sources_relative_config_does_not_double_scan() {
        // `--config .agents/openslate.toml` (relative) spells the same
        // physical dir for source 4 as source 3 (cwd/.agents/skills): the
        // dedupe must see them as equal instead of scanning the dir twice
        // and emitting a spurious self-shadow warning.
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let cwd = tmp.path().to_path_buf();
        let skills_dir = cwd.join(".agents").join("skills");
        write_skill(
            &skills_dir,
            "dedupe-probe",
            "name: dedupe-probe\ndescription: d\n",
            "b",
        );

        let sources = skills_sources(Path::new(".agents/openslate.toml"), &cwd);
        let mut sorted = sources.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            sources.len(),
            "no duplicate sources: {sources:?}"
        );
        assert_eq!(
            sources
                .iter()
                .filter(|s| s.starts_with(&cwd) && s.ends_with(".agents/skills"))
                .count(),
            1,
            "the shared dir is listed exactly once: {sources:?}"
        );

        let (_, warnings) = discover_skills(&sources);
        let self_shadows: Vec<_> = warnings
            .iter()
            .filter(|w| w.path.starts_with(&cwd) && w.message.contains("shadows"))
            .collect();
        assert!(
            self_shadows.is_empty(),
            "same dir scanned twice would self-shadow: {self_shadows:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_skills_sources_symlinked_spelling_dedupes() {
        // cwd spelled through a symlink, config spelled through the real
        // path: canonicalization must make sources 3 and 4 compare equal.
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let real = tmp.path().join("real");
        fs::create_dir_all(real.join(".agents").join("skills")).expect("mkdir");
        std::os::unix::fs::symlink(&real, tmp.path().join("link")).expect("symlink");

        let sources = skills_sources(
            &real.join(".agents/openslate.toml"),
            &tmp.path().join("link"),
        );
        let mut sorted = sources.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            sources.len(),
            "symlinked spelling must not evade dedupe: {sources:?}"
        );
    }

    // ── build_app_context skills wiring ──────────────────────────────────

    /// `temp_project` plus a `[database] path` inside the temp dir (so
    /// `build_app_context` never touches the real global data dir).
    fn temp_project_isolated_db() -> tempfile::TempDir {
        let tmp = temp_project();
        let toml_path = tmp.path().join(".openslate/openslate.toml");
        let base = fs::read_to_string(&toml_path).expect("read toml");
        let toml = format!(
            "{base}\n[database]\npath = \"{}\"\n",
            tmp.path().join("test.sqlite").display()
        );
        fs::write(&toml_path, toml).expect("write toml");
        tmp
    }

    fn write_skill(root: &Path, dir_name: &str, frontmatter: &str, body: &str) {
        let dir = root.join(dir_name);
        fs::create_dir_all(&dir).expect("create skill dir");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\n{frontmatter}---\n{body}"),
        )
        .expect("write SKILL.md");
    }

    #[tokio::test]
    async fn test_build_app_context_discovers_project_skills() {
        let tmp = temp_project_isolated_db();
        write_skill(
            &tmp.path().join(".openslate/skills"),
            "demo",
            "name: demo\ndescription: demo skill\n",
            "demo body",
        );

        let config_path = tmp.path().join(".openslate/openslate.toml");
        let ctx = build_app_context(config_path.to_str()).await.expect("ctx");
        assert!(!ctx.skills.is_empty(), "catalog should be non-empty");
        let skill = ctx.skills.get("demo").expect("demo skill discovered");
        assert_eq!(skill.body, "demo body");
        // The manager carries its own copy for prompt injection.
        assert_eq!(ctx.manager.skills.get("demo").unwrap().name, "demo");
    }

    #[tokio::test]
    async fn test_build_app_context_disabled_skills_yield_empty_catalog() {
        let tmp = temp_project_isolated_db();
        write_skill(
            &tmp.path().join(".openslate/skills"),
            "demo",
            "name: demo\ndescription: demo skill\n",
            "demo body",
        );
        let toml_path = tmp.path().join(".openslate/openslate.toml");
        let base = fs::read_to_string(&toml_path).expect("read toml");
        fs::write(&toml_path, format!("{base}\n[skills]\nenabled = false\n")).expect("write");

        let config_path = tmp.path().join(".openslate/openslate.toml");
        let ctx = build_app_context(config_path.to_str()).await.expect("ctx");
        assert!(ctx.skills.is_empty(), "disabled → empty catalog");
        assert!(ctx.manager.skills.is_empty());
    }

    #[tokio::test]
    async fn test_build_app_context_bad_skill_warns_but_others_load() {
        let tmp = temp_project_isolated_db();
        let skills_dir = tmp.path().join(".openslate/skills");
        write_skill(
            &skills_dir,
            "good",
            "name: good\ndescription: g\n",
            "good body",
        );
        // Malformed frontmatter (no closing delimiter): warned + skipped.
        let bad_dir = skills_dir.join("bad");
        fs::create_dir_all(&bad_dir).expect("create bad dir");
        fs::write(bad_dir.join("SKILL.md"), "name: bad\n(no closing ---\n").expect("write bad");

        let config_path = tmp.path().join(".openslate/openslate.toml");
        let ctx = build_app_context(config_path.to_str()).await.expect("ctx");
        assert!(
            ctx.skills.get("good").is_some(),
            "valid skill still loads next to a broken one"
        );
        assert!(
            ctx.skills.get("bad").is_none(),
            "broken skill is skipped (warning logged, startup proceeds)"
        );
    }

    // ── Approval wiring (Phase 1) ────────────────────────────────────────

    use openslate_core::approval::{ApprovalPolicy, RiskLevel};

    fn auto_except(tools: &[&str]) -> ApprovalPolicy {
        ApprovalPolicy::AutoExcept(tools.iter().map(|t| (*t).to_owned()).collect())
    }

    #[test]
    fn test_derive_effective_policy_full_matrix() {
        // (configured, interactive, yes) → expected. --yes wins over
        // everything; else the configured policy wins; else interactive
        // sessions get the auto_except([shell, run_code]) default and
        // non-interactive runs get plain auto.
        let manual = Some(ApprovalPolicy::Manual);
        let custom = Some(auto_except(&["write_file"]));
        let cases: &[(Option<ApprovalPolicy>, bool, bool, ApprovalPolicy)] = &[
            // --yes forces auto regardless of config or interactivity.
            (None, false, true, ApprovalPolicy::Auto),
            (None, true, true, ApprovalPolicy::Auto),
            (manual.clone(), false, true, ApprovalPolicy::Auto),
            (manual.clone(), true, true, ApprovalPolicy::Auto),
            (custom.clone(), false, true, ApprovalPolicy::Auto),
            (custom.clone(), true, true, ApprovalPolicy::Auto),
            // Configured policy wins without --yes.
            (manual.clone(), false, false, ApprovalPolicy::Manual),
            (manual.clone(), true, false, ApprovalPolicy::Manual),
            (custom.clone(), false, false, auto_except(&["write_file"])),
            (custom, true, false, auto_except(&["write_file"])),
            // No section: interactive default vs non-interactive default.
            (None, true, false, auto_except(&["shell", "run_code"])),
            (None, false, false, ApprovalPolicy::Auto),
            // Some(auto) explicitly configured behaves like the default.
            (
                Some(ApprovalPolicy::Auto),
                true,
                false,
                ApprovalPolicy::Auto,
            ),
            (
                Some(ApprovalPolicy::Auto),
                false,
                false,
                ApprovalPolicy::Auto,
            ),
        ];
        for (configured, interactive, yes, expected) in cases {
            assert_eq!(
                derive_effective_policy(configured.clone(), *interactive, *yes),
                *expected,
                "configured={configured:?} interactive={interactive} yes={yes}"
            );
        }
    }

    #[test]
    fn test_repl_default_tools_are_shell_and_run_code() {
        assert_eq!(REPL_DEFAULT_APPROVAL_TOOLS, ["shell", "run_code"]);
    }

    #[test]
    fn test_noninteractive_gate_denies_high_risk() {
        let req = ApprovalRequest {
            tool_name: "run_code".to_owned(),
            arguments: serde_json::json!({"code": "async () => 1"}),
            agent_id: "root".to_owned(),
            risk_level: RiskLevel::High,
        };
        match NonInteractiveGate.decide(&req) {
            ApprovalDecision::Denied(reason) => {
                assert!(reason.contains("--yes"), "reason: {reason}");
            }
            other => panic!("expected denial, got {other:?}"),
        }
    }

    #[test]
    fn test_noninteractive_gate_approves_lower_risk() {
        for risk in [RiskLevel::Low, RiskLevel::Medium] {
            let req = ApprovalRequest {
                tool_name: "read_file".to_owned(),
                arguments: serde_json::json!({}),
                agent_id: "root".to_owned(),
                risk_level: risk,
            };
            assert_eq!(NonInteractiveGate.decide(&req), ApprovalDecision::Approved);
        }
    }

    #[test]
    fn test_apply_approval_installs_derived_policy_on_manager() {
        let build_manager = |toml: &str| -> RunManager {
            let config = parse_openslate_toml(toml).expect("parse");
            let agents = AgentsConfig {
                agents: vec![openslate_core::types::AgentConfig {
                    id: openslate_core::types::AgentId("root".into()),
                    name: "Root".into(),
                    model: "main".into(),
                    children: vec![],
                    tools: vec![],
                    default_prompt: "p".into(),
                }],
            };
            let tree = AgentTree::from_configs(&agents.agents).expect("tree");
            RunManager::new(config, tree, ToolRegistry::new(), SkillsCatalog::default())
        };
        let base = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"
"#;

        // Non-interactive + manual config (no --yes) → Manual.
        let mut manager = build_manager(&format!("{base}\n[approval]\npolicy = \"manual\"\n"));
        let cfg = manager.config.clone();
        assert_eq!(
            apply_approval(&mut manager, &cfg, false, false),
            ApprovalPolicy::Manual
        );

        // --yes overrides the configured manual policy → Auto.
        let mut manager = build_manager(&format!("{base}\n[approval]\npolicy = \"manual\"\n"));
        let cfg = manager.config.clone();
        assert_eq!(
            apply_approval(&mut manager, &cfg, false, true),
            ApprovalPolicy::Auto
        );
        assert_eq!(manager.approval.policy(), &ApprovalPolicy::Auto);

        // No section + non-interactive → Auto (no gate, no callback).
        let mut manager = build_manager(base);
        let cfg = manager.config.clone();
        assert_eq!(
            apply_approval(&mut manager, &cfg, false, false),
            ApprovalPolicy::Auto
        );
    }
}
