//! 附件 image tokens 的**响应侧回填**（协议 v5）。
//!
//! 方向（用户拍板）：发起 API 请求之前**绝不**把附件数据发到任何服务器
//! ——导入侧零网络（见 `attachments::import_attachment`）；只有**真实模型
//! 响应**顺带携带的 image token 信息才显示（OpenAI 系 chat usage 的
//! `prompt_tokens_details.image_tokens`，vendor 补丁 #3 归一化为 genai
//! `Usage::image_input_tokens` → core `Usage::image_tokens`）。智谱 chat
//! usage 不分模态、天然无此字段 → 永远回退文件大小显示，符合预期。
//!
//! ## 语义
//!
//! - **首次调用口径**：一个回合可能含多次模型调用（agentic 循环），历史
//!   图随每轮请求全量重发、每次都会计入 usage——只有本回合**第一次**
//!   调用的 `usage.image_tokens` 才是"这张图的首次完整成本"，后续调用
//!   的值一律忽略（闩锁：首次 `ServerMsg::Usage` 定值，之后不再更新）。
//! - **单图限定**：仅当本回合 Submit 的 user 消息**恰好带 1 张图片附件**
//!   时回填；多图 usage 不分图拆分，回填哪个都是错的 → 全部不填
//!   （`tokens` 键缺省，UI 回退文件大小）。视频附件同理不填。
//! - **回合结果**：TurnOk / TurnError 都收尾（首次调用已完成即测量真实，
//!   回合后续失败不否定该值）；未捕获到值（provider 不分模态）则丢弃。
//!
//! ## 流转（避开引擎线程持 core 锁的窗口）
//!
//! [`EventSink`] 的 tap 在**广播线程**上收到 `Usage` / `TurnOk` /
//! `TurnError`（引擎广播 TurnOk 时持有 core 锁——tap 里绝不能再锁 core，
//! 否则死锁）。因此 tap 只动本结构自己的 mutex：闩值 → 回合收尾时把
//! `(attachment_id, tokens)` 推进 `completed` 队列；`transcripts::sync_loop`
//! 每个周期（活跃 300ms / 空闲 1s）在**本就持有 core 锁的快照段**里取队
//! 列、原地更新 live transcript 的 `EntryDto::User`（快照
//! `build_snapshot` 从 inner.transcript 构造 → 之后所有快照自然携带）。
//!
//! ## 持久化 / 重启可见性
//!
//! `transcript_entries` 追加式落库，回合中途 User 条目早已持久化（无
//! UPDATE API，改不动旧行）；重开会话时 [`rebuild_transcript`] 会从
//! `messages.meta_json`（每条 assistant 消息的 `MessageMeta.usage`，含
//! `image_tokens`）**重放同口径的回填**——持久化真相源是消息表，重启
//! 后 tokens 照常读回（见 `overlay_tokens_from_messages`）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use openslate_core::types::{Message, MessageRole, MessageMeta};
use openslate_protocol::{EntryDto, ServerMsg};
use openslate_store_sqlite::store::SqliteStore;

/// 回填状态机（mobile 运行时单例；`EventSink` tap 与 sync_loop 共享）。
#[derive(Debug, Default)]
pub struct AttachmentTokenBackfill {
    /// attachment_id → 实测 image tokens（first-write-wins：同一附件只
    /// 记首次测量；跨回合重复提交同 id 不覆盖——id 每次导入唯一，这里
    /// 只是防御性语义）。
    tokens: Mutex<HashMap<String, u64>>,
    /// 本回合待观测的单图附件（Submit 落地时设置）。
    pending: Mutex<Option<PendingTurn>>,
    /// 已收尾、待 sync_loop 消费（更新 live transcript）的回填队列。
    completed: Mutex<Vec<(String, u64)>>,
}

#[derive(Debug)]
struct PendingTurn {
    attachment_id: String,
    /// 首次模型调用 usage.image_tokens 的闩锁。外层 `None` = 尚无 Usage
    /// 事件；`Some(None)` = 首次调用已闩但其值就是 None（provider 不分
    /// 模态，如智谱——回合收尾不产生回填）；`Some(Some(t))` = 待回填。
    /// 一旦闩上不再更新（agentic 循环后续调用忽略）。
    latched: Option<Option<u64>>,
}

