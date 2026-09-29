//! Overlay render tests (P3 lane-c) — TestBackend assertions for the
//! approval banner and the help overlay through the components' public
//! render surface.
//!
//! The app-level painters (minimum-size guard, exit confirmation) are
//! covered by inline tests in `src/app.rs` (`App::render` itself needs a
//! fully wired App); these tests exercise what an integration test CAN
//! reach: component rendering at controlled sizes.

use openslate_tui::components::{
    AppCtx, ApprovalComponent, Component, ConfigSummary, Focus, HelpComponent, RunInfo, RunState,
};
use openslate_tui::event::ApprovalSummary;
use openslate_tui::theme::Theme;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::Frame;

/// Theme slot accessor for color assertions (theme-1).
fn theme() -> Theme {
    Theme::new()
}

/// Minimal AppCtx for component rendering (only the theme is consumed
/// by these components, but the trait takes the full snapshot).
fn test_ctx() -> AppCtx {
    AppCtx {
        theme: Theme::new(),
        focus: Focus::Input,
        run: RunInfo {
            state: RunState::Idle,
            spinner_frame: 0,
            model_label: "main@mock".into(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            elapsed: None,
            tool_calls_cur: 0,
            depth_cur: 0,
            context_remaining: None,
        },
        config: ConfigSummary {
            model_alias: "main".into(),
            model_id: "mock-model".into(),
            provider_name: "mock".into(),
            max_depth: 4,
            max_tool_calls: 20,
            run_id: None,
            model_aliases: Vec::new(),
        },
        size: (80, 24),
        notice: None,
    }
}

fn summary(args: &str) -> ApprovalSummary {
    ApprovalSummary {
        tool_name: "shell".into(),
        arguments: args.into(),
        agent_id: "root".into(),
        risk_level: "high".into(),
    }
}

/// Render `paint` on a TestBackend and return the screen rows.
///
/// The backend's Display view wraps each row in quotes and appends a
/// "Hidden by multi-width symbols" note after the closing quote when
/// wide glyphs overwrite cells — take exactly the content between the
/// first two quotes.
fn draw(width: u16, height: u16, paint: impl FnOnce(&mut Frame)) -> Vec<String> {
    let mut terminal =
        ratatui::Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|f| paint(f))
        .expect("painting must not panic");
    terminal
        .backend()
        .to_string()
        .lines()
        .map(|l| l.split('"').nth(1).unwrap_or("").to_owned())
        .collect()
}

// ─── Approval banner ───────────────────────────────────────────────────────

/// The banner's request line must FIT one 80-column row (args truncated
/// with an ellipsis instead of wrapping/clipping the closing paren), and
/// the Chinese key hints must sit on the last content row.
#[test]
fn approval_banner_fits_80_columns() {
    let rows = draw(80, 5, |f| {
        let mut approval = ApprovalComponent::new();
        approval.enqueue(1, summary(&"x".repeat(200)));
        approval.render(f, Rect::new(0, 0, 80, 5), &test_ctx());
    });
    // restyle-1: `│` tone track + request line on row 0, meta on
    // row 1, hints on row 2 (blocked marker `\u{25C6}`).
    assert!(
        rows[0].starts_with('│'),
        "tone track claims column 0: {}",
        rows[0]
    );
    assert!(
        rows[0].contains("approve? [root] shell("),
        "request line: {}",
        rows[0]
    );
    // The whole request (including the closing paren) sits on ONE row.
    let request_row = rows[0].trim_end();
    assert!(
        request_row.ends_with(')'),
        "args closed within the row (no wrap): {request_row}"
    );
    assert!(rows[0].contains('…'), "truncation is marked");
    let xs = rows[0].matches('x').count();
    assert!(xs <= 60, "args preview ≤60 display cols, got {xs}");
    // Hints on the third content row (Chinese copy per the brief).
    for needle in ["[y]", "允许", "[n]", "拒绝", "[a]", "全部允许"] {
        assert!(
            rows[2].contains(needle),
            "hints row needs {needle}: {}",
            rows[2]
        );
    }
}

/// Queued requests surface as `+N pending` on the meta row.
#[test]
fn approval_banner_counts_pending_queue() {
    let rows = draw(80, 5, |f| {
        let mut approval = ApprovalComponent::new();
        for id in 1..=3u64 {
            approval.enqueue(id, summary("{}"));
        }
        approval.render(f, Rect::new(0, 0, 80, 5), &test_ctx());
    });
    assert!(rows[1].contains("risk high"), "meta row: {}", rows[1]);
    assert!(rows[1].contains("+2 pending"), "queue suffix: {}", rows[1]);

    let single = draw(80, 5, |f| {
        let mut approval = ApprovalComponent::new();
        approval.enqueue(1, summary("{}"));
        approval.render(f, Rect::new(0, 0, 80, 5), &test_ctx());
    });
    assert!(
        !single[1].contains("pending"),
        "no suffix with one request: {}",
        single[1]
    );
}

/// An empty queue paints nothing (the banner truly disappears).
#[test]
fn approval_banner_empty_queue_is_blank() {
    let rows = draw(80, 5, |f| {
        let approval = ApprovalComponent::new();
        approval.render(f, Rect::new(0, 0, 80, 5), &test_ctx());
    });
    assert!(rows.iter().all(|r| r.trim().is_empty()));
}

