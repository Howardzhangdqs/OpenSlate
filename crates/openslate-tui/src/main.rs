//! openslate-tui — the terminal agent frontend (web-1 client edition).
//!
//! Startup order:
//! 1. clap (`--server` optional — flag > `OPENSLATE_SERVER` env >
//!    server.json auto-discovery, see `discovery`; `--config` now scopes
//!    the LOCAL display prefs only, `--log-level` default info);
//! 2. file tracing to `~/.local/share/openslate/logs/tui.log` — THE only
//!    debug window into the client/server link;
//! 3. panic hook that synchronously restores the terminal BEFORE chaining
//!    into the original hook (release builds use `panic = "abort"`, so
//!    `Drop`-based restoration is unreliable);
//! 4. server link (initial ladder 0.5s/1s/2s; exhaustion exits 1);
//! 5. raw mode + alternate screen + bracketed paste;
//! 6. the App main loop.
//!
//! Exit order: App teardown (link drop — the transport task exits with
//! the channel) → terminal restore (symmetric with init) → log flush →
//! one stdout summary line (session_id / turns / cost).
//!
//! GAP-2 (display prefs): `[tui]`/`[tui.icons]` stays a LOCAL,
//! read-only concern — the client never builds an engine context, but
//! it still reads the same config discovery chain for the icon
//! overrides (flag > env > local `[tui]` section).

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::fmt::MakeWriter;

use openslate_tui::app::{App, ClientBootstrap, SessionSummary};
use openslate_tui::client;
use openslate_tui::icons::Icons;
use openslate_tui::theme::{Theme, ThemeMode};

/// Log-file truncation threshold: startup truncates an existing log
/// larger than this (simple size cap).
const LOG_TRUNCATE_BYTES: u64 = 1 << 20; // 1 MiB

#[derive(Parser)]
#[command(name = "openslate-tui")]
#[command(
    version,
    about = "OpenSlate TUI — terminal agent frontend (server client)"
)]
struct Cli {
    /// Server URL to attach to (e.g. `ws://127.0.0.1:7800`; a bare
    /// host[:port] gets `ws://` and `/api/ws` appended). Precedence:
    /// flag > `OPENSLATE_SERVER` env > server.json auto-discovery
    /// (written by `openslate serve` into the config dir); all absent
    /// → error exit 1.
    #[arg(long, global = true, env = "OPENSLATE_SERVER")]
    server: Option<String>,

    /// Optional auth token (see `openslate serve --auth-token`);
    /// read from the environment only — never typed into a flag that
    /// shell history keeps.
    #[arg(long, global = true, env = "OPENSLATE_TOKEN")]
    token: Option<String>,

    /// Path of a LOCAL config file to read display preferences
    /// (`[tui]`/`[tui.icons]` only — the server owns everything else;
    /// default: local/global discovery, same file chain the CLI uses).
    #[arg(long, global = true)]
    config: Option<String>,

    /// Log level for ~/.local/share/openslate/logs/tui.log
    /// (trace, debug, info, warn, error).
    #[arg(long, global = true, default_value = "info")]
    log_level: String,

    /// Icon tier: `unicode` (default, geometric symbols), `nerd`
    /// (Nerd Font PUA markers), `ascii` (pure printable-ASCII glyphs —
    /// the dumb-terminal/CI floor). Precedence: flag >
    /// `OPENSLATE_ICONS` env > default; invalid values exit with an
    /// error.
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "unicode",
        env = "OPENSLATE_ICONS"
    )]
    icons: Icons,

    /// Color theme: `dark` (default), `light`, or `ansi` (256-color
    /// degraded palette for no-truecolor terminals). Precedence: flag >
    /// `OPENSLATE_THEME` env > default.
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "dark",
        env = "OPENSLATE_THEME"
    )]
    theme: ThemeMode,
}

// ── File logging (custom MakeWriter) ───────────────────────────────────────

/// Shared handle to the log file; also kept by main for the final flush.
type LogHandle = Arc<Mutex<fs::File>>;

