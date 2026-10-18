//! 引擎任务：tui/event.rs `spawn_turn` 的会话核心移植 + pre-turn
//! auto-compact（源在 tui/app.rs `start_turn`/`run_compact`/
//! `generate_summary`，随 TUI 纯客户端化由本 crate 接管）。
//!
//! - RunManager **所有权回传**：submit 时从 `CoreInner` take，回合结束
//!   放回（与 TUI 同模式；manager 非 Sync，不能共享引用进异步任务）。
//! - 进度回调 = `SessionProgressBridge`：持 core 锁「先改转写镜像、再
//!   广播」，同一连接上的事件序 == 转写追加序。
//! - auto-compact：回合提交时 history 超阈值 → 先用压缩模型摘要压缩
//!   （capability=compact 绑定的代号；未绑定 → 继承会话当前模型。
//!   provider 失败/别名缺失 → 机械拼接兜底，core `compact()` 内建），
//!   再进正式回合。compact 的 usage 计入会话累计（不计 run 行，同 TUI）。

use std::sync::Arc;

use openslate_core::context_manager::compact::{compact, needs_compact};
use openslate_core::model_config::resolve_model;
use openslate_core::provider::{GenerateRequest, ModelProvider, ProgressCallback};
use openslate_core::run_manager::RunManager;
use openslate_core::runtime::CostSpec;
use openslate_core::types::{Message, MessageRole, RunId, Usage};
use openslate_protocol::{EntryDto, ServerMsg, TurnSummaryDto};
use tokio_util::sync::CancellationToken;

use crate::state::{
    effective_limits, flush_stream, fold_tool_outcomes, tool_end_entry, tool_start_entry, AppState,
};

/// 摘要器系统提示（与 REPL/TUI 逐字一致——两个前端的摘要行为必须相同）。
const SUMMARY_SYSTEM_PROMPT: &str = "You summarize agent conversation transcripts \
for continued work. Produce a concise summary that preserves: \
(1) key decisions made and their rationale, \
(2) important file paths, commands, and code artifacts touched, \
(3) unfinished tasks, open questions, and next steps. \
Drop pleasantries and verbose tool output details. Be brief — only what is \
needed to continue the work effectively.";

/// 标题生成系统提示（capability=title）。模型只回标题本身。
const TITLE_SYSTEM_PROMPT: &str = "You generate a short session title from the \
user's first message. Output ONLY the title text: at most 16 characters, \
no quotes, no label prefix, no trailing punctuation. Write it in the same \
language as the user's message.";

/// 进度回调 → 转写镜像 + 广播。`&mut self` 方法全部内联锁 core。
pub struct SessionProgressBridge {
    pub state: Arc<AppState>,
}

impl ProgressCallback for SessionProgressBridge {
    fn on_request_start(&mut self, step: u32, model_id: &str) {
        let msg = {
            let mut inner = self.state.core.lock();
            // 新请求 = 冲掉上一请求的挂起 usage 行（fix-19 hold 语义）。
            flush_stream(&mut inner);
            ServerMsg::RequestStart {
                step,
                model: model_id.to_owned(),
            }
        };
        self.state.sink.broadcast(msg);
    }

    fn on_input_estimate(&mut self, tokens: u32) {
        self.state
            .sink
            .broadcast(ServerMsg::InputEstimate { tokens });
    }

    fn on_first_token(&mut self) {
        self.state.sink.broadcast(ServerMsg::FirstToken);
    }

    fn on_delta(&mut self, text: &str) {
        {
            let mut inner = self.state.core.lock();
            inner.stream.answer.push_str(text);
        }
        self.state.sink.broadcast(ServerMsg::Delta {
            text: text.to_owned(),
        });
    }

    fn on_reasoning(&mut self, text: &str) {
        {
            let mut inner = self.state.core.lock();
            inner.stream.reasoning.push_str(text);
        }
        self.state.sink.broadcast(ServerMsg::Reasoning {
            text: text.to_owned(),
        });
    }

