//! OpenSlate CLI — command-line interface for the Agent Runtime.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::Path;

#[derive(Parser)]
#[command(name = "openslate")]
#[command(version, about = "OpenSlate — Lightweight Agent Runtime")]
struct Cli {
    /// Path to openslate.toml config file
    #[arg(long, global = true)]
    config: Option<String>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, global = true, default_value = "info")]
    log_level: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new OpenSlate project
    Init {
        /// Project name
        #[arg(long)]
        name: Option<String>,
    },
    /// Validate configuration files
    Validate,
    /// Run an agent
    Run {
        /// Agent ID to run (defaults to root agent)
        #[arg(long)]
        agent: Option<String>,
        /// Input prompt
        #[arg(long)]
        prompt: Option<String>,
        /// Profile to use
        #[arg(long, default_value = "default")]
        profile: String,
    },
}

// ── Default file contents ──────────────────────────────────────────────────

fn default_openslate_toml(project_name: &str) -> String {
    format!(
        r#"[project]
name = "{project_name}"

[database]
path = ".openslate/openslate.sqlite"

[limits]
max_steps = 12
max_depth = 4
max_tool_calls = 20
timeout_ms = 60000

[providers.zhipu]
kind = "openai_compatible"
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key_env = "ZHIPU_API_KEY"

[models.main]
provider = "zhipu"
model = "glm-5.1"
supports_tool_call = true

[trace]
enabled = true
"#,
        project_name = project_name
    )
}

const DEFAULT_AGENTS_YAML: &str = r#"agents:
  - id: root
    name: Root Agent
    model: main
    tools:
      - current_time
      - read_file
      - list_dir
    default_prompt: You are a helpful AI assistant.
"#;

const DEFAULT_PROMPT_MD: &str = "You are a helpful AI assistant.\n";

// ── Init logic ─────────────────────────────────────────────────────────────

/// Initialize a new OpenSlate project in the given directory.
/// Returns `Ok(())` on success or `Ok(())` with a printed message if already
/// initialized.
fn run_init(dir: &Path, name: Option<&str>) -> Result<()> {
    let openslate_dir = dir.join(".openslate");

    if openslate_dir.exists() {
        println!("Already initialized.");
        return Ok(());
    }

    let project_name = name.unwrap_or("my-project");
    let prompts_dir = openslate_dir.join("prompts").join("default");

    fs::create_dir_all(&prompts_dir).with_context(|| {
        format!(
            "Failed to create directory structure at {}",
            openslate_dir.display()
        )
    })?;

    // openslate.toml
    let toml_path = openslate_dir.join("openslate.toml");
    fs::write(&toml_path, default_openslate_toml(project_name)).with_context(|| {
        format!(
            "Failed to write {}",
            toml_path.display()
        )
    })?;

    // agents.yaml
    let agents_path = openslate_dir.join("agents.yaml");
    fs::write(&agents_path, DEFAULT_AGENTS_YAML).with_context(|| {
        format!(
            "Failed to write {}",
            agents_path.display()
        )
    })?;

    // prompts/default/prompt.md
    let prompt_path = prompts_dir.join("prompt.md");
    fs::write(&prompt_path, DEFAULT_PROMPT_MD).with_context(|| {
        format!(
            "Failed to write {}",
            prompt_path.display()
        )
    })?;

    println!("Initialized OpenSlate project in {}", dir.display());
    Ok(())
}

