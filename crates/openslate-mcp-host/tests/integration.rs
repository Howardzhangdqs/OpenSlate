//! 端到端集成测试：host（HTTP 上游）× 2 个 mock-server（stdio 下游）。
//!
//! 覆盖：token 鉴权（401）、tools/list 聚合与 `alias__` 前缀、tools/call
//! 路由与结果/错误透传、未知 server 报错。全部走真实 TCP + 真实子进程。

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

use openslate_mcp_host::{build_router, DownstreamHub, HostConfig, ServerSpec};

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
async fn spawn_host(hub: Arc<DownstreamHub>, token: Option<String>) -> String {
    let router = build_router(hub, token);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}/mcp")
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
    let url = spawn_host(hub_with_two_downstreams(), Some("test-token".into())).await;
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

    // 6. 无前缀名：提示缺少 server 前缀。
    let result = client
        .peer()
        .call_tool(call_params("echo", None))
        .await
        .unwrap();
    assert!(is_error(&result));
    assert!(first_text(&result).contains("missing the server prefix"));

    let _ = obj(json!({}));
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let url = spawn_host(hub_with_two_downstreams(), Some("right-token".into())).await;
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
}
