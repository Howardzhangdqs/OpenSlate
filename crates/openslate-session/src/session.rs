//! `ClientMsg` 派发与 submit 全流程（传输无关；原 server ws.rs 的读循环
//! 处理逻辑，剥离 WS 类型后的同一状态机）。
//!
//! WS server 的读任务与 mobile 的 FFI `send()` 都调用 [`dispatch`]；
//! notice 经 [`MsgSink::send_to`] 定向回发起连接。

use std::sync::Arc;

use openslate_protocol::{
    ApprovalAnswerChoice, ClientMsg, EntryDto, NoticeLevel, ServerMsg, SessionStatsDto,
};

use crate::config_ops::{apply_config_change, apply_set_api_key, ConfigChange};
use crate::engine::{run_auto_compact, spawn_engine_turn};
use crate::state::{build_snapshot, flush_stream, AppState};

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
        ClientMsg::Submit { text } => handle_submit(state, conn_id, text).await,
        ClientMsg::ApprovalAnswer { id, choice } => {
            // 先 bridge（拿走首答权）再 core（推转写 + 广播）——锁序见
            // state.rs 模块文档。
            match state.approval.respond(id, choice) {
                Some(summary) => {
                    let label = match choice {
                        ApprovalAnswerChoice::Approve => "approved",
                        ApprovalAnswerChoice::Deny => "denied",
                        ApprovalAnswerChoice::ApproveAll => "approve-all",
                    };
                    crate::session_event!("approval: {label} (conn {conn_id})");
                    {
                        let mut inner = state.core.lock();
                        flush_stream(&mut inner);
                        inner
                            .transcript
                            .push(EntryDto::Approval {
                                tool_name: summary.tool_name.clone(),
                                decision: label.to_owned(),
                            });
                    }
                    state.sink.broadcast(ServerMsg::ApprovalResolved {
                        id,
                        choice: choice_str(choice).to_owned(),
                    });
                }
                None => {
                    notice(state, conn_id, "已由其他客户端应答".into(), NoticeLevel::Info);
                }
            }
        }
        ClientMsg::Cancel => {
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
            let old_run = {
                let mut inner = state.core.lock();
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
                inner.stats = SessionStatsDto::default();
                inner.session_run.take()
            };
            // 关掉旧 run 行（异步落库，不挡广播）。
            if let Some(run) = old_run {
                let cost = { state.core.lock().stats.total_cost_usd };
                let recorder = run.recorder.clone();
                tokio::spawn(async move {
                    if let Err(e) = recorder.finish("completed", None, cost).await {
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
                None => notice(
                    state,
                    conn_id,
                    "未支持的消息类型".into(),
                    NoticeLevel::Warn,
                ),
            }
        }
    }
}

/// submit 全流程（TUI start_turn 移植 + auto-compact）。
async fn handle_submit(state: &Arc<AppState>, conn_id: u64, text: String) {
    let text_trimmed = text.trim().to_owned();
    if text_trimmed.is_empty() {
        return;
    }

    // 1. 占位守卫 + 推用户消息（running 先置位防竞态）。
    let user_message = {
        let mut inner = state.core.lock();
        if inner.busy() {
            drop(inner);
            notice(state, conn_id, "回合进行中".into(), NoticeLevel::Warn);
            return;
        }
        inner.tool_calls_cur = 0;
        inner.depth_cur = 0;
        let user_message = openslate_core::types::Message {
            role: openslate_core::types::MessageRole::User,
            content: text_trimmed.clone(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        };
        inner.history.push(user_message.clone());
        inner
            .transcript
            .push(EntryDto::User { text: text_trimmed });
        inner.running = true;
        user_message
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
    let session_run = open_session_run(state, Some(title_hint.as_str())).await;

    // 3. 用户消息落库（引擎侧逐条追加走同一 recorder，seq 连续）。
    if let Some(run) = &session_run {
        if let Err(e) = run.recorder.write_message(&user_message).await {
            tracing::warn!("failed to persist user message: {e}");
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

fn new_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn notice(state: &Arc<AppState>, conn_id: u64, text: String, level: NoticeLevel) {
    tracing::debug!("notice → conn {conn_id}: {text}");
    state.sink.send_to(conn_id, ServerMsg::Notice { text, level });
}

fn choice_str(choice: ApprovalAnswerChoice) -> &'static str {
    match choice {
        ApprovalAnswerChoice::Approve => "approve",
        ApprovalAnswerChoice::Deny => "deny",
        ApprovalAnswerChoice::ApproveAll => "approve_all",
    }
}
