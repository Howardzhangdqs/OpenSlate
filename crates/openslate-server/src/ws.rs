//! WS 连接生命周期与 ClientMsg 派发（spec §3/§4）。
//!
//! 握手：首条必须是 hello（proto=1 + token 校验），失败发 error 后
//! close；成功则在 **core 锁内** 注册连接 + 入队 snapshot（与引擎回调
//! 的「持锁广播」互斥，保证 snapshot 严格先于其后的事件）。
//!
//! 每连接两个任务：写任务独占 sink（drain 单队列 → 同一连接全序），
//! 读任务处理 ClientMsg。Ping 由读任务转写任务回 Pong。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use openslate_protocol::{
    ApprovalAnswerChoice, ClientMsg, NoticeLevel, ServerMsg, PROTOCOL_VERSION,
};
use tokio::sync::mpsc;

use crate::config_ops::{apply_config_change, apply_set_api_key, ConfigChange};
use crate::engine::{run_auto_compact, spawn_engine_turn};
use crate::state::{build_snapshot, flush_stream, AppState};

/// 写任务出站帧（ServerMsg 或协议控制帧）。
enum OutFrame {
    Msg(Arc<ServerMsg>),
    Pong(Vec<u8>),
    Close,
}

pub async fn ws_handler(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_connection(state, socket))
}

/// 握手失败的统一拒绝路径：发 error 后 close。
async fn deny_and_close(socket: &mut WebSocket, code: &str, message: String) {
    let _ = socket
        .send(Message::text(
            serde_json::to_string(&ServerMsg::Error {
                code: code.to_owned(),
                message,
            })
            .unwrap_or_default(),
        ))
        .await;
    let _ = socket.close().await;
}

