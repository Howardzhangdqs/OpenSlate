//! 共享单会话状态：`SessionCore`（引擎 + 转写镜像 + 配置镜像）与
//! [`MsgSink`]（传输无关的事件出口）。
//!
//! ## 锁与顺序（不变量，改动前必读）
//!
//! - `SessionCore::inner` 是 **std Mutex**：引擎的 ProgressCallback 是
//!   同步 trait，无法 await tokio 锁；所有回调持锁「先改镜像、再广播」，
//!   保证同一连接上的事件序 == 转写追加序。
//! - 锁方向唯一：`core → sink` 与 `core → approval bridge`（持 core 锁
//!   期间允许 broadcast / 读审批队首，二者内部都是非阻塞短临界区）；
//!   **严禁** `bridge → core`（审批应答路径必须先 `respond()` 释放
//!   bridge 锁、再取 core 锁推转写，反向会与 snapshot 构成环）。
//! - `RunManager` 沿用所有权回传模式：submit 时 take，回合结束由引擎
//!   任务放回（manager 不是 Sync 共享，靠 `Option` 槽位表达"在家/
//!   引擎持有"两态）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use openslate_core::agent_tree::AgentTree;
use openslate_core::config::{AgentsConfig, LimitsConfig, OpenSlateConfig};
use openslate_core::provider::ModelProvider;
use openslate_core::run_manager::RunManager;
use openslate_core::types::{Message, RunId};
use openslate_protocol::{
    AgentNodeDto, ApprovalSummaryDto, ConfigViewDto, EntryDto, LimitsDto, PendingApprovalDto,
    ProviderDto, SessionStatsDto, SkillInfoDto, SnapshotDto, ToolEntryDetailDto, ToolStatusDto,
    PROTOCOL_VERSION,
};
use openslate_store_sqlite::recorder::RunRecorder;
use openslate_store_sqlite::store::SqliteStore;
use tokio_util::sync::CancellationToken;

use crate::approval::SessionApprovalBridge;

/// 测试接缝：per-turn provider 构造器。生产 = `build_provider_for_model`；
/// 集成测试注入 ScriptedProvider（与 TUI 的 ProviderFactory 同构）。
pub type ProviderFactory =
    Arc<dyn Fn(&OpenSlateConfig, &str) -> anyhow::Result<Box<dyn ModelProvider>> + Send + Sync>;

/// 传输无关的事件出口：session 核心产生的每条 [`ServerMsg`] 都经它送出。
///
/// - WS server 的 ConnectionHub 实现它（fan-out 到多条连接，每连接一个
///   队列 + 写任务，队列天然给出「同一连接内事件全序」）；
/// - mobile 的 EventSink 实现它（单队列 + 泵线程 → JSON 回调宿主）。
///
/// 实现必须**非阻塞**（入队即返回）：broadcast 会被引擎线程在 core 锁
/// 释放后同步调用，阻塞在此会拖慢整个回合。
pub trait MsgSink: Send + Sync {
    /// 广播到所有连接。返回后消息已入队（不保证已送达）。
    fn broadcast(&self, msg: openslate_protocol::ServerMsg);
    /// 定向发送。返回 false = 连接已不在。
    fn send_to(&self, conn_id: u64, msg: openslate_protocol::ServerMsg) -> bool;
    /// 当前连接 id 快照。
    fn conn_ids(&self) -> Vec<u64>;
    /// 当前连接数（审批桥「无人可问即拒绝」的判据）。
    fn count(&self) -> usize;
    /// 关闭所有连接（优雅停机）。
    fn close_all(&self);
    /// 传输侧具体类型下沉（server 的 ConnectionHub 用它做连接注册等
    /// 传输特有操作；session 核心自身不调用）。
    fn as_any(&self) -> &dyn std::any::Any;
}

/// 配置三路径（ConfigViewDto 投影用）。
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// 实际生效配置（显式指定或发现链结果）。
    pub active: PathBuf,
    /// 全局库（存在时 provider/model 条目写这里）。
    pub global: Option<PathBuf>,
    /// 本地叠加配置。
    pub local: Option<PathBuf>,
}

/// 会话当前打开的持久化 run（lazy，首个 submit 建立，/new 关闭）。
#[derive(Clone)]
pub struct SessionRun {
    pub run_id: RunId,
    pub recorder: Arc<RunRecorder>,
}

/// 流式缓冲（live 答案/思维链；在条目产生事件处冲刷提交）。
#[derive(Default)]
pub struct StreamBuffers {
    pub answer: String,
    pub reasoning: String,
}

