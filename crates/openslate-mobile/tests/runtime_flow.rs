//! 集成测试：MobileRuntime 全链路（desktop 可跑，无 Android 依赖）。
//!
//! 1. create 即握手：首份 snapshot 事件。
//! 2. submit → 引擎回合 → request_start / delta / turn_ok 事件序。
//! 3. host call 全链路：模型调用 mobile.ping → host_call_requested 信封 →
//!    resolve_host_call → 工具结果回填 → 下一请求拿到工具输出 → 完成。

use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use openslate_core::error::ProviderError;
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::types::{ModelResponse, ModelStreamEvent, ToolCall, ToolCallId, Usage};
use openslate_mobile::{EventCallback, MobilePaths, MobileRuntime, RuntimeOptions};
use openslate_session::state::ProviderFactory;

// ── Scripted provider（与 server 集成测试同构）────────────────────────

#[derive(Clone)]
struct ScriptedProvider {
    scripts: Arc<Mutex<VecDeque<Vec<ModelStreamEvent>>>>,
    /// 每次流式请求收到的 system prompt（按到达顺序；热生效类测试断言用）。
    seen_system_prompts: Arc<Mutex<Vec<String>>>,
}

impl ScriptedProvider {
    fn new(scripts: Vec<Vec<ModelStreamEvent>>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts.into())),
            seen_system_prompts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn seen_system_prompts(&self) -> Vec<String> {
        self.seen_system_prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn generate(&self, _request: GenerateRequest) -> Result<ModelResponse, ProviderError> {
        Err(ProviderError::ServerError(500))
    }

    async fn generate_stream(
        &self,
        request: GenerateRequest,
    ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
        if let Some(sp) = &request.system_prompt {
            self.seen_system_prompts.lock().unwrap().push(sp.clone());
        }
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
        tokio::spawn(async move {
            for event in script {
                let _ = tx.send(Ok(event)).await;
            }
        });
        rx
    }

    fn provider_name(&self) -> &str {
        "scripted"
    }
}

fn factory_for(provider: ScriptedProvider) -> ProviderFactory {
    Arc::new(move |_config, _alias| Ok(Box::new(provider.clone()) as Box<dyn ModelProvider>))
}

// ── 事件采集回调（泵线程 → std mpsc 通知）────────────────────────────

struct Collector {
    events: Mutex<Vec<serde_json::Value>>,
    notify: std_mpsc::Sender<()>,
    closed: AtomicBool,
}

impl EventCallback for Collector {
    fn on_event(&self, event_json: String) {
        if let Ok(v) = serde_json::from_str(&event_json) {
            self.events.lock().unwrap().push(v);
        }
        let _ = self.notify.send(());
    }
}

impl Collector {
    fn new() -> (Arc<Self>, std_mpsc::Receiver<()>) {
        let (tx, rx) = std_mpsc::channel();
        (
            Arc::new(Self {
                events: Mutex::new(Vec::new()),
                notify: tx,
                closed: AtomicBool::new(false),
            }),
            rx,
        )
    }

    /// 阻塞等待直到出现匹配 type 的第 n 条（超时失败）。
    fn wait_for(
        &self,
        rx: &std_mpsc::Receiver<()>,
        want_type: &str,
        deadline: Duration,
    ) -> Vec<serde_json::Value> {
        let start = Instant::now();
        loop {
            let all = self.events.lock().unwrap().clone();
            if all
                .iter()
                .any(|e| e.get("type").and_then(|t| t.as_str()) == Some(want_type))
            {
                return all;
            }
            assert!(
                start.elapsed() < deadline,
                "等待事件 {want_type} 超时；已收到: {:?}",
                all.iter()
                    .map(|e| e.get("type").and_then(|t| t.as_str()).unwrap_or("?"))
                    .collect::<Vec<_>>()
            );
            rx.recv_timeout(Duration::from_millis(200)).ok();
        }
    }
}

fn temp_paths() -> (MobilePaths, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let paths = MobilePaths {
        config_dir: dir.path().join("config"),
        workspace_dir: dir.path().join("workspace"),
        data_dir: dir.path().join("data"),
        cache_dir: dir.path().join("cache"),
    };
    (paths, dir)
}

fn text_response(text: &str) -> Vec<ModelStreamEvent> {
    vec![
        ModelStreamEvent::Delta(text.to_owned()),
        ModelStreamEvent::Usage(Usage {
            input_tokens: 8,
            output_tokens: 4,
            cached_input_tokens: None,
            reasoning_tokens: None,
            image_tokens: None,
        }),
        ModelStreamEvent::Done(ModelResponse {
            content: Some(text.to_owned()),
            tool_calls: vec![],
            reasoning_content: None,
            usage: None,
            finish_reason: Some("stop".into()),
        }),
    ]
}

// ── 测试 ─────────────────────────────────────────────────────────────

