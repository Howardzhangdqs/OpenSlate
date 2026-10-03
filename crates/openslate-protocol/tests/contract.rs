//! 协议契约测试（spec §3/§6）：每个变体一条**逐字节**样例。
//!
//! 这些字符串就是线格式契约本身——任何字段增删改名都会在这里红灯，
//! 改契约必须同步升 `PROTOCOL_VERSION` 并与 client 侧协商。

use openslate_protocol::*;

/// 通用断言：序列化 == 样例字节，且样例可反序列化回原值（往返）。
fn assert_wire(msg: ClientMsg, json: &str) {
    let ser = serde_json::to_string(&msg).unwrap();
    assert_eq!(ser, json, "serialize mismatch");
    let de: ClientMsg = serde_json::from_str(json).unwrap();
    assert_eq!(de, msg, "round-trip mismatch");
}

/// ServerMsg 侧同上；`ServerMsg` 无 `PartialEq`（内嵌 core `Message`），
/// 往返比对走 `serde_json::to_value`。
fn assert_wire_server(msg: ServerMsg, json: &str) {
    let ser = serde_json::to_string(&msg).unwrap();
    assert_eq!(ser, json, "serialize mismatch");
    let de: ServerMsg = serde_json::from_str(json).unwrap();
    assert_eq!(
        serde_json::to_value(&de).unwrap(),
        serde_json::to_value(&msg).unwrap(),
        "round-trip mismatch"
    );
}

fn provider_dto() -> ProviderDto {
    ProviderDto {
        base_url: "https://api.example.com/v1".into(),
        api_key_env: "EXAMPLE_KEY".into(),
        adapter: Some("openai".into()),
        title: None,
        max_attempts: 3,
        retry_base_ms: 500,
    }
}

fn model_dto() -> ModelDto {
    ModelDto {
        provider: "p1".into(),
        model: "glm-4".into(),
        max_context_tokens: Some(128000),
        max_output_tokens: Some(4096),
        supports_tool_call: true,
        supports_vision: false,
        supports_reasoning: true,
        input_price_per_mtok: Some(0.5),
        output_price_per_mtok: Some(2.0),
    }
}

fn config_view() -> ConfigViewDto {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert("p1".to_string(), provider_dto());
    let mut models = std::collections::BTreeMap::new();
    models.insert("main".to_string(), model_dto());
    let mut levels = std::collections::BTreeMap::new();
    levels.insert("fast".to_string(), "main".to_string());
    ConfigViewDto {
        providers,
        models,
        levels,
        limits: LimitsDto {
            max_steps: 0,
            max_depth: 4,
            max_tool_calls: 20,
            max_child_agent_calls: 8,
            timeout_ms: 300000,
            max_context_messages: 16,
            max_context_bytes: 64000,
            max_output_bytes: 65536,
            auto_compact: true,
            parallel_tool_calls: true,
        },
        agents: AgentNodeDto {
            id: "root".into(),
            name: "Root".into(),
            model: "main".into(),
            children: vec![AgentNodeDto {
                id: "researcher".into(),
                name: "Researcher".into(),
                model: "fast".into(),
                children: vec![],
            }],
        },
        skills: vec![SkillInfoDto {
            name: "rust-testing".into(),
            description: "Rust 测试技巧".into(),
        }],
        active_config: "/home/u/p/.openslate/openslate.toml".into(),
        global_config: Some("/home/u/.config/openslate/openslate.toml".into()),
        local_config: Some("/home/u/p/.openslate/openslate.toml".into()),
    }
}

fn summary_dto() -> ApprovalSummaryDto {
    ApprovalSummaryDto {
        tool_name: "bash".into(),
        arguments: r#"{"command":"cargo test"}"#.into(),
        agent_id: "root".into(),
        risk_level: "high".into(),
    }
}

// ---- ClientMsg：13 个变体 ------------------------------------------------

