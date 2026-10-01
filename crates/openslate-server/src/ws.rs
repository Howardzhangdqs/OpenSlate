//! WS 连接生命周期与握手（spec §3/§4）；`ClientMsg` 派发本体在
//! openslate-session（传输无关）。
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
use openslate_protocol::{ClientMsg, NoticeLevel, ServerMsg, PROTOCOL_VERSION};
use tokio::sync::mpsc;

use crate::state::{build_snapshot, AppState};

/// 写任务出站帧（ServerMsg 或协议控制帧）。
enum OutFrame {
    Msg(Arc<ServerMsg>),
    Pong(Vec<u8>),
    Close,
}

/// 从传输无关的 [`MsgSink`] 下沉出具体 [`ConnectionHub`]（连接注册/退订
/// 是 WS 传输特有操作，session 核心不感知）。
fn hub(state: &Arc<AppState>) -> &crate::hub::ConnectionHub {
    state
        .sink
        .as_any()
        .downcast_ref::<crate::hub::ConnectionHub>()
        .expect("server AppState 的 sink 必须是 ConnectionHub")
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
        let id = hub(&state).register(tx);
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
    tracing::info!("client {conn_id} attached ({} online)", state.sink.count());
    crate::stdout_event!("client connected (n={})", state.sink.count());

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
                dispatch(&state, conn_id, client_msg).await;
            }
            Message::Ping(payload) => {
                let _ = writer_tx.send(OutFrame::Pong(payload.to_vec()));
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // 掉线清理：退订 + 结束写任务。
    hub(&state).unregister(conn_id);
    let _ = writer_tx.send(OutFrame::Close);
    let _ = writer.await;
    tracing::info!("client {conn_id} detached ({} online)", state.sink.count());
    crate::stdout_event!("client disconnected (n={})", state.sink.count());
}

/// ClientMsg 派发 → openslate-session（传输无关状态机）。
/// 无法解析的消息以定向 notice 回告（同原行为）。
async fn dispatch(state: &Arc<AppState>, conn_id: u64, msg: ClientMsg) {
    openslate_session::session::dispatch(state, conn_id, msg).await;
}
