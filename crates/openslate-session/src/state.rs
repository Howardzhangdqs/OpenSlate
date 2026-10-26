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
use openslate_core::approval::ApprovalRules;
use openslate_core::config::{AgentsConfig, LimitsConfig, OpenSlateConfig};
use openslate_core::provider::ModelProvider;
use openslate_core::run_manager::RunManager;
use openslate_core::types::{Message, RunId};
use openslate_protocol::{
    AgentNodeDto, ApprovalConfigViewDto, ApprovalSummaryDto, ConfigViewDto, EntryDto, LimitsDto,
    PendingApprovalDto, ProviderDto, SessionStatsDto, SkillInfoDto, SnapshotDto, ToolEntryDetailDto,
    ToolStatusDto, PROTOCOL_VERSION,
};
use openslate_store_sqlite::recorder::RunRecorder;
use openslate_store_sqlite::store::SqliteStore;
use tokio_util::sync::CancellationToken;

use crate::approval::SessionApprovalBridge;
use crate::notify::NotifyHandler;
use crate::question::QuestionHandler;

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

/// 转写前置冲刷钩子（历史转写丢失修复 C1）：`NewSession` 清空内存转写
/// 之前，宿主把未落库的尾部条目**强制**落库——回合未干净收尾（模型流
/// 挂死后取消）时，sync_loop 推迟的 ToolCall 尾部只有此刻还有机会写库，
/// 清了就随内存蒸发（DB 截断 → 重开只剩前缀）。
///
/// server / cli / tui 不注册（`None` = 无前置冲刷，行为不变）。实现自取
/// 状态快照（mobile 侧锁序唯一方向：SyncState cursors → core），失败
/// 只记录不传播（best-effort：冲刷失败不应挡住新建会话）。
pub trait TranscriptFlusher: Send + Sync {
    /// 强制冲刷当前 run 的转写尾部。future 完成即冲刷已尽最大努力。
    fn flush(&self, state: Arc<AppState>) -> futures::future::BoxFuture<'static, ()>;
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
    /// 当前回合任务（submit / resume 各存一次）。JoinHandle 无人消费：
    /// 闭包体已包 catch_unwind（engine.rs panic 防线），Err(JoinError)
    /// 路径实际不可达，仅作任务存活引用（drop 不 abort）。
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
    /// 最近一次「回合级断连重试」发生时刻（会话级冷却窗判据：两次网络
    /// 重试至少间隔 [`crate::engine::NETWORK_RETRY_COOLDOWN`]，防网络
    /// 瘫痪时逐回合空转重试；照 mcp_recovery 的 10s 冷却先例）。
    /// `None` / 会话新建时重置。
    pub last_network_retry: Option<Instant>,
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
    /// ask_user 应答通道（见 question.rs 模块文档：注入点在此而非
    /// AppState 字段，避免破坏 server/mobile 的结构体字面量装配）。
    /// `None` = 提问通道未接入（dispatch 收到 question_reply 时定向
    /// notice）。锁方向遵守模块头不变量：core 锁内只做非阻塞短临界
    /// 区操作（clone Arc），handler 本身由宿主在 core 锁外调用。
    questions: Mutex<Option<Arc<dyn QuestionHandler>>>,
    /// notify 通知应答通道（见 notify.rs 模块文档：注入点仿 questions，
    /// mobile 装配层在构造 AppState 之后调用）。`None` = 通道未接入
    ///（dispatch 收到 notification_reply 时定向 notice）。锁内只
    /// clone Arc，handler 本身由宿主在 core 锁外调用。
    notifies: Mutex<Option<Arc<dyn NotifyHandler>>>,
    /// 转写前置冲刷钩子（C1；注入点仿 questions：避免破坏各装配点的
    /// 结构体字面量）。`None` = 未注册（server/cli/tui，行为不变）；
    /// mobile 注册 SyncState 委托（见 mobile transcripts.rs）。锁内只
    /// clone Arc，flush 本身由调用方在 core 锁外 await。
    transcripts: Mutex<Option<Arc<dyn TranscriptFlusher>>>,
}

impl SessionCore {
    pub fn new(inner: CoreInner) -> Self {
        Self {
            inner: Mutex::new(inner),
            questions: Mutex::new(None),
            notifies: Mutex::new(None),
            transcripts: Mutex::new(None),
        }
    }

