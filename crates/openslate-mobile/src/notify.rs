//! notify（Agent 主动系统通知）的 mobile 侧完整实现（notify 设计 S1+S1b）。
//!
//! 设计真相源：`docs/proposals/notify-tool-design.md`（决策 D1-D9 见
//! `.slim/deepwork/notify-tool.md`）。本模块承载：
//!
//! 1. [`NotifyTool`]：core `Tool` 实现（宿主直连，`AskUserTool` 同款
//!    模式）。双模式：默认非阻塞（fire-and-forget，不落库，宿主提交
//!    即返回）；`wait:true` 阻塞（**先落库** `pending_notifications`
//!    → host call 自定义超时 → outcome 回填原 tool call）。
//! 2. 下行信封 v:1 **内嵌 `run_id`**（D9：不改 HostCallRouter 信封结构
//!    `{type,id,tool,args}`，Kotlin 从 `args.run_id` 读取做会话路由）。
//! 3. [`MobileNotifyHandler`]：`ClientMsg::notification_reply` 的宿主
//!    实现（迟到双通路：在途 resolve / 原子关卷首答者赢 + 续跑；
//!    uplink outcome 白名单已在协议枚举处机制化）。cancel 关闭 wait
//!    行走防御实现（边角 2 决策）：迟到上行 outcome=cancelled → 原子
//!    关卷 status='cancelled' + 以 cancelled tool result 续跑（文案
//!    对齐 dismissed 语义）。
//! 4. [`startup_notify_maintenance`]：启动清扫（TTL 24h，对齐
//!    `startup_question_maintenance` 模式：过期合成 expired 补全、
//!    已解决关卷、可恢复保留；store 无 TTL / 无 DELETE）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use openslate_core::ask_user::{append_late_tool_result, dangling_tool_calls};
use openslate_core::config::{AskTimeout, NotifyConfig};
use openslate_core::error::ToolError;
use openslate_core::run_manager::RunManager;
use openslate_core::tool::Tool;
use openslate_core::types::{Message, MessageRole, ToolOutput, ToolOutputStatus};
use openslate_protocol::{NotificationOutcome, ServerMsg};
use openslate_session::notify::NotifyHandler;
use openslate_session::state::AppState;
use openslate_store_sqlite::store::SqliteStore;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::askuser::spawn_resume_turn;
use crate::hostcall::{HostCallRouter, HOST_CALL_TIMEOUT_ERR_PREFIX};

/// 工具名（注册表 / 白名单 / 审批豁免 / fail_all 豁免共用）。
pub const NOTIFY_TOOL: &str = "notify";

/// 通知等待行 TTL（24h；对齐 `QUESTION_TTL_MS` 模式——过期启动清扫
/// 合成 expired 结果，notify 设计 §4.2 / D6）。
pub const NOTIFICATION_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// `timeout = -1`（永不超时）时 host call 的实际等待上限（tokio 定时器
/// 无「无限」档，取一年——askuser.rs 同款约定）。
const NEVER_TIMEOUT: Duration = Duration::from_secs(365 * 24 * 3600);

/// 信封版本（v:1；Kotlin 对未知字段向前兼容）。
const NOTIFY_ENVELOPE_VERSION: i64 = 1;

/// 标题截断（字符数；notify 设计 §6 限额宽松档）。
const MAX_TITLE_CHARS: usize = 60;
/// 正文截断（字符数，UTF-8 边界安全——按 char 截）。
const MAX_MESSAGE_CHARS: usize = 2000;
/// 动作数上限（Android 通知按钮上限即 3）。
const MAX_ACTIONS: usize = 3;
/// **新 id** post 频率限额（条/分钟；同 id 更新豁免、wait 模式豁免
/// ——一等待一条；notify 设计 §6 / §10 决策 7）。
const RATE_LIMIT_PER_MINUTE: usize = 12;
/// 通知 id 合法字符集（`[A-Za-z0-9_-]{1,64}`，设计 §2）。
const ID_MAX_CHARS: usize = 64;

// ---------------------------------------------------------------------------
// 参数归一化（模型输入 → 下发信封契约；Rust 侧做全部业务校验，Kotlin
// 收到即合法——设计 §5.6）
// ---------------------------------------------------------------------------

/// 归一化后的动作按钮。
#[derive(Debug, Clone, PartialEq)]
struct NormalizedAction {
    label: String,
    /// 点击语义文本（缺省 = 「用户在通知上点了「label」」）。
    value: Option<String>,
    /// default | positive | destructive（仅 UI 语义）。
    style: String,
    /// cancel_turn = 立即停止当前回合（不携带 value——行为即语义）。
    behavior: Option<String>,
    /// true = 内联回复输入框（RemoteInput；忽略 value）。
    reply: bool,
    placeholder: Option<String>,
}

/// 归一化后的 notify 入参（信封的可序列化形态）。
#[derive(Debug, Clone, PartialEq)]
struct NormalizedNotify {
    /// post | cancel。
    action: String,
    id: Option<String>,
    title: Option<String>,
    message: Option<String>,
    /// normal | important | progress。
    kind: String,
    progress: Option<i64>,
    progress_indeterminate: bool,
    ongoing: bool,
    completed: bool,
    auto_cancel: bool,
    public: bool,
    wait: bool,
    timeout: Option<i64>,
    /// id | all（action=cancel 时生效）。
    cancel_scope: String,
    actions: Vec<NormalizedAction>,
}

/// 按**字符**截断（UTF-8 边界安全）+ 省略号。
fn clip_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let clipped: String = text.chars().take(max).collect();
    format!("{clipped}…")
}

/// 校验通知 id 形态（`[A-Za-z0-9_-]{1,64}`）。
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= ID_MAX_CHARS
        && id.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 归一化动作按钮数组（≤3；cancel_turn 与 value/reply 互斥；reply 忽略
/// value）。业务校验失败返回 Err（文案回喂模型自纠）。
fn normalize_actions(raw: &[Value]) -> Result<Vec<NormalizedAction>, String> {
    if raw.len() > MAX_ACTIONS {
        return Err(format!("actions 最多 {MAX_ACTIONS} 个（Android 上限），收到 {}", raw.len()));
    }
    let mut out = Vec::with_capacity(raw.len());
    for item in raw {
        let obj = item
            .as_object()
            .ok_or_else(|| "actions[] 每项必须是对象".to_owned())?;
        let label = obj
            .get("label")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "actions[] 缺 label".to_owned())?;
        if label.trim().is_empty() {
            return Err("actions[].label 不能为空".to_owned());
        }
        let label = clip_chars(label.trim(), 8);
        let behavior = obj
            .get("behavior")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        if let Some(b) = &behavior {
            if b != "cancel_turn" {
                return Err(format!("未知 behavior「{b}」（仅支持 cancel_turn）"));
            }
            // cancel_turn 行为即语义，携带 value 属模型误用——拒绝。
            if obj.get("value").is_some() {
                return Err("cancel_turn 按钮不携带 value（行为即语义）".to_owned());
            }
            if obj.get("reply").and_then(|v| v.as_bool()) == Some(true) {
                return Err("cancel_turn 与 reply 互斥".to_owned());
            }
        }
        let reply = obj.get("reply").and_then(|v| v.as_bool()).unwrap_or(false);
        // reply:true 时 value 被忽略（文本由用户输入）。
        let value = if reply {
            None
        } else {
            obj.get("value").and_then(|v| v.as_str()).map(str::to_owned)
        };
        let style = obj
            .get("style")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_owned();
        if !matches!(style.as_str(), "default" | "positive" | "destructive") {
            return Err(format!("未知 style「{style}」（default|positive|destructive）"));
        }
        let placeholder = obj
            .get("placeholder")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        out.push(NormalizedAction {
            label,
            value,
            style,
            behavior,
            reply,
            placeholder,
        });
    }
    Ok(out)
}

