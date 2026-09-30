//! select-1 integration — dispatch-level drag-to-select.
//!
//! Drives the REAL App's dispatcher with the mouse action sequence
//! (`MouseDown`/`MouseDrag`/`MouseUp`, what `map_event` produces from
//! crossterm's left-button events) against a streaming transcript:
//!
//! * a completed drag extracts the covered rows and AUTO-COPIES them
//!   through copy-1's chain (notice format, `last-copy.md` content,
//!   OSC 52 payload) while the highlight overlay paints
//!   `theme.selection_bg` over exactly the covered cells;
//! * a press-release that never drags still fires the legacy
//!   positional click (the fix-17 hint-jump regression);
//! * Esc and an overlay opening clear the selection;
//! * a modal-active gesture is swallowed whole (no copy, no click).
//!
//! Content streams in through engine events (the fix-18 test
//! pattern — no engine task needed); drawing goes through the
//! transcript component exactly like a frame would (the fix-19
//! pattern), capturing the FRONT buffer for style assertions.

use std::sync::{Arc, Mutex};

use openslate_core::config::parse_openslate_toml;
use openslate_tui::action::Action;
use openslate_tui::app::{App, ClientBootstrap};
use openslate_tui::client::MemLink;
use openslate_tui::components::{Component, ConfigSummary, Focus, RunInfo, RunState};
use openslate_tui::event::TuiEvent;

// ─── Harness ───────────────────────────────────────────────────────────────

/// The client-fixture config (web-1: pure parse; the copy-file leg
/// still lands in a tempdir — hermetic, no test writes the real
/// `~/.local/share/openslate/last-copy.md`).
fn fixture_toml() -> &'static str {
    r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "TUI_TEST_KEY"

[models.main]
provider = "mock"
model = "mock-model"

[models.fast]
provider = "mock"
model = "mock-model"

[limits]
max_steps = 10
max_depth = 4
max_tool_calls = 20
max_context_bytes = 100_000
max_output_bytes = 10_000
"#
}

async fn select_test_app() -> (App, Arc<Mutex<Vec<String>>>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let config = parse_openslate_toml(fixture_toml()).expect("fixture parses");
    let (_link, events) = MemLink::pair();
    let mut app = App::new(ClientBootstrap {
        link: _link,
        events,
        config,
        root_agent_id: "root".into(),
    });

    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    app.set_clipboard_sink(Box::new(move |payload: &str| {
        sink_seen
            .lock()
            .expect("sink lock")
            .push(payload.to_owned());
        true
    }));
    app.set_copy_file_dir(tmp.path().join("copy-out"));
    (app, seen, tmp)
}

fn copy_file(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().join("copy-out").join("last-copy.md")
}

/// The dark board's selection background (theme contract: #2B6473).
const SEL_BG: ratatui::style::Color = ratatui::style::Color::Rgb(0x2B, 0x64, 0x73);

/// Draw the transcript component into `w x h` (exactly like a frame
/// would) and return the FRONT buffer — style assertions need the
/// post-render state (the backend buffer only receives the diff).
fn draw_transcript(app: &App, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let render_ctx = openslate_tui::components::AppCtx {
        theme: openslate_tui::theme::Theme::new(),
        focus: Focus::Transcript,
        run: RunInfo {
            state: RunState::Thinking,
            spinner_frame: 0,
            model_label: "mock-model".into(),
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
        size: (w, h),
        notice: None,
    };
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("test terminal");
    let mut front = None;
    terminal
        .draw(|f| {
            app.transcript().render(f, f.area(), &render_ctx);
            front = Some(f.buffer_mut().clone());
        })
        .expect("draw frame");
    front.expect("front buffer captured")
}

/// The screen row holding `needle`'s first cell (its x is the char's
/// column) — locates content rows robustly against the optional
/// time-gated live-stats row.
fn find_cell(buf: &ratatui::buffer::Buffer, needle: char) -> (u16, u16) {
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if buf
                .cell((x, y))
                .is_some_and(|c| c.symbol().starts_with(needle))
            {
                return (x, y);
            }
        }
    }
    panic!("{needle:?} not rendered");
}

// ─── Tests ─────────────────────────────────────────────────────────────────