#[test]
fn mobile_runtime_closed_loop_streaming() {
    let (paths, _dir) = temp_paths();
    let (collector, rx) = Collector::new();

    let scripted = ScriptedProvider::new(vec![text_response("你好，我是 OpenSlate。")]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted)),
        ..Default::default()
    };

    let runtime = MobileRuntime::create_with(paths, collector.clone(), opts).unwrap();

    // create 即握手：首份 snapshot。
    let events = collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
    let snap = events.iter().find(|e| e["type"] == "snapshot").unwrap();
    assert!(snap["session"]["transcript"].is_array());

    // submit → request_start / delta / turn_ok。
    runtime
        .send(r#"{"type":"submit","text":"打个招呼"}"#)
        .unwrap();
    let events = collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));
    let types: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
        .collect();
    let i_start = types
        .iter()
        .position(|t| t == &"request_start")
        .expect("request_start");
    let i_delta = types.iter().position(|t| t == &"delta").expect("delta");
    let i_ok = types.iter().position(|t| t == &"turn_ok").expect("turn_ok");
    assert!(i_start < i_delta && i_delta < i_ok, "事件序: {types:?}");
    let delta = events.iter().find(|e| e["type"] == "delta").unwrap();
    assert_eq!(delta["text"].as_str().unwrap(), "你好，我是 OpenSlate。");

    // 统计入账。
    let stats = runtime.stats();
    assert_eq!(stats.turns, 1);

    runtime.shutdown();
}

#[test]
fn mobile_runtime_host_call_roundtrip() {
    let (paths, _dir) = temp_paths();
    let (collector, rx) = Collector::new();

    // 第一回合：模型点名调用 mobile.ping；第二回合：拿到工具结果后收尾。
    let step1 = vec![ModelStreamEvent::Done(ModelResponse {
        reasoning_content: None,
        content: None,
        tool_calls: vec![ToolCall {
            id: ToolCallId("call-1".into()),
            name: "mobile.ping".into(),
            arguments: serde_json::json!({}),
        }],
        usage: None,
        finish_reason: Some("tool_calls".into()),
    })];
    let scripted = ScriptedProvider::new(vec![step1, text_response("宿主在线。")]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted)),
        host_call_timeout: Duration::from_secs(10),
        ..Default::default()
    };

    let runtime = MobileRuntime::create_with(paths, collector.clone(), opts).unwrap();
    // 工具审批自 P1b 起 [approval] 缺省 opt-in Auto：mobile.ping（启发式
    // Medium）会进审查链弹人。本测试聚焦 host call 通路，先把审批缺省
    // 放开为 allow（恢复本 harness 原有 gating 行为）。
    runtime
        .send(r#"{"type":"set_approval_config","default_level":"allow","tool_levels":{},"server_levels":{}}"#)
        .unwrap();
    collector.wait_for(&rx, "config_changed", Duration::from_secs(10));

    runtime
        .send(r#"{"type":"submit","text":"ping 一下宿主"}"#)
        .unwrap();

    // 宿主侧收到 host_call_requested 信封并应答。
    let events = collector.wait_for(&rx, "host_call_requested", Duration::from_secs(15));
    let envelope = events
        .iter()
        .find(|e| e["type"] == "host_call_requested")
        .unwrap();
    assert_eq!(envelope["tool"].as_str().unwrap(), "mobile.ping");
    let id = envelope["id"].as_u64().unwrap();
    assert!(runtime.resolve_host_call(id, true, r#"{"pong":true}"#.to_owned()));

    // 回合完成；第二轮请求应包含工具结果消息（脚本只有 2 步，engine 收敛）。
    let events = collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));
    let tool_start_seen = events
        .iter()
        .any(|e| e["type"] == "tool_start" && e["name"] == "mobile.ping");
    assert!(
        tool_start_seen,
        "应有 mobile.ping 的 tool_start: {events:?}"
    );

    runtime.shutdown();
}

#[test]
fn mobile_runtime_hello_version_check() {
    let (paths, _dir) = temp_paths();
    let (collector, rx) = Collector::new();
    let scripted = ScriptedProvider::new(vec![]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted)),
        ..Default::default()
    };
    let runtime = MobileRuntime::create_with(paths, collector.clone(), opts).unwrap();
    collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

    // 错误版本号 → proto_mismatch 错误信封，而非 snapshot。
    runtime.send(r#"{"type":"hello","proto":999}"#).unwrap();
    let events = collector.wait_for(&rx, "error", Duration::from_secs(5));
    let err = events.iter().find(|e| e["type"] == "error").unwrap();
    assert_eq!(err["code"].as_str().unwrap(), "proto_mismatch");

    // 正确版本号 → 新 snapshot（等待数量增长，排除 create 时的首份）。
    let count_before = {
        let all = collector.events.lock().unwrap().clone();
        all.iter().filter(|e| e["type"] == "snapshot").count()
    };
    let proto = openslate_protocol::PROTOCOL_VERSION;
    runtime
        .send(&format!(r#"{{"type":"hello","proto":{proto}}}"#))
        .unwrap();
    let start = std::time::Instant::now();
    loop {
        let snapshots = collector
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "snapshot")
            .count();
        if snapshots > count_before {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "hello 应补发 snapshot"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    runtime.shutdown();
}

#[test]
fn mobile_runtime_shutdown_rejects_send() {
    let (paths, _dir) = temp_paths();
    let (collector, _rx) = Collector::new();
    let scripted = ScriptedProvider::new(vec![]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted)),
        ..Default::default()
    };
    let runtime = MobileRuntime::create_with(paths, collector, opts).unwrap();
    runtime.shutdown();
    assert!(runtime.send(r#"{"type":"cancel"}"#).is_err());
}

#[test]
fn mobile_runtime_tool_visibility_roundtrip() {
    let (paths, dir) = temp_paths();
    let (collector, _rx) = Collector::new();
    let scripted = ScriptedProvider::new(vec![]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted)),
        ..Default::default()
    };
    let runtime = MobileRuntime::create_with(paths, collector, opts).unwrap();

    // list_tools：注册表全部工具，按 name 排序。
    let tools: serde_json::Value = serde_json::from_str(&runtime.list_tools_json()).unwrap();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"mobile.ping"),
        "mobile.ping 应注册: {names:?}"
    );
    // bash 工具按后端探测注册（桌面测试环境可能缺席），只验排序与字段。
    assert!(
        names.windows(2).all(|w| w[0] <= w[1]),
        "按 name 排序: {names:?}"
    );
    for t in tools.as_array().unwrap() {
        assert!(
            t["description"].is_string(),
            "name+description 字段固定: {t}"
        );
        // input_schema 键恒存在（无 schema 时为 {}）；且必须是对象。
        assert!(
            t["input_schema"].is_object(),
            "input_schema 应为对象（可空 schema 输出 {{}}）: {t}"
        );
        // server 键恒存在（tool-approval P1b：审批作用域；非 MCP 工具
        // 为 JSON null——本 harness 全部为 host/native 工具）。
        assert!(
            t.get("server").is_some() && t["server"].is_null(),
            "server 恒存在、非 MCP 为 null: {t}"
        );
    }
    // 至少一个工具的参数 schema 非空（mobile.ping 恒注册且带完整 schema）。
    assert!(
        tools
            .as_array()
            .unwrap()
            .iter()
            .any(|t| { t["input_schema"].as_object().is_some_and(|o| !o.is_empty()) }),
        "应存在参数 schema 非空的工具: {tools}"
    );

    // 初始可见性 = bootstrap 默认白名单、无 hidden。
    let vis: serde_json::Value =
        serde_json::from_str(&runtime.get_agent_tool_visibility_json()).unwrap();
    assert_eq!(
        vis["tools"],
        serde_json::json!(["mobile.ping", "bash", "termux_bash", "output_query", "notify"])
    );
    assert_eq!(vis["hidden_tools"], serde_json::json!([]));

    // 全量覆写：白名单清空（= 暴露全部）+ 隐藏 termux_bash。
    let ret: serde_json::Value = serde_json::from_str(
        &runtime.set_agent_tool_visibility_json(vec![], vec!["termux_bash".into()]),
    )
    .unwrap();
    assert_eq!(ret["ok"], serde_json::json!(true), "覆写应成功: {ret}");

    // 回读：运行时树已热替换为新名单。
    let vis: serde_json::Value =
        serde_json::from_str(&runtime.get_agent_tool_visibility_json()).unwrap();
    assert_eq!(vis["tools"], serde_json::json!([]));
    assert_eq!(vis["hidden_tools"], serde_json::json!(["termux_bash"]));

    // 落盘检查：root.md 外科手术式改写——两个键替换，其余 frontmatter
    // 与正文保留。
    let md = std::fs::read_to_string(dir.path().join("config/agents/root.md")).unwrap();
    assert!(md.contains("id: root\n"));
    assert!(md.contains("name: OpenSlate\n"));
    assert!(md.contains("model: main\n"));
    assert!(md.contains("tools: []\n"));
    assert!(md.contains("hidden_tools:\n    - termux_bash\n"));
    assert!(md.contains("You are a helpful AI assistant."));

    runtime.shutdown();
}