/// 归一化 notify 入参：缺省填充、越界钳制（progress 0..100）、字符边界
/// 安全截断（title/message）、组合校验（post 必带 title/message、wait
/// 必带 ≥1 action、wait 与 cancel 互斥、progress 互斥字段）。
fn normalize_notify_args(args: &Value) -> Result<NormalizedNotify, String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "参数必须是对象".to_owned())?;
    let action = obj
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("post")
        .to_owned();
    if !matches!(action.as_str(), "post" | "cancel") {
        return Err(format!("未知 action「{action}」（post|cancel）"));
    }

    let id = obj.get("id").and_then(|v| v.as_str()).map(str::to_owned);
    if let Some(id) = &id {
        if !valid_id(id) {
            return Err(format!(
                "id「{id}」不合法（[A-Za-z0-9_-]{{1,{ID_MAX_CHARS}}}）"
            ));
        }
    }

    let wait = obj.get("wait").and_then(|v| v.as_bool()).unwrap_or(false);
    if action == "cancel" {
        if wait {
            return Err("wait:true 不能与 action:\"cancel\" 组合".to_owned());
        }
        let cancel_scope = obj
            .get("cancel_scope")
            .and_then(|v| v.as_str())
            .unwrap_or("id")
            .to_owned();
        if !matches!(cancel_scope.as_str(), "id" | "all") {
            return Err(format!("未知 cancel_scope「{cancel_scope}」（id|all）"));
        }
        if cancel_scope == "id" && id.is_none() {
            return Err("cancel_scope=id 需要同请求携带 id 字段".to_owned());
        }
        return Ok(NormalizedNotify {
            action,
            id,
            title: None,
            message: None,
            kind: "normal".into(),
            progress: None,
            progress_indeterminate: false,
            ongoing: false,
            completed: false,
            auto_cancel: true,
            public: false,
            wait: false,
            timeout: None,
            cancel_scope,
            actions: Vec::new(),
        });
    }

    // action == post。
    let title = obj
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "post 必须携带 title".to_owned())?;
    if title.trim().is_empty() {
        return Err("title 不能为空".to_owned());
    }
    let title = clip_chars(title.trim(), MAX_TITLE_CHARS);
    let message = obj
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "post 必须携带 message".to_owned())?;
    let message = clip_chars(message.trim(), MAX_MESSAGE_CHARS);

    let kind = obj
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("normal")
        .to_owned();
    if !matches!(kind.as_str(), "normal" | "important" | "progress") {
        return Err(format!("未知 kind「{kind}」（normal|important|progress）"));
    }

    let has_progress = obj.contains_key("progress");
    let progress = obj.get("progress").and_then(|v| v.as_i64());
    let progress_indeterminate = obj
        .get("progress_indeterminate")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if has_progress && progress_indeterminate {
        return Err("progress 与 progress_indeterminate 互斥".to_owned());
    }
    // 边界钳制：0..100（101 → 100、-1 → 0）。
    let progress = progress.map(|p| p.clamp(0, 100));
    if kind != "progress" && (progress.is_some() || progress_indeterminate) {
        return Err("progress/progress_indeterminate 仅 progress 类型可用".to_owned());
    }

    let completed = obj
        .get("completed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if completed && kind != "progress" {
        return Err("completed 仅 progress 类型可用".to_owned());
    }
    let ongoing = obj.get("ongoing").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut auto_cancel = obj.get("auto_cancel").and_then(|v| v.as_bool()).unwrap_or(true);
    // 未完成的 progress 恒常驻（进行中不可点击即消）。
    if kind == "progress" && !completed {
        auto_cancel = false;
    }
    let public = obj.get("public").and_then(|v| v.as_bool()).unwrap_or(false);

    let actions = normalize_actions(
        obj.get("actions")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]),
    )?;
    let timeout = obj.get("timeout").and_then(|v| v.as_i64());
    if let Some(t) = timeout {
        if t < -1 {
            return Err("timeout 取值：正整数秒或 -1（永不）".to_owned());
        }
    }
    if wait && actions.is_empty() {
        return Err("wait:true 需要至少 1 个 action（否则用户无法回应）".to_owned());
    }

    Ok(NormalizedNotify {
        action,
        id,
        title: Some(title),
        message: Some(message),
        kind,
        progress,
        progress_indeterminate,
        ongoing,
        completed,
        auto_cancel,
        public,
        wait,
        timeout,
        cancel_scope: String::new(),
        actions,
    })
}

/// 生效等待超时：入参覆盖 > `[notify] default_wait_timeout`（D9/决策 9：
/// 缺省 300 秒）。返回（信封秒数表示, router 等待时长）。配置 0 秒按
/// 1 秒容忍（`AskTimeout::Seconds` 文档约定）。
fn effective_wait_timeout(override_secs: Option<i64>, config: &NotifyConfig) -> (i64, Duration) {
    match override_secs {
        Some(-1) => (-1, NEVER_TIMEOUT),
        Some(n) if n >= 1 => (n, Duration::from_secs(n as u64)),
        _ => match config.default_wait_timeout {
            AskTimeout::Never => (-1, NEVER_TIMEOUT),
            AskTimeout::Seconds(s) => (s.max(1) as i64, Duration::from_secs(s.max(1))),
        },
    }
}

/// 组装 host call 下发信封（D9：`run_id` 内嵌 args——信封结构
/// `{type,id,tool,args}` 不变，Kotlin 从 `args.run_id` 读）。可选字段
/// 缺席即省键；`actions[].value` 缺省在此补齐（「用户在通知上点了
/// 「label」」），Kotlin 收到即完整。
fn host_envelope(normalized: &NormalizedNotify, run_id: Option<&str>, timeout_secs: i64) -> Value {
    let mut env = json!({
        "v": NOTIFY_ENVELOPE_VERSION,
        "action": normalized.action,
    });
    let obj = env.as_object_mut().expect("envelope is object");
    if let Some(id) = &normalized.id {
        obj.insert("id".into(), json!(id));
    }
    if normalized.action == "cancel" {
        obj.insert("cancel_scope".into(), json!(normalized.cancel_scope));
    } else {
        obj.insert(
            "title".into(),
            json!(normalized.title.clone().unwrap_or_default()),
        );
        obj.insert(
            "message".into(),
            json!(normalized.message.clone().unwrap_or_default()),
        );
        obj.insert("kind".into(), json!(normalized.kind));
        if let Some(p) = normalized.progress {
            obj.insert("progress".into(), json!(p));
        }
        if normalized.progress_indeterminate {
            obj.insert("progress_indeterminate".into(), json!(true));
        }
        if normalized.ongoing {
            obj.insert("ongoing".into(), json!(true));
        }
        if normalized.completed {
            obj.insert("completed".into(), json!(true));
        }
        obj.insert("auto_cancel".into(), json!(normalized.auto_cancel));
        if normalized.public {
            obj.insert("public".into(), json!(true));
        }
        if normalized.wait {
            obj.insert("wait".into(), json!(true));
            obj.insert("timeout".into(), json!(timeout_secs));
        }
        if !normalized.actions.is_empty() {
            let actions: Vec<Value> = normalized
                .actions
                .iter()
                .map(|a| {
                    let mut item = json!({ "label": a.label });
                    let o = item.as_object_mut().expect("action is object");
                    if a.style != "default" {
                        o.insert("style".into(), json!(a.style));
                    }
                    if let Some(b) = &a.behavior {
                        o.insert("behavior".into(), json!(b));
                    }
                    if a.reply {
                        o.insert("reply".into(), json!(true));
                        if let Some(p) = &a.placeholder {
                            o.insert("placeholder".into(), json!(p));
                        }
                    } else if let Some(v) = &a.value {
                        o.insert("value".into(), json!(v));
                    } else if a.behavior.is_none() {
                        // 非 cancel_turn 按钮的 value 缺省文案（设计 §2）。
                        o.insert(
                            "value".into(),
                            json!(format!("用户在通知上点了「{}」", a.label)),
                        );
                    }
                    item
                })
                .collect();
            obj.insert("actions".into(), json!(actions));
        }
    }
    if let Some(run_id) = run_id {
        obj.insert("run_id".into(), json!(run_id));
    }
    env
}

