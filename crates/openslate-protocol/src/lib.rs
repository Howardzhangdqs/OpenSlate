//! openslate server/client 共享协议（web-1）。
//!
//! 本 crate 是 server 与 client（TUI）两侧共用的**唯一**协议真相源：
//! WebSocket 上下行消息（`ClientMsg` / `ServerMsg`）与全部 DTO。
//! 纯数据层 —— 不依赖 tokio / axum，server 与 client 各自装配传输。
//!
//! 线格式：每条 WS text frame = 一个 JSON 对象，tagged enum
//! （`{"type":"...","...字段}` / EntryDto 用 `"kind"`），字段名 snake_case。
//! 契约测试（tests/contract.rs）逐变体钉死样例字节，**任何字段增删改名
//! 都必须同步改契约测试并升 [`PROTOCOL_VERSION`]**。
//!
//! DTO 原则（spec §2）：core 已有 serde derive 的类型
//! （`Message` / `Usage` / `RunId` / `RunStatus`）直接嵌入；core 仅
//! Deserialize 的 config 类型（`ProviderConfig` / `ModelConfig`）在协议侧
//! 定义镜像 DTO + `From` 转换，避免给 core 加 Serialize 耦合。

use std::collections::BTreeMap;

use openslate_core::agent_tree::AgentTree;
use openslate_core::approval::ApprovalRequest;
use openslate_core::config::{AskTimeout, LimitsConfig, ModelConfig, ProviderConfig};
use openslate_core::types::{Attachment, Message, MessageRole, RunId, RunStatus, Usage};

/// 当前协议版本。`hello.proto` 不等即拒连。
///
/// v2（ask-user r4/r5）：新增 `ClientMsg::QuestionReply` 与
/// `SnapshotDto.pending_questions`（问卷持久化 / 重启恢复 / 迟到答案）；
/// 未发布期间追补 `ClientMsg::SetAskUserConfig`（`[ask_user]` 设置同步）。
///
/// v3（tool-approval P1a）：审批扩展——`ApprovalAnswerChoice` 新增
/// `always_tool` / `session_mcp` / `always_mcp`；`ApprovalSummaryDto`
/// 新增 `server` / `trigger` / `note`（Option，None 跳键，旧 payload
/// 双向容忍）；新增 `ClientMsg::SetApprovalConfig` 与
/// `ConfigViewDto.approval`（生效规则视图）。`EntryDto::Approval.decision`
/// / `ApprovalResolved.choice` 为 String 值域扩展（新增 "always-tool" /
/// "session-mcp" / "always-mcp"，由 session 层产生）。
///
/// v4（存储 v5 多模态输入）：新增 [`AttachmentDto`]（附件引用形状，
/// 与 core `Attachment` 的持久化形状逐字段一致，`data` 永不过 wire）；
/// `ClientMsg::Submit` 与 `EntryDto::User` 新增 `attachments` 可选字段
/// （`serde(default)` + None 跳键——旧客户端 / 旧 payload 双向容忍，
/// 既有样例字节不变）。图/视频混发等约束由 mobile dispatch 层门控，
/// 协议层不表达。
///
/// v5（附件 token 响应侧回填）：[`AttachmentDto`] 新增 `tokens` 可选字段
/// （u64，**真实模型响应**顺带携带的 image input tokens——OpenAI 系
/// chat usage `prompt_tokens_details.image_tokens`；导入侧绝不预估，
/// 发起 API 请求之前附件数据不上行任何服务器）。语义：单图 Submit 的
/// 回合取**第一次**模型调用的值（agentic 循环里历史图每轮重发都计入
/// usage，只有首次调用才是这张图的首次完整成本）；多图不拆分（无值）；
/// 智谱 chat usage 不分模态 → 恒无值（UI 回退文件大小）。仅出现在
/// `EntryDto::User.attachments[]`（transcript 快照 / 持久化条目），
/// `ClientMsg::Submit` 方向忽略该字段。`serde(default)` + None 跳键，
/// 旧 payload 双向容忍。
///
/// v6（notify S1b）：新增 [`ClientMsg::NotificationReply`]（notify 工具
/// `wait:true` 模式的通知应答，即时或迟到，语义同 `question_reply`）。
/// `outcome` 上行白名单 = action / replied / dismissed / cancelled
/// （[`NotificationOutcome`] 枚举即白名单——timeout / expired 仅 Rust
/// 侧合成：router 超时 / TTL 清扫，wire 上不可表达，上行发送即解析
/// 失败被拒）。`label` / `value` / `text` 全 Option + None 跳键；
/// `run_id` 带 `serde(default)`（缺键 = 空串 = 跳过错配检测，与 Option
/// 字段缺省行为互补——收紧兼容性，不升版本）。
pub const PROTOCOL_VERSION: u32 = 6;

/// 附件引用（存储 v5 多模态输入，设计 §3/§7）。
///
/// 与 core [`Attachment`] 的**持久化形状**逐字段一致（`id` / `mime` /
/// `filename`）；core 侧的内存字段 `data: Option<Arc<[u8]>>` 被
/// `#[serde(skip)]`，永不序列化——DB 行 / 协议 wire / transcript 条目
/// 里都只有本 DTO 这个引用形状，字节由 mobile ingest 层经 content-
/// addressed 文件（`files/{sha256}.{ext}`）按 id 回填。
///
/// `mime` 为 ingest 归一化后的值：图像恒 `image/jpeg` / `image/png`
/// （webp/gif 已转码 JPEG），视频 `video/mp4` / `video/x-matroska` /
/// `video/quicktime`——即 wire 发送白名单本身（设计 §3.2）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentDto {
    /// `att-<uuid>`（attachments 表主键）。
    pub id: String,
    /// 归一化后 mime（见类型注释）。
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// 实测 image input tokens（v5 响应侧回填）：本附件随 Submit 后
    /// **第一次**模型调用 usage 里的 image tokens（provider 归一化；
    /// 智谱 chat usage 不分模态 → 恒 None）。非导入侧预估——导入时绝不
    /// 发网络请求。仅 `EntryDto::User` 出现；`ClientMsg::Submit` 忽略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

