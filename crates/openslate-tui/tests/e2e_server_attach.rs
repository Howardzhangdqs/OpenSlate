//! End-to-end attach test (web-1) — the REAL server in-process +
//! the REAL client transport.
//!
//! `#[ignore]`d by default (spawns a TCP listener + full engine): run
//! with `cargo test -p openslate-tui --test e2e_server_attach -- --ignored`.
//! Proves the whole client half of the wire contract against A's
//! server: `connect()` initial ladder → hello → first snapshot → the
//! App's dispatch pipeline (submit → broadcasts → transcript merge),
//! plus `/model` switching through `ModelChanged`.
//!
//! auto-attach-1: a second test drives the FULL discovery path —
//! `resolve_server(None, …)` picks the server.json the server wrote and
//! `connect()` attaches through it (no `--server` value anywhere).
//!
//! Both tests run the server on a TEMP project (port 0): the serve
//! family now writes `<config dir>/server.json`, which must never leak
//! into the repo's fixtures.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use openslate_core::error::ProviderError;
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::types::{ModelResponse, Usage};
use openslate_server::{serve_with_shutdown, ServeOptions};
use openslate_tui::action::Action;
use openslate_tui::app::{App, ClientBootstrap};
use openslate_tui::client;
use openslate_tui::components::transcript::TranscriptEntry;
use openslate_tui::components::RunState;

/// A provider that answers one fixed assistant message (the default
/// `generate_stream` wraps it into a single Delta + Done sequence).
struct FixedProvider;

#[async_trait]
impl ModelProvider for FixedProvider {
    async fn generate(&self, request: GenerateRequest) -> Result<ModelResponse, ProviderError> {
        let n = request.messages.len();
        Ok(ModelResponse {
            content: Some(format!("echo:{n}")),
            tool_calls: vec![],
            usage: Some(Usage {
                input_tokens: 5,
                output_tokens: 2,
                cached_input_tokens: None,
            }),
            finish_reason: Some("stop".into()),
        })
    }

    fn provider_name(&self) -> &str {
        "fixed-e2e"
    }
}

/// Temp project + a running serve (port 0 → actual port lands in
/// server.json). Returns the config path, the server.json path and a
/// shutdown trigger.
struct RunningServer {
    config: String,
    info_path: std::path::PathBuf,
    shutdown: tokio::sync::oneshot::Sender<()>,
    _tmp: tempfile::TempDir,
}

async fn spawn_server() -> RunningServer {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dir = tmp.path().join(".openslate");
    std::fs::create_dir_all(&dir).expect("mkdir .openslate");
    let db = tmp.path().join("e2e.db");
    let toml = format!(
        r#"
[providers.mock]
base_url = "http://127.0.0.1:1/v1"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "mock-main"

[models.fast]
provider = "mock"
model = "mock-fast"

[database]
path = "{}"

[limits]
timeout_ms = 30000
"#,
        db.display()
    );
    std::fs::write(dir.join("openslate.toml"), toml).expect("write toml");
    std::fs::create_dir_all(dir.join("agents")).expect("mkdir agents");
    std::fs::write(
        dir.join("agents").join("root.md"),
        "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - read_file\n---\nYou are root.\n",
    )
    .expect("write root.md");

    let config = dir.join("openslate.toml").to_string_lossy().into_owned();
    let opts = ServeOptions {
        port: 0,
        config_flag: Some(config.clone()),
        provider_factory: Some(Arc::new(|_cfg, _alias| {
            Ok(Box::new(FixedProvider) as Box<dyn ModelProvider>)
        })),
        ..Default::default()
    };
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        // 停机信号触发后 serve 返回；错误仅打印（server.json 生命周期
        // 断言在 server crate 的集成测试里）。
        if let Err(e) = serve_with_shutdown(opts, async move {
            let _ = rx.await;
        })
        .await
        {
            eprintln!("E2E SERVE ERR: {e:#}");
        }
    });
    let info_path = dir.join("server.json");
    // 等 server.json 落盘（= 监听就绪，实际端口在里面）。
    for _ in 0..40 {
        if info_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(info_path.exists(), "serve 应已写 server.json");
    RunningServer {
        config,
        info_path,
        shutdown: tx,
        _tmp: tmp,
    }
}