/// `SessionCore` 的全部可变状态。字段组与 TUI `App` 一一对应。
pub struct CoreInner {
    pub session_id: String,
    pub session_label: String,
    pub history: Vec<Message>,
    pub transcript: Vec<EntryDto>,
    pub running: bool,
    pub compacting: bool,
    pub depth_cur: u32,
    pub agents_running: u32,
    pub tool_calls_cur: u32,
    pub model_alias: String,
    pub cancel: Option<CancellationToken>,
    pub config: OpenSlateConfig,
    pub agents_cfg: AgentsConfig,
    pub agent_tree: AgentTree,
    pub skills: Vec<SkillInfoDto>,
    pub store: Option<SqliteStore>,
    pub session_run: Option<SessionRun>,
    pub manager: Option<RunManager>,
    pub engine_task: Option<tokio::task::JoinHandle<()>>,
    pub paths: ConfigPaths,
    pub stats: SessionStatsDto,
    pub stream: StreamBuffers,
    /// live 工具条目的开始时刻（transcript 下标 → Instant），
    /// ToolEnd 时算 `elapsed_ms`。
    pub tool_timers: HashMap<usize, Instant>,
    /// RequestEnd 挂起的 usage 行（TUI fix-19 同款 hold：落在本步
    /// 工具行之下，无工具则在下一 RequestStart / 回合结束冲刷）。
    pub pending_step_meta: Option<String>,
}

impl CoreInner {
    /// 回合/压缩进行中（submit/new_session 的守卫）。
    pub fn busy(&self) -> bool {
        self.running || self.compacting
    }
}

/// 共享单会话核心。
pub struct SessionCore {
    inner: Mutex<CoreInner>,
}

impl SessionCore {
    pub fn new(inner: CoreInner) -> Self {
        Self {
            inner: Mutex::new(inner),
        }
    }

    /// 毒化容忍锁（引擎回调里 panic 不能把整个会话锁死）。
    pub fn lock(&self) -> MutexGuard<'_, CoreInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 服务器全局状态（由装配方持有；传输侧共享）。
pub struct AppState {
    pub core: Arc<SessionCore>,
    pub sink: Arc<dyn MsgSink>,
    pub approval: Arc<SessionApprovalBridge>,
    pub auth_token: Option<String>,
    pub provider_factory: ProviderFactory,
    /// run 落库的来源标注（server 保持 `"server"` 以兼容历史数据；
    /// mobile 用 `"mobile"`）。
    pub origin: &'static str,
    /// 保活：MCP 连接守卫必须活得比工具注册表久（drop 序序约束见
    /// wiring.rs AppContext 文档）。
    pub _mcp: openslate_core::mcp::McpConnectionGuard,
}

// ---------------------------------------------------------------------------
// 视图构造（snapshot / config view）
// ---------------------------------------------------------------------------

/// 解析后的限额（`Option` 语义解掉：未配置段用默认值——与 TUI
/// context_limits/auto_compact_enabled 的回退一致）。
pub fn effective_limits(config: &OpenSlateConfig) -> LimitsConfig {
    config.limits.clone().unwrap_or_default()
}

/// ConfigViewDto：纯函数，从 CoreInner 投影。
pub fn build_config_view(inner: &CoreInner) -> ConfigViewDto {
    let providers = inner
        .config
        .providers
        .iter()
        .map(|(k, v)| (k.clone(), ProviderDto::from(v)))
        .collect();
    let models = inner
        .config
        .models
        .iter()
        .map(|(k, v)| (k.clone(), openslate_protocol::ModelDto::from(v)))
        .collect();
    let levels = inner.config.levels.clone().into_iter().collect();
    // 功能映射出"生效值"（并入缺省回退）——客户端拿到的即真相，
    // 与 limits 的"server 解析默认值"同一先例。
    let capabilities = openslate_core::model_config::CAPABILITIES
        .iter()
        .map(|cap| {
            (
                (*cap).to_owned(),
                openslate_core::model_config::capability_alias(&inner.config, cap),
            )
        })
        .collect();
    ConfigViewDto {
        providers,
        models,
        levels,
        capabilities,
        limits: LimitsDto::from(&effective_limits(&inner.config)),
        agents: AgentNodeDto::from(&inner.agent_tree),
        skills: inner.skills.clone(),
        active_config: inner.paths.active.display().to_string(),
        global_config: inner.paths.global.as_ref().map(|p| p.display().to_string()),
        local_config: inner.paths.local.as_ref().map(|p| p.display().to_string()),
    }
}

/// SnapshotDto：纯函数投影 + 审批桥的队首待审。
pub fn build_snapshot(inner: &CoreInner, approval: &SessionApprovalBridge) -> SnapshotDto {
    let pending_approval = approval
        .front_pending()
        .map(|(id, summary)| PendingApprovalDto {
            id,
            summary: summary.clone(),
        });
    SnapshotDto {
        proto: PROTOCOL_VERSION,
        session_id: inner.session_id.clone(),
        session_label: inner.session_label.clone(),
        transcript: inner.transcript.clone(),
        running: inner.busy(),
        depth_cur: inner.depth_cur,
        agents_running: inner.agents_running,
        tool_calls_cur: inner.tool_calls_cur,
        model_alias: inner.model_alias.clone(),
        pending_approval,
        config: build_config_view(inner),
        session_stats: inner.stats,
    }
}

// ---------------------------------------------------------------------------
// 转写镜像辅助（与 TUI transcript.rs 的同名行为 1:1）
// ---------------------------------------------------------------------------

/// 工具参数预览：trim + 去引号，≤40 列截断加 `…`（UTF-8 边界安全）。
/// TUI transcript.rs `args_preview` 的镜像。
pub fn args_preview(args: &str) -> String {
    let cleaned = args.trim().trim_matches('"');
    const MAX: usize = 40;
    if cleaned.len() <= MAX {
        return cleaned.to_owned();
    }
    let mut end = MAX.saturating_sub(1);
    while end > 0 && !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &cleaned[..end])
}