impl From<&Attachment> for AttachmentDto {
    fn from(a: &Attachment) -> Self {
        Self {
            id: a.id.clone(),
            mime: a.mime.clone(),
            filename: a.filename.clone(),
            tokens: None,
        }
    }
}

impl From<Attachment> for AttachmentDto {
    fn from(a: Attachment) -> Self {
        Self {
            id: a.id,
            mime: a.mime,
            filename: a.filename,
            tokens: None,
        }
    }
}

impl From<AttachmentDto> for Attachment {
    /// 引用形状 → core 混合形状：`data` 置 `None`（内存直达 provider 的
    /// 字节由 mobile ingest / resume 回填层负责，绝不过协议）。
    fn from(dto: AttachmentDto) -> Self {
        Self {
            id: dto.id,
            mime: dto.mime,
            filename: dto.filename,
            data: None,
        }
    }
}

// ---------------------------------------------------------------------------
// DTO：审批
// ---------------------------------------------------------------------------

/// 审批横幅摘要（TUI ApprovalSummary 的协议镜像）。`arguments` 为
/// 单行化 JSON，超过 120 字符截断加 `…`（与 TUI 展示约束一致）。
///
/// v3 新增字段全部 `Option` + None 跳键：旧客户端 / 旧快照双向安全
/// （v2 形状的 payload 反序列化后新字段为 `None`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApprovalSummaryDto {
    pub tool_name: String,
    pub arguments: String,
    pub agent_id: String,
    /// `"low"` / `"medium"` / `"high"`。
    pub risk_level: String,
    /// MCP server 作用域（tool-approval P1a）：聚合形态 = 下游 slug
    /// （`"github"`）；host 内置裸名 = host 前缀（`"termux"`）；非 MCP
    /// 工具无此键。UI 的 (c)/(d) MCP 级批准选项据此显示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// 本次询问的触发来源：`"manual"`（等级 Manual 直问）/
    /// `"auto_risk"`（Auto 等级下启发式风险 ≥ Medium——可能经 AI 审查
    /// 后才弹人工）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    /// AI 审查理由（`auto_risk` 触发且 LLM 审查器判定高风险时携带；
    /// `manual` 触发无此键）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 审批摘要的参数截断上限（字符，非字节；CJK 安全）。
pub const APPROVAL_SUMMARY_CLIP_CHARS: usize = 120;

/// 单行化并按字符截断到 [`APPROVAL_SUMMARY_CLIP_CHARS`]。
fn clip_oneline(s: &str, limit: usize) -> String {
    let oneline: String = s
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if oneline.chars().count() <= limit {
        oneline
    } else {
        let head: String = oneline.chars().take(limit).collect();
        format!("{head}…")
    }
}

impl From<&ApprovalRequest> for ApprovalSummaryDto {
    fn from(req: &ApprovalRequest) -> Self {
        // 与 TUI ApprovalSummary 相同的单行化：紧凑 JSON，失败回退原文。
        let args =
            serde_json::to_string(&req.arguments).unwrap_or_else(|_| req.arguments.to_string());
        Self {
            tool_name: req.tool_name.clone(),
            arguments: clip_oneline(&args, APPROVAL_SUMMARY_CLIP_CHARS),
            agent_id: req.agent_id.clone(),
            risk_level: req.risk_level.to_string(),
            server: req.server.clone(),
            trigger: Some(req.trigger.to_string()),
            note: req.note.clone(),
        }
    }
}

/// 待审批条目（snapshot 内嵌）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingApprovalDto {
    pub id: u64,
    pub summary: ApprovalSummaryDto,
}

/// 客户端审批应答的选择（tool-approval P1a 扩为六值）。
///
/// 前三值 = TUI 的 y / n / a 既有语义（`approve_all` ≡ 选项 (a) 当前
/// 会话允许该工具，写会话级工具白名单）；后三值 = mobile 审批卡的
/// 持久化 / MCP 作用域选项（(b) 一直允许该工具 / (c) 当前会话允许该
/// MCP / (d) 一直允许该 MCP）。取消/禁止执行 = `deny`（选项 (e)）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAnswerChoice {
    Approve,
    Deny,
    /// 本会话后续审批自动放行（写工具白名单）。
    ApproveAll,
    /// 一直允许该工具（持久化 `[approval].tool_levels` = allow）。
    AlwaysTool,
    /// 当前会话允许该 MCP server 的全部工具（会话内存作用域）。
    SessionMcp,
    /// 一直允许该 MCP server（持久化 `[approval].server_levels` = allow）。
    AlwaysMcp,
}

// ---------------------------------------------------------------------------
// DTO：ask_user 问卷（Agent 向用户提问；docs/proposals/ask-user-tool-design.md）
// ---------------------------------------------------------------------------

/// 客户端问卷应答动作（对齐 MCP Elicitation 的 accept / decline；cancel
/// 不在此表达——用户打断会话走现有回合取消通道）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionReplyAction {
    /// 提交答案（`answers` 为答案负载，结构由 mobile 层定义、协议透传）。
    Accept,
    /// 整卡拒绝作答（模型收到「用户未作答，请自行判断」类文案后继续）。
    Decline,
}