// ---------------------------------------------------------------------------
// 结果格式化（即时/迟到共用 —— 迟到等价不变量的落点）
// ---------------------------------------------------------------------------

/// 结构化 JSON 行（与自然语言并存回填，仿 ask_user / opencode）。
fn json_line(payload: &Value) -> String {
    serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_owned())
}

/// 非 wait（及 wait 的 posted:false 立即返回路径）结果文案。
/// `posted` 的 id/kind/channel 以 Kotlin resolve payload 为权威。
fn format_post_result(payload: &Value) -> String {
    let posted = payload.get("posted").and_then(|p| p.as_bool()).unwrap_or(false);
    if posted {
        let id = payload.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let kind = payload.get("kind").and_then(|v| v.as_str()).unwrap_or("normal");
        let channel = payload
            .get("channel")
            .and_then(|v| v.as_str())
            .unwrap_or("agent_messages");
        format!(
            "Notification posted (id={id}, kind={kind}, channel={channel}). 已提交系统（不保证展示——渠道/勿扰由系统与用户设置决定）.\n{}",
            json_line(payload)
        )
    } else {
        let reason = payload
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let text = match reason {
            "permission_denied" => "通知未展示：用户未授予通知权限。请改在回复中直接说明，或请用户到系统设置开启 OpenSlate 通知。".to_owned(),
            "channel_disabled" => {
                let channel = payload.get("channel").and_then(|v| v.as_str()).unwrap_or("agent_messages");
                format!("通知未展示：渠道「{channel}」已被用户关闭。请改在回复中直接说明，或请用户到系统设置开启该渠道。")
            }
            "rate_limited" => "通知发送过于频繁。请稍后再试，或复用同一条通知的 id 更新内容。".to_owned(),
            other => format!("通知未展示：{other}。"),
        };
        format!("{text}\n{}", json_line(payload))
    }
}

/// wait 模式 outcome 结果文案（§4.2 表）。**迟到等价不变量**：即时路径
/// （Tool::execute）与迟到路径（NotifyHandler → resume_from_tool_result）
/// 共用本函数，对模型完全同形。timeout / expired 分支仅 Rust 侧合成
/// 路径调用（wire 上行不可达）。
pub fn format_wait_result(payload: &Value, timeout_secs: i64) -> String {
    let outcome = payload.get("outcome").and_then(|o| o.as_str()).unwrap_or("");
    let text = match outcome {
        "action" => {
            let label = payload.get("label").and_then(|v| v.as_str()).unwrap_or("?");
            match payload.get("value").and_then(|v| v.as_str()) {
                Some(value) => format!("用户在通知上点了「{label}」（{value}）。"),
                None => format!("用户在通知上点了「{label}」。"),
            }
        }
        "replied" => {
            let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
            format!("用户回复：{text}")
        }
        "dismissed" => "用户划掉了通知（未作选择）。请自行判断继续，并在回复中说明你采用的假设。".to_owned(),
        "cancelled" => "通知被撤销（用户停止了回合）。请自行判断继续。".to_owned(),
        "timeout" => format!("用户在 {timeout_secs} 秒内未回应通知，可能不在。请自行判断继续。"),
        "expired" => "通知等待已过期（超过 24 小时未回应）。请自行判断继续。".to_owned(),
        // posted:false 立即返回（wait 通知没能发出去，等待无从谈起）。
        _ => return format_post_result(payload),
    };
    format!("{text}\n{}", json_line(payload))
}

/// 启动清扫的过期合成文案（mobile 层专用，不经 wire）。
pub fn expired_result_content() -> String {
    format_wait_result(&json!({ "outcome": "expired" }), 0)
}

// ---------------------------------------------------------------------------
// 共享上下文（工具本体 / handler / 清扫共用；AskUserContext 同款装配时序）
// ---------------------------------------------------------------------------

struct NotifyWireParts {
    state: Weak<AppState>,
    store: Option<SqliteStore>,
    config: NotifyConfig,
}

/// notify 共享上下文。工具在 `build_app_context_with` **之前**构造
/// （AppState 尚不存在），装配完成后由 `create_with` 调 [`Self::wire`]
/// 注入 `Weak<AppState>` + store + `[notify]` 配置（防引用环，
/// `AskUserContext` 同款时序）。
pub struct NotifyContext {
    router: Arc<HostCallRouter>,
    wired: Mutex<Option<NotifyWireParts>>,
    /// 新 id post 限流窗（毫秒时间戳；同 id 更新 / wait / cancel 豁免
    /// 不进表）。独立于 wire 状态（纯内存计数，无装配依赖）。
    recent_new_posts: Mutex<VecDeque<i64>>,
}

impl NotifyContext {
    pub fn new(router: Arc<HostCallRouter>) -> Arc<Self> {
        Arc::new(Self {
            router,
            wired: Mutex::new(None),
            recent_new_posts: Mutex::new(VecDeque::new()),
        })
    }

    /// 装配后接线：从 AppState 读取 store 与 `[notify]` 配置快照。
    pub fn wire(&self, state: &Arc<AppState>) {
        let (store, config) = {
            let inner = state.core.lock();
            (inner.store.clone(), inner.config.notify.clone())
        };
        let mut guard = self.wired.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(NotifyWireParts {
            state: Arc::downgrade(state),
            store,
            config,
        });
    }

    fn parts(&self) -> Option<(Weak<AppState>, Option<SqliteStore>, NotifyConfig)> {
        let mut guard = self.wired.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_mut()
            .map(|w| (w.state.clone(), w.store.clone(), w.config.clone()))
    }

    fn state(&self) -> Option<Arc<AppState>> {
        self.parts().and_then(|(state, _, _)| state.upgrade())
    }

    fn store(&self) -> Option<SqliteStore> {
        self.parts().and_then(|(_, store, _)| store)
    }

    fn notify_config(&self) -> NotifyConfig {
        self.parts().map(|(_, _, c)| c).unwrap_or_default()
    }

    /// 当前活动 run id（信封 `run_id` 与落库行的来源；无会话返回 None
    /// ——信封省键，Kotlin 回退当前会话路由）。
    fn current_run_id(&self) -> Option<String> {
        let state = self.state()?;
        let inner = state.core.lock();
        inner.session_run.as_ref().map(|r| r.run_id.0.clone())
    }

    /// 当前悬挂 notify 调用的定位（`AskUserContext::find_call_id` 同款：
    /// 读当前 run 的持久化消息——runner 在执行工具前已把带 `tool_calls`
    /// 的 assistant 消息落盘；不能用 `inner.history`，回合进行中它是
    /// 回合前的旧副本）。返回 `(run_id, tool_call_id)`；找不到（PTC
    /// 沙箱内 bridge 调用 / 未接线 / 无 run）→ None → wait 不持久化
    /// （仅内存通路）。
    async fn find_call_site(&self, args: &Value) -> Option<(String, String)> {
        let state = self.state()?;
        let store = self.store()?;
        let run_id = {
            let inner = state.core.lock();
            inner.session_run.as_ref()?.run_id.0.clone()
        };
        let messages =
            openslate_store_sqlite::recorder::RunRecorder::load_messages_keep_dangling(
                &store, &run_id,
            )
            .await
            .ok()?;
        let notify_dangling: Vec<String> = dangling_tool_calls(&messages)
            .into_iter()
            .filter(|(_, name)| name == NOTIFY_TOOL)
            .map(|(id, _)| id)
            .collect();
        if notify_dangling.is_empty() {
            return None;
        }
        for msg in &messages {
            let Some(tool_calls) = &msg.tool_calls else { continue };
            for tc in tool_calls {
                if tc.name == NOTIFY_TOOL
                    && notify_dangling.contains(&tc.id.0)
                    && crate::askuser::AskUserContext::args_equivalent(&tc.arguments, args)
                {
                    return Some((run_id, tc.id.0.clone()));
                }
            }
        }
        // 唯一悬挂 notify 且无精确匹配（模型 args 经手差异）→ 取之。
        if notify_dangling.len() == 1 {
            return Some((run_id, notify_dangling[0].clone()));
        }
        None
    }

