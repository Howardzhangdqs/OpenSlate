//! ask_user（Agent 向用户提问）的 mobile 侧完整实现（ask-user 设计 r4/r5）。
//!
//! 设计真相源：`docs/proposals/ask-user-tool-design.md`。本模块承载：
//!
//! 1. [`AskUserTool`]：core `Tool` 实现（宿主直连，`HostTool` 同款模式），
//!    经 [`HostCallRouter`] 自定义超时（`[ask_user] default_timeout`），
//!    下发信封与回填文案严格对齐 Android UI 契约（见模块底部测试）。
//! 2. **secret 会话凭据**（本 lane 内存版）：[`AskUserSecretVault`] trait +
//!    [`InMemorySecretVault`]——工具收到 secret 答案时入表，模型只收到
//!    引用回填；**明文不进任何日志/上下文**。持久化加密存储、env 注入
//!    shell、file 写入、输出脱敏属下一 lane（trait 即替换接缝）。
//! 3. **问卷持久化**：store v3 `pending_questions` 表（request_id 主键 =
//!    host call id，TTL 24h，状态机 pending→answered/declined/superseded/
//!    expired）；启动清扫 [`startup_question_maintenance`]（过期 → 合成
//!    expired 补全 + 关卷）。
//! 4. [`MobileQuestionHandler`]：`ClientMsg::question_reply` 的宿主实现。
//!    在途 host call 优先（`resolve` 唤醒工具，即时路径）；否则迟到答案
//!    以**与即时路径完全一致**的结果内容（迟到等价不变量，设计 §3 工程要点
//!    7）经 `RunManager::resume_from_tool_result` 补全原 tool call 并续跑。
//!    幂等首答者赢由 store 的原子状态迁移保证。
//! 5. [`SideEffectFreeApproval`]（原 `AskUserExemptApproval`，长输出落盘
//!    C4 起 general 为无副作用工具免审器）：ask_user 与 output_query 免
//!    审批（Manual 全审模式同样免——见类型文档）。

use std::collections::HashSet;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use openslate_core::ask_user::{
    append_late_tool_result, dangling_tool_calls, ASK_USER_TOOL, EXPIRED_RESULT,
};
use openslate_core::config::{AskTimeout, AskUserConfig};
use openslate_core::error::ToolError;
use openslate_core::provider::ModelProvider;
use openslate_core::run_manager::RunManager;
use openslate_core::runtime::MessageSink;
use openslate_core::tool::Tool;
use openslate_core::types::{Message, MessageRole, RunId, ToolOutput, ToolOutputStatus};
use openslate_protocol::{
    PendingQuestionDto, QuestionReplyAction, QuestionStatus, ServerMsg, TurnErrorKind,
    TurnSummaryDto,
};
use openslate_session::engine::{
    finalize_turn, panic_payload_summary, SessionProgressBridge, TurnTerminal,
};
use openslate_session::question::QuestionHandler;
use openslate_session::state::{flush_stream, fold_tool_outcomes, AppState};
use openslate_store_sqlite::store::SqliteStore;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::hostcall::{HostCallRouter, HOST_CALL_TIMEOUT_ERR_PREFIX};

/// 问卷 TTL（24h；过期启动清扫合成 expired 结果，设计 §6 S1）。
pub const QUESTION_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// `timeout = -1`（永不超时）时 host call 的实际等待上限。tokio 定时器
/// 无「无限」档，取一年——移动端等人场景事实上的无限。
const NEVER_TIMEOUT: Duration = Duration::from_secs(365 * 24 * 3600);

/// 问卷负载版本（S1 恒 1；编辑/撤销问卷的版本语义留给后续 lane）。
const QUESTION_VERSION: i64 = 1;

/// 整卡 decline 的回填文案（设计 §2 逐字）。
const DECLINED_TEXT: &str =
    "用户关闭了提问卡片未作答。请自行判断继续，并在回复中说明你采用的假设。";

// ---------------------------------------------------------------------------
// 参数归一化（模型输入 → Android 下发契约）
// ---------------------------------------------------------------------------

/// 题型（设计 §2；`type` 缺省 choice）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionKind {
    Choice,
    Text,
    Confirm,
    Secret,
}

impl QuestionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Choice => "choice",
            Self::Text => "text",
            Self::Confirm => "confirm",
            Self::Secret => "secret",
        }
    }
}

/// secret 落地形态（仅 secret 题型；设计 r3 拍板 env/file 双形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionStorage {
    Env { name: String },
    File { path: String },
}

impl QuestionStorage {
    /// 模型可见的引用描述（结构化 JSON 用）。
    fn reference(&self) -> String {
        match self {
            Self::Env { name } => format!("env:{name}"),
            Self::File { path } => format!("file:{path}"),
        }
    }

    /// 自然语言引用（回填行用）。
    fn display(&self) -> String {
        match self {
            Self::Env { name } => format!("环境变量 {name}"),
            Self::File { path } => format!("文件 {path}"),
        }
    }
}

/// 归一化后的单题（下发信封 `questions[]` 的 Rust 镜像）。
#[derive(Debug, Clone)]
pub struct NormalizedQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub kind: QuestionKind,
    pub options: Vec<(String, Option<String>, bool)>, // (label, description, recommended)
    pub multi_select: bool,
    pub placeholder: Option<String>,
    pub optional: bool,
    pub storage: Option<QuestionStorage>,
}

impl NormalizedQuestion {
    /// 下发信封里的单题 JSON（Android 契约形状：id/header/question/type/
    /// options/multi_select/placeholder/optional/storage；options 仅
    /// choice/confirm，multi_select 仅 choice，placeholder 可缺省，
    /// storage 仅 secret）。
    fn to_envelope(&self) -> Value {
        let mut q = json!({
            "id": self.id,
            "header": self.header,
            "question": self.question,
            "type": self.kind.as_str(),
        });
        let obj = q.as_object_mut().expect("literal object");
        match self.kind {
            QuestionKind::Choice | QuestionKind::Confirm => {
                obj.insert(
                    "options".into(),
                    Value::Array(
                        self.options
                            .iter()
                            .map(|(label, description, recommended)| {
                                let mut o = json!({ "label": label });
                                let om = o.as_object_mut().expect("literal object");
                                if let Some(d) = description {
                                    om.insert("description".into(), Value::String(d.clone()));
                                }
                                if *recommended {
                                    om.insert("recommended".into(), Value::Bool(true));
                                }
                                Value::Object(om.clone())
                            })
                            .collect(),
                    ),
                );
                if self.kind == QuestionKind::Choice {
                    obj.insert("multi_select".into(), Value::Bool(self.multi_select));
                }
            }
            QuestionKind::Secret => {
                if let Some(storage) = &self.storage {
                    obj.insert("storage".into(), storage_to_envelope(storage));
                }
            }
            QuestionKind::Text => {}
        }
        if let Some(p) = &self.placeholder {
            obj.insert("placeholder".into(), Value::String(p.clone()));
        }
        obj.insert("optional".into(), Value::Bool(self.optional));
        q
    }