    fn on_usage(&mut self, usage: Usage) {
        {
            let mut inner = self.state.core.lock();
            // 挂起：RequestEnd 落成 Meta 行（TUI fix-19 同款位置语义）。
            inner.pending_step_meta = Some(usage_meta_line(&usage));
        }
        self.state.sink.broadcast(ServerMsg::Usage { usage });
    }

    fn on_request_end(&mut self) {
        {
            let mut inner = self.state.core.lock();
            flush_stream(&mut inner);
        }
        self.state.sink.broadcast(ServerMsg::RequestEnd);
    }

    fn on_tool_start(&mut self, name: &str, args: &str) {
        {
            let mut inner = self.state.core.lock();
            tool_start_entry(&mut inner, name, args);
        }
        self.state.sink.broadcast(ServerMsg::ToolStart {
            name: name.to_owned(),
            args: args.to_owned(),
        });
    }

    fn on_tool_end(&mut self, name: &str, bytes: usize, truncated: bool, preview: &str) {
        {
            let mut inner = self.state.core.lock();
            tool_end_entry(&mut inner, name, bytes, truncated);
        }
        self.state.sink.broadcast(ServerMsg::ToolEnd {
            name: name.to_owned(),
            bytes,
            truncated,
            preview: if preview.is_empty() {
                None
            } else {
                Some(preview.to_owned())
            },
        });
    }

    fn on_step_end(&mut self) {
        {
            let mut inner = self.state.core.lock();
            flush_stream(&mut inner);
            // 连续边界事件折叠为一个分隔（TUI step_end 同款）。
            let already = matches!(inner.transcript.last(), Some(EntryDto::StepBreak) | None);
            if !already {
                inner.transcript.push(EntryDto::StepBreak);
            }
        }
        self.state.sink.broadcast(ServerMsg::StepEnd);
    }
}

/// 精确 usage 的 Meta 行文本（简式：`↑in ↓out [⎓cached]`；
/// TUI live 视图里的 ttft/速率装饰是客户端渲染层的事，不进协议数据）。
fn usage_meta_line(usage: &Usage) -> String {
    match usage.cached_input_tokens {
        Some(cached) => format!(
            "↑{} ↓{} ⎓{}",
            usage.input_tokens, usage.output_tokens, cached
        ),
        None => format!("↑{} ↓{}", usage.input_tokens, usage.output_tokens),
    }
}

/// 起一个回合。**跑在 `spawn_blocking` 专用线程**而非 tokio worker：
/// 审批 `decide()` 是同步 trait、会阻塞等待客户端应答（见
/// approval.rs 模块文档），引擎执行期间还有多个同步回调；占住一个
/// runtime worker 会饿死同 worker 上的转发/写任务（实测复现），
/// 而阻塞池线程阻塞正是其设计用途。内层 future 经
/// `Handle::block_on` 驱动，tokio 定时器/通道/sqlx 均可用。
///
/// 引擎任务自行完成收尾（history 替换 / manager 回传 / 广播
/// turn_ok / turn_error），调用方不再介入。
#[allow(clippy::too_many_arguments)]
pub fn spawn_engine_turn(
    state: Arc<AppState>,
    manager: RunManager,
    run_id: RunId,
    provider: Box<dyn ModelProvider>,
    history: Vec<Message>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        handle.block_on(engine_turn(
            state, manager, run_id, provider, history, cancel,
        ))
    })
}

