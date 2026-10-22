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
//! - **回合级断连重试**：Network 类失败（流中途连接断开，adapter 层
//!   因「首个 Delta 后不重试」放行的链路）自动重跑整个回合——预算
//!   每回合 1 次 + 会话级 10s 冷却窗；重试广播 Notice、转写追加 Meta
//!   标记；用尽后 `TurnError` 携 `kind=network` 与重试次数文案。
//! - **panic 防线**：回合体整体跑在 `catch_unwind` 内（exp-10）——
//!   收尾是唯一恢复点，panic 跳过它会让 `running` 永久卡 true
//!   （busy 守卫全拒 + Kotlin wakelock 永持）。拦截后强制收尾 +
//!   `TurnError(kind=other)`。依赖 `panic=unwind`（mobile profile
//!   已保证）。

use std::sync::Arc;
use std::time::Duration;

use openslate_core::context_manager::compact::{compact, needs_compact};
use openslate_core::error::OpenSlateError;
use openslate_core::model_config::resolve_model;
use openslate_core::provider::{GenerateRequest, ModelProvider, ProgressCallback};
use openslate_core::run_manager::RunManager;
use openslate_core::runtime::CostSpec;
use openslate_core::types::{Message, MessageRole, RunId, Usage};
use openslate_protocol::{EntryDto, NoticeLevel, ServerMsg, TurnErrorKind, TurnSummaryDto};
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

    fn on_tool_start(&mut self, name: &str, args: &str, call_id: &str) {
        {
            let mut inner = self.state.core.lock();
            tool_start_entry(&mut inner, name, args, Some(call_id));
        }
        self.state.sink.broadcast(ServerMsg::ToolStart {
            name: name.to_owned(),
            args: args.to_owned(),
            call_id: if call_id.is_empty() { None } else { Some(call_id.to_owned()) },
        });
    }

    fn on_tool_end(&mut self, name: &str, bytes: usize, truncated: bool, preview: &str, call_id: &str) {
        {
            let mut inner = self.state.core.lock();
            tool_end_entry(&mut inner, name, bytes, truncated, Some(call_id));
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
            call_id: if call_id.is_empty() { None } else { Some(call_id.to_owned()) },
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
/// 引擎任务自行完成收尾（运行态复位 / manager 回传 / 广播
/// turn_ok / turn_error），调用方不再介入。
///
/// ## panic 防线（exp-10）
///
/// 收尾（[`finalize_turn`]）是 running=false 的**唯一恢复点**：任何
/// 一次 attempt（含网络重试）中 panic 都会跳过它，`running` 永久卡
/// true——此后 submit / NewSession / open_session 全被 busy 守卫拒绝、
/// Kotlin 侧 wakelock（观察 running）永久持有，只有重启 App 可解。
/// 故回合体（attempt 循环 + 终态构造）整体包进 `catch_unwind`：
/// panic 被拦截后走**同一套** [`finalize_turn`] 强制收尾，并广播
/// `TurnError(kind=other)`（panic 不触发网络重试，直接终态）。
///
/// manager 以 `&mut` 借用进被保护闭包（`AssertUnwindSafe` 放行）：
/// panic 展开只作废引用、不销毁值——所有权留在闭包外的本帧，拦截后
/// 仍能送回 core。注意这依赖 `panic = "unwind"`（workspace 的
/// `[profile.mobile]` 为 FFI 防线已显式保证）。
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
        let mut manager = manager;
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.block_on(engine_turn(
                state.clone(),
                &mut manager,
                run_id,
                provider,
                history,
                cancel,
            ))
        }));
        let terminal = match caught {
            Ok(terminal) => terminal,
            Err(payload) => {
                let summary = panic_payload_summary(payload.as_ref());
                tracing::error!("回合 panic 已拦截（{summary}），强制收尾");
                TurnTerminal::Error {
                    message: format!("内部错误：回合发生 panic（已恢复，可重试）；{summary}"),
                    kind: Some(TurnErrorKind::Other),
                }
            }
        };
        finalize_turn(&state, manager, terminal);
    })
}

/// 回合终态（收尾时在同一 core 锁内广播）。正常 / 失败 / panic 拦截
/// 三路共用 [`finalize_turn`]。`pub` 供 mobile `resume_turn`（askuser）
/// 同构复用。
pub enum TurnTerminal {
    /// 成功：history/stats 已由引擎侧折叠完毕，只剩广播。
    Ok(Box<TurnSummaryDto>),
    /// 失败（含 panic 拦截）：message 是人读通道；kind 是机器通道
    /// （`None` = 未分类——resume 路径既有语义）。
    Error {
        message: String,
        kind: Option<TurnErrorKind>,
    },
}