/// 问卷生命周期状态（`request_id` 幂等主键的当前阶段）。`cancelled`
/// 不在本枚举——用户停止由回合取消通道表达，问卷侧随 Tool 消息折叠。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    Pending,
    Answered,
    Declined,
    /// 新 turn 开始前被悬挂修复合成结果关卷（r5）。
    Superseded,
    /// TTL 过期（默认 24h，mobile 持久层判定）。
    Expired,
}

/// notify 通知应答的 outcome（v6，notify 设计 S1b；上行白名单本体）。
///
/// **枚举即白名单**：`timeout` / `expired` 仅 Rust 侧合成（host call
/// router 超时路径 / TTL 启动清扫），wire 上不可表达——客户端上行携带
/// 会在反序列化处直接失败被拒（D5），不进入 handler。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationOutcome {
    /// 用户点了通知上的动作按钮（`label` + 可选 `value`）。
    Action,
    /// 用户内联回复了文本（`text`）。
    Replied,
    /// 用户划掉通知未作选择（`deleteIntent` 通路）。
    Dismissed,
    /// 通知等待被撤销（`cancel_turn` 按钮 / 停止回合；迟到上行时关闭
    /// wait 行并按 cancelled 语义续跑——通知工具边角决策）。
    Cancelled,
}

impl NotificationOutcome {
    /// wire 字面量（snake_case，与 serde rename 一致；日志/文案用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Action => "action",
            Self::Replied => "replied",
            Self::Dismissed => "dismissed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// 待答问卷条目（snapshot 内嵌；重启恢复回放用）。`tool_call_id` 关联
/// 消息历史中的悬挂 tool call——迟到答案按 r5 语义补全该调用的
/// tool result 并从该点续跑，**不注入新用户消息**。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PendingQuestionDto {
    /// 幂等主键（host call id）；重复应答首答者赢。
    pub request_id: String,
    /// 关联的 tool call id（assistant 消息 `tool_calls[].id`）。
    pub tool_call_id: String,
    /// 问卷负载（§2 的 `questions` 数组；结构由下发侧定义，协议透传
    /// 不解释）。
    pub questions: serde_json::Value,
    pub status: QuestionStatus,
    /// 负载版本号（防回答已撤销/已改的问题）。
    pub version: u32,
}

/// ask_user 默认等待时限（core `AskTimeout` 的协议镜像；DTO 原则见
/// 模块头——core 类型 Deserialize-only，不能直接进 `ClientMsg`）。
/// serde 形状与 core **逐字节一致**（外部标签 + snake_case）：`"never"`
/// 或 `{"seconds": n}`——同一份 JSON 双侧可互换解析。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskUserTimeoutDto {
    /// 永不超时（默认；移动端等人场景）。
    Never,
    /// 阈值秒数（`0` 由消费端容忍为立即过期）。
    Seconds(u64),
}

impl From<AskUserTimeoutDto> for AskTimeout {
    fn from(t: AskUserTimeoutDto) -> Self {
        match t {
            AskUserTimeoutDto::Never => Self::Never,
            AskUserTimeoutDto::Seconds(s) => Self::Seconds(s),
        }
    }
}

impl From<AskTimeout> for AskUserTimeoutDto {
    fn from(t: AskTimeout) -> Self {
        match t {
            AskTimeout::Never => Self::Never,
            AskTimeout::Seconds(s) => Self::Seconds(s),
        }
    }
}

// ---------------------------------------------------------------------------
// DTO：转写条目（TUI TranscriptEntry 的协议镜像）
// ---------------------------------------------------------------------------

/// 工具条目状态（EntryDto 专用；`tool_end` 事件本身不携带状态——
/// 与 TuiEvent::ToolEnd 一致，失败由回合结束的 Tool 消息折叠判定）。
/// Live 迁移仅 Running → Done；`failed` 只由折叠启发式产生。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ToolStatusDto {
    Running,
    Done {
        bytes: usize,
        /// 输出留存被截断时为 true（args/输出留存上限）。
        truncated: bool,
        /// live 完成路径的实测时长；折叠路径（rebuild 后）为 `None`。
        elapsed_ms: Option<u64>,
    },
    Failed {
        summary: String,
    },
}

/// 工具条目展开详情。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolEntryDetailDto {
    /// 完整调用参数（pretty JSON，与 TUI DisplayArgs 一致）。
    pub args: String,
    /// 工具输出全文；live 期为 `None`（回合结束从 Tool 消息回填）。
    pub output: Option<String>,
}

/// 转写条目。字段名与 TUI `TranscriptEntry` 一一对应；不可序列化的
/// `DisplayArgs` / `ToolEntryDetail` 降为 String / [`ToolEntryDetailDto`]。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryDto {
    User {
        text: String,
        /// 随行附件引用（v4 多模态输入；transcript 渲染缩略图用）。
        /// `serde(default)` + None 跳键：旧 payload 双向容忍。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<AttachmentDto>>,
    },
    Assistant {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        name: String,
        args: String,
        /// 工具调用 id：live 事件（on_tool_start）与 DB 重建条目均携带；
        /// 旧版本 live 条目为 `None`（按名字 + FIFO 配对的兜底路径）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        status: ToolStatusDto,
        detail: ToolEntryDetailDto,
    },
    Approval {
        tool_name: String,
        /// `"approved"` / `"denied"` / `"approve-all"`（与 TUI 决议行
        /// 一致）；v3 起新增 `"always-tool"` / `"session-mcp"` /
        /// `"always-mcp"`（由 session 层产生，协议侧为 String 值域
        /// 扩展、无类型变化）。
        decision: String,
    },
    Delegate {
        agent: String,
        done: bool,
    },
    StepBreak,
    Meta {
        text: String,
    },
}