#[test]
fn mobile_runtime_agent_persona_roundtrip() {
    let (paths, dir) = temp_paths();
    let (collector, rx) = Collector::new();
    let scripted = ScriptedProvider::new(vec![text_response("好。"), text_response("好。")]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted.clone())),
        ..Default::default()
    };
    let runtime = MobileRuntime::create_with(paths, collector.clone(), opts).unwrap();

    // 初始正文 = bootstrap 默认（get 与每回合 system prompt 同源）。
    assert_eq!(
        runtime.get_agent_persona(),
        "You are a helpful AI assistant."
    );

    // 全量覆写（入参带冗余空白）→ 返回规范化生效文本；get 读回一致。
    let root_md_path = dir.path().join("config/agents/root.md");
    let md_before = std::fs::read_to_string(&root_md_path).unwrap();
    let effective = runtime.set_agent_persona("  你是移动端编码专家。\n").unwrap();
    assert_eq!(effective, "你是移动端编码专家。");
    assert_eq!(runtime.get_agent_persona(), "你是移动端编码专家。");

    // 落盘检查：frontmatter（含 tools 白名单）逐字节不变，正文已替换。
    let md_after = std::fs::read_to_string(&root_md_path).unwrap();
    let fm_before = &md_before[..md_before.find("\n---\n").expect("seed closing")];
    let fm_after = &md_after[..md_after.find("\n---\n").expect("closing")];
    assert_eq!(fm_before, fm_after, "frontmatter 必须逐字节保留");
    assert!(md_after.ends_with("---\n你是移动端编码专家。\n"));

    // set 后再走工具可见性外科改写仍成功（两种 frontmatter/body 手术互不干扰）。
    let ret: serde_json::Value = serde_json::from_str(
        &runtime.set_agent_tool_visibility_json(vec![], vec!["termux_bash".into()]),
    )
    .unwrap();
    assert_eq!(ret["ok"], serde_json::json!(true), "tool visibility set 应成功: {ret}");
    // persona 正文不受工具可见性改写影响。
    assert_eq!(runtime.get_agent_persona(), "你是移动端编码专家。");

    // 热生效：set 后的新回合，runner 发出的 system prompt 用新正文
    //（skills catalog 由 runner 在其上追加——只断言正文被包含）。
    runtime
        .send(r#"{"type":"submit","text":"继续"}"#)
        .unwrap();
    collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));
    let seen = scripted.seen_system_prompts();
    let last = seen.last().expect("provider 应已收到请求").as_str();
    assert!(
        last.contains("你是移动端编码专家。"),
        "新回合 system prompt 应用新正文: {last}"
    );

    // 空正文 → 回默认文案（get/set 往返一致）。
    let effective = runtime.set_agent_persona("   \n").unwrap();
    assert_eq!(effective, "You are a helpful AI assistant.");
    assert_eq!(
        runtime.get_agent_persona(),
        "You are a helpful AI assistant."
    );

    runtime.shutdown();
}

