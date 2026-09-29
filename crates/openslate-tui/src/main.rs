//! openslate-tui — the terminal agent frontend.
//!
//! Startup order (frozen):
//! 1. clap (`--config` top-level like the CLI, `--log-level` default info);
//! 2. file tracing to `~/.local/share/openslate/logs/tui.log` — THE only
//!    debug window into the child-agent delegation chain (sub-agent
//!    events surface only as tracing output, see spec D5);
//! 3. panic hook that synchronously restores the terminal BEFORE chaining
//!    into the original hook (release builds use `panic = "abort`, so
//!    `Drop`-based restoration is unreliable);
//! 4. `build_app_context` (config → validation → store → agents → tools);
//! 5. raw mode + alternate screen + bracketed paste;
//! 6. the App main loop (multi-thread runtime — `current_thread` is
//!    forbidden: the approval `decide()` blocks a worker thread).
//!
//! Exit order: App shutdown (cancel → deny approvals → await engine with
//! timeout → finalize run row) → terminal restore (symmetric with init) →
//! log flush → one stdout summary line (run_id / turns / cost).

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::fmt::MakeWriter;

use openslate_tui::app::{App, SessionSummary};
use openslate_tui::icons::Icons;
use openslate_tui::theme::{Theme, ThemeMode};

/// Log-file truncation threshold: startup truncates an existing log
/// larger than this (simple size cap, spec D5).
const LOG_TRUNCATE_BYTES: u64 = 1 << 20; // 1 MiB

#[derive(Parser)]
#[command(name = "openslate-tui")]
#[command(version, about = "OpenSlate TUI — terminal agent frontend")]
struct Cli {
    /// Path to openslate.toml config file (default: local/global resolution
    /// identical to the CLI).
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
/// precedence as the CLI); `rmcp` is quieted (its INFO logs are multi-KB).
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
        "openslate-tui session ended — run_id: {run_id} | turns: {} | cost: ${:.4}",
        summary.turns, summary.total_cost_usd
    );
}

// ── Entry point ────────────────────────────────────────────────────────────

#[tokio::main] // default multi-thread runtime (MANDATORY — see module docs)
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let (log_path, log_handle) = open_log_file()?;
    init_logging(&cli.log_level, log_handle.clone())?;
    tracing::info!("openslate-tui starting (log: {})", log_path.display());

    install_panic_hook();

    // Wire the engine context (may print startup errors to the normal
    // screen — the TUI has not taken over the terminal yet).
    let ctx = match openslate_app::wiring::build_app_context(cli.config.as_deref()).await {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("启动失败: {e:#}");
            return Err(e);
        }
    };

    let terminal = init_terminal()?;
    // theme-1: the icon tier rides inside the theme (single source).
    // icons-4: user-level per-slot overrides from `[tui.icons]
    // overrides` stack on top of the CLI tier — missing glyphs
    // self-heal per font. Unknown slots / empty glyphs are skipped
    // with a warning (patch_field is the validator).
    let overrides = &ctx.config.tui.icons.overrides;
    let icons = if overrides.is_empty() {
        cli.icons
    } else {
        let mut base = cli.icons.set();
        let mut applied = 0usize;
        for (slot, glyph) in overrides {
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
    tracing::info!(icons = ?icons, theme = ?cli.theme, "ui appearance");
    let app = App::new(ctx).with_theme(theme);
    let result = app.run(terminal).await;

    // Restore first so errors/summary render on the normal screen.
    restore_terminal();

    match result {
        Ok(summary) => {
            print_summary(&summary);
            // Flush logs (spec: 恢复终端 → flush 日志 → 摘要 → exit).
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