// ---------------------------------------------------------------------------
// 消息投影 → 转写条目（存储 v5 §4，L6 共享函数）
// ---------------------------------------------------------------------------

/// 消息历史 → 转写条目的**共享投影**（存储 v5 设计 §4）。
///
/// 消费方：mobile 两级重建的回退路径（`transcript_entries` 为空——
/// v5 之前的旧 run）；TUI `entry_from_message` 为同型独立实现（各自
/// 的条目枚举不同，未做架构合并——见设计 follow-up）。
///
/// 语义（与原 mobile `messages_to_transcript` 逐条对齐）：
/// - **User** → `User` 条目；`attachments` 引用形状随行（缩略图渲染；
///   `data` 永不出现在投影里——`AttachmentDto::from` 只取 id/mime/
///   filename）。
/// - **Assistant** → 非空 `reasoning_content` 先出 `Reasoning` 条目，
///   再出 `tool_calls` 的 `ToolCall` 条目（折叠路径终值 `Done{bytes: 0,
///   truncated: false, elapsed_ms: None}`——live 实测时长只在内存里），
///   最后非空白 `content` 出 `Assistant` 条目。
/// - **Tool** → 不产生新条目：输出（前 2000 字符）并入**最近一条**
///   匹配的 `ToolCall`（优先 `tool_call_id` 精确配对；消息无 id 时按
///   工具名兜底）。悬挂 tool_calls（无 Tool 消息跟随时）保持 `Done`
///   零字节形态——这正是 open_session 悬挂尾部 pop 的前置形态。
/// - **System** → 跳过（provider 指令，非用户可见转写）。
/// - `Approval` / `Delegate` / `StepBreak` / `Meta` **不可由消息投影**
///   ——它们只存在于 `transcript_entries` 行（v5 真相源）或 live 事件，
///   这正是两级重建优先读 entries 表的原因。
///
/// 纯函数：无 IO、无状态；空入参 → 空转写。
pub fn project_from_messages(messages: &[Message]) -> Vec<EntryDto> {
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            MessageRole::User => out.push(EntryDto::User {
                text: m.content.clone(),
                attachments: m
                    .attachments
                    .as_ref()
                    .map(|atts| atts.iter().map(AttachmentDto::from).collect()),
            }),
            MessageRole::Assistant => {
                // 思维链随消息持久化——恢复时还原为折叠的 Reasoning 条目。
                if let Some(rc) = m.reasoning_content.as_ref().filter(|r| !r.is_empty()) {
                    out.push(EntryDto::Reasoning { text: rc.clone() });
                }
                if let Some(tcs) = m.tool_calls.as_ref() {
                    for tc in tcs {
                        out.push(EntryDto::ToolCall {
                            name: tc.name.clone(),
                            // 预览截断（字符，CJK 安全）；全文在 detail.args。
                            args: tc
                                .arguments
                                .to_string()
                                .chars()
                                .take(40)
                                .collect::<String>(),
                            call_id: Some(tc.id.0.clone()),
                            status: ToolStatusDto::Done {
                                bytes: 0,
                                truncated: false,
                                elapsed_ms: None,
                            },
                            detail: ToolEntryDetailDto {
                                args: tc.arguments.to_string(),
                                output: None,
                            },
                        });
                    }
                }
                if !m.content.trim().is_empty() {
                    out.push(EntryDto::Assistant {
                        text: m.content.clone(),
                    });
                }
            }
            MessageRole::Tool => {
                // 工具结果并入最近一条匹配的 ToolCall（id 精确配对优先，
                // 无 id 时按名字兜底——与 TUI fold 语义同型）。
                if let Some(EntryDto::ToolCall { detail, .. }) =
                    out.iter_mut().rev().find_map(|e| {
                        if let EntryDto::ToolCall { name, call_id, .. } = e {
                            if call_id.as_deref()
                                == m.tool_call_id.as_ref().map(|x| x.0.as_str())
                                || (m.tool_call_id.is_none()
                                    && name == m.name.as_deref().unwrap_or(""))
                            {
                                return Some(e);
                            }
                            None
                        } else {
                            None
                        }
                    })
                {
                    detail.output = Some(m.content.chars().take(2000).collect::<String>());
                }
            }
            MessageRole::System => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// DTO：回合摘要 / 快照
// ---------------------------------------------------------------------------

/// 回合终态摘要（TUI TurnSummary 的可序列化投影；execution_tree 不进
/// 协议——客户端用它重建折叠 Tool 消息，不需要执行树）。
///
/// 不 derive `PartialEq`：内嵌的 core `Message` 未实现它。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnSummaryDto {
    pub run_id: RunId,
    pub status: RunStatus,
    pub total_steps: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    pub model: String,
    pub messages: Vec<Message>,
}

impl From<openslate_core::run_manager::ManagedRunResult> for TurnSummaryDto {
    fn from(r: openslate_core::run_manager::ManagedRunResult) -> Self {
        Self {
            run_id: r.run_id,
            status: r.status,
            total_steps: r.total_steps,
            total_input_tokens: r.total_input_tokens,
            total_output_tokens: r.total_output_tokens,
            total_cost_usd: r.total_cost_usd,
            model: r.model,
            messages: r.messages,
        }
    }
}

