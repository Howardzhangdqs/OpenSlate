//! transcript 持久化（存储 v5 设计 §4/§5；历史转写丢失修复 C1/C2/C3）。
//!
//! live transcript（`CoreInner.transcript`）的 append 点在 session crate
//! （dispatch / 进度桥），mobile 无法逐点挂钩——本模块用**增量同步任务**
//! 达成同一落库语义：周期性（活跃期 300ms / 空闲 1s）对比内存转写与
//! 已落库游标，把新增条目按 `seq`（= 条目在转写中的下标）写入
//! `transcript_entries`（行级增量，EntryDto 原样序列化，无投影损耗）。
//! 这等价于设计 §4 的"append 路径收口到一个 helper"。
//!
//! ## 历史转写丢失修复（写库收口重构）
//!
//! 周期同步有 ≤ 一个周期的丢失窗口；三个高危时刻由此前的纯内存蒸发
//! 变为强制冲刷（都走 [`SyncState::flush_now`]，与 sync_loop 共享同一
//! 游标表——`transcript_entries` 的写者仍收敛在 [`SyncState`] 内）：
//!
//! - **NewSession**（session.rs 前置冲刷钩子 [`TranscriptFlusher`]）：
//!   清转写之前把尾部（含回合中推迟的 ToolCall）落库；
//! - **open_session 切换**：换出旧 run 的内存转写之前冲刷（busy 守卫
//!   已保证此刻无并发 submit）；
//! - **shutdown**：runtime 收束（`rt.take()`）之前同步冲刷。
//!
//! 冲刷发现「写了未折叠终值的 ToolCall 条目」（回合未完成快照）时追加
//! 一条 [`INCOMPLETE_TURN_META`] Meta 行显性化（不碰内存转写）。
//!
//! 一致性模型：同步任务是 `transcript_entries` 的**唯一写者**，且只在
//! 转写尾部追加（`seq` = 前缀下标 ⇒ 已落库行数恒等于游标）。切换/
//! 恢复 run 时游标从 DB 行数自举——无需与 open_session 共享任何状态。
//! 悬挂尾巴 pop 重建（C2）后经 [`SyncState::invalidate`] 清游标，让下
//! 一次自举 = 0 → 重建转写全量写回（幂等）。
//!
//! 两个保守细节：
//! - 回合进行中**推迟 ToolCall 条目**落库：回合结束的折叠
//!   （`fold_tool_outcomes`）会把工具输出并入条目，落库要等终值；
//!   中断/切换/关停路径的强制冲刷（force）跳过该推迟。
//! - 存储健康上报（设计 §5）同循环承担：`RunRecorder::
//!   pending_persisted_writes() > 0` → notice 横幅一次。
//!
//! dev 边界：`transcript_entries` 面向 append-only（无单行 UPDATE）；
//! 事后被修改的条目（折叠）以推迟落库覆盖主要场景，pop 重建（C2）走
//! 全删 + 投影重建 + 游标自愈重写。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use openslate_protocol::{EntryDto, NoticeLevel, ServerMsg};
use openslate_session::state::{AppState, MsgSink};
use openslate_store_sqlite::store::SqliteStore;

use crate::attachment_tokens::AttachmentTokenBackfill;
use crate::events::EventSink;

/// 活跃期（回合进行中 / 有未落库条目 / 有挂起写入）的同步周期。
const ACTIVE_INTERVAL: Duration = Duration::from_millis(300);
/// 空闲期周期（只有游标对比，开销可忽略）。
const IDLE_INTERVAL: Duration = Duration::from_secs(1);

/// 游标缓存的上限（run → 已落库条数；防无限增长，切多了丢最旧）。
const MAX_TRACKED_RUNS: usize = 4;

/// 崩溃/上次回合中断的显性化 Meta 文案（设计 §5）。
pub const DANGLING_TAIL_META: &str = "上次回合未完成，已丢弃待答复的工具调用";

/// 回合未完成快照的 Meta 文案（C1 `flush_now`）：强制冲刷写了未折叠
/// 终值的 ToolCall 条目（`detail.output.is_none()`）→ 追加此行显性化
/// 「其后是中断时的快照」。与 [`DANGLING_TAIL_META`]（重开时 pop 悬挂
/// 尾巴的显性化）语义区分。
pub const INCOMPLETE_TURN_META: &str = "回合未完成，以下为中断时的快照";

