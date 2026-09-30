//! server.json 生命周期集成测试（auto-attach-1）：起**真 serve**
//! （注入停机信号），断言——
//!
//! - 监听成功后 server.json 落盘（proto/url/pid/port/bind/token/cwd
//!   字段、0600 权限）；
//! - 防双：同 config 目录再起第二个 serve 应失败退出（真 health 端点
//!   探测，文案含 pid/url）；
//! - 停机后 server.json 删除；
//! - 启动块关键行（listening / server info）经 writer 注入在 lib 单测
//!   钉死，这里对真 serve 的装配路径不再重复。

use std::sync::Arc;
use std::time::Duration;

use openslate_core::error::ProviderError;
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::types::{ModelResponse, Usage};
use openslate_protocol::{ServerInfo, PROTOCOL_VERSION};
use openslate_server::{serve_with_shutdown, ServeOptions};

/// 固定回包 provider（不产生出站网络）。
struct FixedProvider;

#[async_trait::async_trait]
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
        "fixed-server-info"
    }
}

/// 临时工程（mock provider + 临时 DB + root agent），返回 config 路径。
fn temp_project(tag: &str) -> (tempfile::TempDir, String) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dir = tmp.path().join(".openslate");
    std::fs::create_dir_all(&dir).expect("mkdir .openslate");
    let db = tmp.path().join(format!("test-{tag}.db"));
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
    (tmp, config)
}

fn opts_for(config: &str) -> ServeOptions {
    ServeOptions {
        config_flag: Some(config.to_owned()),
        // port 0 = 随机分配（本机可能有真实 serve 常驻 7800，避开；
        // 实际端口经 server.json 断言）。
        port: 0,
        provider_factory: Some(Arc::new(|_cfg, _alias| {
            Ok(Box::new(FixedProvider) as Box<dyn ModelProvider>)
        })),
        ..Default::default()
    }
}

/// 轮询等条件成立（500ms 间隔，10s 上限）。
async fn wait_until<F: Fn() -> bool>(cond: F) -> bool {
    for _ in 0..20 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

#[tokio::test]
async fn serve_writes_guards_and_removes_server_info() {
    let (tmp, config) = temp_project("main");
    let info_path = tmp.path().join(".openslate").join("server.json");

    // 1. 起 serve（port 0 → 实际端口写进 server.json）。
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let opts = opts_for(&config);
    let serve_task = tokio::spawn(async move {
        let r = serve_with_shutdown(opts, async move {
            let _ = shutdown_rx.await;
        })
        .await;
        if let Err(e) = &r {
            eprintln!("SERVE ERR: {e:#}");
        }
        r
    });
    assert!(
        wait_until(|| info_path.exists()).await,
        "server.json 应在监听成功后落盘"
    );

    // 2. 字段断言。
    let info: ServerInfo = serde_json::from_str(&std::fs::read_to_string(&info_path).unwrap())
        .expect("server.json parses");
    assert_eq!(info.proto, PROTOCOL_VERSION);
    assert_eq!(info.pid, std::process::id());
    assert_eq!(info.bind, "127.0.0.1");
    assert!(
        info.url.starts_with("ws://127.0.0.1:"),
        "url = {}",
        info.url
    );
    assert_eq!(
        info.url,
        format!("ws://127.0.0.1:{}/api/ws", info.port),
        "url 是完整 WS 路径"
    );
    assert!(info.port > 0, "port 0 必须替换为实际分配端口");
    assert!(!info.started_at.is_empty(), "started_at 必填");
    assert_eq!(info.token, None, "无 --auth-token 时为 null");
    assert!(!info.cwd.is_empty());

    // 0600（token 在里面）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&info_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // 3. 真 health 端点可达（防双判据的探测对象）。
    let health = format!("http://127.0.0.1:{}/api/health", info.port);
    let mut ok = false;
    for _ in 0..20 {
        if let Ok(resp) = reqwest::get(&health).await {
            if resp.status().is_success() {
                ok = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "health 端点应可达: {health}");

    // 4. 防双：同 config 目录起第二个 serve → Err（文案含 pid/url）。
    let opts2 = opts_for(&config);
    let second = serve_with_shutdown(opts2, std::future::pending::<()>()).await;
    let err = second.expect_err("第二个 serve 必须被防双守卫拒绝");
    let msg = format!("{err:#}");
    assert!(msg.contains(&info.pid.to_string()), "文案含 pid: {msg}");
    assert!(msg.contains(&info.url), "文案含 url: {msg}");

    // 5. 停机 → server.json 删除 + serve 正常返回。
    let _ = shutdown_tx.send(());
    let result = tokio::time::timeout(Duration::from_secs(15), serve_task)
        .await
        .expect("serve 应在停机信号后退出")
        .expect("join ok");
    assert!(result.is_ok(), "优雅停机应 Ok: {:?}", result.err());
    assert!(!info_path.exists(), "停机后 server.json 应被删除");
}

#[tokio::test]
async fn serve_overwrites_stale_server_info() {
    // 陈旧文件（死端口）不阻断启动，且被覆盖为新内容。
    let (tmp, config) = temp_project("stale");
    let dir = tmp.path().join(".openslate");
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);
    let stale = ServerInfo {
        proto: PROTOCOL_VERSION,
        url: format!("ws://127.0.0.1:{dead_port}/api/ws"),
        pid: 999_999,
        port: dead_port,
        bind: "127.0.0.1".into(),
        started_at: "2000-01-01T00:00:00+08:00".into(),
        token: None,
        cwd: String::new(),
    };
    std::fs::write(
        dir.join("server.json"),
        serde_json::to_string(&stale).unwrap(),
    )
    .unwrap();

    let opts = opts_for(&config);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let serve_task = tokio::spawn(serve_with_shutdown(opts, async move {
        let _ = shutdown_rx.await;
    }));
    let info_path = dir.join("server.json");
    assert!(
        wait_until(|| {
            std::fs::read_to_string(&info_path)
                .ok()
                .and_then(|c| serde_json::from_str::<ServerInfo>(&c).ok())
                .is_some_and(|i| i.pid == std::process::id())
        })
        .await,
        "陈旧文件应被覆盖为本进程的 server.json"
    );
    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(15), serve_task).await;
}
