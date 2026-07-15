//! OpenSlate CLI — command-line interface for the Agent Runtime.

mod cmd;
mod input;
mod markdown;
mod repl;
mod spinner;
mod wiring;

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

impl Cli {
    /// Returns true if the selected subcommand has `--quiet` set.
    fn is_quiet(&self) -> bool {
        match &self.command {
            Commands::Run { quiet, .. } | Commands::Chat { quiet, .. } => *quiet,
            _ => false,
        }
    }
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
    Validate {
        /// Use strict mode (warnings treated as errors)
        #[arg(long)]
        strict: bool,
    },
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
        /// Output format (text)
        #[arg(long, default_value = "text")]
        format: String,
        /// Write output to file instead of stdout
        #[arg(long)]
        output: Option<String>,
        /// Override root agent ID
        #[arg(long)]
        root_agent: Option<String>,
        /// Only output final result (suppress step-by-step logs)
        #[arg(long)]
        quiet: bool,
        /// Export Chrome Trace JSON to file after execution
        #[arg(long)]
        trace: Option<String>,
    },
    /// Start an interactive chat session
    Chat {
        /// Profile to use
        #[arg(long, default_value = "default")]
        profile: String,
        /// Model alias to use
        #[arg(long)]
        model: Option<String>,
        /// Suppress non-essential output
        #[arg(long)]
        quiet: bool,
    },
}

// ── Tracing format ────────────────────────────────────────────────────────

/// Custom log formatter: `TIME LEVEL target: message` with ANSI colors. Only
/// the time-of-day (local, millis precision) is shown — no date, no tz suffix.
struct SimpleFormatter;

impl Default for SimpleFormatter {
    fn default() -> Self {
        Self
    }
}

/// ANSI color codes.
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

fn level_color(level: &tracing::Level) -> &'static str {
    match level {
        &tracing::Level::ERROR => "\x1b[31m", // red
        &tracing::Level::WARN => "\x1b[33m",  // yellow
        &tracing::Level::INFO => "\x1b[32m",  // green
        &tracing::Level::DEBUG => "\x1b[36m", // cyan
        &tracing::Level::TRACE => "\x1b[35m", // magenta
    }
}

fn target_color(target: &str) -> &'static str {
    if target.contains("runtime") {
        "\x1b[36m" // cyan for runtime step logs
    } else if target.contains("cmd") {
        "\x1b[35m" // magenta for command logs
    } else {
        "\x1b[90m" // bright-black (gray) for others
    }
}

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for SimpleFormatter
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        record: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let meta = record.metadata();
        let level = meta.level();
        let lc = level_color(level);

        // Timestamp: local time-of-day with millis precision (dim). No date/tz.
        write!(writer, "{}", DIM)?;
        write!(writer, "{}", chrono::Local::now().format("%H:%M:%S%.3f"))?;
        write!(writer, "{} ", RESET)?;

        // Level (colored)
        write!(writer, "{}{}{} ", lc, level, RESET)?;

        // Target (colored by category)
        let tc = target_color(meta.target());
        write!(writer, "{}{}{}: ", tc, meta.target(), RESET)?;

        // Message
        ctx.field_format().format_fields(writer.by_ref(), record)?;
        writeln!(writer)
    }
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
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key_env = "ZHIPU_API_KEY"

[models.main]
provider = "zhipu"
model = "glm-5.1"
supports_tool_call = true

[models.fast]
provider = "zhipu"
model = "glm-5.1"
supports_tool_call = true

[trace]
enabled = true
"#,
        project_name = project_name
    )
}

const DEFAULT_ROOT_AGENT_MD: &str = r#"---
id: root
name: Root Agent
model: main
tools:
    - current_time
    - read_file
    - list_dir
