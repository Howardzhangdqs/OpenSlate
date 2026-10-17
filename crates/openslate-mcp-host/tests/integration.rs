//! 端到端集成测试：host（HTTP 上游）× 2 个 mock-server（stdio 下游）×
//! builtin server 群（进程内，含 bash）。
//!
//! 覆盖：token 鉴权（401）、tools/list 聚合与 `alias__` 前缀、tools/call
//! 路由与结果/错误透传、未知 server / 未知裸名报错、builtin 开关组合。
//! 全部走真实 TCP + 真实子进程（mock-server 与 bash）。

use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation, JsonObject,
};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::{RoleClient, ServiceExt};
use serde_json::json;

use openslate_mcp_host::{
    build_router, BuiltinConfig, BuiltinHub, DownstreamHub, HostConfig, ServerSpec,
};

fn mock_spec() -> ServerSpec {
    ServerSpec {
        command: env!("CARGO_BIN_EXE_mock-server").to_string(),
        args: vec![],
        env: None,
    }
}

fn hub_with_two_downstreams() -> Arc<DownstreamHub> {
    let mut servers = BTreeMap::new();
    servers.insert("a".to_string(), mock_spec());
    servers.insert("b".to_string(), mock_spec());
    Arc::new(DownstreamHub::new(servers))
}

/// bind 随机端口并后台起 host；返回实际 URL。
async fn spawn_host(
    hub: Arc<DownstreamHub>,
    builtin: Arc<BuiltinHub>,
    token: Option<String>,
) -> String {
    let router = build_router(hub, builtin, token);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}/mcp")
}

/// builtin 全关的 hub（隔离下游断言用；也是 `[builtin] enabled=false` 的
/// 运行时等价物）。
fn builtin_off() -> Arc<BuiltinHub> {
    Arc::new(BuiltinHub::empty())
}

/// 按需开 builtin 的 hub；root 钉在临时目录（fs/edit 沙箱可断言）。
async fn builtin_in(dir: &std::path::Path, cfg: BuiltinConfig) -> Arc<BuiltinHub> {
    let cfg = BuiltinConfig {
        root: Some(dir.to_path_buf()),
        ..cfg
    };
    Arc::new(BuiltinHub::spawn(&cfg).await.unwrap())
}

/// 经 `custom_headers` 注入完整 `Authorization: Bearer …` 值（与 OpenSlate
/// App 侧 `TransportConfig::Http.headers` 的未来用法一致；`Authorization`
/// 不在 rmcp 保留头名单内，原样透传）。
fn authed_transport(url: &str, token: &str) -> StreamableHttpClientTransport<reqwest::Client> {
    let mut config = StreamableHttpClientTransportConfig::default();
    config.uri = url.into();
    config.custom_headers.insert(
        http::HeaderName::from_static("authorization"),
        http::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    StreamableHttpClientTransport::with_client(reqwest::Client::new(), config)
}

async fn connect(url: &str, token: &str) -> RunningService<RoleClient, ClientConfig> {
    let transport = authed_transport(url, token);
    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("mcp-host-test", "0.0.0"),
    );
    client.serve(transport).await.unwrap()
}

fn call_params(name: &str, args: Option<serde_json::Value>) -> CallToolRequestParams {
    let mut p = CallToolRequestParams::new(name.to_owned());
    if let Some(v) = args {
        p = p.with_arguments(v.as_object().cloned().expect("object"));
    }
    p
}

fn obj(v: serde_json::Value) -> JsonObject {
    v.as_object().cloned().expect("object")
}

fn is_error(result: &rmcp::model::CallToolResult) -> bool {
    result.is_error == Some(true)
}