    /// 毒化容忍锁（引擎回调里 panic 不能把整个会话锁死）。
    pub fn lock(&self) -> MutexGuard<'_, CoreInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 注入 ask_user 应答处理器（装配层在构造 AppState 之后调用；
    /// 宿主实现建议持 `Weak<AppState>` 防引用环，见 question.rs）。
    pub fn set_question_handler(&self, handler: Arc<dyn QuestionHandler>) {
        *Self::lock_questions(&self.questions) = Some(handler);
    }

    /// 当前处理器快照（`None` = 提问通道未接入）。
    pub fn question_handler(&self) -> Option<Arc<dyn QuestionHandler>> {
        Self::lock_questions(&self.questions).clone()
    }

    fn lock_questions(
        m: &Mutex<Option<Arc<dyn QuestionHandler>>>,
    ) -> std::sync::MutexGuard<'_, Option<Arc<dyn QuestionHandler>>> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 注入 notify 通知应答处理器（装配层在构造 AppState 之后调用；
    /// `set_question_handler` 同款防破坏与防环论证，见 notify.rs）。
    pub fn set_notify_handler(&self, handler: Arc<dyn NotifyHandler>) {
        *Self::lock_notifies(&self.notifies) = Some(handler);
    }

    /// 当前处理器快照（`None` = 通知应答通道未接入）。
    pub fn notify_handler(&self) -> Option<Arc<dyn NotifyHandler>> {
        Self::lock_notifies(&self.notifies).clone()
    }

    fn lock_notifies(
        m: &Mutex<Option<Arc<dyn NotifyHandler>>>,
    ) -> std::sync::MutexGuard<'_, Option<Arc<dyn NotifyHandler>>> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 注入转写前置冲刷钩子（C1；mobile 装配层在构造 AppState 之后
    /// 调用，与 sync_loop 共享同一 SyncState 游标表）。
    pub fn set_transcript_flusher(&self, flusher: Arc<dyn TranscriptFlusher>) {
        *Self::lock_flusher(&self.transcripts) = Some(flusher);
    }

    /// 当前冲刷钩子快照（`None` = 未注册，NewSession 跳过前置冲刷）。
    pub fn transcript_flusher(&self) -> Option<Arc<dyn TranscriptFlusher>> {
        Self::lock_flusher(&self.transcripts).clone()
    }

    fn lock_flusher(
        m: &Mutex<Option<Arc<dyn TranscriptFlusher>>>,
    ) -> std::sync::MutexGuard<'_, Option<Arc<dyn TranscriptFlusher>>> {
        m.lock().unwrap_or_else(|e| e.into_inner())
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
///
/// `approval_rules`：审批生效规则快照（bridge 的 rules cell，legacy
/// 映射后的**生效值**，非 toml 原文——oracle P1：UI 显示的必须是生效
/// 规则）。调用方在取 core 锁**之前**快照（锁序红线：先 bridge 后
/// core；rules cell 为叶子锁、非阻塞短临界区）。
pub fn build_config_view(inner: &CoreInner, approval_rules: &ApprovalRules) -> ConfigViewDto {
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
    // 注意：compact 行的 "fast" 缺省仅是展示层的历史默认值，与引擎
    // 实际行为不同——未绑定时 auto-compact 继承会话当前模型
    // （engine.rs `run_auto_compact`），本视图不反映该继承（现无
    // 消费方依赖 compact 行做行为决策，纯展示）。
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
        // tool-approval P1b：审批生效规则视图（legacy 映射后的生效值）。
        approval: Some(ApprovalConfigViewDto {
            default_level: approval_rules.default_level.as_str().to_owned(),
            tool_levels: approval_rules
                .tools
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().to_owned()))
                .collect(),
            server_levels: approval_rules
                .servers
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().to_owned()))
                .collect(),
        }),
    }
}