// ═══ notify 工具全链路（S1+S1b；设计 §8.1 + oracle 补充三条）══════════

/// 独立小 runtime 打开 data_dir 的库（runtime 收束后查询复用；
/// runtime.rs 测试的 scratch_store 同款）。
fn scratch_store(
    paths: &MobilePaths,
) -> (
    tokio::runtime::Runtime,
    openslate_store_sqlite::store::SqliteStore,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let db = paths.data_dir.join("openslate.db").display().to_string();
    let store = rt
        .block_on(async {
            let s = openslate_store_sqlite::store::SqliteStore::new(&db).await?;
            s.run_migrations().await?;
            anyhow::Ok(s)
        })
        .unwrap();
    (rt, store)
}

/// 模型点名叫 notify 的一步脚本（tool_call 参数可覆盖）。
fn notify_call_step(call_id: &str, args: serde_json::Value) -> Vec<ModelStreamEvent> {
    vec![ModelStreamEvent::Done(ModelResponse {
        reasoning_content: None,
        content: None,
        tool_calls: vec![ToolCall {
            id: ToolCallId(call_id.into()),
            name: "notify".into(),
            arguments: args,
        }],
        usage: None,
        finish_reason: Some("tool_calls".into()),
    })]
}

/// 轮询 DB 直到条件满足（on_cancel_all 的行收卷是后台异步任务）。
fn eventually<T>(mut probe: impl FnMut() -> Option<T>, deadline: Duration) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(
            start.elapsed() < deadline,
            "条件在 {deadline:?} 内未满足"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 等待回合终态事件（turn_ok 或 turn_error——cancel 路径两态皆可能）。
fn wait_turn_terminal(
    collector: &Collector,
    rx: &std_mpsc::Receiver<()>,
    deadline: Duration,
) -> serde_json::Value {
    let start = Instant::now();
    loop {
        let all = collector.events.lock().unwrap().clone();
        if let Some(ev) = all
            .iter()
            .rev()
            .find(|e| e["type"] == "turn_ok" || e["type"] == "turn_error")
        {
            return ev.clone();
        }
        assert!(
            start.elapsed() < deadline,
            "回合终态超时；已收到: {:?}",
            all.iter()
                .map(|e| e.get("type").and_then(|t| t.as_str()).unwrap_or("?"))
                .collect::<Vec<_>>()
        );
        rx.recv_timeout(Duration::from_millis(200)).ok();
    }
}

/// 非 wait：信封 → 宿主 resolve posted:true → ToolOutput 入历史。
#[test]
fn notify_roundtrip() {
    let (paths, _dir) = temp_paths();
    let (collector, rx) = Collector::new();
    let scripted = ScriptedProvider::new(vec![
        notify_call_step(
            "call-n1",
            serde_json::json!({
                "id": "export-done", "title": "导出完成", "message": "已导出 128 张照片",
                "kind": "normal"
            }),
        ),
        text_response("已经通知你了。"),
    ]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted)),
        ..Default::default()
    };
    let runtime = MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();

    runtime.send(r#"{"type":"submit","text":"通知我导出结果"}"#).unwrap();
    let events = collector.wait_for(&rx, "host_call_requested", Duration::from_secs(15));
    let envelope = events
        .iter()
        .find(|e| e["type"] == "host_call_requested")
        .unwrap();
    assert_eq!(envelope["tool"].as_str().unwrap(), "notify");
    assert_eq!(envelope["args"]["v"], 1, "信封版本 v:1");
    assert_eq!(envelope["args"]["id"], "export-done");
    assert_eq!(envelope["args"]["kind"], "normal");
    assert!(envelope["args"].get("wait").is_none(), "非 wait 省键");
    let id = envelope["id"].as_u64().unwrap();
    assert!(runtime.resolve_host_call(
        id,
        true,
        r#"{"posted":true,"id":"export-done","kind":"normal","channel":"agent_messages"}"#.to_owned()
    ));

    let events = collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "tool_start" && e["name"] == "notify"),
        "应有 notify 的 tool_start"
    );

    // 工具结果入历史（posted 文案 + 结构化 JSON 并存）。
    let (srt, store) = scratch_store(&paths);
    let msgs = srt.block_on(async {
        openslate_store_sqlite::recorder::RunRecorder::load_messages(&store, &runtime.current_run_id().unwrap())
            .await
            .unwrap()
    });
    let tool_msg = msgs
        .iter()
        .find(|m| m.role == openslate_core::types::MessageRole::Tool
            && m.tool_call_id.as_ref().map(|i| i.0.as_str()) == Some("call-n1"))
        .expect("notify 工具结果入历史");
    assert!(
        tool_msg.content.contains("Notification posted (id=export-done") &&
            tool_msg.content.contains("\"posted\":true"),
        "posted 文案: {}",
        tool_msg.content
    );
    runtime.shutdown();
}