/// 全量快照：新连接 hello 成功后定向发送；`new_session` 重置后逐连接补发。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SnapshotDto {
    pub proto: u32,
    pub session_id: String,
    pub session_label: String,
    /// 当前会话 run 的 run_id（mobile 转写镜像键对齐：Kotlin 落盘/恢复
    /// 以事件流内嵌的 run 键为准，不再旁路 FFI 直读实时态）。`None` =
    /// 无持久化 run（server 投影自 `session_run`）。`serde(default)`：
    /// 新代码反序列化旧快照（缺键）安全，旧客户端忽略未知字段，双向兼容。
    /// None 时缺键不序列化——避免 wire 上出现 `"run_id":null`（Kotlin
    /// org.json 的 optString 对 JSON null 返回字面量 "null"，曾把本地
    /// 转写镜像键污染成 "null" 僵尸文件，见 RuntimeBridge 修复注释）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// 转写条目镜像（字段名 `transcript`，同 spec §3）。
    pub transcript: Vec<EntryDto>,
    pub running: bool,
    pub depth_cur: u32,
    pub agents_running: u32,
    pub tool_calls_cur: u32,
    pub model_alias: String,
    pub pending_approval: Option<PendingApprovalDto>,
    /// 待答问卷（ask_user 重启恢复回放）。`serde(default)`：旧客户端
    /// 反序列化新快照安全。server 投影恒为空——问卷持久化状态在宿主
    /// （mobile）持久层，由其装配侧在快照上回填。
    #[serde(default)]
    pub pending_questions: Vec<PendingQuestionDto>,
    pub config: ConfigViewDto,
    /// 会话累计统计（GAP-3）：server 是真相源，重连/新客户端 attach
    /// 不清零；`turn_ok` 后由 server 更新。`new_session` 归零。
    pub session_stats: SessionStatsDto,
}

/// 会话累计统计（server 侧维护）。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionStatsDto {
    pub turns: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
}

impl Default for SessionStatsDto {
    fn default() -> Self {
        Self {
            turns: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// DTO：配置视图（model-mgmt / REST /api/config 共用）
// ---------------------------------------------------------------------------

/// provider 条目（`[providers.<name>]` 的协议镜像）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderDto {
    pub base_url: String,
    pub api_key_env: String,
    /// `"openai"` / `"anthropic"` / `"gemini"` / `"ollama"`；`None` =
    /// 未配置（按 base_url 猜测的旧行为）。
    #[serde(default)]
    pub adapter: Option<String>,
    /// 人类可读显示名（中文/空格/大小写均可）；`None` = 显示用键名。
    /// 键名（`name`）恒为内部 ID：模型引用、env 派生、Keystore 均按 ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub max_attempts: u32,
    pub retry_base_ms: u64,
}

impl From<&ProviderConfig> for ProviderDto {
    fn from(p: &ProviderConfig) -> Self {
        Self {
            base_url: p.base_url.clone(),
            api_key_env: p.api_key_env.clone(),
            adapter: p.adapter.clone(),
            title: p.title.clone(),
            max_attempts: p.max_attempts,
            retry_base_ms: p.retry_base_ms,
        }
    }
}

impl From<ProviderDto> for ProviderConfig {
    fn from(p: ProviderDto) -> Self {
        ProviderConfig {
            base_url: p.base_url,
            api_key_env: p.api_key_env,
            adapter: p.adapter,
            title: p.title,
            max_attempts: p.max_attempts,
            retry_base_ms: p.retry_base_ms,
        }
    }
}

/// 模型库条目（`[models.<entry>]` 的协议镜像）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelDto {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub max_context_tokens: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub supports_tool_call: bool,
    #[serde(default)]
    pub supports_vision: bool,
    #[serde(default)]
    pub supports_reasoning: bool,
    #[serde(default)]
    pub input_price_per_mtok: Option<f64>,
    #[serde(default)]
    pub output_price_per_mtok: Option<f64>,
    /// Thinking effort（推理力度）：七档全集 `"none" | "minimal" | "low"
    /// | "medium" | "high" | "xhigh" | "max"`；`None` = Auto（不传该
    /// 字段，跟随 provider 默认），`"none"` = 显式硬关闭思考——两种
    /// 语义互不等价。命名沿用本 DTO 的 snake_case 习惯；序列化沿用
    /// `ProviderDto::title` 的后加可选字段先例——None 跳键，保证既有
    /// 线格式样例字节不变（旧端点/旧客户端零感知）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl From<&ModelConfig> for ModelDto {
    fn from(m: &ModelConfig) -> Self {
        Self {
            provider: m.provider.clone(),
            model: m.model.clone(),
            max_context_tokens: m.max_context_tokens,
            max_output_tokens: m.max_output_tokens,
            supports_tool_call: m.supports_tool_call,
            supports_vision: m.supports_vision,
            supports_reasoning: m.supports_reasoning,
            input_price_per_mtok: m.input_price_per_mtok,
            output_price_per_mtok: m.output_price_per_mtok,
            reasoning_effort: m.reasoning_effort.clone(),
        }
    }
}

impl From<ModelDto> for ModelConfig {
    fn from(m: ModelDto) -> Self {
        ModelConfig {
            provider: m.provider,
            model: m.model,
            max_context_tokens: m.max_context_tokens,
            max_output_tokens: m.max_output_tokens,
            supports_tool_call: m.supports_tool_call,
            supports_vision: m.supports_vision,
            supports_reasoning: m.supports_reasoning,
            input_price_per_mtok: m.input_price_per_mtok,
            output_price_per_mtok: m.output_price_per_mtok,
            reasoning_effort: m.reasoning_effort,
        }
    }
}

/// skill 摘要（name + description，渐进披露第一层）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillInfoDto {
    pub name: String,
    pub description: String,
}

