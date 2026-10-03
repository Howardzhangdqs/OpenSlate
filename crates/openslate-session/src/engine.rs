//! 引擎任务：tui/event.rs `spawn_turn` 的会话核心移植 + pre-turn
//! auto-compact（源在 tui/app.rs `start_turn`/`run_compact`/
//! `generate_summary`，随 TUI 纯客户端化由本 crate 接管）。
//!
//! - RunManager **所有权回传**：submit 时从 `CoreInner` take，回合结束
//!   放回（与 TUI 同模式；manager 非 Sync，不能共享引用进异步任务）。
//! - 进度回调 = `SessionProgressBridge`：持 core 锁「先改转写镜像、再
//!   广播」，同一连接上的事件序 == 转写追加序。
//! - auto-compact：回合提交时 history 超阈值 → 先用 `fast` 模型摘要压缩
//!   （provider 失败/别名缺失 → 机械拼接兜底，core `compact()` 内建），
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
            preview: if preview.is_empty() { None } else { Some(preview.to_owned()) },
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
    manager: RunManager,
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

    // fast 模型计划（缺失/构建失败 → None → 机械兜底，core compact 内建）。
    let factory = state.provider_factory.clone();
    let summary_plan: Option<(String, Box<dyn ModelProvider>, CostSpec)> = {
        let config = state.core.lock().config.clone();
        match resolve_model(&config, "fast") {
            Ok(resolved) => match factory(&config, "fast") {
                Ok(provider) => {
                    let pricing = resolved.cost_spec();
                    Some((resolved.model_id, provider, pricing))
                }
                Err(e) => {
                    tracing::debug!("no provider for 'fast' — mechanical fallback: {e}");
                    None
                }
            },
            Err(e) => {
                tracing::debug!("no 'fast' alias — mechanical fallback: {e}");
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

/// 供单元测试：usage Meta 行格式。
#[cfg(test)]
pub(crate) fn meta_line_for_test(usage: &Usage) -> String {
    usage_meta_line(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