fn first_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn aggregates_and_routes_tools() {
    let url = spawn_host(
        hub_with_two_downstreams(),
        builtin_off(),
        Some("test-token".into()),
    )
    .await;
    let client = connect(&url, "test-token").await;

    // 1. 聚合 list：两个下游的 echo/fail 都在，且带 alias 前缀。
    let tools = client.peer().list_all_tools().await.unwrap();
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "a__echo".to_string(),
            "a__fail".to_string(),
            "b__echo".to_string(),
            "b__fail".to_string(),
        ],
        "aggregated tool list with alias prefixes"
    );

    // 2. 路由 call：a__echo → 下游 a 的 echo。
    let result = client
        .peer()
        .call_tool(call_params("a__echo", Some(json!({ "text": "hi" }))))
        .await
        .unwrap();
    assert!(!is_error(&result));
    assert_eq!(first_text(&result), "[echo] hi");

    // 3. 另一 alias 同名工具（证明路由互不串台）。
    let result = client
        .peer()
        .call_tool(call_params("b__echo", Some(json!({ "text": "yo" }))))
        .await
        .unwrap();
    assert_eq!(first_text(&result), "[echo] yo");

    // 4. 下游工具自身的 is_error 语义透传（不是 JSON-RPC 错误）。
    let result = client
        .peer()
        .call_tool(call_params("a__fail", Some(json!({ "message": "boom" }))))
        .await
        .unwrap();
    assert!(is_error(&result));
    assert_eq!(first_text(&result), "[fail] boom");

    // 5. 未知 server：可读错误（is_error），不是连接层断言。
    let result = client
        .peer()
        .call_tool(call_params("nope__echo", None))
        .await
        .unwrap();
    assert!(is_error(&result));
    assert!(first_text(&result).contains("unknown server 'nope'"));

    // 6. 未知裸名：builtin 路由的"unknown builtin tool"文案（旧文案
    //    "missing the server prefix" 已随裸名路由失效——builtin 关闭时
    //    同样落到这里，文案明示 disabled）。
    let result = client
        .peer()
        .call_tool(call_params("echo", None))
        .await
        .unwrap();
    assert!(is_error(&result));
    let text = first_text(&result);
    assert!(text.contains("unknown builtin tool 'echo'"), "{text}");
    assert!(text.contains("builtin tools are disabled"), "{text}");

    let _ = obj(json!({}));
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let url = spawn_host(
        hub_with_two_downstreams(),
        builtin_off(),
        Some("right-token".into()),
    )
    .await;
    // 错 token：握手应失败（401）。
    let transport = authed_transport(&url, "wrong-token");
    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("mcp-host-test", "0.0.0"),
    );
    let err = client.serve(transport).await;
    assert!(err.is_err(), "handshake must fail with wrong token, got ok");
}

#[test]
fn host_config_parses_manifest() {
    let raw = r#"
bind = "127.0.0.1:9999"
token = "abc"

[servers.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/sdcard"]

[servers.custom]
command = "/path/to/server"
[servers.custom.env]
FOO = "bar"
"#;
    let cfg: HostConfig = toml::from_str(raw).unwrap();
    assert_eq!(cfg.bind.as_deref(), Some("127.0.0.1:9999"));
    assert_eq!(cfg.token.as_deref(), Some("abc"));
    assert_eq!(cfg.servers.len(), 2);
    let fs_spec = &cfg.servers["fs"];
    assert_eq!(fs_spec.command, "npx");
    assert_eq!(fs_spec.args.len(), 3);
    let custom = &cfg.servers["custom"];
    assert_eq!(custom.env.as_ref().unwrap()["FOO"], "bar");
    // 旧清单（无 [builtin] 段）→ 全默认：enabled+bash 开、root 缺省。
    assert!(cfg.builtin.enabled);
    assert!(cfg.builtin.bash);
    assert!(cfg.builtin.root.is_none());
}

// ── builtin 段 ──────────────────────────────────────────────────────────────

#[test]
fn host_config_builtin_section_parses() {
    let raw = r#"
[builtin]
enabled = false
"#;
    let cfg: HostConfig = toml::from_str(raw).unwrap();
    assert!(!cfg.builtin.enabled);
    // 段内缺省字段沿用 Default（bash 开、root None）。
    assert!(cfg.builtin.bash);
    assert!(cfg.builtin.root.is_none());

    let raw = r#"
[builtin]
bash = false
root = "/tmp/ws"
"#;
    let cfg: HostConfig = toml::from_str(raw).unwrap();
    assert!(cfg.builtin.enabled);
    assert!(!cfg.builtin.bash);
    assert_eq!(cfg.builtin.root.as_deref(), Some(std::path::Path::new("/tmp/ws")));
}

#[test]
fn host_config_builtin_section_rejects_unknown_fields() {
    // deny_unknown_fields 在 [builtin] 段内同样生效（拼错键当场报错）。
    let raw = r#"
[builtin]
oops = true
"#;
    assert!(toml::from_str::<HostConfig>(raw).is_err());
}

/// 裸名工具（builtin）与 `alias__tool`（下游）在同一清单共存；`bash`
/// 经完整 MCP over HTTP 链路执行成功。
#[tokio::test]
async fn builtin_bare_tools_listed_and_bash_runs() {
    let dir = tempfile::TempDir::new().unwrap();
    let builtin = builtin_in(dir.path(), BuiltinConfig::default()).await;
    let url = spawn_host(
        hub_with_two_downstreams(),
        builtin,
        Some("test-token".into()),
    )
    .await;
    let client = connect(&url, "test-token").await;

    // 裸名 + alias 前缀工具共存于同一 tools/list。
    let tools = client.peer().list_all_tools().await.unwrap();
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "a__echo".to_string(),
            "a__fail".to_string(),
            "b__echo".to_string(),
            "b__fail".to_string(),
            "bash".to_string(),
            "edit_file".to_string(),
            "read_file".to_string(),
            "write_file".to_string(),
        ],
        "bare builtin tools coexist with aliased downstream tools"
    );

    // bash 走完整链路（HTTP → aggregate → in-process BashServer）。
    let result = client
        .peer()
        .call_tool(call_params("bash", Some(json!({ "command": "echo hello-builtin" }))))
        .await
        .unwrap();
    assert!(!is_error(&result), "{}", first_text(&result));
    let text = first_text(&result);
    assert!(text.contains("exit_code: 0"), "{text}");
    assert!(text.contains("hello-builtin"), "{text}");

    // read_file 的沙箱 root = [builtin] root。
    std::fs::write(dir.path().join("note.txt"), b"hi-from-file").unwrap();
    let result = client
        .peer()
        .call_tool(call_params("read_file", Some(json!({ "path": "note.txt" }))))
        .await
        .unwrap();
    assert!(!is_error(&result), "{}", first_text(&result));
    assert!(first_text(&result).contains("hi-from-file"));

    // 未知裸名：新错误文案（带已知清单，不提"缺 server 前缀"）。
    let result = client
        .peer()
        .call_tool(call_params("no_such_tool", None))
        .await
        .unwrap();
    assert!(is_error(&result));
    let text = first_text(&result);
    assert!(text.contains("unknown builtin tool 'no_such_tool'"), "{text}");
    assert!(text.contains("bash"), "{text}");
    assert!(!text.contains("missing the server prefix"), "{text}");
}