/// 执行限额（`[limits]` 的协议镜像，GAP-1：客户端 make_ctx 与上下文
/// 仪表需要；`Option` 语义在 server 侧已用默认值解掉）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LimitsDto {
    pub max_steps: u32,
    pub max_depth: u32,
    pub max_tool_calls: u32,
    pub max_child_agent_calls: u32,
    pub timeout_ms: u64,
    pub max_context_messages: u32,
    pub max_context_bytes: u32,
    pub max_output_bytes: u32,
    /// 长输出落盘阈值（C2；`0` = 关闭）。serde default = 16384：旧
    /// server 载荷缺该键时客户端镜像取与 `LimitsConfig::default()` 一致
    /// 的值（additive 兼容，proto 版本不 bump）。
    #[serde(default = "default_output_store_threshold")]
    pub output_store_threshold: u32,
    pub auto_compact: bool,
    pub parallel_tool_calls: bool,
}

/// [`LimitsDto::output_store_threshold`] 的 serde 缺省（= 配置默认）。
fn default_output_store_threshold() -> u32 {
    16_384
}

impl From<&LimitsConfig> for LimitsDto {
    fn from(l: &LimitsConfig) -> Self {
        Self {
            max_steps: l.max_steps,
            max_depth: l.max_depth,
            max_tool_calls: l.max_tool_calls,
            max_child_agent_calls: l.max_child_agent_calls,
            timeout_ms: l.timeout_ms,
            max_context_messages: l.max_context_messages,
            max_context_bytes: l.max_context_bytes,
            max_output_bytes: l.max_output_bytes,
            output_store_threshold: l.output_store_threshold,
            auto_compact: l.auto_compact,
            parallel_tool_calls: l.parallel_tool_calls,
        }
    }
}

/// agents 树节点（GAP-1：agents 浮层 set_root 与 models_change_guard
/// 的引用检查需要；`children` 递归展开，顺序 = 配置 children 声明序）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentNodeDto {
    pub id: String,
    pub name: String,
    pub model: String,
    pub children: Vec<AgentNodeDto>,
}

impl From<&AgentTree> for AgentNodeDto {
    fn from(tree: &AgentTree) -> Self {
        fn build(tree: &AgentTree, id: &openslate_core::types::AgentId) -> AgentNodeDto {
            let node = tree
                .get_agent(id)
                .expect("tree walk only visits existing nodes");
            AgentNodeDto {
                id: node.id.0.clone(),
                name: node.name.clone(),
                model: node.model_alias.clone(),
                children: node.children.iter().map(|c| build(tree, c)).collect(),
            }
        }
        build(tree, tree.root_id())
    }
}

/// `[approval]` 的生效规则视图（tool-approval P1a）：**映射后的真相**
/// （legacy `policy`/`tools` 已并入，非 toml 原文），由 session 层经
/// core `rules_from_config` 投影。level 字符串值域 `"manual" | "allow"
/// | "auto"`（同 `ClientMsg::SetApprovalConfig`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApprovalConfigViewDto {
    pub default_level: String,
    pub tool_levels: BTreeMap<String, String>,
    pub server_levels: BTreeMap<String, String>,
}

/// 配置只读视图：`/provider` 浮层、REST `/api/config` 与 snapshot 的
/// 同一份数据（GAP-1：客户端是零本地配置的纯消费者，本视图必须自足）。
/// BTreeMap 保证 key 有序（快照 diff / 契约测试稳定）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConfigViewDto {
    pub providers: BTreeMap<String, ProviderDto>,
    pub models: BTreeMap<String, ModelDto>,
    pub levels: BTreeMap<String, String>,
    /// 功能 → 代号（server 侧已并入缺省回退，非 Option —— 客户端拿到的
    /// 即生效值；语义同 `limits` 的"server 解析默认值"先例）。
    /// `serde(default)`：旧客户端反序列化新快照 / 新客户端反序列化旧
    /// 快照都安全。
    #[serde(default)]
    pub capabilities: BTreeMap<String, String>,
    /// 执行限额（server 侧已解析默认值，非 Option）。
    pub limits: LimitsDto,
    /// agents 树（root 起递归）。
    pub agents: AgentNodeDto,
    pub skills: Vec<SkillInfoDto>,
    /// 实际生效配置文件路径（`--config` 显式指定或本地发现链结果）。
    pub active_config: String,
    /// 全局库路径（`~/.config/openslate/openslate.toml`）；无全局库时
    /// `None`。
    pub global_config: Option<String>,
    /// 本地叠加配置路径（`./.openslate/openslate.toml`）；无本地配置时
    /// `None`（`--config` 显式指定时与 active 相同）。
    pub local_config: Option<String>,
    /// 工具审批生效规则（v3，tool-approval P1a）。`None` = 装配层未
    /// 填充（本阶段 session 层接线在 P1b）；None 跳键保证既有样例字节
    /// 不变，旧快照 / 旧客户端双向容忍。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalConfigViewDto>,
}

// ---------------------------------------------------------------------------
// DTO：REST 辅助
// ---------------------------------------------------------------------------

/// store 里的历史 run 行（REST `/api/sessions`）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionListItemDto {
    pub id: String,
    pub title: Option<String>,
    pub root_agent_id: String,
    pub status: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub cost_usd: f64,
    /// run 级 token 聚合（存储 v5 `runs.input_tokens`；存量 run 无
    /// 采集时为 0）。`serde(default)` + None 跳键：旧 payload 双向容忍。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

/// notice 语义级别（渲染仅影响着色，不影响顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

