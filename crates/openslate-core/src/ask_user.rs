//! ask_user（Agent 向用户提问）的消息历史手术与合成文案。
//!
//! 设计真相源：`docs/proposals/ask-user-tool-design.md`（r5 迟到语义）。
//! 工具本体（`Tool` trait 实现 + host call 通道）在 openslate-mobile
//! （后续 lane）；本模块只承载与引擎/会话共享的纯函数逻辑：
//!
//! 1. **迟到答案补全**（[`append_late_tool_result`]）：以答案为悬挂的
//!    tool call 追加 tool result 消息（role=tool、tool_call_id 对应）。
//!    迟到等价不变量（设计 §3 工程要点 7）：补全后的消息历史与「答案
//!    即时返回」完全同形，模型无法也不需要区分。
//! 2. **悬挂修复**（[`repair_dangling_tool_calls`]）：新 turn 开始前
//!    （superseded）或问卷 TTL 过期（expired）对所有悬挂 tool call
//!    合成结果——OpenAI/Anthropic 均要求 assistant 的 tool_calls 先有
//!    对应 tool result，悬挂历史不能再进模型循环。
//! 3. **中断豁免**：进程中断不得对悬挂的 ask_user 预先合成 cancelled
//!    （否则迟到答案无处补全）。豁免在 `runtime.rs` 的取消合成路径按
//!    工具名（[`ASK_USER_TOOL`]）判断；本模块只提供常量与文案。
//!
//! 悬挂（dangling）的定义：某 assistant 消息声明了 `tool_calls`，但其
//! 后**紧随的连续 Tool 结果段**中没有该调用的 `tool_call_id`。合法消息
//! 序里一个批次的全部结果必连续出现在该 assistant 消息之后、任何非
//! Tool 消息之前，因此「连续段」扫描即是完整判定。

use std::collections::HashSet;

use crate::types::{Message, MessageRole, ToolCallId};

/// 提问工具名（设计 §7 决策 3）。工具本体在 openslate-mobile 注册，
/// core 侧只按名引用（与 runtime 对 `run_code` / `call_agent` 的
/// 名字判定同一模式）。
pub const ASK_USER_TOOL: &str = "ask_user";

/// 新用户消息开启新 turn 前的悬挂修复合成文案（superseded）。
pub const SUPERSEDED_RESULT: &str = "Question superseded by a new user message.";

/// 问卷 TTL 过期的合成文案（expired；TTL 由 mobile 持久层判定）。
pub const EXPIRED_RESULT: &str = "Question expired unanswered (TTL).";

/// 同批兄弟工具调用的兜底合成文案。非 ask_user 工具正常路径不会跨
/// turn 悬挂（取消时 runtime 合成 cancelled），仅在进程中断与
/// ask_user 同批悬挂时出现——文案对齐 `cancelled_tool_message` 的
/// 方括号风格。
pub const INTERRUPTED_RESULT: &str = "[tool call did not complete before interruption]";

// ── 进程级工具输出脱敏（ask_user secret 子系统；设计 r3「可开关的输出
//    脱敏」）─────────────────────────────────────────────────────────────────
//
// core 无法依赖 mobile 的凭据库，脱敏**值源**由宿主运行时注入（mobile
// 在装配期把 vault 的已知密钥值 + `[ask_user] mask_secrets` 开关装进
// 闭包）；core 只提供注册表与消费点。默认 `None`——无宿主注入时行为
// 完全不变（桌面 CLI/TUI 与全部既有测试零影响），与
// `mcp::set_mcp_auth_override` 同款进程级接缝模式。

/// 工具输出脱敏器：输入原文，返回脱敏后文本（无注册时恒等）。
pub type SecretMasker = std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>;

/// 进程级脱敏器列表。生产 = 恰一条（mobile 装配期 push）；并行测试
/// 会各自 push（各自的 vault 均为空值源时互不干扰，叠加应用天然安全）。
static SECRET_MASKERS: std::sync::OnceLock<std::sync::RwLock<Vec<SecretMasker>>> =
    std::sync::OnceLock::new();

fn maskers_cell() -> &'static std::sync::RwLock<Vec<SecretMasker>> {
    SECRET_MASKERS.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

/// 整体替换/清空脱敏器列表（`None` 或 `Some` 均先清空再装；测试复位
/// 用 `set_secret_masker(None)`）。
///
/// 开关语义（`[ask_user] mask_secrets`）由各脱敏器闭包自行判断——关闭
/// 时闭包返回原文，core 侧无需感知配置（跨 crate 传递面最小化）。
pub fn set_secret_masker(masker: Option<SecretMasker>) {
    let mut guard = maskers_cell().write().expect("secret masker 锁中毒");
    guard.clear();
    if let Some(m) = masker {
        guard.push(m);
    }
}