/// 回合收尾（正常 / 失败 / panic 拦截三路共用，提取为函数防两份漂移）：
/// 持 core 锁复位运行态 + manager 回家 + 广播终态。**先复位后广播**——
/// 客户端收到 TurnOk/TurnError 时 busy 守卫已放行，可立即提交下一
/// 回合。`pub` 供 mobile `resume_turn` 收尾复用（同款唯一恢复点）。
pub fn finalize_turn(state: &Arc<AppState>, manager: RunManager, terminal: TurnTerminal) {
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
    let mut manager = manager;
    manager.agent_tree = inner.agent_tree.clone();
    inner.manager = Some(manager);
    match terminal {
        TurnTerminal::Ok(summary) => {
            state.sink.broadcast(ServerMsg::TurnOk { summary });
        }
        TurnTerminal::Error { message, kind } => {
            state.sink.broadcast(ServerMsg::TurnError { message, kind });
        }
    }
}

/// panic 载荷摘要上限（字符；CJK 安全按字符截断）。
const PANIC_SUMMARY_MAX_CHARS: usize = 200;

/// panic 载荷 → 人读摘要：`&str` / `String` 载荷取原文（按字符截
/// [`PANIC_SUMMARY_MAX_CHARS`]），其他类型给占位。用于 panic 拦截后的
/// TurnError message 与 error 日志（panic hook 已先打过 PANIC 栈，这里
/// 只补「已拦截，强制收尾」语义）。`pub` 供 mobile `resume_turn` 复用。
pub fn panic_payload_summary(payload: &(dyn std::any::Any + Send)) -> String {
    let raw = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        return "<非文本 panic 载荷>".to_owned();
    };
    raw.chars().take(PANIC_SUMMARY_MAX_CHARS).collect()
}

/// 回合级断连重试预算：每回合至多 1 次自动重跑（后台/网络抖动 →
/// reqwest decode 错 → `ProviderError::ConnectionError` 的链路在
/// adapter 层「首个 Delta 后不重试」——部分文本已流出，只能整回合
/// 重跑，见 model-genai retry.rs）。
const NETWORK_RETRY_BUDGET: u32 = 1;

/// 会话级网络重试冷却窗：两次网络重试至少间隔 10s，防网络瘫痪时
/// 逐回合空转重试（照 mcp_recovery 的 10s 冷却先例）。存于
/// [`crate::state::CoreInner::last_network_retry`]。
pub(crate) const NETWORK_RETRY_COOLDOWN: Duration = Duration::from_secs(10);

/// 断连重试的转写标记：部分流出文本保留在转写里（诚实展示），其后
/// 追加本 Meta 说明该段输出已被重试作废。
const RETRY_META_TEXT: &str = "网络中断，已重试本回合";

/// 回合失败 → 机器可读分类（[`TurnErrorKind`]，随 `TurnError` 广播）。
/// 优先类型化下钻（`OpenSlateError` → Provider 变体结构）；唯一的字符串
/// 匹配兜底是 `RetryExhausted`——adapter 层重试耗尽时把最后一个错误
/// Display 成纯文本（`last: String`），无类型可钻。脆弱点：内层文案
/// 变更会让分类退化为 `Other`（只影响分类展示，不影响正确性）。
fn classify_turn_error(e: &OpenSlateError) -> TurnErrorKind {
    use openslate_core::error::{OpenSlateError as E, ProviderError as P, RuntimeError as R};
    match e {
        // 诊断链路的主形态：流中途传输失败（reqwest decode 错）。
        E::Provider(P::ConnectionError(_)) => TurnErrorKind::Network,
        E::Provider(P::RateLimit) | E::Provider(P::ServerError(429)) => TurnErrorKind::RateLimit,
        E::Provider(P::AuthError(_))
        | E::Provider(P::ServerError(401))
        | E::Provider(P::ServerError(403)) => TurnErrorKind::Auth,
        // 见 fn 文档：内层仅剩 Display 字符串，字符串匹配兜底。
        E::Provider(P::RetryExhausted { last, .. }) => {
            if last.contains("connection error") {
                TurnErrorKind::Network
            } else if last.contains("rate limit") || last.contains("429") {
                TurnErrorKind::RateLimit
            } else {
                TurnErrorKind::Other
            }
        }
        // 模型侧失败：响应畸形 / 模型不存在 / 空响应。
        E::Provider(P::MalformedResponse(_)) | E::Provider(P::NotFound(_)) => TurnErrorKind::Model,
        E::Runtime(R::EmptyResponse { .. }) => TurnErrorKind::Model,
        _ => TurnErrorKind::Other,
    }
}