---
You are a helpful AI assistant.
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

    // agents/ directory + root.md
    let agents_dir = openslate_dir.join("agents");
    fs::create_dir(&agents_dir).with_context(|| {
        format!("Failed to create agents directory at {}", agents_dir.display())
    })?;
    let root_agent_path = agents_dir.join("root.md");
    fs::write(&root_agent_path, DEFAULT_ROOT_AGENT_MD).with_context(|| {
        format!("Failed to write {}", root_agent_path.display())
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

    // Initialize tracing subscriber.
    // When --quiet is set, suppress info-level step logs by default.
    let default_level = if cli.is_quiet() {
        "warn".to_owned()
    } else {
        cli.log_level.clone()
    };
    // Default directive: user's `--log-level` governs OpenSlate's own crates,
    // but quiet the chatty `rmcp` INFO logs — its "Service initialized as
    // client" prints the entire server InitializeResult including a multi-KB
    // `instructions` string that swamps startup output. Per-target overrides
    // still work via RUST_LOG (e.g. RUST_LOG="info,rmcp=debug").
    let default_directive = format!("{default_level},rmcp=warn");
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&default_directive));
    tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .event_format(SimpleFormatter::default())
            .init();

    match cli.command {
        Commands::Init { name } => {
            let cwd = std::env::current_dir().context("Failed to get current directory")?;
            run_init(&cwd, name.as_deref())?;
        }
        Commands::Validate { strict } => {
            let config_path = cmd::validate::resolve_config_path(cli.config.as_deref())?;
            let agents_path =
                cmd::validate::resolve_agents_path(config_path.parent().unwrap_or(Path::new(".")));
            cmd::validate::run_validate_command(&config_path, &agents_path, strict)?;
        }
        Commands::Run {
            agent,
            prompt,
            profile,
            format,
            output,
            root_agent,
            quiet,
            trace,
        } => {
            let output_format = format
                .parse::<cmd::run::OutputFormat>()
                .map_err(|e| anyhow::anyhow!("{}", e))?;

            let params = cmd::run::RunParams {
                config_path: cli.config,
                agent,
                prompt,
                profile,
                format: output_format,
                output,
                root_agent,
                quiet,
                trace_path: trace,
            };
            cmd::run::run_run_command(params).await?;
        }
        Commands::Chat {
            profile,
            model: _model,
            quiet,
        } => {
            let ctx = wiring::build_app_context(cli.config.as_deref()).await?;
            let mut session = repl::ReplSession::new(ctx, profile, quiet)?;
            session.run().await?;
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
    fn test_init_creates_agents_directory() {
        let tmp = init_in_temp(None);
        let agents_dir = tmp.path().join(".openslate/agents");
        assert!(agents_dir.is_dir(), "agents/ directory should exist");
        let root_md = agents_dir.join("root.md");
        assert!(root_md.is_file(), "agents/root.md should exist");

        let content = fs::read_to_string(&root_md).expect("read root.md");
        assert!(content.contains("id: root"));
        assert!(content.contains("name: Root Agent"));
        assert!(content.contains("model: main"));
        assert!(content.contains("You are a helpful AI assistant."));
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
            format,
            output: None,
            root_agent: None,
            quiet: false,
            trace: None,
        } => {
                assert_eq!(prompt.as_deref(), Some("hello"));
                assert_eq!(profile, "default");
                assert_eq!(format, "text");
            }
            _ => panic!("expected Run command"),
        }
    }

    #[test]
    fn test_cli_parse_run_all_flags() {
        let cli = Cli::try_parse_from([
            "openslate",
            "run",
            "--prompt",
            "test",
            "--agent",
            "root",
            "--profile",
            "custom",
            "--format",
            "text",
            "--output",
            "/tmp/out.txt",
            "--root-agent",
            "root",
            "--quiet",
        ])
        .expect("parse run with all flags");
        match cli.command {
            Commands::Run {
                agent,
                prompt,
                profile,
                format,
                output,
                root_agent,
                quiet,
                trace,
            } => {
                assert_eq!(agent.as_deref(), Some("root"));
                assert_eq!(prompt.as_deref(), Some("test"));
                assert_eq!(profile, "custom");
                assert_eq!(format, "text");
                assert_eq!(output.as_deref(), Some("/tmp/out.txt"));
                assert_eq!(root_agent.as_deref(), Some("root"));
                assert!(quiet);
                assert_eq!(trace, None);
            }
            _ => panic!("expected Run command"),
        }
    }

    #[test]
    fn test_cli_parse_validate() {
        let cli = Cli::try_parse_from(["openslate", "validate"]).expect("parse validate");
        assert!(matches!(cli.command, Commands::Validate { strict: false }));
    }

    #[test]
    fn test_cli_parse_validate_strict() {
        let cli = Cli::try_parse_from(["openslate", "validate", "--strict"]).expect("parse validate --strict");
        assert!(matches!(cli.command, Commands::Validate { strict: true }));
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