/// 留存文本字节预算截断（TUI `cap_stored_text` 镜像）。
pub fn cap_stored_text(s: &str, cap: usize) -> String {
    const MARKER: &str = "\n…（已截断）";
    if s.len() <= cap {
        return s.to_owned();
    }
    let mut end = cap.saturating_sub(MARKER.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &s[..end], MARKER)
}

/// args 留存上限（fix-25 同值）。
pub const ARGS_STORE_CAP: usize = 4 * 1024;
/// 输出留存上限（fix-25 同值）。
pub const OUTPUT_STORE_CAP: usize = 8 * 1024;

/// 从 call_agent 参数里取子 agent id（TUI status.rs `delegate_target`
/// 镜像；解析失败回退 `call_agent`）。
pub fn delegate_target(args: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(args) {
        for key in ["agent_id", "agent", "child"] {
            if let Some(id) = value.get(key).and_then(|v| v.as_str()) {
                return id.to_owned();
            }
        }
    }
    "call_agent".to_owned()
}

/// core 写进失败 Tool 消息的失败标记（TUI `tool_failure_summary` 镜像，
/// 锚定行首防误报）。
pub fn tool_failure_summary(content: &str) -> Option<String> {
    const MARKERS: [&str; 5] = [
        "Error:",
        "approval denied tool",
        "child agent call denied",
        "[tool call cancelled",
        "[error]",
    ];
    let trimmed = content.trim_start();
    if MARKERS.iter().any(|m| trimmed.starts_with(m)) {
        Some(first_line_summary(trimmed))
    } else {
        None
    }
}

/// 失败摘要 = 首行，≤60 显示列（CJK 宽度简化为按字符计——协议侧只存
/// 文本，精确宽度渲染是客户端的事）。
fn first_line_summary(s: &str) -> String {
    const MAX: usize = 60;
    let first = s.lines().next().unwrap_or_default();
    if first.chars().count() <= MAX {
        first.to_owned()
    } else {
        let head: String = first.chars().take(MAX).collect();
        format!("{head}…")
    }
}

/// 冲刷流式缓冲为条目（事件序即视觉序；空白缓冲丢弃，同 TUI
/// `flush_streaming_to_entries`）。
pub fn flush_stream(inner: &mut CoreInner) {
    let reasoning = std::mem::take(&mut inner.stream.reasoning);
    if !reasoning.trim().is_empty() {
        inner
            .transcript
            .push(EntryDto::Reasoning { text: reasoning });
    }
    let answer = std::mem::take(&mut inner.stream.answer);
    if !answer.trim().is_empty() {
        inner.transcript.push(EntryDto::Assistant { text: answer });
    }
    flush_pending_step_meta(inner);
}

/// 把挂起的 usage 行冲到转写尾部（有工具行时已在 tool_start 冲过）。
pub fn flush_pending_step_meta(inner: &mut CoreInner) {
    if let Some(meta) = inner.pending_step_meta.take() {
        inner.transcript.push(EntryDto::Meta { text: meta });
    }
}

