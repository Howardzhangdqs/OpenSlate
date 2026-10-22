//! 协议契约测试（spec §3/§6）：每个变体一条**逐字节**样例。
//!
//! 这些字符串就是线格式契约本身——任何字段增删改名都会在这里红灯，
//! 改契约必须同步升 `PROTOCOL_VERSION` 并与 client 侧协商。

use openslate_core::types::Attachment;
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
        reasoning_effort: None,
    }
}

fn config_view() -> ConfigViewDto {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert("p1".to_string(), provider_dto());
    let mut models = std::collections::BTreeMap::new();
    models.insert("main".to_string(), model_dto());
    let mut levels = std::collections::BTreeMap::new();
    levels.insert("fast".to_string(), "main".to_string());
    let mut capabilities = std::collections::BTreeMap::new();
    capabilities.insert("main".to_string(), "main".to_string());
    capabilities.insert("compact".to_string(), "fast".to_string());
    capabilities.insert("title".to_string(), "fast".to_string());
    ConfigViewDto {
        providers,
        models,
        levels,
        capabilities,
        limits: LimitsDto {
            max_steps: 0,
            max_depth: 4,
            max_tool_calls: 20,
            max_child_agent_calls: 8,
            timeout_ms: 300000,
            max_context_messages: 16,
            max_context_bytes: 64000,
            max_output_bytes: 65536,
            output_store_threshold: 16384,
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
        approval: None,
    }
}

fn summary_dto() -> ApprovalSummaryDto {
    ApprovalSummaryDto {
        tool_name: "bash".into(),
        arguments: r#"{"command":"cargo test"}"#.into(),
        agent_id: "root".into(),
        risk_level: "high".into(),
        server: None,
        trigger: None,
        note: None,
    }
}

// ---- ClientMsg：18 个变体 ------------------------------------------------

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
            attachments: None,
        },
        r#"{"type":"submit","text":"你好"}"#,
    );
}