/// 追加一个脱敏器（宿主运行时装配期调用；进程内多次装配——如并行
/// 测试各建一个 runtime——叠加应用，互不覆盖）。
pub fn push_secret_masker(masker: SecretMasker) {
    maskers_cell()
        .write()
        .expect("secret masker 锁中毒")
        .push(masker);
}

fn secret_maskers() -> Vec<SecretMasker> {
    maskers_cell()
        .read()
        .expect("secret masker 锁中毒")
        .clone()
}

/// 工具输出实际发生脱敏替换时，追加到内容末尾的内部提示（告知模型
/// 值在宿主凭据库、按名引用即可，无需索要明文——防模型看到 `***` 后
/// 反向追问/尝试还原）。仅在**确有替换**时追加（避免噪音）；作为 tool
/// result 内容的一部分（不注入独立 system 消息）。
pub const MASKED_NOTE: &str = "<system-note>Output masked: secret values were replaced with *** by the host. \
Actual values are stored in the credential vault and referenced by name; \
you do not need the plaintext.</system-note>";

/// 对文本依次应用全部已注册脱敏器（无注册 → 原样返回）。
pub fn mask_secrets(text: &str) -> String {
    let mut masked = text.to_owned();
    for masker in secret_maskers() {
        let next = masker(&masked);
        if next != masked {
            masked = next;
        }
    }
    masked
}

/// 对工具输出应用脱敏（content 替换 + 实际发生替换时追加
/// [`MASKED_NOTE`] + bytes 重算）。
///
/// 消费点在 core runtime 的工具结果形成处（顺序/并行两条路径共用），
/// 位于 output-size 上限之后、`tool_end` preview / 消息历史 / tracing
/// preview 形成之前——三处模型与日志可见面一次全覆盖。提示长度为固定
/// 小常数（~230B），在 size 上限之后追加不会破坏其防灌爆语义。
pub fn mask_tool_output(mut output: crate::types::ToolOutput) -> crate::types::ToolOutput {
    let mut masked = mask_secrets(&output.content);
    if masked != output.content {
        // 换行分隔防与末行粘连；内容已以换行结尾则不再加空行。
        if !masked.ends_with('\n') {
            masked.push('\n');
        }
        masked.push_str(MASKED_NOTE);
        output.bytes = masked.len();
        output.content = masked;
    }
    output
}

/// 消息历史手术错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AskUserError {
    /// 任何 assistant 消息都未声明该 tool call。
    #[error("tool call '{0}' not found in message history")]
    ToolCallNotFound(String),
    /// 该 tool call 已有结果（幂等首答者赢——迟到者应收到拒绝而非覆盖）。
    #[error("tool call '{0}' already has a result (first answer wins)")]
    AlreadyAnswered(String),
}

/// 悬挂修复的关卷原因（决定 ask_user 的合成文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DanglingClosure {
    /// 新 turn 开始前被新用户消息取代。
    Superseded,
    /// 问卷 TTL 过期（mobile 持久层调用）。
    Expired,
}

impl DanglingClosure {
    /// ask_user 调用在该关卷原因下的合成文案。
    fn ask_user_content(self) -> &'static str {
        match self {
            Self::Superseded => SUPERSEDED_RESULT,
            Self::Expired => EXPIRED_RESULT,
        }
    }
}

/// 合成 tool result 消息（与 runtime 循环内 `tool_msg` 同形：role=tool、
/// `tool_call_id` / `name` 对应原调用）。
fn synthetic_tool_message(tool_call_id: &str, name: &str, content: &str) -> Message {
    Message {
        role: MessageRole::Tool,
        content: content.to_owned(),
        tool_call_id: Some(ToolCallId(tool_call_id.to_owned())),
        name: Some(name.to_owned()),
        tool_calls: None,
        reasoning_content: None,
    }
}

/// assistant 消息 `idx` 之后连续 Tool 结果段的结束下标（ exclusive；
/// 即合成结果的合法插入位置）。
fn result_run_end(messages: &[Message], assistant_idx: usize) -> usize {
    let mut end = assistant_idx + 1;
    while end < messages.len() && messages[end].role == MessageRole::Tool {
        end += 1;
    }
    end
}

/// `messages[assistant_idx + 1 .. run_end]` 段内已落地的 tool_call_id 集。
fn resolved_ids<'a>(
    messages: &'a [Message],
    assistant_idx: usize,
    run_end: usize,
) -> HashSet<&'a str> {
    messages[assistant_idx + 1..run_end]
        .iter()
        .filter_map(|m| m.tool_call_id.as_ref().map(|id| id.0.as_str()))
        .collect()
}