    /// 新 id post 限流闸（决策 7 宽松档：12 条/分钟；同 id 更新与 wait
    /// 模式豁免——进度更新合法高频、一等待一条）。超限返回 Err（文案
    /// 回喂模型）。
    fn rate_limit_new_post(&self, now_ms: i64) -> Result<(), String> {
        let mut guard = self.recent_new_posts.lock().unwrap_or_else(|e| e.into_inner());
        let window_start = now_ms - 60_000;
        while guard.front().is_some_and(|t| *t < window_start) {
            guard.pop_front();
        }
        if guard.len() >= RATE_LIMIT_PER_MINUTE {
            return Err(format!(
                "新通知发送过于频繁（上限 {RATE_LIMIT_PER_MINUTE} 条/分钟）。请复用同一条通知的 id 更新内容，或稍后再试。"
            ));
        }
        guard.push_back(now_ms);
        Ok(())
    }

    /// 通知等待行落库（发信封**之前**——崩溃窗口内通知已可见则行必在；
    /// `args_json` 为完整信封快照，含 v 与 run_id，D2/D9）。
    async fn persist_pending(&self, request_id: &str, tool_call_id: &str, run_id: &str, envelope: &Value) {
        let Some(store) = self.store() else { return };
        let now_ms = now_ms();
        if let Err(e) = store
            .insert_pending_notification(
                request_id,
                tool_call_id,
                run_id,
                &envelope.to_string(),
                now_ms,
            )
            .await
        {
            // 持久化失败不阻断通知（内存通路仍可用），仅告警。
            crate::alog!("notify: persist notification row failed (non-fatal): {e}");
        }
    }

