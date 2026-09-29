//! slash-1 integration — dispatch-level slash-command completion.
//!
//! Drives the REAL App's dispatcher with the exact Action stream
//! `map_event` produces from key events (chars, Tab, Enter, Esc)
//! against a temp project (the `select_integration.rs` harness
//! pattern): the `/`-prefixed input opens the filtered command list,
//! Tab applies, Enter runs the completed command through the real
//! `handle_slash` routing, and Esc dismisses without eating the
//! draft. Rendering-level assertions live in the crate's component
//! tests (`src/components/input.rs`) and App layout tests
//! (`src/app.rs::borderless_layout_tests`).

use openslate_app::wiring::build_app_context;
use openslate_tui::action::Action;
use openslate_tui::app::{App, DispatchOutcome};

/// Real temp project for `build_app_context` (config + agents +
/// store); the `[database]` path is ABSOLUTE inside the tempdir so
/// nothing leaks into the user's global store. Mirrors
/// `select_integration.rs::temp_project`.
fn temp_project() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let openslate_dir = tmp.path().join(".openslate");
    std::fs::create_dir(&openslate_dir).expect("create .openslate dir");
    let db_path = openslate_dir.join("test.sqlite");
    std::fs::write(
        openslate_dir.join("openslate.toml"),
        format!(
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

[database]
path = {db_path:?}

[limits]
max_steps = 10
max_depth = 4
max_tool_calls = 20
max_context_bytes = 100_000
max_output_bytes = 10_000
"#
        ),
    )
    .expect("write toml");
    let agents_dir = openslate_dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create agents dir");
    std::fs::write(
        agents_dir.join("root.md"),
        "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - read_file\n---\nYou are the root agent.\n",
    )
    .expect("write root.md");
    tmp
}

async fn completion_app() -> (App, tempfile::TempDir) {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    (App::new(ctx), tmp)
}

/// Type a string through the dispatcher (what chars become).
async fn type_into(app: &mut App, s: &str) {
    for c in s.chars() {
        app.dispatch(Action::InputChar(c)).await;
    }
}

/// `/mo` opens the filtered list; Tab completes to `/model ` and the
/// ARGUMENT list reopens with the config's real aliases; the
/// confirming Enter pair runs `/model fast` through handle_slash (the
/// notice proves the switch happened).
#[tokio::test]
async fn model_completion_flows_through_the_real_dispatcher() {
    let (mut app, _tmp) = completion_app().await;

    type_into(&mut app, "/mo").await;
    assert!(app.input_completion_open(), "`/mo` opens the list");

    // Tab applies the selection — `/model ` — and the dynamic alias
    // list reopens (main + fast from the temp config).
    app.dispatch(Action::FocusNext).await;
    assert_eq!(app.input_text(), "/model ");
    assert!(app.input_completion_open(), "the args list reopens");

    // Type an alias fragment; Enter applies (changed → no submit)…
    type_into(&mut app, "fa").await;
    assert_eq!(app.input_text(), "/model fa");
    app.dispatch(Action::SubmitInput).await;
    assert_eq!(app.input_text(), "/model fast", "Enter applied the arg");
    assert!(app.input_completion_open(), "waiting for the confirm");

    // …the second Enter submits verbatim; handle_slash switches the
    // model and posts the transient notice.
    app.dispatch(Action::SubmitInput).await;
    assert_eq!(app.input_text(), "", "the submit took the buffer");
    assert!(!app.input_completion_open());
    assert_eq!(
        app.status_notice(),
        Some("model → fast".to_owned()).as_deref()
    );
}

/// Tab while the list is open APPLIES instead of cycling focus: after
/// the apply the input still receives characters.
#[tokio::test]
async fn tab_applies_without_losing_input_focus() {
    let (mut app, _tmp) = completion_app().await;
    type_into(&mut app, "/").await;
    assert!(app.input_completion_open());
    app.dispatch(Action::FocusNext).await; // applies /help (head row)
    assert_eq!(app.input_text(), "/help ");
    assert!(!app.input_completion_open(), "help takes no args");
    // Focus stayed on the input: typing continues the buffer.
    type_into(&mut app, "x").await;
    assert_eq!(app.input_text(), "/help x");
}

/// Esc dismisses the list but never eats the draft; editing resumes.
#[tokio::test]
async fn esc_dismisses_the_list_and_keeps_the_draft() {
    let (mut app, _tmp) = completion_app().await;
    type_into(&mut app, "/mo").await;
    assert!(app.input_completion_open());
    app.dispatch(Action::DismissOverlay).await;
    assert!(!app.input_completion_open());
    assert_eq!(app.input_text(), "/mo");
    // The closed list no longer intercepts: ↑ walks HISTORY again
    // (empty here — no crash), and typing still lands.
    type_into(&mut app, "use").await;
    assert_eq!(app.input_text(), "/mouse");
    // `/mouse` still matches → the live filter reopened the list.
    assert!(app.input_completion_open());
}

/// `//` is the literal-escape prefix — the completion never opens and
/// the submit strips one slash and routes as a NORMAL prompt (REPL
/// semantics), never into handle_slash (whose signature error would
/// be "unknown command"). The mock provider may fail to build on
/// this env — that Error path is fine, the routing is the contract.
#[tokio::test]
async fn double_slash_escape_stays_untouched() {
    let (mut app, _tmp) = completion_app().await;
    type_into(&mut app, "//help").await;
    assert!(!app.input_completion_open(), "`//` never opens the list");
    app.dispatch(Action::SubmitInput).await;
    assert!(
        !app.input_completion_open(),
        "the Err-path restore (set_text) closes the list too"
    );
    if let openslate_tui::components::RunState::Error(e) = app.run_state() {
        assert!(
            !e.contains("unknown command"),
            "`//help` must not route into handle_slash: {e}"
        );
    }
}

/// `/exit` + Enter runs the real slash routing: immediate Quit, no
/// confirmation modal.
#[tokio::test]
async fn exit_enter_quits_immediately() {
    let (mut app, _tmp) = completion_app().await;
    type_into(&mut app, "/exit").await;
    assert!(app.input_completion_open());
    assert_eq!(
        app.dispatch(Action::SubmitInput).await,
        DispatchOutcome::Quit
    );
}

/// `/copy` + Enter behaves like Tab (the argument list opens, no
/// submit); choosing `all` through the two-Enter protocol runs the
/// real copy chain — with nothing to copy the honest notice fires.
#[tokio::test]
async fn copy_arg_completion_reaches_handle_slash() {
    let (mut app, _tmp) = completion_app().await;
    type_into(&mut app, "/copy").await;
    app.dispatch(Action::SubmitInput).await; // (b): apply + reopen
    assert_eq!(app.input_text(), "/copy ");
    assert!(app.input_completion_open());
    type_into(&mut app, "al").await; // filters to `all`
    app.dispatch(Action::SubmitInput).await; // apply (changed)
    assert_eq!(app.input_text(), "/copy all");
    app.dispatch(Action::SubmitInput).await; // identity → submit
    assert_eq!(app.input_text(), "");
    assert_eq!(
        app.status_notice(),
        Some("nothing to copy".to_owned()).as_deref(),
        "the real /copy all ran against an empty transcript"
    );
}