async fn engine_turn(
    state: Arc<AppState>,
    mut manager: RunManager,
    run_id: RunId,
    provider: Box<dyn ModelProvider>,
    history: Vec<Message>,
    cancel: CancellationToken,
) {
    let started = std::time::Instant::now();
    let mut bridge = SessionProgressBridge {
        state: state.clone(),
    };
    let result = manager
        .execute_with_run_id(
            run_id,
            provider.as_ref(),
            &history,
            cancel,
            Some(&mut bridge),
        )
        .await;

    // 回收：无论成败，manager 回家、运行态复位。
    let mut inner = state.core.lock();
    inner.running = false;
    inner.cancel = None;
    inner.depth_cur = 0;
    inner.tool_calls_cur = 0;
    inner.agents_running = 0;
    // 回合期间 agent_tree 可能被热替换（工具可见性页 set_agent_tool_visibility
    // 只改 inner.agent_tree，碰不到被 take 走的 manager）——回家的 manager
    // 采用最新树，下一回合即按新可见性过滤（config 的同款陈旧问题不在
    // 本路径处理，保持既有语义）。
    manager.agent_tree = inner.agent_tree.clone();
    inner.manager = Some(manager);
    match result {
        Ok(r) => {
            inner.history = r.messages.clone();
            flush_stream(&mut inner);
            fold_tool_outcomes(&mut inner, &r.messages);
            inner.stats.turns += 1;
            inner.stats.total_input_tokens += r.total_input_tokens;
            inner.stats.total_output_tokens += r.total_output_tokens;
            inner.stats.total_cost_usd += r.total_cost_usd;
            let dto = TurnSummaryDto::from(r);
            crate::session_event!(
                "turn ok: {}s · ↑{} ↓{}",
                started.elapsed().as_secs(),
                dto.total_input_tokens,
                dto.total_output_tokens
            );
            state.sink.broadcast(ServerMsg::TurnOk {
                summary: Box::new(dto),
            });
        }
        Err(e) => {
            // 失败保留 history 现状（TUI 同款：用户消息留着可重发）。
            flush_stream(&mut inner);
            crate::session_event!("turn error: {e}");
            state.sink.broadcast(ServerMsg::TurnError {
                message: e.to_string(),
            });
        }
    }
}

/// pre-turn auto-compact 门（tui/app.rs `start_turn` 的移植）。
/// 返回 true = 已压缩。调用方保证此刻 `compacting == true`（互斥）。
pub async fn run_auto_compact(state: &Arc<AppState>) {
    // 把 history 搬出来压（std Mutex 守卫不能跨 await）。
    let (mut history, max_msgs, max_bytes, enabled) = {
        let mut inner = state.core.lock();
        let limits = effective_limits(&inner.config);
        (
            std::mem::take(&mut inner.history),
            limits.max_context_messages as usize,
            limits.max_context_bytes as usize,
            limits.auto_compact,
        )
    };
    if !enabled || !needs_compact(&history, max_msgs, max_bytes, 0) {
        let mut inner = state.core.lock();
        inner.history = history;
        return;
    }

    tracing::info!("session auto-compact: history over context limits, summarizing");

    // 压缩模型计划：[capabilities] 显式绑定 compact → 用绑定代号；
    // 未绑定 → 继承会话当前模型（model_alias，与主对话同档；历史上
    // 硬编码回退 "fast" 已废止——未配 compact 又无 "fast" 条目的配置
    // 会无谓退化到机械拼接）。解析/构建失败 → None → 机械兜底
    // （core compact 内建），路径不变。
    let factory = state.provider_factory.clone();
    let (config, session_alias) = {
        let inner = state.core.lock();
        (inner.config.clone(), inner.model_alias.clone())
    };
    let compact_alias = config
        .capabilities
        .get(openslate_core::model_config::CAP_COMPACT)
        .cloned()
        .unwrap_or(session_alias);
    let summary_plan: Option<(String, Box<dyn ModelProvider>, CostSpec)> = {
        match resolve_model(&config, &compact_alias) {
            Ok(resolved) => match factory(&config, &compact_alias) {
                Ok(provider) => {
                    let pricing = resolved.cost_spec();
                    Some((resolved.model_id, provider, pricing))
                }
                Err(e) => {
                    tracing::debug!("no provider for '{compact_alias}' — mechanical fallback: {e}");
                    None
                }
            },
            Err(e) => {
                tracing::debug!("no '{compact_alias}' alias — mechanical fallback: {e}");
                None
            }
        }
    };
    let summary_pricing = summary_plan
        .as_ref()
        .map(|(_, _, pricing)| *pricing)
        .unwrap_or_default();

    let usage_slot = Arc::new(std::sync::Mutex::new(None::<Usage>));
    let slot = Arc::clone(&usage_slot);
    let plan = summary_plan;
    let result = compact(&mut history, None, max_msgs, max_bytes, move |text| {
        let text = text.to_owned();
        async move {
            let (model_id, provider, _pricing) = plan?;
            let (summary, usage) = generate_summary(provider.as_ref(), &model_id, &text).await;
            if let Some(u) = usage {
                *slot.lock().expect("compact usage slot poisoned") = Some(u);
            }
            summary
        }
    })
    .await;

    tracing::info!(
        "session auto-compact: {} → {} messages",
        result.messages_before,
        result.messages_after
    );

    // compact usage 计入会话累计（同 TUI：不进 run 行）。
    let compact_usage = usage_slot
        .lock()
        .expect("compact usage slot poisoned")
        .take();
    {
        let mut inner = state.core.lock();
        inner.history = history;
        if let Some(usage) = compact_usage {
            inner.stats.total_input_tokens += usage.input_tokens as u64;
            inner.stats.total_output_tokens += usage.output_tokens as u64;
            inner.stats.total_cost_usd += summary_pricing.cost_of(&usage);
        }
    }
}