/// `MakeWriter` adapter: every `make_writer` call hands out a guard over
/// the shared file (tracing serializes writes through it).
#[derive(Clone)]
struct LogWriter(LogHandle);

impl<'a> MakeWriter<'a> for LogWriter {
    type Writer = LogWriteGuard;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriteGuard(Arc::clone(&self.0))
    }
}

struct LogWriteGuard(LogHandle);

impl std::io::Write for LogWriteGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut file = self.0.lock().unwrap_or_else(|e| e.into_inner());
        file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut file = self.0.lock().unwrap_or_else(|e| e.into_inner());
        file.flush()
    }
}

/// Resolve `~/.local/share/openslate/logs/tui.log` (XDG data dir, home
/// fallback), create parents, truncate an oversized existing log.
fn open_log_file() -> Result<(PathBuf, LogHandle)> {
    let dir = dirs::data_dir()
        .map(|d| d.join("openslate").join("logs"))
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share/openslate/logs")))
        .context("cannot resolve the user data dir for logs")?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create log dir {}", dir.display()))?;
    let path = dir.join("tui.log");

    let truncate = fs::metadata(&path)
        .map(|m| m.len() > LOG_TRUNCATE_BYTES)
        .unwrap_or(false);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .truncate(truncate)
        .open(&path)
        .with_context(|| format!("failed to open log file {}", path.display()))?;
    Ok((path, Arc::new(Mutex::new(file))))
}

/// Local-time log timestamp (`%Y-%m-%d %H:%M:%S`) — avoids the
/// `tracing-subscriber/chrono` feature by implementing `FormatTime`
/// directly (the CLI uses chrono the same way).
struct LocalTimer;

impl tracing_subscriber::fmt::time::FormatTime for LocalTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"))
    }
}

/// Initialize file tracing. `RUST_LOG` overrides `--log-level` (same
/// precedence as the CLI).
fn init_logging(log_level: &str, handle: LogHandle) -> Result<()> {
    let default_directive = format!("{log_level},rmcp=warn");
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&default_directive));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(LogWriter(handle))
        .with_ansi(false) // plain text file
        .with_timer(LocalTimer)
        .init();
    Ok(())
}

// ── Terminal setup / teardown ──────────────────────────────────────────────

/// Enter TUI mode: raw mode + alternate screen + bracketed paste.
fn init_terminal() -> Result<ratatui::DefaultTerminal> {
    crossterm::terminal::enable_raw_mode().context("failed to enable raw mode")?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste,
        // Mouse capture: without it terminals translate the wheel into
        // ↑/↓ keys, which the input box interprets as history switching.
        crossterm::event::EnableMouseCapture
    )
    .context("failed to enter the alternate screen")?;
    let terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
            .context("failed to create the terminal backend")?;
    Ok(terminal)
}

/// Restore the terminal (symmetric with [`init_terminal`]). Idempotent so
/// the panic hook can call it unconditionally.
fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture
    );
}

/// Panic hook: synchronously restore the terminal FIRST (release builds
/// abort on panic — `Drop` never runs), then chain into the original hook
/// so the panic message lands on the normal screen.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original(info);
    }));
}

fn print_summary(summary: &SessionSummary) {
    let run_id = summary.run_id.clone().unwrap_or_else(|| "-".to_owned());
    println!(
        "openslate-tui session ended — session: {run_id} | turns: {} | cost: ${:.4}",
        summary.turns, summary.total_cost_usd
    );
}

// ── Local display preferences (GAP-2) ──────────────────────────────────────

/// Read-only `[tui.icons] overrides` from the local config discovery
/// chain (`--config` flag, else `./.openslate/openslate.toml`, else the
/// global `~/.config/openslate/openslate.toml`). Missing/unparseable
/// files are silently skipped — display prefs must never block startup.
fn local_tui_overrides(config_flag: Option<&str>) -> std::collections::HashMap<String, String> {
    let candidates: Vec<PathBuf> = match config_flag {
        Some(flag) => vec![PathBuf::from(flag)],
        None => {
            let mut v = Vec::new();
            if let Ok(cwd) = std::env::current_dir() {
                v.push(cwd.join(".openslate").join("openslate.toml"));
            }
            if let Some(cfg) = dirs::config_dir() {
                v.push(cfg.join("openslate").join("openslate.toml"));
            }
            v
        }
    };
    for path in candidates {
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        match openslate_core::config::parse_openslate_toml(&content) {
            Ok(cfg) => return cfg.tui.icons.overrides,
            Err(e) => {
                tracing::warn!(
                    "skipping unparseable display-pref config {}: {e}",
                    path.display()
                );
            }
        }
    }
    Default::default()
}