/// The tone track is a `\u{2502}` in the plain warning slot covering
/// the FULL banner height, with one blank column before the content.
#[test]
fn approval_banner_tone_track_is_warning_full_height() {
    let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 5)).expect("test terminal");
    terminal
        .draw(|f| {
            let mut approval = ApprovalComponent::new();
            approval.enqueue(1, summary("{}"));
            approval.render(f, Rect::new(0, 0, 80, 5), &test_ctx());
        })
        .expect("painting must not panic");
    let buf = terminal.backend().buffer();
    for y in 0..5u16 {
        let bar = buf.cell((0, y)).expect("bar cell");
        assert_eq!(bar.symbol(), "│", "track glyph at row {y}");
        assert_eq!(
            bar.style().fg,
            theme().warning.fg,
            "track warning at row {y}"
        );
        assert!(
            !bar.modifier.contains(ratatui::style::Modifier::BOLD),
            "track plain (BOLD lives on the key hints) at row {y}"
        );
    }
    // One blank column between the bar and the content.
    assert_eq!(
        buf.cell((1, 0)).map(|c| c.symbol()),
        Some(" "),
        "gap column before the request line"
    );
}

// ─── Help overlay ──────────────────────────────────────────────────────────

/// The App places the help overlay at centered 60%×70%; mirroring that
/// geometry here, the borderless overlay must land at the centered
/// coordinates and paint nothing outside them.
#[test]
fn help_overlay_is_centered() {
    let rows = draw(100, 30, |f| {
        let area = f.area().centered(
            ratatui::layout::Constraint::Percentage(60),
            ratatui::layout::Constraint::Percentage(70),
        );
        f.render_widget(ratatui::widgets::Clear, area);
        HelpComponent::new().render(f, area, &test_ctx());
    });
    let title_y = rows
        .iter()
        .position(|r| r.contains("Help"))
        .expect("title renders");
    let title_row = &rows[title_y];
    // restyle-1: the rounded frame header `╭─ Help ─╮` starts at
    // the overlay's left edge (x=20): ╭(20) ─(21) sp(22) H(23).
    let text_x = title_row
        .chars()
        .position(|c| c == 'H')
        .expect("title text on the title row");
    assert_eq!(text_x, 23, "frame header at x=20, text at 23");
    // Nothing painted left of the overlay.
    assert!(title_row[..20].trim().is_empty());
    // panel-meta-1: the close-key meta rides the header's right slot.
    assert!(
        title_row.contains("Esc 关闭"),
        "close-key meta on the header: {title_row:?}"
    );
    // 70% of 30 = 21 rows tall, centered → title lands around row 4.
    assert!(
        (3..=6).contains(&title_y),
        "vertically centered, title at row {title_y}"
    );
    // The footer rides the last CONTENT row — one above the frame's
    // bottom border (21-row overlay: title at +0, bottom at +20).
    let footer_y = title_y + 21 - 2;
    assert!(
        rows[footer_y].contains("Esc / ? 关闭"),
        "footer on the last overlay row: {:?}",
        rows[footer_y]
    );
    assert!(
        rows[footer_y].trim_end().ends_with("闭 │"),
        "footer right-aligned inside the frame: {:?}",
        rows[footer_y]
    );
}

/// The full keymap (global / input / transcript-scroll / approval keys
/// + the slash subset) must be present at a regular 100×30 terminal.
#[test]
fn help_lists_the_full_keymap() {
    let rows = draw(100, 30, |f| {
        HelpComponent::new().render(f, f.area(), &test_ctx());
    });
    let screen = rows.join("\n");
    for needle in [
        "全局",
        "Tab",
        "Ctrl+T",
        "Ctrl+L",
        "Ctrl+C",
        "Esc",
        "输入框",
        "Enter",
        "Ctrl+J",
        "Ctrl+W",
        "转录区滚动",
        "PgUp",
        "审批激活时",
        "y / n / a",
        "全部允许",
        "slash 命令",
        "/model",
        // copy-1: the /copy argument forms line.
        "/copy [all|tool]",
    ] {
        assert!(screen.contains(needle), "help must cover {needle}");
    }
}

/// On short terminals the content clips but the footer (bottom border
/// title) stays visible — the close affordance never disappears.
#[test]
fn help_footer_survives_content_clipping() {
    let rows = draw(48, 10, |f| {
        HelpComponent::new().render(f, f.area(), &test_ctx());
    });
    assert!(rows.iter().any(|r| r.contains("Help")), "title visible");
    // panel-meta-1: the close-key meta survives clipping too (the
    // header closes with ` Esc 关闭 ─╮` before the footer's own hint).
    assert!(
        rows.iter()
            .any(|r| r.contains("Esc 关闭") && r.contains("Help")),
        "header meta on the title row: {rows:?}"
    );
    assert!(
        rows[9].starts_with('╰'),
        "bottom border on the last row: {}",
        rows[9]
    );
    assert!(
        rows[8].contains("Esc / ? 关闭"),
        "footer above the bottom border: {:?}",
        rows[8]
    );
}
