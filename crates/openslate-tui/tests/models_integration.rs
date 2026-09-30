//! model-mgmt integration — the `/provider` management overlay end to
//! end through the REAL App dispatcher, client edition (web-1).
//!
//! The overlay's UI machinery is unchanged (slash routing, modal
//! swallowing, form editing, combo box, in-panel reference guards — all
//! read the config MIRROR). What changed is the commit path: `a/e/d`
//! sends CRUD `ClientMsg`s on the [`MemLink`] and the server's
//! `ConfigChanged` broadcast swaps the mirror + posts the success
//! notice (a directed `Notice` is the failure verdict). File write
//! routing (global library vs active fallback vs `.env`) lives in the
//! server now — these tests assert the WIRE behavior instead.

use std::sync::Arc;

use openslate_core::config::parse_openslate_toml;
use openslate_protocol::{ClientMsg, ConfigViewDto, ServerMsg};
use openslate_tui::action::Action;
use openslate_tui::app::{App, ClientBootstrap};
use openslate_tui::client::MemLink;

/// The merged fixture config (what the server's snapshot carries): the
/// project-local tables plus the global library's entries.
fn fixture_toml() -> String {
    r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "TUI_TEST_KEY"

[providers.lib]
base_url = "https://lib.example"
api_key_env = "LIB_KEY"

[models.main]
provider = "mock"
model = "mock-model"

[models.fast]
provider = "mock"
model = "mock-model"

[models.libmodel]
provider = "lib"
model = "lib-1"

[levels]
main = "main"
fast = "fast"

[limits]
max_steps = 10
max_depth = 4
max_tool_calls = 20
max_context_bytes = 100_000
max_output_bytes = 10_000
"#
    .to_owned()
}

/// Build a wire config view from a TOML string (the fixture + whatever
/// the "server" changed) — the ConfigChanged/Snapshot payload shape.
fn view_from_toml(toml: &str) -> ConfigViewDto {
    openslate_tui::client::view_from_config(&parse_openslate_toml(toml).expect("fixture parses"))
}

/// The client App + recording link, seeded with the merged fixture as
/// a hello snapshot (Connected + mirror swap).
async fn fixture_app() -> (App, Arc<MemLink>) {
    let config = parse_openslate_toml(&fixture_toml()).expect("fixture parses");
    let (link, events) = MemLink::pair();
    let mut app = App::new(ClientBootstrap {
        link: link.clone(),
        events,
        config,
        root_agent_id: "root".into(),
    });
    link.emit(ServerMsg::Snapshot {
        session: Box::new(openslate_protocol::SnapshotDto {
            proto: 1,
            session_id: "models-test".into(),
            session_label: "models test".into(),
            transcript: vec![],
            running: false,
            depth_cur: 0,
            agents_running: 0,
            tool_calls_cur: 0,
            model_alias: "main".into(),
            pending_approval: None,
            config: view_from_toml(&fixture_toml()),
            session_stats: Default::default(),
        }),
    });
    app.drain_engine_events().await;
    (app, link)
}

/// The server ACK for a successful CRUD: the new view broadcast. Inject
/// through the link + drain, so the mirror swap + notice run through
/// the real dispatch path.
async fn server_ack(app: &mut App, link: &MemLink, new_toml: &str) {
    link.emit(ServerMsg::ConfigChanged {
        config: Box::new(view_from_toml(new_toml)),
    });
    app.drain_engine_events().await;
}

async fn type_into(app: &mut App, s: &str) {
    for c in s.chars() {
        app.dispatch(Action::InputChar(c)).await;
    }
}

/// Open the overlay through the real `/provider` slash routing.
async fn open_models(app: &mut App) {
    type_into(app, "/provider").await;
    app.dispatch(Action::SubmitInput).await;
}

/// While the overlay is up, ordinary typing is swallowed (the modal
/// guard); `landed == false` asserts that.
async fn probe_swallowed(app: &mut App, c: char) -> bool {
    app.dispatch(Action::InputChar(c)).await;
    !app.input_text().contains(c)
}

/// Add a provider through the overlay form: `a`, name, Tab, base_url,
/// Tab, api_key_env, Enter.
async fn form_add_provider(app: &mut App, name: &str, base_url: &str, key_env: &str) {
    app.dispatch(Action::InputChar('a')).await;
    type_into(app, name).await;
    app.dispatch(Action::FocusNext).await;
    type_into(app, base_url).await;
    app.dispatch(Action::FocusNext).await;
    type_into(app, key_env).await;
    app.dispatch(Action::SubmitInput).await;
}

/// `/provider` opens through the real slash routing; the modal
/// swallows typing and section keys; Esc returns the input to normal.
#[tokio::test]
async fn models_overlay_opens_swallows_and_closes() {
    let (mut app, _link) = fixture_app().await;
    open_models(&mut app).await;
    assert!(
        probe_swallowed(&mut app, 'x').await,
        "typing is swallowed while the overlay is open"
    );
    // Tab must NOT cycle focus into the transcript while the modal is
    // up (FocusNext is eaten by the models handler).
    app.dispatch(Action::FocusNext).await;
    // Esc closes: the next char lands in the input again.
    app.dispatch(Action::DismissOverlay).await;
    app.dispatch(Action::InputChar('y')).await;
    assert_eq!(app.input_text(), "y", "input editing resumes after Esc");
}