/// 权限未授予：posted:false 是业务结果（ok=true 语义），模型收到指引
/// 文案，回合正常完成。
#[test]
fn notify_permission_denied() {
    let (paths, _dir) = temp_paths();
    let (collector, rx) = Collector::new();
    let scripted = ScriptedProvider::new(vec![
        notify_call_step(
            "call-n2",
            serde_json::json!({"title": "任务完成", "message": "批量重命名完成"}),
        ),
        text_response("好的，我在回复里说明。"),
    ]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted)),
        ..Default::default()
    };
    let runtime = MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();

    runtime.send(r#"{"type":"submit","text":"通知我"}"#).unwrap();
    let events = collector.wait_for(&rx, "host_call_requested", Duration::from_secs(15));
    let envelope = events
        .iter()
        .find(|e| e["type"] == "host_call_requested")
        .unwrap();
    let id = envelope["id"].as_u64().unwrap();
    assert!(runtime.resolve_host_call(
        id,
        true,
        r#"{"posted":false,"reason":"permission_denied"}"#.to_owned()
    ));
    collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));

    let (srt, store) = scratch_store(&paths);
    let msgs = srt.block_on(async {
        openslate_store_sqlite::recorder::RunRecorder::load_messages(&store, &runtime.current_run_id().unwrap())
            .await
            .unwrap()
    });
    let tool_msg = msgs
        .iter()
        .find(|m| m.tool_call_id.as_ref().map(|i| i.0.as_str()) == Some("call-n2"))
        .expect("工具结果入历史");
    assert!(
        tool_msg.content.contains("未授予通知权限"),
        "permission_denied 指引文案: {}",
        tool_msg.content
    );
    runtime.shutdown();
}

/// wait:true 即时路径：点按钮 resolve → action outcome 入历史 + 行关卷。
#[test]
fn notify_wait_immediate_path_roundtrip() {
    let (paths, _dir) = temp_paths();
    let (collector, rx) = Collector::new();
    let scripted = ScriptedProvider::new(vec![
        notify_call_step(
            "call-n3",
            serde_json::json!({
                "title": "确认删除", "message": "要删除 3 个旧导出文件吗？",
                "kind": "important", "wait": true, "timeout": 60,
                "actions": [
                    {"label": "删除", "style": "positive"},
                    {"label": "取消", "style": "destructive"},
                    {"label": "回复", "reply": true}
                ]
            }),
        ),
        text_response("好，已删除。"),
    ]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted)),
        ..Default::default()
    };
    let runtime = MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();

    runtime.send(r#"{"type":"submit","text":"问我要不要删"}"#).unwrap();
    let events = collector.wait_for(&rx, "host_call_requested", Duration::from_secs(15));
    let envelope = events
        .iter()
        .find(|e| e["type"] == "host_call_requested")
        .unwrap();
    // 信封契约（D9：run_id 内嵌 args）。
    assert_eq!(envelope["tool"].as_str().unwrap(), "notify");
    assert_eq!(envelope["args"]["wait"], true);
    assert_eq!(envelope["args"]["timeout"], 60);
    assert_eq!(
        envelope["args"]["run_id"],
        runtime.current_run_id().unwrap(),
        "D9：run_id 内嵌 args（信封顶层无 run_id）"
    );
    assert!(envelope.get("run_id").is_none(), "信封顶层不带 run_id");
    let id = envelope["id"].as_u64().unwrap();
    assert!(runtime.resolve_host_call(
        id,
        true,
        r#"{"outcome":"action","label":"删除","value":"用户在通知上点了「删除」"}"#.to_owned()
    ));
    collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));

    // 工具结果 + 行状态（action 关卷；request_id = host call id）。
    let (srt, store) = scratch_store(&paths);
    let (tool_content, row_status) = srt.block_on(async {
        let msgs = openslate_store_sqlite::recorder::RunRecorder::load_messages(
            &store,
            &runtime.current_run_id().unwrap(),
        )
        .await
        .unwrap();
        let content = msgs
            .iter()
            .find(|m| m.tool_call_id.as_ref().map(|i| i.0.as_str()) == Some("call-n3"))
            .expect("wait 工具结果入历史")
            .content
            .clone();
        let row = store.get_pending_notification(&id.to_string()).await.unwrap();
        (content, row.map(|r| r.status))
    });
    assert!(
        tool_content.contains("用户在通知上点了「删除」") &&
            tool_content.contains("\"outcome\":\"action\""),
        "action 文案: {tool_content}"
    );
    assert_eq!(row_status.as_deref(), Some("action"), "行按 outcome 同名关卷");
    runtime.shutdown();
}

