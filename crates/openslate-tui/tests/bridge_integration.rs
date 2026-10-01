//! Bridge integration — the client's `ServerMsg → TuiEvent → App`
//! pipeline (web-1).
//!
//! No network, no engine: a [`MemLink`] stands in for the server. Tests
//! either dispatch `TuiEvent`s directly (display-mirror semantics,
//! unchanged since the local-engine era) or `emit` real `ServerMsg`s
//! through [`crate::openslate_tui::client::to_tui`] — the exact
//! conversion the production WS task uses — proving:
//!
//! * submit/cancel/approval/CRUD turns into `ClientMsg`s on the link;
//! * broadcast events drive the same transcript/merge/state paths as
//!   before (the fixed merge point survives, manager-less now);
//! * snapshots rebuild the transcript wholesale (hello/reconnect);
//! * the copy chain stays hermetic (capturing sink + tempdir file leg).

use std::sync::Arc;

use openslate_core::config::parse_openslate_toml;
use openslate_core::types::{Message, MessageRole, RunId, ToolCall, ToolCallId, Usage};

use openslate_protocol::{ClientMsg, ServerMsg};
use openslate_tui::action::Action;
use openslate_tui::app::{coalesce_deltas, App, ClientBootstrap, DispatchOutcome};
use openslate_tui::client::{LinkPhase, MemLink};
use openslate_tui::components::transcript::{ToolEntryStatus, TranscriptEntry};
use openslate_tui::components::RunState;
use openslate_tui::event::{ApprovalSummary, TuiEvent, TurnSummary};

// ─── Fixture helpers ───────────────────────────────────────────────────────

fn test_config() -> openslate_core::config::OpenSlateConfig {
    let toml = r#"
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
max_context_bytes = 100_000
max_output_bytes = 10_000
"#;
    parse_openslate_toml(toml).expect("test config should parse")
}

/// Theme slot accessor for color assertions (theme-1).
fn theme() -> openslate_tui::theme::Theme {
    openslate_tui::theme::Theme::new()
}

/// A client App + its recording link (seeded with a hello snapshot —
/// model alias `main`, empty transcript; mirrors production where the
/// first snapshot precedes the first draw).
async fn test_app_linked() -> (App, Arc<MemLink>) {
    let config = test_config();
    let (link, events) = MemLink::pair();
    let mut app = App::new(ClientBootstrap {
        link: link.clone(),
        events,
        config,
        root_agent_id: "root".into(),
    });
    link.emit(ServerMsg::Snapshot {
        session: Box::new(snapshot_dto("main")),
    });
    app.drain_engine_events().await;
    (app, link)
}

/// [`test_app_linked`] without the link handle.
async fn test_app() -> App {
    test_app_linked().await.0
}

/// A minimal wire snapshot for test seeding (custom session id/label;
/// config view from the shared helper).
fn snapshot_dto(model_alias: &str) -> openslate_protocol::SnapshotDto {
    let mut snap = openslate_tui::client::snapshot_from_config(&test_config(), model_alias);
    snap.session_id = "bridge-test-session".into();
    snap.session_label = "bridge test".into();
    snap
}

fn user_message(text: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: text.to_owned(),
        tool_call_id: None,
        name: None,
        tool_calls: None,
        reasoning_content: None,
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

/// The App side: broadcast events drive the transcript streaming area,
/// the tool entry lifecycle, and the TurnDone merge + status fall-back.
#[tokio::test]
async fn app_consumes_turn_events_and_merges_transcript() {
    let mut app = test_app().await;

    let summary = TurnSummary {
        run_id: RunId("app-run".into()),
        status: openslate_core::types::RunStatus::Completed,
        messages: vec![
            user_message("go"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                name: None,
                tool_calls: Some(vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "hi"}),
                }]),
                reasoning_content: None,
            },
            Message {
                role: MessageRole::Tool,
                content: "hi".into(),
                tool_call_id: Some(ToolCallId("tc-1".into())),
                name: Some("echo".into()),
                tool_calls: None,
                reasoning_content: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "All done!".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            },
        ],
        total_steps: 2,
        total_input_tokens: 100,
        total_output_tokens: 20,
        total_cost_usd: 0.002,
        model: "mock-model".into(),
    };

    // ── streaming phase ──
    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    assert_eq!(*app.run_state(), RunState::Thinking);

    app.dispatch(Action::Engine(TuiEvent::Delta("Hel".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("lo".into())))
        .await;
    assert_eq!(
        app.transcript_entries().len(),
        0,
        "deltas live in the streaming area"
    );

    // ── tool phase ──
    app.dispatch(Action::Engine(TuiEvent::ToolStart {
        name: "echo".into(),
        args: r#"{"text":"hi"}"#.into(),
    }))
    .await;
    assert!(!app.transcript_entries().is_empty());
    assert_eq!(
        *app.run_state(),
        RunState::ToolRunning {
            name: "echo".into()
        }
    );
    app.dispatch(Action::Engine(TuiEvent::ToolEnd {
        name: "echo".into(),
        bytes: 2,
        truncated: false,
    }))
    .await;
    assert!(matches!(
        app.transcript_entries().last(),
        Some(TranscriptEntry::ToolCall {
            status: ToolEntryStatus::Done { bytes, .. },
            ..
        }) if *bytes == 2
    ));

    // ── TurnDone: merge + status fall-back ──
    app.dispatch(Action::Engine(TuiEvent::TurnDone(Ok(summary))))
        .await;
    assert_eq!(*app.run_state(), RunState::Idle);
    assert!(!app.is_running());
    // The merge path keeps the event-ordered live view and only adds:
    // the live-flushed "Hello" (streamed before the tool call) stays,
    // the tool result folds into the row, and the messages' "All
    // done!" — never streamed here — appends. (No User entry exists:
    // this test never dispatched StartTurn, and the merge has no
    // user-message guard — the live path owns that push.)
    assert_eq!(app.transcript_entries().len(), 3);
    assert!(matches!(
        app.transcript_entries().first(),
        Some(TranscriptEntry::Assistant(text)) if text == "Hello"
    ));
    assert!(matches!(
        &app.transcript_entries()[1],
        TranscriptEntry::ToolCall { status: ToolEntryStatus::Done { bytes, .. }, .. }
            if *bytes == 2
    ));
    assert!(matches!(
        app.transcript_entries().last(),
        Some(TranscriptEntry::Assistant(text)) if text == "All done!"
    ));
    // History = full message list (the server-side authority mirror).
    assert_eq!(app.history().len(), 4);
}