    /// 关卷（best-effort；状态机迁移失败仅告警——迟到应答会被
    /// 「已非 pending」判定挡住，不会双补全）。
    async fn close_row(&self, request_id: &str, to: &str) {
        let Some(store) = self.store() else { return };
        if let Err(e) = store
            .transition_pending_notification(request_id, "pending", to)
            .await
        {
            crate::alog!("notify: close notification row {request_id} → {to} failed (non-fatal): {e}");
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 工具本体
// ---------------------------------------------------------------------------

/// 工具 description（设计 §2 文案要点；教模型「用户在不在 App」选工具）。
const NOTIFY_DESCRIPTION: &str = "Post an Android system notification to reach the user when they \
are probably NOT looking at the app (long background tasks, task-finished alerts, results that \
need attention). If the user is actively reading your replies, do NOT notify — ask in-chat \
(via ask_user or your reply) instead. Usage rules: \
(1) For long tasks, first post kind:\"progress\" with a stable id and ongoing:true, then update \
the SAME id as progress advances (never a new id per update), and finish with completed:true or \
a final normal notification plus cancel of the old id. \
(2) Use kind:\"important\" sparingly (sound / possible heads-up interruption). \
(3) actions (max 3) let the user respond without opening the app; add {\"reply\":true} for free \
text; add {\"behavior\":\"cancel_turn\"} (no value) to let them stop the whole turn. \
(4) wait:true blocks until the user answers on the notification (button / inline reply / swipe \
/ timeout) and the answer fills this tool call — use it only when you truly cannot proceed \
without the user's decision AND they are likely away; requires at least 1 action. timeout is \
seconds (-1 = never; default from settings, 300s). \
(5) action:\"cancel\" removes a previously posted notification by id (cancel_scope:\"all\" for \
every agent notification). \
(6) Never include secrets or sensitive plaintext in the notification; public:true makes it \
visible on the lock screen. posted:true means submitted to the system, not necessarily shown.";

/// notify 工具（宿主直连 `Tool`；`AskUserTool` 同款模式）。
pub struct NotifyTool {
    ctx: Arc<NotifyContext>,
}

impl NotifyTool {
    pub fn new(ctx: Arc<NotifyContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for NotifyTool {
    fn name(&self) -> &str {
        NOTIFY_TOOL
    }

    fn description(&self) -> &str {
        NOTIFY_DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["post", "cancel"], "description": "post (default) or cancel" },
                "id": { "type": "string", "description": "stable id, [A-Za-z0-9_-]{1,64}; reuse the same id to update; omit for a one-shot auto id" },
                "title": { "type": "string", "description": "<=60 chars, required for post" },
                "message": { "type": "string", "description": "<=2000 chars, required for post; long text auto-expands" },
                "kind": { "type": "string", "enum": ["normal", "important", "progress"], "description": "defaults to normal" },
                "progress": { "type": "integer", "minimum": 0, "maximum": 100, "description": "progress kind only" },
                "progress_indeterminate": { "type": "boolean", "description": "progress kind only; mutually exclusive with progress" },
                "ongoing": { "type": "boolean", "description": "non-dismissible while work is in flight" },
                "completed": { "type": "boolean", "description": "progress final state: drop progress bar / ongoing" },
                "auto_cancel": { "type": "boolean", "description": "dismiss on tap (default true; forced false for unfinished progress)" },
                "public": { "type": "boolean", "description": "show content on the lock screen (default false)" },
                "wait": { "type": "boolean", "description": "block until the user answers on the notification; requires >=1 action" },
                "timeout": { "type": "integer", "description": "wait only: seconds; -1 = never; default from [notify] default_wait_timeout (300)" },
                "cancel_scope": { "type": "string", "enum": ["id", "all"], "description": "action=cancel: which notifications to remove (needs id for \"id\")" },
                "actions": {
                    "type": "array", "maxItems": 3,
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": { "type": "string", "description": "1-8 chars" },
                            "value": { "type": "string", "description": "text sent back when tapped (default: 用户在通知上点了「label」); ignored when reply:true; forbidden with cancel_turn" },
                            "style": { "type": "string", "enum": ["default", "positive", "destructive"] },
                            "behavior": { "type": "string", "enum": ["cancel_turn"], "description": "stop the whole turn immediately; no value allowed" },
                            "reply": { "type": "boolean", "description": "inline reply input (RemoteInput)" },
                            "placeholder": { "type": "string", "description": "reply input hint" }
                        },
                        "required": ["label"],
                        "additionalProperties": false
                    }
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolOutput, ToolError> {
        let started = Instant::now();
        let duration = || started.elapsed().as_millis() as u64;
        let output = |content: String, status: ToolOutputStatus| ToolOutput {
            attachments: None,
            bytes: content.len(),
            content,
            duration_ms: duration(),
            status,
        };

        let normalized = match normalize_notify_args(args) {
            Ok(v) => v,
            Err(msg) => {
                return Ok(output(
                    format!("Invalid notify arguments: {msg}"),
                    ToolOutputStatus::Error,
                ))
            }
        };

        // 限额闸（宽松档）：仅**新 id** 的非 wait post 计数（同 id 更新 /
        // wait / cancel 豁免）。
        if normalized.action == "post"
            && !normalized.wait
            && normalized.id.is_some()
        {
            if let Err(msg) = self.ctx.rate_limit_new_post(now_ms()) {
                return Ok(output(
                    format!("Notification rate limited: {msg}"),
                    ToolOutputStatus::Error,
                ));
            }
        }

        // run_id：非 wait 也要带上（Kotlin 路由用）；wait 还需定位悬挂
        // tool_call_id（找不到 → PTC 沙箱 bridge 调用：不落库仅内存）。
        let (run_id, tool_call_id) = if normalized.wait {
            self.ctx.find_call_site(args).await.unwrap_or_else(|| {
                let run = self.ctx.current_run_id();
                (run.unwrap_or_default(), String::new())
            })
        } else {
            (self.ctx.current_run_id().unwrap_or_default(), String::new())
        };
        let run_id_opt = (!run_id.is_empty()).then_some(run_id.as_str());

        let (timeout_secs, timeout_dur) =
            effective_wait_timeout(normalized.timeout, &self.ctx.notify_config());
        let envelope = host_envelope(&normalized, run_id_opt, timeout_secs);

        // 非 wait：fire-and-forget——不落库，默认 host call 超时（120s）
        // 对「权限查询 + post 毫秒级」绰绰有余。
        if !normalized.wait {
            return match self.ctx.router.call(NOTIFY_TOOL, &envelope).await {
                Ok(payload) => Ok(output(format_post_result(&payload), ToolOutputStatus::Success)),
                Err(message) => Ok(output(message, ToolOutputStatus::Error)),
            };
        }

        // wait：先落库（崩溃窗口内通知已可见则行必在）→ 自定义超时等待。
        let id = self.ctx.router.allocate_id();
        let request_id = id.to_string();
        if !tool_call_id.is_empty() && !run_id.is_empty() {
            self.ctx
                .persist_pending(&request_id, &tool_call_id, &run_id, &envelope)
                .await;
        }

        match self
            .ctx
            .router
            .call_with_id(id, NOTIFY_TOOL, &envelope, timeout_dur)
            .await
        {
            Ok(payload) => {
                let content = format_wait_result(&payload, timeout_secs);
                // 关卷状态：posted:false / 异形 payload → superseded
                // （等待未成立）；outcome 白名单值同名关卷。
                let outcome = payload.get("outcome").and_then(|o| o.as_str());
                let status = match outcome {
                    Some("action") => "action",
                    Some("replied") => "replied",
                    Some("dismissed") => "dismissed",
                    Some("cancelled") => "cancelled",
                    _ => "superseded",
                };
                self.ctx.close_row(&request_id, status).await;
                let tool_status = if outcome.is_some() {
                    ToolOutputStatus::Success
                } else {
                    // posted:false 也是业务结果（ok=true 语义），模型收到
                    // 指引文案后可继续。
                    ToolOutputStatus::Success
                };
                Ok(output(content, tool_status))
            }
            Err(message) if message.starts_with(HOST_CALL_TIMEOUT_ERR_PREFIX) => {
                // 超时是业务结果：模型收到「用户可能不在，自行判断
                // 继续」。行按 timeout 关卷（时间性关闭；迟到应答被
                // 「已非 pending」判定挡住——原子 UPDATE 0 行 no-op）。
                self.ctx.close_row(&request_id, "timeout").await;
                Ok(output(
                    format_wait_result(&json!({ "outcome": "timeout" }), timeout_secs),
                    ToolOutputStatus::Success,
                ))
            }
            Err(message) => {
                // ok=false 仅基础设施错误：错误文本回喂 LLM 自纠。调用已
                // 终结（runner 会把错误结果并入历史）→ 行关卷。
                self.ctx.close_row(&request_id, "superseded").await;
                Ok(output(message, ToolOutputStatus::Error))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// NotifyHandler（迟到应答 / 取消）
// ---------------------------------------------------------------------------

/// 上行应答 → resolve payload（即时路径与迟到路径共用同一形状——
/// 迟到等价不变量的数据面）。
fn reply_payload(
    outcome: NotificationOutcome,
    label: &Option<String>,
    value: &Option<String>,
    text: &Option<String>,
) -> Value {
    let mut payload = json!({ "outcome": outcome.as_str() });
    let obj = payload.as_object_mut().expect("payload is object");
    if let Some(label) = label {
        obj.insert("label".into(), json!(label));
    }
    if let Some(value) = value {
        obj.insert("value".into(), json!(value));
    }
    if let Some(text) = text {
        obj.insert("text".into(), json!(text));
    }
    payload
}

/// 关卷状态（outcome 白名单值同名；D3 状态集）。
fn status_for(outcome: NotificationOutcome) -> &'static str {
    outcome.as_str()
}

/// `ClientMsg::notification_reply` 的 mobile 实现。
///
/// 双通路（`MobileQuestionHandler` 同构，设计 §4.2）：
/// 1. **在途 host call**（工具正等待）：`request_id` 即 host call id，
///    `resolve` 唤醒工具——工具按即时路径格式化回填。
/// 2. **迟到应答**（重启恢复后的 pending 行）：原子关卷（首答者赢）→
///    以与即时路径**完全一致**的内容补全原 tool result →
///    `RunManager::resume_from_tool_result` 从该点续跑。
pub struct MobileNotifyHandler {
    ctx: Arc<NotifyContext>,
}

impl MobileNotifyHandler {
    pub fn new(ctx: Arc<NotifyContext>) -> Self {
        Self { ctx }
    }

    /// 迟到应答路径（同步 trait 内做异步事：block_in_place 让出 worker，
    /// 内层 Handle::block_on 驱动——`MobileQuestionHandler` 同款安全性
    /// 论证）。
    fn late_reply(
        &self,
        request_id: &str,
        run_id: &str,
        outcome: NotificationOutcome,
        label: &Option<String>,
        value: &Option<String>,
        text: &Option<String>,
    ) -> bool {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                self.late_reply_async(request_id, run_id, outcome, label, value, text)
                    .await
            })
        })
    }

    async fn late_reply_async(
        &self,
        request_id: &str,
        reply_run_id: &str,
        outcome: NotificationOutcome,
        label: &Option<String>,
        value: &Option<String>,
        text: &Option<String>,
    ) -> bool {
        let Some(store) = self.ctx.store() else {
            return false;
        };
        let Some(state) = self.ctx.state() else {
            return false;
        };
        // 前置校验（真正的首答者赢在下方原子迁移）。timeout / 已关卷的
        // 行在此返回——迟到上行 no-op（不注入新消息、不续跑）。
        let Ok(Some(row)) = store.get_pending_notification(request_id).await else {
            return false;
        };
        if row.status != "pending" || row.tool_call_id.is_empty() {
            crate::alog!(
                "notify: late reply {request_id} ignored (status={})",
                row.status
            );
            return false;
        }
        // 纵深防御（D2 run_id 列的价值落点）：上行 run_id 与行 run_id
        // 错配 = 路由错乱（openSession 失败回退 / 通知被串台），拒绝
        // 续跑——行保持 pending 等待正确路由或 TTL 兜底。
        if !reply_run_id.is_empty() && reply_run_id != row.run_id {
            crate::alog!(
                "notify: late reply {request_id} run_id mismatch (reply={reply_run_id} row={}), rejected",
                row.run_id
            );
            return false;
        }
        // 悬挂前置校验：tool call 已在历史里被解决（timeout 关卷竞态 /
        // superseded 修复抢跑 / 结果已落地）→ 关卷拒绝，避免 resume 撞
        // AlreadyAnswered 后广播 TurnError 的差体验。锁内只做计算，
        // guard 先放再 await。
        let still_dangling = {
            let inner = state.core.lock();
            dangling_tool_calls(&inner.history)
                .iter()
                .any(|(id, _)| *id == row.tool_call_id)
        };
        if !still_dangling {
            let _ = store
                .transition_pending_notification(request_id, "pending", "superseded")
                .await;
            return false;
        }

        // 与即时路径同一格式化函数（迟到等价不变量）。timeout_secs 不
        // 参与（迟到路径无 timeout outcome——wire 白名单不可达）。
        let payload = reply_payload(outcome, label, value, text);
        let content = format_wait_result(&payload, 0);
        let new_status = status_for(outcome);

        // 引擎快照 + busy 守卫 + running 置位（同一锁内完成，防竞态；
        // MobileQuestionHandler 同款）。
        let snapshot = {
            let mut inner = state.core.lock();
            if inner.busy() {
                None
            } else {
                let run_snapshot = inner
                    .session_run
                    .as_ref()
                    .map(|run| (run.run_id.clone(), run.recorder.clone()));
                let manager = inner.manager.take();
                match (run_snapshot, manager) {
                    (Some((run_id, recorder)), Some(mut manager)) => {
                        inner.running = true;
                        inner.stream = Default::default();
                        let cancel = CancellationToken::new();
                        inner.cancel = Some(cancel.clone());
                        let config = inner.config.clone();
                        let model_alias = inner.model_alias.clone();
                        let history = inner.history.clone();
                        manager.message_sink = Some(recorder as Arc<dyn openslate_core::runtime::MessageSink>);
                        Some((manager, run_id, history, config, model_alias, cancel))
                    }
                    (run_snapshot, manager) => {
                        // 无 run / manager 不在家（瞬态）：放回。
                        let _ = run_snapshot;
                        if let Some(manager) = manager {
                            inner.manager = Some(manager);
                        }
                        None
                    }
                }
            }
        };
        let Some((manager, run_id, history, config, model_alias, cancel)) = snapshot else {
            return false; // busy / 无 run：不动行，调用方收 notice
        };

        // 原子关卷（首答者赢）。输掉竞态（如 timeout 关卷刚好先落地）→
        // 回收引擎占用，迟到方收 notice。
        let won = matches!(
            store
                .transition_pending_notification(request_id, "pending", new_status)
                .await,
            Ok(true)
        );
        if !won {
            recycle_after_failed_start(&state, manager);
            return false;
        }

        // provider 构建（失败 → 回收 + 行重开 pending，可重试）。
        let provider = match (state.provider_factory)(&config, &model_alias) {
            Ok(p) => p,
            Err(e) => {
                recycle_after_failed_start(&state, manager);
                let _ = store
                    .transition_pending_notification(request_id, new_status, "pending")
                    .await;
                state.sink.broadcast(ServerMsg::Notice {
                    text: format!("续跑构建 provider 失败：{e}"),
                    level: openslate_protocol::NoticeLevel::Error,
                });
                return false;
            }
        };

        let handle = spawn_resume_turn(
            "notify",
            state.clone(),
            manager,
            run_id,
            provider,
            history,
            row.tool_call_id.clone(),
            content,
            cancel,
        );
        {
            let mut inner = state.core.lock();
            inner.engine_task = Some(handle);
        }
        crate::alog!("notify: late reply {request_id} resumed run from tool result");
        true
    }
}

/// 启动失败后的引擎占用回收（`askuser.rs` 同款字段复位；抽公共属后续
/// 重构，先保持两处一致）。
fn recycle_after_failed_start(state: &Arc<AppState>, mut manager: RunManager) {
    let mut inner = state.core.lock();
    inner.running = false;
    inner.cancel = None;
    manager.agent_tree = inner.agent_tree.clone();
    inner.manager = Some(manager);
}

impl NotifyHandler for MobileNotifyHandler {
    fn on_reply(
        &self,
        request_id: &str,
        run_id: &str,
        outcome: NotificationOutcome,
        label: Option<String>,
        value: Option<String>,
        text: Option<String>,
    ) -> bool {
        // 通路 1：在途 host call（requestId = host call id）。resolve 无害
        // no-op 保证：非在途 id 返回 false，落到迟到路径。
        if let Ok(id) = request_id.parse::<u64>() {
            let payload = reply_payload(outcome, &label, &value, &text);
            if self.ctx.router.resolve(id, true, payload.to_string()) {
                // 行状态 best-effort（后台关卷，不阻塞应答者）。
                let ctx = self.ctx.clone();
                let request_id = request_id.to_owned();
                let status = status_for(outcome).to_owned();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        ctx.close_row(&request_id, &status).await;
                    });
                }
                return true;
            }
        }
        self.late_reply(request_id, run_id, outcome, &label, &value, &text)
    }

