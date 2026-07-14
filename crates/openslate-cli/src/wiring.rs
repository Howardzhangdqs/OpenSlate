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
use std::path::Path;

use openslate_core::agent_tree::AgentTree;
use openslate_core::config::validation::validate_config;
use openslate_core::config::{parse_agents_dir, parse_openslate_toml, AgentsConfig, OpenSlateConfig};
use openslate_core::paths::{resolve_paths, OpenSlatePaths};
use openslate_core::run_manager::RunManager;
use openslate_core::tool::builtin_registry;
use openslate_store_sqlite::store::SqliteStore;

/// Fully assembled application context ready for running agents.
#[allow(dead_code)]
pub struct AppContext {
    pub config: OpenSlateConfig,
    pub agents: AgentsConfig,
    pub store: Option<SqliteStore>,
    pub agent_tree: AgentTree,
    pub manager: RunManager,
    /// Live MCP server connections. Declared after `manager` so that on drop,
    /// the registry (and its `McpTool`s holding `ServerSink` clones) is dropped
    /// *before* the connections themselves are cancelled — avoiding any window
    /// where a tool could outlive its transport.
    #[cfg(feature = "mcp")]
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
    parse_agents_dir(agents_dir)
        .with_context(|| format!("Failed to parse agents directory '{}'", agents_dir.display()))
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

/// Initialize the SQLite store based on config.
///
/// Creates the database file (if file-based) and runs migrations.
/// Returns `None` if store initialization should be skipped (e.g. missing config).
async fn init_store(config: &OpenSlateConfig, paths: &OpenSlatePaths) -> Result<Option<SqliteStore>> {
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
                format!(
                    "Failed to create database directory '{}'",
                    parent.display()
                )
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
                Err(e) => tracing::warn!(
                    "Failed to load .env at {}: {}",
                    env_path.display(),
                    e
                ),
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

    // 7. Build tool registry
    #[allow(unused_mut)] // `mut` is only exercised when the `mcp` feature is on.
    let mut registry = builtin_registry();

    // 7.5 Connect MCP servers and register their tools (feature-gated).
    //     Static config errors were already caught by validate_config; here we
    //     handle runtime failures (spawn/handshake/list) with warn+skip so one
    //     bad server cannot abort startup. Name collisions are a hard error.
    #[cfg(feature = "mcp")]
    let mut mcp_connections = openslate_core::mcp::McpConnectionGuard::new();
    #[cfg(feature = "mcp")]
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
                                    e.0, name
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

    // 8. Resolve the root agent to determine which model to use (informational;
    //    the provider itself is built per run/turn via build_provider_for_model
    //    so it can be dispatched on ProviderConfig.kind).
    let root_agent = agent_tree.get_root();
    let model_alias = root_agent.model_alias.clone();
    tracing::debug!("Root agent '{}' uses model '{}'", root_agent.id, model_alias);

    // 9. Create RunManager
    let manager = RunManager::new(config.clone(), agent_tree.clone(), registry);

    Ok(AppContext {
        config,
        agents,
        store,
        agent_tree,
        manager,
        #[cfg(feature = "mcp")]
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
        let agents_dir = openslate_dir.join("agents");
        fs::create_dir(&agents_dir).expect("create agents dir");
        let agent_md = "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - current_time\n---\nYou are the root agent.\n";
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
        let resolved =
            resolve_config_file(Some(path.to_str().unwrap())).expect("should resolve");
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
kind = "openai_compatible"
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
}