/// Per-request telemetry through the App's dispatch: RequestStart
/// starts the clock (and resets the TTFT mark), FirstToken records
/// the TTFT numerator (and freezes the live stats row's ttft,
/// fix-19), Usage stores the counts, RequestEnd flushes the streamed
/// blocks — the reasoning estimate at the reasoning block's tail —
/// and HOLDS the exact usage line (fix-19: tools of the step execute
/// after RequestEnd, so the line waits for a boundary; the next
/// RequestStart proves the step had no tools and lands it at the
/// answer's tail). The `TurnDone(Ok)` merge then KEEPS those lines
/// (event-ordered entries) — only the recovery rebuilds drop them.
#[tokio::test]
async fn request_usage_meta_lines_flow_through_dispatch() {
    let mut app = test_app().await;

    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    // FirstToken before the content: the request's ttft is measurable.
    app.dispatch(Action::Engine(TuiEvent::FirstToken)).await;
    app.dispatch(Action::Engine(TuiEvent::Reasoning(
        "thinking about it".into(),
    )))
    .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("the answer".into())))
        .await;
    // Usage may arrive before or after deltas (provider-dependent);
    // RequestEnd assembles the meta regardless.
    app.dispatch(Action::Engine(TuiEvent::Usage(Usage {
        input_tokens: 50,
        output_tokens: 10,
        cached_input_tokens: Some(3),
        reasoning_tokens: None,
    })))
    .await;
    app.dispatch(Action::Engine(TuiEvent::RequestEnd)).await;

    let entries = app.transcript_entries();
    let kinds: Vec<String> = entries
        .iter()
        .map(|e| match e {
            TranscriptEntry::Reasoning(_) => "reasoning".into(),
            TranscriptEntry::Meta(t) => {
                if t.starts_with('~') {
                    "meta_est".into()
                } else {
                    format!("meta_usage:{t}")
                }
            }
            TranscriptEntry::Assistant(_) => "assistant".into(),
            TranscriptEntry::StepBreak => "sep".into(),
            other => format!("other:{other:?}"),
        })
        .collect();
    assert!(
        kinds.iter().any(|k| k == "reasoning"),
        "reasoning flushed: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "meta_est"),
        "reasoning estimate line: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "assistant"),
        "answer flushed: {kinds:?}"
    );
    // fix-19: the exact usage line is HELD at RequestEnd — no tools
    // have started yet, so its position is not yet known.
    assert!(
        !kinds.iter().any(|k| k.starts_with("meta_usage:")),
        "usage line held, not committed: {kinds:?}"
    );

    // The NEXT RequestStart proves the step had no tools — the hold
    // lands at the answer's tail.
    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 2,
        model: "mock-model".into(),
    }))
    .await;
    let entries = app.transcript_entries();
    let kinds: Vec<String> = entries
        .iter()
        .map(|e| match e {
            TranscriptEntry::Reasoning(_) => "reasoning".into(),
            TranscriptEntry::Meta(t) => {
                if t.starts_with('~') {
                    "meta_est".into()
                } else {
                    format!("meta_usage:{t}")
                }
            }
            TranscriptEntry::Assistant(_) => "assistant".into(),
            TranscriptEntry::StepBreak => "sep".into(),
            other => format!("other:{other:?}"),
        })
        .collect();
    let usage_line = kinds
        .iter()
        .find(|k| k.starts_with("meta_usage:"))
        .expect("exact usage meta line flushed at the next RequestStart");
    assert!(
        usage_line.contains("↑50 ↓10 ⎓3"),
        "cached segment present: {usage_line}"
    );
    assert!(
        usage_line.contains("ttft "),
        "ttft segment present (FirstToken was dispatched): {usage_line}"
    );
    // Event order is visual order: estimate above answer above usage.
    let idx = |name: &str| kinds.iter().position(|k| k == name).unwrap();
    assert!(idx("reasoning") < idx("meta_est"));
    assert!(idx("meta_est") < idx("assistant"));
    assert!(
        kinds
            .iter()
            .position(|k| k.starts_with("meta_usage:"))
            .unwrap()
            > idx("assistant")
    );

    // TurnDone(Ok) MERGE: the live meta lines survive as part of the
    // event-ordered record; the turn marker's aggregates join them.
    let summary = openslate_tui::event::TurnSummary {
        run_id: RunId("app-run".into()),
        status: openslate_core::types::RunStatus::Completed,
        messages: vec![
            user_message("go"),
            Message {
                role: MessageRole::Assistant,
                content: "the answer".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            },
        ],
        total_steps: 1,
        total_input_tokens: 50,
        total_output_tokens: 10,
        total_cost_usd: 0.001,
        model: "mock-model".into(),
    };
    app.dispatch(Action::Engine(TuiEvent::TurnDone(Ok(summary))))
        .await;
    assert!(
        app.transcript_entries()
            .iter()
            .any(|e| matches!(e, TranscriptEntry::Meta(_))),
        "meta lines survive the merge path"
    );
    // History stays the engine-side message authority.
    assert_eq!(app.history().len(), 2);
}

/// fix-19 ① (App wiring): `FirstToken` freezes the precise ttft into
/// the transcript; while the ANSWER streams, the live stats row
/// (`ttft S.Ss · ~Rtok/s`) renders at the answer head (muted, no
/// rate segment under the floor — a µs-old buffer shows ttft only).
#[tokio::test]
async fn first_token_feeds_the_live_stats_row_during_answer_streaming() {
    let mut app = test_app().await;

    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    // Reasoning streams first: no live row yet (answer area only).
    app.dispatch(Action::Engine(TuiEvent::FirstToken)).await;
    app.dispatch(Action::Engine(TuiEvent::Reasoning("thinking".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("partial".into())))
        .await;

    let render_ctx = openslate_tui::components::AppCtx {
        theme: theme(),
        focus: openslate_tui::components::Focus::Transcript,
        run: openslate_tui::components::RunInfo {
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
        config: openslate_tui::components::ConfigSummary {
            model_alias: "main".into(),
            model_id: "mock-model".into(),
            provider_name: "mock".into(),
            max_depth: 4,
            max_tool_calls: 20,
            run_id: None,
            model_aliases: Vec::new(),
        },
        size: (30, 5),
        notice: None,
    };
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 5)).expect("test terminal");
    terminal
        .draw(|f| {
            use openslate_tui::components::Component;
            app.transcript().render(f, f.area(), &render_ctx)
        })
        .expect("draw frame");
    let screen: Vec<String> = (0..terminal.backend().buffer().area.height)
        .map(|y| {
            (0..terminal.backend().buffer().area.width)
                .map(|x| {
                    terminal
                        .backend()
                        .buffer()
                        .cell((x, y))
                        .map(|c| c.symbol().to_string())
                        .unwrap_or_default()
                })
                .collect()
        })
        .collect();
    let stats_row = screen
        .iter()
        .find(|r| r.contains("ttft "))
        .expect("live stats row at the answer head");
    assert!(
        stats_row.contains("ttft 0."),
        "precise ttft frozen at FirstToken: {stats_row}"
    );
    assert!(
        !stats_row.contains("tok/s"),
        "no rate segment under the floor: {stats_row}"
    );
    // The stats row is dim (muted) like the meta rows.
    let y = screen.iter().position(|r| r.contains("ttft ")).unwrap() as u16;
    let x = stats_row.find('t').unwrap() as u16;
    assert_eq!(
        terminal.backend().buffer().cell((x, y)).unwrap().style().fg,
        theme().muted.fg
    );
}

/// fix-19 ② (App wiring): a TOOL step's held usage line lands below
/// the tool row — `RequestEnd` holds it, `ToolStart` consumes it.
#[tokio::test]
async fn tool_step_usage_meta_lands_below_the_tool_row() {
    let mut app = test_app().await;

    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::FirstToken)).await;
    app.dispatch(Action::Engine(TuiEvent::Delta("checking".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Usage(Usage {
        input_tokens: 50,
        output_tokens: 10,
        cached_input_tokens: None,
        reasoning_tokens: None,
    })))
    .await;
    app.dispatch(Action::Engine(TuiEvent::RequestEnd)).await;
    // Held: no usage meta among the entries yet.
    assert!(app
        .transcript_entries()
        .iter()
        .all(|e| { !matches!(e, TranscriptEntry::Meta(m) if m.starts_with("↑50")) }));
    app.dispatch(Action::Engine(TuiEvent::ToolStart {
        name: "echo".into(),
        args: "{}".into(),
    }))
    .await;

    let entries = app.transcript_entries();
    let tool = entries
        .iter()
        .position(|e| matches!(e, TranscriptEntry::ToolCall { .. }))
        .expect("tool entry");
    let meta = entries
        .iter()
        .position(|e| matches!(e, TranscriptEntry::Meta(m) if m.starts_with("↑50")))
        .expect("held usage meta landed below the tool row");
    assert!(meta > tool, "meta AFTER the tool row: {tool} vs {meta}");
}