/// fast 模型摘要（tui/app.rs `generate_summary` 逐字移植）。
async fn generate_summary(
    provider: &dyn ModelProvider,
    model_id: &str,
    conversation_text: &str,
) -> (Option<String>, Option<Usage>) {
    let request = GenerateRequest {
        model_id: model_id.to_owned(),
        system_prompt: Some(SUMMARY_SYSTEM_PROMPT.to_owned()),
        messages: vec![Message {
            role: MessageRole::User,
            content: format!(
                "Summarize the following conversation for continuation:\n\n{}",
                conversation_text
            ),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
    };
    match provider.generate(request).await {
        Ok(response) => {
            let usage = response.usage;
            // 空白回复视作失败 → 机械兜底（空摘要消息更糟）。
            let summary = response.content.filter(|c| !c.trim().is_empty());
            (summary, usage)
        }
        Err(e) => {
            tracing::warn!("compact summary failed ({e}); mechanical fallback");
            (None, None)
        }
    }
}

// ── 会话标题生成（capability=title）────────────────────────────────────

/// 首条用户消息喂给标题模型的截断长度（字符，CJK 安全）——标题只需要
///主题，不需要全文。
const TITLE_INPUT_MAX_CHARS: usize = 400;

/// 标题清洗后的最大长度（字符）。模型提示要求 ≤16 字符，这里留裕量
/// 兜住啰嗦模型；超出按字符截断（与 prompt 截断同款 CJK 安全语义）。
const TITLE_MAX_CHARS: usize = 24;

/// 标题模型调用（capability=title）。失败/空白 → None（调用方保留
/// run 开始时已落库的 prompt 截断占位标题——机械兜底已就位，此处
/// 无需再降级）。`pub(crate)` 供 session.rs 异步任务复用。
pub(crate) async fn generate_title(
    provider: &dyn ModelProvider,
    model_id: &str,
    first_prompt: &str,
) -> (Option<String>, Option<Usage>) {
    let trimmed_input: String = first_prompt.chars().take(TITLE_INPUT_MAX_CHARS).collect();
    let request = GenerateRequest {
        model_id: model_id.to_owned(),
        system_prompt: Some(TITLE_SYSTEM_PROMPT.to_owned()),
        messages: vec![Message {
            role: MessageRole::User,
            content: trimmed_input,
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        }],
        tools: Vec::new(),
        max_tokens: Some(48),
        temperature: None,
    };
    match provider.generate(request).await {
        Ok(response) => {
            let usage = response.usage;
            (sanitize_title(response.content.as_deref()), usage)
        }
        Err(e) => {
            tracing::debug!("title generation failed ({e}); keep mechanical title");
            (None, None)
        }
    }
}

/// 标题清洗：首行 → 去包裹引号/书名号/反引号 → 按字符截断。空/纯
/// 标点结果 → None（保留占位标题）。`pub(crate)` 供单测。
pub(crate) fn sanitize_title(raw: Option<&str>) -> Option<String> {
    let first_line = raw?.lines().map(str::trim).find(|l| !l.is_empty())?;
    let stripped = first_line
        .trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | '`' | '“' | '”' | '「' | '」' | '『' | '』' | '《' | '》'
            )
        })
        .trim();
    if stripped.is_empty() {
        return None;
    }
    let title: String = stripped.chars().take(TITLE_MAX_CHARS).collect();
    if title.trim().is_empty() {
        None
    } else {
        Some(title)
    }
}

