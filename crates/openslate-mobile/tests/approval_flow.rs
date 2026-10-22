//! 集成测试：工具调用批准六路径 + 作用域语义（tool-approval P1b 出口
//! 标准，`.slim/deepwork/tool-approval.md`「实施计划」）。
//!
//! harness 与 tests/runtime_flow.rs 同构（ScriptedProvider 流式脚本 +
//! 事件采集），另加两件专用件：
//!
//! - **按别名分发的测试 factory**：`[capabilities].review` 绑定到
//!   `models.review` 条目 → main 别名走主脚本（generate_stream）、
//!   review 别名走 `ReviewProvider` 桩（generate 非流式：逐条弹答案 /
//!   可挂起 / 可报错），两侧互不干扰、精确断言「审查是否被调用」。
//! - **`ProbeTool` 探针工具**：直接执行（非 host call），执行计数 +
//!   可配 namespace（`Some("termux")` + `__` 聚合名 → `server_slug_of`
//!   反解出下游 slug，session_mcp / always_mcp 路径用）。
//!
//! 六路径（oracle 出口标准）：
//! 1. Manual 直问（无审查调用）；2. Auto + Low 跳过（无审批无审查）；
//! 3. safe 静默放行；4. risky 弹人（note/server/trigger 断言）；
//! 5. 审查失败回退弹人；6. 审查超时回退弹人（200ms builder 超时）。
//!
//! 另补：session_mcp 会话作用域直通、always_tool 持久化 + 后续直通、
//! NewSession 清会话作用域。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use openslate_core::error::{ProviderError, ToolError};
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::tool::Tool;
use openslate_core::types::{
    ModelResponse, ModelStreamEvent, ToolCall, ToolCallId, ToolOutput, ToolOutputStatus, Usage,
};
use openslate_mobile::{EventCallback, MobilePaths, MobileRuntime, RuntimeOptions};
use openslate_session::state::ProviderFactory;

