//! `ClientMsg` 派发与 submit 全流程（传输无关；原 server ws.rs 的读循环
//! 处理逻辑，剥离 WS 类型后的同一状态机）。
//!
//! WS server 的读任务与 mobile 的 FFI `send()` 都调用 [`dispatch`]；
//! notice 经 [`MsgSink::send_to`] 定向回发起连接。

use std::sync::Arc;

use openslate_core::approval::ToolApprovalLevel;
use openslate_protocol::{
    ApprovalAnswerChoice, ClientMsg, EntryDto, NoticeLevel, ServerMsg, SessionStatsDto,
};

use crate::config_ops::{apply_approval_rules, apply_config_change, apply_set_api_key, ConfigChange};
use crate::engine::{run_auto_compact, spawn_engine_turn};
use crate::state::{build_snapshot, flush_stream, AppState};
use openslate_store_sqlite::recorder::RunFinal;

/// ClientMsg 派发。
pub async fn dispatch(state: &Arc<AppState>, conn_id: u64, msg: ClientMsg) {
    match msg {
        ClientMsg::Hello { .. } => {
            notice(
                state,
                conn_id,
                "连接已建立，重复握手忽略".into(),
                NoticeLevel::Info,
            );
        }
        // v4 多模态：`attachments` 引用随 Submit 透传（L4 接线；门控在
        // mobile dispatch 层，此处只构造消息/转写，不解释内容）。
        ClientMsg::Submit {
            text,
            attachments,
        } => handle_submit(state, conn_id, text, attachments).await,
        ClientMsg::ApprovalAnswer { id, choice } => {
            // 先 bridge（拿走首答权）再 core（推转写 + 广播）——锁序见
            // state.rs 模块文档。
            match state.approval.respond(id, choice) {
                Some(summary) => {
                    let label = match choice {
                        ApprovalAnswerChoice::Approve => "approved",
                        ApprovalAnswerChoice::Deny => "denied",
                        ApprovalAnswerChoice::ApproveAll => "approve-all",
                        // v3 新 choice 的转写行文案（定稿线格式，
                        // tool-approval P1a；行为接线在 P1b）。
                        ApprovalAnswerChoice::AlwaysTool => "always-tool",
                        ApprovalAnswerChoice::SessionMcp => "session-mcp",
                        ApprovalAnswerChoice::AlwaysMcp => "always-mcp",
                    };
                    crate::session_event!("approval: {label} (conn {conn_id})");
                    {
                        let mut inner = state.core.lock();
                        flush_stream(&mut inner);
                        inner.transcript.push(EntryDto::Approval {
                            tool_name: summary.tool_name.clone(),
                            decision: label.to_owned(),
                        });
                    }
                    state.sink.broadcast(ServerMsg::ApprovalResolved {
                        id,
                        choice: choice_str(choice).to_owned(),
                    });
                    // (b)/(d) 一直允许 → 持久化 + rules 热更（tool-approval
                    // P1b）。规则增量 = 当前生效 rules + 对应域写 Allow，
                    // 经 config_ops 共享更新链落盘（整段重写为新 schema，
                    // legacy policy/tools 键随之消除）。本次调用已由
                    // bridge 按 Approved 放行，新规则作用于下一次 check。
                    match choice {
                        ApprovalAnswerChoice::AlwaysTool => {
                            let mut rules = state.approval.rules_snapshot();
                            rules
                                .tools
                                .insert(summary.tool_name.clone(), ToolApprovalLevel::Allow);
                            if apply_approval_rules(state, conn_id, rules).is_none() {
                                tracing::warn!(
                                    target: "openslate_approval",
                                    "always_tool persistence failed for '{}' (this call \
                                     still approved)",
                                    summary.tool_name
                                );
                            }
                        }
                        ApprovalAnswerChoice::AlwaysMcp => match summary.server.clone() {
                            Some(server) => {
                                let mut rules = state.approval.rules_snapshot();
                                rules.servers.insert(server, ToolApprovalLevel::Allow);
                                if apply_approval_rules(state, conn_id, rules).is_none() {
                                    tracing::warn!(
                                        target: "openslate_approval",
                                        "always_mcp persistence failed for '{}' (this call \
                                         still approved)",
                                        summary.server.as_deref().unwrap_or_default()
                                    );
                                }
                            }
                            // server 缺失（非 MCP 请求发来了 always_mcp）：
                            // 本次已放行，无作用域可持久化 → 定向 warn。
                            None => {
                                notice(
                                    state,
                                    conn_id,
                                    format!(
                                        "工具 {} 无 MCP server 作用域，「一直允许该 MCP」\
                                         仅本次生效",
                                        summary.tool_name
                                    ),
                                    NoticeLevel::Warn,
                                );
                            }
                        },
                        _ => {}
                    }
                }
                None => {
                    notice(
                        state,
                        conn_id,
                        "已由其他客户端应答".into(),
                        NoticeLevel::Info,
                    );
                }
            }
        }
        ClientMsg::QuestionReply {
            request_id,
            action,
            answers,
        } => {
            // ask_user 应答转发（ask-user r5）：桥在 core 锁外调用（宿主
            // 实现内部会取 core 锁做续跑，方向必须 core 内不套桥），
            // 首答者赢的幂等语义同 approval_answer。
            let label = match action {
                openslate_protocol::QuestionReplyAction::Accept => "accept",
                openslate_protocol::QuestionReplyAction::Decline => "decline",
            };
            match state.core.question_handler() {
                Some(handler) if handler.on_reply(&request_id, action, answers) => {
                    crate::session_event!("question reply: {request_id} ({label})");
                }
                Some(_) => {
                    notice(
                        state,
                        conn_id,
                        "该问题已被应答或已关闭".into(),
                        NoticeLevel::Info,
                    );
                }
                None => {
                    notice(state, conn_id, "提问通道未接入".into(), NoticeLevel::Warn);
                }
            }
        }
        ClientMsg::Cancel => {
            // ask_user 关卷（best-effort）：先让宿主把悬挂问题按 cancelled
            // 收尾（引擎 select! 对已取消 token 有偏置，真实结果不保证
            // 落地；确定性兜底是下一 turn 前的 superseded 修复，见
            // handle_submit），再取消 token。
            if let Some(handler) = state.core.question_handler() {
                handler.on_cancel_all();
            }
            let inner = state.core.lock();
            if let Some(cancel) = &inner.cancel {
                cancel.cancel();
            } else {
                drop(inner);
                notice(
                    state,
                    conn_id,
                    "当前没有进行中的回合".into(),
                    NoticeLevel::Info,
                );
            }
        }
        ClientMsg::NewSession => {
            // 原 busy 守卫（先行拒绝，避免无谓冲刷）。
            {
                let inner = state.core.lock();
                if inner.busy() {
                    drop(inner);
                    notice(
                        state,
                        conn_id,
                        "回合进行中，无法新建会话（先取消）".into(),
                        NoticeLevel::Warn,
                    );
                    return;
                }
            }
            // 前置冲刷（历史转写丢失修复 C1）：清内存转写之前把未落库
            // 尾部强制落库（含回合中推迟的 ToolCall 中断快照 + 收尾
            // Meta 行，见 mobile transcripts.rs flush_now）。best-effort：
            // 失败在实现内 log，不阻断新建会话。
            if let Some(flusher) = state.core.transcript_flusher() {
                flusher.flush(state.clone()).await;
            }
            let (old_run, final_cost, final_in, final_out) = {
                let mut inner = state.core.lock();
                // 二次 busy 守卫（C1）：flush 期间 submit 可能已起回合
                //——此刻忙则拒绝，转写清空留给下一次（先取消）。
                if inner.busy() {
                    drop(inner);
                    notice(
                        state,
                        conn_id,
                        "回合进行中，无法新建会话（先取消）".into(),
                        NoticeLevel::Warn,
                    );
                    return;
                }
                inner.history.clear();
                inner.transcript.clear();
                inner.session_id = new_session_id();
                // v5 L2：终态聚合（cost + tokens）快照必须先于 stats 重置
                // 读取——重置后读恒为 0（REPL /new 同款快照顺序回归）。
                // stats 自上次 /new 起累计 = 该 run 生命周期累计（含
                // title/compact 侧调用，与 server shutdown 口径一致）。
                let final_cost = inner.stats.total_cost_usd;
                let final_in = inner.stats.total_input_tokens;
                let final_out = inner.stats.total_output_tokens;
                inner.stats = SessionStatsDto::default();
                (
                    inner.session_run.take(),
                    final_cost,
                    final_in,
                    final_out,
                )
            };
            // 会话作用域审批集合清空（approve_all / session_mcp 写入的
            // 内存白名单；「当前会话」语义——P1a 摸底确认旧实现不清是
            // 漏洞，P1b 补上）。bridge 自锁，在 core 锁外调用（锁序
            // 红线：先 bridge 后 core）。持久化规则（always_*）不随会话
            // 重置——「一直允许」跨会话。
            state.approval.clear_session_scopes();
            // 关掉旧 run 行（异步落库，不挡广播）。v5 L2：RunFinal 携带
            // run 生命周期 token 聚合，落 runs.input_tokens/output_tokens。
            if let Some(run) = old_run {
                let recorder = run.recorder.clone();
                tokio::spawn(async move {
                    if let Err(e) = recorder
                        .finish_with(RunFinal {
                            status: "completed".into(),
                            output_json: None,
                            cost_usd: final_cost,
                            input_tokens: final_in,
                            output_tokens: final_out,
                        })
                        .await
                    {
                        tracing::warn!("failed to persist previous session run: {e}");
                    }
                });
            }
            // 广播 reset；随后每连接各补一份新 snapshot（同锁序）。
            state.sink.broadcast(ServerMsg::SessionReset);
            for id in state.sink.conn_ids() {
                let snap = {
                    let inner = state.core.lock();
                    ServerMsg::Snapshot {
                        session: Box::new(build_snapshot(&inner, &state.approval)),
                    }
                };
                state.sink.send_to(id, snap);
            }
        }
        ClientMsg::SetModel { alias } => {
            let known = {
                let inner = state.core.lock();
                inner.config.models.contains_key(&alias)
                    || openslate_core::model_config::resolve_model(&inner.config, &alias).is_ok()
            };
            if !known {
                let available: Vec<String> = {
                    let inner = state.core.lock();
                    let mut keys: Vec<String> = inner.config.levels.keys().cloned().collect();
                    keys.extend(inner.config.models.keys().cloned());
                    keys.sort();
                    keys.dedup();
                    keys
                };
                notice(
                    state,
                    conn_id,
                    format!("未知模型别名 '{alias}'（可用：{}）", available.join(", ")),
                    NoticeLevel::Error,
                );
                return;
            }
            {
                let mut inner = state.core.lock();
                inner.model_alias = alias.clone();
            }
            state.sink.broadcast(ServerMsg::ModelChanged { alias });
        }
        ClientMsg::SetApiKey { provider, value } => {
            apply_set_api_key(state, conn_id, &provider, &value);
        }
        other => {
            // 配置 CRUD 家族。
            match ConfigChange::from_msg(other) {
                Some(change) => {
                    apply_config_change(state, conn_id, change);
                }
                None => notice(state, conn_id, "未支持的消息类型".into(), NoticeLevel::Warn),
            }
        }
    }
}