/// v4：Submit 带附件引用（id = FFI `import_attachment` 返回值；门控在
/// mobile dispatch 层，协议只透传引用形状）。
#[test]
fn client_submit_with_attachments() {
    assert_wire(
        ClientMsg::Submit {
            text: "看这张图".into(),
            attachments: Some(vec![
                AttachmentDto {
                    id: "att-1".into(),
                    mime: "image/jpeg".into(),
                    filename: Some("cat.jpg".into()),
                    tokens: None,
                },
                AttachmentDto {
                    id: "att-2".into(),
                    mime: "video/mp4".into(),
                    filename: None,
                    tokens: None,
                },
            ]),
        },
        r#"{"type":"submit","text":"看这张图","attachments":[{"id":"att-1","mime":"image/jpeg","filename":"cat.jpg"},{"id":"att-2","mime":"video/mp4"}]}"#,
    );
    // 旧格式（无 attachments 键）→ None（serde default，旧客户端零感知）。
    let de: ClientMsg = serde_json::from_str(r#"{"type":"submit","text":"hi"}"#).unwrap();
    assert_eq!(
        de,
        ClientMsg::Submit {
            text: "hi".into(),
            attachments: None,
        }
    );
    // 空数组显式发送（合法：等价于没有附件，mobile 门控按空集放行）。
    let de: ClientMsg =
        serde_json::from_str(r#"{"type":"submit","text":"hi","attachments":[]}"#).unwrap();
    assert_eq!(
        de,
        ClientMsg::Submit {
            text: "hi".into(),
            attachments: Some(Vec::new()),
        }
    );
}

/// v4：AttachmentDto 与 core `Attachment` 的持久化形状互转（`data`
/// 永不过 wire——core 侧 `#[serde(skip)]`，From 转换置 None）。
#[test]
fn attachment_dto_core_roundtrip() {
    let core = Attachment {
        id: "att-9".into(),
        mime: "image/png".into(),
        filename: Some("shot.png".into()),
        data: Some(std::sync::Arc::from(&b"png-bytes"[..])),
    };
    let dto = AttachmentDto::from(&core);
    assert_eq!(
        dto,
        AttachmentDto {
            id: "att-9".into(),
            mime: "image/png".into(),
            filename: Some("shot.png".into()),
            tokens: None,
        }
    );
    let back = Attachment::from(dto);
    assert_eq!(back.id, "att-9");
    assert_eq!(back.mime, "image/png");
    assert_eq!(back.filename.as_deref(), Some("shot.png"));
    assert!(back.data.is_none(), "data 绝不经协议/core 引用形状复活");
    // serde 形状：data 不出现在 core Attachment 序列化里（skip）。
    let ser = serde_json::to_string(&core).unwrap();
    assert_eq!(ser, r#"{"id":"att-9","mime":"image/png","filename":"shot.png"}"#);
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
    // v3 新增选择（审批卡持久化 / MCP 作用域选项；定稿线格式逐字对齐）。
    for (choice, lit) in [
        (ApprovalAnswerChoice::AlwaysTool, "always_tool"),
        (ApprovalAnswerChoice::SessionMcp, "session_mcp"),
        (ApprovalAnswerChoice::AlwaysMcp, "always_mcp"),
    ] {
        assert_wire(
            ClientMsg::ApprovalAnswer { id: 42, choice },
            &format!(r#"{{"type":"approval_answer","id":42,"choice":"{lit}"}}"#),
        );
    }
}

#[test]
fn client_question_reply() {
    // accept：answers 为 mobile 层定义的答案负载，协议透传不解释。
    assert_wire(
        ClientMsg::QuestionReply {
            request_id: "q-42".into(),
            action: QuestionReplyAction::Accept,
            answers: serde_json::json!({"answers":[{"id":"db_choice","selected":["SQLite"]}]}),
        },
        r#"{"type":"question_reply","request_id":"q-42","action":"accept","answers":{"answers":[{"id":"db_choice","selected":["SQLite"]}]}}"#,
    );
    // decline：整卡不答（负载可为 null）。
    assert_wire(
        ClientMsg::QuestionReply {
            request_id: "q-43".into(),
            action: QuestionReplyAction::Decline,
            answers: serde_json::Value::Null,
        },
        r#"{"type":"question_reply","request_id":"q-43","action":"decline","answers":null}"#,
    );
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
fn client_upsert_model_reasoning_effort() {
    // thinking effort（推理力度）：Some → snake_case 键 `reasoning_effort`
    // 出现在 model 对象；None → 整键跳过（上面 client_upsert_model 的
    // 样例字节不含该键即为此语义，旧端点零感知）。
    let mut dto = model_dto();
    dto.reasoning_effort = Some("high".into());
    assert_wire(
        ClientMsg::UpsertModel {
            entry: "main".into(),
            model: dto,
        },
        r#"{"type":"upsert_model","entry":"main","model":{"provider":"p1","model":"glm-4","max_context_tokens":128000,"max_output_tokens":4096,"supports_tool_call":true,"supports_vision":false,"supports_reasoning":true,"input_price_per_mtok":0.5,"output_price_per_mtok":2.0,"reasoning_effort":"high"}}"#,
    );
    // 旧格式（无该键）反序列化 → None。
    let de: ClientMsg = serde_json::from_str(
        r#"{"type":"upsert_model","entry":"m","model":{"provider":"p","model":"g","reasoning_effort":"low"}}"#,
    )
    .unwrap();
    match de {
        ClientMsg::UpsertModel { model, .. } => {
            assert_eq!(model.reasoning_effort, Some("low".to_owned()));
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
fn client_set_capability() {
    assert_wire(
        ClientMsg::SetCapability {
            capability: "compact".into(),
            alias: "fast".into(),
        },
        r#"{"type":"set_capability","capability":"compact","alias":"fast"}"#,
    );
}

#[test]
fn client_set_ask_user_config() {
    // never：与 core AskTimeout 的 serde 形状对齐（外部标签 unit 变体
    // = 裸字符串 "never"）。
    assert_wire(
        ClientMsg::SetAskUserConfig {
            default_timeout: AskUserTimeoutDto::Never,
            mask_secrets: true,
        },
        r#"{"type":"set_ask_user_config","default_timeout":"never","mask_secrets":true}"#,
    );
    // seconds：newtype 形状 {"seconds": n}（落盘 toml 即
    // default_timeout = { seconds = 300 }）。
    assert_wire(
        ClientMsg::SetAskUserConfig {
            default_timeout: AskUserTimeoutDto::Seconds(300),
            mask_secrets: false,
        },
        r#"{"type":"set_ask_user_config","default_timeout":{"seconds":300},"mask_secrets":false}"#,
    );
    // 形状对齐的机制化钉死：DTO 序列化 JSON 可直接反序列化为 core
    // `AskTimeout`（Deserialize-only，无法反向 to_value——从 DTO 侧单向
    // 验证即可），值经 From 往返不丢。
    for (dto, core_expect) in [
        (
            AskUserTimeoutDto::Never,
            openslate_core::config::AskTimeout::Never,
        ),
        (
            AskUserTimeoutDto::Seconds(600),
            openslate_core::config::AskTimeout::Seconds(600),
        ),
    ] {
        let json = serde_json::to_value(dto).unwrap();
        let core: openslate_core::config::AskTimeout = serde_json::from_value(json).unwrap();
        assert_eq!(core, core_expect);
        assert_eq!(AskUserTimeoutDto::from(core), dto);
    }
}

#[test]
fn server_run_title() {
    assert_wire_server(
        ServerMsg::RunTitle {
            run_id: "r-1".into(),
            title: "调试构建失败".into(),
        },
        r#"{"type":"run_title","run_id":"r-1","title":"调试构建失败"}"#,
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

#[test]
fn client_set_approval_config() {
    // v3（tool-approval P1a）：定稿线格式逐字对齐。
    let mut tool_levels = std::collections::BTreeMap::new();
    tool_levels.insert("bash".to_string(), "manual".to_string());
    assert_wire(
        ClientMsg::SetApprovalConfig {
            default_level: Some("auto".into()),
            tool_levels,
            server_levels: std::collections::BTreeMap::new(),
        },
        r#"{"type":"set_approval_config","default_level":"auto","tool_levels":{"bash":"manual"},"server_levels":{}}"#,
    );
    // default_level 缺省（None → 键不出现）；server_levels 非空样例。
    let mut tool_levels = std::collections::BTreeMap::new();
    tool_levels.insert("read_file".to_string(), "allow".to_string());
    let mut server_levels = std::collections::BTreeMap::new();
    server_levels.insert("github".to_string(), "allow".to_string());
    assert_wire(
        ClientMsg::SetApprovalConfig {
            default_level: None,
            tool_levels,
            server_levels,
        },
        r#"{"type":"set_approval_config","tool_levels":{"read_file":"allow"},"server_levels":{"github":"allow"}}"#,
    );
    // 空 maps 缺省容忍（serde default）。
    let de: ClientMsg = serde_json::from_str(r#"{"type":"set_approval_config"}"#).unwrap();
    match de {
        ClientMsg::SetApprovalConfig {
            default_level,
            tool_levels,
            server_levels,
        } => {
            assert_eq!(default_level, None);
            assert!(tool_levels.is_empty());
            assert!(server_levels.is_empty());
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn config_view_approval_section() {
    // v3：ApprovalConfigViewDto 样例字节（字段序 = 声明序）。
    let mut tool_levels = std::collections::BTreeMap::new();
    tool_levels.insert("bash".to_string(), "manual".to_string());
    let mut server_levels = std::collections::BTreeMap::new();
    server_levels.insert("github".to_string(), "allow".to_string());
    let approval = ApprovalConfigViewDto {
        default_level: "auto".into(),
        tool_levels,
        server_levels,
    };
    assert_eq!(
        serde_json::to_string(&approval).unwrap(),
        r#"{"default_level":"auto","tool_levels":{"bash":"manual"},"server_levels":{"github":"allow"}}"#
    );

    // ConfigViewDto.approval：None → 键不出现（既有样例字节不变）；
    // Some → 序列化在视图尾部（最后声明的字段）。
    let mut view = config_view();
    let base = serde_json::to_string(&view).unwrap();
    assert!(!base.contains("\"approval\""), "{base}");
    view.approval = Some(approval);
    let with_approval = serde_json::to_string(&view).unwrap();
    assert!(
        with_approval.ends_with(
            r#","approval":{"default_level":"auto","tool_levels":{"bash":"manual"},"server_levels":{"github":"allow"}}}"#
        ),
        "{with_approval}"
    );

    // 旧视图（无 approval 键）→ 反序列化为 None（旧快照容忍）；
    // 新视图往返 Some。
    let de: ConfigViewDto = serde_json::from_str(&base).unwrap();
    assert_eq!(de.approval, None);
    let de: ConfigViewDto = serde_json::from_str(&with_approval).unwrap();
    assert_eq!(de.approval, view.approval);
}

// ---- ServerMsg：20 个变体 ------------------------------------------------

#[test]
fn server_snapshot() {
    let snap = SnapshotDto {
        proto: PROTOCOL_VERSION,
        session_id: "s-a1b2c3d4".into(),
        session_label: "openslate 会话".into(),
        run_id: None,
        transcript: vec![
            EntryDto::User {
                text: "hi".into(),
                attachments: None,
            },
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
        pending_questions: Vec::new(),
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
        r#"{"type":"snapshot","session":{"proto":5,"session_id":"s-a1b2c3d4","session_label":"openslate 会话","transcript":[{"kind":"user","text":"hi"},{"kind":"assistant","text":"hello"}],"running":false,"depth_cur":0,"agents_running":0,"tool_calls_cur":0,"model_alias":"main","pending_approval":{"id":3,"summary":{"tool_name":"bash","arguments":"{\"command\":\"cargo test\"}","agent_id":"root","risk_level":"high"}},"pending_questions":[],"config":{"providers":{"p1":{"base_url":"https://api.example.com/v1","api_key_env":"EXAMPLE_KEY","adapter":"openai","max_attempts":3,"retry_base_ms":500}},"models":{"main":{"provider":"p1","model":"glm-4","max_context_tokens":128000,"max_output_tokens":4096,"supports_tool_call":true,"supports_vision":false,"supports_reasoning":true,"input_price_per_mtok":0.5,"output_price_per_mtok":2.0}},"levels":{"fast":"main"},"capabilities":{"compact":"fast","main":"main","title":"fast"},"limits":{"max_steps":0,"max_depth":4,"max_tool_calls":20,"max_child_agent_calls":8,"timeout_ms":300000,"max_context_messages":16,"max_context_bytes":64000,"max_output_bytes":65536,"output_store_threshold":16384,"auto_compact":true,"parallel_tool_calls":true},"agents":{"id":"root","name":"Root","model":"main","children":[{"id":"researcher","name":"Researcher","model":"fast","children":[]}]},"skills":[{"name":"rust-testing","description":"Rust 测试技巧"}],"active_config":"/home/u/p/.openslate/openslate.toml","global_config":"/home/u/.config/openslate/openslate.toml","local_config":"/home/u/p/.openslate/openslate.toml"},"session_stats":{"turns":2,"total_input_tokens":1000,"total_output_tokens":200,"total_cost_usd":0.02}}}"#,
    );
}

#[test]
fn server_snapshot_pending_questions() {
    // ask-user（r4/r5）：重启恢复回放——快照携带待答问卷（含 tool_call_id
    // 关联）。server 投影恒空；本样例钉死 mobile 装配侧回填后的线格式。
    let mut snap = SnapshotDto {
        proto: PROTOCOL_VERSION,
        pending_approval: None,
        pending_questions: vec![PendingQuestionDto {
            request_id: "q-42".into(),
            tool_call_id: "tc-9".into(),
            questions: serde_json::json!({"questions":[{"id":"db_choice","header":"数据库","question":"SQLite 还是 sled？","type":"choice"}]}),
            status: QuestionStatus::Pending,
            version: 1,
        }],
        ..pending_questions_empty_snapshot()
    };
    assert_wire_server(
        ServerMsg::Snapshot {
            session: Box::new(snap.clone()),
        },
        r#"{"type":"snapshot","session":{"proto":5,"session_id":"s-a1b2c3d4","session_label":"openslate 会话","transcript":[],"running":false,"depth_cur":0,"agents_running":0,"tool_calls_cur":0,"model_alias":"main","pending_approval":null,"pending_questions":[{"request_id":"q-42","tool_call_id":"tc-9","questions":{"questions":[{"header":"数据库","id":"db_choice","question":"SQLite 还是 sled？","type":"choice"}]},"status":"pending","version":1}],"config":{"providers":{},"models":{},"levels":{},"capabilities":{},"limits":{"max_steps":0,"max_depth":4,"max_tool_calls":20,"max_child_agent_calls":8,"timeout_ms":60000,"max_context_messages":16,"max_context_bytes":64000,"max_output_bytes":65536,"output_store_threshold":16384,"auto_compact":true,"parallel_tool_calls":true},"agents":{"id":"root","name":"Root","model":"main","children":[]},"skills":[],"active_config":"/dev/null","global_config":null,"local_config":null},"session_stats":{"turns":0,"total_input_tokens":0,"total_output_tokens":0,"total_cost_usd":0.0}}}"#,
    );
    // status 全集（生命周期 pending → answered | declined | superseded | expired）。
    for (status, lit) in [
        (QuestionStatus::Pending, "pending"),
        (QuestionStatus::Answered, "answered"),
        (QuestionStatus::Declined, "declined"),
        (QuestionStatus::Superseded, "superseded"),
        (QuestionStatus::Expired, "expired"),
    ] {
        snap.pending_questions[0].status = status;
        let ser = serde_json::to_string(&snap.pending_questions[0]).unwrap();
        assert!(
            ser.contains(&format!(r#""status":"{lit}""#)),
            "{ser} must pin status {lit}"
        );
        let de: PendingQuestionDto = serde_json::from_str(&ser).unwrap();
        assert_eq!(de.status, status);
    }
}

/// 最小可序列化快照（server_snapshot_pending_questions 的基底）。
fn pending_questions_empty_snapshot() -> SnapshotDto {
    SnapshotDto {
        proto: PROTOCOL_VERSION,
        session_id: "s-a1b2c3d4".into(),
        session_label: "openslate 会话".into(),
        run_id: None,
        transcript: vec![],
        running: false,
        depth_cur: 0,
        agents_running: 0,
        tool_calls_cur: 0,
        model_alias: "main".into(),
        pending_approval: None,
        pending_questions: Vec::new(),
        config: ConfigViewDto {
            providers: Default::default(),
            models: Default::default(),
            levels: Default::default(),
            capabilities: Default::default(),
            limits: LimitsDto::from(&openslate_core::config::LimitsConfig::default()),
            agents: AgentNodeDto {
                id: "root".into(),
                name: "Root".into(),
                model: "main".into(),
                children: vec![],
            },
            skills: vec![],
            active_config: "/dev/null".into(),
            global_config: None,
            local_config: None,
            approval: None,
        },
        session_stats: SessionStatsDto::default(),
    }
}

#[test]
fn server_snapshot_run_id() {
    // C4：快照携带当前 run 的 run_id——mobile 转写镜像的落盘/恢复键改从
    // 事件流内嵌值取（快照 / turn_ok summary），不再旁路 FFI 直读实时态。
    // 带值往返保 Some；None 序列化为**缺键**（skip_serializing_if——wire
    // 上不出现 `"run_id":null`：Kotlin org.json 的 optString 对 JSON null
    // 返回字面量 "null"，曾污染镜像键）；旧快照缺键 → serde default
    // 反序列化为 None（双向兼容）。
    let mut snap = pending_questions_empty_snapshot();
    snap.run_id = Some("run-c4".into());
    let ser = serde_json::to_string(&snap).unwrap();
    assert!(ser.contains(r#""run_id":"run-c4""#), "{ser}");
    let de: SnapshotDto = serde_json::from_str(&ser).unwrap();
    assert_eq!(de.run_id.as_deref(), Some("run-c4"));

    snap.run_id = None;
    let ser_none = serde_json::to_string(&snap).unwrap();
    assert!(!ser_none.contains("run_id"), "{ser_none}");
    let de: SnapshotDto = serde_json::from_str(&ser_none).unwrap();
    assert_eq!(de.run_id, None);
    // 剥掉该键模拟旧快照：缺键 → None。
    let legacy = ser.replace(r#","run_id":"run-c4""#, "");
    assert!(!legacy.contains("run_id"), "{legacy}");
    let de: SnapshotDto = serde_json::from_str(&legacy).unwrap();
    assert_eq!(de.run_id, None);
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
                image_tokens: None,
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
            call_id: None,
        },
        r#"{"type":"tool_start","name":"read_file","args":"{\"path\":\"Cargo.toml\"}"}"#,
    );
    assert_wire_server(
        ServerMsg::ToolEnd {
            name: "read_file".into(),
            bytes: 1024,
            truncated: false,
            preview: None,
            call_id: None,
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
    // v2 形状（新字段 None → 键不出现）：既是 v3 的"无 server 作用域 /
    // 旧值"样例，也是旧 payload 的解析锚点。
    assert_wire_server(
        ServerMsg::ApprovalRequested {
            id: 3,
            summary: summary_dto(),
        },
        r#"{"type":"approval_requested","id":3,"summary":{"tool_name":"bash","arguments":"{\"command\":\"cargo test\"}","agent_id":"root","risk_level":"high"}}"#,
    );
    // v3：summary 新增 server / trigger / note（字段序 = 结构体声明序，
    // 全部 Option + None 跳键）。
    let mut summary = summary_dto();
    summary.server = Some("github".into());
    summary.trigger = Some("auto_risk".into());
    summary.note = Some("AI 审查判定高风险：rm -rf 不可逆删除".into());
    assert_wire_server(
        ServerMsg::ApprovalRequested { id: 9, summary },
        r#"{"type":"approval_requested","id":9,"summary":{"tool_name":"bash","arguments":"{\"command\":\"cargo test\"}","agent_id":"root","risk_level":"high","server":"github","trigger":"auto_risk","note":"AI 审查判定高风险：rm -rf 不可逆删除"}}"#,
    );
    // 旧 payload 容忍（v2 形状的 approval_requested / summary 无新字段
    // → 反序列化成功，新字段为 None）。
    let de: ServerMsg = serde_json::from_str(
        r#"{"type":"approval_requested","id":3,"summary":{"tool_name":"bash","arguments":"x","agent_id":"root","risk_level":"high"}}"#,
    )
    .unwrap();
    match de {
        ServerMsg::ApprovalRequested { summary, .. } => {
            assert_eq!(summary.tool_name, "bash");
            assert_eq!(summary.server, None);
            assert_eq!(summary.trigger, None);
            assert_eq!(summary.note, None);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_wire_server(
        ServerMsg::ApprovalResolved {
            id: 3,
            choice: "approve".into(),
        },
        r#"{"type":"approval_resolved","id":3,"choice":"approve"}"#,
    );
    // v3：ApprovalResolved.choice 新字符串值（String 值域扩展，由
    // session 层产生）。
    assert_wire_server(
        ServerMsg::ApprovalResolved {
            id: 3,
            choice: "always-mcp".into(),
        },
        r#"{"type":"approval_resolved","id":3,"choice":"always-mcp"}"#,
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
            attachments: None,
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
            kind: None,
        },
        r#"{"type":"turn_error","message":"provider 失败"}"#,
    );
    // kind 字段（回合级断连重试）：snake_case wire 形态；旧 payload 无
    // kind 键反序列化为 None（上方样例即覆盖）。
    assert_wire_server(
        ServerMsg::TurnError {
            message: "provider error: connection error: connection reset".into(),
            kind: Some(TurnErrorKind::Network),
        },
        r#"{"type":"turn_error","message":"provider error: connection error: connection reset","kind":"network"}"#,
    );
    assert_wire_server(
        ServerMsg::TurnError {
            message: "rate limit exceeded".into(),
            kind: Some(TurnErrorKind::RateLimit),
        },
        r#"{"type":"turn_error","message":"rate limit exceeded","kind":"rate_limit"}"#,
    );
    assert_wire_server(
        ServerMsg::TurnError {
            message: "authentication error: bad key".into(),
            kind: Some(TurnErrorKind::Auth),
        },
        r#"{"type":"turn_error","message":"authentication error: bad key","kind":"auth"}"#,
    );
    assert_wire_server(
        ServerMsg::TurnError {
            message: "malformed response".into(),
            kind: Some(TurnErrorKind::Model),
        },
        r#"{"type":"turn_error","message":"malformed response","kind":"model"}"#,
    );
    assert_wire_server(
        ServerMsg::TurnError {
            message: "max steps exceeded: 100".into(),
            kind: Some(TurnErrorKind::Other),
        },
        r#"{"type":"turn_error","message":"max steps exceeded: 100","kind":"other"}"#,
    );
}

#[test]
fn server_config_model_session_events() {
    assert_wire_server(
        ServerMsg::ConfigChanged {
            config: Box::new(config_view()),
        },
        r#"{"type":"config_changed","config":{"providers":{"p1":{"base_url":"https://api.example.com/v1","api_key_env":"EXAMPLE_KEY","adapter":"openai","max_attempts":3,"retry_base_ms":500}},"models":{"main":{"provider":"p1","model":"glm-4","max_context_tokens":128000,"max_output_tokens":4096,"supports_tool_call":true,"supports_vision":false,"supports_reasoning":true,"input_price_per_mtok":0.5,"output_price_per_mtok":2.0}},"levels":{"fast":"main"},"capabilities":{"compact":"fast","main":"main","title":"fast"},"limits":{"max_steps":0,"max_depth":4,"max_tool_calls":20,"max_child_agent_calls":8,"timeout_ms":300000,"max_context_messages":16,"max_context_bytes":64000,"max_output_bytes":65536,"output_store_threshold":16384,"auto_compact":true,"parallel_tool_calls":true},"agents":{"id":"root","name":"Root","model":"main","children":[{"id":"researcher","name":"Researcher","model":"fast","children":[]}]},"skills":[{"name":"rust-testing","description":"Rust 测试技巧"}],"active_config":"/home/u/p/.openslate/openslate.toml","global_config":"/home/u/.config/openslate/openslate.toml","local_config":"/home/u/p/.openslate/openslate.toml"}}"#,
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
        // v4：User 带附件引用（transcript 缩略图渲染用；filename None
        // 跳键，字段序 = 声明序）。
        (
            EntryDto::User {
                text: "看图".into(),
                attachments: Some(vec![AttachmentDto {
                    id: "att-7".into(),
                    mime: "image/jpeg".into(),
                    filename: None,
                    tokens: None,
                }]),
            },
            r#"{"kind":"user","text":"看图","attachments":[{"id":"att-7","mime":"image/jpeg"}]}"#,
        ),
        // v5：User 附件带实测 tokens（响应侧回填后的 transcript 形状；
        // tokens None 跳键，字段序 = 声明序）。
        (
            EntryDto::User {
                text: "看图".into(),
                attachments: Some(vec![AttachmentDto {
                    id: "att-8".into(),
                    mime: "image/png".into(),
                    filename: Some("shot.png".into()),
                    tokens: Some(728),
                }]),
            },
            r#"{"kind":"user","text":"看图","attachments":[{"id":"att-8","mime":"image/png","filename":"shot.png","tokens":728}]}"#,
        ),
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
        server: None,
        trigger: openslate_core::approval::ApprovalTrigger::Manual,
        note: None,
    };
    let dto = ApprovalSummaryDto::from(&req);
    assert_eq!(dto.tool_name, "bash");
    assert_eq!(dto.arguments, r#"{"command":"cargo test"}"#);
    assert_eq!(dto.risk_level, "high");
    // v3：新字段经 From 透传（trigger 用 Display；None 字段为 None）。
    assert_eq!(dto.server, None);
    assert_eq!(dto.trigger.as_deref(), Some("manual"));
    assert_eq!(dto.note, None);

    // server/note 有值时的透传 + trigger Display（auto_risk）。
    let req = openslate_core::approval::ApprovalRequest {
        tool_name: "termux_github__create_issue".into(),
        arguments: serde_json::json!({}),
        agent_id: "root".into(),
        risk_level: openslate_core::approval::RiskLevel::High,
        server: Some("github".into()),
        trigger: openslate_core::approval::ApprovalTrigger::AutoRisk,
        note: Some("AI 审查判定高风险：疑似删除仓库".into()),
    };
    let dto = ApprovalSummaryDto::from(&req);
    assert_eq!(dto.server.as_deref(), Some("github"));
    assert_eq!(dto.trigger.as_deref(), Some("auto_risk"));
    assert_eq!(
        dto.note.as_deref(),
        Some("AI 审查判定高风险：疑似删除仓库")
    );
}

#[test]
fn approval_summary_clips_long_args() {
    let long = "x".repeat(200);
    let req = openslate_core::approval::ApprovalRequest {
        tool_name: "bash".into(),
        arguments: serde_json::json!({ "cmd": long }),
        agent_id: "root".into(),
        risk_level: openslate_core::approval::RiskLevel::Medium,
        server: None,
        trigger: openslate_core::approval::ApprovalTrigger::AutoRisk,
        note: None,
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
        reasoning_effort: None,
    });
    let back = ModelConfig::from(m);
    assert_eq!(back.model, "g");
    assert!(back.supports_tool_call);
}

#[test]
fn protocol_version_pinned() {
    // v2：ask_user 问卷（question_reply + snapshot.pending_questions）。
    // v3：审批扩展（answer 新 choice / summary 新字段 / set_approval_config
    // + config_view.approval）。
    // v4：多模态输入（AttachmentDto + Submit/User attachments 字段；
    // 样例字节对旧 payload 逐字节不变——快照内嵌 proto 除外，随版本走）。
    // v5：AttachmentDto.tokens（响应侧 image tokens 回填；导入侧不预估）。
    assert_eq!(PROTOCOL_VERSION, 5);
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
        hidden_tools: Vec::new(),
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