/// SnapshotDto：纯函数投影 + 审批桥的队首待审。
pub fn build_snapshot(inner: &CoreInner, approval: &SessionApprovalBridge) -> SnapshotDto {
    // 规则快照先取（bridge 侧叶子锁、非阻塞短临界区——模块头允许
    // core → bridge 的该方向；与 front_pending 同级），再投影视图。
    let approval_rules = approval.rules_snapshot();
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
        // 当前 run 键随快照下发：Kotlin 转写镜像（落盘/恢复仲裁）以事件流
        // 内嵌的 run_id 对齐泵协程 FIFO，替换直读 FFI 的旁路竞态源。
        run_id: inner.session_run.as_ref().map(|r| r.run_id.0.clone()),
        transcript: inner.transcript.clone(),
        running: inner.busy(),
        depth_cur: inner.depth_cur,
        agents_running: inner.agents_running,
        tool_calls_cur: inner.tool_calls_cur,
        model_alias: inner.model_alias.clone(),
        pending_approval,
        // ask_user 待答清单：问卷持久化状态在宿主（mobile）持久层，
        // server 投影恒空；mobile 装配侧（emit_snapshot）按持久层
        // 回填后再下发——重启恢复回放走这条通路。
        pending_questions: Vec::new(),
        config: build_config_view(inner, &approval_rules),
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
/// 不进转写）。`call_id` 落进 ToolCall 条目（精确配对 / 客户端懒加载
/// 全量输出的键）；旧链路不传时为 `None`。
pub fn tool_start_entry(inner: &mut CoreInner, name: &str, args: &str, call_id: Option<&str>) {
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
        call_id: call_id.map(str::to_owned),
        status: ToolStatusDto::Running,
        detail: ToolEntryDetailDto {
            args: cap_stored_text(args, ARGS_STORE_CAP),
            output: None,
        },
    });
    inner.tool_timers.insert(index, Instant::now());
}

/// live 工具结束：目标条目转 Done。配对优先 `call_id` 精确匹配；
/// 任一侧缺失 call_id 时回退「最后一条同名 Running」（FIFO，历史
/// 行为）。call_agent 额外完成最老的未完成 Delegate 标记（FIFO 配对）。
pub fn tool_end_entry(
    inner: &mut CoreInner,
    name: &str,
    bytes: usize,
    truncated: bool,
    call_id: Option<&str>,
) {
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
    // 精确匹配目标下标：call_id 优先（身份匹配，不受状态限制——重复
    // end 事件只会重写同一条目，不会错配到其它同名调用），任一侧缺失
    // call_id 时回退「最后一条同名 Running」（FIFO，历史行为）。
    let target = call_id
        .and_then(|cid| {
            inner
                .transcript
                .iter()
                .position(|e| matches!(e, EntryDto::ToolCall { call_id: Some(c), .. } if c == cid))
        })
        .or_else(|| {
            // 兼容旧事件（无 call_id）：最后一条同名 Running。
            inner
                .transcript
                .iter()
                .enumerate()
                .rev()
                .find_map(|(i, e)| {
                    matches!(
                        e,
                        EntryDto::ToolCall {
                            name: n,
                            status: ToolStatusDto::Running,
                            ..
                        } if n == name
                    )
                    .then_some(i)
                })
        });
    if let Some(index) = target {
        let elapsed = inner
            .tool_timers
            .remove(&index)
            .map(|t| t.elapsed().as_millis() as u64);
        if let EntryDto::ToolCall { status, .. } = &mut inner.transcript[index] {
            *status = ToolStatusDto::Done {
                bytes,
                truncated,
                elapsed_ms: elapsed,
            };
        }
    }
}