// ── harness：主对话 ScriptedProvider（同 runtime_flow.rs）──────────────

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
        // 主对话走 generate_stream；generate 被调到即视为异常（审查误绑
        // 到主别名时测试会在这里暴露）。
        Err(ProviderError::ServerError(500))
    }

    async fn generate_stream(
        &self,
        _request: GenerateRequest,
    ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
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

// ── harness：审查 provider 桩（generate 非流式）───────────────────────

/// 审查桩：`answers` 逐条弹出（Ok = 审查器原文 / Err = 请求失败）；
/// `hang` = 挂起不返回（超时路径）；`calls` 计数供「是否进 AI 审查」
/// 断言。
#[derive(Clone)]
struct ReviewProvider {
    answers: Arc<Mutex<VecDeque<Result<String, ProviderError>>>>,
    hang: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}

impl ReviewProvider {
    fn with_answers(answers: Vec<Result<String, ProviderError>>) -> Self {
        Self {
            answers: Arc::new(Mutex::new(answers.into())),
            hang: Arc::new(AtomicBool::new(false)),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn hanging() -> Self {
        Self {
            answers: Arc::new(Mutex::new(VecDeque::new())),
            hang: Arc::new(AtomicBool::new(true)),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ModelProvider for ReviewProvider {
    async fn generate(&self, _request: GenerateRequest) -> Result<ModelResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.hang.load(Ordering::SeqCst) {
            // 有效挂起（远超测试超时预算）。
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
        match self.answers.lock().unwrap().pop_front() {
            Some(Ok(content)) => Ok(ModelResponse {
                content: Some(content),
                reasoning_content: None,
                tool_calls: Vec::new(),
                usage: None,
                finish_reason: Some("stop".into()),
            }),
            Some(Err(e)) => Err(e),
            None => Err(ProviderError::ServerError(500)),
        }
    }

    fn provider_name(&self) -> &str {
        "review-stub"
    }
}

/// 按别名分发的测试 factory：`review` 别名（[capabilities].review 绑定）
/// 走审查桩，其余（main / 会话模型）走主脚本。
fn alias_factory(main: ScriptedProvider, review: ReviewProvider) -> ProviderFactory {
    Arc::new(move |_config, alias| {
        if alias == "review" {
            Ok(Box::new(review.clone()) as Box<dyn ModelProvider>)
        } else {
            Ok(Box::new(main.clone()) as Box<dyn ModelProvider>)
        }
    })
}

// ── harness：探针工具 / 事件采集 / 装配脚手架 ─────────────────────────

/// 探针工具：直接执行（非 host call），`runs` 计数断言「是否真的跑了」。
/// `namespace = Some(...)` + `__` 聚合名 → 审批 server 作用域（MCP 形态）。
struct ProbeTool {
    name: String,
    namespace: Option<String>,
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn namespace(&self) -> Option<String> {
        self.namespace.clone()
    }
    fn description(&self) -> &str {
        "integration-test probe tool (approval fixture)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }
    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            attachments: None,
            bytes: 2,
            content: "ok".to_owned(),
            duration_ms: 1,
            status: ToolOutputStatus::Success,
        })
    }
}

struct Collector {
    events: Mutex<Vec<serde_json::Value>>,
    notify: std_mpsc::Sender<()>,
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
            }),
            rx,
        )
    }

    /// 阻塞等待直到出现匹配 type 的第 n 条（超时失败）。
    fn wait_for_count(
        &self,
        rx: &std_mpsc::Receiver<()>,
        want_type: &str,
        n: usize,
        deadline: Duration,
    ) -> Vec<serde_json::Value> {
        let start = Instant::now();
        loop {
            let all = self.events.lock().unwrap().clone();
            let hit = all
                .iter()
                .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some(want_type))
                .count();
            if hit >= n {
                return all;
            }
            assert!(
                start.elapsed() < deadline,
                "等待 {n} 条 {want_type} 事件超时（已有 {hit}）；已收到: {:?}",
                all.iter()
                    .map(|e| e.get("type").and_then(|t| t.as_str()).unwrap_or("?"))
                    .collect::<Vec<_>>()
            );
            rx.recv_timeout(Duration::from_millis(200)).ok();
        }
    }

    fn wait_for(&self, rx: &std_mpsc::Receiver<()>, want_type: &str, deadline: Duration) -> Vec<serde_json::Value> {
        self.wait_for_count(rx, want_type, 1, deadline)
    }
}

/// 装配脚手架：默认布局 + `[capabilities].review` 绑定到独立
/// `models.review` 条目（审查模型与会话模型分离，factory 才能按别名
/// 分发）。
fn approval_test_paths() -> (MobilePaths, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let paths = MobilePaths {
        config_dir: dir.path().join("config"),
        workspace_dir: dir.path().join("workspace"),
        data_dir: dir.path().join("data"),
        cache_dir: dir.path().join("cache"),
    };
    paths.ensure_layout().unwrap();
    // 追加 review 模型条目 + capability 绑定（`title = "fast"` 在默认
    // toml 中唯一，锚定插入 capabilities 表内）。
    let config = paths.config_file();
    let mut body = std::fs::read_to_string(&config).unwrap();
    assert!(
        body.contains("title = \"fast\""),
        "默认 toml 形状变化，锚点失效：{body}"
    );
    body = body.replace(
        "title = \"fast\"",
        "title = \"fast\"\nreview = \"review\"",
    );
    body.push_str("\n[models.review]\nprovider = \"zhipu\"\nmodel = \"glm-review\"\n");
    std::fs::write(&config, body).unwrap();
    (paths, dir)
}