/// `[builtin] enabled = false` → 零裸名工具，清单只剩下游。
#[tokio::test]
async fn builtin_disabled_lists_no_bare_tools() {
    let dir = tempfile::TempDir::new().unwrap();
    let builtin = builtin_in(dir.path(), BuiltinConfig { enabled: false, ..Default::default() }).await;
    let url = spawn_host(
        hub_with_two_downstreams(),
        builtin,
        Some("test-token".into()),
    )
    .await;
    let client = connect(&url, "test-token").await;

    let tools = client.peer().list_all_tools().await.unwrap();
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    // sort：DownstreamHub 内部是 HashMap，alias 顺序非确定。
    names.sort();
    assert_eq!(
        names,
        vec![
            "a__echo".to_string(),
            "a__fail".to_string(),
            "b__echo".to_string(),
            "b__fail".to_string(),
        ],
        "no bare tools when [builtin] disabled"
    );

    // 调 bash → unknown builtin tool（且文案明示 builtin 关闭）。
    let result = client
        .peer()
        .call_tool(call_params("bash", Some(json!({ "command": "true" }))))
        .await
        .unwrap();
    assert!(is_error(&result));
    let text = first_text(&result);
    assert!(text.contains("unknown builtin tool 'bash'"), "{text}");
    assert!(text.contains("builtin tools are disabled"), "{text}");
}

/// `bash = false` 而 fs/edit 在 → 清单只缺 `bash`。
#[tokio::test]
async fn builtin_bash_off_keeps_fs_tools() {
    let dir = tempfile::TempDir::new().unwrap();
    let builtin = builtin_in(dir.path(), BuiltinConfig { bash: false, ..Default::default() }).await;
    let url = spawn_host(hub_with_two_downstreams(), builtin, Some("test-token".into())).await;
    let client = connect(&url, "test-token").await;

    let tools = client.peer().list_all_tools().await.unwrap();
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    assert!(!names.contains(&"bash".to_string()), "{names:?}");
    for expected in ["read_file", "write_file", "edit_file"] {
        assert!(names.contains(&expected.to_string()), "{names:?}");
    }

    // bash 调用 → unknown builtin tool（不是 spawned，无此工具）。
    let result = client
        .peer()
        .call_tool(call_params("bash", Some(json!({ "command": "true" }))))
        .await
        .unwrap();
    assert!(is_error(&result));
    assert!(first_text(&result).contains("unknown builtin tool 'bash'"));
}
