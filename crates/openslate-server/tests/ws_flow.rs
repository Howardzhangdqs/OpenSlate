//! server 集成测试（spec §6）：真 HTTP listener + 真 WS 连接，
//! ScriptedProvider 跑全回合（无网络出站）。
//!
//! 覆盖：hello→snapshot→submit→流式→tool→审批→turn_ok 全链路；
//! 双连接广播一致性；审批首答赢/迟到 notice；CRUD 热替换+config_changed；
//! auth token 拒绝（WS+REST）；REST 只读端点。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::types::{ModelResponse, ModelStreamEvent, ToolCall, ToolCallId, Usage};
use openslate_protocol::{ApprovalAnswerChoice, ClientMsg, ServerMsg};
use openslate_server::{build_router, build_state, ServeOptions};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

// ---------------------------------------------------------------------------
// ScriptedProvider：按请求次序回放脚本；脚本耗尽 → 500（让缺脚本的
// 测试显式失败，而不是挂死）。`hold_open`：回放完后不关通道（cancel
// 用例需要一个"卡住"的流）。
// ---------------------------------------------------------------------------

type SharedScripts = Arc<Mutex<VecDeque<Vec<ModelStreamEvent>>>>;

#[derive(Clone)]
struct ScriptedProvider {
    scripts: SharedScripts,
    hold_open: bool,
}

impl ScriptedProvider {
    fn new(scripts: Vec<Vec<ModelStreamEvent>>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts.into())),
            hold_open: false,
        }
    }

    fn hold_open(mut self) -> Self {
        self.hold_open = true;
        self
    }
}

#[async_trait::async_trait]
impl ModelProvider for ScriptedProvider {
    async fn generate(
        &self,
        _request: GenerateRequest,
    ) -> Result<ModelResponse, openslate_core::error::ProviderError> {
        Err(openslate_core::error::ProviderError::ServerError(500))
    }

    async fn generate_stream(
        &self,
        _request: GenerateRequest,
    ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, openslate_core::error::ProviderError>>
    {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
        let hold = self.hold_open;
        tokio::spawn(async move {
            for event in script {
                let _ = tx.send(Ok(event)).await;
            }
            if hold {
                // 保持通道打开（rx 消费方取消时任务随 runtime 释放）。
                std::future::pending::<()>().await;
            }
        });
        rx
    }

    fn provider_name(&self) -> &str {
        "scripted"
    }
}

fn usage(in_tok: u32, out_tok: u32) -> Usage {
    Usage {
        input_tokens: in_tok,
        output_tokens: out_tok,
        cached_input_tokens: None,
    }
}

fn resp_tool(call_id: &str, name: &str, args: serde_json::Value) -> ModelResponse {
    ModelResponse {
        content: None,
        tool_calls: vec![ToolCall {
            id: ToolCallId(call_id.into()),
            name: name.into(),
            arguments: args,
        }],
        usage: Some(usage(60, 12)),
        finish_reason: Some("tool_calls".into()),
    }
}

fn resp_text(text: &str) -> ModelResponse {
    ModelResponse {
        content: Some(text.into()),
        tool_calls: vec![],
        usage: Some(usage(40, 8)),
        finish_reason: Some("stop".into()),
    }
}

// ---------------------------------------------------------------------------
// 测试环境
// ---------------------------------------------------------------------------

struct TestServer {
    addr: std::net::SocketAddr,
    state: Arc<openslate_server::state::AppState>,
    _tmp: tempfile::TempDir,
}

/// 建临时工程（mock provider 配置 + manual 审批 + root agent + 临时 DB），
/// 起真 listener。返回地址与 state（供断言镜像状态）。
async fn spawn_server(auth: Option<String>, scripts: Vec<Vec<ModelStreamEvent>>) -> TestServer {
    spawn_server_impl(auth, ScriptedProvider::new(scripts)).await
}

async fn spawn_server_impl(auth: Option<String>, provider: ScriptedProvider) -> TestServer {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let openslate_dir = tmp.path().join(".openslate");
    std::fs::create_dir(&openslate_dir).expect("mkdir .openslate");
    let db_path = tmp.path().join("test.db");
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

[approval]
policy = "manual"
"#,
        db_path.display(),
    );
    std::fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
    std::fs::create_dir(openslate_dir.join("agents")).expect("mkdir agents");
    std::fs::write(
        openslate_dir.join("agents").join("root.md"),
        "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - read_file\n  - list_dir\n---\nYou are the root agent.\n",
    )
    .expect("write root.md");