/// Down → drag → Up over two streaming rows: the covered text lands
/// in the copy FILE verbatim, the notice keeps copy-1's format, the
/// OSC 52 leg fires, and the highlight paints exactly the covered
/// cells (first row from the anchor column, last row to the cursor).
#[tokio::test]
async fn drag_select_auto_copies_and_highlights() {
    let (mut app, seen, tmp) = select_test_app().await;
    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("alpha beta".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("\ngamma delta".into())))
        .await;

    // Probe frame: locate the two content rows (the optional
    // time-gated stats row must not be assumed).
    let probe = draw_transcript(&app, 40, 8);
    let (_, y0) = find_cell(&probe, 'a'); // `● alpha beta`
    let (_, y1) = find_cell(&probe, 'g'); // `  gamma delta`
    assert!(y1 > y0, "two distinct rows: {y0} vs {y1}");

    // The gesture: press on `a`'s row at its text column, release on
    // the second row inside `gamma`.
    app.dispatch(Action::MouseDown(2, y0)).await;
    app.dispatch(Action::MouseDrag(6, y1)).await;
    app.dispatch(Action::MouseUp(6, y1)).await;

    // Notice: copy-1's format names the char count and the ABSOLUTE
    // copy-file path, plus the OSC 52 leg that fired.
    let notice = app.status_notice().expect("copy notice").to_owned();
    assert!(notice.starts_with("copied "), "notice: {notice}");
    let expected_path = copy_file(&tmp);
    assert!(
        notice.contains(&expected_path.display().to_string()),
        "notice names the copy file: {notice}"
    );
    assert!(notice.contains("+osc52"), "notice: {notice}");

    // The copy FILE holds exactly the covered rows: row 0 from the
    // anchor column (`alpha beta`), row 1 to the cursor column
    // (`  gamma` — its 2-column indent is real rendered content).
    let file_text = std::fs::read_to_string(&expected_path).expect("copy file written");
    assert_eq!(file_text, "alpha beta\n  gamma");

    // The OSC 52 payload fired (base64 of the same text).
    {
        let payloads = seen.lock().expect("sink lock");
        assert_eq!(payloads.len(), 1, "one OSC 52 write");
        assert!(!payloads[0].is_empty());
    }

    // Highlight (persisted `Selected`): first row from the anchor
    // column to the right edge, last row from the left edge to the
    // cursor column; the columns before/after stay untouched.
    let buf = draw_transcript(&app, 40, 8);
    assert_eq!(buf.cell((2, y0)).unwrap().bg, SEL_BG, "anchor cell");
    assert_eq!(buf.cell((39, y0)).unwrap().bg, SEL_BG, "row tail");
    assert_eq!(buf.cell((0, y1)).unwrap().bg, SEL_BG, "last row head");
    assert_eq!(buf.cell((6, y1)).unwrap().bg, SEL_BG, "cursor cell");
    assert_eq!(
        buf.cell((1, y0)).unwrap().bg,
        ratatui::style::Color::Reset,
        "before the anchor"
    );
    assert_eq!(
        buf.cell((7, y1)).unwrap().bg,
        ratatui::style::Color::Reset,
        "after the cursor"
    );
}

/// A press-release that never drags STILL fires the legacy click:
/// the `+N ↓ 回到底部` hint hit through the mouse gesture re-follows
/// (fix-17 regression through the select-1 pipeline), with no copy.
#[tokio::test]
async fn press_release_without_drag_still_clicks_the_hint() {
    let (mut app, seen, tmp) = select_test_app().await;
    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    // Enough rows to overflow an 8-row viewport.
    app.dispatch(Action::Engine(TuiEvent::Delta(
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten".into(),
    )))
    .await;

    // Pin via the wheel, then a frame (records the hint rect).
    app.dispatch(Action::WheelScrollUp(40, 2)).await;
    assert!(app.transcript_pinned(), "wheel pins the view");
    let probe = draw_transcript(&app, 40, 8);
    let last_y = 8 - 1;
    let hint_row: String = (0..40)
        .map(|x| probe.cell((x, last_y)).map(|c| c.symbol()).unwrap_or(" "))
        .collect();
    // (Wide glyphs pad their trailing cells with spaces in the
    // per-cell concatenation — compare space-stripped.)
    let compact: String = hint_row.split_whitespace().collect();
    assert!(
        compact.contains("+2↓") && compact.contains("回到底部"),
        "hint row rendered at the bottom (over the content): {hint_row:?}"
    );

    // Press + release on the hint (no drag): the legacy click fires.
    app.dispatch(Action::MouseDown(33, last_y)).await;
    app.dispatch(Action::MouseUp(33, last_y)).await;
    assert!(
        !app.transcript_pinned(),
        "the hint click through the gesture restores following"
    );
    // No copy side effects — a click is a click.
    assert_eq!(app.status_notice(), None, "no copy notice");
    assert!(!copy_file(&tmp).exists(), "no copy file");
    assert!(seen.lock().expect("sink lock").is_empty(), "no OSC 52");
}