// ── Entry point ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize tracing subscriber
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cli.log_level));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .init();

    match cli.command {
        Commands::Init { name } => {
            let cwd = std::env::current_dir().context("Failed to get current directory")?;
            run_init(&cwd, name.as_deref())?;
        }
        Commands::Validate => {
            anyhow::bail!("validate subcommand is not yet implemented");
        }
        Commands::Run {
            agent: _,
            prompt: _,
            profile: _,
        } => {
            anyhow::bail!("run subcommand is not yet implemented");
        }
    }

    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use tempfile::TempDir;

    // Helper: run init in a temp dir
    fn init_in_temp(name: Option<&str>) -> TempDir {
        let tmp = TempDir::new().expect("create temp dir");
        run_init(tmp.path(), name).expect("init should succeed");
        tmp
    }

    #[test]
    fn test_init_creates_directory_structure() {
        let tmp = init_in_temp(None);
        let openslate = tmp.path().join(".openslate");
        assert!(openslate.is_dir(), ".openslate/ should be a directory");
        assert!(openslate.join("prompts").is_dir());
        assert!(openslate.join("prompts/default").is_dir());
    }

    #[test]
    fn test_init_creates_openslate_toml() {
        let tmp = init_in_temp(Some("test-project"));
        let toml_path = tmp.path().join(".openslate/openslate.toml");
        assert!(toml_path.is_file(), "openslate.toml should exist");

        let content = fs::read_to_string(&toml_path).expect("read toml");
        assert!(content.contains("name = \"test-project\""));
        assert!(content.contains("[database]"));
        assert!(content.contains("[limits]"));
        assert!(content.contains("[providers.zhipu]"));
        assert!(content.contains("[models.main]"));
        assert!(content.contains("[trace]"));
    }

    #[test]
    fn test_init_creates_agents_yaml() {
        let tmp = init_in_temp(None);
        let agents_path = tmp.path().join(".openslate/agents.yaml");
        assert!(agents_path.is_file(), "agents.yaml should exist");

        let content = fs::read_to_string(&agents_path).expect("read agents.yaml");
        assert!(content.contains("id: root"));
        assert!(content.contains("name: Root Agent"));
        assert!(content.contains("model: main"));
    }

    #[test]
    fn test_init_creates_prompts_dir() {
        let tmp = init_in_temp(None);
        let prompt_path = tmp.path().join(".openslate/prompts/default/prompt.md");
        assert!(prompt_path.is_file(), "prompts/default/prompt.md should exist");

        let content = fs::read_to_string(&prompt_path).expect("read prompt.md");
        assert!(content.contains("helpful AI assistant"));
    }

    #[test]
    fn test_init_idempotent() {
        let tmp = TempDir::new().expect("create temp dir");
        // First init — should succeed
        run_init(tmp.path(), None).expect("first init should succeed");

        // Capture the openslate.toml content before second init
        let toml_before = fs::read_to_string(tmp.path().join(".openslate/openslate.toml"))
            .expect("read toml");

        // Second init — should print "already initialized" but not error
        run_init(tmp.path(), None).expect("second init should not error");

        // Verify files were NOT overwritten
        let toml_after = fs::read_to_string(tmp.path().join(".openslate/openslate.toml"))
            .expect("read toml");
        assert_eq!(toml_before, toml_after, "files should not be overwritten");
    }

    #[test]
    fn test_cli_parse_init() {
        let cli = Cli::try_parse_from(["openslate", "init"]).expect("parse init");
        assert!(matches!(cli.command, Commands::Init { name: None }));
    }

    #[test]
    fn test_cli_parse_init_with_name() {
        let cli =
            Cli::try_parse_from(["openslate", "init", "--name", "my-app"]).expect("parse init");
        assert!(matches!(cli.command, Commands::Init { ref name } if name.as_deref() == Some("my-app")));
    }

    #[test]
    fn test_cli_parse_run() {
        let cli =
            Cli::try_parse_from(["openslate", "run", "--prompt", "hello"]).expect("parse run");
        match cli.command {
            Commands::Run {
                agent: None,
                prompt,
                profile,
            } => {
                assert_eq!(prompt.as_deref(), Some("hello"));
                assert_eq!(profile, "default");
            }
            _ => panic!("expected Run command"),
        }
    }

    #[test]
    fn test_cli_parse_validate() {
        let cli = Cli::try_parse_from(["openslate", "validate"]).expect("parse validate");
        assert!(matches!(cli.command, Commands::Validate));
    }

    #[test]
    fn test_cli_global_config_flag() {
        let cli = Cli::try_parse_from([
            "openslate",
            "--config",
            "/tmp/test.toml",
            "--log-level",
            "debug",
            "validate",
        ])
        .expect("parse with globals");
        assert_eq!(cli.config.as_deref(), Some("/tmp/test.toml"));
        assert_eq!(cli.log_level, "debug");
    }
}
