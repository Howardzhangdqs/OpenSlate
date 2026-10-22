//! `project_from_messages` 投影快照测试（存储 v5 §4，L6）。
//!
//! 每个场景钉死投影结果的 JSON 形状（`serde_json::Value` 比对——键名
//! 与值全量钉死；EntryDto 自身的线格式契约在 contract.rs，此处只管
//! 投影语义）。覆盖：8 类 EntryDto 中可由 Message 投影的（User /
//! Assistant / Reasoning / ToolCall）+ 边界（悬挂 tool_calls、空
//! content、附件引用、输出/预览截断、System 跳过）。
//! Approval/Delegate/StepBreak/Meta 不可由消息投影（设计 §4——它们
//! 只存在于 transcript_entries 行），断言其缺席。

use openslate_core::types::{Attachment, Message, MessageRole, ToolCall, ToolCallId};
use openslate_protocol::project_from_messages;
use serde_json::json;

fn msg(role: MessageRole, content: &str) -> Message {
    Message {
        role,
        content: content.to_owned(),
        tool_call_id: None,
        name: None,
        tool_calls: None,
        reasoning_content: None,
        attachments: None,
    }
}

fn assistant_with_call(id: &str, name: &str) -> Message {
    Message {
        role: MessageRole::Assistant,
        content: String::new(),
        tool_call_id: None,
        name: None,
        reasoning_content: None,
        attachments: None,
        tool_calls: Some(vec![ToolCall {
            id: ToolCallId(id.to_owned()),
            name: name.to_owned(),
            arguments: json!({"path": "a.rs"}),
        }]),
    }
}

fn tool_result(id: &str, name: &str, content: &str) -> Message {
    Message {
        role: MessageRole::Tool,
        content: content.to_owned(),
        tool_call_id: Some(ToolCallId(id.to_owned())),
        name: Some(name.to_owned()),
        tool_calls: None,
        reasoning_content: None,
        attachments: None,
    }
}

fn projected(messages: &[Message]) -> serde_json::Value {
    serde_json::to_value(project_from_messages(messages)).unwrap()
}

/// ① User 纯文本；② 附件引用投影（id/mime/filename 引用形状，
/// `data` 永不出现；filename None 跳键）。
#[test]
fn user_projects_with_attachment_refs() {
    let mut with_atts = msg(MessageRole::User, "看图");
    with_atts.attachments = Some(vec![
        Attachment {
            id: "att-1".into(),
            mime: "image/jpeg".into(),
            filename: Some("cat.jpg".into()),
            data: Some(std::sync::Arc::from(&b"bytes"[..])),
        },
        Attachment {
            id: "att-2".into(),
            mime: "video/mp4".into(),
            filename: None,
            data: None,
        },
    ]);
    assert_eq!(
        projected(&[msg(MessageRole::User, "hi"), with_atts]),
        json!([
            {"kind": "user", "text": "hi"},
            {
                "kind": "user",
                "text": "看图",
                "attachments": [
                    {"id": "att-1", "mime": "image/jpeg", "filename": "cat.jpg"},
                    {"id": "att-2", "mime": "video/mp4"},
                ],
            },
        ])
    );
}

/// ③ Assistant：非空 reasoning 先行 → Reasoning 条目，随后 Assistant
/// 正文；空串 reasoning 不产生条目。
#[test]
fn assistant_reasoning_restored_as_entry() {
    let mut assistant = msg(MessageRole::Assistant, "答案");
    assistant.reasoning_content = Some("先想再答".into());
    let mut empty_reasoning = msg(MessageRole::Assistant, "接下来说");
    empty_reasoning.reasoning_content = Some(String::new());
    assert_eq!(
        projected(&[assistant, empty_reasoning]),
        json!([
            {"kind": "reasoning", "text": "先想再答"},
            {"kind": "assistant", "text": "答案"},
            {"kind": "assistant", "text": "接下来说"},
        ])
    );
}

/// ④ ToolCall 完整链路：assistant 带 tool_calls → Tool 消息按
/// call_id 配对折叠，output 回填（detail.args 全文 / 顶层 args 预览）。
#[test]
fn tool_call_folds_output_by_call_id() {
    let messages = vec![
        msg(MessageRole::User, "go"),
        assistant_with_call("t1", "read_file"),
        tool_result("t1", "read_file", "file body"),
    ];
    assert_eq!(
        projected(&messages),
        json!([
            {"kind": "user", "text": "go"},
            {
                "kind": "tool_call",
                "name": "read_file",
                "args": "{\"path\":\"a.rs\"}",
                "call_id": "t1",
                "status": {"state": "done", "bytes": 0, "truncated": false, "elapsed_ms": null},
                "detail": {"args": "{\"path\":\"a.rs\"}", "output": "file body"},
            },
        ])
    );
}