#[test]
fn client_hello() {
    assert_wire(
        ClientMsg::Hello {
            proto: 1,
            token: Some("s3cret".into()),
        },
        r#"{"type":"hello","proto":1,"token":"s3cret"}"#,
    );
    // token 可省（无鉴权部署）。
    let de: ClientMsg = serde_json::from_str(r#"{"type":"hello","proto":1}"#).unwrap();
    assert_eq!(
        de,
        ClientMsg::Hello {
            proto: 1,
            token: None
        }
    );
}

#[test]
fn client_submit() {
    assert_wire(
        ClientMsg::Submit {
            text: "你好".into(),
        },
        r#"{"type":"submit","text":"你好"}"#,
    );
}

#[test]
fn client_approval_answer() {
    for (choice, lit) in [
        (ApprovalAnswerChoice::Approve, "approve"),
        (ApprovalAnswerChoice::Deny, "deny"),
        (ApprovalAnswerChoice::ApproveAll, "approve_all"),
    ] {
        assert_wire(
            ClientMsg::ApprovalAnswer { id: 7, choice },
            &format!(r#"{{"type":"approval_answer","id":7,"choice":"{lit}"}}"#),
        );
    }
}

#[test]
fn client_unit_variants() {
    assert_wire(ClientMsg::Cancel, r#"{"type":"cancel"}"#);
    assert_wire(ClientMsg::NewSession, r#"{"type":"new_session"}"#);
}

#[test]
fn client_set_model() {
    assert_wire(
        ClientMsg::SetModel {
            alias: "fast".into(),
        },
        r#"{"type":"set_model","alias":"fast"}"#,
    );
}

#[test]
fn client_upsert_provider() {
    assert_wire(
        ClientMsg::UpsertProvider {
            name: "p1".into(),
            provider: provider_dto(),
        },
        r#"{"type":"upsert_provider","name":"p1","provider":{"base_url":"https://api.example.com/v1","api_key_env":"EXAMPLE_KEY","adapter":"openai","max_attempts":3,"retry_base_ms":500}}"#,
    );
    // adapter 省略 → None（旧 client 兼容）。
    let de: ClientMsg = serde_json::from_str(
        r#"{"type":"upsert_provider","name":"p1","provider":{"base_url":"u","api_key_env":"K","max_attempts":1,"retry_base_ms":10}}"#,
    )
    .unwrap();
    match de {
        ClientMsg::UpsertProvider { provider, .. } => {
            assert_eq!(provider.adapter, None);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn client_delete_provider() {
    assert_wire(
        ClientMsg::DeleteProvider { name: "p1".into() },
        r#"{"type":"delete_provider","name":"p1"}"#,
    );
}

#[test]
fn client_upsert_model() {
    assert_wire(
        ClientMsg::UpsertModel {
            entry: "main".into(),
            model: model_dto(),
        },
        r#"{"type":"upsert_model","entry":"main","model":{"provider":"p1","model":"glm-4","max_context_tokens":128000,"max_output_tokens":4096,"supports_tool_call":true,"supports_vision":false,"supports_reasoning":true,"input_price_per_mtok":0.5,"output_price_per_mtok":2.0}}"#,
    );
    // 可选字段省略 → 默认值。
    let de: ClientMsg = serde_json::from_str(
        r#"{"type":"upsert_model","entry":"m","model":{"provider":"p","model":"g"}}"#,
    )
    .unwrap();
    match de {
        ClientMsg::UpsertModel { model, .. } => {
            assert_eq!(model.max_context_tokens, None);
            assert!(!model.supports_tool_call);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn client_delete_model() {
    assert_wire(
        ClientMsg::DeleteModel {
            entry: "old".into(),
        },
        r#"{"type":"delete_model","entry":"old"}"#,
    );
}

#[test]
fn client_set_level_and_delete() {
    assert_wire(
        ClientMsg::SetLevel {
            level: "fast".into(),
            entry: "main".into(),
        },
        r#"{"type":"set_level","level":"fast","entry":"main"}"#,
    );
    assert_wire(
        ClientMsg::DeleteLevel { level: "lv".into() },
        r#"{"type":"delete_level","level":"lv"}"#,
    );
}

#[test]
fn client_set_api_key() {
    assert_wire(
        ClientMsg::SetApiKey {
            provider: "p1".into(),
            value: "sk-xxx".into(),
        },
        r#"{"type":"set_api_key","provider":"p1","value":"sk-xxx"}"#,
    );
}

// ---- ServerMsg：20 个变体 ------------------------------------------------

#[test]
fn server_snapshot() {
    let snap = SnapshotDto {
        proto: PROTOCOL_VERSION,
        session_id: "s-a1b2c3d4".into(),
        session_label: "openslate 会话".into(),
        transcript: vec![
            EntryDto::User { text: "hi".into() },
            EntryDto::Assistant {
                text: "hello".into(),
            },
        ],
        running: false,
        depth_cur: 0,
        agents_running: 0,
        tool_calls_cur: 0,
        model_alias: "main".into(),
        pending_approval: Some(PendingApprovalDto {
            id: 3,
            summary: summary_dto(),
        }),
        config: config_view(),
        session_stats: SessionStatsDto {
            turns: 2,
            total_input_tokens: 1000,
            total_output_tokens: 200,
            total_cost_usd: 0.02,
        },
    };
    assert_wire_server(
        ServerMsg::Snapshot {
            session: Box::new(snap),
        },
        r#"{"type":"snapshot","session":{"proto":1,"session_id":"s-a1b2c3d4","session_label":"openslate 会话","transcript":[{"kind":"user","text":"hi"},{"kind":"assistant","text":"hello"}],"running":false,"depth_cur":0,"agents_running":0,"tool_calls_cur":0,"model_alias":"main","pending_approval":{"id":3,"summary":{"tool_name":"bash","arguments":"{\"command\":\"cargo test\"}","agent_id":"root","risk_level":"high"}},"config":{"providers":{"p1":{"base_url":"https://api.example.com/v1","api_key_env":"EXAMPLE_KEY","adapter":"openai","max_attempts":3,"retry_base_ms":500}},"models":{"main":{"provider":"p1","model":"glm-4","max_context_tokens":128000,"max_output_tokens":4096,"supports_tool_call":true,"supports_vision":false,"supports_reasoning":true,"input_price_per_mtok":0.5,"output_price_per_mtok":2.0}},"levels":{"fast":"main"},"limits":{"max_steps":0,"max_depth":4,"max_tool_calls":20,"max_child_agent_calls":8,"timeout_ms":300000,"max_context_messages":16,"max_context_bytes":64000,"max_output_bytes":65536,"auto_compact":true,"parallel_tool_calls":true},"agents":{"id":"root","name":"Root","model":"main","children":[{"id":"researcher","name":"Researcher","model":"fast","children":[]}]},"skills":[{"name":"rust-testing","description":"Rust 测试技巧"}],"active_config":"/home/u/p/.openslate/openslate.toml","global_config":"/home/u/.config/openslate/openslate.toml","local_config":"/home/u/p/.openslate/openslate.toml"},"session_stats":{"turns":2,"total_input_tokens":1000,"total_output_tokens":200,"total_cost_usd":0.02}}}"#,
    );
}

#[test]
fn server_stream_events() {
    assert_wire_server(
        ServerMsg::RequestStart {
            step: 1,
            model: "glm-4".into(),
        },
        r#"{"type":"request_start","step":1,"model":"glm-4"}"#,
    );
    assert_wire_server(ServerMsg::FirstToken, r#"{"type":"first_token"}"#);
    assert_wire_server(
        ServerMsg::Delta {
            text: "部分".into(),
        },
        r#"{"type":"delta","text":"部分"}"#,
    );
    assert_wire_server(
        ServerMsg::Reasoning {
            text: "思考".into(),
        },
        r#"{"type":"reasoning","text":"思考"}"#,
    );
    assert_wire_server(
        ServerMsg::InputEstimate { tokens: 120 },
        r#"{"type":"input_estimate","tokens":120}"#,
    );
    assert_wire_server(
        ServerMsg::Usage {
            usage: openslate_core::types::Usage {
                input_tokens: 100,
                output_tokens: 20,
                cached_input_tokens: Some(5),
                reasoning_tokens: None,
            },
        },
        r#"{"type":"usage","usage":{"input_tokens":100,"output_tokens":20,"cached_input_tokens":5,"reasoning_tokens":null}}"#,
    );
    assert_wire_server(ServerMsg::RequestEnd, r#"{"type":"request_end"}"#);
    assert_wire_server(ServerMsg::StepEnd, r#"{"type":"step_end"}"#);
}

#[test]
fn server_tool_events() {
    assert_wire_server(
        ServerMsg::ToolStart {
            name: "read_file".into(),
            args: r#"{"path":"Cargo.toml"}"#.into(),
        },
        r#"{"type":"tool_start","name":"read_file","args":"{\"path\":\"Cargo.toml\"}"}"#,
    );
    assert_wire_server(
        ServerMsg::ToolEnd {
            name: "read_file".into(),
            bytes: 1024,
            truncated: false,
            preview: None,
        },
        r#"{"type":"tool_end","name":"read_file","bytes":1024,"truncated":false}"#,
    );
    // running 状态只出现在 EntryDto（live 条目 call_id 恒 None）。
    let de: EntryDto = serde_json::from_str(
        r#"{"kind":"tool_call","name":"bash","args":"{...}","status":{"state":"running"},"detail":{"args":"{}","output":null}}"#,
    )
    .unwrap();
    assert_eq!(
        de,
        EntryDto::ToolCall {
            name: "bash".into(),
            args: "{...}".into(),
            call_id: None,
            status: ToolStatusDto::Running,
            detail: ToolEntryDetailDto {
                args: "{}".into(),
                output: None
            },
        }
    );
}

#[test]
fn server_approval_events() {
    assert_wire_server(
        ServerMsg::ApprovalRequested {
            id: 3,
            summary: summary_dto(),
        },
        r#"{"type":"approval_requested","id":3,"summary":{"tool_name":"bash","arguments":"{\"command\":\"cargo test\"}","agent_id":"root","risk_level":"high"}}"#,
    );
    assert_wire_server(
        ServerMsg::ApprovalResolved {
            id: 3,
            choice: "approve".into(),
        },
        r#"{"type":"approval_resolved","id":3,"choice":"approve"}"#,
    );
}

#[test]
fn server_turn_events() {
    let summary = TurnSummaryDto {
        run_id: openslate_core::types::RunId("run-9".into()),
        status: openslate_core::types::RunStatus::Completed,
        total_steps: 2,
        total_input_tokens: 500,
        total_output_tokens: 80,
        total_cost_usd: 0.01,
        model: "glm-4".into(),
        messages: vec![openslate_core::types::Message {
            role: openslate_core::types::MessageRole::User,
            content: "hi".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        }],
    };
    assert_wire_server(
        ServerMsg::TurnOk {
            summary: Box::new(summary),
        },
        r#"{"type":"turn_ok","summary":{"run_id":"run-9","status":"completed","total_steps":2,"total_input_tokens":500,"total_output_tokens":80,"total_cost_usd":0.01,"model":"glm-4","messages":[{"role":"user","content":"hi"}]}}"#,
    );
    assert_wire_server(
        ServerMsg::TurnError {
            message: "provider 失败".into(),
        },
        r#"{"type":"turn_error","message":"provider 失败"}"#,
    );
}

#[test]
fn server_config_model_session_events() {
    assert_wire_server(
        ServerMsg::ConfigChanged {
            config: Box::new(config_view()),
        },
        r#"{"type":"config_changed","config":{"providers":{"p1":{"base_url":"https://api.example.com/v1","api_key_env":"EXAMPLE_KEY","adapter":"openai","max_attempts":3,"retry_base_ms":500}},"models":{"main":{"provider":"p1","model":"glm-4","max_context_tokens":128000,"max_output_tokens":4096,"supports_tool_call":true,"supports_vision":false,"supports_reasoning":true,"input_price_per_mtok":0.5,"output_price_per_mtok":2.0}},"levels":{"fast":"main"},"limits":{"max_steps":0,"max_depth":4,"max_tool_calls":20,"max_child_agent_calls":8,"timeout_ms":300000,"max_context_messages":16,"max_context_bytes":64000,"max_output_bytes":65536,"auto_compact":true,"parallel_tool_calls":true},"agents":{"id":"root","name":"Root","model":"main","children":[{"id":"researcher","name":"Researcher","model":"fast","children":[]}]},"skills":[{"name":"rust-testing","description":"Rust 测试技巧"}],"active_config":"/home/u/p/.openslate/openslate.toml","global_config":"/home/u/.config/openslate/openslate.toml","local_config":"/home/u/p/.openslate/openslate.toml"}}"#,
    );
    assert_wire_server(
        ServerMsg::ModelChanged {
            alias: "fast".into(),
        },
        r#"{"type":"model_changed","alias":"fast"}"#,
    );
    assert_wire_server(ServerMsg::SessionReset, r#"{"type":"session_reset"}"#);
}

#[test]
fn server_notice_error() {
    assert_wire_server(
        ServerMsg::Notice {
            text: "已由其他客户端应答".into(),
            level: NoticeLevel::Info,
        },
        r#"{"type":"notice","text":"已由其他客户端应答","level":"info"}"#,
    );
    assert_wire_server(
        ServerMsg::Notice {
            text: "回合进行中".into(),
            level: NoticeLevel::Warn,
        },
        r#"{"type":"notice","text":"回合进行中","level":"warn"}"#,
    );
    assert_wire_server(
        ServerMsg::Error {
            code: "proto_mismatch".into(),
            message: "协议版本不符：server=1 client=2".into(),
        },
        r#"{"type":"error","code":"proto_mismatch","message":"协议版本不符：server=1 client=2"}"#,
    );
}

// ---- 其余 EntryDto 变体 --------------------------------------------------

#[test]
fn entry_dto_variants() {
    let cases: Vec<(EntryDto, &str)> = vec![
        (
            EntryDto::Reasoning {
                text: "想一想".into(),
            },
            r#"{"kind":"reasoning","text":"想一想"}"#,
        ),
        (
            EntryDto::Approval {
                tool_name: "bash".into(),
                decision: "denied".into(),
            },
            r#"{"kind":"approval","tool_name":"bash","decision":"denied"}"#,
        ),
        (
            EntryDto::Delegate {
                agent: "researcher".into(),
                done: false,
            },
            r#"{"kind":"delegate","agent":"researcher","done":false}"#,
        ),
        (EntryDto::StepBreak, r#"{"kind":"step_break"}"#),
        (
            EntryDto::Meta {
                text: "~42tok".into(),
            },
            r#"{"kind":"meta","text":"~42tok"}"#,
        ),
        (
            EntryDto::ToolCall {
                name: "bash".into(),
                args: "cargo test".into(),
                call_id: None,
                status: ToolStatusDto::Done {
                    bytes: 30,
                    truncated: true,
                    elapsed_ms: Some(5),
                },
                detail: ToolEntryDetailDto {
                    args: "{}".into(),
                    output: Some("ok".into()),
                },
            },
            r#"{"kind":"tool_call","name":"bash","args":"cargo test","status":{"state":"done","bytes":30,"truncated":true,"elapsed_ms":5},"detail":{"args":"{}","output":"ok"}}"#,
        ),
        (
            EntryDto::ToolCall {
                name: "bash".into(),
                args: "cargo build".into(),
                call_id: Some("call-7".into()),
                status: ToolStatusDto::Failed {
                    summary: "Error: exit 1".into(),
                },
                detail: ToolEntryDetailDto {
                    args: "{}".into(),
                    output: None,
                },
            },
            r#"{"kind":"tool_call","name":"bash","args":"cargo build","call_id":"call-7","status":{"state":"failed","summary":"Error: exit 1"},"detail":{"args":"{}","output":null}}"#,
        ),
    ];
    for (entry, json) in cases {
        assert_eq!(serde_json::to_string(&entry).unwrap(), json);
        let de: EntryDto = serde_json::from_str(json).unwrap();
        assert_eq!(de, entry);
    }
}

// ---- From 转换 / 截断行为 -------------------------------------------------

#[test]
fn approval_summary_from_request() {
    let req = openslate_core::approval::ApprovalRequest {
        tool_name: "bash".into(),
        arguments: serde_json::json!({"command": "cargo test"}),
        agent_id: "root".into(),
        risk_level: openslate_core::approval::RiskLevel::High,
    };
    let dto = ApprovalSummaryDto::from(&req);
    assert_eq!(dto.tool_name, "bash");
    assert_eq!(dto.arguments, r#"{"command":"cargo test"}"#);
    assert_eq!(dto.risk_level, "high");
}

#[test]
fn approval_summary_clips_long_args() {
    let long = "x".repeat(200);
    let req = openslate_core::approval::ApprovalRequest {
        tool_name: "bash".into(),
        arguments: serde_json::json!({ "cmd": long }),
        agent_id: "root".into(),
        risk_level: openslate_core::approval::RiskLevel::Medium,
    };
    let dto = ApprovalSummaryDto::from(&req);
    assert!(dto.arguments.chars().count() <= 120 + 1); // 120 + …
    assert!(dto.arguments.ends_with('…'));
    assert_eq!(dto.risk_level, "medium");
}

#[test]
fn provider_model_dto_roundtrip_with_core() {
    use openslate_core::config::{ModelConfig, ProviderConfig};
    let p = ProviderDto::from(&ProviderConfig {
        base_url: "u".into(),
        api_key_env: "K".into(),
        adapter: None,
        title: None,
        max_attempts: 2,
        retry_base_ms: 100,
    });
    assert_eq!(p.adapter, None);
    let back = ProviderConfig::from(p);
    assert_eq!(back.base_url, "u");

    let m = ModelDto::from(&ModelConfig {
        provider: "p".into(),
        model: "g".into(),
        max_context_tokens: None,
        max_output_tokens: None,
        supports_tool_call: true,
        supports_vision: false,
        supports_reasoning: false,
        input_price_per_mtok: None,
        output_price_per_mtok: None,
    });
    let back = ModelConfig::from(m);
    assert_eq!(back.model, "g");
    assert!(back.supports_tool_call);
}

#[test]
fn protocol_version_pinned() {
    assert_eq!(PROTOCOL_VERSION, 1);
}

#[test]
fn limits_dto_from_core() {
    let l = openslate_core::config::LimitsConfig::default();
    let dto = LimitsDto::from(&l);
    assert_eq!(dto.max_depth, 4);
    assert_eq!(dto.max_tool_calls, 20);
    assert!(dto.auto_compact);
}

#[test]
fn agent_node_dto_from_tree() {
    use openslate_core::agent_tree::AgentTree;
    use openslate_core::types::{AgentConfig, AgentId};
    let cfg = |id: &str, children: Vec<&str>| AgentConfig {
        id: AgentId(id.into()),
        name: format!("Agent-{id}"),
        model: "main".into(),
        children: children.into_iter().map(|c| AgentId(c.into())).collect(),
        tools: vec![],
        default_prompt: String::new(),
    };
    let tree = AgentTree::from_configs(&[
        cfg("root", vec!["researcher", "verifier"]),
        cfg("researcher", vec![]),
        cfg("verifier", vec![]),
    ])
    .unwrap();
    let dto = AgentNodeDto::from(&tree);
    assert_eq!(dto.id, "root");
    assert_eq!(dto.children.len(), 2);
    // children 顺序 = 配置声明序
    assert_eq!(dto.children[0].id, "researcher");
    assert_eq!(dto.children[1].id, "verifier");
    assert_eq!(dto.children[0].name, "Agent-researcher");
    assert!(dto.children[0].children.is_empty());
}

#[test]
fn session_stats_default_zeroed() {
    let s = SessionStatsDto::default();
    assert_eq!(s.turns, 0);
    assert_eq!(s.total_input_tokens, 0);
    assert_eq!(s.total_output_tokens, 0);
    assert_eq!(s.total_cost_usd, 0.0);
}

// ---- ServerInfo：server.json 落盘文件（auto-attach-1）---------------------

fn server_info() -> ServerInfo {
    ServerInfo {
        proto: 1,
        url: "ws://127.0.0.1:7800/api/ws".into(),
        pid: 4242,
        port: 7800,
        bind: "127.0.0.1".into(),
        started_at: "2026-09-30T15:20:19+08:00".into(),
        token: Some("s3cret".into()),
        cwd: "/home/u/proj".into(),
    }
}

#[test]
fn server_info_wire() {
    let info = server_info();
    let ser = serde_json::to_string(&info).unwrap();
    assert_eq!(
        ser,
        r#"{"proto":1,"url":"ws://127.0.0.1:7800/api/ws","pid":4242,"port":7800,"bind":"127.0.0.1","started_at":"2026-09-30T15:20:19+08:00","token":"s3cret","cwd":"/home/u/proj"}"#,
        "serialize mismatch"
    );
    let de: ServerInfo = serde_json::from_str(&ser).unwrap();
    assert_eq!(de, info, "round-trip mismatch");
}

#[test]
fn server_info_token_null_and_default() {
    // token None → null（字段仍出现）；缺 token 字段可解析（serde default，
    // 向前兼容手写/旧版文件）。
    let info = ServerInfo {
        token: None,
        ..server_info()
    };
    let ser = serde_json::to_string(&info).unwrap();
    assert_eq!(
        ser,
        r#"{"proto":1,"url":"ws://127.0.0.1:7800/api/ws","pid":4242,"port":7800,"bind":"127.0.0.1","started_at":"2026-09-30T15:20:19+08:00","token":null,"cwd":"/home/u/proj"}"#
    );
    let de: ServerInfo =
        serde_json::from_str(r#"{"proto":1,"url":"ws://127.0.0.1:7800/api/ws","pid":4242,"port":7800,"bind":"0.0.0.0","started_at":"2026-09-30T15:20:19+08:00","cwd":"/home/u/proj"}"#)
            .unwrap();
    assert_eq!(de.token, None);
    assert_eq!(de.bind, "0.0.0.0", "bind 与 url host 可以不同");
}