#[tokio::test]
#[ignore = "spawns a real TCP+WS server in-process; run explicitly"]
async fn attach_hello_snapshot_and_full_turn_end_to_end() {
    // 1. The server (scripted provider — no network beyond loopback).
    let srv = spawn_server().await;
    // url 取自 server.json（port 0 → 实际分配端口）。
    let info: openslate_protocol::ServerInfo =
        serde_json::from_str(&std::fs::read_to_string(&srv.info_path).unwrap()).unwrap();
    let url = info.url;

    // 2. The client transport: connect() completes only after the first
    // snapshot passed through (initial-ladder contract).
    let conn = client::connect(&url, None)
        .await
        .expect("initial connect + hello + first snapshot");

    // 3. The App over that connection.
    let seed =
        openslate_core::config::parse_openslate_toml("").expect("empty TOML parses to defaults");
    let mut app = App::new(ClientBootstrap {
        link: Arc::new(conn.link),
        events: conn.events,
        config: seed,
        root_agent_id: String::new(),
    });

    // 4. Drain the queued first snapshot: session hydrated.
    app.drain_engine_events().await;
    assert!(
        app.session_run_id().is_some(),
        "snapshot hydrated the session"
    );
    assert!(
        app.transcript_entries().is_empty(),
        "fresh session snapshot carries an empty transcript"
    );

    // 5. Submit a turn through the REAL dispatch; pump until TurnDone.
    app.dispatch(Action::StartTurn("hello e2e".into())).await;
    let mut guard = 0;
    while app.is_running() && guard < 300 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        app.drain_engine_events().await;
        guard += 1;
    }
    app.drain_engine_events().await;
    assert!(!app.is_running(), "the turn must finish (guard {guard})");
    assert_eq!(*app.run_state(), RunState::Idle, "turn completed");
    let entries = app.transcript_entries();
    assert!(
        entries
            .iter()
            .any(|e| matches!(e, TranscriptEntry::User(t) if t == "hello e2e")),
        "the submitted user turn is in the transcript"
    );
    assert!(
        entries
            .iter()
            .any(|e| matches!(e, TranscriptEntry::Assistant(t) if t.starts_with("echo:"))),
        "the scripted answer streamed and merged"
    );

    // 6. /model through the server: SetModel leaves, ModelChanged
    // confirms (the fixture has a `fast` alias).
    app.dispatch(Action::StartTurn("/model fast".into())).await;
    let mut guard = 0;
    while app.status_notice() != Some("model → fast") && guard < 100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        app.drain_engine_events().await;
        guard += 1;
    }
    assert_eq!(
        app.status_notice(),
        Some("model → fast"),
        "ModelChanged landed"
    );

    let _ = srv.shutdown.send(());
}

#[tokio::test]
#[ignore = "spawns a real TCP+WS server in-process; run explicitly"]
async fn auto_attach_via_server_info_without_server_flag() {
    // 1. 起 server（temp 工程，port 0）。
    let srv = spawn_server().await;

    // 2. 无 --server / env（explicit=None）→ 经 --config 指向的目录
    //    发现 server.json，url/token 全部来自文件。
    let spec = openslate_tui::discovery::resolve_server(None, None, Some(&srv.config))
        .expect("server.json 应被发现");
    let file_url = {
        let info: openslate_protocol::ServerInfo =
            serde_json::from_str(&std::fs::read_to_string(&srv.info_path).unwrap()).unwrap();
        info.url
    };
    assert_eq!(spec.url, file_url, "发现链取的是 server.json 的 url");
    assert_eq!(
        spec.discovered_from.as_deref(),
        Some(srv.info_path.as_path())
    );
    assert_eq!(spec.token, None, "无 --auth-token 时文件 token 为空");

    // 3. 发现出的 url 直连：hello → 首个 snapshot 到达。
    let conn = client::connect(&spec.url, spec.token.clone())
        .await
        .expect("auto-attach connect + hello + first snapshot");

    // 4. App 驱动一回合，证明链路完整可用。
    let seed =
        openslate_core::config::parse_openslate_toml("").expect("empty TOML parses to defaults");
    let mut app = App::new(ClientBootstrap {
        link: Arc::new(conn.link),
        events: conn.events,
        config: seed,
        root_agent_id: String::new(),
    });
    app.drain_engine_events().await;
    assert!(app.session_run_id().is_some(), "snapshot hydrated");
    app.dispatch(Action::StartTurn("auto attach".into())).await;
    let mut guard = 0;
    while app.is_running() && guard < 300 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        app.drain_engine_events().await;
        guard += 1;
    }
    app.drain_engine_events().await;
    assert!(!app.is_running(), "the turn must finish (guard {guard})");
    assert!(
        app.transcript_entries()
            .iter()
            .any(|e| matches!(e, TranscriptEntry::Assistant(t) if t.starts_with("echo:"))),
        "answer merged over the discovered link"
    );

    // 5. 停机 → server.json 删除（发现文件生命周期闭环）。
    let _ = srv.shutdown.send(());
    for _ in 0..40 {
        if !srv.info_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(!srv.info_path.exists(), "停机后 server.json 应删除");
}