fn tool_call_response(call_id: &str, tool: &str) -> Vec<ModelStreamEvent> {
    vec![ModelStreamEvent::Done(ModelResponse {
        content: None,
        tool_calls: vec![ToolCall {
            id: ToolCallId(call_id.into()),
            name: tool.into(),
            arguments: serde_json::json!({"why": "fixture"}),
        }],
        reasoning_content: None,
        usage: None,
        finish_reason: Some("tool_calls".into()),
    })]
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

/// 一个回合的完整主脚本：工具调用 → 收尾文本。
fn turn_scripts(call_id: &str, tool: &str) -> Vec<Vec<ModelStreamEvent>> {
    vec![tool_call_response(call_id, tool), text_response("完成")]
}

fn assemble(
    paths: MobilePaths,
    collector: Arc<Collector>,
    main: ScriptedProvider,
    review: ReviewProvider,
    probes: Vec<ProbeTool>,
    review_timeout: Duration,
) -> Arc<MobileRuntime> {
    let mut extra: Vec<Box<dyn Tool>> = Vec::new();
    for probe in probes {
        extra.push(Box::new(probe));
    }
    let opts = RuntimeOptions {
        provider_factory: Some(alias_factory(main, review.clone())),
        extra_tools: extra,
        approval_review_timeout: review_timeout,
        ..Default::default()
    };
    MobileRuntime::create_with(paths, collector, opts).unwrap()
}

/// 取最新一条指定类型事件。
fn latest<'a>(events: &'a [serde_json::Value], ty: &str) -> &'a serde_json::Value {
    events
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(|t| t.as_str()) == Some(ty))
        .unwrap_or_else(|| panic!("缺少 {ty} 事件: {events:?}"))
}

fn count_of(events: &[serde_json::Value], ty: &str) -> usize {
    events
        .iter()
        .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some(ty))
        .count()
}