/// 回合结束的 Tool 消息折叠（TUI `fold_tool_outcomes_merge` 的简化镜像：
/// call_id 精确配对优先，任一侧缺失时按名字 + FIFO 游标配对；成功精化
/// 保住 live elapsed；失败标记翻转 Failed；输出全文留存 8KB）。
pub fn fold_tool_outcomes(inner: &mut CoreInner, messages: &[Message]) {
    use openslate_core::types::MessageRole;
    let mut consumed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut cursors: HashMap<&str, usize> = HashMap::new();

    for msg in messages.iter().filter(|m| m.role == MessageRole::Tool) {
        // 配对优先级：① msg.tool_call_id == entry.call_id（两侧都有时，
        // 身份精确匹配，并发同名也零错配）；② 任一侧缺失 call_id →
        // 名字 + FIFO 游标（第 N 个结果配第 N 条，历史行为）。
        let target = msg
            .tool_call_id
            .as_ref()
            .and_then(|cid| {
                inner
                    .transcript
                    .iter()
                    .enumerate()
                    .find(|(i, e)| {
                        // 身份匹配：call_id 全局唯一，不限状态（回合重放
                        // 幂等——Failed 条目再次折合同一条目而非错位到
                        // 后续同名条目），仅排除本轮已消费。
                        if let EntryDto::ToolCall { call_id: Some(c), .. } = e {
                            c == &cid.0 && !consumed.contains(i)
                        } else {
                            false
                        }
                    })
                    .map(|(i, _)| i)
            })
            .or_else(|| {
                let Some(name) = msg.name.as_deref() else {
                    return None;
                };
                // 游标 = 扫描起点（上次命中下标 + 1，TUI
                // fold_tool_outcomes_merge 同款语义）：从起点向后找首个
                // 未消费同名条目。旧实现的「过滤后 nth(count)」双重计数
                // 会把同名第 2 个及以后的结果当孤儿丢弃（既有 bug，随
                // 本轮配对升级修正）。
                let start = cursors.entry(name).or_insert(0);
                let target = inner
                    .transcript
                    .iter()
                    .enumerate()
                    .skip(*start)
                    .find(|(i, e)| {
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
                    .map(|(i, _)| i);
                if let Some(i) = target {
                    *start = i + 1;
                }
                target
            });
        let Some(index) = target else {
            let name = msg.name.as_deref().unwrap_or("(unnamed)");
            tracing::debug!("orphan tool result for '{name}' dropped (no matching entry)");
            continue;
        };
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
                        // 截断/落盘双标记：旧 limit 截断（"[TRUNCATED:"）或
                        // 长输出落盘预览（core `STORED_OUTPUT_MARKER`，
                        // C3——落盘预览同样意味着 UI 直读的不是全文，详情
                        // 弹窗经 tool_output_by_call_id 全文闭环）。
                        truncated: msg.content.contains("[TRUNCATED:")
                            || msg
                                .content
                                .contains(openslate_core::output_store::STORED_OUTPUT_MARKER),
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

#[cfg(test)]
mod tests {
    //! 工具条目配对单元测试：tool_end / fold 两处的「call_id 精确匹配
    //! 优先、缺失回退名字 + FIFO」双分支（懒加载全量显示的配对基础）。

    use super::*;
    use openslate_core::config::{parse_openslate_toml, AgentsConfig};
    use openslate_core::types::{AgentConfig, AgentId, MessageRole, ToolCallId};

    fn test_inner() -> CoreInner {
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
        CoreInner {
            session_id: "test-session".into(),
            session_label: "test".into(),
            history: Vec::new(),
            transcript: Vec::new(),
            running: false,
            compacting: false,
            depth_cur: 0,
            agents_running: 0,
            tool_calls_cur: 0,
            model_alias: "main".into(),
            cancel: None,
            config,
            agents_cfg: agents.clone(),
            agent_tree: AgentTree::from_configs(&agents.agents).expect("tree builds"),
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
        }
    }

    fn tool_call_entry(call_id: Option<&str>) -> EntryDto {
        EntryDto::ToolCall {
            name: "bash".into(),
            args: "{}".into(),
            call_id: call_id.map(str::to_owned),
            status: ToolStatusDto::Running,
            detail: ToolEntryDetailDto {
                args: "{}".into(),
                output: None,
            },
        }
    }

    fn tool_msg(call_id: &str, name: &str, content: &str) -> Message {
        Message {
            attachments: None,
            role: MessageRole::Tool,
            content: content.into(),
            tool_call_id: Some(ToolCallId(call_id.into())),
            name: Some(name.into()),
            tool_calls: None,
            reasoning_content: None,
        }
    }

    /// 新链路：并发同名 running 条目按 call_id 精确关闭（旧名字配对
    /// 会错关最后一条 running）。
    #[test]
    fn tool_end_pairs_by_call_id_exact() {
        let mut inner = test_inner();
        tool_start_entry(&mut inner, "bash", "{}", Some("tc-a"));
        tool_start_entry(&mut inner, "bash", "{}", Some("tc-b"));
        // 先结束 a：名字 FIFO（rev 找最后一条 running）会错关 b。
        tool_end_entry(&mut inner, "bash", 10, false, Some("tc-a"));
        match (&inner.transcript[0], &inner.transcript[1]) {
            (
                EntryDto::ToolCall { status: a, .. },
                EntryDto::ToolCall { status: b, .. },
            ) => {
                assert!(matches!(a, ToolStatusDto::Done { .. }), "tc-a 应已结束");
                assert!(matches!(b, ToolStatusDto::Running), "tc-b 应仍运行");
            }
            other => panic!("transcript 形状不符: {other:?}"),
        }
        tool_end_entry(&mut inner, "bash", 20, false, Some("tc-b"));
        assert!(
            matches!(
                &inner.transcript[1],
                EntryDto::ToolCall { status: ToolStatusDto::Done { .. }, .. }
            ),
            "tc-b 随后结束"
        );
    }

    /// 旧链路回退：事件与条目都没有 call_id 时维持名字 + FIFO（最后
    /// 一条同名 Running）。
    #[test]
    fn tool_end_falls_back_to_name_fifo_without_call_id() {
        let mut inner = test_inner();
        tool_start_entry(&mut inner, "bash", "{}", None);
        tool_start_entry(&mut inner, "bash", "{}", None);
        tool_end_entry(&mut inner, "bash", 10, false, None);
        assert!(
            matches!(
                &inner.transcript[0],
                EntryDto::ToolCall { status: ToolStatusDto::Running, .. }
            ),
            "第 1 条应仍运行（rev 语义关最后一条）"
        );
        assert!(
            matches!(
                &inner.transcript[1],
                EntryDto::ToolCall { status: ToolStatusDto::Done { .. }, .. }
            ),
            "第 2 条（最后一条 running）应已结束"
        );
    }

    /// 新链路：折叠按 call_id 精确落 output（乱序 Tool 消息也零错配）。
    #[test]
    fn fold_pairs_by_call_id_exact() {
        let mut inner = test_inner();
        tool_start_entry(&mut inner, "bash", "{}", Some("tc-a"));
        tool_start_entry(&mut inner, "bash", "{}", Some("tc-b"));
        // 乱序回放（b 的结果先到）——身份匹配不受顺序影响。
        fold_tool_outcomes(
            &mut inner,
            &[
                tool_msg("tc-b", "bash", "output-b"),
                tool_msg("tc-a", "bash", "output-a"),
            ],
        );
        match (&inner.transcript[0], &inner.transcript[1]) {
            (
                EntryDto::ToolCall { detail: da, .. },
                EntryDto::ToolCall { detail: db, .. },
            ) => {
                assert_eq!(da.output.as_deref(), Some("output-a"));
                assert_eq!(db.output.as_deref(), Some("output-b"));
            }
            other => panic!("transcript 形状不符: {other:?}"),
        }
    }

    /// 旧链路回退：条目无 call_id（消息有）→ 名字 + FIFO 游标依序配对。
    #[test]
    fn fold_falls_back_to_name_fifo_without_entry_call_id() {
        let mut inner = test_inner();
        tool_start_entry(&mut inner, "bash", "{}", None);
        tool_start_entry(&mut inner, "bash", "{}", None);
        fold_tool_outcomes(
            &mut inner,
            &[
                tool_msg("tc-1", "bash", "first"),
                tool_msg("tc-2", "bash", "second"),
            ],
        );
        match (&inner.transcript[0], &inner.transcript[1]) {
            (
                EntryDto::ToolCall { detail: d1, .. },
                EntryDto::ToolCall { detail: d2, .. },
            ) => {
                assert_eq!(d1.output.as_deref(), Some("first"), "第 1 个结果配第 1 条");
                assert_eq!(d2.output.as_deref(), Some("second"), "第 2 个结果配第 2 条");
            }
            other => panic!("transcript 形状不符: {other:?}"),
        }
    }

    /// 回合重放幂等（r.messages 为全量历史每回合重放）：带 call_id 的
    /// Failed 条目重复折合到自身，不错位到后续同名条目。
    #[test]
    fn fold_replay_with_call_id_is_idempotent_across_status() {
        let mut inner = test_inner();
        tool_start_entry(&mut inner, "bash", "{}", Some("tc-fail"));
        tool_start_entry(&mut inner, "bash", "{}", Some("tc-ok"));
        // 第一回合：tc-fail 失败（折叠置 Failed）。
        fold_tool_outcomes(
            &mut inner,
            &[tool_msg("tc-fail", "bash", "Error: boom"), tool_msg("tc-ok", "bash", "fine")],
        );
        // 第二回合重放同一历史：两条各自命中自身（Failed 条目按身份
        // 重新折合，不消费 tc-ok 的配对）。
        fold_tool_outcomes(
            &mut inner,
            &[tool_msg("tc-fail", "bash", "Error: boom"), tool_msg("tc-ok", "bash", "fine")],
        );
        match (&inner.transcript[0], &inner.transcript[1]) {
            (
                EntryDto::ToolCall { detail: df, status: sf, .. },
                EntryDto::ToolCall { detail: d2, status: s2, .. },
            ) => {
                assert!(
                    matches!(sf, ToolStatusDto::Failed { .. }),
                    "失败态重放后仍为 Failed"
                );
                assert!(df.output.as_deref().unwrap().contains("Error: boom"));
                assert!(
                    matches!(s2, ToolStatusDto::Done { .. }),
                    "tc-ok 不被失败重放挤占"
                );
                assert_eq!(d2.output.as_deref(), Some("fine"));
            }
            other => panic!("transcript 形状不符: {other:?}"),
        }
    }
}