/// C3 rebuild 截断自愈的 Meta 文案：entries 行缺失尾部、已从消息投影
/// 恢复的显性化标记。
pub const HEAL_META: &str = "检测到未同步的转写尾部，已从消息记录恢复";

/// sync_loop 的共享游标状态（C1 重构）：run → 已落库条数。
///
/// 周期同步（[`sync_loop`] → [`SyncState::persist_range`]）与强制冲刷
/// （NewSession / open_session / shutdown → [`SyncState::flush_now`]）
/// 共用同一游标表——`transcript_entries` 的写者收敛在这一个结构里，
/// 无双写竞态。
///
/// 锁序（唯一方向，不得反向）：`cursors`（tokio Mutex，可跨 await 持
/// 有写库全程）→ `core`（std Mutex，短临界区快照 clone，绝不跨 await）。
pub(crate) struct SyncState {
    /// run → 已落库条数（= 下一条的 seq）。首次见到 run 时从 DB 自举。
    cursors: tokio::sync::Mutex<HashMap<String, usize>>,
}

/// 一轮 persist 的结果形状（`flush_now` 的收尾判定需要游标与写入形状）。
struct PersistOutcome {
    /// 本轮写入（按 attempted 计）的条数。
    inserted: usize,
    /// 写入后的游标（`None` = 本轮无写入或无 run/store 可写）。
    cursor: Option<usize>,
    /// 本轮写入的 run（收尾 Meta 行的写入目标）。
    run_id: Option<String>,
    /// store 快照（收尾 Meta 行的写入通道）。
    store: Option<SqliteStore>,
    /// 本轮是否写入过未折叠终值的 ToolCall（`detail.output.is_none()`）。
    wrote_unfolded_toolcall: bool,
}