    /// 从持久化 JSON（自己写入的归一化形状）还原；损坏返回 None（调用
    /// 方按已关闭问卷处理——不应发生，防御式）。
    fn from_envelope(value: &Value) -> Option<Self> {
        let kind = match value.get("type")?.as_str()? {
            "choice" => QuestionKind::Choice,
            "text" => QuestionKind::Text,
            "confirm" => QuestionKind::Confirm,
            "secret" => QuestionKind::Secret,
            _ => return None,
        };
        let options = value
            .get("options")
            .and_then(|o| o.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| {
                        Some((
                            o.get("label")?.as_str()?.to_owned(),
                            o.get("description").and_then(|d| d.as_str()).map(str::to_owned),
                            o.get("recommended").and_then(|r| r.as_bool()).unwrap_or(false),
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let storage = value.get("storage").and_then(storage_from_envelope);
        Some(Self {
            id: value.get("id")?.as_str()?.to_owned(),
            header: value
                .get("header")
                .and_then(|h| h.as_str())
                .unwrap_or_default()
                .to_owned(),
            question: value.get("question")?.as_str()?.to_owned(),
            kind,
            options,
            multi_select: value
                .get("multi_select")
                .and_then(|m| m.as_bool())
                .unwrap_or(false),
            placeholder: value
                .get("placeholder")
                .and_then(|p| p.as_str())
                .map(str::to_owned),
            optional: value
                .get("optional")
                .and_then(|o| o.as_bool())
                .unwrap_or(false),
            storage,
        })
    }
}

fn storage_to_envelope(storage: &QuestionStorage) -> Value {
    match storage {
        QuestionStorage::Env { name } => json!({ "kind": "env", "name": name }),
        QuestionStorage::File { path } => json!({ "kind": "file", "path": path }),
    }
}

fn storage_from_envelope(value: &Value) -> Option<QuestionStorage> {
    match value.get("kind")?.as_str()? {
        "env" => Some(QuestionStorage::Env {
            name: value.get("name")?.as_str()?.to_owned(),
        }),
        "file" => Some(QuestionStorage::File {
            path: value.get("path")?.as_str()?.to_owned(),
        }),
        _ => None,
    }
}

/// 校验 + 归一化工具入参。返回 `(归一化题集, timeout 覆盖)`；错误文案
/// 直接回喂模型自纠（业务错误约定，见 adding-a-tool.md §1a）。
///
/// 约束（设计 §2）：1~4 题；choice/confirm 2~4 个选项且 label 非空；
/// secret 必填 storage（env→name / file→path）；timeout 为 -1（永不）
/// 或 ≥1 秒；id 缺省 q1..qn 且不得重复。
pub fn normalize_questions(args: &Value) -> Result<(Vec<NormalizedQuestion>, Option<i64>), String> {
    let questions = args
        .get("questions")
        .and_then(|q| q.as_array())
        .ok_or_else(|| "'questions' must be an array of 1-4 questions".to_owned())?;
    if questions.is_empty() || questions.len() > 4 {
        return Err(format!(
            "'questions' must contain 1-4 items (got {}); prefer <= 3",
            questions.len()
        ));
    }

    let mut normalized = Vec::with_capacity(questions.len());
    let mut seen_ids: HashSet<String> = HashSet::new();
    for (idx, raw) in questions.iter().enumerate() {
        let question = raw
            .get("question")
            .and_then(|q| q.as_str())
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| format!("questions[{idx}].question must be a non-empty string"))?
            .to_owned();

        let id = raw
            .get("id")
            .and_then(|i| i.as_str())
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("q{}", idx + 1));
        if !seen_ids.insert(id.clone()) {
            return Err(format!("duplicate question id '{id}'"));
        }

        // header 缺省 = 问句截 12 字符（CJK 按字符安全截）。
        let header = raw
            .get("header")
            .and_then(|h| h.as_str())
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(|h| h.chars().take(12).collect::<String>())
            .unwrap_or_else(|| question.chars().take(12).collect::<String>());

        let kind = match raw.get("type").and_then(|t| t.as_str()) {
            None | Some("choice") => QuestionKind::Choice,
            Some("text") => QuestionKind::Text,
            Some("confirm") => QuestionKind::Confirm,
            Some("secret") => QuestionKind::Secret,
            Some(other) => {
                return Err(format!(
                    "questions[{idx}].type '{other}' is invalid (choice|text|confirm|secret)"
                ))
            }
        };

        let mut options = Vec::new();
        if matches!(kind, QuestionKind::Choice | QuestionKind::Confirm) {
            let raw_options = raw
                .get("options")
                .and_then(|o| o.as_array())
                .ok_or_else(|| {
                    format!("questions[{idx}].options is required for {kind:?} (2-4 options)")
                })?;
            if raw_options.len() < 2 || raw_options.len() > 4 {
                return Err(format!(
                    "questions[{idx}].options must contain 2-4 items (got {}); do NOT add an \
                     'Other' option — the UI always appends a free-text input",
                    raw_options.len()
                ));
            }
            for (oi, opt) in raw_options.iter().enumerate() {
                let label = opt
                    .get("label")
                    .and_then(|l| l.as_str())
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .ok_or_else(|| {
                        format!("questions[{idx}].options[{oi}].label must be non-empty")
                    })?
                    .to_owned();
                let description = opt
                    .get("description")
                    .and_then(|d| d.as_str())
                    .map(str::to_owned);
                let recommended = opt
                    .get("recommended")
                    .and_then(|r| r.as_bool())
                    .unwrap_or(false);
                options.push((label, description, recommended));
            }
        }

        let storage = if kind == QuestionKind::Secret {
            let raw_storage = raw
                .get("storage")
                .ok_or_else(|| format!("questions[{idx}].storage is required for secret"))?;
            let parsed = storage_from_envelope(raw_storage).ok_or_else(|| {
                format!(
                    "questions[{idx}].storage must be {{kind:'env',name}} or {{kind:'file',path}}"
                )
            })?;
            Some(parsed)
        } else {
            None // 非 secret 上的 storage 容忍忽略（无害冗余）
        };

        normalized.push(NormalizedQuestion {
            id,
            header,
            question,
            kind,
            options,
            multi_select: raw
                .get("multi_select")
                .and_then(|m| m.as_bool())
                .unwrap_or(false),
            placeholder: raw
                .get("placeholder")
                .and_then(|p| p.as_str())
                .map(str::to_owned),
            optional: raw
                .get("optional")
                .and_then(|o| o.as_bool())
                .unwrap_or(false),
            storage,
        });
    }

    let timeout_override = match args.get("timeout") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_i64() {
            Some(-1) => Some(-1),
            Some(n) if n >= 1 => Some(n),
            Some(0) => {
                return Err(
                    "'timeout' must be -1 (wait forever) or a positive number of seconds".to_owned(),
                )
            }
            Some(_) => {
                return Err(
                    "'timeout' must be -1 (wait forever) or a positive number of seconds".to_owned(),
                )
            }
            None => return Err("'timeout' must be an integer (-1 or seconds)".to_owned()),
        },
    };

    Ok((normalized, timeout_override))
}

/// 生效超时：入参覆盖 > `[ask_user] default_timeout`。返回
/// `(下发信封用的秒数表示, router 等待时长)`。配置 0 秒按 1 秒容忍
/// （`AskTimeout::Seconds` 文档：0 由消费端容忍）。
fn effective_timeout(override_secs: Option<i64>, config: &AskUserConfig) -> (i64, Duration) {
    match override_secs {
        Some(-1) => (-1, NEVER_TIMEOUT),
        Some(n) if n >= 1 => (n, Duration::from_secs(n as u64)),
        _ => match config.default_timeout {
            AskTimeout::Never => (-1, NEVER_TIMEOUT),
            AskTimeout::Seconds(s) => (s.max(1) as i64, Duration::from_secs(s.max(1))),
        },
    }
}

/// 组装 host call 下发信封（Android 契约：
/// `{"v":1,"timeout":-1|N,"questions":[…]}`；payload 版本化，UI 对未知
/// 字段向前兼容——设计 §3 工程要点 6）。
fn host_envelope(questions: &[NormalizedQuestion], timeout_secs: i64) -> Value {
    json!({
        "v": 1,
        "timeout": timeout_secs,
        "questions": questions.iter().map(NormalizedQuestion::to_envelope).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// 结果格式化（即时/迟到共用 —— 迟到等价不变量的落点）
// ---------------------------------------------------------------------------

/// timeout 回填（设计 §2）。
pub fn timeout_result_content(secs: i64) -> String {
    format!(
        "用户在 {secs} 秒内未回应，可能不在。请自行判断继续。\n{{\"outcome\":\"timeout\"}}"
    )
}

/// 整卡 decline 回填（设计 §2 逐字）。
pub fn declined_result_content() -> String {
    format!("{DECLINED_TEXT}\n{{\"outcome\":\"declined\"}}")
}

/// 从 resolve payload（ok=true 的业务形态）取指定题的答案条目。
fn answer_item<'a>(payload: &'a Value, id: &str) -> Option<&'a Value> {
    payload
        .get("answers")
        .and_then(|a| a.as_array())?
        .iter()
        .find(|item| item.get("id").and_then(|i| i.as_str()) == Some(id))
}

/// 单题自然语言行（设计 §2 输出示例）。
fn answer_line(question: &NormalizedQuestion, payload: &Value) -> String {
    let Some(item) = answer_item(payload, &question.id) else {
        return "(no answer)".to_owned();
    };
    if item.get("skipped").and_then(|s| s.as_bool()) == Some(true) {
        return "(skipped)".to_owned();
    }
    if item.get("declined").and_then(|d| d.as_bool()) == Some(true) {
        return "(declined)".to_owned();
    }
    if let Some(selected) = item.get("selected").and_then(|s| s.as_array()) {
        let labels: Vec<String> = selected
            .iter()
            .filter_map(|v| v.as_str())
            .map(|label| {
                // 命中推荐项时复述推荐标记（与模型侧约定一致）。
                let recommended = question
                    .options
                    .iter()
                    .any(|(l, _, r)| *r && l == label);
                if recommended {
                    format!("{label} (Recommended)")
                } else {
                    label.to_owned()
                }
            })
            .collect();
        if labels.is_empty() {
            return "(no answer)".to_owned();
        }
        return labels.join(", ");
    }
    if let Some(value) = item.get("value").and_then(|v| v.as_str()) {
        if question.kind == QuestionKind::Secret {
            // 明文绝不进上下文——只回填引用（设计 §0/r3）。
            let remember = item
                .get("remember")
                .and_then(|r| r.as_bool())
                .unwrap_or(false);
            let scope = if remember {
                "已记住，后续会话可用"
            } else {
                "本会话生效"
            };
            return match &question.storage {
                Some(storage) => format!("已存入凭据：{}（{scope}）", storage.display()),
                None => "已存入凭据（本会话生效）".to_owned(),
            };
        }
        // text 答案：自然语言行截断（结构化 JSON 保留全文）。
        let capped: String = value.chars().take(500).collect();
        return if value.chars().count() > 500 {
            format!("{capped}…")
        } else {
            capped
        };
    }
    "(no answer)".to_owned()
}

/// 结构化 JSON 里的单题条目（secret 换成引用；未知形状丢弃）。
fn answer_json_item(question: &NormalizedQuestion, payload: &Value) -> Option<Value> {
    let item = answer_item(payload, &question.id)?;
    if item.get("skipped").and_then(|s| s.as_bool()) == Some(true) {
        return Some(json!({ "id": question.id, "skipped": true }));
    }
    if item.get("declined").and_then(|d| d.as_bool()) == Some(true) {
        return Some(json!({ "id": question.id, "declined": true }));
    }
    if let Some(selected) = item.get("selected").and_then(|s| s.as_array()) {
        let labels: Vec<&str> = selected.iter().filter_map(|v| v.as_str()).collect();
        if labels.is_empty() {
            return None;
        }
        return Some(json!({ "id": question.id, "selected": labels }));
    }
    if let Some(value) = item.get("value").and_then(|v| v.as_str()) {
        if question.kind == QuestionKind::Secret {
            let reference = question
                .storage
                .as_ref()
                .map(|s| s.reference())
                .unwrap_or_else(|| "credential".to_owned());
            return Some(json!({ "id": question.id, "secret": reference }));
        }
        return Some(json!({ "id": question.id, "value": value }));
    }
    None
}

/// 回填模型的结果内容（自然语言 + 结构化并存，仿 opencode；设计 §2）。
///
/// **迟到等价不变量**：即时路径（Tool::execute）与迟到路径
/// （QuestionHandler → resume_from_tool_result）共用本函数，对模型完全
/// 同形。secret 明文经 [`apply_secret_answers`] 先行入库，本函数只见引用。
pub fn format_question_result(questions: &[NormalizedQuestion], payload: &Value) -> String {
    // 整卡 decline：payload 无 answers（或显式 outcome=declined）。
    let outcome_declined = payload
        .get("outcome")
        .and_then(|o| o.as_str())
        .map(|o| o == "declined")
        .unwrap_or(false)
        || payload
            .get("answers")
            .and_then(|a| a.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true);
    if outcome_declined {
        return declined_result_content();
    }

    let mut lines = vec!["User answered your questions:".to_owned()];
    let mut answers_json = Vec::new();
    for question in questions {
        lines.push(format!("- \"{}\" → {}", question.id, answer_line(question, payload)));
        if let Some(item) = answer_json_item(question, payload) {
            answers_json.push(item);
        }
    }
    let structured = json!({ "outcome": "answered", "answers": answers_json });
    format!("{}\n{}", lines.join("\n"), structured)
}

/// secret 答案入凭据表（明文只进 vault，不进日志/上下文/转写）。
/// 逐题扫描 payload，把 secret 题的 `value` 存入 requestId 域。
/// env/file 落地（写文件、remember 持久化、env 注入源可见）由 vault
/// 实现侧生效（见 [`crate::secrets`])。
async fn apply_secret_answers(
    questions: &[NormalizedQuestion],
    payload: &Value,
    vault: &dyn AskUserSecretVault,
    request_id: &str,
) {
    for question in questions {
        if question.kind != QuestionKind::Secret {
            continue;
        }
        let Some(item) = answer_item(payload, &question.id) else {
            continue;
        };
        let (Some(value), Some(storage)) = (
            item.get("value").and_then(|v| v.as_str()),
            question.storage.as_ref(),
        ) else {
            continue;
        };
        let remember = item
            .get("remember")
            .and_then(|r| r.as_bool())
            .unwrap_or(false);
        let key = match storage {
            QuestionStorage::Env { name } => name.clone(),
            QuestionStorage::File { path } => format!("file:{path}"),
        };
        vault.store(request_id, &key, value, remember).await;
    }
}

// ---------------------------------------------------------------------------
// secret 凭据库（trait 接缝；持久化实现见 crate::secrets）
// ---------------------------------------------------------------------------

/// ask_user 凭据存取接缝。生产装配用 [`crate::secrets::PersistentSecretVault`]
/// （会话内存 + store v4 `credentials` 持久层 + env 注入/脱敏值源）；
/// 测试与降级用 [`InMemorySecretVault`]。
///
/// 实现约定：**明文绝不进任何日志**（实现不得 derive Debug 输出值）。
#[async_trait]
pub trait AskUserSecretVault: Send + Sync {
    /// 存入一条凭据（requestId 域内；同名后写覆盖先写）。file 形态
    /// （`file:<path>` 键）由实现侧写出文件；`remember=true` 落持久层。
    async fn store(&self, request_id: &str, name: &str, value: &str, remember: bool);
    /// 按名取值（会话级视图；env 注入用）。最新写入优先，持久行兜底。
    fn lookup(&self, name: &str) -> Option<String>;
    /// 该名下最新凭据是否被用户勾选「记住」（持久层行存在亦为 true）。
    fn is_remembered(&self, name: &str) -> bool;
    /// env 形态凭据 → 键值对（shell/bash 执行环境注入源；spawn 时取）。
    fn env_pairs(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// 全部已知密钥值（env + file；输出脱敏的值源）。
    fn known_secret_values(&self) -> Vec<String> {
        Vec::new()
    }
    /// 条目数（诊断/测试用；不含明文）。
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct StoredSecret {
    request_id: String,
    name: String,
    value: String,
    remember: bool,
}

/// 内存版凭据表（进程存活期有效；测试与降级实现）。
pub struct InMemorySecretVault {
    entries: Mutex<Vec<StoredSecret>>,
}

/// 手写 Debug：绝不泄露明文。
impl std::fmt::Debug for InMemorySecretVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InMemorySecretVault({} entries)", self.len())
    }
}

impl Default for InMemorySecretVault {
    fn default() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }
}

impl InMemorySecretVault {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl AskUserSecretVault for InMemorySecretVault {
    async fn store(&self, request_id: &str, name: &str, value: &str, remember: bool) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|e| !(e.name == name && e.request_id == request_id));
        entries.push(StoredSecret {
            request_id: request_id.to_owned(),
            name: name.to_owned(),
            value: value.to_owned(),
            remember,
        });
    }

