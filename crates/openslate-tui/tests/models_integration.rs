//! model-mgmt-2 integration — the `/provider` management overlay end to
//! end through the REAL App dispatcher: slash routing, modal swallowing,
//! form editing, the persist write routing (global library vs active
//! fallback vs `.env`), the merged-config hot swap, and the deletion
//! reference guards.
//!
//! Harness: `slash_completion.rs`'s temp-project pattern plus a temp
//! global library dir wired in via `ctx.global_config_path`
//! (`select_config_files` is pure on its arguments, so layered tests
//! drive both paths explicitly).

use openslate_app::wiring::build_app_context;
use openslate_tui::action::Action;
use openslate_tui::app::App;

/// A layered fixture: a project-local config (providers/models/levels +
/// agents + store) plus a global library with its own provider and
/// model entry. Returns `(project_dir, global_dir, local_config_path)`.
fn layered_fixture() -> (tempfile::TempDir, tempfile::TempDir, std::path::PathBuf) {
    let project = tempfile::tempdir().expect("project tempdir");
    let openslate_dir = project.path().join(".openslate");
    std::fs::create_dir(&openslate_dir).expect("create .openslate");
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

[levels]
main = "main"
fast = "fast"

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
    .expect("write local toml");
    let agents_dir = openslate_dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create agents dir");
    std::fs::write(
        agents_dir.join("root.md"),
        "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - read_file\n---\nYou are the root agent.\n",
    )
    .expect("write root.md");

    let global = tempfile::tempdir().expect("global tempdir");
    std::fs::write(
        global.path().join("openslate.toml"),
        r#"
[providers.lib]
base_url = "https://lib.example"
api_key_env = "LIB_KEY"

[models.libmodel]
provider = "lib"
model = "lib-1"
"#,
    )
    .expect("write global toml");

    (project, global, openslate_dir.join("openslate.toml"))
}

/// Plain single-file project (no global library): `build_app_context`
/// with an explicit config → `global_config_path == None` and
/// provider/model writes must fall back to the ACTIVE file.
fn flat_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let project = tempfile::tempdir().expect("project tempdir");
    let openslate_dir = project.path().join(".openslate");
    std::fs::create_dir(&openslate_dir).expect("create .openslate");
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

[levels]
main = "main"
fast = "fast"

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
    (project, openslate_dir.join("openslate.toml"))
}

async fn layered_app() -> (
    App,
    tempfile::TempDir,
    tempfile::TempDir,
    std::path::PathBuf,
) {
    let (project, global, local) = layered_fixture();
    let mut ctx = build_app_context(local.to_str())
        .await
        .expect("build app context");
    // Wire the global library the way default discovery would have
    // (the explicit-config build yields None).
    ctx.global_config_path = Some(global.path().join("openslate.toml"));
    (App::new(ctx), project, global, local)
}