/// wait 超时：router 按入参 1s 超时 → timeout 文案入历史 + 行 timeout。
#[test]
fn notify_wait_timeout() {
    let (paths, _dir) = temp_paths();
    let (collector, rx) = Collector::new();
    let scripted = ScriptedProvider::new(vec![
        notify_call_step(
            "call-n4",
            serde_json::json!({
                "title": "等你确认", "message": "继续吗？", "wait": true, "timeout": 1,
                "actions": [{"label": "继续"}, {"label": "停止"}]
            }),
        ),
        text_response("用户不在，我按计划继续。"),
    ]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted)),
        ..Default::default()
    };
    let runtime = MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();

    runtime.send(r#"{"type":"submit","text":"确认一下"}"#).unwrap();
    let events = collector.wait_for(&rx, "host_call_requested", Duration::from_secs(15));
    let envelope = events
        .iter()
        .find(|e| e["type"] == "host_call_requested")
        .unwrap();
    let id = envelope["id"].as_u64().unwrap();
    // 不 resolve：1s 后 router 超时，工具回填 timeout 文案、回合继续。
    collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));

    let (srt, store) = scratch_store(&paths);
    let (tool_content, row_status) = srt.block_on(async {
        let msgs = openslate_store_sqlite::recorder::RunRecorder::load_messages(
            &store,
            &runtime.current_run_id().unwrap(),
        )
        .await
        .unwrap();
        let content = msgs
            .iter()
            .find(|m| m.tool_call_id.as_ref().map(|i| i.0.as_str()) == Some("call-n4"))
            .expect("timeout 工具结果入历史")
            .content
            .clone();
        let row = store.get_pending_notification(&id.to_string()).await.unwrap();
        (content, row.map(|r| r.status))
    });
    assert!(
        tool_content.contains("用户在 1 秒内未回应通知") &&
            tool_content.contains("\"outcome\":\"timeout\""),
        "timeout 文案: {tool_content}"
    );
    assert_eq!(row_status.as_deref(), Some("timeout"), "行按 timeout 关卷");
    runtime.shutdown();
}

/// 竞态（oracle 补充）：timeout 已获胜后的迟到回复 = 原子 UPDATE 0 行
/// → no-op + notice，不注入新消息、不二次续跑。
#[test]
fn notify_wait_timeout_then_late_reply_noop() {
    let (paths, _dir) = temp_paths();
    let (collector, rx) = Collector::new();
    let scripted = ScriptedProvider::new(vec![
        notify_call_step(
            "call-n5",
            serde_json::json!({
                "title": "等你", "message": "回我", "wait": true, "timeout": 1,
                "actions": [{"label": "好"}]
            }),
        ),
        text_response("超时继续。"),
    ]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted)),
        ..Default::default()
    };
    let runtime = MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();

    runtime.send(r#"{"type":"submit","text":"等回复"}"#).unwrap();
    let events = collector.wait_for(&rx, "host_call_requested", Duration::from_secs(15));
    let envelope = events
        .iter()
        .find(|e| e["type"] == "host_call_requested")
        .unwrap();
    let id = envelope["id"].as_u64().unwrap();
    let run_id = runtime.current_run_id().unwrap();
    collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));

    // 迟到回复：在途 resolve 已失效（id 出表）→ 迟到路径 → 行非 pending
    // → no-op + 定向 notice。
    assert!(!runtime.resolve_host_call(
        id,
        true,
        r#"{"outcome":"action","label":"好"}"#.to_owned()
    ), "失效 id resolve 应 no-op");
    runtime
        .send(&format!(
            r#"{{"type":"notification_reply","request_id":"{id}","run_id":"{run_id}","outcome":"action","label":"好"}}"#
        ))
        .unwrap();
    let events = collector.wait_for(&rx, "notice", Duration::from_secs(10));
    let notice = events
        .iter()
        .rev()
        .find(|e| e["type"] == "notice")
        .expect("notice 已发出");
    assert!(
        notice["text"].as_str().unwrap().contains("已被应答或已关闭"),
        "首答者赢语义（timeout 先胜）: {notice}"
    );

    // 行仍 timeout（迟到回复不改状态、不重开）。
    let (srt, store) = scratch_store(&paths);
    let status = srt.block_on(async {
        store
            .get_pending_notification(&id.to_string())
            .await
            .unwrap()
            .unwrap()
            .status
    });
    assert_eq!(status, "timeout");
    runtime.shutdown();
}