    fn lookup(&self, name: &str) -> Option<String> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .iter()
            .rev()
            .find(|e| e.name == name)
            .map(|e| e.value.clone())
    }

    fn is_remembered(&self, name: &str) -> bool {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .iter()
            .rev()
            .find(|e| e.name == name)
            .map(|e| e.remember)
            .unwrap_or(false)
    }

    fn env_pairs(&self) -> Vec<(String, String)> {
        // file: 前缀键是文件形态，不入执行环境。
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let mut pairs: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for e in entries.iter() {
            if !e.name.starts_with("file:") {
                pairs.insert(e.name.clone(), e.value.clone());
            }
        }
        pairs.into_iter().collect()
    }

    fn known_secret_values(&self) -> Vec<String> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let mut values = Vec::new();
        for e in entries.iter() {
            if !values.contains(&e.value) {
                values.push(e.value.clone());
            }
        }
        values
    }

    fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

// ---------------------------------------------------------------------------
// 共享上下文（工具本体 / handler / 快照回填共用）
// ---------------------------------------------------------------------------

struct WireParts {
    state: Weak<AppState>,
    store: Option<SqliteStore>,
    config: AskUserConfig,
}

/// ask_user 共享上下文。工具在 `build_app_context_with` **之前**构造
/// （AppState 尚不存在），装配完成后由 `create_with` 调 [`Self::wire`]
/// 注入 `Weak<AppState>` + store + `[ask_user]` 配置（question.rs 模块
/// 文档的防引用环建议）。
pub struct AskUserContext {
    router: Arc<HostCallRouter>,
    vault: Arc<dyn AskUserSecretVault>,
    wired: Mutex<Option<WireParts>>,
}

impl AskUserContext {
    /// vault 由装配方传入（生产 = [`crate::secrets::PersistentSecretVault`]；
    /// 测试可用内存版）。工具在 `build_app_context_with` **之前**构造
    /// （AppState 尚不存在），装配完成后由 `create_with` 调 [`Self::wire`]
    /// 注入 `Weak<AppState>` + store + `[ask_user]` 配置（question.rs 模块
    /// 文档的防引用环建议）。
    pub fn new(router: Arc<HostCallRouter>, vault: Arc<dyn AskUserSecretVault>) -> Arc<Self> {
        Arc::new(Self {
            router,
            vault,
            wired: Mutex::new(None),
        })
    }

    /// 装配后接线：从 AppState 读取 store 与 `[ask_user]` 配置快照。
    pub fn wire(&self, state: &Arc<AppState>) {
        let (store, config) = {
            let inner = state.core.lock();
            (inner.store.clone(), inner.config.ask_user.clone())
        };
        *self.wired.lock().unwrap_or_else(|e| e.into_inner()) = Some(WireParts {
            state: Arc::downgrade(state),
            store,
            config,
        });
    }