/// 消息历史中的悬挂 tool call（声明了但未得到结果）。返回
/// `(tool_call_id, tool_name)`，按声明顺序。
pub fn dangling_tool_calls(messages: &[Message]) -> Vec<(String, String)> {
    let mut dangling = Vec::new();
    for (idx, msg) in messages.iter().enumerate() {
        let Some(tool_calls) = &msg.tool_calls else {
            continue;
        };
        let run_end = result_run_end(messages, idx);
        let resolved = resolved_ids(messages, idx, run_end);
        for tc in tool_calls {
            if !resolved.contains(tc.id.0.as_str()) {
                dangling.push((tc.id.0.clone(), tc.name.clone()));
            }
        }
    }
    dangling
}

/// 迟到答案补全：为 `tool_call_id` 在消息历史中追加 tool result。
///
/// 插入位置 = 声明该调用的 assistant 消息之后的连续 Tool 结果段末尾
/// （常态即历史尾部——悬挂只在 turn 中断处产生）；这保证无论悬挂在
/// 尾部还是批次中部（兄弟调用已有结果），插入后消息序都合法。
/// 返回合成出的消息（含工具名，供调用方审计/续跑）。
pub fn append_late_tool_result(
    messages: &mut Vec<Message>,
    tool_call_id: &str,
    content: &str,
) -> Result<Message, AskUserError> {
    // 已有结果 → 幂等拒绝（首答者赢）。再找不到声明处 → Not Found。
    for (idx, msg) in messages.iter().enumerate() {
        if msg.tool_calls.is_none() {
            continue;
        }
        let run_end = result_run_end(messages, idx);
        if resolved_ids(messages, idx, run_end).contains(tool_call_id) {
            return Err(AskUserError::AlreadyAnswered(tool_call_id.to_owned()));
        }
        let Some(tool_calls) = &msg.tool_calls else {
            continue;
        };
        if let Some(tc) = tool_calls.iter().find(|tc| tc.id.0 == tool_call_id) {
            let inserted = synthetic_tool_message(tool_call_id, &tc.name, content);
            messages.insert(run_end, inserted.clone());
            return Ok(inserted);
        }
    }
    Err(AskUserError::ToolCallNotFound(tool_call_id.to_owned()))
}

/// 悬挂修复：为**所有**悬挂 tool call 合成结果并插入合法位置。
///
/// - ask_user 调用 → 按 [`DanglingClosure`] 取 superseded / expired 文案；
/// - 同批兄弟（非 ask_user）→ [`INTERRUPTED_RESULT`] 兜底——否则修复了
///   ask_user 仍留下别的悬挂，消息序依旧非法。
///
/// 返回合成出的消息（按历史顺序），供调用方持久化（如
/// `RunRecorder::write_message`）。修复后
/// `dangling_tool_calls(messages)` 必为空（测试钉死该不变量）。
pub fn repair_dangling_tool_calls(
    messages: &mut Vec<Message>,
    closure: DanglingClosure,
) -> Vec<Message> {
    // 先收集插入计划（位置 + 消息），再按位置降序插入，避免下标失效。
    let mut plan: Vec<(usize, Message)> = Vec::new();
    for (idx, msg) in messages.iter().enumerate() {
        let Some(tool_calls) = &msg.tool_calls else {
            continue;
        };
        let run_end = result_run_end(messages, idx);
        let resolved = resolved_ids(messages, idx, run_end);
        for tc in tool_calls {
            if resolved.contains(tc.id.0.as_str()) {
                continue;
            }
            let content = if tc.name == ASK_USER_TOOL {
                closure.ask_user_content()
            } else {
                INTERRUPTED_RESULT
            };
            plan.push((
                run_end,
                synthetic_tool_message(&tc.id.0, &tc.name, content),
            ));
        }
    }
    let mut repaired = Vec::with_capacity(plan.len());
    for (pos, msg) in plan.into_iter().rev() {
        messages.insert(pos, msg.clone());
        repaired.push(msg);
    }
    repaired.reverse(); // 恢复历史顺序。
    repaired
}