/// ⑤ 悬挂 tool_calls（无 Tool 消息跟随，如回合中途崩溃）：条目保留
/// Done 零字节形态，output 为 null。
#[test]
fn dangling_tool_calls_kept_as_done_zero_bytes() {
    let messages = vec![
        msg(MessageRole::User, "go"),
        assistant_with_call("tc-x", "bash"),
    ];
    assert_eq!(
        projected(&messages),
        json!([
            {"kind": "user", "text": "go"},
            {
                "kind": "tool_call",
                "name": "bash",
                "args": "{\"path\":\"a.rs\"}",
                "call_id": "tc-x",
                "status": {"state": "done", "bytes": 0, "truncated": false, "elapsed_ms": null},
                "detail": {"args": "{\"path\":\"a.rs\"}", "output": null},
            },
        ])
    );
}

/// ⑥ Tool 消息无 call_id → 按工具名兜底配对最近的同名 ToolCall；
/// 未知工具名（孤儿结果）静默丢弃。
#[test]
fn tool_result_folds_by_name_when_no_call_id() {
    let mut orphan_name = tool_result("ignored", "other_tool", "?");
    orphan_name.tool_call_id = None;
    let mut by_name = tool_result("ignored", "read_file", "by name");
    by_name.tool_call_id = None;
    let messages = vec![
        msg(MessageRole::User, "go"),
        assistant_with_call("t1", "read_file"),
        orphan_name,
        by_name,
    ];
    let out = projected(&messages);
    assert_eq!(
        out[1]["detail"]["output"],
        json!("by name"),
        "name fallback folds into the latest matching ToolCall: {out}"
    );
    assert_eq!(out.as_array().unwrap().len(), 2, "orphan adds no entry");
}

/// ⑦ 空 content 边界：纯空白 assistant 不产生 Assistant 条目；
/// System 消息整体跳过。
#[test]
fn blank_assistant_and_system_skipped() {
    let messages = vec![
        msg(MessageRole::System, "you are an agent"),
        msg(MessageRole::User, "hi"),
        msg(MessageRole::Assistant, "  \n  "),
    ];
    assert_eq!(
        projected(&messages),
        json!([
            {"kind": "user", "text": "hi"},
        ])
    );
}

/// ⑧ 截断边界：顶层 args 预览 40 字符；折叠 output 2000 字符。
#[test]
fn truncation_bounds_preview_and_output() {
    let long_arg = "x".repeat(300);
    let mut assistant = msg(MessageRole::Assistant, "");
    assistant.tool_calls = Some(vec![ToolCall {
        id: ToolCallId("t-long".into()),
        name: "bash".into(),
        arguments: json!({ "cmd": long_arg }),
    }]);
    let long_output = "y".repeat(2500);
    let messages = vec![assistant, tool_result("t-long", "bash", &long_output)];
    let out = projected(&messages);
    let entries = out.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["args"].as_str().unwrap().chars().count(), 40);
    let full_args = serde_json::to_string(&json!({ "cmd": long_arg })).unwrap();
    assert_eq!(
        entries[0]["detail"]["args"], json!(full_args),
        "full serialized args retained in detail"
    );
    assert_eq!(
        entries[0]["detail"]["output"].as_str().unwrap().chars().count(),
        2000,
        "output capped at 2000 chars"
    );
}

/// Reasoning + tool_calls + 正文同一条 assistant 消息的完整投影
/// （顺序：Reasoning → ToolCall → Assistant）；空入参 → 空转写。
#[test]
fn assistant_full_shape_order_and_empty_input() {
    assert!(project_from_messages(&[]).is_empty());
    let mut assistant = assistant_with_call("t9", "bash");
    assistant.reasoning_content = Some("要不要跑命令".into());
    assistant.content = "跑完了".into();
    let out = projected(&[assistant]);
    let kinds: Vec<&str> = out
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["reasoning", "tool_call", "assistant"]);
}

/// Approval/Delegate/StepBreak/Meta 不可由消息投影——空历史之外的
/// 任何消息组合都不会产生这四类条目（它们只活在 transcript_entries）。
#[test]
fn non_projectable_kinds_never_emitted() {
    let messages = vec![
        msg(MessageRole::User, "go"),
        assistant_with_call("t1", "read_file"),
        tool_result("t1", "read_file", "ok"),
        msg(MessageRole::Assistant, "done"),
    ];
    let out = projected(&messages);
    for e in out.as_array().unwrap() {
        let kind = e["kind"].as_str().unwrap();
        assert!(
            matches!(kind, "user" | "assistant" | "reasoning" | "tool_call"),
            "unexpected projectable kind: {kind}"
        );
    }
}
