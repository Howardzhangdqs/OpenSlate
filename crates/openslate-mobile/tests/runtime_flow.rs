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
}

impl ScriptedProvider {
    fn new(scripts: Vec<Vec<ModelStreamEvent>>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts.into())),
        }
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn generate(&self, _request: GenerateRequest) -> Result<ModelResponse, ProviderError> {
        Err(ProviderError::ServerError(500))
    }

    async fn generate_stream(
        &self,
        _request: GenerateRequest,
    ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
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
    Arc::new(move |_config, _alias| {
        Ok(Box::new(provider.clone()) as Box<dyn ModelProvider>)
    })
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
    fn wait_for(&self, rx: &std_mpsc::Receiver<()>, want_type: &str, deadline: Duration) -> Vec<serde_json::Value> {
        let start = Instant::now();
        loop {
            let all = self.events.lock().unwrap().clone();
            if all.iter().any(|e| e.get("type").and_then(|t| t.as_str()) == Some(want_type)) {
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
    let i_start = types.iter().position(|t| t == &"request_start").expect("request_start");
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
    runtime.send(r#"{"type":"submit","text":"ping 一下宿主"}"#).unwrap();

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
    assert!(tool_start_seen, "应有 mobile.ping 的 tool_start: {events:?}");

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
    runtime
        .send(r#"{"type":"hello","proto":999}"#)
        .unwrap();
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