    let opts = ServeOptions {
        auth_token: auth,
        config_flag: Some(
            openslate_dir
                .join("openslate.toml")
                .to_string_lossy()
                .into_owned(),
        ),
        provider_factory: Some(Arc::new(move |_cfg, _alias| {
            Ok(Box::new(provider.clone()) as Box<dyn ModelProvider>)
        })),
        ..ServeOptions::default()
    };
    let state = build_state(&opts).await.expect("build state");
    let app = build_router(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    TestServer {
        addr,
        state,
        _tmp: tmp,
    }
}

type ClientWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(addr: &std::net::SocketAddr) -> ClientWs {
    let (ws, _) = connect_async(format!("ws://{addr}/api/ws"))
        .await
        .expect("ws connect");
    ws
}

async fn send_msg(ws: &mut ClientWs, msg: &ClientMsg) {
    ws.send(WsMessage::Text(
        serde_json::to_string(msg).expect("encode").into(),
    ))
    .await
    .expect("ws send");
}

async fn next_msg(ws: &mut ClientWs) -> ServerMsg {
    let deadline = Duration::from_secs(15);
    loop {
        let frame = tokio::time::timeout(deadline, ws.next())
            .await
            .expect("ws recv timeout")
            .expect("ws stream ended");
        match frame.expect("ws frame") {
            WsMessage::Text(text) => {
                return serde_json::from_str(&text).expect("decode ServerMsg");
            }
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            WsMessage::Close(_) => panic!("closed while waiting for message"),
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

/// hello 握手并断言 snapshot 到达。
async fn hello(ws: &mut ClientWs, token: Option<String>) -> ServerMsg {
    send_msg(ws, &ClientMsg::Hello { proto: 1, token }).await;
    next_msg(ws).await
}

/// 收集事件直到谓词命中（含命中那条），返回全部。
async fn collect_until(ws: &mut ClientWs, stop: impl Fn(&ServerMsg) -> bool) -> Vec<ServerMsg> {
    let mut out = Vec::new();
    loop {
        let msg = next_msg(ws).await;
        let hit = stop(&msg);
        out.push(msg);
        if hit {
            return out;
        }
    }
}

fn type_of(msg: &ServerMsg) -> &'static str {
    serde_json::to_value(msg)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(|t| t.as_str())
                .map(|s| Box::leak(s.to_string().into_boxed_str()) as &str)
        })
        .unwrap_or("?")
}

/// 一轮带工具+审批的完整脚本：流式 → read_file 工具（manual 审批）→ 终答。
fn full_turn_scripts() -> Vec<Vec<ModelStreamEvent>> {
    vec![
        vec![
            ModelStreamEvent::Reasoning("先想想".into()),
            ModelStreamEvent::Delta("我来看一下".into()),
            ModelStreamEvent::Usage(usage(60, 12)),
            ModelStreamEvent::Done(resp_tool(
                "call-1",
                "read_file",
                serde_json::json!({ "path": "Cargo.toml" }),
            )),
        ],
        vec![
            ModelStreamEvent::Delta("看完了".into()),
            ModelStreamEvent::Usage(usage(40, 8)),
            ModelStreamEvent::Done(resp_text("最终回答")),
        ],
    ]
}

fn simple_answer_scripts() -> Vec<Vec<ModelStreamEvent>> {
    vec![vec![
        ModelStreamEvent::Delta("你好".into()),
        ModelStreamEvent::Usage(usage(30, 5)),
        ModelStreamEvent::Done(resp_text("回答完毕")),
    ]]
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_turn_with_streaming_tool_and_approval() {
    let server = spawn_server(None, full_turn_scripts()).await;
    let mut ws = connect(&server.addr).await;

    // hello → snapshot（会话空态自足校验）
    match hello(&mut ws, None).await {
        ServerMsg::Snapshot { session } => {
            assert_eq!(session.proto, 1);
            assert!(!session.running);
            assert!(session.transcript.is_empty());
            assert_eq!(session.model_alias, "main");
            assert_eq!(session.session_stats.turns, 0);
            assert!(session.config.models.contains_key("main"));
            assert_eq!(session.config.agents.id, "root");
            assert!(session.config.limits.max_tool_calls > 0);
            assert!(session.config.skills.is_empty());
        }
        other => panic!("expected snapshot, got {:?}", type_of(&other)),
    }

    send_msg(
        &mut ws,
        &ClientMsg::Submit {
            text: "读一下 Cargo.toml".into(),
        },
    )
    .await;

    // 收到审批请求后应答（引擎阻塞在 decide() 等人）。
    let head = collect_until(&mut ws, |m| {
        matches!(m, ServerMsg::ApprovalRequested { .. })
    })
    .await;
    let approval_id = match head.last() {
        Some(ServerMsg::ApprovalRequested { id, .. }) => *id,
        other => panic!("expected approval_requested, got {other:?}"),
    };
    send_msg(
        &mut ws,
        &ClientMsg::ApprovalAnswer {
            id: approval_id,
            choice: ApprovalAnswerChoice::Approve,
        },
    )
    .await;

    // 应答后的尾流（resolved → tool_end → 第二请求 → turn_ok）。
    let events = collect_until(&mut ws, |m| matches!(m, ServerMsg::TurnOk { .. })).await;
    let mut all = head;
    all.extend(events);
    let events = all;
    let kinds: Vec<&str> = events.iter().map(type_of).collect();

    // 关键子序列（审批在 tool_start 之后、tool_end 之前——runtime 的
    // on_tool_start 先于 execute 内的 decide）。
    fn idx_of(kinds: &[&str], k: &str) -> usize {
        kinds
            .iter()
            .position(|t| *t == k)
            .unwrap_or_else(|| panic!("missing {k} in {kinds:?}"))
    }
    let request_start = idx_of(&kinds, "request_start");
    let reasoning = idx_of(&kinds, "reasoning");
    let delta = idx_of(&kinds, "delta");
    let tool_start = idx_of(&kinds, "tool_start");
    let requested = idx_of(&kinds, "approval_requested");
    assert!(request_start < reasoning);
    assert!(reasoning < delta);
    assert!(delta < tool_start);
    assert!(tool_start < requested);
    let tool_end = idx_of(&kinds, "tool_end");
    let turn_ok = kinds.len() - 1;
    assert!(tool_end < turn_ok);
    assert!(kinds.contains(&"first_token"));
    assert!(kinds.contains(&"input_estimate"));
    assert!(kinds.contains(&"usage"));
    assert!(kinds.contains(&"request_end"));

    // 审批事件内容断言。
    for msg in &events {
        if let ServerMsg::ApprovalRequested { summary, .. } = msg {
            assert_eq!(summary.tool_name, "read_file");
            assert_eq!(summary.risk_level, "low");
            assert!(summary.arguments.contains("Cargo.toml"));
        }
    }

    // resolved / tool_end / turn_ok 细节断言。
    let mut seen_resolved = false;
    for msg in &events {
        match msg {
            ServerMsg::ApprovalResolved { id, choice } => {
                assert_eq!(*id, approval_id);
                assert_eq!(choice, "approve");
                seen_resolved = true;
            }
            ServerMsg::ToolEnd { name, bytes, .. } => {
                assert_eq!(name, "read_file");
                assert!(*bytes > 0, "read_file 输出应有内容");
            }
            _ => {}
        }
    }
    assert!(seen_resolved, "approval_resolved 必达");
    match events.last() {
        Some(ServerMsg::TurnOk { summary }) => {
            assert_eq!(summary.status, openslate_core::types::RunStatus::Completed);
            assert_eq!(summary.total_steps, 2);
            assert!(summary.total_input_tokens >= 100);
            assert_eq!(
                summary.messages.first().map(|m| m.content.as_str()),
                Some("读一下 Cargo.toml")
            );
            assert!(summary
                .messages
                .iter()
                .any(|m| m.role == openslate_core::types::MessageRole::Tool));
        }
        other => panic!("expected turn_ok, got {other:?}"),
    }

    // 镜像终态：transcript 有折叠后的 ToolCall（Done + output 回填）、
    // session_stats 累计。
    let inner = server.state.core.lock();
    assert_eq!(inner.stats.turns, 1);
    assert!(inner.stats.total_input_tokens >= 100);
    assert!(inner.history.len() >= 4);
    let has_filled_tool = inner.transcript.iter().any(|e| {
        matches!(
            e,
            openslate_protocol::EntryDto::ToolCall { status, detail, .. }
                if matches!(status, openslate_protocol::ToolStatusDto::Done { .. })
                    && detail.output.as_deref().map(|o| !o.is_empty()).unwrap_or(false)
        )
    });
    assert!(
        has_filled_tool,
        "回合结束折叠应回填工具输出：{:?}",
        inner.transcript
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dual_connection_broadcast_consistency() {
    let server = spawn_server(None, simple_answer_scripts()).await;
    let mut a = connect(&server.addr).await;
    let mut b = connect(&server.addr).await;
    for ws in [&mut a, &mut b] {
        assert!(matches!(hello(ws, None).await, ServerMsg::Snapshot { .. }));
    }

    // A 提交，B 也应收到同一事件序列（共享会话广播）。
    send_msg(
        &mut a,
        &ClientMsg::Submit {
            text: "你好".into(),
        },
    )
    .await;
    let events_a = collect_until(&mut a, |m| matches!(m, ServerMsg::TurnOk { .. })).await;
    let events_b = collect_until(&mut b, |m| matches!(m, ServerMsg::TurnOk { .. })).await;

    let json_a: Vec<String> = events_a
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    let json_b: Vec<String> = events_b
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    assert_eq!(json_a, json_b, "双连接事件序列必须逐字节一致");
    assert!(json_a.iter().any(|s| s.contains("\"delta\"")));
    assert!(json_a.iter().any(|s| s.contains("\"turn_ok\"")));

    // 后接入的客户端 snapshot 恢复现场（重连语义）。
    let mut c = connect(&server.addr).await;
    match hello(&mut c, None).await {
        ServerMsg::Snapshot { session } => {
            assert_eq!(session.session_stats.turns, 1);
            assert!(!session.transcript.is_empty(), "重连 snapshot 应含既有转写");
            assert!(session
                .transcript
                .iter()
                .any(|e| matches!(e, openslate_protocol::EntryDto::User { .. })));
        }
        other => panic!("expected snapshot, got {:?}", type_of(&other)),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approval_first_answer_wins_late_gets_notice() {
    let server = spawn_server(None, full_turn_scripts()).await;
    let mut a = connect(&server.addr).await;
    let mut b = connect(&server.addr).await;
    for ws in [&mut a, &mut b] {
        assert!(matches!(hello(ws, None).await, ServerMsg::Snapshot { .. }));
    }

    send_msg(
        &mut a,
        &ClientMsg::Submit {
            text: "读文件".into(),
        },
    )
    .await;

    // 两边都收到 approval_requested（同 id）。
    let id_a = collect_until(&mut a, |m| matches!(m, ServerMsg::ApprovalRequested { .. }))
        .await
        .into_iter()
        .find_map(|m| match m {
            ServerMsg::ApprovalRequested { id, .. } => Some(id),
            _ => None,
        })
        .expect("a got approval_requested");
    let id_b = collect_until(&mut b, |m| matches!(m, ServerMsg::ApprovalRequested { .. }))
        .await
        .into_iter()
        .find_map(|m| match m {
            ServerMsg::ApprovalRequested { id, .. } => Some(id),
            _ => None,
        })
        .expect("b got approval_requested");
    assert_eq!(id_a, id_b, "共享会话同一审批 id");

    // A 首答 approve；B 迟到 deny → 定向 notice。
    send_msg(
        &mut a,
        &ClientMsg::ApprovalAnswer {
            id: id_a,
            choice: ApprovalAnswerChoice::Approve,
        },
    )
    .await;
    // 等 resolved 广播落地（首答已生效）。
    collect_until(&mut a, |m| matches!(m, ServerMsg::ApprovalResolved { .. })).await;

    send_msg(
        &mut b,
        &ClientMsg::ApprovalAnswer {
            id: id_b,
            choice: ApprovalAnswerChoice::Deny,
        },
    )
    .await;
    let b_tail = collect_until(&mut b, |m| {
        matches!(m, ServerMsg::Notice { .. }) || matches!(m, ServerMsg::TurnOk { .. })
    })
    .await;
    assert!(
        b_tail.iter().any(|m| matches!(
            m,
            ServerMsg::Notice { text, .. } if text.contains("已由其他客户端应答")
        )),
        "迟到应答应收到定向 notice：{b_tail:?}"
    );

    // 回合照常完成（首答 approve 生效）。
    let a_end = collect_until(&mut a, |m| matches!(m, ServerMsg::TurnOk { .. })).await;
    assert!(matches!(a_end.last(), Some(ServerMsg::TurnOk { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crud_upsert_hot_swap_and_config_changed() {
    let server = spawn_server(None, Vec::new()).await;
    let mut a = connect(&server.addr).await;
    let mut b = connect(&server.addr).await;
    for ws in [&mut a, &mut b] {
        assert!(matches!(hello(ws, None).await, ServerMsg::Snapshot { .. }));
    }

    // upsert_model：两连接都收到 config_changed，视图含新条目。
    send_msg(
        &mut a,
        &ClientMsg::UpsertModel {
            entry: "extra".into(),
            model: openslate_protocol::ModelDto {
                provider: "mock".into(),
                model: "mock-extra".into(),
                max_context_tokens: None,
                max_output_tokens: None,
                supports_tool_call: true,
                supports_vision: false,
                supports_reasoning: false,
                input_price_per_mtok: None,
                output_price_per_mtok: None,
            },
        },
    )
    .await;
    for ws in [&mut a, &mut b] {
        let events = collect_until(ws, |m| matches!(m, ServerMsg::ConfigChanged { .. })).await;
        match events.last() {
            Some(ServerMsg::ConfigChanged { config }) => {
                assert!(config.models.contains_key("extra"));
                assert_eq!(config.models["extra"].model, "mock-extra");
            }
            other => panic!("expected config_changed, got {other:?}"),
        }
    }

    // 热替换生效：core.config 已换新。
    assert!(server.state.core.lock().config.models.contains_key("extra"));

    // set_level：级别绑定写 active，config_changed 再达。
    send_msg(
        &mut a,
        &ClientMsg::SetLevel {
            level: "backup".into(),
            entry: "extra".into(),
        },
    )
    .await;
    let events = collect_until(&mut a, |m| matches!(m, ServerMsg::ConfigChanged { .. })).await;
    match events.last() {
        Some(ServerMsg::ConfigChanged { config }) => {
            assert_eq!(
                config.levels.get("backup").map(String::as_str),
                Some("extra")
            );
        }
        other => panic!("expected config_changed, got {other:?}"),
    }

    // set_model：别名解析成功 → model_changed 广播。
    send_msg(
        &mut a,
        &ClientMsg::SetModel {
            alias: "extra".into(),
        },
    )
    .await;
    let events = collect_until(&mut a, |m| matches!(m, ServerMsg::ModelChanged { .. })).await;
    match events.last() {
        Some(ServerMsg::ModelChanged { alias }) => assert_eq!(alias, "extra"),
        other => panic!("expected model_changed, got {other:?}"),
    }
    assert_eq!(server.state.core.lock().model_alias, "extra");

    // 引用守卫：删除被 extra 引用的 provider mock → notice 拒绝。
    // （先加级别绑定让引用成立：models.main/fast 都引用 mock。）
    send_msg(
        &mut a,
        &ClientMsg::DeleteProvider {
            name: "mock".into(),
        },
    )
    .await;
    let events = collect_until(&mut a, |m| {
        matches!(m, ServerMsg::Notice { .. }) || matches!(m, ServerMsg::ConfigChanged { .. })
    })
    .await;
    assert!(
        events.iter().any(|m| matches!(
            m,
            ServerMsg::Notice { text, .. } if text.contains("被模型条目引用")
        )),
        "被引用 provider 删除应被拒：{events:?}"
    );

    // main 级别不可删。
    send_msg(
        &mut a,
        &ClientMsg::DeleteLevel {
            level: "main".into(),
        },
    )
    .await;
    let events = collect_until(&mut a, |m| matches!(m, ServerMsg::Notice { .. })).await;
    assert!(
        events.iter().any(|m| matches!(
            m,
            ServerMsg::Notice { text, .. } if text.contains("不可删除")
        )),
        "main 级别删除应被拒：{events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_token_enforced_on_ws_and_rest() {
    let server = spawn_server(Some("s3cret".into()), Vec::new()).await;

    // 错 token：error(bad_token) + close。
    let mut ws = connect(&server.addr).await;
    send_msg(
        &mut ws,
        &ClientMsg::Hello {
            proto: 1,
            token: Some("wrong".into()),
        },
    )
    .await;
    match next_msg(&mut ws).await {
        ServerMsg::Error { code, message } => {
            assert_eq!(code, "bad_token");
            assert!(!message.is_empty());
        }
        other => panic!("expected error, got {:?}", type_of(&other)),
    }
    // 连接应被关闭（后续 recv 结束或超时均可，不断言具体形态）。

    // proto 不符：error(proto_mismatch)。
    let mut ws2 = connect(&server.addr).await;
    send_msg(
        &mut ws2,
        &ClientMsg::Hello {
            proto: 99,
            token: Some("s3cret".into()),
        },
    )
    .await;
    match next_msg(&mut ws2).await {
        ServerMsg::Error { code, .. } => assert_eq!(code, "proto_mismatch"),
        other => panic!("expected error, got {:?}", type_of(&other)),
    }

    // 正确 token：snapshot。
    let mut ws3 = connect(&server.addr).await;
    assert!(matches!(
        hello(&mut ws3, Some("s3cret".into())).await,
        ServerMsg::Snapshot { .. }
    ));

    // REST：无 token 401，有 token 200。
    let base = format!("http://{}/api", server.addr);
    let no_token = reqwest::get(format!("{base}/config")).await.expect("http");
    assert_eq!(no_token.status(), 401);
    let with_token = reqwest::get(format!("{base}/config?token=s3cret"))
        .await
        .expect("http");
    assert_eq!(with_token.status(), 200);
    let body: serde_json::Value = with_token.json().await.expect("json");
    assert!(body["models"].is_object());

    // health 免鉴权。
    let health = reqwest::get(format!("{base}/health")).await.expect("http");
    assert_eq!(health.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rest_readonly_endpoints() {
    let server = spawn_server(None, simple_answer_scripts()).await;
    let base = format!("http://{}/api", server.addr);

    // health
    let health: serde_json::Value = reqwest::get(format!("{base}/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");
    assert_eq!(health["proto"], 1);

    // config
    let config: serde_json::Value = reqwest::get(format!("{base}/config"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(config["providers"]["mock"].is_object());
    assert!(config["agents"]["id"].is_string());

    // agents
    let agents: serde_json::Value = reqwest::get(format!("{base}/agents"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(agents["id"], "root");

    // skills（空目录 → 空数组）
    let skills: serde_json::Value = reqwest::get(format!("{base}/skills"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(skills.as_array().unwrap().is_empty());

    // sessions：空 → []；跑一轮后 ≥1
    let sessions: serde_json::Value = reqwest::get(format!("{base}/sessions"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(sessions.as_array().unwrap().is_empty());
    let mut ws = connect(&server.addr).await;
    assert!(matches!(
        hello(&mut ws, None).await,
        ServerMsg::Snapshot { .. }
    ));
    send_msg(&mut ws, &ClientMsg::Submit { text: "hi".into() }).await;
    collect_until(&mut ws, |m| matches!(m, ServerMsg::TurnOk { .. })).await;
    let sessions: serde_json::Value = reqwest::get(format!("{base}/sessions"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arr = sessions.as_array().unwrap();
    assert!(!arr.is_empty(), "跑过回合后 sessions 应有 run 行");
    assert_eq!(arr[0]["root_agent_id"], "root");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_session_broadcasts_reset_and_fresh_snapshots() {
    let server = spawn_server(None, simple_answer_scripts()).await;
    let mut a = connect(&server.addr).await;
    let mut b = connect(&server.addr).await;
    for ws in [&mut a, &mut b] {
        assert!(matches!(hello(ws, None).await, ServerMsg::Snapshot { .. }));
    }

    send_msg(&mut a, &ClientMsg::Submit { text: "hi".into() }).await;
    collect_until(&mut a, |m| matches!(m, ServerMsg::TurnOk { .. })).await;
    collect_until(&mut b, |m| matches!(m, ServerMsg::TurnOk { .. })).await;

    // /new：session_reset 广播 + 每连接新 snapshot（stats 清零）。
    send_msg(&mut a, &ClientMsg::NewSession).await;
    for ws in [&mut a, &mut b] {
        let events = collect_until(ws, |m| matches!(m, ServerMsg::Snapshot { .. })).await;
        assert!(
            events.iter().any(|m| matches!(m, ServerMsg::SessionReset)),
            "reset 广播先于新 snapshot"
        );
        match events.last() {
            Some(ServerMsg::Snapshot { session }) => {
                assert_eq!(session.session_stats.turns, 0);
                assert!(session.transcript.is_empty());
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
    }
    let inner = server.state.core.lock();
    assert!(inner.history.is_empty());
    assert_eq!(inner.stats.turns, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submit_while_running_gets_notice() {
    let server = spawn_server(None, full_turn_scripts()).await;
    let mut ws = connect(&server.addr).await;
    assert!(matches!(
        hello(&mut ws, None).await,
        ServerMsg::Snapshot { .. }
    ));

    // 第一回合（会阻塞在审批上）。
    send_msg(
        &mut ws,
        &ClientMsg::Submit {
            text: "读文件".into(),
        },
    )
    .await;
    let head = collect_until(&mut ws, |m| {
        matches!(m, ServerMsg::ApprovalRequested { .. })
    })
    .await;
    let approval_id = match head.last() {
        Some(ServerMsg::ApprovalRequested { id, .. }) => *id,
        other => panic!("expected approval_requested, got {other:?}"),
    };

    // 回合进行中再 submit → 定向 notice。
    send_msg(
        &mut ws,
        &ClientMsg::Submit {
            text: "再来".into(),
        },
    )
    .await;
    let events = collect_until(
        &mut ws,
        |m| matches!(m, ServerMsg::Notice { text, .. } if text.contains("回合进行中")),
    )
    .await;
    assert!(!events.is_empty());

    // 清场：deny 审批让回合结束（工具被拒 → 引擎进入下一请求 → 脚本已
    // 耗尽 → 500 → turn_error；两种终态都可接受）。
    send_msg(
        &mut ws,
        &ClientMsg::ApprovalAnswer {
            id: approval_id,
            choice: ApprovalAnswerChoice::Deny,
        },
    )
    .await;
    let tail = collect_until(&mut ws, |m| {
        matches!(m, ServerMsg::TurnOk { .. }) || matches!(m, ServerMsg::TurnError { .. })
    })
    .await;
    assert!(matches!(
        tail.last(),
        Some(ServerMsg::TurnOk { .. }) | Some(ServerMsg::TurnError { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_mid_turn_stops_engine() {
    // 脚本 1 发一个 delta 后流挂住 → cancel 生效。
    let server = spawn_server_impl(
        None,
        ScriptedProvider::new(vec![vec![ModelStreamEvent::Delta("开".into())]]).hold_open(),
    )
    .await;
    let mut ws = connect(&server.addr).await;
    assert!(matches!(
        hello(&mut ws, None).await,
        ServerMsg::Snapshot { .. }
    ));

    send_msg(
        &mut ws,
        &ClientMsg::Submit {
            text: "开始".into(),
        },
    )
    .await;
    collect_until(&mut ws, |m| matches!(m, ServerMsg::Delta { .. })).await;

    send_msg(&mut ws, &ClientMsg::Cancel).await;
    let tail = collect_until(&mut ws, |m| {
        matches!(m, ServerMsg::TurnOk { .. }) || matches!(m, ServerMsg::TurnError { .. })
    })
    .await;
    // cancel → Interrupted 状态的 TurnOk（Phase 4 语义）。
    match tail.last() {
        Some(ServerMsg::TurnOk { summary }) => {
            assert_eq!(
                summary.status,
                openslate_core::types::RunStatus::Interrupted
            );
        }
        other => panic!("expected turn_ok(interrupted), got {other:?}"),
    }
    // 引擎收尾后 running=false、manager 回家。
    let inner = server.state.core.lock();
    assert!(!inner.running);
    assert!(inner.manager.is_some(), "manager 应回传");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_message_after_hello_gets_notice() {
    let server = spawn_server(None, Vec::new()).await;
    let mut ws = connect(&server.addr).await;
    assert!(matches!(
        hello(&mut ws, None).await,
        ServerMsg::Snapshot { .. }
    ));

    ws.send(WsMessage::Text("{not json".into())).await.unwrap();
    let events = collect_until(&mut ws, |m| matches!(m, ServerMsg::Notice { .. })).await;
    assert!(
        events
            .iter()
            .any(|m| matches!(m, ServerMsg::Notice { level, .. } if *level == openslate_protocol::NoticeLevel::Error))
    );
}