    fn parts(&self) -> Option<(Weak<AppState>, Option<SqliteStore>, AskUserConfig)> {
        let guard = self.wired.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|w| (w.state.clone(), w.store.clone(), w.config.clone()))
    }

    fn state(&self) -> Option<Arc<AppState>> {
        self.parts().and_then(|(state, _, _)| state.upgrade())
    }

    fn store(&self) -> Option<SqliteStore> {
        self.parts().and_then(|(_, store, _)| store)
    }

    fn ask_config(&self) -> AskUserConfig {
        self.parts().map(|(_, _, c)| c).unwrap_or_default()
    }

    pub fn vault(&self) -> Arc<dyn AskUserSecretVault> {
        self.vault.clone()
    }

    /// `[ask_user] mask_secrets` 当前快照（脱敏闭包开关；默认 true）。
    pub fn mask_enabled(&self) -> bool {
        self.ask_config().mask_secrets
    }

    /// 用 vault 已知密钥值对文本打码（core `set_secret_masker` 注入的
    /// 闭包体；`mask_secrets=false` 时调用方短路返回原文）。
    pub fn mask(&self, text: &str) -> String {
        crate::secrets::mask_with_known_values(&self.vault.known_secret_values(), text)
    }

    /// args 宽松等价（精确比较的兜底）：结构相等，或仅**数值表示形式**
    /// 不同——模型 args 经 provider / 持久化 round-trip 后可能出现
    /// `120` 与 `120.0` 这类 int/float 漂移，逐层递归比较（键序无关）。
    /// 让更多悬挂声明能按 tool_call_id 精确对上，减少「无行可恢复」
    /// （args 不匹配且非唯一悬挂时 find_call_id 只能放弃）的路径。
    fn args_equivalent(a: &Value, b: &Value) -> bool {
        if a == b {
            return true;
        }
        match (a, b) {
            (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            },
            (Value::Object(x), Value::Object(y)) => {
                x.len() == y.len()
                    && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| Self::args_equivalent(v, w)))
            }
            (Value::Array(x), Value::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(v, w)| Self::args_equivalent(v, w))
            }
            _ => false,
        }
    }

    /// 当前悬挂 ask_user 调用的定位：读**当前 run 的持久化消息**（runner
    /// 在执行工具前已把带 `tool_calls` 的 assistant 消息落盘——
    /// runtime.rs「assistant 消息先于任何工具运行进持久存储」契约）。
    /// 不能用 `inner.history`：回合进行中它是回合前的旧副本（回合结束
    /// 才整体替换），看不到正在执行的这次调用。
    ///
    /// 精确匹配入参（并行批次里同参调用的区分度），唯一悬挂时兜底取之。
    /// 找不到（PTC 沙箱内 bridge 调用 / 未接线 / 无 run）返回 None →
    /// 本问卷不持久化（重启恢复无从补全，仅内存通路）。
    async fn find_call_id(&self, args: &Value) -> Option<String> {
        let state = self.state()?;
        let store = self.store()?;
        let run_id = {
            let inner = state.core.lock();
            inner.session_run.as_ref()?.run_id.clone()
        };
        let messages =
            openslate_store_sqlite::recorder::RunRecorder::load_messages_keep_dangling(
                &store, &run_id.0,
            )
            .await
            .ok()?;
        let ask_dangling: Vec<String> = dangling_tool_calls(&messages)
            .into_iter()
            .filter(|(_, name)| name == ASK_USER_TOOL)
            .map(|(id, _)| id)
            .collect();
        if ask_dangling.is_empty() {
            return None;
        }
        for msg in &messages {
            let Some(tool_calls) = &msg.tool_calls else { continue };
            for tc in tool_calls {
                if tc.name == ASK_USER_TOOL
                    && ask_dangling.contains(&tc.id.0)
                    && Self::args_equivalent(&tc.arguments, args)
                {
                    return Some(tc.id.0.clone());
                }
            }
        }
        // 唯一悬挂 ask_user 且无精确匹配（模型 args 经手差异）→ 取之。
        if ask_dangling.len() == 1 {
            return Some(ask_dangling[0].clone());
        }
        None
    }

    /// 问卷行落库（发信封**之前**——崩溃窗口内 UI 已见问卷则行必在）。
    async fn persist_pending(&self, request_id: &str, tool_call_id: &str, envelope_questions: &[Value]) {
        let Some(store) = self.store() else { return };
        let questions_json = serde_json::to_string(envelope_questions).unwrap_or_default();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if let Err(e) = store
            .insert_pending_question(request_id, tool_call_id, &questions_json, QUESTION_VERSION, now_ms)
            .await
        {
            // 持久化失败不阻断提问（内存通路仍可用），仅告警。
            crate::alog!("ask_user: persist question row failed (non-fatal): {e}");
        }
    }

    /// 关卷（best-effort；状态机迁移失败仅告警——迟到答案会被历史
    /// AlreadyAnswered 挡住，不会双补全）。
    async fn close_row(&self, request_id: &str, to: &str) {
        let Some(store) = self.store() else { return };
        if let Err(e) = store
            .transition_pending_question(request_id, "pending", to)
            .await
        {
            crate::alog!("ask_user: close question row {request_id} → {to} failed (non-fatal): {e}");
        }
    }

    /// 快照回填：当前会话 history 中仍悬挂的 pending 问卷 → DTO；同时
    /// 把「历史里已有结果」的行关卷（superseded——竞态自愈：工具结果
    /// 落地与行更新之间崩溃的窗口）。属于其他会话的行（tool_call_id
    /// 不在当前 history）不动不显示。
    pub async fn snapshot_dtos(&self) -> Vec<PendingQuestionDto> {
        let mut dtos = Vec::new();
        let mut closed: Vec<String> = Vec::new();
        let Some((state, store)) =
            self.parts()
                .and_then(|(s, st, _)| s.upgrade().map(|state| (state, st)))
        else {
            return dtos;
        };
        let Some(store) = store else { return dtos };
        let Ok(rows) = store.list_pending_questions().await else {
            return dtos;
        };
        if rows.is_empty() {
            return dtos;
        }
        let (resolved, declared_ask): (HashSet<String>, HashSet<String>) = {
            let inner = state.core.lock();
            let mut resolved = HashSet::new();
            let mut declared = HashSet::new();
            for msg in &inner.history {
                match msg.role {
                    MessageRole::Tool => {
                        if let Some(id) = &msg.tool_call_id {
                            resolved.insert(id.0.clone());
                        }
                    }
                    MessageRole::Assistant => {
                        if let Some(tcs) = &msg.tool_calls {
                            for tc in tcs {
                                if tc.name == ASK_USER_TOOL {
                                    declared.insert(tc.id.0.clone());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            (resolved, declared)
        };
        for row in rows {
            if resolved.contains(&row.tool_call_id) {
                closed.push(row.request_id.clone());
            } else if declared_ask.contains(&row.tool_call_id) {
                if let Ok(questions) = serde_json::from_str::<Value>(&row.questions_json) {
                    dtos.push(PendingQuestionDto {
                        request_id: row.request_id,
                        tool_call_id: row.tool_call_id,
                        questions,
                        status: QuestionStatus::Pending,
                        version: row.version.max(0) as u32,
                    });
                }
            }
        }
        for id in &closed {
            self.close_row(id, "superseded").await;
        }
        dtos
    }
}

// ---------------------------------------------------------------------------
// 工具本体
// ---------------------------------------------------------------------------

/// ask_user 工具（宿主直连 `Tool`；`HostTool` 同款模式但通道语义自定义：
/// 自定义超时、问卷持久化、secret 凭据入库、业务化回填）。
pub struct AskUserTool {
    ctx: Arc<AskUserContext>,
}

impl AskUserTool {
    pub fn new(ctx: Arc<AskUserContext>) -> Self {
        Self { ctx }
    }
}

/// 工具 description（设计 §2 文案要点；教模型用好）。
const ASK_USER_DESCRIPTION: &str = "Ask the human user questions and wait for their answers. \
Use ONLY when you are truly blocked and cannot proceed on a reasonable assumption — if you can \
assume, assume and state the assumption in your reply instead of asking. Rules: \
(1) Put your recommended option FIRST, suffix its label with \" (Recommended)\" and set \
recommended=true. \
(2) NEVER add an \"Other\"/\"都不是\" option — the UI always appends a free-text input for that. \
(3) Batch closely-related questions into ONE call (1-4 questions, prefer <=3); do not pad \
unrelated ones. \
(4) Use \"confirm\" for yes/no decisions, \"text\" for names/URLs/paths, \"secret\" for \
credentials/keys (declare storage: env name or file path), \"choice\" otherwise. \
(5) Option labels are 1-5 words; descriptions explain the trade-offs. \
(6) The user may decline, skip, or not respond — you will be told; proceed with your own \
judgment and state the assumptions you adopted.";

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        ASK_USER_TOOL
    }

    fn description(&self) -> &str {
        ASK_USER_DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "snake_case answer key; defaults to q1..qn" },
                            "header": { "type": "string", "description": "short card label, <=12 chars; defaults to a truncated question" },
                            "question": { "type": "string", "description": "the full question sentence" },
                            "type": { "type": "string", "enum": ["choice", "text", "confirm", "secret"], "description": "question type; defaults to choice" },
                            "options": {
                                "type": "array", "minItems": 2, "maxItems": 4,
                                "description": "required for choice/confirm; no Other option (UI appends free-text)",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string", "description": "1-5 words" },
                                        "description": { "type": "string", "description": "trade-offs of this option" },
                                        "recommended": { "type": "boolean" }
                                    },
                                    "required": ["label"],
                                    "additionalProperties": false
                                }
                            },
                            "multi_select": { "type": "boolean", "description": "choice only" },
                            "placeholder": { "type": "string", "description": "input hint for text/secret/free-text" },
                            "optional": { "type": "boolean", "description": "true allows the user to skip this question" },
                            "storage": {
                                "type": "object",
                                "description": "secret only, required: where the credential takes effect",
                                "properties": {
                                    "kind": { "type": "string", "enum": ["env", "file"] },
                                    "name": { "type": "string", "description": "env var name, e.g. GITHUB_TOKEN" },
                                    "path": { "type": "string", "description": "file path, e.g. ~/.netrc" }
                                },
                                "required": ["kind"],
                                "additionalProperties": false
                            }
                        },
                        "required": ["question"],
                        "additionalProperties": false
                    }
                },
                "timeout": { "type": "integer", "description": "seconds to wait; -1 = wait forever; omit for the user's configured default" }
            },
            "required": ["questions"],
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

        let (questions, timeout_override) = match normalize_questions(args) {
            Ok(v) => v,
            Err(msg) => return Ok(output(format!("Invalid ask_user arguments: {msg}"), ToolOutputStatus::Error)),
        };
        let (timeout_secs, timeout_dur) = effective_timeout(timeout_override, &self.ctx.ask_config());
        let envelope = host_envelope(&questions, timeout_secs);
        let envelope_questions = envelope
            .get("questions")
            .cloned()
            .and_then(|q| q.as_array().cloned())
            .unwrap_or_default();

        // 定位本次调用的 tool_call_id（当前 run 持久化消息里悬挂的
        // ask_user 声明）；找不到（PTC 沙箱 bridge / 未接线）→ 不持久
        // 化，仅内存通路。
        let tool_call_id = self.ctx.find_call_id(args).await;

        // 幂等主键 = host call id（requestId 与 host call id 对齐）。
        let id = self.ctx.router.allocate_id();
        let request_id = id.to_string();
        if let Some(call_id) = &tool_call_id {
            self.ctx
                .persist_pending(&request_id, call_id, &envelope_questions)
                .await;
        }

        match self
            .ctx
            .router
            .call_with_id(id, ASK_USER_TOOL, &envelope, timeout_dur)
            .await
        {
            Ok(payload) => {
                apply_secret_answers(&questions, &payload, self.ctx.vault.as_ref(), &request_id).await;
                let content = format_question_result(&questions, &payload);
                let outcome = payload.get("outcome").and_then(|o| o.as_str());
                let status = if outcome == Some("declined") { "declined" } else { "answered" };
                self.ctx.close_row(&request_id, status).await;
                Ok(output(content, ToolOutputStatus::Success))
            }
            Err(message) if message.starts_with(HOST_CALL_TIMEOUT_ERR_PREFIX) => {
                // 超时是业务结果（ok=true 语义）：模型收到「用户可能不在，
                // 自行判断继续」（设计 §3 工程要点 3）。行按 expired 关卷
                // （时间性关闭；迟到答案会被历史 AlreadyAnswered 挡住）。
                self.ctx.close_row(&request_id, "expired").await;
                Ok(output(timeout_result_content(timeout_secs), ToolOutputStatus::Success))
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
// QuestionHandler（迟到答案 / decline / 取消）
// ---------------------------------------------------------------------------

fn reply_payload(action: QuestionReplyAction, answers: &Value) -> String {
    match action {
        QuestionReplyAction::Accept => json!({ "outcome": "answered", "answers": answers }),
        QuestionReplyAction::Decline => json!({ "outcome": "declined" }),
    }
    .to_string()
}

fn status_for(action: QuestionReplyAction) -> &'static str {
    match action {
        QuestionReplyAction::Accept => "answered",
        QuestionReplyAction::Decline => "declined",
    }
}

/// `ClientMsg::question_reply` 的 mobile 实现。
///
/// 双通路（设计 §3）：
/// 1. **在途 host call**（工具正等待）：`request_id` 即 host call id，
///    `resolve` 唤醒工具——工具按即时路径格式化回填（等价不变量天然成立）。
/// 2. **迟到答案**（重启恢复后的 pending 问卷）：原子关卷（首答者赢）→
///    以与即时路径**完全一致**的内容补全原 tool result →
///    `RunManager::resume_from_tool_result` 从该点续跑 agent 循环。
pub struct MobileQuestionHandler {
    ctx: Arc<AskUserContext>,
}

impl MobileQuestionHandler {
    pub fn new(ctx: Arc<AskUserContext>) -> Self {
        Self { ctx }
    }

    /// 迟到答案路径（同步 trait 内做异步事：block_in_place 让出 worker，
    /// 内层 Handle::block_on 驱动——mobile runtime 是 multi_thread，模式
    /// 与审批桥同款安全性论证）。
    fn late_reply(&self, request_id: &str, action: QuestionReplyAction, answers: &Value) -> bool {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                self.late_reply_async(request_id, action, answers).await
            })
        })
    }

    async fn late_reply_async(
        &self,
        request_id: &str,
        action: QuestionReplyAction,
        answers: &Value,
    ) -> bool {
        let Some(store) = self.ctx.store() else {
            return false;
        };
        let Some(state) = self.ctx.state() else {
            return false;
        };
        // 前置校验（真正的首答者赢在下方原子迁移）。
        let Ok(Some(row)) = store.get_pending_question(request_id).await else {
            return false;
        };
        if row.status != "pending" || row.tool_call_id.is_empty() {
            return false;
        }
        // 悬挂前置校验：tool call 已在历史里被解决（submit 的 superseded
        // 修复抢跑 / 结果已落地）→ 关卷拒绝，避免 resume 撞
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
                .transition_pending_question(request_id, "pending", "superseded")
                .await;
            return false;
        }
        let Some(envelope_questions) = serde_json::from_str::<Value>(&row.questions_json)
            .ok()
            .and_then(|v| v.as_array().cloned())
        else {
            return false;
        };
        let questions: Vec<NormalizedQuestion> = envelope_questions
            .iter()
            .filter_map(NormalizedQuestion::from_envelope)
            .collect();
        if questions.len() != envelope_questions.len() {
            // 持久化负载损坏（不应发生）——关卷拒绝。
            let _ = store
                .transition_pending_question(request_id, "pending", "superseded")
                .await;
            return false;
        }

        let payload = match action {
            QuestionReplyAction::Accept => json!({ "outcome": "answered", "answers": answers }),
            QuestionReplyAction::Decline => json!({ "outcome": "declined" }),
        };
        apply_secret_answers(&questions, &payload, self.ctx.vault.as_ref(), request_id).await;
        // 与即时路径同一格式化函数（迟到等价不变量）。
        let content = format_question_result(&questions, &payload);
        let new_status = status_for(action);

        // 引擎快照 + busy 守卫 + running 置位（同一锁内完成，防竞态）。
        let snapshot = {
            let mut inner = state.core.lock();
            if inner.busy() {
                None
            } else {
                // 先取 run 的可克隆字段（recorder Arc / run_id），再 take
                // manager——避免 immutable/mutable 借用交叠。
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
                        manager.message_sink = Some(recorder as Arc<dyn MessageSink>);
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

        // 原子关卷（首答者赢）。输掉竞态 → 回收引擎占用。
        let won = matches!(
            store
                .transition_pending_question(request_id, "pending", new_status)
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
                    .transition_pending_question(request_id, new_status, "pending")
                    .await;
                state.sink.broadcast(ServerMsg::Notice {
                    text: format!("续跑构建 provider 失败：{e}"),
                    level: openslate_protocol::NoticeLevel::Error,
                });
                return false;
            }
        };

        let handle = spawn_resume_turn(
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
        crate::alog!("ask_user: late reply {request_id} resumed run from tool result");
        true
    }
}