/// 应答当前审批并等回合完成。
fn answer_and_finish(
    runtime: &MobileRuntime,
    collector: &Collector,
    rx: &std_mpsc::Receiver<()>,
    events: &[serde_json::Value],
    choice: &str,
) {
    let id = latest(events, "approval_requested")["id"].as_u64().unwrap();
    runtime
        .send(format!(r#"{{"type":"approval_answer","id":{id},"choice":"{choice}"}}"#).as_str())
        .unwrap();
    collector.wait_for(rx, "turn_ok", Duration::from_secs(30));
}

// ── 路径 1：Manual 直问（不进 AI 审查）───────────────────────────────

/// 工具级 manual → approval_requested（trigger=manual，无 note），
/// **无任何审查调用**（P0-2：全部手动不被 AI 过滤）；应答 approve →
/// 执行。
#[test]
fn manual_level_asks_directly_without_ai_review() {
    let (paths, _dir) = approval_test_paths();
    let (collector, rx) = Collector::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let review = ReviewProvider::with_answers(vec![]);
    let runtime = assemble(
        paths,
        collector.clone(),
        ScriptedProvider::new(turn_scripts("call-1", "probe_touch")),
        review.clone(),
        vec![ProbeTool {
            name: "probe_touch".into(),
            namespace: None,
            runs: runs.clone(),
        }],
        Duration::from_secs(20),
    );
    collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

    // 工具级 manual（定稿线格式 set_approval_config）。
    runtime
        .send(r#"{"type":"set_approval_config","default_level":"auto","tool_levels":{"probe_touch":"manual"},"server_levels":{}}"#)
        .unwrap();
    let events = collector.wait_for(&rx, "config_changed", Duration::from_secs(10));
    let view = latest(&events, "config_changed")["config"]["approval"].clone();
    assert_eq!(view["default_level"], "auto", "生效规则视图: {view}");
    assert_eq!(view["tool_levels"]["probe_touch"], "manual");

    runtime.send(r#"{"type":"submit","text":"调用探针"}"#).unwrap();
    let events = collector.wait_for(&rx, "approval_requested", Duration::from_secs(20));
    let req = latest(&events, "approval_requested");
    assert_eq!(req["summary"]["tool_name"], "probe_touch");
    assert_eq!(req["summary"]["trigger"], "manual", "Manual 等级直问: {req}");
    assert!(
        req["summary"].get("note").is_none(),
        "manual 触发无 AI 审查 note: {req}"
    );
    assert_eq!(review.calls(), 0, "Manual 请求绝不进 AI 审查（P0-2）");

    answer_and_finish(&runtime, &collector, &rx, &events, "approve");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "应答后工具执行");
    assert_eq!(review.calls(), 0);
    runtime.shutdown();
}

// ── 路径 2：Auto + 低危名跳过（无审批无审查）─────────────────────────

/// mobile [approval] 段缺省 → 装配层 opt-in Auto（P0-4）；启发式 Low
/// （纯只读名）在 core 预过滤直接放行——无审批事件、无审查调用。
#[test]
fn auto_level_low_risk_skips_approval_and_review() {
    let (paths, _dir) = approval_test_paths();
    let (collector, rx) = Collector::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let review = ReviewProvider::with_answers(vec![]);
    // "probe_read" 含 "read" → 启发式 Low。
    let runtime = assemble(
        paths,
        collector.clone(),
        ScriptedProvider::new(turn_scripts("call-2", "probe_read")),
        review.clone(),
        vec![ProbeTool {
            name: "probe_read".into(),
            namespace: None,
            runs: runs.clone(),
        }],
        Duration::from_secs(20),
    );
    collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
    // 装配即 Auto（无 set_approval_config）——首份快照的生效规则视图
    // 锚定 mobile 的 [approval] 缺省 opt-in 语义（P0-4：oracle P1 的
    // 「UI 显示的必须是生效值」同一条通路）。
    {
        let events = collector.events.lock().unwrap().clone();
        let snap = latest(&events, "snapshot");
        assert_eq!(
            snap["session"]["config"]["approval"]["default_level"], "auto",
            "mobile 缺省 opt-in Auto: {snap}"
        );
    }

    runtime.send(r#"{"type":"submit","text":"读一下"}"#).unwrap();
    let events = collector.wait_for(&rx, "turn_ok", Duration::from_secs(30));
    assert_eq!(runs.load(Ordering::SeqCst), 1, "低危工具直接执行");
    assert_eq!(count_of(&events, "approval_requested"), 0, "无审批事件");
    assert_eq!(review.calls(), 0, "低危不进 AI 审查（省一次调用）");
    // 审查前广播的 info Notice 也不应出现（没进审查链）。
    assert!(!events.iter().any(|e| {
        e["type"] == "notice" && e["text"].as_str().unwrap_or("").contains("正在 AI 审查")
    }));
    runtime.shutdown();
}


// ── 路径 3：safe 静默放行 ────────────────────────────────────────────

/// 审查回 `{"risk":"ok"}` → 直接执行：无审批事件；审查恰好一次。
#[test]
fn review_safe_silently_approves() {
    let (paths, _dir) = approval_test_paths();
    let (collector, rx) = Collector::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let review = ReviewProvider::with_answers(vec![Ok(r#"{"risk":"ok"}"#.to_owned())]);
    // "probe_touch" 无高危/低危子串 → Medium → Auto 进审查链。
    let runtime = assemble(
        paths,
        collector.clone(),
        ScriptedProvider::new(turn_scripts("call-3", "probe_touch")),
        review.clone(),
        vec![ProbeTool {
            name: "probe_touch".into(),
            namespace: None,
            runs: runs.clone(),
        }],
        Duration::from_secs(20),
    );
    collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

    runtime.send(r#"{"type":"submit","text":"碰一下"}"#).unwrap();
    let events = collector.wait_for(&rx, "turn_ok", Duration::from_secs(30));
    assert_eq!(runs.load(Ordering::SeqCst), 1, "safe → 静默放行执行");
    assert_eq!(count_of(&events, "approval_requested"), 0, "无审批事件");
    assert_eq!(review.calls(), 1, "恰好一次审查调用");
    // 审查前广播了 info Notice（20s 空窗防「卡死」观感）。
    assert!(
        events.iter().any(|e| {
            e["type"] == "notice"
                && e["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("正在 AI 审查工具调用：probe_touch")
        }),
        "应广播审查 info notice: {events:?}"
    );
    runtime.shutdown();
}

// ── 路径 4：risky 弹人（note / trigger / server 断言）────────────────

/// 审查回 `{"risk":"high","reason":…}` → approval_requested（trigger=
/// auto_risk、note 携带审查理由）→ 应答 approve → 执行。
#[test]
fn review_risky_asks_human_with_note() {
    let (paths, _dir) = approval_test_paths();
    let (collector, rx) = Collector::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let review = ReviewProvider::with_answers(vec![Ok(
        r#"{"risk":"high","reason":"不可逆删除操作"}"#.to_owned(),
    )]);
    let runtime = assemble(
        paths,
        collector.clone(),
        ScriptedProvider::new(turn_scripts("call-4", "probe_touch")),
        review.clone(),
        vec![ProbeTool {
            name: "probe_touch".into(),
            namespace: None,
            runs: runs.clone(),
        }],
        Duration::from_secs(20),
    );
    collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

    runtime.send(r#"{"type":"submit","text":"碰一下"}"#).unwrap();
    let events = collector.wait_for(&rx, "approval_requested", Duration::from_secs(20));
    let req = latest(&events, "approval_requested");
    assert_eq!(req["summary"]["tool_name"], "probe_touch");
    assert_eq!(req["summary"]["trigger"], "auto_risk", "Auto 风险命中: {req}");
    let note = req["summary"]["note"].as_str().unwrap_or_default();
    assert!(
        note.starts_with("AI 审查判定高风险") && note.contains("不可逆删除操作"),
        "note 携带审查理由: {req}"
    );
    assert!(
        req["summary"].get("server").is_none(),
        "非 MCP 工具无 server 键: {req}"
    );
    assert_eq!(review.calls(), 1);

    answer_and_finish(&runtime, &collector, &rx, &events, "approve");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    runtime.shutdown();
}

// ── 路径 5：审查失败回退（fail-safe 弹人）────────────────────────────

/// 审查 provider 报错 → fail-safe 弹人（绝不因失败默认放行）。
#[test]
fn review_error_falls_back_to_human() {
    let (paths, _dir) = approval_test_paths();
    let (collector, rx) = Collector::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let review =
        ReviewProvider::with_answers(vec![Err(ProviderError::ServerError(500))]);
    let runtime = assemble(
        paths,
        collector.clone(),
        ScriptedProvider::new(turn_scripts("call-5", "probe_touch")),
        review.clone(),
        vec![ProbeTool {
            name: "probe_touch".into(),
            namespace: None,
            runs: runs.clone(),
        }],
        Duration::from_secs(20),
    );
    collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

    runtime.send(r#"{"type":"submit","text":"碰一下"}"#).unwrap();
    let events = collector.wait_for(&rx, "approval_requested", Duration::from_secs(20));
    let req = latest(&events, "approval_requested");
    assert_eq!(req["summary"]["trigger"], "auto_risk");
    assert!(
        req["summary"].get("note").is_none(),
        "fail-safe 回退无审查 note（未获得可信结论）: {req}"
    );
    assert_eq!(review.calls(), 1);

    answer_and_finish(&runtime, &collector, &rx, &events, "approve");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    runtime.shutdown();
}

// ── 路径 6：审查超时回退（builder 200ms 超时）────────────────────────

/// 审查挂起 → 200ms 超时 → detached 任务丢弃 → fail-safe 弹人。
#[test]
fn review_timeout_falls_back_to_human() {
    let (paths, _dir) = approval_test_paths();
    let (collector, rx) = Collector::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let review = ReviewProvider::hanging();
    let started = Instant::now();
    let runtime = assemble(
        paths,
        collector.clone(),
        ScriptedProvider::new(turn_scripts("call-6", "probe_touch")),
        review.clone(),
        vec![ProbeTool {
            name: "probe_touch".into(),
            namespace: None,
            runs: runs.clone(),
        }],
        // builder 超时覆盖（测试接缝）。
        Duration::from_millis(200),
    );
    collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

    runtime.send(r#"{"type":"submit","text":"碰一下"}"#).unwrap();
    let events = collector.wait_for(&rx, "approval_requested", Duration::from_secs(20));
    // 超时预算实际生效（远小于默认 20s；留余量容忍调度抖动）。
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "200ms 超时应快速回退，实际 {}ms",
        started.elapsed().as_millis()
    );
    assert_eq!(review.calls(), 1, "审查已被发起（挂起中）");

    answer_and_finish(&runtime, &collector, &rx, &events, "approve");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    runtime.shutdown();
}

// ── 补充：session_mcp 会话作用域直通 ─────────────────────────────────

/// MCP 形态工具（namespace=termux + 聚合名 → server="github"）审查
/// 判高风险 → 弹人（summary.server）→ 应答 session_mcp → 同 server
/// 的**其它**工具本会话直通（无审查调用、无审批事件，P0-3）。
#[test]
fn session_mcp_answer_scopes_server_for_the_session() {
    let (paths, _dir) = approval_test_paths();
    let (collector, rx) = Collector::new();
    let runs_a = Arc::new(AtomicUsize::new(0));
    let runs_b = Arc::new(AtomicUsize::new(0));
    let review = ReviewProvider::with_answers(vec![Ok(
        r#"{"risk":"high","reason":"外部服务变更"}"#.to_owned(),
    )]);
    let runtime = assemble(
        paths,
        collector.clone(),
        ScriptedProvider::new(
            turn_scripts("call-a", "termux_github__create_issue")
                .into_iter()
                .chain(turn_scripts("call-b", "termux_github__delete_repo"))
                .collect(),
        ),
        review.clone(),
        vec![
            ProbeTool {
                name: "termux_github__create_issue".into(),
                namespace: Some("termux".into()),
                runs: runs_a.clone(),
            },
            ProbeTool {
                name: "termux_github__delete_repo".into(),
                namespace: Some("termux".into()),
                runs: runs_b.clone(),
            },
        ],
        Duration::from_secs(20),
    );
    collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

    // 回合 1：高风险 → 弹人（server=github）→ session_mcp。
    runtime.send(r#"{"type":"submit","text":"建 issue"}"#).unwrap();
    let events = collector.wait_for(&rx, "approval_requested", Duration::from_secs(20));
    let req = latest(&events, "approval_requested");
    assert_eq!(
        req["summary"]["server"], "github",
        "聚合名反解出下游 slug: {req}"
    );
    assert_eq!(review.calls(), 1);
    answer_and_finish(&runtime, &collector, &rx, &events, "session_mcp");
    assert_eq!(runs_a.load(Ordering::SeqCst), 1);

    // 回合 2：同 server 的另一工具（名字含 delete → 启发式 High）本会话
    // 直通——不再付审查费、不弹人。
    runtime
        .send(r#"{"type":"submit","text":"删仓库"}"#)
        .unwrap();
    let events = collector.wait_for_count(&rx, "turn_ok", 2, Duration::from_secs(30));
    assert_eq!(runs_b.load(Ordering::SeqCst), 1, "同 server 后续工具直通");
    assert_eq!(
        count_of(&events, "approval_requested"),
        1,
        "会话作用域放行不再弹人（仅回合 1 的那一次）"
    );
    assert_eq!(review.calls(), 1, "P0-3：会话放行先于 LLM 审查");
    runtime.shutdown();
}

// ── 补充：always_tool 持久化 + 后续直通 ───────────────────────────────

/// risky → always_tool 应答 → toml 落盘 `[approval.tool_levels]` +
/// 后续同名工具经规则引擎直通（无审查无审批）。
#[test]
fn always_tool_answer_persists_and_allows_next_call() {
    let (paths, _dir) = approval_test_paths();
    let (collector, rx) = Collector::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let review = ReviewProvider::with_answers(vec![Ok(
        r#"{"risk":"high","reason":"高危操作"}"#.to_owned(),
    )]);
    let config_file = paths.config_file();
    let runtime = assemble(
        paths,
        collector.clone(),
        ScriptedProvider::new(
            turn_scripts("call-7", "probe_touch")
                .into_iter()
                .chain(turn_scripts("call-8", "probe_touch"))
                .collect(),
        ),
        review.clone(),
        vec![ProbeTool {
            name: "probe_touch".into(),
            namespace: None,
            runs: runs.clone(),
        }],
        Duration::from_secs(20),
    );
    collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

    runtime.send(r#"{"type":"submit","text":"碰一下"}"#).unwrap();
    let events = collector.wait_for(&rx, "approval_requested", Duration::from_secs(20));
    answer_and_finish(&runtime, &collector, &rx, &events, "always_tool");
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // toml 落盘断言：[approval] 整段重写为新 schema。
    let on_disk = std::fs::read_to_string(&config_file).unwrap();
    assert!(
        on_disk.contains("[approval.tool_levels]") && on_disk.contains("probe_touch = \"allow\""),
        "always_tool 持久化: {on_disk}"
    );
    assert!(
        !on_disk.contains("policy ="),
        "legacy policy 键被整段重写消除: {on_disk}"
    );

    // 回合 2：同名工具经规则引擎（tools[probe_touch]=Allow）直通。
    runtime
        .send(r#"{"type":"submit","text":"再碰一下"}"#)
        .unwrap();
    let events = collector.wait_for_count(&rx, "turn_ok", 2, Duration::from_secs(30));
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "后续同名直通；事件流: {events:?}"
    );
    assert_eq!(
        count_of(&events, "approval_requested"),
        1,
        "规则放行不弹人（仅回合 1 的那一次）"
    );
    assert_eq!(review.calls(), 1, "规则放行不经审查");
    runtime.shutdown();
}

// ── 补充：NewSession 清会话作用域 ─────────────────────────────────────

/// session_mcp 放行后 new_session → 会话作用域清空 → 同 server 的下一
/// 个工具重新走审查（risky → 再弹人）。
#[test]
fn new_session_clears_session_scopes() {
    let (paths, _dir) = approval_test_paths();
    let (collector, rx) = Collector::new();
    let runs_a = Arc::new(AtomicUsize::new(0));
    let runs_b = Arc::new(AtomicUsize::new(0));
    let review = ReviewProvider::with_answers(vec![
        Ok(r#"{"risk":"high","reason":"外部服务变更"}"#.to_owned()),
        Ok(r#"{"risk":"high","reason":"删除不可逆"}"#.to_owned()),
    ]);
    let runtime = assemble(
        paths,
        collector.clone(),
        ScriptedProvider::new(
            turn_scripts("call-a", "termux_github__create_issue")
                .into_iter()
                .chain(turn_scripts("call-b", "termux_github__delete_repo"))
                .collect(),
        ),
        review.clone(),
        vec![
            ProbeTool {
                name: "termux_github__create_issue".into(),
                namespace: Some("termux".into()),
                runs: runs_a.clone(),
            },
            ProbeTool {
                name: "termux_github__delete_repo".into(),
                namespace: Some("termux".into()),
                runs: runs_b.clone(),
            },
        ],
        Duration::from_secs(20),
    );
    collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

    // 回合 1：session_mcp 放行 github 作用域。
    runtime.send(r#"{"type":"submit","text":"建 issue"}"#).unwrap();
    let events = collector.wait_for(&rx, "approval_requested", Duration::from_secs(20));
    answer_and_finish(&runtime, &collector, &rx, &events, "session_mcp");
    assert_eq!(runs_a.load(Ordering::SeqCst), 1);

    // 新会话（回合已结束，busy 守卫通过）。
    runtime.send(r#"{"type":"new_session"}"#).unwrap();
    collector.wait_for(&rx, "session_reset", Duration::from_secs(10));

    // 回合 2：同 server 工具重新审查（risky）→ 再弹人。
    runtime
        .send(r#"{"type":"submit","text":"删仓库"}"#)
        .unwrap();
    let events =
        collector.wait_for_count(&rx, "approval_requested", 2, Duration::from_secs(20));
    let req = latest(&events, "approval_requested");
    assert_eq!(req["summary"]["tool_name"], "termux_github__delete_repo");
    assert_eq!(review.calls(), 2, "会话作用域已清空 → 重新审查");
    answer_and_finish(&runtime, &collector, &rx, &events, "approve");
    assert_eq!(runs_b.load(Ordering::SeqCst), 1);
    runtime.shutdown();
}