/// Esc (no overlay open) clears the selection: the next frame paints
/// no highlight, and the pin behavior is unchanged.
#[tokio::test]
async fn esc_clears_the_selection() {
    let (mut app, _seen, _tmp) = select_test_app().await;
    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("alpha beta".into())))
        .await;

    let probe = draw_transcript(&app, 40, 8);
    let (_, y0) = find_cell(&probe, 'a');
    app.dispatch(Action::MouseDown(2, y0)).await;
    app.dispatch(Action::MouseDrag(6, y0)).await;
    app.dispatch(Action::MouseUp(6, y0)).await;
    let selected = draw_transcript(&app, 40, 8);
    assert_eq!(selected.cell((2, y0)).unwrap().bg, SEL_BG, "highlight on");

    // Esc: the App clears the selection regardless of which
    // component holds focus (the input does, by default).
    app.dispatch(Action::DismissOverlay).await;
    let cleared = draw_transcript(&app, 40, 8);
    assert_eq!(
        cleared.cell((2, y0)).unwrap().bg,
        ratatui::style::Color::Reset,
        "highlight gone after Esc"
    );
}

/// An overlay opening clears the selection (help here — the same
/// rule covers exit-confirm, the agents panel and approvals), and a
/// gesture begun under a modal is swallowed whole: no click, no
/// copy, no selection.
#[tokio::test]
async fn overlay_open_clears_and_modal_swallows_the_gesture() {
    let (mut app, seen, tmp) = select_test_app().await;
    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("alpha beta".into())))
        .await;

    // Select something first.
    let probe = draw_transcript(&app, 40, 8);
    let (_, y0) = find_cell(&probe, 'a');
    app.dispatch(Action::MouseDown(2, y0)).await;
    app.dispatch(Action::MouseDrag(6, y0)).await;
    app.dispatch(Action::MouseUp(6, y0)).await;

    // Opening the help overlay clears it.
    app.dispatch(Action::ToggleHelp).await;
    let cleared = draw_transcript(&app, 40, 8);
    assert_eq!(
        cleared.cell((2, y0)).unwrap().bg,
        ratatui::style::Color::Reset,
        "overlay opening cleared the selection"
    );

    // While the modal is up, the whole gesture is swallowed: Down,
    // Drag and Up never reach the transcript (no click, no SECOND
    // copy — the first selection's copy legs must be the only ones).
    let first_copy = std::fs::read_to_string(copy_file(&tmp)).expect("first copy file");
    app.dispatch(Action::MouseDown(2, y0)).await;
    app.dispatch(Action::MouseDrag(6, y0)).await;
    app.dispatch(Action::MouseUp(6, y0)).await;
    assert_eq!(
        std::fs::read_to_string(copy_file(&tmp)).expect("copy file"),
        first_copy,
        "no second copy under a modal"
    );
    assert_eq!(
        seen.lock().expect("sink lock").len(),
        1,
        "no second OSC 52 under a modal"
    );
    let still = draw_transcript(&app, 40, 8);
    assert_eq!(
        still.cell((2, y0)).unwrap().bg,
        ratatui::style::Color::Reset,
        "no selection appeared under a modal"
    );
}

/// A drag that becomes a selection suppresses the click for the
/// WHOLE gesture — pressing on the hint and dragging away does not
/// jump to the bottom (the pin survives), and the release copies.
#[tokio::test]
async fn drag_from_the_hint_row_suppresses_the_click() {
    let (mut app, _seen, _tmp) = select_test_app().await;
    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::Delta(
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten".into(),
    )))
    .await;
    app.dispatch(Action::WheelScrollUp(40, 2)).await;
    draw_transcript(&app, 40, 8); // records the hint rect + geometry

    // Press on the hint, drag into the content, release: NO jump.
    app.dispatch(Action::MouseDown(33, 7)).await;
    app.dispatch(Action::MouseDrag(4, 2)).await;
    app.dispatch(Action::MouseUp(4, 2)).await;
    assert!(
        app.transcript_pinned(),
        "a dragged press never fires the hint click"
    );
    assert!(
        app.status_notice()
            .is_some_and(|n| n.starts_with("copied ")),
        "the release copied instead: {:?}",
        app.status_notice()
    );
}