/// App 内停止回合（cancel）：在途 wait 调用被引擎合成 cancelled 结果、
/// 行收 superseded；之后的迟到 notification_reply 为 no-op。
#[test]
fn notify_wait_cancel_superseded() {
    let (paths, _dir) = temp_paths();
    let (collector, rx) = Collector::new();
    let scripted = ScriptedProvider::new(vec![notify_call_step(
        "call-n6",
        serde_json::json!({
            "title": "等待确认", "message": "继续？", "wait": true, "timeout": 60,
            "actions": [{"label": "继续"}, {"label": "停止任务", "behavior": "cancel_turn"}]
        }),
    )]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted)),
        ..Default::default()
    };
    let runtime = MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();

    runtime.send(r#"{"type":"submit","text":"确认"}"#).unwrap();
    let events = collector.wait_for(&rx, "host_call_requested", Duration::from_secs(15));
    let envelope = events
        .iter()
        .find(|e| e["type"] == "host_call_requested")
        .unwrap();
    let id = envelope["id"].as_u64().unwrap();
    let run_id = runtime.current_run_id().unwrap();

    // 用户停止回合。
    runtime.send(r#"{"type":"cancel"}"#).unwrap();
    let _ = wait_turn_terminal(&collector, &rx, Duration::from_secs(20));

    // 行被 on_cancel_all 收卷为 superseded（后台异步，轮询 DB）。
    let (srt, store) = scratch_store(&paths);
    let status = eventually(
        || {
            srt.block_on(async {
                store
                    .get_pending_notification(&id.to_string())
                    .await
                    .unwrap()
                    .map(|r| r.status)
            })
        },
        Duration::from_secs(10),
    );
    assert_eq!(status, "superseded", "cancel 后 wait 行关卷 superseded");

    // 迟到 cancelled 上行（边角 2 决策的防御通路：行已关 → no-op）。
    runtime
        .send(&format!(
            r#"{{"type":"notification_reply","request_id":"{id}","run_id":"{run_id}","outcome":"cancelled"}}"#
        ))
        .unwrap();
    let events = collector.wait_for(&rx, "notice", Duration::from_secs(10));
    let notice = events
        .iter()
        .rev()
        .find(|e| e["type"] == "notice")
        .expect("notice 已发出");
    assert!(
        notice["text"].as_str().unwrap().contains("已被应答或已关闭"),
        "行已关卷，迟到 cancelled 为 no-op: {notice}"
    );
    runtime.shutdown();
}

/// wait 划掉通知（dismissed）即时路径。
#[test]
fn notify_wait_dismissed() {
    let (paths, _dir) = temp_paths();
    let (collector, rx) = Collector::new();
    let scripted = ScriptedProvider::new(vec![
        notify_call_step(
            "call-n7",
            serde_json::json!({
                "title": "要不要继续", "message": "划掉=默认继续", "wait": true, "timeout": 60,
                "actions": [{"label": "继续"}, {"label": "停"}]
            }),
        ),
        text_response("用户没选，我按假设继续。"),
    ]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted)),
        ..Default::default()
    };
    let runtime = MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();

    runtime.send(r#"{"type":"submit","text":"问一下"}"#).unwrap();
    let events = collector.wait_for(&rx, "host_call_requested", Duration::from_secs(15));
    let envelope = events
        .iter()
        .find(|e| e["type"] == "host_call_requested")
        .unwrap();
    let id = envelope["id"].as_u64().unwrap();
    assert!(runtime.resolve_host_call(id, true, r#"{"outcome":"dismissed"}"#.to_owned()));
    collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));

    let (srt, store) = scratch_store(&paths);
    let (tool_content, row_status) = srt.block_on(async {
        let msgs = openslate_store_sqlite::recorder::RunRecorder::load_messages(
            &store,
            &runtime.current_run_id().unwrap(),
        )
        .await
        .unwrap();
        let content = msgs
            .iter()
            .find(|m| m.tool_call_id.as_ref().map(|i| i.0.as_str()) == Some("call-n7"))
            .expect("dismissed 工具结果入历史")
            .content
            .clone();
        let row = store.get_pending_notification(&id.to_string()).await.unwrap();
        (content, row.map(|r| r.status))
    });
    assert!(
        tool_content.contains("用户划掉了通知"),
        "dismissed 文案: {tool_content}"
    );
    assert_eq!(row_status.as_deref(), Some("dismissed"));
    runtime.shutdown();
}