/// The ranking contract survives the renamed command: `/mo` + Tab
/// still applies `/model ` (`provider` left the `/mo` prefix class
/// when `/models` was renamed, so the class is {model, mouse}).
#[tokio::test]
async fn mo_tab_contract_survives_provider_rename() {
    let (mut app, _link) = fixture_app().await;
    type_into(&mut app, "/mo").await;
    assert!(app.input_completion_open());
    app.dispatch(Action::FocusNext).await;
    assert_eq!(app.input_text(), "/model ", "`/mo`+Tab → `/model `");
    // `/mod` + Tab lands on /model too (its sole match now).
    for _ in 0..7 {
        app.dispatch(Action::InputBackspace).await; // clear "/model "
    }
    assert_eq!(app.input_text(), "");
    type_into(&mut app, "/mod").await;
    assert!(app.input_completion_open());
    app.dispatch(Action::FocusNext).await;
    assert_eq!(app.input_text(), "/model ", "`/mod`+Tab → `/model `");
}

/// Provider add → `UpsertProvider` leaves → the server's
/// `ConfigChanged` swaps the mirror (success notice) → a model entry
/// bound to the new provider becomes `/model`-switchable immediately.
#[tokio::test]
async fn provider_upsert_sends_and_hot_swaps_on_config_changed() {
    let (mut app, link) = fixture_app().await;
    open_models(&mut app).await;
    form_add_provider(&mut app, "acme", "https://api.acme.example", "ACME_API_KEY").await;

    let sent = link.take_sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMsg::UpsertProvider { name, provider }]
                if name == "acme" && provider.base_url == "https://api.acme.example"
        ),
        "the upsert left for the server: {sent:?}"
    );
    assert_eq!(
        app.status_notice(),
        None,
        "no local notice — the server's verdict decides"
    );

    // Server ACK: the new view carries the provider.
    let mut ack = fixture_toml();
    ack.push_str(
        "\n[providers.acme]\nbase_url = \"https://api.acme.example\"\napi_key_env = \"ACME_API_KEY\"\n",
    );
    server_ack(&mut app, &link, &ack).await;
    assert_eq!(app.status_notice(), Some("已保存并生效 · provider acme"));

    // Add a model entry bound to the new provider through the SAME open
    // overlay (multi-edit session): Models tab → a. The snapshot
    // re-synced from the new view, so the provider choice's head IS the
    // fresh "acme".
    app.dispatch(Action::InputChar('2')).await;
    app.dispatch(Action::InputChar('a')).await;
    type_into(&mut app, "acme-1").await;
    app.dispatch(Action::FocusNext).await; // provider choice, head = acme
    app.dispatch(Action::FocusNext).await; // model id
    type_into(&mut app, "acme-model-1").await;
    app.dispatch(Action::SubmitInput).await;
    assert!(
        matches!(
            link.take_sent().as_slice(),
            [ClientMsg::UpsertModel { entry, model }]
                if entry == "acme-1" && model.provider == "acme"
        ),
        "the model upsert left for the server"
    );

    // Hot swap: the new entry resolves right away through /model (the
    // mirror knows it after a second ACK).
    let mut ack2 = ack;
    ack2.push_str("\n[models.acme-1]\nprovider = \"acme\"\nmodel = \"acme-model-1\"\n");
    server_ack(&mut app, &link, &ack2).await;
    app.dispatch(Action::DismissOverlay).await;
    type_into(&mut app, "/model acme-1").await;
    app.dispatch(Action::SubmitInput).await;
    assert!(
        matches!(
            link.take_sent().as_slice(),
            [ClientMsg::SetModel { alias }] if alias == "acme-1"
        ),
        "the swapped mirror knows the new entry"
    );
}

/// Pasting an API key sends `UpsertProvider` + `SetApiKey` (the server
/// derives the variable name and writes `.env` itself).
#[tokio::test]
async fn pasted_api_key_sends_upsert_and_set_api_key() {
    let (mut app, link) = fixture_app().await;
    open_models(&mut app).await;
    app.dispatch(Action::InputChar('a')).await;
    type_into(&mut app, "zhipu").await;
    app.dispatch(Action::FocusNext).await;
    type_into(&mut app, "https://api.zhipu.example").await;
    app.dispatch(Action::FocusNext).await;
    // api_key_env left EMPTY; paste the key instead.
    app.dispatch(Action::FocusNext).await;
    app.dispatch(Action::PasteText("sk-live-abc123".into()))
        .await;
    app.dispatch(Action::SubmitInput).await;

    let sent = link.take_sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMsg::UpsertProvider { name, .. }, ClientMsg::SetApiKey { provider, value }]
                if name == "zhipu" && provider == "zhipu" && value == "sk-live-abc123"
        ),
        "upsert + set_api_key pair: {sent:?}"
    );
}