/// 回合失败的机器可读分类（[`ServerMsg::TurnError`] 的 `kind` 字段）。
/// session 引擎按它判定「回合级断连自动重试」（仅 `network`，预算 1 次
/// + 会话级 10s 冷却窗）；客户端可按类别差异化展示（Kotlin 侧暂不
/// 消费，serde 忽略未知字段）。snake_case 变体名即 wire 形态
/// （`"network"` / `"rate_limit"` / `"auth"` / `"model"` / `"other"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnErrorKind {
    /// 网络中断 / 连接失败（reqwest decode 错、`ConnectionError`、
    /// 内层为连接错误的 `RetryExhausted`）。唯一可自动重试的类别。
    Network,
    /// 限流（429 / rate limit）。
    RateLimit,
    /// 认证 / 授权失败（401/403、key 无效）。
    Auth,
    /// 模型侧失败（响应畸形、模型不存在、空响应）。
    Model,
    /// 其余（超时、运行时限制、存储等）。
    Other,
}

// ---------------------------------------------------------------------------
// server 自描述文件（auto-attach-1：`<config 目录>/server.json`）
// ---------------------------------------------------------------------------

/// server.json 文件名（server 侧落盘 / TUI 侧发现共用，放协议 crate
/// 保持单一真相源）。
pub const SERVER_INFO_FILE: &str = "server.json";

/// server 启动时落盘的自描述信息：TUI 无 `--server` 时读取它自动
/// attach（发现链 flag > env > server.json）。server 优雅停机时删除；
/// 防双启动靠对其 `url` 的 health 探测（不是 pid 校验——pid 会复用）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServerInfo {
    /// 协议版本；读侧与 [`PROTOCOL_VERSION`] 不符视为陈旧文件。
    pub proto: u32,
    /// 完整 WS 路径（如 `ws://127.0.0.1:7800/api/ws`）。
    pub url: String,
    pub pid: u32,
    pub port: u16,
    /// 实际 bind 地址（可能与 url host 不同——bind 0.0.0.0 时 url
    /// 仍写 127.0.0.1 供本机客户端）。
    pub bind: String,
    /// 启动时刻（ISO 8601，本地时区带偏移）。
    pub started_at: String,
    /// `--auth-token` 值；无鉴权为 `None`。文件权限 0600 的原因。
    #[serde(default)]
    pub token: Option<String>,
    /// server 进程工作目录（调试友好）。
    pub cwd: String,
}

// ---------------------------------------------------------------------------
// ClientMsg（上行）
// ---------------------------------------------------------------------------

/// 客户端 → server。首条必须是 [`ClientMsg::Hello`]。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// 握手：版本 + 可选 token。失败即被断开。
    Hello {
        proto: u32,
        #[serde(default)]
        token: Option<String>,
    },
    /// 提交一轮用户输入。
    Submit {
        text: String,
        /// 随行附件引用（v4 多模态输入）：id 均为 mobile FFI
        /// `import_attachment` 返回的 `att-<uuid>`；门控（模态能力 /
        /// 数量 / 体积 / 图视频互斥）在 mobile dispatch 层执行，拒绝经
        /// notice 通道返回（设计 §3.3/§3.4/§6.4）。`serde(default)`：
        /// 旧客户端（无字段）→ None。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<AttachmentDto>>,
    },
    /// 审批应答。任一客户端首答生效；迟到应答收到定向 notice。
    ApprovalAnswer {
        id: u64,
        choice: ApprovalAnswerChoice,
    },
    /// ask_user 问卷应答（即时或迟到，首答者赢——语义同
    /// `approval_answer`）。`answers` 结构由 mobile 层定义，协议透传
    /// 不解释；decline 时负载由客户端自定（通常为 `null` 或空对象）。
    QuestionReply {
        request_id: String,
        action: QuestionReplyAction,
        answers: serde_json::Value,
    },
    /// notify 通知应答（v6，notify S1b；即时或迟到，首答者赢——语义
    /// 同 `question_reply`）。即时路径：`request_id` = 在途 host call
    /// id，`resolve` 唤醒工具；迟到路径（进程重启后点通知动作）：
    /// 按 `request_id` 原子关卷 `pending_notifications` 行并以
    /// cancelled/action/… 语义补全原 tool result 续跑。`run_id` 用于
    /// 路由对齐（Kotlin openSession）与错配检测（mobile 侧纵深防御；
    /// 空串 = 跳过错配检测）。`#[serde(default)]`：客户端非空才带键
    /// （Kotlin 惯例），缺键解析为空串而非整条拒绝——收紧兼容性，
    /// 不升版本。
    NotificationReply {
        request_id: String,
        #[serde(default)]
        run_id: String,
        outcome: NotificationOutcome,
        /// action 的按钮 label。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// action 的按钮 value（缺省 = 「用户在通知上点了「label」」，
        /// 由 Kotlin 侧补齐后上行）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        /// replied 的回复文本。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// 取消当前回合（复用 TUI Ctrl-C 语义：引擎侧 cancel）。
    Cancel,
    /// 新会话：历史/转写清空，session_id 更新，广播 session_reset。
    NewSession,
    /// 切模型级别（alias 必须可解析，server 广播 model_changed）。
    SetModel {
        alias: String,
    },
    UpsertProvider {
        name: String,
        provider: ProviderDto,
    },
    DeleteProvider {
        name: String,
    },
    UpsertModel {
        entry: String,
        model: ModelDto,
    },
    DeleteModel {
        entry: String,
    },
    SetLevel {
        level: String,
        entry: String,
    },
    DeleteLevel {
        level: String,
    },
    /// 功能 → 代号绑定（capability ∈ main/compact/title；alias 必须可
    /// 解析为 levels key 或 models 条目，server 校验后落盘并广播
    /// config_changed；capability == "main" 同时热切换会话模型并广播
    /// model_changed）。
    SetCapability {
        capability: String,
        alias: String,
    },
    /// ask_user 工具行为（`[ask_user]` 节）：默认超时 + 输出脱敏开关。
    /// server 写 **active** 配置落盘（merge 规则：ask_user 取 local
    /// verbatim）并广播 config_changed。mobile 设置页「内置工具」的
    /// 同步入口——openslate.toml 为生效真值，本地偏好仅 UI 显示源。
    SetAskUserConfig {
        default_timeout: AskUserTimeoutDto,
        mask_secrets: bool,
    },
    /// 工具审批规则写入（v3，tool-approval P1a；设置页「工具权限」）：
    /// server **整段重写** `[approval]` 为新 schema（删 legacy
    /// `policy`/`tools` 键）→ 重载 → rules 热更 → 广播 config_changed。
    /// `default_level = None` 表示缺省（allow）。level 字符串值域
    /// `"manual" | "allow" | "auto"`；`server_levels` 键 = 审批作用域
    /// 标识（下游 slug / host 前缀，core `mcp::server_slug_of` 对齐）。
    SetApprovalConfig {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_level: Option<String>,
        #[serde(default)]
        tool_levels: BTreeMap<String, String>,
        #[serde(default)]
        server_levels: BTreeMap<String, String>,
    },
    /// 新增/更新 MCP server（Streamable HTTP 条目；手机端 mcp-host 场景）。
    /// headers 如 `{ "Authorization": "Bearer <token>" }`，可选。
    UpsertMcpServer {
        name: String,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<std::collections::HashMap<String, String>>,
    },
    /// 删除 MCP server 条目（不存在时静默成功，幂等）。
    RemoveMcpServer {
        name: String,
    },
    /// 直接粘贴 API key：server 派生 `<NAME>_API_KEY` 写 `.env`（0600），
    /// 值不回显、不广播。
    SetApiKey {
        provider: String,
        value: String,
    },
}