impl AttachmentTokenBackfill {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Submit 落地前标记本回合的观测目标（**单图**才调用——判定在
    /// runtime send 路径；覆盖式：新回合新目标）。
    pub fn mark_submit(&self, attachment_id: String) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(PendingTurn {
            attachment_id,
            latched: None,
        });
    }
    /// 提交未落地（busy/回滚）→ 丢弃标记（本回合与该附件无关）。
    pub fn clear_pending(&self) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// EventSink tap 入口（广播线程；只动本结构 mutex，绝不锁 core）。
    pub fn on_event(&self, msg: &ServerMsg) {
        match msg {
            // 首次调用闩值：agentic 循环后续调用的值忽略。
            ServerMsg::Usage { usage } => {
                let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(p) = guard.as_mut() {
                    if p.latched.is_none() {
                        p.latched = Some(usage.image_tokens);
                    }
                }
            }
            // 回合收尾：闩到实值 → 交 sync_loop 更新 transcript；未闩到
            // （无 Usage 事件）或首次调用即 None（不分模态）→ 丢弃。
            ServerMsg::TurnOk { .. } | ServerMsg::TurnError { .. } => {
                let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(p) = guard.take() {
                    if let Some(Some(t)) = p.latched {
                        self.completed
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((p.attachment_id, t));
                    }
                }
            }
            _ => {}
        }
    }

    /// sync_loop 消费：取走待回填列表并落入 map（first-write-wins）。
    pub fn drain_completed(&self) -> Vec<(String, u64)> {
        let batch = std::mem::take(&mut *self.completed.lock().unwrap_or_else(|e| e.into_inner()));
        if !batch.is_empty() {
            let mut map = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
            for (id, t) in &batch {
                map.entry(id.clone()).or_insert(*t);
            }
        }
        batch
    }

    /// 已回填的 token 值（测试 / 诊断用）。
    pub fn get(&self, attachment_id: &str) -> Option<u64> {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(attachment_id)
            .copied()
    }

    /// 把回填值写进 live transcript：命中 attachment id 的**最后一条**
    /// `EntryDto::User`（id 每次导入唯一，最后一条 = 本回合的气泡）；
    /// 已有值的条目不动（first-write-wins）。纯内存操作，sync_loop 在
    /// 持有 core 锁的快照段内调用。
    pub fn apply_to_transcript(transcript: &mut [EntryDto], batch: &[(String, u64)]) {
        for (id, t) in batch {
            for entry in transcript.iter_mut().rev() {
                let EntryDto::User { attachments, .. } = entry else { continue };
                let Some(atts) = attachments.as_mut() else { continue };
                for att in atts.iter_mut() {
                    if att.id == *id && att.tokens.is_none() {
                        att.tokens = Some(*t);
                    }
                }
                // id 唯一：找到最后一条含它的 User 后继续找更早的并无意义，
                // 但 rev 迭代天然先处理最新条目，全部扫完即可（重复 id 的
                // 防御路径：最新条目先填，旧条目已被 first-write 跳过）。
                let _ = id;
            }
        }
    }
}