async fn flat_app() -> (App, tempfile::TempDir, std::path::PathBuf) {
    let (project, local) = flat_fixture();
    let ctx = build_app_context(local.to_str())
        .await
        .expect("build app context");
    (App::new(ctx), project, local)
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
/// Tab, (skip api_key_env), Tab, (skip paste), Tab, (skip adapter),
/// Enter. Returns after the submit dispatch.
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
    let (mut app, _project, _global, _local) = layered_app().await;
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
    let (mut app, _project, _global, _local) = layered_app().await;
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

/// Provider add → global library file written → merged config hot
/// swapped (a model entry bound to the new provider is immediately
/// switchable via `/model`).
#[tokio::test]
async fn provider_upsert_writes_global_library_and_hot_swaps() {
    let (mut app, _project, global, _local) = layered_app().await;
    open_models(&mut app).await;
    form_add_provider(&mut app, "acme", "https://api.acme.example", "ACME_API_KEY").await;

    let global_text =
        std::fs::read_to_string(global.path().join("openslate.toml")).expect("read global");
    assert!(
        global_text.contains("[providers.acme]"),
        "global library updated: {global_text}"
    );
    assert!(global_text.contains("https://api.acme.example"));
    assert_eq!(
        app.status_notice(),
        Some("已保存并生效 · provider acme".to_owned()).as_deref(),
        "success notice"
    );

    // Add a model entry bound to the new provider through the SAME
    // open overlay (multi-edit session): Models tab → a. The snapshot
    // was re-synced from the merged config, so the provider choice's
    // head IS the fresh "acme".
    app.dispatch(Action::InputChar('2')).await;
    app.dispatch(Action::InputChar('a')).await;
    type_into(&mut app, "acme-1").await;
    app.dispatch(Action::FocusNext).await; // provider choice, head = acme
    app.dispatch(Action::FocusNext).await; // model id
    type_into(&mut app, "acme-model-1").await;
    app.dispatch(Action::SubmitInput).await;

    let global_text =
        std::fs::read_to_string(global.path().join("openslate.toml")).expect("read global");
    assert!(
        global_text.contains("[models.acme-1]"),
        "model entry written: {global_text}"
    );
    // Hot swap: the new entry resolves right away through /model.
    app.dispatch(Action::DismissOverlay).await;
    type_into(&mut app, "/model acme-1").await;
    app.dispatch(Action::SubmitInput).await;
    assert_eq!(
        app.status_notice(),
        Some("model → acme-1".to_owned()).as_deref(),
        "the reloaded config knows the new entry"
    );
}

/// Pasting an API key stores it in the global dir's `.env` and points
/// the provider's `api_key_env` at the derived variable name.
#[tokio::test]
async fn pasted_api_key_lands_in_the_env_file() {
    let (mut app, _project, global, _local) = layered_app().await;
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

    let env_text = std::fs::read_to_string(global.path().join(".env")).expect("read .env");
    assert!(
        env_text.contains("ZHIPU_API_KEY=sk-live-abc123"),
        ".env content: {env_text}"
    );
    let global_text =
        std::fs::read_to_string(global.path().join("openslate.toml")).expect("read global");
    assert!(
        global_text.contains("api_key_env = \"ZHIPU_API_KEY\""),
        "provider points at the derived var: {global_text}"
    );
}

/// Level rebinding writes the ACTIVE file (local-overlay semantics)
/// and hot swaps.
#[tokio::test]
async fn level_rebind_writes_active_file_and_hot_swaps() {
    let (mut app, _project, _global, local) = layered_app().await;
    open_models(&mut app).await;
    app.dispatch(Action::InputChar('3')).await; // Levels tab
    app.dispatch(Action::InputHistoryNext).await; // row 1 = fast (required)
    app.dispatch(Action::SubmitInput).await; // Enter = rebind
                                             // Picker: entries sorted [fast, main] → move to main.
    app.dispatch(Action::InputHistoryNext).await;
    app.dispatch(Action::SubmitInput).await;

    let local_text = std::fs::read_to_string(&local).expect("read local");
    assert!(
        local_text.contains("fast = \"main\""),
        "levels.fast rebound in the ACTIVE file: {local_text}"
    );
    assert_eq!(
        app.status_notice(),
        Some("已保存并生效 · levels.fast → main".to_owned()).as_deref()
    );
}

/// A provider still referenced by a model entry cannot be deleted; the
/// notice lists the referencing entries and the file stays untouched.
#[tokio::test]
async fn referenced_provider_delete_is_refused() {
    let (mut app, _project, global, _local) = layered_app().await;
    let before =
        std::fs::read_to_string(global.path().join("openslate.toml")).expect("read global");
    open_models(&mut app).await;
    app.dispatch(Action::InputChar('d')).await; // cursor row 0 = lib? no: local-only snapshot → mock
    app.dispatch(Action::InputChar('y')).await;
    let notice = app.status_notice().unwrap_or_default();
    assert!(
        notice.contains("无法删除 provider"),
        "refusal notice: {notice}"
    );
    let after = std::fs::read_to_string(global.path().join("openslate.toml")).expect("read global");
    assert_eq!(before, after, "the global file is byte-identical");
}

/// `main`/`fast` are required levels: the component refuses the delete
/// in-panel (warning flash, no confirmation step); a custom level
/// added through the overlay deletes cleanly (full add → bind →
/// delete round trip).
#[tokio::test]
async fn required_level_refuses_and_custom_level_round_trips() {
    let (mut app, _project, _global, local) = layered_app().await;
    open_models(&mut app).await;
    app.dispatch(Action::InputChar('3')).await; // Levels
                                                // Cursor on row 0 = main (required).
    app.dispatch(Action::InputChar('d')).await;
    let notice = app.status_notice().unwrap_or_default();
    assert!(
        notice.is_empty(),
        "the refusal is the in-panel flash, not a status notice: {notice}"
    );
    // Still in the list (no confirm step was reached): 'y' is inert,
    // and the file is untouched.
    app.dispatch(Action::InputChar('y')).await;
    let local_text = std::fs::read_to_string(&local).expect("read local");
    assert!(
        local_text.contains("main = \"main\""),
        "required level survives: {local_text}"
    );

    // Add a custom level through the overlay: a → name → Enter →
    // picker → Enter.
    app.dispatch(Action::InputChar('a')).await;
    type_into(&mut app, "draft").await;
    app.dispatch(Action::SubmitInput).await; // name → picker
    app.dispatch(Action::SubmitInput).await; // pick first entry (fast)
    let local_text = std::fs::read_to_string(&local).expect("read local");
    assert!(
        local_text.contains("draft = \"fast\""),
        "custom level written: {local_text}"
    );

    // Delete it: it renders after main/fast → cursor to row 2.
    app.dispatch(Action::InputHistoryNext).await;
    app.dispatch(Action::InputHistoryNext).await;
    app.dispatch(Action::InputChar('d')).await;
    app.dispatch(Action::InputChar('y')).await;
    let local_text = std::fs::read_to_string(&local).expect("read local");
    assert!(
        !local_text.contains("draft"),
        "custom level removed: {local_text}"
    );
}

/// No global library: provider/model writes fall back to the ACTIVE
/// file and the notice says so.
#[tokio::test]
async fn no_global_library_falls_back_to_active_file() {
    let (mut app, _project, local) = flat_app().await;
    open_models(&mut app).await;
    form_add_provider(&mut app, "acme", "https://api.acme.example", "ACME_KEY").await;

    let local_text = std::fs::read_to_string(&local).expect("read local");
    assert!(
        local_text.contains("[providers.acme]"),
        "fallback write to the active file: {local_text}"
    );
    let notice = app.status_notice().unwrap_or_default();
    assert!(
        notice.contains("已保存并生效 · provider acme") && notice.contains("无全局库"),
        "fallback notice: {notice}"
    );
}

/// The `.env` fallback follows the ACTIVE config's dir when there is
/// no global library.
#[tokio::test]
async fn env_fallback_writes_beside_the_active_file() {
    let (mut app, project, local) = flat_app().await;
    open_models(&mut app).await;
    app.dispatch(Action::InputChar('a')).await;
    type_into(&mut app, "zhipu").await;
    app.dispatch(Action::FocusNext).await;
    type_into(&mut app, "https://api.zhipu.example").await;
    app.dispatch(Action::FocusNext).await;
    app.dispatch(Action::FocusNext).await;
    app.dispatch(Action::PasteText("sk-fallback-1".into()))
        .await;
    app.dispatch(Action::SubmitInput).await;

    let env_path = project.path().join(".openslate").join(".env");
    let env_text = std::fs::read_to_string(&env_path).expect("read .env");
    assert!(
        env_text.contains("ZHIPU_API_KEY=sk-fallback-1"),
        "fallback .env: {env_text}"
    );
    let _ = local;
}