// ─── ora-2 dispatch-level tests (frozen-revision window) ───────────────────

/// Err path (web-1 injection): the server's `TurnError` broadcast → the
/// error surfaces as `RunState::Error`, and input submitted while the
/// turn ran is RESTORED to the editor instead of auto-retried.
#[tokio::test]
async fn turn_error_restores_pending_input_and_surfaces_error() {
    let (mut app, link) = test_app_linked().await;

    app.dispatch(Action::StartTurn("hello".into())).await;
    assert!(app.is_running(), "optimistic in-flight after the submit");
    assert_eq!(
        link.take_sent(),
        vec![ClientMsg::Submit {
            text: "hello".into()
        }],
        "the submit left for the server"
    );

    // Submit while the turn is running → queued as pending input (never
    // reaches the editor yet).
    app.dispatch(Action::StartTurn("queued follow-up".into()))
        .await;
    assert_eq!(
        app.input_text(),
        "",
        "queued text is not typed into the editor"
    );

    // The server's failure lands as TurnDone(Err) through the link.
    link.emit(ServerMsg::TurnError {
        message: "boom".into(),
    });
    app.drain_engine_events().await;
    assert!(!app.is_running());

    // The turn error surfaces (session stays alive — not a Quit).
    assert!(matches!(app.run_state(), RunState::Error(_)));

    // Pending input restored to the editor (no auto-retry storms).
    assert_eq!(app.input_text(), "queued follow-up");
}

/// Approval modal preemption (review item 4b): while ApprovalActive, y/n/a
/// answer the queue and EVERYTHING else — including input-box characters
/// and submit — is swallowed; typing resumes once the resolutions land
/// (web-1: the banner drains on `approval_resolved` broadcasts, not on
/// the local keypress — first answer wins server-side).
#[tokio::test]
async fn approval_modal_preempts_input_keys() {
    let (mut app, link) = test_app_linked().await;

    let summary = ApprovalSummary {
        tool_name: "shell".into(),
        arguments: r#"{"cmd":"ls"}"#.into(),
        agent_id: "root".into(),
        risk_level: "high".into(),
    };
    for id in 1..=3u64 {
        app.dispatch(Action::Engine(TuiEvent::ApprovalRequested {
            id,
            request: summary.clone(),
        }))
        .await;
    }
    assert_eq!(*app.run_state(), RunState::ApprovalPending);

    // Ordinary characters and submit are swallowed while approval is up.
    app.dispatch(Action::InputChar('h')).await;
    app.dispatch(Action::InputChar('i')).await;
    app.dispatch(Action::SubmitInput).await;
    assert_eq!(app.input_text(), "");

    // `y` answers the front request — the answer LEAVES, but the queue
    // keeps preemption active until the broadcast resolves it.
    app.dispatch(Action::InputChar('y')).await;
    app.dispatch(Action::InputChar('q')).await; // still pending → swallowed
    assert_eq!(app.input_text(), "");
    assert_eq!(
        link.take_sent(),
        vec![ClientMsg::ApprovalAnswer {
            id: 1,
            choice: openslate_protocol::ApprovalAnswerChoice::Approve,
        }],
        "y answered the front request"
    );
    // `n` and `a` answer the remaining two.
    app.dispatch(Action::InputChar('n')).await;
    app.dispatch(Action::InputChar('a')).await;

    // Server broadcasts resolve all three: preemption over → characters
    // type into the editor again (also proves the layer derives from the
    // queue, not a stale modal flag).
    for (id, choice) in [(1u64, "approve"), (2, "deny"), (3, "approve_all")] {
        link.emit(ServerMsg::ApprovalResolved {
            id,
            choice: choice.into(),
        });
        app.drain_engine_events().await;
    }
    app.dispatch(Action::InputChar('x')).await;
    assert_eq!(app.input_text(), "x");
}