/// 供单元测试：usage Meta 行格式。
#[cfg(test)]
pub(crate) fn meta_line_for_test(usage: &Usage) -> String {
    usage_meta_line(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_title_takes_first_line_strips_quotes_and_caps() {
        assert_eq!(
            sanitize_title(Some("  调试构建失败  \n第二行")),
            Some("调试构建失败".into())
        );
        assert_eq!(
            sanitize_title(Some("\"fix login bug\"")),
            Some("fix login bug".into())
        );
        assert_eq!(
            sanitize_title(Some("「重构压缩逻辑」")),
            Some("重构压缩逻辑".into())
        );
        // 啰嗦模型：按字符截断到 TITLE_MAX_CHARS（24），CJK 安全。
        let long = "一".repeat(60);
        assert_eq!(
            sanitize_title(Some(&long)).map(|t| t.chars().count()),
            Some(TITLE_MAX_CHARS)
        );
        // 空白 / 纯引号 → None（保留机械占位标题）。
        assert_eq!(sanitize_title(Some("   ")), None);
        assert_eq!(sanitize_title(Some("\"\"")), None);
        assert_eq!(sanitize_title(None), None);
    }

    #[tokio::test]
    async fn generate_title_caps_input_and_sanitizes_output() {
        // 桩 provider：验证输入截断 + 小请求限额 + 输出清洗链路。
        use openslate_core::error::ProviderError;
        use openslate_core::provider::{GenerateRequest, ModelProvider};
        use openslate_core::types::ModelResponse;
        struct TitleStub;
        #[async_trait::async_trait]
        impl ModelProvider for TitleStub {
            async fn generate(&self, req: GenerateRequest) -> Result<ModelResponse, ProviderError> {
                let input = &req.messages[0].content;
                assert!(
                    input.chars().count() <= TITLE_INPUT_MAX_CHARS,
                    "input must be truncated"
                );
                assert_eq!(req.max_tokens, Some(48), "title gen is a tiny request");
                Ok(ModelResponse {
                    content: Some("  \"修复登录超时\" \n多余行".into()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    usage: None,
                    finish_reason: None,
                })
            }

            fn provider_name(&self) -> &str {
                "title-stub"
            }
        }
        let long_prompt = "x".repeat(1_000);
        let (title, _usage) = generate_title(&TitleStub, "m", &long_prompt).await;
        assert_eq!(title.as_deref(), Some("修复登录超时"));
    }

    #[test]
    fn usage_meta_line_formats() {
        let plain = Usage {
            input_tokens: 10,
            output_tokens: 2,
            cached_input_tokens: None,
            reasoning_tokens: None,
        };
        assert_eq!(meta_line_for_test(&plain), "↑10 ↓2");
        let cached = Usage {
            input_tokens: 10,
            output_tokens: 2,
            cached_input_tokens: Some(4),
            reasoning_tokens: None,
        };
        assert_eq!(meta_line_for_test(&cached), "↑10 ↓2 ⎓4");
    }

    // ── run_auto_compact：压缩模型选择（绑定优先，未绑定继承会话模型）──

    mod auto_compact {
        use super::*;
        use crate::approval::SessionApprovalBridge;
        use crate::state::{ConfigPaths, CoreInner, MsgSink, ProviderFactory, SessionCore};
        use openslate_core::agent_tree::AgentTree;
        use openslate_core::config::{parse_openslate_toml, AgentsConfig};
        use openslate_core::types::{AgentConfig, AgentId};
        use std::sync::Mutex;

        /// 摘要桩的固定回复（区别于机械拼接兜底的 "User: …" 文案）。
        const STUB_SUMMARY: &str = "STUB-LLM-SUMMARY";

        /// 桩 provider / 工厂侧的调用记录（代号询问 + 实际请求 model_id）。
        #[derive(Default)]
        struct Recorder {
            aliases_asked: Mutex<Vec<String>>,
            models_used: Mutex<Vec<String>>,
        }

        struct SummaryStub {
            recorder: Arc<Recorder>,
        }

        #[async_trait::async_trait]
        impl ModelProvider for SummaryStub {
            async fn generate(
                &self,
                req: openslate_core::provider::GenerateRequest,
            ) -> Result<openslate_core::types::ModelResponse, openslate_core::error::ProviderError>
            {
                self.recorder.models_used.lock().unwrap().push(req.model_id);
                Ok(openslate_core::types::ModelResponse {
                    content: Some(STUB_SUMMARY.into()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    usage: None,
                    finish_reason: None,
                })
            }

            fn provider_name(&self) -> &str {
                "summary-stub"
            }
        }

        /// run_auto_compact 不触 sink / 审批，no-op 实现即可。
        #[derive(Default)]
        struct NoopSink;

        impl MsgSink for NoopSink {
            fn broadcast(&self, _msg: ServerMsg) {}
            fn send_to(&self, _conn_id: u64, _msg: ServerMsg) -> bool {
                true
            }
            fn conn_ids(&self) -> Vec<u64> {
                Vec::new()
            }
            fn count(&self) -> usize {
                0
            }
            fn close_all(&self) {}
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        /// 最小 state：9 条 history + limits.max_context_messages=10 →
        /// 9/10 > 0.8 触发 compact（保持 KEEP_RECENT_COUNT=2 → 压后 3 条）。
        /// 无 [levels]：代号直查 [models]（legacy 路径，本测试只关心
        /// compact 选哪个代号）。返回调用记录供断言（工厂被问的代号 +
        /// 桩实际收到的 model_id）。
        fn test_state(toml: &str, model_alias: &str) -> (Arc<AppState>, Arc<Recorder>) {
            let recorder = Arc::new(Recorder::default());
            let state = build_state(toml, model_alias, Arc::clone(&recorder));
            (state, recorder)
        }

        fn build_state(toml: &str, model_alias: &str, recorder: Arc<Recorder>) -> Arc<AppState> {
            let config = parse_openslate_toml(toml).expect("fixture parses");
            let agents = AgentsConfig {
                agents: vec![AgentConfig {
                    hidden_tools: Vec::new(),
                    id: AgentId("root".into()),
                    name: "Root".into(),
                    model: "main".into(),
                    children: vec![],
                    tools: vec![],
                    default_prompt: "root prompt".into(),
                }],
            };
            let agent_tree = AgentTree::from_configs(&agents.agents).expect("tree builds");
            let history: Vec<Message> = (0..9)
                .map(|i| Message {
                    role: if i % 2 == 0 {
                        MessageRole::User
                    } else {
                        MessageRole::Assistant
                    },
                    content: format!("msg-{i}"),
                    tool_call_id: None,
                    name: None,
                    tool_calls: None,
                    reasoning_content: None,
                })
                .collect();
            let sink = Arc::new(NoopSink);
            let factory_recorder = Arc::clone(&recorder);
            let factory: ProviderFactory = Arc::new(move |_cfg, alias| {
                factory_recorder
                    .aliases_asked
                    .lock()
                    .unwrap()
                    .push(alias.to_owned());
                Ok(Box::new(SummaryStub {
                    recorder: Arc::clone(&factory_recorder),
                }) as Box<dyn ModelProvider>)
            });
            let inner = CoreInner {
                session_id: "test-session".into(),
                session_label: "test".into(),
                history,
                transcript: Vec::new(),
                running: false,
                compacting: true,
                depth_cur: 0,
                agents_running: 0,
                tool_calls_cur: 0,
                model_alias: model_alias.into(),
                cancel: None,
                config,
                agents_cfg: agents,
                agent_tree,
                skills: Vec::new(),
                store: None,
                session_run: None,
                manager: None,
                engine_task: None,
                paths: ConfigPaths {
                    active: std::path::PathBuf::from("/dev/null"),
                    global: None,
                    local: None,
                },
                stats: Default::default(),
                stream: Default::default(),
                tool_timers: Default::default(),
                pending_step_meta: None,
            };
            Arc::new(AppState {
                core: Arc::new(SessionCore::new(inner)),
                sink,
                approval: Arc::new(SessionApprovalBridge::new(Arc::new(NoopSink))),
                auth_token: None,
                provider_factory: factory,
                origin: "test",
                _mcp: Default::default(),
            })
        }

        fn base_toml(capabilities: Option<&str>) -> String {
            // entry-a / entry-b 两个可解析条目；limits 收紧到 10 条触发
            // auto-compact。capabilities 由用例注入（None = 未绑定）。
            format!(
                r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.entry_a]
provider = "zhipu"
model = "model-a"

[models.entry_b]
provider = "zhipu"
model = "model-b"

[limits]
max_context_messages = 10
{}"#,
                capabilities.unwrap_or("")
            )
        }

        fn history_after(state: &Arc<AppState>) -> Vec<Message> {
            state.core.lock().history.clone()
        }

        #[tokio::test]
        async fn unbound_compact_inherits_session_model() {
            // 未绑定 compact + 会话 model_alias=entry_a → 用 entry_a
            //（工厂被问 entry_a、实际请求 model-a；摘要非机械拼接）。
            let (state, recorder) = test_state(&base_toml(None), "entry_a");
            run_auto_compact(&state).await;
            assert_eq!(
                *recorder.aliases_asked.lock().unwrap(),
                vec!["entry_a".to_owned()],
                "factory must be asked for the session model alias"
            );
            assert_eq!(
                *recorder.models_used.lock().unwrap(),
                vec!["model-a".to_owned()],
                "summary request must use the session model"
            );
            let history = history_after(&state);
            assert_eq!(history.len(), 3, "1 summary + 2 recent");
            assert!(
                history[0].content.contains(STUB_SUMMARY),
                "summary must come from the stub provider (session model): {}",
                history[0].content
            );
            assert!(
                !history[0].content.contains("msg-0"),
                "not mechanical concat"
            );
        }

        #[tokio::test]
        async fn bound_compact_overrides_session_model() {
            // 显式 [capabilities] compact=entry_b → 用 entry_b（优先于
            // 会话模型 entry_a）：工厂被问 entry_b、实际请求 model-b。
            let toml = base_toml(Some("\n[capabilities]\ncompact = \"entry_b\"\n"));
            let (state, recorder) = test_state(&toml, "entry_a");
            run_auto_compact(&state).await;
            assert_eq!(
                *recorder.aliases_asked.lock().unwrap(),
                vec!["entry_b".to_owned()],
                "explicit capability binding must win over session model"
            );
            assert_eq!(
                *recorder.models_used.lock().unwrap(),
                vec!["model-b".to_owned()]
            );
            let history = history_after(&state);
            assert_eq!(history.len(), 3);
            assert!(
                history[0].content.contains(STUB_SUMMARY),
                "summary via bound model expected: {}",
                history[0].content
            );
        }

        #[tokio::test]
        async fn unresolved_alias_falls_back_to_mechanical() {
            // 会话模型解析失败（别名不在 [models]）且未绑定 → 机械兜底：
            // 工厂零调用，摘要是 "User: …" 拼接文案，不含桩回复。
            let (state, recorder) = test_state(&base_toml(None), "ghost-alias");
            run_auto_compact(&state).await;
            assert!(
                recorder.aliases_asked.lock().unwrap().is_empty(),
                "no factory call for unresolvable alias"
            );
            assert!(
                recorder.models_used.lock().unwrap().is_empty(),
                "no LLM call for unresolvable alias"
            );
            let history = history_after(&state);
            assert_eq!(history.len(), 3, "mechanical fallback still compacts");
            assert!(
                history[0].content.contains("User: msg-0"),
                "mechanical concat summary expected: {}",
                history[0].content
            );
            assert!(
                !history[0].content.contains(STUB_SUMMARY),
                "no LLM call should happen for unresolvable alias"
            );
        }
    }
}