/// submit 全流程（TUI start_turn 移植 + auto-compact）。
async fn handle_submit(
    state: &Arc<AppState>,
    conn_id: u64,
    text: String,
    attachments: Option<Vec<openslate_protocol::AttachmentDto>>,
) {
    let text_trimmed = text.trim().to_owned();
    if text_trimmed.is_empty() {
        return;
    }

    // 1. 占位守卫 + 推用户消息（running 先置位防竞态）。
    let (user_message, repaired_dangling) = {
        let mut inner = state.core.lock();
        if inner.busy() {
            drop(inner);
            notice(state, conn_id, "回合进行中".into(), NoticeLevel::Warn);
            return;
        }
        inner.tool_calls_cur = 0;
        inner.depth_cur = 0;
        // ask-user 悬挂修复（r5）：新 turn 开始前，对所有悬挂 tool call
        // 合成结果（ask_user → superseded；同批兄弟 → interrupted 兜底
        // 文案），保证消息序合法（provider 拒绝 dangling tool_calls）。
        // 合成消息在 step 3 先于用户消息落库。无悬挂时零成本。
        let repaired_dangling = openslate_core::ask_user::repair_dangling_tool_calls(
            &mut inner.history,
            openslate_core::ask_user::DanglingClosure::Superseded,
        );
        if !repaired_dangling.is_empty() {
            crate::session_event!(
                "ask_user repair: {} dangling call(s) superseded",
                repaired_dangling.len()
            );
        }
        // v4 多模态（L4）：附件引用进 user 消息（持久化形状 = {id,mime,
        // filename}，data 由 mobile 请求路径装载）；无附件行为不变。
        let core_attachments = attachments.as_ref().map(|dtos| {
            dtos.iter()
                .map(|d| openslate_core::types::Attachment {
                    id: d.id.clone(),
                    mime: d.mime.clone(),
                    filename: d.filename.clone(),
                    data: None,
                })
                .collect::<Vec<_>>()
        });
        let user_message = openslate_core::types::Message {
            attachments: core_attachments,
            role: openslate_core::types::MessageRole::User,
            content: text_trimmed.clone(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        };
        inner.history.push(user_message.clone());
        // v4 多模态（L4）：transcript User 条目携带引用（UI 缩略图）。
        inner.transcript.push(EntryDto::User {
            text: text_trimmed,
            attachments,
        });
        inner.running = true;
        (user_message, repaired_dangling)
    };

    // 单行运行时事件：prompt 截 40 字符（CJK 安全，按字符截）。
    // 摘要同时作为 run 标题落库（见 open_session_run）——历史会话列表
    // 直接读 run.title，免逐 run 查 messages 的 N+1。
    let title_hint = {
        let prompt = &user_message.content;
        let mut head: String = prompt.chars().take(40).collect();
        if prompt.chars().count() > 40 {
            head.push('…');
        }
        crate::session_event!("turn: {head}");
        head
    };

    // 2. lazy 打开持久化 run（无 store / 失败 → None = 本会话不落库）。
    // had_run：本条是否是该 run 的首条消息（此前无 run）——只有首条
    // 才值得花一次 title 模型调用生成标题。
    let had_run = {
        let inner = state.core.lock();
        inner.session_run.is_some()
    };
    let session_run = open_session_run(state, Some(title_hint.as_str())).await;

    // 3. 用户消息落库（引擎侧逐条追加走同一 recorder，seq 连续）。
    // ask_user 悬挂修复合成的 tool result 在用户消息之前（历史顺序）。
    if let Some(run) = &session_run {
        for msg in &repaired_dangling {
            if let Err(e) = run.recorder.write_message(msg).await {
                tracing::warn!("failed to persist dangling repair message: {e}");
            }
        }
        if let Err(e) = run.recorder.write_message(&user_message).await {
            tracing::warn!("failed to persist user message: {e}");
        }
    }

    // 3.5 首条消息 → 异步 LLM 标题（capability=title）。占位标题（prompt
    // 截断）此刻已在库，生成失败/别名缺失即保留占位；成功则替换并广播
    // RunTitle（历史列表原位更新）。与主回合并行，不阻塞首 token。
    if !had_run {
        if let Some(run) = &session_run {
            let task_state = Arc::clone(state);
            let task_run_id = run.run_id.0.clone();
            let first_prompt = user_message.content.clone();
            tokio::spawn(async move {
                generate_session_title(&task_state, task_run_id, first_prompt).await;
            });
        }
    }

    // 3. pre-turn auto-compact（超过上下文阈值先摘要压缩）。
    {
        let need = {
            let inner = state.core.lock();
            let limits = crate::state::effective_limits(&inner.config);
            limits.auto_compact
                && openslate_core::context_manager::needs_compact(
                    &inner.history,
                    limits.max_context_messages as usize,
                    limits.max_context_bytes as usize,
                    0,
                )
        };
        if need {
            {
                let mut inner = state.core.lock();
                inner.compacting = true;
            }
            run_auto_compact(state).await;
            {
                let mut inner = state.core.lock();
                inner.compacting = false;
            }
        }
    }

    // 4. provider 构建（失败回滚用户消息，回合作废——TUI 同款）。
    let (config, model_alias) = {
        let inner = state.core.lock();
        (inner.config.clone(), inner.model_alias.clone())
    };
    let provider = match (state.provider_factory)(&config, &model_alias) {
        Ok(p) => p,
        Err(e) => {
            let mut inner = state.core.lock();
            if matches!(inner.history.last(), Some(m) if m.role == openslate_core::types::MessageRole::User)
            {
                inner.history.pop();
            }
            inner.running = false;
            drop(inner);
            state.sink.send_to(
                conn_id,
                ServerMsg::Notice {
                    text: format!("provider 构建失败：{e}"),
                    level: NoticeLevel::Error,
                },
            );
            return;
        }
    };

    // 5. 引擎 spawn（manager 所有权进任务，回合结束自动回传）。
    let mut inner = state.core.lock();
    let run_id = inner
        .session_run
        .as_ref()
        .map(|r| r.run_id.clone())
        .unwrap_or_else(openslate_core::run_manager::RunManager::new_run_id);
    inner.stream = crate::state::StreamBuffers::default();
    let cancel = tokio_util::sync::CancellationToken::new();
    inner.cancel = Some(cancel.clone());
    if let Some(manager) = inner.manager.as_mut() {
        manager.message_sink = session_run.as_ref().map(|r| {
            r.recorder.clone() as std::sync::Arc<dyn openslate_core::runtime::MessageSink>
        });
    }
    let Some(manager) = inner.manager.take() else {
        inner.running = false;
        drop(inner);
        state.sink.broadcast(ServerMsg::TurnError {
            message: "manager unavailable".into(),
            kind: None,
        });
        return;
    };
    let history = inner.history.clone();
    let handle = spawn_engine_turn(state.clone(), manager, run_id, provider, history, cancel);
    inner.engine_task = Some(handle);
}

/// lazy 打开持久化 run：已有 → clone；没有 → begin 新 run 并存回
/// core（此刻 running=true，独占会话状态，无并发写者）。
///
/// `title_hint`：本回合 prompt 的 40 字符摘要，begin 时直接作为 run
/// 标题落库（历史会话列表渲染用，免 N+1 查询）。`None` = 调用方无
/// 摘要，退回 origin 占位标题（desktop/server 现状语义不变）。
async fn open_session_run(
    state: &Arc<AppState>,
    title_hint: Option<&str>,
) -> Option<crate::state::SessionRun> {
    {
        let inner = state.core.lock();
        if let Some(run) = &inner.session_run {
            return Some(run.clone());
        }
    }
    let (store, root_agent_id) = {
        let inner = state.core.lock();
        match inner.store.clone() {
            Some(store) => (store, inner.agent_tree.get_root().id.0.clone()),
            None => return None,
        }
    };
    let run_id = openslate_core::run_manager::RunManager::new_run_id();
    let (origin_label, origin_meta) = match state.origin {
        "mobile" => ("mobile session", r#"{"kind":"mobile"}"#),
        _ => ("server session", r#"{"kind":"server"}"#),
    };
    // begin 的第 4 参即 title：有 prompt 摘要用摘要；否则保持 origin
    // 占位（兼容 desktop/server 调用方）。
    let title = title_hint.or(Some(origin_label));
    match openslate_store_sqlite::recorder::RunRecorder::begin(
        store,
        run_id.clone(),
        &root_agent_id,
        title,
        origin_meta,
    )
    .await
    {
        Ok(recorder) => {
            let run = crate::state::SessionRun {
                run_id,
                recorder: Arc::new(recorder),
            };
            let mut inner = state.core.lock();
            // 单写者（running 守卫），直接存回。
            inner.session_run = Some(run.clone());
            Some(run)
        }
        Err(e) => {
            tracing::warn!("session persistence unavailable ({e}); running unpersisted");
            None
        }
    }
}

/// 首条消息后的异步 LLM 标题生成（capability=title）。
///
/// 计划构建（capability → 代号 → provider）任一步失败 → 静默返回：
/// run 开始时的 prompt 截断占位标题已在库，这与 compact 的机械兜底
/// 同语义（降级可用，永不报错）。成功路径：替换 run.title 落库 →
/// 广播 `RunTitle`（历史列表原位更新）。usage 计入会话累计（同
/// compact：不进 run 行）。
async fn generate_session_title(state: &Arc<AppState>, run_id: String, first_prompt: String) {
    use openslate_core::model_config::{capability_alias, resolve_model, CAP_TITLE};

    let (factory, config, store) = {
        let inner = state.core.lock();
        (
            state.provider_factory.clone(),
            inner.config.clone(),
            inner.store.clone(),
        )
    };
    let alias = capability_alias(&config, CAP_TITLE);
    let Some(resolved) = resolve_model(&config, &alias).ok() else {
        tracing::debug!("title alias '{alias}' unresolvable; keep mechanical title");
        return;
    };
    let Ok(provider) = factory(&config, &alias) else {
        tracing::debug!("title provider build failed; keep mechanical title");
        return;
    };
    let pricing = resolved.cost_spec();

    let (title, usage) =
        crate::engine::generate_title(provider.as_ref(), &resolved.model_id, &first_prompt).await;

    if let Some(usage) = usage {
        let mut inner = state.core.lock();
        inner.stats.total_input_tokens += usage.input_tokens as u64;
        inner.stats.total_output_tokens += usage.output_tokens as u64;
        inner.stats.total_cost_usd += pricing.cost_of(&usage);
    }

    let Some(title) = title else {
        return;
    };
    let Some(store) = store else {
        return;
    };
    // run 可能已被删（生成期间用户长按删除）：UPDATE 影响零行也成功
    // 返回，此时广播无意义——保守起见仍统一广播（客户端按 run_id
    // 原位更新，目标不存在即自然丢弃）。
    if let Err(e) = store.update_run_title(&run_id, &title).await {
        tracing::warn!("failed to persist generated title: {e}");
        return;
    }
    state.sink.broadcast(ServerMsg::RunTitle { run_id, title });
}

/// 新会话 id（uuid v4 simple 形态）。pub 供 mobile open_session 的会话
/// 身份轮换复用（C2：换 run 即换会话——session_id 变化驱动 ChatScreen
/// 的会话揭示）。
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn notice(state: &Arc<AppState>, conn_id: u64, text: String, level: NoticeLevel) {
    tracing::debug!("notice → conn {conn_id}: {text}");
    state
        .sink
        .send_to(conn_id, ServerMsg::Notice { text, level });
}

fn choice_str(choice: ApprovalAnswerChoice) -> &'static str {
    match choice {
        ApprovalAnswerChoice::Approve => "approve",
        ApprovalAnswerChoice::Deny => "deny",
        ApprovalAnswerChoice::ApproveAll => "approve_all",
        // v3 新 choice 的广播拼写（ApprovalResolved.choice 值域扩展，
        // tool-approval P1a 定稿线格式）。
        ApprovalAnswerChoice::AlwaysTool => "always-tool",
        ApprovalAnswerChoice::SessionMcp => "session-mcp",
        ApprovalAnswerChoice::AlwaysMcp => "always-mcp",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::SessionApprovalBridge;
    use crate::question::QuestionHandler;
    use crate::state::{ConfigPaths, CoreInner, MsgSink, ProviderFactory, SessionCore};
    use openslate_core::agent_tree::AgentTree;
    use openslate_core::config::{parse_openslate_toml, AgentsConfig};
    use openslate_core::provider::{GenerateRequest, ModelProvider};
    use openslate_core::types::{
        AgentConfig, AgentId, MessageRole, ModelResponse, ToolCall, ToolCallId,
    };
    use openslate_protocol::QuestionReplyAction;
    use std::sync::Mutex as StdMutex;

    /// 记录型 sink：捕获 broadcast / send_to 消息供断言。
    #[derive(Default)]
    struct RecordingSink {
        broadcasted: StdMutex<Vec<ServerMsg>>,
        sent: StdMutex<Vec<(u64, ServerMsg)>>,
    }

    impl MsgSink for RecordingSink {
        fn broadcast(&self, msg: ServerMsg) {
            self.broadcasted.lock().unwrap().push(msg);
        }
        fn send_to(&self, conn_id: u64, msg: ServerMsg) -> bool {
            self.sent.lock().unwrap().push((conn_id, msg));
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

    /// 桩 provider（handle_submit 走到 manager 检查即可，不产生模型调用）。
    struct StubProvider;

    #[async_trait::async_trait]
    impl ModelProvider for StubProvider {
        async fn generate(
            &self,
            _req: GenerateRequest,
        ) -> Result<ModelResponse, openslate_core::error::ProviderError> {
            Ok(ModelResponse {
                content: Some("ok".into()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                usage: None,
                finish_reason: Some("stop".into()),
            })
        }
        fn provider_name(&self) -> &str {
            "stub"
        }
    }

    fn state_with(history: Vec<openslate_core::types::Message>) -> Arc<AppState> {
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
        let factory: ProviderFactory = Arc::new(|_cfg, _alias| Ok(Box::new(StubProvider)));
        let inner = CoreInner {
            session_id: "test-session".into(),
            session_label: "test".into(),
            history,
            transcript: Vec::new(),
            running: false,
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
            manager: None, // handle_submit 走到 TurnError("manager unavailable")
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
        let sink = Arc::new(RecordingSink::default());
        Arc::new(AppState {
            core: Arc::new(SessionCore::new(inner)),
            sink: sink.clone() as Arc<dyn MsgSink>,
            approval: Arc::new(SessionApprovalBridge::new(sink as Arc<dyn MsgSink>)),
            auth_token: None,
            provider_factory: factory,
            origin: "test",
            _mcp: Default::default(),
        })
    }

    /// 记录型 QuestionHandler。
    struct RecordingHandler {
        replies: StdMutex<Vec<(String, QuestionReplyAction, serde_json::Value)>>,
        cancelled: StdMutex<u32>,
        accept: bool,
    }

    impl QuestionHandler for RecordingHandler {
        fn on_reply(
            &self,
            request_id: &str,
            action: QuestionReplyAction,
            answers: serde_json::Value,
        ) -> bool {
            self.replies
                .lock()
                .unwrap()
                .push((request_id.to_owned(), action, answers));
            self.accept
        }
        fn on_cancel_all(&self) {
            *self.cancelled.lock().unwrap() += 1;
        }
    }

    #[tokio::test]
    async fn question_reply_forwards_to_handler_and_notifies_late() {
        let state = state_with(Vec::new());
        let handler = Arc::new(RecordingHandler {
            replies: StdMutex::new(Vec::new()),
            cancelled: StdMutex::new(0),
            accept: true,
        });
        state.core.set_question_handler(handler.clone());

        dispatch(
            &state,
            1,
            ClientMsg::QuestionReply {
                request_id: "q-1".into(),
                action: QuestionReplyAction::Accept,
                answers: serde_json::json!({"a": 1}),
            },
        )
        .await;
        assert_eq!(handler.replies.lock().unwrap().len(), 1);
        // 首答生效 → 无定向 notice。
        let sink = state.sink.as_any().downcast_ref::<RecordingSink>().unwrap();
        assert!(sink.sent.lock().unwrap().is_empty());

        // 首答者赢：第二份应答被拒（handler 返回 false）→ 定向 notice。
        let handler2 = Arc::new(RecordingHandler {
            replies: StdMutex::new(Vec::new()),
            cancelled: StdMutex::new(0),
            accept: false,
        });
        state.core.set_question_handler(handler2.clone());
        dispatch(
            &state,
            2,
            ClientMsg::QuestionReply {
                request_id: "q-1".into(),
                action: QuestionReplyAction::Decline,
                answers: serde_json::Value::Null,
            },
        )
        .await;
        assert_eq!(handler2.replies.lock().unwrap().len(), 1);
        let sent = sink.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "late/duplicate reply gets a notice");
        assert_eq!(sent[0].0, 2);
    }

    #[tokio::test]
    async fn question_reply_without_handler_notices() {
        let state = state_with(Vec::new());
        dispatch(
            &state,
            1,
            ClientMsg::QuestionReply {
                request_id: "q-x".into(),
                action: QuestionReplyAction::Accept,
                answers: serde_json::Value::Null,
            },
        )
        .await;
        let sink = state.sink.as_any().downcast_ref::<RecordingSink>().unwrap();
        let sent = sink.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(matches!(
            &sent[0].1,
            ServerMsg::Notice {
                level: NoticeLevel::Warn,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn cancel_notifies_question_handler() {
        let state = state_with(Vec::new());
        let handler = Arc::new(RecordingHandler {
            replies: StdMutex::new(Vec::new()),
            cancelled: StdMutex::new(0),
            accept: true,
        });
        state.core.set_question_handler(handler.clone());
        dispatch(&state, 1, ClientMsg::Cancel).await;
        assert_eq!(*handler.cancelled.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn submit_repairs_dangling_ask_user_before_user_message() {
        // 尾部悬挂的 ask_user 调用（进程中断恢复后的典型形状）。
        let history = vec![
            openslate_core::types::Message {
                attachments: None,
                role: MessageRole::User,
                content: "which db?".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            },
            openslate_core::types::Message {
                attachments: None,
                role: MessageRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                name: None,
                tool_calls: Some(vec![ToolCall {
                    id: ToolCallId("tc-q".into()),
                    name: "ask_user".into(),
                    arguments: serde_json::json!({}),
                }]),
                reasoning_content: None,
            },
        ];
        let state = state_with(history);
        // manager=None → 引擎 spawn 失败（TurnError），但 step 1 的修复
        // 已经落进 history——正好独立断言修复形状。
        dispatch(
            &state,
            1,
            ClientMsg::Submit {
                text: "改用 sled".into(),
                attachments: None,
            },
        )
        .await;

        let inner = state.core.lock();
        // user → assistant(tool_calls) → tool(superseded) → user(新消息)。
        assert_eq!(inner.history.len(), 4);
        let repaired = &inner.history[2];
        assert_eq!(repaired.role, MessageRole::Tool);
        assert_eq!(
            repaired.tool_call_id,
            Some(ToolCallId("tc-q".into())),
            "修复必须补全原 tool call"
        );
        assert_eq!(
            repaired.content,
            openslate_core::ask_user::SUPERSEDED_RESULT
        );
        assert_eq!(repaired.name.as_deref(), Some("ask_user"));
        assert_eq!(inner.history[3].role, MessageRole::User);
        assert_eq!(inner.history[3].content, "改用 sled");
        // 补全后无悬挂（消息序合法性）。
        assert!(openslate_core::ask_user::dangling_tool_calls(&inner.history).is_empty());
    }
}