    fn on_cancel_all(&self) {
        // 用户显式取消回合：悬挂 wait 通知行按 superseded 关卷（引擎
        // select! 已为在途调用合成 cancelled 结果，历史侧自洽；cancelled
        // 是 outcome 词表、行状态机在此折叠为 superseded——同 ask_user
        // 的 on_cancel_all 语义与 busy 守卫论证）。
        let Some(state) = self.ctx.state() else { return };
        let Some(store) = self.ctx.store() else { return };
        {
            let inner = state.core.lock();
            if inner.cancel.is_none() {
                return;
            }
        }
        let ctx = self.ctx.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let Ok(rows) = store.list_pending_notifications().await else {
                    return;
                };
                for row in rows {
                    ctx.close_row(&row.request_id, "superseded").await;
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// 启动清扫（TTL 过期 → expired 合成；已解决行 → superseded 关卷）
// ---------------------------------------------------------------------------

/// 重启时的通知等待行维护（`startup_question_maintenance` 同构兄弟，
/// D6：清扫在 mobile 层启动路径，store 无 TTL / 无 DELETE；在
/// assemble_state 恢复 history 之后调用）：
///
/// - **过期**（created_at 早于 now-TTL）且悬挂 → `append_late_tool_result`
///   合成 [`expired_result_content`] 补全；行 → expired。声明不在本
///   history 的过期行无补全点，同样 expired 关卷（TTL 兜底语义）。
/// - **已解决**（恢复的 history 里确有该 tool_call_id 的结果消息——
///   崩溃发生在结果落地与行更新之间）→ 行 → superseded。
/// - **其余 pending 保持**：悬挂者可恢复（迟到 notification_reply 续跑）；
///   声明与结果均不在本 history 者（属于其它 run）同样保持 pending——
///   「本 history 查无此调用」不构成「已被取代」的证据，TTL 过期兜底。
///
/// 返回仍可恢复（保持 pending 且当前 history 中悬挂）的 tool_call_id 集。
pub async fn startup_notify_maintenance(
    store: &SqliteStore,
    history: &mut Vec<Message>,
    recorder: Option<&Arc<openslate_store_sqlite::recorder::RunRecorder>>,
) -> Vec<String> {
    let mut recoverable = Vec::new();
    let Ok(rows) = store.list_pending_notifications().await else {
        return recoverable;
    };
    if rows.is_empty() {
        return recoverable;
    }
    let now = now_ms();
    let mut dangling: std::collections::HashSet<String> = dangling_tool_calls(history)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let resolved_here: std::collections::HashSet<String> = history
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .filter_map(|m| m.tool_call_id.as_ref().map(|id| id.0.clone()))
        .collect();

    for row in rows {
        let is_dangling = dangling.contains(&row.tool_call_id);
        let expired = now.saturating_sub(row.created_at) >= NOTIFICATION_TTL_MS;
        if expired {
            if is_dangling {
                match append_late_tool_result(
                    &mut *history,
                    &row.tool_call_id,
                    &expired_result_content(),
                ) {
                    Ok(msg) => {
                        if let Some(recorder) = recorder {
                            if let Err(e) = recorder.write_message(&msg).await {
                                crate::alog!(
                                    "notify startup: persist expired synth failed (non-fatal): {e}"
                                );
                            }
                        }
                        dangling.remove(&row.tool_call_id);
                    }
                    Err(e) => {
                        // 无声明处（负载/历史错位）：仅关卷。
                        crate::alog!("notify startup: expired synth skipped: {e}");
                    }
                }
            }
            let _ = store
                .transition_pending_notification(&row.request_id, "pending", "expired")
                .await;
        } else if is_dangling {
            recoverable.push(row.tool_call_id.clone());
        } else if resolved_here.contains(&row.tool_call_id) {
            // 结果已在但行还 pending（崩溃窗口）→ 关卷。
            let _ = store
                .transition_pending_notification(&row.request_id, "pending", "superseded")
                .await;
        } else {
            // 声明与结果均不在本 history（行属于其它 run / 会话）：保持
            // pending，不误关——迟到应答补全或 TTL 过期兜底。
        }
    }
    recoverable
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── 归一化矩阵（设计 §8.1）────────────────────────────────────────

    #[test]
    fn normalize_applies_defaults() {
        let n = normalize_notify_args(&json!({
            "title": "导出完成", "message": "已导出 128 张照片"
        }))
        .expect("post 最小形");
        assert_eq!(n.action, "post");
        assert_eq!(n.kind, "normal");
        assert!(n.auto_cancel, "缺省 auto_cancel=true");
        assert!(!n.public);
        assert!(!n.wait);
        assert!(n.actions.is_empty());
        assert!(n.id.is_none(), "缺省 id 省键（Kotlin 生成一次性 id）");

        // wait 缺省 timeout：不进归一化（生效值由 effective_wait_timeout
        // 在执行期决定），信封在 wait 分支携带生效值。
        let n = normalize_notify_args(&json!({
            "title": "确认", "message": "删吗",
            "wait": true,
            "actions": [{"label": "删除", "style": "positive"}]
        }))
        .expect("wait 形");
        assert!(n.wait);
        assert_eq!(n.actions.len(), 1);
        assert_eq!(n.actions[0].style, "positive");
        assert!(n.actions[0].value.is_none(), "value 缺省在信封组装时补齐");
    }

    #[test]
    fn normalize_rejects_bad_shapes() {
        // post 缺 title / message。
        assert!(normalize_notify_args(&json!({"message": "x"})).is_err());
        assert!(normalize_notify_args(&json!({"title": "x"})).is_err());
        // 非法枚举。
        assert!(normalize_notify_args(&json!({"title": "t", "message": "m", "kind": "loud"})).is_err());
        assert!(normalize_notify_args(&json!({"action": "reply"})).is_err());
        // id 形态。
        assert!(normalize_notify_args(&json!({"title": "t", "message": "m", "id": "带空格 的id"})).is_err());
        assert!(normalize_notify_args(&json!({"title": "t", "message": "m", "id": ""})).is_err());
        // wait 组合校验。
        assert!(normalize_notify_args(&json!({"title": "t", "message": "m", "wait": true})).is_err(), "wait 必带 action");
        assert!(normalize_notify_args(&json!({"action": "cancel", "wait": true, "id": "a"})).is_err(), "wait 与 cancel 互斥");
        // progress 仅 progress kind + 互斥字段。
        assert!(normalize_notify_args(&json!({"title": "t", "message": "m", "progress": 40})).is_err());
        assert!(normalize_notify_args(&json!({
            "title": "t", "message": "m", "kind": "progress",
            "progress": 40, "progress_indeterminate": true
        }))
        .is_err());
        // cancel_scope=id 缺 id。
        assert!(normalize_notify_args(&json!({"action": "cancel"})).is_err());
        // actions 超限。
        let too_many = json!({
            "title": "t", "message": "m",
            "actions": [
                {"label": "a"}, {"label": "b"}, {"label": "c"}, {"label": "d"}
            ]
        });
        assert!(normalize_notify_args(&too_many).is_err());
        // cancel_turn 带 value 拒绝（行为即语义）。
        let ct = json!({
            "title": "t", "message": "m",
            "actions": [{"label": "停止", "behavior": "cancel_turn", "value": "x"}]
        });
        assert!(normalize_notify_args(&ct).is_err());
    }

    #[test]
    fn normalize_clamps_and_clips() {
        // progress 边界：0/100 原样、101→100、-1→0。
        for (input, expect) in [(0i64, 0), (100, 100), (101, 100), (-1, 0)] {
            let n = normalize_notify_args(&json!({
                "title": "t", "message": "m", "kind": "progress", "progress": input
            }))
            .expect("clamp");
            assert_eq!(n.progress, Some(expect), "progress {input} → {expect}");
        }
        // 字符边界截断（多字节字符不劈裂）。
        let long_title: String = "标".repeat(80);
        let n = normalize_notify_args(&json!({"title": long_title, "message": "m"})).unwrap();
        let title = n.title.unwrap();
        assert!(title.chars().count() <= MAX_TITLE_CHARS + 1, "60 字 + …");
        assert!(title.ends_with('…'));
        let long_msg: String = "文".repeat(3000);
        let n = normalize_notify_args(&json!({"title": "t", "message": long_msg})).unwrap();
        assert!(n.message.unwrap().chars().count() <= MAX_MESSAGE_CHARS + 1);
        // label 截 8 字符。
        let n = normalize_notify_args(&json!({
            "title": "t", "message": "m",
            "actions": [{"label": "这是一个特别长的按钮文字"}]
        }))
        .unwrap();
        assert!(n.actions[0].label.chars().count() <= 9);
        // 未完成 progress 强制 auto_cancel=false。
        let n = normalize_notify_args(&json!({
            "title": "t", "message": "m", "kind": "progress",
            "progress": 10, "auto_cancel": true
        }))
        .unwrap();
        assert!(!n.auto_cancel, "未完成 progress 不可点击即消");
        let done = normalize_notify_args(&json!({
            "title": "t", "message": "m", "kind": "progress",
            "progress": 100, "completed": true
        }))
        .unwrap();
        assert!(done.auto_cancel, "终态恢复点击即消缺省");
    }

    #[test]
    fn effective_wait_timeout_matrix() {
        let mut config = NotifyConfig::default();
        assert_eq!(config.default_wait_timeout, AskTimeout::Seconds(300), "决策 9 缺省");
        // 入参覆盖。
        assert_eq!(
            effective_wait_timeout(Some(60), &config),
            (60, Duration::from_secs(60))
        );
        // -1 = 永不（近似无限）。
        let (secs, dur) = effective_wait_timeout(Some(-1), &config);
        assert_eq!(secs, -1);
        assert_eq!(dur, NEVER_TIMEOUT);
        // 配置缺省。
        assert_eq!(
            effective_wait_timeout(None, &config),
            (300, Duration::from_secs(300))
        );
        config.default_wait_timeout = AskTimeout::Never;
        let (secs, dur) = effective_wait_timeout(None, &config);
        assert_eq!((secs, dur), (-1, NEVER_TIMEOUT));
    }

    /// 信封逐字段契约（Android 对齐；D9 run_id 内嵌 args）。
    #[test]
    fn host_envelope_matches_android_contract() {
        let normalized = normalize_notify_args(&json!({
            "action": "post", "wait": true, "timeout": 300,
            "id": "confirm-delete", "title": "确认删除",
            "message": "要删除 3 个旧导出文件吗？", "kind": "important",
            "actions": [
                {"label": "删除", "style": "positive"},
                {"label": "取消", "style": "destructive"},
                {"label": "回复", "reply": true, "placeholder": "输入给 Agent 的话"}
            ]
        }))
        .unwrap();
        let env = host_envelope(&normalized, Some("r-20261026-abc"), 300);
        // 逐字段断言（快照式）。
        assert_eq!(env["v"], 1);
        assert_eq!(env["action"], "post");
        assert_eq!(env["id"], "confirm-delete");
        assert_eq!(env["title"], "确认删除");
        assert_eq!(env["kind"], "important");
        assert_eq!(env["wait"], true);
        assert_eq!(env["timeout"], 300);
        assert_eq!(env["run_id"], "r-20261026-abc", "D9：run_id 内嵌 args");
        assert_eq!(env["actions"][0]["label"], "删除");
        assert_eq!(
            env["actions"][0]["value"],
            json!("用户在通知上点了「删除」"),
            "非 cancel_turn 按钮 value 缺省补齐"
        );
        assert_eq!(env["actions"][1]["label"], "取消");
        assert!(env["actions"][2]["reply"].as_bool().unwrap());
        assert!(env["actions"][2].get("value").is_none(), "reply 忽略 value");
        // cancel_turn 按钮不带 value 键（行为即语义）。
        let normalized = normalize_notify_args(&json!({
            "title": "t", "message": "m",
            "actions": [{"label": "停止任务", "behavior": "cancel_turn"}]
        }))
        .unwrap();
        let env = host_envelope(&normalized, None, 300);
        assert!(env.get("run_id").is_none(), "无 run 时空缺省（PTC/未接线）");
        assert!(env.get("wait").is_none(), "非 wait 不携带 wait/timeout");
        assert!(env.get("timeout").is_none());
        assert_eq!(env["actions"][0]["behavior"], "cancel_turn");
        assert!(env["actions"][0].get("value").is_none());
        // cancel 分支。
        let normalized =
            normalize_notify_args(&json!({"action": "cancel", "id": "export"})).unwrap();
        let env = host_envelope(&normalized, Some("r-1"), 300);
        assert_eq!(env["action"], "cancel");
        assert_eq!(env["cancel_scope"], "id");
        assert!(env.get("title").is_none(), "cancel 不带展示字段");
        let all = normalize_notify_args(&json!({"action": "cancel", "cancel_scope": "all"})).unwrap();
        let env = host_envelope(&all, Some("r-1"), 300);
        assert_eq!(env["cancel_scope"], "all");
    }

    /// 结果格式化：posted / posted:false 原因 / wait 全 outcome。
    #[test]
    fn format_results_matrix() {
        let posted = format_post_result(&json!({
            "posted": true, "id": "export-photos", "kind": "progress", "channel": "agent_progress"
        }));
        assert!(posted.contains("Notification posted (id=export-photos, kind=progress, channel=agent_progress)"));
        assert!(posted.contains("\"posted\":true"));

        for (reason, needle) in [
            ("permission_denied", "未授予通知权限"),
            ("channel_disabled", "已被用户关闭"),
            ("rate_limited", "发送过于频繁"),
        ] {
            let out = format_post_result(&json!({ "posted": false, "reason": reason }));
            assert!(out.contains(needle), "{reason}: {out}");
            assert!(out.contains("\"posted\":false"));
        }

        // wait 全 outcome（含 Rust 侧合成的 timeout / expired——文案层
        // 统一，wire 白名单在协议层）。
        let action = format_wait_result(
            &json!({"outcome": "action", "label": "确认", "value": "用户在通知上点了「确认」"}),
            300,
        );
        assert!(action.contains("用户在通知上点了「确认」"));
        assert!(action.contains("\"outcome\":\"action\""));
        let replied = format_wait_result(&json!({"outcome": "replied", "text": "改成室内活动"}), 300);
        assert!(replied.contains("用户回复：改成室内活动"));
        let dismissed = format_wait_result(&json!({"outcome": "dismissed"}), 300);
        assert!(dismissed.contains("用户划掉了通知"));
        let cancelled = format_wait_result(&json!({"outcome": "cancelled"}), 300);
        assert!(cancelled.contains("通知被撤销"), "cancelled 文案对齐 dismissed 语义");
        let timeout = format_wait_result(&json!({"outcome": "timeout"}), 300);
        assert!(timeout.contains("用户在 300 秒内未回应通知"));
        let expired = expired_result_content();
        assert!(expired.contains("超过 24 小时未回应"));
        // wait 路径的 posted:false 立即返回（等待未成立）。
        let denied = format_wait_result(
            &json!({"posted": false, "reason": "permission_denied"}),
            300,
        );
        assert!(denied.contains("未授予通知权限"));
    }

    /// 新 id 限流（决策 7 宽松档）：12 条/分钟，第 13 条拒绝；窗口
    /// 滑动后重新放行（同 id / wait / cancel 不进计数——execute 分支
    /// 已按形态豁免）。
    #[test]
    fn rate_limit_new_post_gate() {
        let (tx, _rx) = std::sync::mpsc::channel::<String>();
        struct Cb(std::sync::mpsc::Sender<String>);
        impl crate::EventCallback for Cb {
            fn on_event(&self, _event_json: String) {}
        }
        let sink = crate::events::EventSink::new(Arc::new(Cb(tx)));
        let ctx = NotifyContext::new(crate::hostcall::HostCallRouter::new(
            sink,
            Duration::from_secs(5),
        ));
        for i in 0..RATE_LIMIT_PER_MINUTE {
            assert!(
                ctx.rate_limit_new_post(1000 + i as i64).is_ok(),
                "第 {} 条放行",
                i + 1
            );
        }
        assert!(ctx.rate_limit_new_post(2000).is_err(), "第 13 条拒绝");
        // 窗口滑动：旧记录出窗后重新放行。
        assert!(ctx.rate_limit_new_post(1000 + 61_000).is_ok());
    }

    /// 启动清扫（TTL / 已解决关卷 / 可恢复保留）——ask_user 同款矩阵。
    #[tokio::test]
    async fn startup_maintenance_expires_and_reconciles() {
        let store = openslate_store_sqlite::store::SqliteStore::new_in_memory()
            .await
            .unwrap();
        store.run_migrations().await.unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        // 过期行（超 TTL）+ 新鲜行 + 已解决行 + 他人 run 行。
        store
            .insert_pending_notification("old", "tc-old", "run-1", "{}", now - NOTIFICATION_TTL_MS - 1)
            .await
            .unwrap();
        store
            .insert_pending_notification("fresh", "tc-fresh", "run-1", "{}", now)
            .await
            .unwrap();
        store
            .insert_pending_notification("gone", "tc-gone", "run-1", "{}", now)
            .await
            .unwrap();
        store
            .insert_pending_notification(
                "other-run",
                "tc-other",
                "run-2",
                "{}",
                now,
            )
            .await
            .unwrap();

        fn assistant_with_notify(id: &str) -> Message {
            Message {
                attachments: None,
                role: MessageRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                name: None,
                tool_calls: Some(vec![openslate_core::types::ToolCall {
                    id: openslate_core::types::ToolCallId(id.into()),
                    name: NOTIFY_TOOL.into(),
                    arguments: serde_json::json!({}),
                }]),
                reasoning_content: None,
            }
        }
        fn tool_result_msg(id: &str, content: &str) -> Message {
            Message {
                attachments: None,
                role: MessageRole::Tool,
                content: content.into(),
                tool_call_id: Some(openslate_core::types::ToolCallId(id.into())),
                name: Some(NOTIFY_TOOL.into()),
                tool_calls: None,
                reasoning_content: None,
            }
        }

        let mut history = vec![
            Message {
                attachments: None,
                role: MessageRole::User,
                content: "go".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            },
            assistant_with_notify("tc-old"),
            assistant_with_notify("tc-fresh"),
            tool_result_msg("tc-gone", "already answered elsewhere"),
        ];

        let recoverable = startup_notify_maintenance(&store, &mut history, None).await;
        assert_eq!(
            recoverable,
            vec!["tc-fresh".to_owned()],
            "仅新鲜悬挂可恢复（他人 run 保持 pending 不在恢复集）"
        );

        // 过期：行 expired + 历史补全 expired 文案。
        assert_eq!(
            store.get_pending_notification("old").await.unwrap().unwrap().status,
            "expired"
        );
        let expired_msg = history
            .iter()
            .find(|m| m.tool_call_id.as_ref().map(|id| id.0.as_str()) == Some("tc-old"))
            .expect("expired synth appended");
        assert_eq!(expired_msg.content, expired_result_content());
        // 已解决：行 superseded。
        assert_eq!(
            store.get_pending_notification("gone").await.unwrap().unwrap().status,
            "superseded"
        );
        // 新鲜 + 他人 run：仍 pending。
        assert_eq!(
            store.get_pending_notification("fresh").await.unwrap().unwrap().status,
            "pending"
        );
        assert_eq!(
            store.get_pending_notification("other-run").await.unwrap().unwrap().status,
            "pending",
            "他人 run 行不误关"
        );
        // 悬挂态：tc-fresh 仍悬挂（预期恢复态），tc-old 已补全。
        let dangling: Vec<String> = dangling_tool_calls(&history)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(dangling.contains(&"tc-fresh".to_owned()));
        assert!(!dangling.contains(&"tc-old".to_owned()));
    }
}