/// live 工具开始（call_agent 走 Delegate 标记，spec D2：子代理内容
/// 不进转写）。
pub fn tool_start_entry(inner: &mut CoreInner, name: &str, args: &str) {
    flush_stream(inner);
    inner.tool_calls_cur = inner.tool_calls_cur.saturating_add(1);
    if name == "call_agent" {
        inner.depth_cur = inner.depth_cur.saturating_add(1);
        inner.agents_running = inner.agents_running.saturating_add(1);
        inner.transcript.push(EntryDto::Delegate {
            agent: delegate_target(args),
            done: false,
        });
        return;
    }
    let index = inner.transcript.len();
    inner.transcript.push(EntryDto::ToolCall {
        name: name.to_owned(),
        args: args_preview(args),
        call_id: None,
        status: ToolStatusDto::Running,
        detail: ToolEntryDetailDto {
            args: cap_stored_text(args, ARGS_STORE_CAP),
            output: None,
        },
    });
    inner.tool_timers.insert(index, Instant::now());
}

/// live 工具结束：最后一条同名 Running 条目转 Done（call_agent 额外
/// 完成最老的未完成 Delegate 标记，FIFO 配对）。
pub fn tool_end_entry(inner: &mut CoreInner, name: &str, bytes: usize, truncated: bool) {
    if name == "call_agent" {
        inner.depth_cur = inner.depth_cur.saturating_sub(1);
        inner.agents_running = inner.agents_running.saturating_sub(1);
        for entry in &mut inner.transcript {
            if let EntryDto::Delegate { done, .. } = entry {
                if !*done {
                    *done = true;
                    break;
                }
            }
        }
    }
    for (index, entry) in inner.transcript.iter_mut().enumerate().rev() {
        if let EntryDto::ToolCall {
            name: n, status, ..
        } = entry
        {
            if n == name && matches!(status, ToolStatusDto::Running) {
                let elapsed = inner
                    .tool_timers
                    .remove(&index)
                    .map(|t| t.elapsed().as_millis() as u64);
                *status = ToolStatusDto::Done {
                    bytes,
                    truncated,
                    elapsed_ms: elapsed,
                };
                break;
            }
        }
    }
}

/// 回合结束的 Tool 消息折叠（TUI `fold_tool_outcomes_merge` 的简化镜像：
/// live 条目无 call_id，按名字 + FIFO 游标配对；成功精化保住 live
/// elapsed；失败标记翻转 Failed；输出全文留存 8KB）。
pub fn fold_tool_outcomes(inner: &mut CoreInner, messages: &[Message]) {
    use openslate_core::types::MessageRole;
    let mut consumed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut cursors: HashMap<&str, usize> = HashMap::new();

    for msg in messages.iter().filter(|m| m.role == MessageRole::Tool) {
        // live 条目没有 call_id → 名字 + FIFO 游标（第 N 个结果配第 N 条）。
        let Some(name) = msg.name.as_deref() else {
            continue;
        };
        let cursor = cursors.entry(name).or_insert(0);
        let target = inner
            .transcript
            .iter()
            .enumerate()
            .filter(|(i, e)| {
                !consumed.contains(i) && {
                    if let EntryDto::ToolCall {
                        name: n, status, ..
                    } = e
                    {
                        n == name
                            && matches!(status, ToolStatusDto::Running | ToolStatusDto::Done { .. })
                    } else {
                        false
                    }
                }
            })
            .nth(*cursor)
            .map(|(i, _)| i);
        let Some(index) = target else {
            tracing::debug!("orphan tool result for '{name}' dropped (no matching entry)");
            continue;
        };
        *cursor += 1;
        consumed.insert(index);
        let failure = tool_failure_summary(&msg.content);
        if let EntryDto::ToolCall { status, detail, .. } = &mut inner.transcript[index] {
            match failure {
                Some(summary) => {
                    *status = ToolStatusDto::Failed { summary };
                }
                None => {
                    // 成功精化：保住 live elapsed（TUI 同款语义）。
                    let elapsed = match status {
                        ToolStatusDto::Done { elapsed_ms, .. } => *elapsed_ms,
                        _ => None,
                    };
                    *status = ToolStatusDto::Done {
                        bytes: msg.content.len(),
                        truncated: msg.content.contains("[TRUNCATED:"),
                        elapsed_ms: elapsed,
                    };
                }
            }
            detail.output = Some(cap_stored_text(&msg.content, OUTPUT_STORE_CAP));
        }
    }
}

/// ApprovalSummaryDto 直转（state 侧无额外逻辑，转接 protocol 的 From）。
pub fn approval_summary(req: &openslate_core::approval::ApprovalRequest) -> ApprovalSummaryDto {
    ApprovalSummaryDto::from(req)
}