/// 重开会话的回填重放：从 `messages` 表（`meta_json` 携带每次模型调用的
/// `MessageMeta.usage`）按**与 live 相同的口径**（单图 user 消息 → 其后
/// **第一条** assistant 消息的 `usage.image_tokens`）恢复 tokens，写进
/// 重建的 transcript 条目。transcript_entries 行追加式不可变（回填晚于
/// 落库），这里是重启后 tokens 的持久化读回路径。
pub async fn overlay_tokens_from_messages(
    store: &SqliteStore,
    run_id: &str,
    transcript: &mut Vec<EntryDto>,
) {
    let rows = match store.list_messages_by_run(run_id).await {
        Ok(rows) => rows,
        Err(e) => {
            crate::alog!("attachment_tokens: 读 run {run_id} 消息失败（跳过 tokens 回填）: {e}");
            return;
        }
    };
    let mut batch: Vec<(String, u64)> = Vec::new();
    // 当前待配对的单图附件 id（其后第一条 assistant 消息配对后清空）。
    let mut waiting: Option<String> = None;
    for row in rows {
        let Ok(message) = serde_json::from_str::<Message>(&row.content_json) else {
            continue;
        };
        match message.role {
            MessageRole::User => {
                waiting = None; // 新 user 消息重置配对窗口
                let Some(atts) = message.attachments.as_ref() else { continue };
                if atts.len() == 1 && atts[0].mime.starts_with("image/") {
                    waiting = Some(atts[0].id.clone());
                }
            }
            MessageRole::Assistant => {
                if let Some(id) = waiting.take() {
                    let tokens = row
                        .meta_json
                        .as_deref()
                        .and_then(|j| serde_json::from_str::<MessageMeta>(j).ok())
                        .and_then(|m| m.usage)
                        .and_then(|u| u.image_tokens);
                    if let Some(t) = tokens {
                        batch.push((id, t));
                    }
                }
            }
            _ => {}
        }
    }
    if !batch.is_empty() {
        AttachmentTokenBackfill::apply_to_transcript(transcript, &batch);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use openslate_core::types::{RunId, RunStatus, Usage};
    use openslate_protocol::TurnSummaryDto;
    use openslate_protocol::AttachmentDto;

    fn turn_ok() -> ServerMsg {
        ServerMsg::TurnOk {
            summary: Box::new(TurnSummaryDto {
                run_id: RunId("run-t".into()),
                status: RunStatus::Completed,
                total_steps: 1,
                total_input_tokens: 0,
                total_output_tokens: 0,
                total_cost_usd: 0.0,
                model: "test-model".into(),
                messages: Vec::new(),
            }),
        }
    }

    fn usage(image_tokens: Option<u64>) -> Usage {
        Usage {
            input_tokens: 100,
            output_tokens: 5,
            cached_input_tokens: None,
            reasoning_tokens: None,
            image_tokens,
        }
    }

    fn user_entry(atts: Vec<AttachmentDto>) -> EntryDto {
        EntryDto::User {
            text: "看图".into(),
            attachments: Some(atts),
        }
    }

    fn att(id: &str) -> AttachmentDto {
        AttachmentDto {
            id: id.into(),
            mime: "image/png".into(),
            filename: None,
            tokens: None,
        }
    }

    #[test]
    fn first_usage_latches_and_later_calls_ignored() {
        let bf = AttachmentTokenBackfill::new();
        bf.mark_submit("att-1".into());
        bf.on_event(&ServerMsg::Usage {
            usage: usage(Some(728)),
        });
        // agentic 循环第二次调用（历史图重发，值更大）——忽略。
        bf.on_event(&ServerMsg::Usage {
            usage: usage(Some(9999)),
        });
        bf.on_event(&turn_ok());
        let batch = bf.drain_completed();
        assert_eq!(batch, vec![("att-1".to_string(), 728)], "首次调用值胜出");
        assert_eq!(bf.get("att-1"), Some(728));
    }

    #[test]
    fn first_call_none_means_no_backfill_even_if_later_some() {
        // 智谱口径：首次调用就不分模态（None）→ 本回合无回填；后续调用
        // 冒出的值也忽略（严格首次口径）。
        let bf = AttachmentTokenBackfill::new();
        bf.mark_submit("att-1".into());
        bf.on_event(&ServerMsg::Usage { usage: usage(None) });
        bf.on_event(&ServerMsg::Usage {
            usage: usage(Some(500)),
        });
        bf.on_event(&turn_ok());
        assert!(bf.drain_completed().is_empty());
        assert_eq!(bf.get("att-1"), None);
    }

    #[test]
    fn turn_error_still_applies_latched_measurement() {
        // 首次调用已完成即测量真实；回合后续失败不否定该值。
        let bf = AttachmentTokenBackfill::new();
        bf.mark_submit("att-1".into());
        bf.on_event(&ServerMsg::Usage {
            usage: usage(Some(321)),
        });
        bf.on_event(&ServerMsg::TurnError {
            message: "tool failed".into(),
            kind: None,
        });
        assert_eq!(bf.drain_completed(), vec![("att-1".to_string(), 321)]);
    }

    #[test]
    fn no_pending_marker_means_usage_events_ignored() {
        // 迟到答案续跑（ask_user 广播的 TurnOk）等未标记回合：不回填。
        let bf = AttachmentTokenBackfill::new();
        bf.on_event(&ServerMsg::Usage {
            usage: usage(Some(728)),
        });
        bf.on_event(&turn_ok());
        assert!(bf.drain_completed().is_empty());
    }

    #[test]
    fn clear_pending_drops_marker() {
        let bf = AttachmentTokenBackfill::new();
        bf.mark_submit("att-1".into());
        bf.clear_pending();
        bf.on_event(&ServerMsg::Usage {
            usage: usage(Some(728)),
        });
        bf.on_event(&turn_ok());
        assert!(bf.drain_completed().is_empty());
    }

    #[test]
    fn map_first_write_wins_across_turns() {
        let bf = AttachmentTokenBackfill::new();
        for (turn, t) in [(1u64, 100u64), (2, 200)] {
            bf.mark_submit("att-1".into());
            bf.on_event(&ServerMsg::Usage { usage: usage(Some(t)) });
            bf.on_event(&turn_ok());
            let batch = bf.drain_completed();
            AttachmentTokenBackfill::apply_to_transcript(&mut [], &batch);
            let _ = turn;
        }
        assert_eq!(bf.get("att-1"), Some(100), "first-write-wins");
    }

    #[test]
    fn apply_to_transcript_fills_last_user_only_once() {
        let mut transcript = vec![
            user_entry(vec![att("att-1")]), // 早前回合同 id（防御路径）
            EntryDto::Assistant { text: "hi".into() },
            user_entry(vec![att("att-1")]),
            user_entry(vec![att("att-2"), att("att-3")]), // 多图条目不在此路径
        ];
        AttachmentTokenBackfill::apply_to_transcript(
            &mut transcript,
            &[("att-1".to_string(), 728)],
        );
        let EntryDto::User { attachments, .. } = &transcript[2] else { unreachable!() };
        assert_eq!(attachments.as_ref().unwrap()[0].tokens, Some(728));
        // 已填条目不被二次覆盖。
        AttachmentTokenBackfill::apply_to_transcript(
            &mut transcript,
            &[("att-1".to_string(), 999)],
        );
        let EntryDto::User { attachments, .. } = &transcript[2] else { unreachable!() };
        assert_eq!(attachments.as_ref().unwrap()[0].tokens, Some(728));
    }
}