/// 脱敏注册表相关测试的串行锁：本模块的
/// `masker_registry_passthrough_and_replace` 与 runtime.rs 的
/// `test_tool_output_masked_before_history` 都会改写进程级注册表，
/// 并行必互相踩（分别持锁即可）。
#[cfg(test)]
pub(crate) static MASKER_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, MessageRole, ToolCall, ToolCallId};

    fn user(text: &str) -> Message {
        Message {
            role: MessageRole::User,
            content: text.into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        }
    }

    fn tool_result(id: &str, name: &str, content: &str) -> Message {
        Message {
            role: MessageRole::Tool,
            content: content.into(),
            tool_call_id: Some(ToolCallId(id.into())),
            name: Some(name.into()),
            tool_calls: None,
            reasoning_content: None,
        }
    }

    /// assistant 消息声明一组工具调用（content 置空，与 runtime 循环一致）。
    fn assistant_with_calls(calls: &[(&str, &str)]) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            name: None,
            tool_calls: Some(
                calls
                    .iter()
                    .map(|(id, name)| ToolCall {
                        id: ToolCallId((*id).into()),
                        name: (*name).into(),
                        arguments: serde_json::json!({}),
                    })
                    .collect(),
            ),
            reasoning_content: None,
        }
    }

    #[test]
    fn dangling_detection_tail_mid_and_none() {
        // 尾部悬挂：assistant(ask_user) 无结果。
        let history = vec![
            user("go"),
            assistant_with_calls(&[("tc-1", "ask_user")]),
        ];
        assert_eq!(
            dangling_tool_calls(&history),
            vec![("tc-1".into(), "ask_user".into())]
        );

        // 批次中部悬挂：echo 有结果、ask_user 没有。
        let history = vec![
            user("go"),
            assistant_with_calls(&[("tc-1", "echo"), ("tc-2", "ask_user")]),
            tool_result("tc-1", "echo", "ok"),
        ];
        assert_eq!(
            dangling_tool_calls(&history),
            vec![("tc-2".into(), "ask_user".into())]
        );

        // 无悬挂。
        let history = vec![
            user("go"),
            assistant_with_calls(&[("tc-1", "ask_user")]),
            tool_result("tc-1", "ask_user", "answered"),
            user("next"),
        ];
        assert!(dangling_tool_calls(&history).is_empty());
    }

    #[test]
    fn append_late_result_at_tail_shapes_message() {
        let mut history = vec![
            user("go"),
            assistant_with_calls(&[("tc-7", "ask_user")]),
        ];
        let msg = append_late_tool_result(&mut history, "tc-7", "User answered: SQLite")
            .expect("tail dangling completes");
        assert_eq!(msg.name.as_deref(), Some("ask_user"));
        assert_eq!(history.len(), 3);
        let last = history.last().unwrap();
        assert_eq!(last.role, MessageRole::Tool);
        assert_eq!(last.tool_call_id, Some(ToolCallId("tc-7".into())));
        assert_eq!(last.name.as_deref(), Some("ask_user"));
        assert_eq!(last.content, "User answered: SQLite");
        assert!(dangling_tool_calls(&history).is_empty());
    }

    #[test]
    fn append_late_result_mid_batch_inserts_after_sibling_results() {
        // ask_user 悬挂在批次中部（echo 已有结果）且历史已继续——插入
        // 位置必须在 echo 结果之后、后续 user 消息之前，保证消息序合法。
        let mut history = vec![
            user("go"),
            assistant_with_calls(&[("tc-1", "echo"), ("tc-2", "ask_user")]),
            tool_result("tc-1", "echo", "ok"),
            user("late turn message"),
        ];
        append_late_tool_result(&mut history, "tc-2", "late answer")
            .expect("mid-batch dangling completes");
        assert_eq!(history.len(), 5);
        assert_eq!(history[3].role, MessageRole::Tool);
        assert_eq!(history[3].tool_call_id, Some(ToolCallId("tc-2".into())));
        assert_eq!(history[4].content, "late turn message");
        assert!(dangling_tool_calls(&history).is_empty());
    }

    #[test]
    fn append_late_result_idempotent_and_not_found() {
        let mut history = vec![
            user("go"),
            assistant_with_calls(&[("tc-1", "ask_user")]),
            tool_result("tc-1", "ask_user", "first answer"),
        ];
        match append_late_tool_result(&mut history, "tc-1", "second answer") {
            Err(AskUserError::AlreadyAnswered(id)) => assert_eq!(id, "tc-1"),
            other => panic!("expected AlreadyAnswered, got {other:?}"),
        }
        match append_late_tool_result(&mut history, "ghost", "x") {
            Err(AskUserError::ToolCallNotFound(id)) => assert_eq!(id, "ghost"),
            other => panic!("expected ToolCallNotFound, got {other:?}"),
        }
        // 失败路径不得改动历史。
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn repair_superseded_and_expired_texts() {
        let mut history = vec![
            user("first turn"),
            assistant_with_calls(&[("tc-1", "ask_user")]),
        ];
        let repaired = repair_dangling_tool_calls(&mut history, DanglingClosure::Superseded);
        assert_eq!(repaired.len(), 1);
        assert_eq!(repaired[0].content, SUPERSEDED_RESULT);
        assert!(dangling_tool_calls(&history).is_empty());

        let mut history = vec![
            user("first turn"),
            assistant_with_calls(&[("tc-1", "ask_user")]),
        ];
        let repaired = repair_dangling_tool_calls(&mut history, DanglingClosure::Expired);
        assert_eq!(repaired[0].content, EXPIRED_RESULT);
        assert!(dangling_tool_calls(&history).is_empty());
    }

    #[test]
    fn repair_fills_non_ask_user_siblings_and_keeps_order() {
        // 进程中断导致整批悬挂：echo 兜底文案 + ask_user 关卷文案，
        // 全部插入在 assistant 之后（echo 先声明先插入）。
        let mut history = vec![
            user("go"),
            assistant_with_calls(&[("tc-1", "echo"), ("tc-2", "ask_user")]),
        ];
        let repaired = repair_dangling_tool_calls(&mut history, DanglingClosure::Superseded);
        assert_eq!(repaired.len(), 2);
        assert_eq!(repaired[0].name.as_deref(), Some("echo"));
        assert_eq!(repaired[0].content, INTERRUPTED_RESULT);
        assert_eq!(repaired[1].name.as_deref(), Some("ask_user"));
        assert_eq!(repaired[1].content, SUPERSEDED_RESULT);
        // 历史形状：user → assistant → tool(echo) → tool(ask_user)。
        assert_eq!(history.len(), 4);
        assert_eq!(history[2].tool_call_id, Some(ToolCallId("tc-1".into())));
        assert_eq!(history[3].tool_call_id, Some(ToolCallId("tc-2".into())));
        assert!(dangling_tool_calls(&history).is_empty());
    }

    #[test]
    fn repair_noop_on_clean_history() {
        let mut history = vec![
            user("go"),
            assistant_with_calls(&[("tc-1", "ask_user")]),
            tool_result("tc-1", "ask_user", "answered"),
        ];
        assert!(repair_dangling_tool_calls(&mut history, DanglingClosure::Expired).is_empty());
        assert_eq!(history.len(), 3);
    }

    // ── 进程级输出脱敏注册表 ─────────────────────────────────────────
    //
    // 全局注册表测试：断言后立即复位（None），不干扰其他测试。

    #[test]
    fn masker_registry_passthrough_and_replace() {
        let _guard = super::MASKER_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        // 未注册 → 恒等。
        set_secret_masker(None);
        assert_eq!(mask_secrets("raw sk-123456"), "raw sk-123456");

        // 整体替换 → 子串替换语义由闭包决定（vault 侧实现）。
        set_secret_masker(Some(std::sync::Arc::new(|text: &str| {
            text.replace("sk-123456", "***")
        })));
        assert_eq!(mask_secrets("token=sk-123456 ok"), "token=*** ok");

        // 追加叠加（mobile 装配期 push；并行多 runtime 互不覆盖）。
        push_secret_masker(std::sync::Arc::new(|text: &str| text.replace("pw-987", "###")));
        assert_eq!(mask_secrets("a sk-123456 b pw-987"), "a *** b ###");

        // ToolOutput 包装：content 替换 + 内部提示追加 + bytes 重算。
        let output = crate::types::ToolOutput {
            bytes: 100,
            content: "leak sk-123456".to_owned(),
            duration_ms: 1,
            status: crate::types::ToolOutputStatus::Success,
        };
        let masked = mask_tool_output(output);
        assert!(masked.content.starts_with("leak ***"), "{}", masked.content);
        assert!(
            masked.content.contains("<system-note>Output masked:"),
            "实际发生替换 → 追加内部提示: {}",
            masked.content
        );
        assert!(
            masked.content.matches("<system-note>").count() == 1,
            "提示恰好一次: {}",
            masked.content
        );
        assert_eq!(masked.bytes, masked.content.len(), "bytes 随终稿 content 重算");

        // 无替换 → 原样返回（不追加提示，避免噪音）。
        let clean = crate::types::ToolOutput {
            bytes: 5,
            content: "nothing secret".to_owned(),
            duration_ms: 1,
            status: crate::types::ToolOutputStatus::Success,
        };
        let clean = mask_tool_output(clean);
        assert_eq!(clean.content, "nothing secret");
        assert!(!clean.content.contains("<system-note>"));

        set_secret_masker(None);
        assert_eq!(mask_secrets("again sk-123456"), "again sk-123456", "清空后恒等");
    }
}