/// 杀进程恢复 + 首答者赢（dismissed 变体）：预置崩溃现场（悬挂 notify
/// 调用的 run + pending 行）→ 重启恢复 → notification_reply 迟到补全
/// 续跑 → turn_ok；重复应答输掉首答者赢。
#[test]
fn notify_wait_restart_recovery_dismissed() {
    let (paths, _dir) = temp_paths();
    paths.ensure_layout().unwrap();

    let run_id_str = "notify-recover-run";
    let tool_call_id = "call-notify-9";
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    {
        let (srt, store) = scratch_store(&paths);
        srt.block_on(async {
            store
                .insert_run(run_id_str, Some("通知恢复"), "root", "interrupted", "{}", now_ms - 60_000)
                .await
                .unwrap();
            store
                .insert_execution_node(
                    &format!("{run_id_str}-persist"),
                    run_id_str,
                    "root",
                    None,
                    None,
                    "running",
                    "{}",
                    now_ms - 60_000,
                )
                .await
                .unwrap();
            use openslate_core::types::{Message, MessageRole, ToolCall, ToolCallId};
            let user_msg = Message {
                attachments: None,
                role: MessageRole::User,
                content: "发个确认通知".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            };
            let assistant_msg = Message {
                attachments: None,
                role: MessageRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                name: None,
                tool_calls: Some(vec![ToolCall {
                    id: ToolCallId(tool_call_id.into()),
                    name: "notify".into(),
                    arguments: serde_json::json!({
                        "title": "确认", "message": "继续吗", "wait": true,
                        "actions": [{"label": "继续"}]
                    }),
                }]),
                reasoning_content: None,
            };
            store
                .insert_message(
                    &format!("msg-{run_id_str}-1"),
                    run_id_str,
                    &format!("{run_id_str}-persist"),
                    Some("root"),
                    "user",
                    &openslate_store_sqlite::codec::encode_content(
                        &serde_json::to_string(&user_msg).unwrap(),
                    ),
                    None,
                    1,
                    now_ms - 59_000,
                )
                .await
                .unwrap();
            store
                .insert_message(
                    &format!("msg-{run_id_str}-2"),
                    run_id_str,
                    &format!("{run_id_str}-persist"),
                    Some("root"),
                    "assistant",
                    &openslate_store_sqlite::codec::encode_content(
                        &serde_json::to_string(&assistant_msg).unwrap(),
                    ),
                    None,
                    2,
                    now_ms - 58_000,
                )
                .await
                .unwrap();
            // 完整信封快照落行（含 v 与 run_id，D2/D9）。
            store
                .insert_pending_notification(
                    "77",
                    tool_call_id,
                    run_id_str,
                    r#"{"v":1,"action":"post","title":"确认","message":"继续吗","wait":true,"timeout":300,"run_id":"notify-recover-run"}"#,
                    now_ms - 58_000,
                )
                .await
                .unwrap();
        });
    }

    let (collector, rx) = Collector::new();
    let scripted = ScriptedProvider::new(vec![text_response("用户划掉了通知，我按原计划继续。")]);
    let opts = RuntimeOptions {
        provider_factory: Some(factory_for(scripted)),
        ..Default::default()
    };
    let runtime = MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();

    // 迟到 dismissed：notification_reply → 原子关卷 → 补全原 tool result
    // → resume_from_tool_result 续跑 → turn_ok。
    runtime
        .send(&format!(
            r#"{{"type":"notification_reply","request_id":"77","run_id":"{run_id_str}","outcome":"dismissed"}}"#
        ))
        .unwrap();
    collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));

    // 首答者赢：重复应答 → 定向 notice，不二次续跑。
    runtime
        .send(&format!(
            r#"{{"type":"notification_reply","request_id":"77","run_id":"{run_id_str}","outcome":"action","label":"继续"}}"#
        ))
        .unwrap();
    let events = collector.wait_for(&rx, "notice", Duration::from_secs(10));
    let notice = events
        .iter()
        .rev()
        .find(|e| e["type"] == "notice")
        .expect("notice 已发出");
    assert!(
        notice["text"].as_str().unwrap().contains("已被应答或已关闭"),
        "首答者赢语义: {notice}"
    );

    // 行 dismissed；消息历史里迟到补全的 tool result 与即时路径同形；
    // 续跑后的最终回复已入库。
    let (srt, store) = scratch_store(&paths);
    let (row_status, msgs) = srt.block_on(async {
        let row = store
            .get_pending_notification("77")
            .await
            .unwrap()
            .expect("行存在");
        let msgs = openslate_store_sqlite::recorder::RunRecorder::load_messages(&store, run_id_str)
            .await
            .unwrap();
        (row.status, msgs)
    });
    assert_eq!(row_status, "dismissed");
    let tool_msg = msgs
        .iter()
        .find(|m| m.role == openslate_core::types::MessageRole::Tool
            && m.tool_call_id.as_ref().map(|i| i.0.as_str()) == Some(tool_call_id))
        .expect("迟到 dismissed 已补全 tool result");
    assert!(
        tool_msg.content.contains("用户划掉了通知")
            && tool_msg.content.contains("\"outcome\":\"dismissed\""),
        "与即时路径完全同形（迟到等价不变量）: {}",
        tool_msg.content
    );
    assert!(
        msgs.iter().any(|m| m.role == openslate_core::types::MessageRole::Assistant
            && m.content.contains("我按原计划继续")),
        "续跑后的最终回复已入库"
    );
    runtime.shutdown();
}