async fn handle_connection(state: Arc<AppState>, mut socket: WebSocket) {
    // ── 握手：hello（30s 超时防挂死连接）─────────────────────────────
    let first = match tokio::time::timeout(Duration::from_secs(30), socket.recv()).await {
        Ok(Some(Ok(msg))) => msg,
        _ => {
            let _ = socket
                .send(Message::text(
                    serde_json::to_string(&ServerMsg::Error {
                        code: "bad_message".into(),
                        message: "等待 hello 超时或读取失败，连接关闭".into(),
                    })
                    .unwrap_or_default(),
                ))
                .await;
            let _ = socket.close().await;
            return;
        }
    };
    let hello: Option<ClientMsg> = match first {
        Message::Text(text) => serde_json::from_str::<ClientMsg>(&text).ok(),
        _ => None,
    };
    let denied = deny_and_close;
    match hello {
        Some(ClientMsg::Hello { proto, token }) => {
            if proto != PROTOCOL_VERSION {
                denied(
                    &mut socket,
                    "proto_mismatch",
                    format!("协议版本不符：server={PROTOCOL_VERSION} client={proto}"),
                )
                .await;
                return;
            }
            let token_ok = match &state.auth_token {
                Some(expected) => token.as_deref() == Some(expected.as_str()),
                None => true,
            };
            if !token_ok {
                denied(&mut socket, "bad_token", "token 校验失败".into()).await;
                return;
            }
        }
        Some(_) => {
            denied(&mut socket, "bad_message", "首条消息必须是 hello".into()).await;
            return;
        }
        None => {
            denied(&mut socket, "bad_message", "hello 消息无法解析".into()).await;
            return;
        }
    }

    // ── 注册 + snapshot（core 锁内原子完成，序保证见模块文档）────────
    // 通道拓扑：hub 广播 → per-conn hub 队列 → forwarder → 写队列 →
    // sink。snapshot 直入写队列（严格首条），其后事件经 forwarder 保序。
    let (writer_tx, writer_rx) = mpsc::unbounded_channel::<OutFrame>();
    let conn_id = {
        let inner = state.core.lock();
        let (tx, mut hub_rx) = mpsc::unbounded_channel::<Arc<ServerMsg>>();
        let id = state.hub.register(tx);
        let _ = writer_tx.send(OutFrame::Msg(Arc::new(ServerMsg::Snapshot {
            session: Box::new(build_snapshot(&inner, &state.approval)),
        })));
        let wtx = writer_tx.clone();
        tokio::spawn(async move {
            // hub 队列 → 写队列（unregister 后 recv 返回 None 自然退出）。
            while let Some(msg) = hub_rx.recv().await {
                if wtx.send(OutFrame::Msg(msg)).is_err() {
                    break;
                }
            }
        });
        id
    };
    tracing::info!("client {conn_id} attached ({} online)", state.hub.count());
    crate::stdout_event!("client connected (n={})", state.hub.count());

    let (mut sink, mut stream) = socket.split();

    // 写任务：唯一 sink 持有者。
    let writer = tokio::spawn(async move {
        let mut rx = writer_rx;
        while let Some(frame) = rx.recv().await {
            let ok = match frame {
                OutFrame::Msg(msg) => {
                    let text = serde_json::to_string(&*msg).unwrap_or_else(|_| {
                        serde_json::to_string(&ServerMsg::Error {
                            code: "encode".into(),
                            message: "消息编码失败".into(),
                        })
                        .unwrap_or_default()
                    });
                    sink.send(Message::text(text)).await.is_ok()
                }
                OutFrame::Pong(payload) => sink.send(Message::Pong(payload.into())).await.is_ok(),
                OutFrame::Close => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            };
            if !ok {
                break;
            }
        }
    });

    // 读循环。
    while let Some(frame) = stream.next().await {
        let msg = match frame {
            Ok(m) => m,
            Err(_) => break,
        };
        match msg {
            Message::Text(text) => {
                let client_msg: ClientMsg = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = writer_tx.send(OutFrame::Msg(Arc::new(ServerMsg::Notice {
                            text: format!("无法解析的消息：{e}"),
                            level: NoticeLevel::Error,
                        })));
                        continue;
                    }
                };
                dispatch(&state, conn_id, client_msg, &writer_tx).await;
            }
            Message::Ping(payload) => {
                let _ = writer_tx.send(OutFrame::Pong(payload.to_vec()));
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // 掉线清理：退订 + 结束写任务。
    state.hub.unregister(conn_id);
    let _ = writer_tx.send(OutFrame::Close);
    let _ = writer.await;
    tracing::info!("client {conn_id} detached ({} online)", state.hub.count());
    crate::stdout_event!("client disconnected (n={})", state.hub.count());
}

/// ClientMsg 派发。
async fn dispatch(
    state: &Arc<AppState>,
    conn_id: u64,
    msg: ClientMsg,
    writer_tx: &mpsc::UnboundedSender<OutFrame>,
) {
    let notice = |text: String, level: NoticeLevel| {
        let _ = writer_tx.send(OutFrame::Msg(Arc::new(ServerMsg::Notice { text, level })));
    };
    match msg {
        ClientMsg::Hello { .. } => {
            notice("连接已建立，重复握手忽略".into(), NoticeLevel::Info);
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
                    crate::stdout_event!("approval: {label} (client {conn_id})");
                    {
                        let mut inner = state.core.lock();
                        flush_stream(&mut inner);
                        inner
                            .transcript
                            .push(openslate_protocol::EntryDto::Approval {
                                tool_name: summary.tool_name.clone(),
                                decision: label.to_owned(),
                            });
                    }
                    state.hub.broadcast(ServerMsg::ApprovalResolved {
                        id,
                        choice: choice_str(choice).to_owned(),
                    });
                }
                None => {
                    notice("已由其他客户端应答".into(), NoticeLevel::Info);
                }
            }
        }
        ClientMsg::Cancel => {
            let inner = state.core.lock();
            if let Some(cancel) = &inner.cancel {
                cancel.cancel();
            } else {
                drop(inner);
                notice("当前没有进行中的回合".into(), NoticeLevel::Info);
            }
        }
        ClientMsg::NewSession => {
            let old_run = {
                let mut inner = state.core.lock();
                if inner.busy() {
                    drop(inner);
                    notice(
                        "回合进行中，无法新建会话（先 Ctrl+C 取消）".into(),
                        NoticeLevel::Warn,
                    );
                    return;
                }
                inner.history.clear();
                inner.transcript.clear();
                inner.session_id = new_session_id();
                inner.stats = openslate_protocol::SessionStatsDto::default();
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
            state.hub.broadcast(ServerMsg::SessionReset);
            for id in state.hub.conn_ids() {
                let snap = {
                    let inner = state.core.lock();
                    ServerMsg::Snapshot {
                        session: Box::new(build_snapshot(&inner, &state.approval)),
                    }
                };
                state.hub.send_to(id, snap);
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
                    format!("未知模型别名 '{alias}'（可用：{}）", available.join(", ")),
                    NoticeLevel::Error,
                );
                return;
            }
            {
                let mut inner = state.core.lock();
                inner.model_alias = alias.clone();
            }
            state.hub.broadcast(ServerMsg::ModelChanged { alias });
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
                None => notice("未支持的消息类型".into(), NoticeLevel::Warn),
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
    let notice = |t: &str| {
        state.hub.send_to(
            conn_id,
            ServerMsg::Notice {
                text: t.to_owned(),
                level: NoticeLevel::Warn,
            },
        );
    };

    // 1. 占位守卫 + 推用户消息（running 先置位防竞态）。
    let user_message = {
        let mut inner = state.core.lock();
        if inner.busy() {
            drop(inner);
            notice("回合进行中");
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
        };
        inner.history.push(user_message.clone());
        inner
            .transcript
            .push(openslate_protocol::EntryDto::User { text: text_trimmed });
        inner.running = true;
        user_message
    };

    // stdout 单行事件：prompt 截 40 字符（CJK 安全，按字符截）。
    {
        let prompt = &user_message.content;
        let mut head: String = prompt.chars().take(40).collect();
        if prompt.chars().count() > 40 {
            head.push('…');
        }
        crate::stdout_event!("turn: {head}");
    }

    // 2. lazy 打开持久化 run（无 store / 失败 → None = 本会话不落库）。
    let session_run = open_session_run(state).await;

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
            state.hub.send_to(
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
        state.hub.broadcast(ServerMsg::TurnError {
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
async fn open_session_run(state: &Arc<AppState>) -> Option<crate::state::SessionRun> {
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
    match openslate_store_sqlite::recorder::RunRecorder::begin(
        store,
        run_id.clone(),
        &root_agent_id,
        Some("server session"),
        r#"{"kind":"server"}"#,
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

fn choice_str(choice: ApprovalAnswerChoice) -> &'static str {
    match choice {
        ApprovalAnswerChoice::Approve => "approve",
        ApprovalAnswerChoice::Deny => "deny",
        ApprovalAnswerChoice::ApproveAll => "approve_all",
    }
}