// ---------------------------------------------------------------------------
// ServerMsg（下行）
// ---------------------------------------------------------------------------

/// server → 客户端。同一连接内事件全序（单发送队列）。
///
/// 不 derive `PartialEq`：`TurnOk` 内嵌 core `Message`（无 PartialEq）；
/// 测试侧用 `serde_json::to_value` 比对。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// 全量快照（hello 成功后 / session_reset 后，逐连接定向）。
    /// `Box` 仅为压 enum 变体尺寸（clippy large_enum_variant），
    /// serde 线格式与裸值完全一致。
    Snapshot {
        session: Box<SnapshotDto>,
    },
    RequestStart {
        step: u32,
        model: String,
    },
    FirstToken,
    /// 答案流式增量。
    Delta {
        text: String,
    },
    /// 思维链流式增量。
    Reasoning {
        text: String,
    },
    /// 输入 token 估算（流式期 `↑N`）。
    InputEstimate {
        tokens: u32,
    },
    /// 单次请求精确 usage。
    Usage {
        usage: Usage,
    },
    /// 请求结束（附带的 usage 行语义）。
    RequestEnd,
    /// 步进（step+1 / step=N）。
    StepEnd,
    /// 工具调用开始（镜像 TuiEvent::ToolStart）。`call_id` 为 provider
    /// 分配的工具调用 id（客户端按其精确配对条目 / 懒加载全量输出）；
    /// 旧版本事件不带（None），客户端按名字 + FIFO 兜底。
    ToolStart {
        name: String,
        args: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    /// 工具调用结束（镜像 TuiEvent::ToolEnd；失败与否由回合结束的
    /// Tool 消息折叠判定，事件本身只带字节数/截断位）。
    /// `preview` 为输出前若干字符（UI 投影兜底；None = 无输出或未携带）。
    /// `call_id` 同 [`ServerMsg::ToolStart`]。
    ToolEnd {
        name: String,
        bytes: usize,
        truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    ApprovalRequested {
        id: u64,
        summary: ApprovalSummaryDto,
    },
    /// 审批已裁决（首答后广播；所有客户端清横幅）。`choice` 值域同
    /// [`ApprovalAnswerChoice`]（approve/deny/approve_all + v3 新增
    /// always_tool/session_mcp/always_mcp 的连字符拼写
    /// "always-tool"/"session-mcp"/"always-mcp"，由 session 层产生）。
    ApprovalResolved {
        id: u64,
        choice: String,
    },
    TurnOk {
        /// `Box` 同上，仅压尺寸。
        summary: Box<TurnSummaryDto>,
    },
    TurnError {
        message: String,
        /// 机器可读错误分类（[`TurnErrorKind`]）；`serde(default)` +
        /// None 跳键：旧端 payload 双向容忍（未分类 = 无键）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<TurnErrorKind>,
    },
    /// 配置 CRUD 成功后广播新视图（`Box` 同上，仅压尺寸）。
    ConfigChanged {
        config: Box<ConfigViewDto>,
    },
    ModelChanged {
        alias: String,
    },
    /// run 标题异步生成完成（首条消息的 LLM 标题替换 run 开始时的
    /// prompt 截断占位）。历史列表按 `run_id` 原位更新标题。
    RunTitle {
        run_id: String,
        title: String,
    },
    SessionReset,
    Notice {
        text: String,
        level: NoticeLevel,
    },
    /// 协议级错误（hello 版本不符 / token 校验失败 / 非法消息），
    /// 定向发送后随即断开。`code` 机器可读（`proto_mismatch` /
    /// `bad_token` / `bad_message`），`message` 人读中文。
    Error {
        code: String,
        message: String,
    },
}