/// Level rebinding sends `SetLevel`; the ConfigChanged ACK swaps the
/// mirror + posts the notice.
#[tokio::test]
async fn level_rebind_sends_set_level_and_swaps() {
    let (mut app, link) = fixture_app().await;
    open_models(&mut app).await;
    app.dispatch(Action::InputChar('3')).await; // Levels tab
    app.dispatch(Action::InputHistoryNext).await; // row 1 = fast (required)
    app.dispatch(Action::SubmitInput).await; // Enter = rebind
                                             // Picker: entries sorted [fast, libmodel, main] →
                                             // two notches land on main.
    app.dispatch(Action::InputHistoryNext).await;
    app.dispatch(Action::InputHistoryNext).await;
    app.dispatch(Action::SubmitInput).await;
    assert_eq!(
        link.take_sent(),
        vec![ClientMsg::SetLevel {
            level: "fast".into(),
            entry: "main".into()
        }],
        "the rebind left for the server"
    );

    let ack = fixture_toml().replace("fast = \"fast\"", "fast = \"main\"");
    server_ack(&mut app, &link, &ack).await;
    assert_eq!(
        app.status_notice(),
        Some("已保存并生效 · levels.fast → main")
    );
}

/// A provider still referenced by a model entry cannot be deleted: the
/// CLIENT-side guard (reads the mirror) refuses with the referencing
/// names and nothing leaves.
#[tokio::test]
async fn referenced_provider_delete_is_refused() {
    let (mut app, link) = fixture_app().await;
    open_models(&mut app).await;
    // Cursor row 0 on Providers = lib (sorted: lib < mock): libmodel
    // references it.
    app.dispatch(Action::InputChar('d')).await;
    app.dispatch(Action::InputChar('y')).await;
    let notice = app.status_notice().unwrap_or_default();
    assert!(
        notice.contains("无法删除 provider"),
        "refusal notice: {notice}"
    );
    assert!(link.take_sent().is_empty(), "nothing left for the server");
}

/// `main`/`fast` are required levels: the component refuses the delete
/// in-panel (warning flash, no confirmation step, nothing sent); a
/// custom level round-trips through the wire (add → ack → delete → ack).
#[tokio::test]
async fn required_level_refuses_and_custom_level_round_trips() {
    let (mut app, link) = fixture_app().await;
    open_models(&mut app).await;
    app.dispatch(Action::InputChar('3')).await; // Levels
                                                // Cursor on row 0 = main (required).
    app.dispatch(Action::InputChar('d')).await;
    let notice = app.status_notice().unwrap_or_default();
    assert!(
        notice.is_empty(),
        "the refusal is the in-panel flash, not a status notice: {notice}"
    );
    // Still in the list (no confirm step was reached): 'y' is inert.
    app.dispatch(Action::InputChar('y')).await;
    assert!(link.take_sent().is_empty());

    // Add a custom level through the overlay: a → name → Enter →
    // picker → Enter.
    app.dispatch(Action::InputChar('a')).await;
    type_into(&mut app, "draft").await;
    app.dispatch(Action::SubmitInput).await; // name → picker
    app.dispatch(Action::SubmitInput).await; // pick first entry (fast)
    assert_eq!(
        link.take_sent(),
        vec![ClientMsg::SetLevel {
            level: "draft".into(),
            entry: "fast".into()
        }],
        "the custom level left for the server"
    );
    let ack = fixture_toml().replace("fast = \"fast\"", "fast = \"fast\"\ndraft = \"fast\"");
    server_ack(&mut app, &link, &ack).await;

    // Delete it: it renders after main/fast → cursor to row 2.
    app.dispatch(Action::InputHistoryNext).await;
    app.dispatch(Action::InputHistoryNext).await;
    app.dispatch(Action::InputChar('d')).await;
    app.dispatch(Action::InputChar('y')).await;
    assert_eq!(
        link.take_sent(),
        vec![ClientMsg::DeleteLevel {
            level: "draft".into()
        }],
        "the deletion left for the server"
    );
}

/// A server-side rejection (directed Notice) keeps the previous mirror:
/// the overlay re-syncs to the OLD config and the pending confirmation
/// clears — the failure verdict IS the notice.
#[tokio::test]
async fn server_rejection_keeps_the_previous_mirror() {
    let (mut app, link) = fixture_app().await;
    open_models(&mut app).await;
    form_add_provider(&mut app, "acme", "https://api.acme.example", "ACME_API_KEY").await;
    assert!(matches!(
        link.take_sent().as_slice(),
        [ClientMsg::UpsertProvider { .. }]
    ));
    // The server rejects (e.g. its validation backstop found something).
    link.emit(ServerMsg::Notice {
        text: "保存失败 · provider acme 校验未通过".into(),
        level: openslate_protocol::NoticeLevel::Warn,
    });
    app.drain_engine_events().await;
    assert_eq!(
        app.status_notice(),
        Some("保存失败 · provider acme 校验未通过")
    );
    // No ConfigChanged follows → the mirror keeps the old tables: the
    // next /model resolves against the pre-save state only.
    type_into(&mut app, "/model acme").await;
    app.dispatch(Action::SubmitInput).await;
    assert!(
        link.take_sent().is_empty(),
        "unknown alias rejected locally"
    );
}