/// 启动失败后的引擎占用回收（与 engine_turn 收尾同款字段复位；history
/// 不动——失败保留现状语义）。
fn recycle_after_failed_start(state: &Arc<AppState>, mut manager: RunManager) {
    let mut inner = state.core.lock();
    inner.running = false;
    inner.cancel = None;
    manager.agent_tree = inner.agent_tree.clone();
    inner.manager = Some(manager);
}

impl QuestionHandler for MobileQuestionHandler {
    fn on_reply(
        &self,
        request_id: &str,
        action: QuestionReplyAction,
        answers: Value,
    ) -> bool {
        // 通路 1：在途 host call（requestId = host call id）。resolve 无害
        // no-op 保证：非在途 id 返回 false，落到迟到路径。
        if let Ok(id) = request_id.parse::<u64>() {
            let payload = reply_payload(action, &answers);
            if self.ctx.router.resolve(id, true, payload) {
                // 行状态 best-effort（后台关卷，不阻塞应答者）。
                let ctx = self.ctx.clone();
                let request_id = request_id.to_owned();
                let status = status_for(action).to_owned();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        ctx.close_row(&request_id, &status).await;
                    });
                }
                return true;
            }
        }
        self.late_reply(request_id, action, &answers)
    }

    fn on_cancel_all(&self) {
        // 用户显式取消回合：悬挂问卷按 superseded 关卷（cancelled 不在
        // 状态枚举——问卷随回合取消通道折叠，见 QuestionStatus 文档）。
        // 引擎 select! 对已取消 token 有偏置，会为整批悬挂 tool call 合成
        // cancelled 结果，历史侧自洽；这里只收持久层行。
        //
        // 有进行中回合时直接关**全部** pending 行：回合中的
        // `inner.history` 是旧副本（看不到本轮正在等待的调用），按悬挂
        // 过滤会漏关当前问卷；而 busy 守卫下任何 pending 行此刻都不可能
        // 再被迟到答案消费，全关是安全的。无回合（重启恢复的待答问卷
        // 场景）则 no-op——不误杀。
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
                let Ok(rows) = store.list_pending_questions().await else {
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
// 迟到答案续跑（engine::engine_turn 的 resume 变体）
// ---------------------------------------------------------------------------

/// 起「从迟到 tool result 续跑」的回合任务（engine.rs `spawn_engine_turn`
/// 的同构变体：跑在 spawn_blocking 专用线程，进度回调走
/// SessionProgressBridge，收尾自回收）。**同款 panic 防线**（exp-10）：
/// 回合体包 catch_unwind，panic 拦截后走 engine 的 [`finalize_turn`]
/// 强制收尾 + `TurnError(kind=other)`——续跑路径同样不能让 running
/// 卡 true（engine.rs spawn_engine_turn 文档详述动机）。
#[allow(clippy::too_many_arguments)]
fn spawn_resume_turn(
    state: Arc<AppState>,
    manager: RunManager,
    run_id: RunId,
    provider: Box<dyn ModelProvider>,
    history: Vec<Message>,
    tool_call_id: String,
    result_content: String,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        // manager 以 &mut 借用进被保护闭包：panic 展开只作废引用、
        // 不销毁值——拦截后仍能送回 core。
        let mut manager = manager;
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.block_on(resume_turn(
                state.clone(),
                &mut manager,
                run_id,
                provider,
                history,
                tool_call_id,
                result_content,
                cancel,
            ))
        }));
        let terminal = match caught {
            Ok(terminal) => terminal,
            Err(payload) => {
                let summary = panic_payload_summary(payload.as_ref());
                crate::alog!(
                    "ask_user resume turn panicked ({summary}); forcing finalization"
                );
                TurnTerminal::Error {
                    message: format!("内部错误：回合发生 panic（已恢复，可重试）；{summary}"),
                    kind: Some(TurnErrorKind::Other),
                }
            }
        };
        finalize_turn(&state, manager, terminal);
    })
}