/// 本次失败是否值得自动重跑整个回合。全部条件：
/// - 分类为 `Network`（唯一可重试类别；RateLimit/Auth/Model/Other、
///   审批拒绝、工具失败（不进 Err 路径）均不重试）；
/// - 本回合重试预算未耗尽（[`NETWORK_RETRY_BUDGET`]）；
/// - 用户未取消（取消与断连同刻竞态进 Err 分支时，尊重取消意图）；
/// - 不在会话级冷却窗内（[`NETWORK_RETRY_COOLDOWN`]）。
fn should_network_retry(
    state: &Arc<AppState>,
    e: &OpenSlateError,
    retries_used: u32,
    cancel: &CancellationToken,
) -> bool {
    if retries_used >= NETWORK_RETRY_BUDGET {
        return false;
    }
    if cancel.is_cancelled() {
        return false;
    }
    if classify_turn_error(e) != TurnErrorKind::Network {
        return false;
    }
    let inner = state.core.lock();
    match inner.last_network_retry {
        Some(last) if last.elapsed() < NETWORK_RETRY_COOLDOWN => false,
        _ => true,
    }
}

/// 回合体（attempt 循环 + 终态构造）。**不做收尾**——运行态复位 /
/// manager 回家 / 终态广播统一在 [`finalize_turn`]，由
/// [`spawn_engine_turn`] 的 catch_unwind 外层在正常与 panic 两条路径
/// 上各调一次（panic 不触发网络重试：重试循环本身在 catch 保护内，
/// 展开即直接落到拦截分支的终态）。
async fn engine_turn(
    state: Arc<AppState>,
    manager: &mut RunManager,
    run_id: RunId,
    provider: Box<dyn ModelProvider>,
    history: Vec<Message>,
    cancel: CancellationToken,
) -> TurnTerminal {
    let started = std::time::Instant::now();
    let mut bridge = SessionProgressBridge {
        state: state.clone(),
    };
    // 回合级断连重试循环：Err 时按 [`should_network_retry`] 判定是否整
    // 回合重跑（user 消息在 history 未动——Err 路径不改 inner.history，
    // 重跑即全新生成）。重试期间 running 保持 true（UI 转圈不停），
    // manager 回收 / 运行态复位只在最终收尾做一次。
    let mut network_retries: u32 = 0;
    let result = loop {
        let attempt = manager
            .execute_with_run_id(
                run_id.clone(),
                provider.as_ref(),
                &history,
                cancel.clone(),
                Some(&mut bridge),
            )
            .await;
        match attempt {
            Err(e) if should_network_retry(&state, &e, network_retries, &cancel) => {
                network_retries += 1;
                // 部分流出文本：保留 + Meta 标记（先改转写镜像、再广播，
                // 与 ProgressCallback 同一顺序不变量），随后重跑回合。
                {
                    let mut inner = state.core.lock();
                    flush_stream(&mut inner);
                    inner.transcript.push(EntryDto::Meta {
                        text: RETRY_META_TEXT.to_owned(),
                    });
                    inner.last_network_retry = Some(std::time::Instant::now());
                }
                crate::session_event!("turn error (network), retrying: {e}");
                state.sink.broadcast(ServerMsg::Notice {
                    text: "网络中断，正在自动重试…".into(),
                    level: NoticeLevel::Info,
                });
                continue;
            }
            result => break result,
        }
    };

    // 终态构造：各自分支短锁折叠产物（history/stats / flush），广播
    // 交给 finalize_turn（同一收尾时序：先复位后广播）。
    match result {
        Ok(r) => {
            let dto = {
                let mut inner = state.core.lock();
                inner.history = r.messages.clone();
                flush_stream(&mut inner);
                fold_tool_outcomes(&mut inner, &r.messages);
                inner.stats.turns += 1;
                inner.stats.total_input_tokens += r.total_input_tokens;
                inner.stats.total_output_tokens += r.total_output_tokens;
                inner.stats.total_cost_usd += r.total_cost_usd;
                TurnSummaryDto::from(r)
            };
            crate::session_event!(
                "turn ok: {}s · ↑{} ↓{}",
                started.elapsed().as_secs(),
                dto.total_input_tokens,
                dto.total_output_tokens
            );
            TurnTerminal::Ok(Box::new(dto))
        }
        Err(e) => {
            // 失败保留 history 现状（TUI 同款：用户消息留着可重发）。
            {
                let mut inner = state.core.lock();
                flush_stream(&mut inner);
            }
            let kind = classify_turn_error(&e);
            crate::session_event!("turn error: {e}");
            // 重试用尽的信息并入文案（协议 message 是人读通道，kind 是
            // 机器通道）。
            let mut message = e.to_string();
            if network_retries > 0 {
                message.push_str(&format!("（已自动重试 {network_retries} 次）"));
            }
            TurnTerminal::Error {
                message,
                kind: Some(kind),
            }
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
            attachments: None,
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
            attachments: None,
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
            image_tokens: None,
        };
        assert_eq!(meta_line_for_test(&plain), "↑10 ↓2");
        let cached = Usage {
            input_tokens: 10,
            output_tokens: 2,
            cached_input_tokens: Some(4),
            reasoning_tokens: None,
            image_tokens: None,
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
                    attachments: None,
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
                last_network_retry: None,
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

    // ── 回合级断连自动重试（engine_turn Err 分支）──────────────────────

    mod network_retry {
        use super::*;
        use crate::approval::SessionApprovalBridge;
        use crate::state::{ConfigPaths, CoreInner, MsgSink, ProviderFactory, SessionCore};
        use openslate_core::agent_tree::AgentTree;
        use openslate_core::config::{parse_openslate_toml, AgentsConfig};
        use openslate_core::error::ProviderError;
        use openslate_core::provider::GenerateRequest;
        use openslate_core::run_manager::RunManager;
        use openslate_core::skills::SkillsCatalog;
        use openslate_core::tool::ToolRegistry;
        use openslate_core::types::{
            AgentConfig, AgentId, ModelResponse, ModelStreamEvent, Usage,
        };
        use std::collections::VecDeque;
        use std::sync::Mutex as StdMutex;
        use tokio_util::sync::CancellationToken;

        /// 一段流脚本：元素为流事件或流中途错误（错误即该次尝试失败）。
        type Script = Vec<Result<ModelStreamEvent, ProviderError>>;

        /// 断连注入 provider：每次 `generate_stream` 按序弹出一段脚本；
        /// 调用计数走共享句柄供断言。`cancel_on_first` 在第一段脚本
        /// 开播前取消 token（「取消不触发重试」用例）。
        struct FlakyProvider {
            scripts: StdMutex<VecDeque<Script>>,
            calls: Arc<StdMutex<u32>>,
            cancel_on_first: Option<CancellationToken>,
        }

        #[async_trait::async_trait]
        impl ModelProvider for FlakyProvider {
            async fn generate(
                &self,
                _req: GenerateRequest,
            ) -> Result<ModelResponse, ProviderError> {
                Err(ProviderError::ServerError(500))
            }

            async fn generate_stream(
                &self,
                _req: GenerateRequest,
            ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
                let n = {
                    let mut c = self.calls.lock().unwrap();
                    *c += 1;
                    *c
                };
                let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
                let token = self.cancel_on_first.clone();
                let (tx, rx) = tokio::sync::mpsc::channel(64);
                tokio::spawn(async move {
                    if n == 1 {
                        if let Some(t) = &token {
                            t.cancel();
                        }
                    }
                    for ev in script {
                        let _ = tx.send(ev).await;
                    }
                });
                rx
            }

            fn provider_name(&self) -> &str {
                "flaky"
            }
        }

        /// 记录型 sink：捕获 broadcast 供断言。
        #[derive(Default)]
        struct RecordingSink {
            broadcasted: StdMutex<Vec<ServerMsg>>,
        }

        impl MsgSink for RecordingSink {
            fn broadcast(&self, msg: ServerMsg) {
                self.broadcasted.lock().unwrap().push(msg);
            }
            fn send_to(&self, _conn_id: u64, _msg: ServerMsg) -> bool {
                true
            }
            fn conn_ids(&self) -> Vec<u64> {
                vec![1]
            }
            fn count(&self) -> usize {
                1
            }
            fn close_all(&self) {}
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        fn text_done(text: &str) -> Script {
            vec![
                Ok(ModelStreamEvent::Delta(text.to_owned())),
                Ok(ModelStreamEvent::Usage(Usage {
                    input_tokens: 8,
                    output_tokens: 4,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                    image_tokens: None,
                })),
                Ok(ModelStreamEvent::Done(ModelResponse {
                    content: Some(text.to_owned()),
                    tool_calls: vec![],
                    reasoning_content: None,
                    usage: None,
                    finish_reason: Some("stop".into()),
                })),
            ]
        }

        /// 流中途断连：先流出部分文本，再连接错误（诊断链路的复刻：
        /// reqwest decode 错 → WebStream → ConnectionError）。
        fn drop_mid_stream(partial: &str) -> Script {
            vec![
                Ok(ModelStreamEvent::Delta(partial.to_owned())),
                Err(ProviderError::ConnectionError(
                    "reqwest error: decode error".into(),
                )),
            ]
        }

        fn user_msg(content: &str) -> Message {
            Message {
                attachments: None,
                role: MessageRole::User,
                content: content.to_owned(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            }
        }

        /// 装配可跑 engine_turn 的 state + manager（模型别名 main 必须
        /// 可解析——execute_with_run_id 会 resolve 根代理的模型）。
        /// provider 由调用方注入 engine_turn（脚本/取消形态各异）。
        fn assemble() -> (Arc<AppState>, Arc<RecordingSink>, RunManager) {
            let config = parse_openslate_toml(
                r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "m"
"#,
            )
            .expect("fixture parses");
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
            let manager = RunManager::new(
                config.clone(),
                AgentTree::from_configs(&agents.agents).expect("tree builds"),
                ToolRegistry::new(),
                SkillsCatalog::default(),
            );
            let sink = Arc::new(RecordingSink::default());
            let factory: ProviderFactory = Arc::new(|_cfg, _alias| {
                Err(anyhow::anyhow!("engine tests inject the provider directly"))
            });
            let inner = CoreInner {
                session_id: "retry-test-session".into(),
                session_label: "retry-test".into(),
                history: vec![user_msg("继续")],
                transcript: Vec::new(),
                running: true,
                compacting: false,
                depth_cur: 0,
                agents_running: 0,
                tool_calls_cur: 0,
                model_alias: "main".into(),
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
                last_network_retry: None,
            };
            let state = Arc::new(AppState {
                core: Arc::new(SessionCore::new(inner)),
                sink: sink.clone() as Arc<dyn MsgSink>,
                approval: Arc::new(SessionApprovalBridge::new(sink.clone() as Arc<dyn MsgSink>)),
                auth_token: None,
                provider_factory: factory,
                origin: "test",
                _mcp: Default::default(),
            });
            (state, sink, manager)
        }

        fn flaky(
            scripts: Vec<Script>,
            cancel_on_first: Option<CancellationToken>,
        ) -> (Box<dyn ModelProvider>, Arc<StdMutex<u32>>) {
            let calls = Arc::new(StdMutex::new(0u32));
            (
                Box::new(FlakyProvider {
                    scripts: StdMutex::new(scripts.into()),
                    calls: Arc::clone(&calls),
                    cancel_on_first,
                }),
                calls,
            )
        }

        /// 跑一个回合（engine_turn 直调 + 手动收尾，复刻 spawn 闭包的
        /// 正常路径；panic 防线测试走 spawn_engine_turn 真路径）。
        async fn run_turn(
            scripts: Vec<Script>,
        ) -> (Arc<AppState>, Arc<RecordingSink>, Arc<StdMutex<u32>>) {
            let (provider, calls) = flaky(scripts, None);
            let (state, sink, mut manager) = assemble();
            let history = state.core.lock().history.clone();
            let terminal = engine_turn(
                state.clone(),
                &mut manager,
                openslate_core::types::RunId("retry-test-run".into()),
                provider,
                history,
                CancellationToken::new(),
            )
            .await;
            finalize_turn(&state, manager, terminal);
            (state, sink, calls)
        }

        fn turn_error(sink: &RecordingSink) -> Option<(String, Option<TurnErrorKind>)> {
            sink.broadcasted.lock().unwrap().iter().find_map(|m| {
                if let ServerMsg::TurnError { message, kind } = m {
                    Some((message.clone(), *kind))
                } else {
                    None
                }
            })
        }

        fn has_retry_notice(sink: &RecordingSink) -> bool {
            sink.broadcasted
                .lock()
                .unwrap()
                .iter()
                .any(|m| matches!(m, ServerMsg::Notice { text, .. } if text.contains("自动重试")))
        }

        fn transcript_texts(state: &Arc<AppState>) -> Vec<String> {
            state.core
                .lock()
                .transcript
                .iter()
                .map(|e| match e {
                    EntryDto::Meta { text }
                    | EntryDto::Assistant { text }
                    | EntryDto::Reasoning { text }
                    | EntryDto::User { text, .. } => text.clone(),
                    _ => String::new(),
                })
                .collect()
        }

        #[tokio::test]
        async fn network_error_retries_and_succeeds() {
            // 第一次流中途断连（部分文本已流出），第二次成功 → 回合成功、
            // 转写保留部分文本 + Meta 标记、无 TurnError。
            let (state, sink, calls) = run_turn(vec![
                drop_mid_stream("部分流出"),
                text_done("完整回答"),
            ])
            .await;

            assert_eq!(*calls.lock().unwrap(), 2, "one retry after the drop");
            assert!(
                sink.broadcasted.lock().unwrap().iter().any(|m| matches!(m, ServerMsg::TurnOk { .. })),
                "turn must succeed after retry"
            );
            assert!(turn_error(&sink).is_none(), "no TurnError expected");
            assert!(has_retry_notice(&sink), "retry Notice broadcast");

            let texts = transcript_texts(&state);
            assert!(texts.contains(&"部分流出".to_owned()), "partial text kept: {texts:?}");
            assert!(texts.contains(&RETRY_META_TEXT.to_owned()), "Meta marker: {texts:?}");
            assert!(texts.contains(&"完整回答".to_owned()), "final answer: {texts:?}");

            // 重跑是全新生成：history = user + 完整回答（部分文本不进
            // history——Err 尝试的产物被丢弃）。
            let history = state.core.lock().history.clone();
            assert_eq!(history.len(), 2);
            assert_eq!(history[1].content, "完整回答");

            // 冷却时间戳已记账（下一回合 10s 内不再网络重试）。
            assert!(state.core.lock().last_network_retry.is_some());

            // manager 回家、running 复位。
            let inner = state.core.lock();
            assert!(inner.manager.is_some());
            assert!(!inner.running);
        }

        #[tokio::test]
        async fn retry_budget_is_one() {
            // 两次都断 → 重试 1 次后走终态：TurnError kind=network，
            // message 含重试字样。
            let (state, sink, calls) =
                run_turn(vec![drop_mid_stream("甲"), drop_mid_stream("乙")]).await;

            assert_eq!(*calls.lock().unwrap(), 2, "budget = 1 retry (2 attempts total)");
            let (message, kind) = turn_error(&sink).expect("TurnError terminal");
            assert_eq!(kind, Some(TurnErrorKind::Network));
            assert!(message.contains("已自动重试 1 次"), "message: {message}");
            assert!(has_retry_notice(&sink));
            let texts = transcript_texts(&state);
            assert!(texts.contains(&RETRY_META_TEXT.to_owned()), "Meta marker: {texts:?}");

            // 失败保留 history 现状（user 消息仍在，可重发）。
            let history = state.core.lock().history.clone();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].role, MessageRole::User);
        }

        #[tokio::test]
        async fn non_network_error_does_not_retry() {
            // Auth 错误（401/403/key 无效类）不重试：1 次调用即终态。
            let (state, sink, calls) = run_turn(vec![vec![Err(ProviderError::AuthError(
                "bad key".into(),
            ))]])
            .await;

            assert_eq!(*calls.lock().unwrap(), 1, "auth errors are not retried");
            let (message, kind) = turn_error(&sink).expect("TurnError terminal");
            assert_eq!(kind, Some(TurnErrorKind::Auth));
            assert!(!message.contains("已自动重试"), "message: {message}");
            assert!(!has_retry_notice(&sink));
            assert!(!transcript_texts(&state).contains(&RETRY_META_TEXT.to_owned()));
        }

        #[tokio::test]
        async fn cooldown_window_suppresses_retry() {
            // 会话级冷却：上一刻刚发生过网络重试（时间戳距今 < 10s 窗）
            // → 本次断连不再重试，直接终态（防网络瘫痪时空转）。
            let (provider, calls) = flaky(vec![drop_mid_stream("部分")], None);
            let (state, sink, mut manager) = assemble();
            state.core.lock().last_network_retry = Some(std::time::Instant::now());
            let history = state.core.lock().history.clone();
            let terminal = engine_turn(
                state.clone(),
                &mut manager,
                openslate_core::types::RunId("cooldown-run".into()),
                provider,
                history,
                CancellationToken::new(),
            )
            .await;
            finalize_turn(&state, manager, terminal);

            assert_eq!(*calls.lock().unwrap(), 1, "cooldown suppresses the retry");
            let (message, kind) = turn_error(&sink).expect("TurnError terminal");
            assert_eq!(kind, Some(TurnErrorKind::Network));
            assert!(!message.contains("已自动重试"), "message: {message}");
            assert!(!has_retry_notice(&sink));
            assert!(!transcript_texts(&state).contains(&RETRY_META_TEXT.to_owned()));
        }

        #[tokio::test]
        async fn cancel_does_not_trigger_retry() {
            // 取消与断连同刻竞态：token 在第一段脚本开播前被取消，脚本
            // 仍报流中途 ConnectionError。无论 runtime 把它收成
            // Interrupted（Ok 路径）还是 Err（ConnectionError），都不得
            // 重试：恰好 1 次调用、无重试 Notice、无 Meta 标记。
            let cancel = CancellationToken::new();
            let (provider, calls) = flaky(vec![drop_mid_stream("部分")], Some(cancel.clone()));
            let (state, sink, mut manager) = assemble();
            let history = state.core.lock().history.clone();
            let terminal = engine_turn(
                state.clone(),
                &mut manager,
                openslate_core::types::RunId("cancel-run".into()),
                provider,
                history,
                cancel,
            )
            .await;
            finalize_turn(&state, manager, terminal);

            assert_eq!(*calls.lock().unwrap(), 1, "cancel suppresses the retry");
            assert!(!has_retry_notice(&sink));
            assert!(!transcript_texts(&state).contains(&RETRY_META_TEXT.to_owned()));
            // 回合必须收尾（TurnOk=Interrupted 或 TurnError，取决于
            // select 竞态胜负——两者都是「不重试的终态」）。
            let broadcasted = sink.broadcasted.lock().unwrap().clone();
            assert!(
                broadcasted
                    .iter()
                    .any(|m| matches!(m, ServerMsg::TurnOk { .. } | ServerMsg::TurnError { .. })),
                "turn must terminate: {:?}",
                broadcasted
                    .iter()
                    .map(|m| match m {
                        ServerMsg::TurnOk { .. } => "turn_ok".to_owned(),
                        ServerMsg::TurnError { .. } => "turn_error".to_owned(),
                        ServerMsg::Notice { .. } => "notice".to_owned(),
                        _ => "other".to_owned(),
                    })
                    .collect::<Vec<_>>()
            );
        }

        // ── 分类（classify_turn_error）单元 ──────────────────────────

        #[test]
        fn classification_typed_drilldown() {
            use openslate_core::error::{OpenSlateError as E, ProviderError as P, RuntimeError as R};
            let cases: Vec<(E, TurnErrorKind)> = vec![
                (E::from(P::ConnectionError("reset".into())), TurnErrorKind::Network),
                (
                    E::from(P::RetryExhausted {
                        attempts: 3,
                        last: "connection error: connection reset by peer".into(),
                    }),
                    TurnErrorKind::Network,
                ),
                (
                    E::from(P::RetryExhausted {
                        attempts: 3,
                        last: "rate limit exceeded".into(),
                    }),
                    TurnErrorKind::RateLimit,
                ),
                (E::from(P::RateLimit), TurnErrorKind::RateLimit),
                (E::from(P::ServerError(429)), TurnErrorKind::RateLimit),
                (E::from(P::AuthError("bad key".into())), TurnErrorKind::Auth),
                (E::from(P::ServerError(401)), TurnErrorKind::Auth),
                (E::from(P::ServerError(403)), TurnErrorKind::Auth),
                (
                    E::from(P::MalformedResponse("bad json".into())),
                    TurnErrorKind::Model,
                ),
                (E::from(P::NotFound("model-x".into())), TurnErrorKind::Model),
                (
                    E::from(R::EmptyResponse {
                        step: 1,
                        agent_id: "root".into(),
                        model_alias: "main".into(),
                    }),
                    TurnErrorKind::Model,
                ),
                (
                    E::from(R::MaxStepsExceeded { max: 100 }),
                    TurnErrorKind::Other,
                ),
                (E::from(P::Timeout), TurnErrorKind::Other),
                (E::Validation("bad".into()), TurnErrorKind::Other),
            ];
            for (err, want) in cases {
                assert_eq!(classify_turn_error(&err), want, "error: {err}");
            }
        }
    }

    // ── panic 防线（catch_unwind 强制收尾，exp-10）───────────────────

    mod panic_guard {
        use super::*;
        use crate::approval::SessionApprovalBridge;
        use crate::state::{ConfigPaths, CoreInner, MsgSink, ProviderFactory, SessionCore};
        use openslate_core::agent_tree::AgentTree;
        use openslate_core::config::{parse_openslate_toml, AgentsConfig};
        use openslate_core::error::ProviderError;
        use openslate_core::provider::GenerateRequest;
        use openslate_core::run_manager::RunManager;
        use openslate_core::skills::SkillsCatalog;
        use openslate_core::tool::ToolRegistry;
        use openslate_core::types::{AgentConfig, AgentId, ModelResponse};
        use openslate_protocol::ClientMsg;
        use std::sync::Mutex as StdMutex;

        /// 开播即 panic 的 provider：panic 发生在流 future 的 poll 路径
        /// 上（工具 panic 已被 runtime `execute_tool_safely` 隔离，能
        /// 到达引擎防线的真实链路就是 provider 侧），经 block_on 栈展开
        /// 到 spawn 闭包的 catch_unwind。
        struct PanickingProvider;

        #[async_trait::async_trait]
        impl ModelProvider for PanickingProvider {
            async fn generate(
                &self,
                _req: GenerateRequest,
            ) -> Result<ModelResponse, ProviderError> {
                Err(ProviderError::ServerError(500))
            }

            async fn generate_stream(
                &self,
                _req: GenerateRequest,
            ) -> tokio::sync::mpsc::Receiver<Result<openslate_core::types::ModelStreamEvent, ProviderError>>
            {
                panic!("scripted provider panic");
            }

            fn provider_name(&self) -> &str {
                "panic-stub"
            }
        }

        /// 记录型 sink：捕获 broadcast 供断言（busy 拒绝走 send_to，若
        /// 被拒则 broadcasted 里不会出现 SessionReset——正好区分）。
        #[derive(Default)]
        struct RecordingSink {
            broadcasted: StdMutex<Vec<ServerMsg>>,
        }

        impl MsgSink for RecordingSink {
            fn broadcast(&self, msg: ServerMsg) {
                self.broadcasted.lock().unwrap().push(msg);
            }
            fn send_to(&self, _conn_id: u64, _msg: ServerMsg) -> bool {
                true
            }
            fn conn_ids(&self) -> Vec<u64> {
                vec![1]
            }
            fn count(&self) -> usize {
                1
            }
            fn close_all(&self) {}
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        fn user_msg(content: &str) -> Message {
            Message {
                attachments: None,
                role: MessageRole::User,
                content: content.to_owned(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            }
        }

        /// 装配可跑 spawn_engine_turn 的 state + manager（running=true
        /// 起步，复刻回合进行中被 panic 打断的形态）。
        fn assemble() -> (Arc<AppState>, Arc<RecordingSink>, RunManager) {
            let config = parse_openslate_toml(
                r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "m"
"#,
            )
            .expect("fixture parses");
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
            let manager = RunManager::new(
                config.clone(),
                AgentTree::from_configs(&agents.agents).expect("tree builds"),
                ToolRegistry::new(),
                SkillsCatalog::default(),
            );
            let sink = Arc::new(RecordingSink::default());
            let factory: ProviderFactory = Arc::new(|_cfg, _alias| {
                Err(anyhow::anyhow!("panic tests inject the provider directly"))
            });
            let inner = CoreInner {
                session_id: "panic-test-session".into(),
                session_label: "panic-test".into(),
                history: vec![user_msg("触发 panic")],
                transcript: Vec::new(),
                running: true,
                compacting: false,
                depth_cur: 0,
                agents_running: 0,
                tool_calls_cur: 0,
                model_alias: "main".into(),
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
                last_network_retry: None,
            };
            let state = Arc::new(AppState {
                core: Arc::new(SessionCore::new(inner)),
                sink: sink.clone() as Arc<dyn MsgSink>,
                approval: Arc::new(SessionApprovalBridge::new(sink.clone() as Arc<dyn MsgSink>)),
                auth_token: None,
                provider_factory: factory,
                origin: "test",
                _mcp: Default::default(),
            });
            (state, sink, manager)
        }

        /// 跑一个 panic 回合（经 spawn_engine_turn 生产真路径，含
        /// catch_unwind 防线）。
        async fn run_panicking_turn()
        -> (Arc<AppState>, Arc<RecordingSink>, tokio::task::JoinHandle<()>) {
            let (state, sink, manager) = assemble();
            let history = state.core.lock().history.clone();
            let handle = spawn_engine_turn(
                state.clone(),
                manager,
                openslate_core::types::RunId("panic-run".into()),
                Box::new(PanickingProvider),
                history,
                tokio_util::sync::CancellationToken::new(),
            );
            (state, sink, handle)
        }

        fn turn_error(sink: &RecordingSink) -> Option<(String, Option<TurnErrorKind>)> {
            sink.broadcasted.lock().unwrap().iter().find_map(|m| {
                if let ServerMsg::TurnError { message, kind } = m {
                    Some((message.clone(), *kind))
                } else {
                    None
                }
            })
        }

        #[tokio::test]
        async fn panic_is_contained_and_finalized() {
            // 回合中 provider panic → catch_unwind 拦截：spawn 闭包不上
            // 抛（JoinHandle ok）、TurnError(kind=other, message 含
            // panic 字样)、running=false / cancel=None / manager 回家。
            let (state, sink, handle) = run_panicking_turn().await;
            handle
                .await
                .expect("panic must be contained by the guard, not propagate");

            let (message, kind) = turn_error(&sink).expect("TurnError after contained panic");
            assert_eq!(kind, Some(TurnErrorKind::Other));
            assert!(message.contains("panic"), "message: {message}");
            assert!(message.contains("已恢复"), "message: {message}");

            let inner = state.core.lock();
            assert!(!inner.running, "running must be reset (the very bug)");
            assert!(inner.cancel.is_none(), "cancel must be cleared");
            assert!(inner.manager.is_some(), "manager must be back home");
            assert!(!inner.busy(), "busy guard must be released");
        }

        #[tokio::test]
        async fn new_session_succeeds_after_panic() {
            // 回归断言（本漏洞）：panic 前若收尾被跳过，running 卡 true
            // → NewSession 被 busy 守卫拒绝。防线生效后必须能立即新建。
            let (state, sink, handle) = run_panicking_turn().await;
            handle.await.expect("panic contained");

            crate::session::dispatch(&state, 1, ClientMsg::NewSession).await;
            let broadcasted = sink.broadcasted.lock().unwrap().clone();
            assert!(
                broadcasted
                    .iter()
                    .any(|m| matches!(m, ServerMsg::SessionReset)),
                "NewSession must succeed after a contained panic, got: {:?}",
                broadcasted
                    .iter()
                    .map(|m| match m {
                        ServerMsg::SessionReset => "session_reset".to_owned(),
                        ServerMsg::TurnError { .. } => "turn_error".to_owned(),
                        ServerMsg::Notice { .. } => "notice".to_owned(),
                        _ => "other".to_owned(),
                    })
                    .collect::<Vec<_>>()
            );
        }

        #[test]
        fn panic_payload_summary_formats_and_truncates() {
            let s = panic_payload_summary(&"boom".to_owned());
            assert_eq!(s, "boom");
            assert_eq!(panic_payload_summary(&String::from("炸了")), "炸了");
            // 按字符截 200（CJK 安全），不按字节。
            let long = String::from("啪").repeat(500);
            assert_eq!(
                panic_payload_summary(&long).chars().count(),
                PANIC_SUMMARY_MAX_CHARS
            );
            // 非文本载荷 → 占位。
            assert!(panic_payload_summary(&42u8).contains("panic"));
        }
    }
}