// ── Entry point ────────────────────────────────────────────────────────────

#[tokio::main] // multi-thread runtime (WS tasks + the terminal event stream)
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let (log_path, log_handle) = open_log_file()?;
    init_logging(&cli.log_level, log_handle.clone())?;
    tracing::info!("openslate-tui starting (log: {})", log_path.display());

    install_panic_hook();

    // Attach to the server BEFORE taking over the terminal: connection
    // errors (ladder exhausted) print on the normal screen and exit 1.
    // auto-attach-1: flag/env absent → discover server.json (flag >
    // env > file; see `discovery` module docs).
    let Some(spec) = openslate_tui::discovery::resolve_server(
        cli.server.as_deref(),
        cli.token.clone(),
        cli.config.as_deref(),
    ) else {
        eprintln!("未发现运行中的 server：请先启动 openslate serve，或用 --server 指定地址");
        std::process::exit(1);
    };
    let connection = match client::connect(&spec.url, spec.token.clone()).await {
        Ok(conn) => conn,
        Err(e) => {
            match &spec.discovered_from {
                Some(path) => eprintln!(
                    "发现 {} 但 server 未响应（可能已停止），可重启 openslate serve 或用 --server 指定",
                    path.display()
                ),
                None => eprintln!("连接服务器失败: {e:#}"),
            }
            return Err(e);
        }
    };

    let terminal = init_terminal()?;
    // theme-1: the icon tier rides inside the theme (single source).
    // icons-4: user-level per-slot overrides from the LOCAL
    // `[tui.icons] overrides` stack on top of the CLI tier — missing
    // glyphs self-heal per font. Unknown slots / empty glyphs are
    // skipped with a warning (patch_field is the validator).
    let overrides = local_tui_overrides(cli.config.as_deref());
    let icons = if overrides.is_empty() {
        cli.icons
    } else {
        let mut base = cli.icons.set();
        let mut applied = 0usize;
        for (slot, glyph) in &overrides {
            if glyph.is_empty() || !base.patch_field(slot, glyph) {
                tracing::warn!(
                    target: "openslate_tui",
                    slot = slot.as_str(),
                    "ignoring invalid icon override (unknown slot or empty glyph)"
                );
            } else {
                applied += 1;
            }
        }
        tracing::info!(
            target: "openslate_tui",
            applied,
            total = overrides.len(),
            "icon overrides applied on top of the --icons tier"
        );
        Icons::Custom(Box::leak(Box::new(base)))
    };
    let theme = Theme::from_palette(cli.theme.palette(), icons);
    tracing::info!(icons = ?icons, theme = ?cli.theme, server = %spec.url, "ui appearance");

    // Seed mirrors are empty; the queued first snapshot (hello ack)
    // hydrates config/agents/session on the first loop iteration.
    let seed_config =
        openslate_core::config::parse_openslate_toml("").expect("empty TOML parses to defaults");
    let bootstrap = ClientBootstrap {
        link: Arc::new(connection.link),
        events: connection.events,
        config: seed_config,
        root_agent_id: String::new(),
    };
    let app = App::new(bootstrap).with_theme(theme);
    let result = app.run(terminal).await;

    // Restore first so errors/summary render on the normal screen.
    restore_terminal();

    match result {
        Ok(summary) => {
            print_summary(&summary);
            // Flush logs (恢复终端 → flush 日志 → 摘要 → exit).
            if let Ok(mut file) = log_handle.lock() {
                let _ = file.flush();
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("TUI error: {e:#}");
            Err(e)
        }
    }
}