/// Delta coalescing (review item 4c): adjacent `Engine(Delta)` actions in
/// one drained batch merge; anything between them breaks the run. Exposed
/// via the pub `coalesce_deltas` (dispatch itself does not coalesce —
/// coalescing happens between the drain and the dispatch loop).
#[test]
fn coalesce_deltas_merges_only_adjacent_deltas() {
    let mut batch = vec![
        Action::Engine(TuiEvent::Delta("Hel".into())),
        Action::Engine(TuiEvent::Delta("lo".into())),
        Action::Redraw, // separator: no merge across it
        Action::Engine(TuiEvent::Delta("a".into())),
        Action::Tick, // separator
        Action::Engine(TuiEvent::Delta("b".into())),
        Action::Engine(TuiEvent::Delta("c".into())),
    ];
    coalesce_deltas(&mut batch);

    let deltas: Vec<String> = batch
        .iter()
        .filter_map(|action| match action {
            Action::Engine(TuiEvent::Delta(text)) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["Hello".to_owned(), "a".into(), "bc".into()]);
    // Non-delta actions survive in order:
    // [Delta("Hello"), Redraw, Delta("a"), Tick, Delta("bc")].
    assert_eq!(batch.len(), 5);
    assert!(matches!(batch[1], Action::Redraw));
    assert!(matches!(batch[3], Action::Tick));
}

/// CancelTurn under ApprovalActive (ora-3 review item 2): the Deny AND
/// the turn Cancel both leave for the server — the R3 hazard was "Deny
/// alone lets the engine keep running". The banner itself waits for the
/// `approval_resolved` broadcast (web-1).
#[tokio::test]
async fn cancel_turn_while_approval_pending_denies_and_cancels() {
    let (mut app, link) = test_app_linked().await;

    let summary = ApprovalSummary {
        tool_name: "shell".into(),
        arguments: r#"{"cmd":"ls"}"#.into(),
        agent_id: "root".into(),
        risk_level: "high".into(),
    };
    app.dispatch(Action::Engine(TuiEvent::ApprovalRequested {
        id: 1,
        request: summary,
    }))
    .await;
    assert_eq!(*app.run_state(), RunState::ApprovalPending);
    assert!(!app.is_cancelled());

    app.dispatch(Action::CancelTurn).await;

    // Both halves left: the Deny answers the blocked request …
    assert_eq!(
        link.take_sent(),
        vec![
            ClientMsg::ApprovalAnswer {
                id: 1,
                choice: openslate_protocol::ApprovalAnswerChoice::Deny,
            },
            ClientMsg::Cancel,
        ],
        "deny + cancel both left for the server"
    );
    // … and the turn cancel is marked locally. Preemption lifts with
    // the broadcast:
    assert!(app.is_cancelled(), "the running turn is cancelled");
    link.emit(ServerMsg::ApprovalResolved {
        id: 1,
        choice: "deny".into(),
    });
    app.drain_engine_events().await;
    assert_ne!(*app.run_state(), RunState::ApprovalPending);
}

/// Exit-confirmation semantics (user request): a second Ctrl+C inside
/// the modal CONFIRMS the quit; Esc dismisses without quitting.
#[tokio::test]
async fn second_ctrl_c_confirms_exit_and_esc_cancels() {
    let mut app = test_app().await;

    // Idle Ctrl+C escalates to the exit-confirmation modal (no quit yet).
    assert_eq!(
        app.dispatch(Action::CancelTurn).await,
        DispatchOutcome::Continue
    );

    // A second Ctrl+C inside the modal confirms the quit.
    assert_eq!(
        app.dispatch(Action::CancelTurn).await,
        DispatchOutcome::Quit
    );

    // Esc path: reopen the modal, then Esc dismisses without quitting.
    // (Fresh App so the quit above doesn't poison the loop state.)
    let (mut app2, _link2) = test_app_linked().await;
    assert_eq!(
        app2.dispatch(Action::CancelTurn).await,
        DispatchOutcome::Continue
    );
    assert_eq!(
        app2.dispatch(Action::DismissOverlay).await,
        DispatchOutcome::Continue
    );
    // Modal closed → ordinary input flows again (no lingering preemption).
    app2.dispatch(Action::InputChar('x')).await;
    assert_eq!(app2.input_text(), "x");
}

/// Mouse wheel routing: a wheel notch ALWAYS scrolls the chat transcript,
/// never the input-box history — even while the input box holds focus
/// (where keyboard ↑/↓ switch history). Wheel-down back to the bottom
/// re-follows the live tail.
#[tokio::test]
async fn wheel_scroll_targets_transcript_not_input_history() {
    let mut app = test_app().await;

    // Default focus is the input box; type a char so any accidental
    // history navigation would be observable through `input_text()`.
    app.dispatch(Action::InputChar('a')).await;
    assert!(!app.transcript_pinned());

    // A wheel notch up pins the transcript (scrolls the chat) …
    app.dispatch(Action::WheelScrollUp(40, 2)).await;
    assert!(app.transcript_pinned());

    // … and leaves the input editor (and history) untouched.
    assert_eq!(app.input_text(), "a");

    // Wheel back down reaches the bottom and re-follows the live tail.
    app.dispatch(Action::WheelScrollDown(40, 2)).await;
    app.dispatch(Action::WheelScrollDown(40, 2)).await;
    assert!(!app.transcript_pinned());
}

// ─── fix-11: streaming order + mouse-capture toggle ────────────────────────

/// Streaming order (user report 1+3): reasoning that streamed BEFORE a
/// tool call must sit ABOVE the tool line in the live transcript —
/// event order is visual order — and the reasoning that streams after
/// the tool lands below it, above the answer. The `TurnDone(Ok)`
/// merge then KEEPS the live view in that order (reasoning survives;
/// the tool result folds in).
#[tokio::test]
async fn reasoning_and_tool_line_keep_event_order_in_live_view() {
    let mut app = test_app().await;

    app.dispatch(Action::StartTurn("go".into())).await;

    // Step 1: reasoning streams, then a tool fires mid-stream.
    app.dispatch(Action::Engine(TuiEvent::Reasoning("think A".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::ToolStart {
        name: "echo".into(),
        args: "{}".into(),
    }))
    .await;
    // The reasoning was flushed into entries ABOVE the tool call.
    assert!(matches!(
        app.transcript_entries(),
        [TranscriptEntry::User(_), TranscriptEntry::Reasoning(text), TranscriptEntry::ToolCall { name, .. }]
            if text == "think A" && name == "echo"
    ));

    // Step 2: more reasoning + the answer — both stay in the streaming
    // area (below the tool line) until the next boundary/turn end.
    app.dispatch(Action::Engine(TuiEvent::ToolEnd {
        name: "echo".into(),
        bytes: 2,
        truncated: false,
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::Reasoning("think B".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("final".into())))
        .await;
    assert_eq!(
        app.transcript_entries().len(),
        3,
        "no new entries mid-stream"
    );

    // TurnDone(Ok) MERGE: the live view is kept in event order — the
    // reasoning blocks survive (core never persists reasoning; the
    // live entries are the only record), the straggler "think B"/
    // "final" buffers commit, and the tool result folds into the row.
    let summary = openslate_tui::event::TurnSummary {
        run_id: RunId("order-run".into()),
        status: openslate_core::types::RunStatus::Completed,
        messages: vec![
            user_message("go"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                name: None,
                tool_calls: Some(vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "echo".into(),
                    arguments: serde_json::json!({}),
                }]),
                reasoning_content: None,
            },
            Message {
                role: MessageRole::Tool,
                content: "ok".into(),
                tool_call_id: Some(ToolCallId("tc-1".into())),
                name: Some("echo".into()),
                tool_calls: None,
                reasoning_content: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "final".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            },
        ],
        total_steps: 2,
        total_input_tokens: 10,
        total_output_tokens: 5,
        total_cost_usd: 0.001,
        model: "mock-model".into(),
    };
    app.dispatch(Action::Engine(TuiEvent::TurnDone(Ok(summary))))
        .await;
    assert!(matches!(
        app.transcript_entries(),
        [
            TranscriptEntry::User(_),
            TranscriptEntry::Reasoning(a),
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Done { bytes, .. },
                ..
            },
            TranscriptEntry::Reasoning(b),
            TranscriptEntry::Assistant(f),
        ] if a == "think A" && *bytes == 2 && b == "think B" && f == "final"
    ));
}

/// Mouse-capture runtime toggle (user report 2): Ctrl+M flips the App
/// flag with a notice explaining the trade-off; `/mouse` is the slash
/// alias. Default ON (wheel scrolls); OFF = native selection works.
#[tokio::test]
async fn toggle_mouse_capture_flips_state_and_notice() {
    let mut app = test_app().await;

    assert!(app.mouse_capture(), "capture on by default (wheel)");

    // Ctrl+M → off, with the selection/wheel trade-off notice.
    app.dispatch(Action::ToggleMouseCapture).await;
    assert!(!app.mouse_capture());
    let notice = app.status_notice().expect("notice set").to_owned();
    assert!(notice.contains("off"), "notice names the state: {notice}");
    assert!(
        notice.contains("selectable"),
        "notice explains the selection win: {notice}"
    );
    assert!(
        notice.contains("arrow"),
        "notice explains the wheel degradation: {notice}"
    );
    assert!(
        notice.is_ascii(),
        "notices render in the REVERSED status bar (ASCII-only): {notice}"
    );

    // `/mouse` — the slash alias flips back on.
    app.dispatch(Action::StartTurn("/mouse".into())).await;
    assert!(app.mouse_capture());
    let notice = app.status_notice().expect("notice set").to_owned();
    assert!(notice.contains("on"), "notice names the state: {notice}");
    assert!(
        notice.contains("wheel"),
        "notice names the wheel win: {notice}"
    );
    assert!(notice.is_ascii());

    // Ctrl+M round-trips and the toggle works mid-modal too? — No:
    // modal guards swallow it (same as Ctrl+T); assert the non-modal
    // path only, plus idempotent round-trip.
    app.dispatch(Action::ToggleMouseCapture).await;
    assert!(!app.mouse_capture());
    app.dispatch(Action::ToggleMouseCapture).await;
    assert!(app.mouse_capture());
}

// ─── fix-18: streaming markdown renders live ───────────────────────────────

/// Dispatch level: a Delta carrying markdown paints its styles into
/// the rendered buffer IMMEDIATELY — the heading accent exists in the
/// frame while the text still lives in the streaming area (entries
/// stay empty; nothing waited for a flush boundary).
#[tokio::test]
async fn streaming_delta_renders_markdown_styles_before_flush() {
    let mut app = test_app().await;

    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("# Live heading".into())))
        .await;
    assert!(
        app.transcript_entries().is_empty(),
        "the delta still lives in the streaming area"
    );

    // Draw the transcript exactly like a frame would.
    let render_ctx = openslate_tui::components::AppCtx {
        theme: theme(),
        focus: openslate_tui::components::Focus::Transcript,
        run: openslate_tui::components::RunInfo {
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
        config: openslate_tui::components::ConfigSummary {
            model_alias: "main".into(),
            model_id: "mock-model".into(),
            provider_name: "mock".into(),
            max_depth: 4,
            max_tool_calls: 20,
            run_id: None,
            model_aliases: Vec::new(),
        },
        size: (30, 4),
        notice: None,
    };
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 4)).expect("test terminal");
    terminal
        .draw(|f| {
            use openslate_tui::components::Component;
            app.transcript().render(f, f.area(), &render_ctx)
        })
        .expect("draw frame");
    let buf = terminal.backend().buffer().clone();

    // The heading text carries the accent (Cyan) + BOLD mid-stream…
    let mut found = None;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if buf.cell((x, y)).is_some_and(|c| c.symbol() == "L") {
                found = buf.cell((x, y)).map(|c| (x, y, c.style()));
            }
        }
    }
    let (x, y, style) = found.expect("'L' of Live rendered");
    let _ = (x, y);
    assert_eq!(style.fg, theme().md_heading.fg);
    assert!(style.add_modifier.contains(ratatui::style::Modifier::BOLD));
    // …and the live tail cursor (▍, running yellow) closes the row.
    let mut cursor = None;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if buf.cell((x, y)).is_some_and(|c| c.symbol() == "▍") {
                cursor = buf.cell((x, y)).map(|c| c.style());
            }
        }
    }
    assert_eq!(
        cursor.expect("tail cursor rendered").fg,
        theme().tool_running.fg
    );
}