/// 与 `engine::engine_turn` 同款回合体（终态构造 + 交回
/// `engine::finalize_turn` 收尾）；执行体换成
/// `RunManager::resume_from_tool_result`（补全悬挂 tool result 后从该点
/// 重新进入 agent 循环——与正常工具结果后的继续同一条路径）。
#[allow(clippy::too_many_arguments)]
async fn resume_turn(
    state: Arc<AppState>,
    manager: &mut RunManager,
    run_id: RunId,
    provider: Box<dyn ModelProvider>,
    history: Vec<Message>,
    tool_call_id: String,
    result_content: String,
    cancel: CancellationToken,
) -> TurnTerminal {
    let started = Instant::now();
    let mut bridge = SessionProgressBridge {
        state: state.clone(),
    };
    let result = manager
        .resume_from_tool_result(
            run_id,
            provider.as_ref(),
            &history,
            &tool_call_id,
            &result_content,
            cancel,
            Some(&mut bridge),
        )
        .await;

    match result {
        Ok(r) => {
            let dto = {
                let mut inner = state.core.lock();
                inner.history = r.messages.clone();
                flush_stream(&mut inner);
                fold_tool_outcomes(&mut inner, &r.messages);
                inner.stats.turns += 1;
                inner.stats.total_input_tokens += r.total_input_tokens;
                inner.stats.total_output_tokens += r.total_output_tokens;
                inner.stats.total_cost_usd += r.total_cost_usd;
                TurnSummaryDto::from(r)
            };
            crate::alog!(
                "ask_user resume turn ok: {}s · ↑{} ↓{}",
                started.elapsed().as_secs(),
                dto.total_input_tokens,
                dto.total_output_tokens
            );
            TurnTerminal::Ok(Box::new(dto))
        }
        Err(e) => {
            // 失败保留 history 现状（engine_turn 同款）。问卷行已关卷——
            // 若失败源于 AlreadyAnswered（被 superseded 修复抢跑），行本
            // 就该关；其他失败路径行状态由快照自愈收口。
            {
                let mut inner = state.core.lock();
                flush_stream(&mut inner);
            }
            crate::alog!("ask_user resume turn error: {e}");
            TurnTerminal::Error {
                message: e.to_string(),
                kind: None,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 启动清扫（TTL 过期 → expired 合成；已解决行 → superseded 关卷）
// ---------------------------------------------------------------------------

/// 重启时的问卷持久层维护（在 assemble_state 恢复 history 之后调用）：
///
/// - **过期**（created_at 早于 now-TTL）且悬挂 → `append_late_tool_result`
///   合成 [`EXPIRED_RESULT`] 补全（消息经 recorder 落盘，与内存同序——
///   悬挂只会出现在尾部，append 即尾序）；行 → expired。声明不在本
///   history 的过期行无补全点，同样 expired 关卷（TTL 兜底语义）。
/// - **已解决**（恢复的 history 里**确有该 tool_call_id 的结果消息**——
///   崩溃发生在结果落地与行更新之间）→ 行 → superseded。
/// - **其余 pending 保持**：悬挂者可恢复（快照回放 + 迟到答案续跑）；
///   **结果与声明均不在本 history 者**（属于其它 run——如状态 failed
///   被恢复逻辑排除的 run、或提问后用户开过更新的会话）**同样保持
///   pending**：「本 history 查无此调用」不构成「已被取代」的证据——
///   误关会让该问卷永久丢失回放；迟到答案仍可经 question_reply 补全
///   （若其 run 恰被恢复），最终由 TTL 过期兜底关卷。
///
/// 返回仍可恢复（保持 pending 且当前 history 中悬挂）的 tool_call_id 集。
pub async fn startup_question_maintenance(
    store: &SqliteStore,
    history: &mut Vec<Message>,
    recorder: Option<&Arc<openslate_store_sqlite::recorder::RunRecorder>>,
) -> Vec<String> {
    let mut recoverable = Vec::new();
    let Ok(rows) = store.list_pending_questions().await else {
        return recoverable;
    };
    if rows.is_empty() {
        return recoverable;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut dangling: HashSet<String> = dangling_tool_calls(history)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    // 本 history 中**确有结果落地**的 tool_call_id（Tool 消息直接证据）。
    // 「已解决关卷」的可验证前提：结果就写在本 run 的 history 里；查无
    // 结果 = 行属于其它 run（本 history 没有它的补全点）→ 保持 pending
    // （见 fn 文档），不能因「不在 dangling 集」就推定被取代。
    let resolved_here: HashSet<String> = history
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .filter_map(|m| m.tool_call_id.as_ref().map(|id| id.0.clone()))
        .collect();

    for row in rows {
        let is_dangling = dangling.contains(&row.tool_call_id);
        let expired = now_ms.saturating_sub(row.created_at) >= QUESTION_TTL_MS;
        if expired {
            if is_dangling {
                match append_late_tool_result(&mut *history, &row.tool_call_id, EXPIRED_RESULT) {
                    Ok(msg) => {
                        if let Some(recorder) = recorder {
                            if let Err(e) = recorder.write_message(&msg).await {
                                crate::alog!(
                                    "ask_user startup: persist expired synth failed (non-fatal): {e}"
                                );
                            }
                        }
                        dangling.remove(&row.tool_call_id);
                    }
                    Err(e) => {
                        // 无声明处（负载/历史错位）：仅关卷。
                        crate::alog!("ask_user startup: expired synth skipped: {e}");
                    }
                }
            }
            let _ = store
                .transition_pending_question(&row.request_id, "pending", "expired")
                .await;
        } else if is_dangling {
            recoverable.push(row.tool_call_id.clone());
        } else if resolved_here.contains(&row.tool_call_id) {
            // 结果已在但行还 pending（崩溃窗口）→ 关卷。
            let _ = store
                .transition_pending_question(&row.request_id, "pending", "superseded")
                .await;
        } else {
            // 声明与结果均不在本 history（行属于其它 run / 会话）：保持
            // pending，不误关——迟到答案补全或 TTL 过期兜底（见 fn 文档）。
        }
    }
    recoverable
}

// ---------------------------------------------------------------------------
// 审批豁免（无副作用工具不触发审批）
// ---------------------------------------------------------------------------

/// 免审工具集（无副作用——提问 / 纯读本会话已脱敏落盘的数据）。
const APPROVAL_EXEMPT_TOOLS: &[&str] = &[ASK_USER_TOOL, crate::output_query::OUTPUT_QUERY_TOOL];

/// 审批回调装饰器：命中的无副作用工具直通放行，其余委托内层回调。
///
/// - `ask_user`：Manual 全审模式下同样免审——「问个问题还要先批准才能
///   问」是体验灾难（ask-user 设计 §0）；
/// - `output_query`（长输出落盘 C4）：纯读已脱敏落盘的输出，Auto 缺省
///   下启发式 Medium 会为每次查询付一次 AI 审查调用——读日志不该过审。
///
/// 豁免实现点选最小侵入处——装饰 `manager.approval` 的 callback 槽
/// （mobile 装配层），不动 core 审批逻辑、不动 `AppState.approval`
/// （respond 通道照旧）。
///
/// 内层泛化为 `Arc<dyn ApprovalCallback>`（tool-approval P1b）：mobile
/// 装配链 `SideEffectFreeApproval(AiReviewApproval(SessionApprovalBridge))`
/// ——审查装饰器包在桥外、豁免器再包审查器。
pub struct SideEffectFreeApproval {
    inner: Arc<dyn openslate_core::approval::ApprovalCallback>,
}

impl SideEffectFreeApproval {
    pub fn new(inner: Arc<dyn openslate_core::approval::ApprovalCallback>) -> Self {
        Self { inner }
    }
}

impl openslate_core::approval::ApprovalCallback for SideEffectFreeApproval {
    fn decide(
        &self,
        req: &openslate_core::approval::ApprovalRequest,
    ) -> openslate_core::approval::ApprovalDecision {
        if APPROVAL_EXEMPT_TOOLS.contains(&req.tool_name.as_str()) {
            return openslate_core::approval::ApprovalDecision::Approved;
        }
        self.inner.decide(req)
    }
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use openslate_core::types::{ToolCall, ToolCallId};

    // ── normalize_questions ──────────────────────────────────────────

    fn choice_args() -> Value {
        serde_json::json!({
            "questions": [
                {
                    "question": "本地存储用 SQLite 还是 sled？",
                    "type": "choice",
                    "options": [
                        { "label": "SQLite", "description": "零配置", "recommended": true },
                        { "label": "sled", "description": "纯 Rust" }
                    ]
                }
            ]
        })
    }

    #[test]
    fn normalize_applies_defaults() {
        let (qs, timeout) = normalize_questions(&choice_args()).expect("valid");
        assert_eq!(timeout, None);
        assert_eq!(qs.len(), 1);
        let q = &qs[0];
        assert_eq!(q.id, "q1", "id 缺省 q1..qn");
        assert_eq!(q.kind, QuestionKind::Choice);
        assert_eq!(q.header.chars().count(), 12, "header 缺省 = 问句截 12 字符");
        assert!(!q.multi_select);
        assert!(!q.optional);
        assert!(q.placeholder.is_none());
        assert_eq!(q.options.len(), 2);
        assert_eq!(q.options[0].0, "SQLite");
        assert!(q.options[0].2, "recommended 透传");
    }

    #[test]
    fn normalize_rejects_bad_shapes() {
        // 0 题 / 5 题。
        assert!(normalize_questions(&serde_json::json!({"questions": []})).is_err());
        let five = serde_json::json!({"questions": (0..5).map(|_| serde_json::json!({"question":"q"})).collect::<Vec<_>>()});
        assert!(normalize_questions(&five).is_err());
        // choice 缺 options / 1 个选项 / 5 个选项。
        assert!(normalize_questions(&serde_json::json!({"questions":[{"question":"x","type":"choice"}]})).is_err());
        let one = serde_json::json!({"questions":[{"question":"x","type":"confirm","options":[{"label":"a"}]}]});
        assert!(normalize_questions(&one).is_err());
        // secret 缺 storage。
        let secret = serde_json::json!({"questions":[{"question":"token?","type":"secret"}]});
        assert!(normalize_questions(&secret).is_err());
        // 非法 type。
        let bad_type = serde_json::json!({"questions":[{"question":"x","type":"radio"}]});
        assert!(normalize_questions(&bad_type).is_err());
        // 重复 id。
        let dup = serde_json::json!({"questions":[{"id":"a","question":"1"},{"id":"a","question":"2"}]});
        assert!(normalize_questions(&dup).is_err());
        // timeout=0 非法；-1 / >=1 合法（text 题免 options 要求）。
        let zero = serde_json::json!({"questions":[{"question":"x","type":"text"}],"timeout":0});
        assert!(normalize_questions(&zero).is_err());
        let never = serde_json::json!({"questions":[{"question":"x","type":"text"}],"timeout":-1});
        assert_eq!(normalize_questions(&never).unwrap().1, Some(-1));
        let secs = serde_json::json!({"questions":[{"question":"x","type":"text"}],"timeout":120});
        assert_eq!(normalize_questions(&secs).unwrap().1, Some(120));
    }

    #[test]
    fn normalize_secret_storage_shapes() {
        let env = serde_json::json!({"questions":[{"question":"token?","type":"secret","storage":{"kind":"env","name":"GITHUB_TOKEN"}}]});
        let (qs, _) = normalize_questions(&env).expect("env storage ok");
        assert_eq!(
            qs[0].storage,
            Some(QuestionStorage::Env { name: "GITHUB_TOKEN".into() })
        );
        let file = serde_json::json!({"questions":[{"question":"netrc?","type":"secret","storage":{"kind":"file","path":"~/.netrc"}}]});
        let (qs, _) = normalize_questions(&file).expect("file storage ok");
        assert_eq!(
            qs[0].storage,
            Some(QuestionStorage::File { path: "~/.netrc".into() })
        );
        // file 缺 path → 非法。
        let bad = serde_json::json!({"questions":[{"question":"x","type":"secret","storage":{"kind":"file"}}]});
        assert!(normalize_questions(&bad).is_err());
    }

    /// 下发信封逐字段对齐 Android 契约。
    #[test]
    fn host_envelope_matches_android_contract() {
        let (qs, _) = normalize_questions(&serde_json::json!({
            "timeout": 300,
            "questions": [
                { "question": "本地存储用哪个？", "type": "choice",
                  "options": [
                    {"label":"SQLite","recommended":true},
                    {"label":"sled"}
                  ] },
                { "id":"name", "question":"项目名？", "type":"text", "placeholder":"my-app" },
                { "id":"run_tests", "question":"跑测试吗？", "type":"confirm",
                  "options":[{"label":"是"},{"label":"否"}] },
                { "id":"tok", "question":"token？", "type":"secret",
                  "storage":{"kind":"env","name":"GITHUB_TOKEN"} }
            ]
        }))
        .expect("valid");
        let env = host_envelope(&qs, 300);
        assert_eq!(env["v"], 1, "payload 版本字段 v=1");
        assert_eq!(env["timeout"], 300);
        let arr = env["questions"].as_array().unwrap();
        assert_eq!(arr.len(), 4);

        let q1 = &arr[0];
        assert_eq!(q1["id"], "q1");
        assert_eq!(q1["type"], "choice");
        assert!(q1["header"].is_string(), "header 恒存在");
        assert!(q1["question"].is_string());
        assert_eq!(q1["multi_select"], false, "multi_select 恒存在（choice）");
        assert_eq!(q1["optional"], false);
        let opts = q1["options"].as_array().unwrap();
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0]["label"], "SQLite");
        assert_eq!(opts[0]["recommended"], true, "recommended 以布尔键存在");
        assert!(opts[1].get("recommended").is_none(), "非推荐项可省键");

        let q2 = &arr[1];
        assert_eq!(q2["type"], "text");
        assert_eq!(q2["placeholder"], "my-app");
        assert!(q2.get("options").is_none(), "text 无 options 键");

        let q3 = &arr[2];
        assert_eq!(q3["type"], "confirm");
        assert!(q3.get("multi_select").is_none(), "multi_select 仅 choice");
        assert_eq!(q3["options"].as_array().unwrap().len(), 2);

        let q4 = &arr[3];
        assert_eq!(q4["type"], "secret");
        assert_eq!(q4["storage"]["kind"], "env");
        assert_eq!(q4["storage"]["name"], "GITHUB_TOKEN");
        assert!(q4.get("options").is_none());
    }

    #[test]
    fn effective_timeout_matrix() {
        let never_cfg = AskUserConfig::default(); // default_timeout = Never
        assert_eq!(effective_timeout(None, &never_cfg).0, -1);
        assert_eq!(effective_timeout(Some(-1), &never_cfg).0, -1);
        assert_eq!(effective_timeout(Some(45), &never_cfg).0, 45);
        let secs_cfg = AskUserConfig {
            default_timeout: AskTimeout::Seconds(300),
            mask_secrets: true,
        };
        assert_eq!(effective_timeout(None, &secs_cfg).0, 300);
        // 入参覆盖配置。
        assert_eq!(effective_timeout(Some(-1), &secs_cfg).0, -1);
        // 配置 0 秒按 1 秒容忍。
        let zero_cfg = AskUserConfig {
            default_timeout: AskTimeout::Seconds(0),
            mask_secrets: true,
        };
        assert_eq!(effective_timeout(None, &zero_cfg).0, 1);
    }

    // ── format_question_result（即时/迟到共用 → 等价不变量落点）──────

    fn three_questions() -> Vec<NormalizedQuestion> {
        let (qs, _) = normalize_questions(&serde_json::json!({
            "questions": [
                { "id": "db", "question": "数据库？", "type": "choice",
                  "options": [
                    {"label":"SQLite","recommended":true},
                    {"label":"sled"}
                  ] },
                { "id": "scope", "question": "范围？", "type": "choice", "multi_select": true,
                  "options": [ {"label":"tests"},{"label":"docs"} ] },
                { "id": "note", "question": "备注？", "type": "text", "optional": true }
            ]
        }))
        .expect("valid");
        qs
    }

    #[test]
    fn format_answered_full_shape() {
        let qs = three_questions();
        let payload = serde_json::json!({
            "outcome": "answered",
            "answers": [
                { "id": "db", "selected": ["SQLite"] },
                { "id": "scope", "selected": ["tests", "docs"] },
                { "id": "note", "skipped": true }
            ]
        });
        let out = format_question_result(&qs, &payload);
        assert!(out.contains("User answered your questions:"));
        // 逐题行（推荐标记 + 多选拼接 + skipped）。
        assert!(out.contains("- \"db\" → SQLite (Recommended)"), "{out}");
        assert!(out.contains("- \"scope\" → tests, docs"), "{out}");
        assert!(out.contains("- \"note\" → (skipped)"), "{out}");
        // 结构化 JSON 并存。
        let json_part = out.lines().last().unwrap();
        let parsed: Value = serde_json::from_str(json_part).expect("末行是合法 JSON");
        assert_eq!(parsed["outcome"], "answered");
        assert_eq!(parsed["answers"][0]["selected"][0], "SQLite");
        assert_eq!(parsed["answers"][2]["skipped"], true);
    }

    #[test]
    fn format_declined_variants() {
        let qs = three_questions();
        // 整卡 decline。
        let out = format_question_result(&qs, &serde_json::json!({"outcome": "declined"}));
        assert!(out.contains(DECLINED_TEXT), "{out}");
        assert!(out.contains("{\"outcome\":\"declined\"}"));
        // answers 空 → 同整卡 decline。
        let out = format_question_result(&qs, &serde_json::json!({"answers": []}));
        assert!(out.contains(DECLINED_TEXT));
        // 逐题 decline：其余照常回填。
        let payload = serde_json::json!({
            "outcome": "answered",
            "answers": [
                { "id": "db", "declined": true },
                { "id": "note", "value": "尽快" }
            ]
        });
        let out = format_question_result(&qs, &payload);
        assert!(out.contains("- \"db\" → (declined)"), "{out}");
        assert!(out.contains("- \"note\" → 尽快"), "{out}");
        assert!(out.contains("- \"scope\" → (no answer)"), "{out}");
    }

    #[test]
    fn format_secret_never_leaks_plaintext() {
        let (qs, _) = normalize_questions(&serde_json::json!({
            "questions": [
                { "id": "tok", "question": "token？", "type": "secret",
                  "storage": {"kind":"env","name":"GITHUB_TOKEN"} }
            ]
        }))
        .expect("valid");
        let payload = serde_json::json!({
            "outcome": "answered",
            "answers": [ { "id": "tok", "value": "sk-super-secret-plaintext", "remember": false } ]
        });
        // 明文先入 vault（与工具/handler 路径一致；异步 store → 单线程
        // runtime block_on）。
        let vault = InMemorySecretVault::new();
        apply_secret_answers_sync(&qs, &payload, &vault, "17");
        assert_eq!(vault.lookup("GITHUB_TOKEN").as_deref(), Some("sk-super-secret-plaintext"));
        assert!(!vault.is_remembered("GITHUB_TOKEN"));

        let out = format_question_result(&qs, &payload);
        assert!(
            !out.contains("sk-super-secret-plaintext"),
            "明文绝不进上下文: {out}"
        );
        assert!(out.contains("已存入凭据：环境变量 GITHUB_TOKEN（本会话生效）"), "{out}");
        assert!(out.contains("\"secret\":\"env:GITHUB_TOKEN\""), "{out}");

        // remember=true → 已记住文案。
        let payload = serde_json::json!({
            "answers": [ { "id": "tok", "value": "sk-x", "remember": true } ]
        });
        apply_secret_answers_sync(&qs, &payload, &vault, "18");
        assert!(vault.is_remembered("GITHUB_TOKEN"));
        let out = format_question_result(&qs, &payload);
        assert!(out.contains("（已记住，后续会话可用）"), "{out}");
        // Debug 不泄露明文。
        let debug = format!("{vault:?}");
        assert!(!debug.contains("sk-"), "{debug}");
    }

    /// 同步测试用的 apply 包装（单线程 runtime 驱动 async store）。
    fn apply_secret_answers_sync(
        questions: &[NormalizedQuestion],
        payload: &Value,
        vault: &InMemorySecretVault,
        request_id: &str,
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        rt.block_on(async move {
            apply_secret_answers(questions, payload, vault, request_id).await;
        });
    }

    #[test]
    fn timeout_content_shape() {
        let out = timeout_result_content(300);
        assert!(out.contains("用户在 300 秒内未回应，可能不在。请自行判断继续。"));
        assert!(out.contains("{\"outcome\":\"timeout\"}"));
    }

    // ── secret vault 行为 ────────────────────────────────────────────

    fn store_sync(vault: &InMemorySecretVault, request_id: &str, name: &str, value: &str, remember: bool) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        rt.block_on(async move {
            vault.store(request_id, name, value, remember).await;
        });
    }

    #[test]
    fn vault_overwrite_and_scope() {
        let vault = InMemorySecretVault::new();
        // requestId 域内 (name, request_id) 唯一：同域同名后写覆盖。
        store_sync(&vault, "1", "A_TOKEN", "v1", false);
        store_sync(&vault, "1", "A_TOKEN", "v1b", false);
        assert_eq!(vault.len(), 1);
        assert_eq!(vault.lookup("A_TOKEN").as_deref(), Some("v1b"));
        // 跨域同名共存，lookup 取最新写入。
        store_sync(&vault, "2", "A_TOKEN", "v2", false);
        assert_eq!(vault.len(), 2);
        assert_eq!(vault.lookup("A_TOKEN").as_deref(), Some("v2"), "同名最新优先");
        store_sync(&vault, "2", "B_TOKEN", "v3", false);
        assert_eq!(vault.len(), 3);
        assert!(vault.lookup("MISSING").is_none());

        // env injection source: file: keys excluded, env keys collected (latest values).
        store_sync(&vault, "3", "file:/tmp/x", "fv", false);
        let pairs = vault.env_pairs();
        assert_eq!(pairs.len(), 2, "file 键不入 env: {pairs:?}");
        assert!(pairs.contains(&("A_TOKEN".to_owned(), "v2".to_owned())));
        assert!(pairs.contains(&("B_TOKEN".to_owned(), "v3".to_owned())));
        // 脱敏值源：全部值去重收录（含 file 值）。
        let values = vault.known_secret_values();
        assert!(values.contains(&"fv".to_owned()));
        assert!(values.contains(&"v3".to_owned()));
    }

    // ── 启动清扫（TTL / 已解决关卷 / 可恢复保留）─────────────────────

    fn tool_result_msg(id: &str, content: &str) -> Message {
        Message {
            attachments: None,
            role: MessageRole::Tool,
            content: content.into(),
            tool_call_id: Some(ToolCallId(id.into())),
            name: Some(ASK_USER_TOOL.into()),
            tool_calls: None,
            reasoning_content: None,
        }
    }

    fn assistant_with_ask(id: &str) -> Message {
        Message {
            attachments: None,
            role: MessageRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: ToolCallId(id.into()),
                name: ASK_USER_TOOL.into(),
                arguments: serde_json::json!({}),
            }]),
            reasoning_content: None,
        }
    }

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

        // 过期行（超 TTL）+ 新鲜行 + 已解决行。
        store
            .insert_pending_question("old", "tc-old", "[]", 1, now - QUESTION_TTL_MS - 1)
            .await
            .unwrap();
        store
            .insert_pending_question("fresh", "tc-fresh", "[]", 1, now)
            .await
            .unwrap();
        store
            .insert_pending_question("gone", "tc-gone", "[]", 1, now)
            .await
            .unwrap();

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
            assistant_with_ask("tc-old"),
            assistant_with_ask("tc-fresh"),
            tool_result_msg("tc-gone", "already answered elsewhere"),
        ];

        let recoverable = startup_question_maintenance(&store, &mut history, None).await;
        assert_eq!(recoverable, vec!["tc-fresh".to_owned()], "仅新鲜悬挂可恢复");

        // 过期：行 expired + 历史补全 EXPIRED_RESULT。
        assert_eq!(
            store.get_pending_question("old").await.unwrap().unwrap().status,
            "expired"
        );
        let expired_msg = history
            .iter()
            .find(|m| m.tool_call_id.as_ref().map(|id| id.0.as_str()) == Some("tc-old"))
            .expect("expired synth appended");
        assert_eq!(expired_msg.content, EXPIRED_RESULT);
        // 已解决：行 superseded。
        assert_eq!(
            store.get_pending_question("gone").await.unwrap().unwrap().status,
            "superseded"
        );
        // 新鲜：仍 pending。
        assert_eq!(
            store.get_pending_question("fresh").await.unwrap().unwrap().status,
            "pending"
        );
        // 悬挂全消（expired 补全后 tc-old 不再悬挂；tc-fresh 仍悬挂是预期恢复态）。
        let dangling: Vec<String> = dangling_tool_calls(&history)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(dangling.contains(&"tc-fresh".to_owned()));
        assert!(!dangling.contains(&"tc-old".to_owned()));
    }

    /// 行属于非恢复 run（如 failed 被排除 / 提问后开过更新的会话）：
    /// 恢复 history 查无该调用的声明与结果 → 不构成「被取代」证据，
    /// 保持 pending（迟到答案仍可补全）；过期者由 TTL 兜底关卷。
    #[tokio::test]
    async fn startup_maintenance_keeps_rows_of_other_runs_pending() {
        let store = openslate_store_sqlite::store::SqliteStore::new_in_memory()
            .await
            .unwrap();
        store.run_migrations().await.unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        // 「外来」行：属于其它 run（本 history 查无 tc-foreign）。
        store
            .insert_pending_question("foreign", "tc-foreign", "[]", 1, now)
            .await
            .unwrap();
        // 「外来」且过期：TTL 兜底照常关卷（expired），不受新判定影响。
        store
            .insert_pending_question(
                "foreign-old",
                "tc-foreign-old",
                "[]",
                1,
                now - QUESTION_TTL_MS - 1,
            )
            .await
            .unwrap();
        // 对照组：本 history 悬挂（可恢复）+ 结果已落地（崩溃窗口关卷）。
        store
            .insert_pending_question("mine", "tc-mine", "[]", 1, now)
            .await
            .unwrap();
        store
            .insert_pending_question("gone", "tc-gone", "[]", 1, now)
            .await
            .unwrap();

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
            assistant_with_ask("tc-mine"),
            tool_result_msg("tc-gone", "already answered elsewhere"),
        ];

        let recoverable = startup_question_maintenance(&store, &mut history, None).await;
        assert_eq!(recoverable, vec!["tc-mine".to_owned()]);

        // 外来新鲜行：保持 pending（不误关 superseded）。
        assert_eq!(
            store.get_pending_question("foreign").await.unwrap().unwrap().status,
            "pending",
            "属于非恢复 run 的行不得被误关"
        );
        // 外来过期行：TTL 兜底 expired（且无声明处 → 不合成补全）。
        assert_eq!(
            store
                .get_pending_question("foreign-old")
                .await
                .unwrap()
                .unwrap()
                .status,
            "expired"
        );
        assert!(
            !history
                .iter()
                .any(|m| m.tool_call_id.as_ref().map(|id| id.0.as_str()) == Some("tc-foreign-old")),
            "无声明处不合成 expired 补全"
        );
        // 对照组语义不变。
        assert_eq!(
            store.get_pending_question("gone").await.unwrap().unwrap().status,
            "superseded"
        );
        assert_eq!(
            store.get_pending_question("mine").await.unwrap().unwrap().status,
            "pending"
        );
    }

    /// args 宽松等价：int/float 表示形式漂移（provider / 持久化 round-trip）
    /// 不影响 find_call_id 的精确匹配路径。
    #[test]
    fn args_equivalent_tolerates_numeric_representation_drift() {
        use serde_json::json;
        let exact = json!({"questions": [{"question": "db?", "timeout": 120}]});
        assert!(AskUserContext::args_equivalent(
            &exact,
            &json!({"questions": [{"question": "db?", "timeout": 120}]})
        ));
        // 120 vs 120.0：结构不等但数值等价。
        assert!(AskUserContext::args_equivalent(
            &exact,
            &json!({"questions": [{"question": "db?", "timeout": 120.0}]})
        ));
        // 键序无关。
        assert!(AskUserContext::args_equivalent(
            &json!({"timeout": 60, "questions": []}),
            &json!({"questions": [], "timeout": 60.0})
        ));
        // 数值不同 / 结构不同 → 不等价。
        assert!(!AskUserContext::args_equivalent(
            &exact,
            &json!({"questions": [{"question": "db?", "timeout": 121}]})
        ));
        assert!(!AskUserContext::args_equivalent(
            &exact,
            &json!({"questions": [{"question": "别的?", "timeout": 120}]})
        ));
    }

    // ── 审批豁免 ─────────────────────────────────────────────────────

    #[test]
    fn approval_exempt_passes_side_effect_free_tools_through() {
        use openslate_core::approval::{
            ApprovalCallback, ApprovalDecision, ApprovalRequest, ApprovalTrigger, RiskLevel,
        };
        use openslate_session::approval::SessionApprovalBridge;
        use openslate_session::state::MsgSink;
        use std::sync::Arc as StdArc;

        #[derive(Default)]
        struct NoopSink;
        impl MsgSink for NoopSink {
            fn broadcast(&self, _msg: ServerMsg) {}
            fn send_to(&self, _conn_id: u64, _msg: ServerMsg) -> bool {
                true
            }
            fn conn_ids(&self) -> Vec<u64> {
                vec![1] // 有人在线：内层桥会阻塞等应答——豁免必须短路在先
            }
            fn count(&self) -> usize {
                1
            }
            fn close_all(&self) {}
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        let bridge = SessionApprovalBridge::new(Arc::new(NoopSink));
        let exempt = SideEffectFreeApproval::new(StdArc::new(bridge));
        let req = |tool: &str| ApprovalRequest {
            tool_name: tool.into(),
            arguments: serde_json::json!({}),
            agent_id: "root".into(),
            risk_level: RiskLevel::Medium,
            server: None,
            trigger: ApprovalTrigger::Manual,
            note: None,
        };
        // 若未豁免会阻塞在 mpsc recv（测试超时）；豁免则立即 Approved。
        // ask_user（先例）+ output_query（C4 纯读落盘输出）。
        for tool in [ASK_USER_TOOL, crate::output_query::OUTPUT_QUERY_TOOL] {
            assert_eq!(exempt.decide(&req(tool)), ApprovalDecision::Approved);
        }
    }
}