impl Default for SyncState {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncState {
    pub(crate) fn new() -> Self {
        Self {
            cursors: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// 周期写库段（原 sync_loop 内联实现的原样收口）：对比内存转写与
    /// 游标，把新增条目按 `seq` 逐条写库（含游标自举、MAX_TRACKED_RUNS
    /// 驱逐）。`force = true` 跳过回合中 ToolCall 推迟（中断快照语义：
    /// 回合没收尾也要有完整落库）。返回本轮写入条数。
    pub(crate) async fn persist_range(&self, state: &Arc<AppState>, force: bool) -> usize {
        let mut cursors = self.cursors.lock().await;
        self.persist_locked(state, force, &mut cursors).await.inserted
    }

    /// 强制冲刷 + 中断快照收尾（C1）：NewSession / open_session 切换 /
    /// shutdown 前调用。`persist_range(force)` 之后，若本轮写过未折叠
    /// 终值的 ToolCall（回合未完成快照）→ 直接向 DB 追加一条
    /// [`INCOMPLETE_TURN_META`] Meta 行（seq = 新游标，游标 +1），
    /// **不碰内存转写**（清转写动作归调用方）。
    pub(crate) async fn flush_now(&self, state: &Arc<AppState>, force: bool) {
        let mut cursors = self.cursors.lock().await;
        let outcome = self.persist_locked(state, force, &mut cursors).await;
        let (Some(run_id), Some(store), Some(cursor)) =
            (outcome.run_id, outcome.store, outcome.cursor)
        else {
            return;
        };
        if !outcome.wrote_unfolded_toolcall {
            return;
        }
        let meta = EntryDto::Meta {
            text: INCOMPLETE_TURN_META.to_owned(),
        };
        let Ok(json) = serde_json::to_string(&meta) else {
            return;
        };
        let id = format!("te-{}", uuid::Uuid::new_v4().simple());
        match store
            .insert_transcript_entry(&id, &run_id, cursor as i64, &json, now_ms())
            .await
        {
            Ok(()) => {
                cursors.insert(run_id, cursor + 1);
            }
            Err(e) => crate::alog!("transcripts: 中断快照 Meta 行落库失败（非致命）: {e}"),
        }
    }

    /// 移除该 run 的游标（C2：open_session pop 悬挂尾巴重建转写后调用）
    /// ——下次见到该 run 时从 DB 行数重新自举（pop 路径已清空该 run 的
    /// entries 行 → 自举 0 → 重建转写全量写回，幂等）。
    pub(crate) async fn invalidate(&self, run_id: &str) {
        self.cursors.lock().await.remove(run_id);
    }

    /// run 是否仍有未落库条目（sync_loop 周期选择信号：含回合中推迟的
    /// ToolCall 尾部与写库失败待重试的批次）。
    pub(crate) async fn has_backlog(&self, state: &Arc<AppState>) -> bool {
        let cursors = self.cursors.lock().await;
        let (run, entries_len) = {
            let inner = state.core.lock();
            (
                inner.session_run.as_ref().map(|r| r.run_id.0.clone()),
                inner.transcript.len(),
            )
        };
        run.as_ref()
            .and_then(|r| cursors.get(r))
            .is_some_and(|c| *c < entries_len)
    }

    /// 写库核心（调用方持有 cursors 锁；锁序 cursors → core，唯一方向，
    /// 不得出现 core → cursors 的反向嵌套）。
    async fn persist_locked(
        &self,
        state: &Arc<AppState>,
        force: bool,
        cursors: &mut tokio::sync::MutexGuard<'_, HashMap<String, usize>>,
    ) -> PersistOutcome {
        // 短持 core 锁做快照（不持 core 锁跨 await）。
        let (run, entries, running, store) = {
            let inner = state.core.lock();
            (
                inner.session_run.as_ref().map(|r| r.run_id.0.clone()),
                inner.transcript.clone(),
                inner.running,
                inner.store.clone(),
            )
        };
        let mut outcome = PersistOutcome {
            inserted: 0,
            cursor: None,
            run_id: run.clone(),
            store: store.clone(),
            wrote_unfolded_toolcall: false,
        };
        let (Some(run_id), Some(store)) = (run, store) else {
            return outcome;
        };
        // 游标自举（首次见到 run 从 DB 行数起）+ 容量驱逐。
        if !cursors.contains_key(&run_id) {
            let persisted = persisted_count(&store, &run_id).await;
            if cursors.len() >= MAX_TRACKED_RUNS {
                // 丢最旧（插入序近似 = 访问序，足够用）。
                if let Some(oldest) = cursors.keys().next().cloned() {
                    cursors.remove(&oldest);
                }
            }
            cursors.insert(run_id.clone(), persisted);
        }
        let cursor = *cursors.get(&run_id).expect("just inserted");
        // 可落库前缀：回合进行中停在第一个未定稿 ToolCall（折叠后才是
        // 终值）；空闲时全量；force（中断快照 / 冲刷）全量。
        let mut end = cursor.min(entries.len());
        if force {
            end = entries.len();
        } else {
            while end < entries.len() {
                if running && matches!(entries[end], EntryDto::ToolCall { .. }) {
                    break;
                }
                end += 1;
            }
        }
        if end <= cursor {
            return outcome;
        }
        let batch: Vec<EntryDto> = entries[cursor.min(entries.len())..end].to_vec();
        let now = now_ms();
        let mut inserted = 0usize;
        for (offset, entry) in batch.iter().enumerate() {
            let seq = (cursor + offset) as i64;
            let id = format!("te-{}", uuid::Uuid::new_v4().simple());
            let json = match serde_json::to_string(entry) {
                Ok(j) => j,
                Err(e) => {
                    crate::alog!("transcripts: EntryDto 序列化失败（跳过）: {e}");
                    inserted += 1;
                    continue;
                }
            };
            match store
                .insert_transcript_entry(&id, &run_id, seq, &json, now)
                .await
            {
                Ok(()) => inserted += 1,
                Err(e) => {
                    // 本批中止，下个周期从断点重试（insert 已是
                    // INSERT OR IGNORE：唯一索引冲突静默让位首写者，
                    // 其余错误仍持续告警、不推进未尝试部分）。
                    crate::alog!("transcripts: 条目落库失败（seq={seq}，将重试）: {e}");
                    break;
                }
            }
            // 未折叠终值的 ToolCall（live/中断形态）→ flush_now 收尾
            // 需要「回合未完成快照」信号。
            if matches!(entry, EntryDto::ToolCall { detail, .. } if detail.output.is_none()) {
                outcome.wrote_unfolded_toolcall = true;
            }
        }
        if inserted > 0 {
            cursors.insert(run_id, cursor + inserted);
            outcome.inserted = inserted;
            outcome.cursor = Some(cursor + inserted);
        }
        outcome
    }
}

/// C1 装配入口（create_with 调用）：构造 [`SyncState`] 并向 SessionCore
/// 注册 NewSession 前置冲刷钩子。返回的 Arc 交给 sync_loop 持有——同一
/// 游标表，写者收敛。
pub(crate) fn install_flusher(
    core: &Arc<openslate_session::state::SessionCore>,
) -> Arc<SyncState> {
    /// session crate `TranscriptFlusher` 的 mobile 实现：委托
    /// `SyncState::flush_now(force = true)`。
    struct MobileFlusher(Arc<SyncState>);

    impl openslate_session::state::TranscriptFlusher for MobileFlusher {
        fn flush(&self, state: Arc<AppState>) -> futures::future::BoxFuture<'static, ()> {
            let sync = self.0.clone();
            Box::pin(async move {
                sync.flush_now(&state, true).await;
            })
        }
    }

    let sync = Arc::new(SyncState::new());
    core.set_transcript_flusher(Arc::new(MobileFlusher(sync.clone())));
    sync
}

/// 后台同步任务（create_with 里 spawn；shutdown_flag 置位即退出）。
pub(crate) async fn sync_loop(
    state: Arc<AppState>,
    sink: Arc<EventSink>,
    shutdown: Arc<AtomicBool>,
    attachment_tokens: Arc<AttachmentTokenBackfill>,
    sync: Arc<SyncState>,
) {
    let mut notified_storage_issue = false;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        // 附件 image tokens 回填消费：tap 在广播线程把回合收尾结果推进
        // completed 队列（那里不能锁 core），这里在本来就持锁的快照段
        // 内落 transcript（之后所有快照自然携带）。
        let token_backfills = attachment_tokens.drain_completed();
        // 锁内快照（不持 core 锁跨 await）——只取健康上报与周期选择
        // 所需字段；写库段收口在 SyncState（自取快照）。
        let (running, pending_writes) = {
            let mut inner = state.core.lock();
            if !token_backfills.is_empty() {
                AttachmentTokenBackfill::apply_to_transcript(
                    &mut inner.transcript,
                    &token_backfills,
                );
            }
            (
                inner.running,
                inner
                    .session_run
                    .as_ref()
                    .map(|r| r.recorder.pending_persisted_writes())
                    .unwrap_or(0),
            )
        };

        // 存储健康上报（设计 §5）：重试队列非空 → 横幅一次；回落清标志。
        if pending_writes > 0 {
            if !notified_storage_issue {
                notified_storage_issue = true;
                crate::alog!("transcripts: recorder 挂起写入 {pending_writes} 条，发横幅");
                sink.broadcast(ServerMsg::Notice {
                    text: "存储写入失败，历史可能不完整".into(),
                    level: NoticeLevel::Warn,
                });
            }
        } else {
            notified_storage_issue = false;
        }

        // 写库段（C1 收口进 SyncState；force=false 维持回合中 ToolCall
        // 推迟——终值折叠语义不变）。
        let dirty = sync.persist_range(&state, false).await > 0;
        let backlog = sync.has_backlog(&state).await;
        let interval = if running || dirty || backlog || pending_writes > 0 {
            ACTIVE_INTERVAL
        } else {
            IDLE_INTERVAL
        };
        tokio::time::sleep(interval).await;
    }
}

/// run 已落库条数（自举游标 = 下一条 seq）。读取失败按 0 处理并告警
/// （insert 是 INSERT OR IGNORE：按 0 自举后的全量重插会被首写行静默
/// 让位，不再构成致命告警死循环——C1）。
async fn persisted_count(store: &SqliteStore, run_id: &str) -> usize {
    match store.list_transcript_entries(run_id).await {
        Ok(rows) => rows.len(),
        Err(e) => {
            crate::alog!("transcripts: 读取 run {run_id} 已有条目失败（游标按 0 自举）: {e}");
            0
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 两级转写重建（设计 §4）：优先 `transcript_entries` 行（v5 起的真
/// 相源，Approval/Delegate/Meta/StepBreak 等 message 投影拿不到的条目
/// 都在）；空（v5 前的 run）→ 消息投影回退（L6 再换 protocol 共享
/// 投影函数）。解析失败的行跳过并告警。
///
/// C3 截断自愈：entries 行可能缺尾部（回合未干净收尾 + 清转写/换出蒸
/// 发）。 [`heal_truncated_tail`] 拿消息投影对照补齐——表不重写，重开
/// 后 sync_loop 游标按行数自举，补齐尾部按接续 seq 追加（幂等自愈）。
pub async fn rebuild_transcript(
    store: &SqliteStore,
    run_id: &str,
    fallback_messages: &[openslate_core::types::Message],
) -> Vec<EntryDto> {
    let mut out = match store.list_transcript_entries(run_id).await {
        Ok(rows) if !rows.is_empty() => {
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                match serde_json::from_str::<EntryDto>(&r.entry_json) {
                    Ok(e) => out.push(e),
                    Err(e) => crate::alog!(
                        "transcripts: run {run_id} 条目 seq={} 解析失败（跳过）: {e}",
                        r.seq
                    ),
                }
            }
            out
        }
        _ => crate::runtime::messages_to_transcript(fallback_messages),
    };
    // C3：截断自愈（overlay 之前——token 回填按 User 条目定位，尾部
    // 补齐后一并覆盖）。
    heal_truncated_tail(&mut out, fallback_messages, run_id);
    // 附件 image tokens 回填重放（v5）：transcript_entries 追加式不可变
    // （回填晚于 User 条目落库），从 messages.meta_json 按同口径恢复——
    // 重启后 tokens 的持久化读回路径。
    crate::attachment_tokens::overlay_tokens_from_messages(store, run_id, &mut out).await;
    out
}

/// 条目是否为 durable 种类（可由消息投影对齐的种类）。Meta / StepBreak /
/// Approval / Delegate / Reasoning 是装饰条目（投影拿不到或与 durable
/// 交错），walk 两侧都跳过；projected 尾部里的 Reasoning 随恢复一并
/// 追加。
fn is_durable(e: &EntryDto) -> bool {
    matches!(
        e,
        EntryDto::User { .. } | EntryDto::Assistant { .. } | EntryDto::ToolCall { .. }
    )
}

/// C3 rebuild 截断自愈：`out`（entries 行重建）的 durable 前缀若短于
/// 消息投影（尾部曾因回合中断蒸发），且二者逐条对得上——User↔User /
/// Assistant↔Assistant 种类匹配；ToolCall↔ToolCall call_id 精确优先、
/// 任一侧缺失时按名字（walk 本身有序，FIFO 天然成立；配对语义同
/// state.rs `fold_tool_outcomes` 的双分支）——则补 [`HEAL_META`] 行 +
/// 投影尾部。种类错位 / 数量对不齐 → 立即放弃（log，不 heal——宁缺
/// 勿错）。
fn heal_truncated_tail(
    out: &mut Vec<EntryDto>,
    fallback_messages: &[openslate_core::types::Message],
    run_id: &str,
) {
    let projected = openslate_protocol::project_from_messages(fallback_messages);
    let durable_entries = out.iter().filter(|e| is_durable(e)).count();
    if durable_entries >= projected.len() {
        return;
    }
    // 双向 walk：out 的 durable 条目逐一配对 projected 的 durable 前缀。
    let mut i = 0usize; // out 游标
    let mut j = 0usize; // projected 游标
    let mut matched = 0usize;
    // 恢复起点 = 最后一次配对成功后 projected 的下一位置（其间的装饰
    // 条目已随配对消费，之后的——含 Reasoning——归尾部恢复）。
    let mut remainder_start = 0usize;
    let mut aligned = true;
    while matched < durable_entries {
        while i < out.len() && !is_durable(&out[i]) {
            i += 1;
        }
        while j < projected.len() && !is_durable(&projected[j]) {
            j += 1;
        }
        if i >= out.len() || j >= projected.len() {
            aligned = false; // 投影侧 durable 不够配（数量对不齐）
            break;
        }
        let pair_ok = match (&out[i], &projected[j]) {
            (EntryDto::User { .. }, EntryDto::User { .. })
            | (EntryDto::Assistant { .. }, EntryDto::Assistant { .. }) => true,
            (
                EntryDto::ToolCall {
                    name: name_a,
                    call_id: id_a,
                    ..
                },
                EntryDto::ToolCall {
                    name: name_b,
                    call_id: id_b,
                    ..
                },
            ) => match (id_a, id_b) {
                // 两侧都有 call_id → 身份精确匹配；任一侧缺失（旧链路）
                // → 名字匹配。
                (Some(a), Some(b)) => a == b && name_a == name_b,
                _ => name_a == name_b,
            },
            _ => false, // 种类错位
        };
        if !pair_ok {
            aligned = false;
            break;
        }
        i += 1;
        j += 1;
        matched += 1;
        remainder_start = j;
    }
    let remainder = &projected[remainder_start..];
    if !aligned || matched != durable_entries || remainder.is_empty() {
        if durable_entries > 0 {
            crate::alog!(
                "transcripts: run {run_id} 截断自愈放弃（durable={durable_entries} 投影={} \
                 matched={matched} aligned={aligned}）",
                projected.len()
            );
        }
        return;
    }
    crate::alog!(
        "transcripts: run {run_id} 检测到未同步转写尾部，从消息投影恢复 {} 条",
        remainder.len()
    );
    out.push(EntryDto::Meta {
        text: HEAL_META.to_owned(),
    });
    out.extend(remainder.iter().cloned());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_text_pinned() {
        // §5 定稿文案（悬挂尾部显性化）。
        assert_eq!(
            super::DANGLING_TAIL_META,
            "上次回合未完成，已丢弃待答复的工具调用"
        );
        // C1/C3 新增文案。
        assert_eq!(
            super::INCOMPLETE_TURN_META,
            "回合未完成，以下为中断时的快照"
        );
        assert_eq!(
            super::HEAL_META,
            "检测到未同步的转写尾部，已从消息记录恢复"
        );
    }

    // ── T7：heal walk 单测（call_id 配对 / 名字兜底 / 错位 bail /
    //    装饰跳过）────────────────────────────────────────────────

    use openslate_core::types::{Message, MessageRole, ToolCall, ToolCallId};
    use openslate_protocol::{ToolEntryDetailDto, ToolStatusDto};

    fn user_msg(text: &str) -> Message {
        Message {
            attachments: None,
            role: MessageRole::User,
            content: text.to_owned(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        }
    }

    fn assistant_msg(text: &str) -> Message {
        Message {
            attachments: None,
            role: MessageRole::Assistant,
            content: text.to_owned(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        }
    }

    fn assistant_tool_call_msg(call_id: &str, name: &str) -> Message {
        Message {
            attachments: None,
            role: MessageRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: ToolCallId(call_id.into()),
                name: name.into(),
                arguments: serde_json::json!({}),
            }]),
            reasoning_content: None,
        }
    }

    fn live_tool_call(name: &str, call_id: Option<&str>) -> EntryDto {
        EntryDto::ToolCall {
            name: name.to_owned(),
            args: "{}".into(),
            call_id: call_id.map(str::to_owned),
            status: ToolStatusDto::Running,
            detail: ToolEntryDetailDto {
                args: "{}".into(),
                output: None,
            },
        }
    }

    fn assert_meta_at(out: &[EntryDto], idx: usize, want: &str) {
        match out.get(idx) {
            Some(EntryDto::Meta { text }) => assert_eq!(text, want),
            other => panic!("下标 {idx} 应为 Meta({want:?})，实为 {other:?}"),
        }
    }

    /// call_id 精确配对：live 条目与投影同 call_id 同名 → 对齐补尾。
    #[test]
    fn heal_pairs_tool_call_by_call_id() {
        let mut out = vec![
            EntryDto::User {
                text: "看下目录".into(),
                attachments: None,
            },
            live_tool_call("bash", Some("tc-1")),
        ];
        let messages = vec![
            user_msg("看下目录"),
            assistant_tool_call_msg("tc-1", "bash"),
            assistant_msg("共 3 个文件"),
        ];
        heal_truncated_tail(&mut out, &messages, "run-x");
        // User + ToolCall（live 前缀）+ HEAL Meta + 投影尾（Assistant）。
        assert_eq!(out.len(), 4);
        assert_meta_at(&out, 2, HEAL_META);
        assert!(matches!(&out[3], EntryDto::Assistant { text } if text == "共 3 个文件"));
    }

    /// 名字兜底：live 条目无 call_id（旧链路）、投影有 → 名字匹配配对。
    #[test]
    fn heal_falls_back_to_name_when_call_id_missing() {
        let mut out = vec![
            EntryDto::User {
                text: "跑一下".into(),
                attachments: None,
            },
            live_tool_call("bash", None),
        ];
        let messages = vec![
            user_msg("跑一下"),
            assistant_tool_call_msg("tc-9", "bash"),
            assistant_msg("完成"),
        ];
        heal_truncated_tail(&mut out, &messages, "run-x");
        // User + ToolCall（live 前缀）+ HEAL Meta + 投影尾（Assistant）。
        assert_eq!(out.len(), 4);
        assert_meta_at(&out, 2, HEAL_META);
        assert!(matches!(&out[3], EntryDto::Assistant { text } if text == "完成"));
    }

    /// 种类错位立即 bail：entries 行前缀与投影种类对不上 → 不 heal。
    #[test]
    fn heal_bails_on_kind_mismatch() {
        let mut out = vec![EntryDto::Assistant {
            text: "旧答案".into(),
        }];
        let messages = vec![user_msg("问题"), assistant_msg("回答")];
        heal_truncated_tail(&mut out, &messages, "run-x");
        assert_eq!(out.len(), 1, "错位不 heal，原样返回");
    }

    /// call_id 双侧都在但不相等 → 身份不匹配 bail。
    #[test]
    fn heal_bails_on_call_id_conflict() {
        let mut out = vec![live_tool_call("bash", Some("tc-a"))];
        let messages = vec![
            user_msg("跑"),
            assistant_tool_call_msg("tc-b", "bash"),
        ];
        heal_truncated_tail(&mut out, &messages, "run-x");
        assert_eq!(out.len(), 1);
    }

    /// 装饰条目（Meta / Reasoning）在 walk 两侧都被跳过；投影尾部的
    /// Reasoning 随恢复追加。
    #[test]
    fn heal_skips_decoration_entries() {
        let mut out = vec![
            EntryDto::Meta {
                text: "↑1 ↓2".into(),
            },
            EntryDto::User {
                text: "问".into(),
                attachments: None,
            },
        ];
        let messages = vec![
            user_msg("问"),
            Message {
                attachments: None,
                role: MessageRole::Assistant,
                content: "答".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: Some("思考中".into()),
            },
        ];
        heal_truncated_tail(&mut out, &messages, "run-x");
        // Meta（装饰跳过）+ User + HEAL + Reasoning + Assistant。
        assert_eq!(out.len(), 5);
        assert_meta_at(&out, 2, HEAL_META);
        assert!(matches!(&out[3], EntryDto::Reasoning { text } if text == "思考中"));
        assert!(matches!(&out[4], EntryDto::Assistant { text } if text == "答"));
    }

    /// 无缺尾（durable ≥ 投影长度）→ no-op。
    #[test]
    fn heal_noop_when_not_truncated() {
        let mut out = vec![
            EntryDto::User {
                text: "问".into(),
                attachments: None,
            },
            EntryDto::Assistant {
                text: "答".into(),
            },
        ];
        let messages = vec![user_msg("问"), assistant_msg("答")];
        heal_truncated_tail(&mut out, &messages, "run-x");
        assert_eq!(out.len(), 2);
    }

    /// 数量对不齐（out 的 durable 多于投影可配对的）→ bail。
    #[test]
    fn heal_bails_when_out_durable_exceeds_projection() {
        let mut out = vec![
            EntryDto::User {
                text: "问".into(),
                attachments: None,
            },
            EntryDto::Assistant {
                text: "答".into(),
            },
        ];
        // 投影 = [User, Reasoning, Reasoning]（len 3 > durable 2，但
        // durable 只有 1 个）→ walk 到投影侧耗尽仍差 1 条 → bail。
        let reasoning_only = |text: &str| Message {
            attachments: None,
            role: MessageRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: Some(text.into()),
        };
        let messages = vec![user_msg("问"), reasoning_only("思考一"), reasoning_only("思考二")];
        heal_truncated_tail(&mut out, &messages, "run-x");
        assert_eq!(out.len(), 2, "对不齐不 heal");
    }
}