// ─── merge batch: TurnDone(Ok) merge + TurnDone(Err) rebuild ────────────────

/// The multi-step App-level contract: a reasoning → tool → reasoning →
/// answer turn ends in `TurnDone(Ok)` and the merge keeps EVERYTHING
/// in event order — both thinking blocks, both requests' meta lines
/// (request 1 with a ttft segment — FirstToken dispatched; request 2
/// without — none dispatched), the folded tool row — while `history`
/// stays the engine-side message authority.
#[tokio::test]
async fn turn_done_ok_merge_keeps_thinking_meta_and_tool_rows() {
    let mut app = test_app().await;

    app.dispatch(Action::StartTurn("go".into())).await;

    // Request 1: reasoning, a pre-tool answer, usage, end.
    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::FirstToken)).await;
    app.dispatch(Action::Engine(TuiEvent::Reasoning("think 1".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("checking".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Usage(Usage {
        input_tokens: 50,
        output_tokens: 10,
        cached_input_tokens: None,
        reasoning_tokens: None,
    })))
    .await;
    app.dispatch(Action::Engine(TuiEvent::RequestEnd)).await;

    // Tool round-trip between the requests.
    app.dispatch(Action::Engine(TuiEvent::ToolStart {
        name: "echo".into(),
        args: r#"{"text":"hi"}"#.into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::ToolEnd {
        name: "echo".into(),
        bytes: 2,
        truncated: false,
    }))
    .await;

    // Request 2: reasoning, the final answer (NO FirstToken → no ttft).
    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 2,
        model: "mock-model".into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::Reasoning("think 2".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("final".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Usage(Usage {
        input_tokens: 80,
        output_tokens: 5,
        cached_input_tokens: None,
        reasoning_tokens: None,
    })))
    .await;
    app.dispatch(Action::Engine(TuiEvent::RequestEnd)).await;

    let summary = openslate_tui::event::TurnSummary {
        run_id: RunId("merge-run".into()),
        status: openslate_core::types::RunStatus::Completed,
        messages: vec![
            user_message("go"),
            Message {
                role: MessageRole::Assistant,
                content: "checking".into(),
                tool_call_id: None,
                name: None,
                tool_calls: Some(vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "hi"}),
                }]),
                reasoning_content: None,
            },
            Message {
                role: MessageRole::Tool,
                content: "hi".into(),
                tool_call_id: Some(ToolCallId("tc-1".into())),
                name: Some("echo".into()),
                tool_calls: None,
                reasoning_content: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "final".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            },
        ],
        total_steps: 2,
        total_input_tokens: 130,
        total_output_tokens: 15,
        total_cost_usd: 0.002,
        model: "mock-model".into(),
    };
    app.dispatch(Action::Engine(TuiEvent::TurnDone(Ok(summary))))
        .await;
    assert_eq!(*app.run_state(), RunState::Idle);

    let entries = app.transcript_entries();
    let kind = |e: &TranscriptEntry| -> &'static str {
        match e {
            TranscriptEntry::User(_) => "user",
            TranscriptEntry::Reasoning(_) => "reasoning",
            TranscriptEntry::Meta(m) if m.starts_with('~') => "meta_est",
            TranscriptEntry::Meta(_) => "meta_usage",
            TranscriptEntry::Assistant(_) => "assistant",
            TranscriptEntry::ToolCall { .. } => "tool",
            TranscriptEntry::Approval { .. } | TranscriptEntry::Delegate { .. } => "other",
            TranscriptEntry::StepBreak => "sep",
        }
    };
    let kinds: Vec<&str> = entries.iter().map(kind).collect();
    let reasoning1 = kinds.iter().position(|k| *k == "reasoning").unwrap();
    let tool = kinds.iter().position(|k| *k == "tool").unwrap();
    let reasoning2 = kinds
        .iter()
        .enumerate()
        .filter(|(_, k)| **k == "reasoning")
        .nth(1)
        .expect("second reasoning block")
        .0;
    assert!(
        reasoning1 < tool && tool < reasoning2,
        "event order kept: {kinds:?}"
    );
    // Both thinking blocks in place, texts untouched.
    assert!(matches!(
        &entries[reasoning1],
        TranscriptEntry::Reasoning(t) if t == "think 1"
    ));
    assert!(matches!(
        &entries[reasoning2],
        TranscriptEntry::Reasoning(t) if t == "think 2"
    ));
    // Both requests' meta lines survived the turn end — request 1's
    // usage line carries the ttft segment (its FirstToken was
    // dispatched), request 2's does not.
    let usage_lines: Vec<&str> = entries
        .iter()
        .filter_map(|e| match e {
            TranscriptEntry::Meta(m) if m.starts_with("↑") => Some(m.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        usage_lines.len(),
        2,
        "one usage meta per request: {kinds:?}"
    );
    assert!(usage_lines[0].starts_with("↑50 ↓10"), "{}", usage_lines[0]);
    assert!(
        usage_lines[0].contains("ttft "),
        "request 1 observed FirstToken: {}",
        usage_lines[0]
    );
    assert!(usage_lines[1].starts_with("↑80 ↓5"), "{}", usage_lines[1]);
    assert!(
        !usage_lines[1].contains("ttft"),
        "request 2 saw no FirstToken: {}",
        usage_lines[1]
    );
    // The tool row folded the Tool message ("hi" → 2 bytes).
    assert!(matches!(
        &entries[tool],
        TranscriptEntry::ToolCall { status: ToolEntryStatus::Done { bytes, .. }, .. }
            if *bytes == 2
    ));
    // History stays the engine-side message authority.
    assert_eq!(app.history().len(), 4);
}

/// The Err-path client semantics (web-1): the server's `TurnError`
/// flushes the held step meta, drops the streaming buffers, and leaves
/// the committed live-only entries (reasoning/tool rows) IN PLACE — the
/// server is the history authority, so there is no local store reload
/// anymore; a reconnect snapshot is the reconciliation path.
#[tokio::test]
async fn turn_done_err_keeps_committed_live_entries() {
    let (mut app, link) = test_app_linked().await;

    app.dispatch(Action::StartTurn("hello".into())).await;
    assert!(app.is_running(), "optimistic in-flight after the submit");

    // Live-only entries exist when the turn dies: a reasoning block
    // flushed above a tool line (synthetic events).
    app.dispatch(Action::Engine(TuiEvent::Reasoning("dying thought".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::ToolStart {
        name: "echo".into(),
        args: "{}".into(),
    }))
    .await;
    assert!(
        app.transcript_entries()
            .iter()
            .any(|e| matches!(e, TranscriptEntry::Reasoning(_))),
        "reasoning visible before the failure"
    );

    // The server's failure lands through the link.
    link.emit(ServerMsg::TurnError {
        message: "boom".into(),
    });
    app.drain_engine_events().await;
    assert!(!app.is_running());
    assert!(matches!(app.run_state(), RunState::Error(_)));

    // Committed entries SURVIVE the error (user + reasoning + tool row):
    // no local rebuild — the pushed user entry and the streamed history
    // stay exactly what the user watched.
    let entries = app.transcript_entries();
    assert!(matches!(&entries[0], TranscriptEntry::User(t) if t == "hello"));
    assert!(matches!(&entries[1], TranscriptEntry::Reasoning(t) if t == "dying thought"));
    assert!(matches!(&entries[2], TranscriptEntry::ToolCall { .. }));
    // The local history mirror keeps the submitted user message (the
    // server's authoritative list arrives with the next snapshot).
    assert_eq!(app.history().len(), 1);
    assert_eq!(app.history()[0].content, "hello");
}

// ─── fix-13 + copy-1: the three-path copy chain ────────────────────────────

use openslate_tui::clipboard;

/// A capturing clipboard sink: records every payload written and
/// reports success (the injectable/capturable write seam).
fn capturing_sink() -> (Arc<std::sync::Mutex<Vec<String>>>, clipboard::ClipboardSink) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = {
        let seen = Arc::clone(&seen);
        Box::new(move |payload: &str| {
            seen.lock().expect("sink lock").push(payload.to_owned());
            true
        }) as clipboard::ClipboardSink
    };
    (seen, sink)
}

/// Snapshot of everything the sink wrote (ends the MutexGuard within
/// the statement — never held across an await).
fn written_payloads(seen: &Arc<std::sync::Mutex<Vec<String>>>) -> Vec<String> {
    seen.lock().expect("sink lock").clone()
}

/// Fresh client app with the capturing sink installed and the copy-file
/// leg redirected into a tempdir (hermetic — no test writes the real
/// `~/.local/share/openslate/last-copy.md`). A plain-text StartTurn
/// pushes the user transcript entry and sends `Submit` — the server
/// (MemLink) never answers, so no background events race the assertions.
async fn copy_test_app() -> (App, Arc<std::sync::Mutex<Vec<String>>>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let (mut app, _link) = test_app_linked().await;
    let (seen, sink) = capturing_sink();
    app.set_clipboard_sink(sink);
    app.set_copy_file_dir(tmp.path().join("copy-out"));
    (app, seen, tmp)
}

/// The copy file's expected location for a [`copy_test_app`] tempdir.
fn copy_file(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().join("copy-out").join("last-copy.md")
}

/// The chain notice's deterministic shape: `copied N [of M] chars ->
/// <file> (+osc52)`. The optional `+<tool>` tail only appears when a
/// real clipboard tool exists AND its run exited 0 — none does on the
/// dev/CI container, so both shapes are accepted by name of the
/// detected tool.
fn assert_copy_notice(notice: Option<&str>, copied: usize, total: usize, file: &std::path::Path) {
    let notice = notice.expect("copy posts a notice");
    let count = if copied < total {
        format!("copied {copied} of {total} chars")
    } else {
        format!("copied {copied} chars")
    };
    let core = format!("{count} -> {}", file.display());
    let ok = notice == format!("{core} (+osc52)")
        || clipboard::detect_clipboard_tool()
            .is_some_and(|tool| notice == format!("{core} (+osc52 +{tool})"));
    assert!(ok, "unexpected notice {notice:?} (expected core {core:?})");
}

/// Commit an assistant entry through the engine-event seam: a Delta
/// followed by RequestEnd flushes the streamed text into entries
/// (no Usage → no meta line), exactly like a live request boundary.
async fn commit_assistant(app: &mut App, text: String) {
    app.dispatch(Action::Engine(TuiEvent::Delta(text))).await;
    app.dispatch(Action::Engine(TuiEvent::RequestEnd)).await;
}

/// A TurnDone(Ok) carrying only a tool result (folded into the live
/// matching entry by name) — hands back a spare manager so the App
/// stays usable for the next turn.
fn tool_fold_turn_done(content: &str) -> TuiEvent {
    let summary = openslate_tui::event::TurnSummary {
        run_id: RunId("copy-tool".into()),
        status: openslate_core::types::RunStatus::Completed,
        messages: vec![Message {
            role: MessageRole::Tool,
            content: content.to_owned(),
            tool_call_id: None,
            name: Some("read_file".into()),
            tool_calls: None,
            reasoning_content: None,
        }],
        total_steps: 1,
        total_input_tokens: 10,
        total_output_tokens: 5,
        total_cost_usd: 0.0,
        model: "mock-model".into(),
    };
    TuiEvent::TurnDone(Ok(summary))
}

/// Empty state: Ctrl+Y with no assistant output posts the
/// `nothing to copy` notice and never touches ANY write channel.
#[tokio::test]
async fn copy_last_without_assistant_output_notices_nothing() {
    let (mut app, seen, tmp) = copy_test_app().await;
    app.dispatch(Action::CopyLast).await;
    assert_eq!(app.status_notice(), Some("nothing to copy"));
    assert!(
        written_payloads(&seen).is_empty(),
        "no payload written without assistant output"
    );
    assert!(!copy_file(&tmp).exists(), "no copy file written");

    // A user message alone is not assistant output either.
    app.dispatch(Action::StartTurn("/copy".into())).await; // no-op copy
    assert_eq!(app.status_notice(), Some("nothing to copy"));
    assert!(written_payloads(&seen).is_empty());
}

/// Happy path: the chain writes the OSC 52 payload of the last
/// assistant entry's RAW markdown (asterisks intact — entries store
/// the original text; markdown rendering happens at draw time), lands
/// the SAME text in the copy file, and the notice names the file path
/// plus `(+osc52)` (sent — not receipt-claimed). `/copy` takes the
/// same path.
#[tokio::test]
async fn copy_last_runs_osc52_and_file_legs() {
    let (mut app, seen, tmp) = copy_test_app().await;
    commit_assistant(&mut app, "Hello **world**".into()).await;
    // A later reasoning block must NOT shadow the assistant entry.
    app.dispatch(Action::Engine(TuiEvent::Reasoning("musing".into())))
        .await;

    app.dispatch(Action::CopyLast).await;
    let written = written_payloads(&seen);
    assert_eq!(written.len(), 1, "exactly one write");
    assert_eq!(
        written[0],
        clipboard::osc52_payload("Hello **world**"),
        "payload carries the raw markdown"
    );
    assert_eq!(
        std::fs::read_to_string(copy_file(&tmp)).expect("copy file"),
        "Hello **world**",
        "the file leg carries the same text"
    );
    assert_copy_notice(app.status_notice(), 15, 15, &copy_file(&tmp));

    // `/copy` — the slash alias writes the same payload again.
    let (seen2, sink2) = capturing_sink();
    app.set_clipboard_sink(sink2);
    app.dispatch(Action::StartTurn("/copy".into())).await;
    assert_eq!(
        written_payloads(&seen2),
        vec![clipboard::osc52_payload("Hello **world**")],
        "/copy aliases Ctrl+Y"
    );
    assert_copy_notice(app.status_notice(), 15, 15, &copy_file(&tmp));
}

/// The last entry WINS: a second assistant block replaces the copy
/// source on EVERY leg.
#[tokio::test]
async fn copy_last_takes_the_most_recent_assistant_block() {
    let (mut app, _seen, tmp) = copy_test_app().await;
    commit_assistant(&mut app, "first answer".into()).await;
    commit_assistant(&mut app, "second **answer**".into()).await;

    app.dispatch(Action::CopyLast).await;
    assert_eq!(
        std::fs::read_to_string(copy_file(&tmp)).expect("copy file"),
        "second **answer**",
        "the file leg holds the LATEST block"
    );
    assert_copy_notice(app.status_notice(), 17, 17, &copy_file(&tmp));
}

/// Length cap: >32 KiB assistant text truncates to 32 KiB on ALL legs
/// and the notice says how much was taken of how much existed. A CJK
/// char straddling the cap boundary is never split (char-boundary-safe
/// walk-back shows up as N of M chars too).
#[tokio::test]
async fn copy_last_truncates_at_32_kib() {
    let (mut app, seen, tmp) = copy_test_app().await;
    commit_assistant(&mut app, "y".repeat(clipboard::COPY_MAX_BYTES + 5)).await;
    app.dispatch(Action::CopyLast).await;
    let expected = "y".repeat(clipboard::COPY_MAX_BYTES);
    assert_eq!(
        written_payloads(&seen),
        vec![clipboard::osc52_payload(&expected)]
    );
    assert_eq!(
        std::fs::read_to_string(copy_file(&tmp))
            .expect("copy file")
            .len(),
        clipboard::COPY_MAX_BYTES,
        "the file leg truncates identically"
    );
    assert_copy_notice(app.status_notice(), 32768, 32773, &copy_file(&tmp));

    // CJK straddle: 32767 ASCII bytes + two 3-byte chars = 32769 chars;
    // the cut walks back to 32767 bytes (no partial char).
    let (mut app, seen, tmp) = copy_test_app().await;
    let mut text = "a".repeat(clipboard::COPY_MAX_BYTES - 1);
    text.push_str("中中");
    commit_assistant(&mut app, text).await;
    app.dispatch(Action::CopyLast).await;
    let expected = "a".repeat(clipboard::COPY_MAX_BYTES - 1);
    assert_eq!(
        written_payloads(&seen),
        vec![clipboard::osc52_payload(&expected)]
    );
    assert_eq!(
        std::fs::read_to_string(copy_file(&tmp)).expect("copy file"),
        expected
    );
    assert_copy_notice(app.status_notice(), 32767, 32769, &copy_file(&tmp));
}

/// EVERY leg failing → `clipboard write failed`: the sink reports
/// failure AND the file dir is blocked (a regular FILE where the dir
/// should be). The tool leg is environment-dependent — on machines
/// without a clipboard tool (the dev/CI container) the notice is
/// exactly the failure string; with one it may rescue, so only the
/// deterministic absences are asserted there.
#[tokio::test]
async fn copy_all_legs_failed_notices_failure() {
    let (mut app, _seen, tmp) = copy_test_app().await;
    commit_assistant(&mut app, "answer".into()).await;
    app.set_clipboard_sink(Box::new(|_| false));
    // Block the file leg: a regular FILE at the copy dir path makes
    // create_dir_all fail.
    std::fs::write(tmp.path().join("copy-out"), b"not a dir").expect("blocker");

    app.dispatch(Action::CopyLast).await;
    let notice = app.status_notice().expect("notice").to_owned();
    assert!(!notice.contains("->"), "file leg failed: {notice}");
    assert!(
        !notice.contains("(+osc52)"),
        "sink reported failure: {notice}"
    );
    if clipboard::detect_clipboard_tool().is_none() {
        assert_eq!(notice, "clipboard write failed");
    }
}

/// `/copy all`: the whole transcript as plain text — user messages
/// `> `-prefixed, assistant text verbatim, tool entries one-line
/// summaries with the retained output's line count (P2) — riding the
/// same chain (payload + file contents + notice).
#[tokio::test]
async fn copy_all_renders_the_transcript_as_plain_text() {
    let (mut app, seen, tmp) = copy_test_app().await;
    // User entry: the offline factory pushes it and bails cleanly.
    app.dispatch(Action::StartTurn("hello user".into())).await;
    commit_assistant(&mut app, "first answer".into()).await;

    // A tool call whose output folds in at the TurnDone merge.
    app.dispatch(Action::Engine(TuiEvent::ToolStart {
        name: "read_file".into(),
        args: r#"{"path":"a.rs"}"#.into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::ToolEnd {
        name: "read_file".into(),
        bytes: 12,
        truncated: false,
    }))
    .await;
    commit_assistant(&mut app, "final answer".into()).await;
    app.dispatch(Action::Engine(tool_fold_turn_done("line1\nline2")))
        .await;
    assert!(
        app.transcript()
            .last_tool_output()
            .is_some_and(|t| t == "line1\nline2"),
        "fixture: the tool output folded in"
    );

    app.dispatch(Action::StartTurn("/copy all".into())).await;
    let expected = "> hello user\n\nfirst answer\n\n\
                    [tool] read_file({\"path\":\"a.rs\"}) · 2 output lines\n\
                    final answer\n\n";
    let n = expected.chars().count();
    assert_eq!(
        written_payloads(&seen),
        vec![clipboard::osc52_payload(expected)],
        "the plain-text rendering rides the OSC 52 leg"
    );
    assert_eq!(
        std::fs::read_to_string(copy_file(&tmp)).expect("copy file"),
        expected,
        "the plain-text rendering lands in the file verbatim"
    );
    assert_copy_notice(app.status_notice(), n, n, &copy_file(&tmp));
}

/// `/copy tool`: the most recent tool call's retained output text
/// (fix-25 detail storage, filled by the turn's merge). No tool (or
/// no retained output) → `nothing to copy`.
#[tokio::test]
async fn copy_tool_takes_the_last_tool_output() {
    let (mut app, seen, tmp) = copy_test_app().await;
    // Nothing at all.
    app.dispatch(Action::StartTurn("/copy tool".into())).await;
    assert_eq!(app.status_notice(), Some("nothing to copy"));
    assert!(written_payloads(&seen).is_empty());
    assert!(!copy_file(&tmp).exists());

    // A folded tool output.
    app.dispatch(Action::Engine(TuiEvent::ToolStart {
        name: "read_file".into(),
        args: "{}".into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::ToolEnd {
        name: "read_file".into(),
        bytes: 14,
        truncated: false,
    }))
    .await;
    app.dispatch(Action::Engine(tool_fold_turn_done("file body here")))
        .await;

    app.dispatch(Action::StartTurn("/copy tool".into())).await;
    assert_eq!(
        std::fs::read_to_string(copy_file(&tmp)).expect("copy file"),
        "file body here",
        "the tool output text lands in the file"
    );
    assert_eq!(
        written_payloads(&seen),
        vec![clipboard::osc52_payload("file body here")]
    );
    assert_copy_notice(app.status_notice(), 14, 14, &copy_file(&tmp));
}

/// `/copy` with an unknown argument shows the usage notice and
/// copies nothing.
#[tokio::test]
async fn copy_with_unknown_arg_shows_usage() {
    let (mut app, seen, tmp) = copy_test_app().await;
    commit_assistant(&mut app, "answer".into()).await;
    app.dispatch(Action::StartTurn("/copy everything".into()))
        .await;
    assert_eq!(app.status_notice(), Some("usage: /copy [all|tool]"));
    assert!(written_payloads(&seen).is_empty(), "no OSC 52 write");
    assert!(!copy_file(&tmp).exists(), "no copy file written");
}

// ─── web-1: client-lifecycle behaviors ─────────────────────────────────────

/// `/new`: sends `NewSession` when idle; while a turn is running the
/// guard posts the notice instead (the reset broadcast owns the swap).
#[tokio::test]
async fn new_session_sends_and_guards_while_running() {
    let (mut app, link) = test_app_linked().await;
    app.dispatch(Action::StartTurn("/new".into())).await;
    assert_eq!(link.take_sent(), vec![ClientMsg::NewSession]);
    // The notice lands with the broadcast, not the local keypress.
    link.emit(ServerMsg::SessionReset);
    app.drain_engine_events().await;
    assert_eq!(app.status_notice(), Some("已开启新会话"));

    // While a turn runs: guarded, nothing leaves.
    app.dispatch(Action::StartTurn("go".into())).await;
    link.take_sent();
    app.dispatch(Action::StartTurn("/new".into())).await;
    assert!(link.sent_snapshot().is_empty(), "guarded while running");
    assert_eq!(
        app.status_notice(),
        Some("turn running — Ctrl+C to cancel, then /new")
    );
}

/// `/model <alias>`: known alias → `SetModel` leaves, the notice lands
/// with the `ModelChanged` broadcast; unknown alias → local mirror
/// error, nothing leaves.
#[tokio::test]
async fn model_switch_flows_through_the_server() {
    let (mut app, link) = test_app_linked().await;
    app.dispatch(Action::StartTurn("/model fast".into())).await;
    assert_eq!(
        link.take_sent(),
        vec![ClientMsg::SetModel {
            alias: "fast".into()
        }]
    );
    assert_eq!(app.status_notice(), None, "no notice until the broadcast");
    link.emit(ServerMsg::ModelChanged {
        alias: "fast".into(),
    });
    app.drain_engine_events().await;
    assert_eq!(app.status_notice(), Some("model → fast"));

    // Unknown alias: rejected against the mirror, nothing leaves.
    app.dispatch(Action::StartTurn("/model nope".into())).await;
    assert!(link.take_sent().is_empty());
    assert!(matches!(app.run_state(), RunState::Error(_)));
}

/// Link-phase notices: the INITIAL connect is silent (the UI is not on
/// screen yet — a notice there would linger as a stale banner on the
/// first frame, the "首次打开显示重连中" bug); a mid-run drop announces
/// with the attempt number; recovery announces only after a real drop.
#[tokio::test]
async fn link_state_notice_lifecycle() {
    let (mut app, link) = test_app_linked().await;
    assert_eq!(app.status_notice(), None);

    // A late/defensive Connecting event stays silent.
    link.emit_tui(TuiEvent::LinkState(LinkPhase::Connecting));
    app.drain_engine_events().await;
    assert_eq!(app.status_notice(), None);

    // Mid-run drop → attempt-numbered freeze notice.
    link.emit_tui(TuiEvent::LinkState(LinkPhase::Reconnecting { attempt: 2 }));
    app.drain_engine_events().await;
    assert_eq!(app.status_notice(), Some("已断线，第 2 次重连中…"));

    // Recovery after a real drop → announced.
    link.emit_tui(TuiEvent::LinkState(LinkPhase::Connected));
    app.drain_engine_events().await;
    assert_eq!(app.status_notice(), Some("已重新连接"));

    // A repeated Connected (snapshot's defensive path) never re-announces.
    link.emit_tui(TuiEvent::LinkState(LinkPhase::Connected));
    app.drain_engine_events().await;
    assert_eq!(app.status_notice(), Some("已重新连接"));
}

/// Snapshot rebuild: the server's entry mirror swaps the transcript
/// WHOLESALE (live-only kinds included), session state re-syncs, and a
/// second snapshot REPLACES rather than appends.
#[tokio::test]
async fn snapshot_rebuilds_the_transcript_wholesale() {
    use openslate_protocol::EntryDto;
    let (mut app, link) = test_app_linked().await;
    // Local live state first (submitted turn in flight).
    app.dispatch(Action::StartTurn("local".into())).await;
    assert!(app.is_running());

    let mut snap = snapshot_dto("main");
    snap.session_id = "snap-2".into();
    snap.running = false;
    snap.transcript = vec![
        EntryDto::User {
            text: "history user".into(),
        },
        EntryDto::Assistant {
            text: "history answer".into(),
        },
        EntryDto::Reasoning {
            text: "history reasoning".into(),
        },
        EntryDto::Meta {
            text: "↑1 ↓2".into(),
        },
    ];
    link.emit(ServerMsg::Snapshot {
        session: Box::new(snap),
    });
    app.drain_engine_events().await;

    let entries = app.transcript_entries();
    assert_eq!(
        entries.len(),
        4,
        "the local user entry is GONE — wholesale swap"
    );
    assert!(matches!(&entries[0], TranscriptEntry::User(t) if t == "history user"));
    assert!(matches!(&entries[1], TranscriptEntry::Assistant(t) if t == "history answer"));
    assert!(matches!(&entries[2], TranscriptEntry::Reasoning(t) if t == "history reasoning"));
    assert!(matches!(&entries[3], TranscriptEntry::Meta(t) if t == "↑1 ↓2"));
    assert_eq!(app.session_run_id(), Some("snap-2"));
    assert!(!app.is_running(), "running re-synced from the snapshot");

    // A second snapshot REPLACES the first.
    let mut snap2 = snapshot_dto("main");
    snap2.transcript = vec![EntryDto::User {
        text: "only".into(),
    }];
    link.emit(ServerMsg::Snapshot {
        session: Box::new(snap2),
    });
    app.drain_engine_events().await;
    assert_eq!(app.transcript_entries().len(), 1);
}

/// Pending input auto-submits when the turn completes (TurnOk) and the
/// optimistic in-flight flag retires on both TurnOk/TurnError.
#[tokio::test]
async fn pending_input_auto_submits_after_turn_ok() {
    let (mut app, link) = test_app_linked().await;
    app.dispatch(Action::StartTurn("first".into())).await;
    link.take_sent();
    // Queued while the first turn runs.
    app.dispatch(Action::StartTurn("second".into())).await;
    assert!(link.take_sent().is_empty(), "queued, not sent");

    link.emit(ServerMsg::TurnOk {
        summary: Box::new(openslate_protocol::TurnSummaryDto {
            run_id: RunId("t1".into()),
            status: openslate_core::types::RunStatus::Completed,
            messages: vec![user_message("first")],
            total_steps: 1,
            total_input_tokens: 10,
            total_output_tokens: 5,
            total_cost_usd: 0.001,
            model: "mock-model".into(),
        }),
    });
    app.drain_engine_events().await;
    assert_eq!(
        link.take_sent(),
        vec![ClientMsg::Submit {
            text: "second".into()
        }],
        "the queued input auto-submitted"
    );
    assert!(app.is_running(), "optimistic flag set for the next turn");
}

/// Ctrl+C while running sends `Cancel` (the server runs the engine-side
/// cancel; TurnOk/TurnError closes the turn).
#[tokio::test]
async fn cancel_while_running_sends_cancel() {
    let (mut app, link) = test_app_linked().await;
    app.dispatch(Action::StartTurn("go".into())).await;
    link.take_sent();
    app.dispatch(Action::CancelTurn).await;
    assert_eq!(link.take_sent(), vec![ClientMsg::Cancel]);
    assert!(app.is_cancelled());
}
