use crate::types::{Message, MessageRole};

const COMPACT_NAME: &str = "compact";
const SUMMARY_PREFIX: &str = "[Conversation summary: ";
const SUMMARY_SUFFIX: &str = "]";
const KEEP_RECENT_COUNT: usize = 2;
const THRESHOLD_RATIO: f64 = 0.8;

pub struct CompactResult {
    pub messages_before: usize,
    pub messages_after: usize,
}

pub fn needs_compact(
    messages: &[Message],
    max_context_messages: usize,
    max_context_bytes: usize,
    system_prompt_bytes: usize,
) -> bool {
    let msg_usage = messages.len() as f64 / max_context_messages as f64;
    let total_bytes = system_prompt_bytes + messages.iter().map(|m| m.content.len()).sum::<usize>();
    let byte_usage = total_bytes as f64 / max_context_bytes as f64;
    msg_usage > THRESHOLD_RATIO || byte_usage > THRESHOLD_RATIO
}

pub fn compact<F>(
    messages: &mut Vec<Message>,
    system_prompt: Option<&str>,
    max_context_messages: usize,
    max_context_bytes: usize,
    summarize: F,
) -> CompactResult
where
    F: FnOnce(&str) -> Option<String>,
{
    let messages_before = messages.len();

    if messages_before <= KEEP_RECENT_COUNT {
        return CompactResult {
            messages_before,
            messages_after: messages_before,
        };
    }

    let split_point = messages.len().saturating_sub(KEEP_RECENT_COUNT);
    let older = &messages[..split_point];

    let conversation_text = format_older_messages(older);

    let summary = summarize(&conversation_text).unwrap_or_else(|| {
        let keep = std::cmp::min(max_context_messages, split_point);
        let start = split_point.saturating_sub(keep);
        older[start..]
            .iter()
            .map(|m| {
                let role = match m.role {
                    MessageRole::User => "User",
                    MessageRole::Assistant => "Assistant",
                    MessageRole::Tool => "Tool",
                    MessageRole::System => "System",
                };
                format!("{}: {}", role, m.content)
            })
            .collect::<Vec<_>>()
            .join("\n")
    });

    let summary_msg = Message {
        role: MessageRole::System,
        content: format!("{}{}{}", SUMMARY_PREFIX, summary, SUMMARY_SUFFIX),
        tool_call_id: None,
        name: Some(COMPACT_NAME.to_owned()),
    };

    let recent = messages.split_off(split_point);
    *messages = vec![summary_msg];
    messages.extend(recent);

    let system_bytes = system_prompt.map(|s| s.len()).unwrap_or(0);
    let mut total: usize = system_bytes;
    for msg in messages.iter() {
        total += msg.content.len();
        if total > max_context_bytes {
            let excess_msg_idx = messages.len() - 1;
            if excess_msg_idx > 0 {
                messages.truncate(excess_msg_idx);
            }
            break;
        }
    }

    CompactResult {
        messages_before,
        messages_after: messages.len(),
    }
}

fn format_older_messages(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| {
            let role = match m.role {
                MessageRole::User => "User",
                MessageRole::Assistant => "Assistant",
                MessageRole::Tool => "Tool",
                MessageRole::System => "System",
            };
            format!("{}: {}", role, m.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MessageRole;

    fn make_messages(n: usize) -> Vec<Message> {
        (0..n)
            .flat_map(|i| {
                vec![
                    Message {
                        role: MessageRole::User,
                        content: format!("user msg {}", i),
                        tool_call_id: None,
                        name: None,
                    },
                    Message {
                        role: MessageRole::Assistant,
                        content: format!("assistant msg {}", i),
                        tool_call_id: None,
                        name: None,
                    },
                ]
            })
            .collect()
    }

    #[test]
    fn compact_replaces_older_with_summary() {
        let mut msgs = make_messages(5);
        let result = compact(&mut msgs, None, 100, 1_000_000, |_text| {
            Some("summary of conversation".to_owned())
        });

        assert_eq!(result.messages_before, 10);
        assert_eq!(result.messages_after, 3);
        assert_eq!(msgs[0].name.as_deref(), Some("compact"));
        assert!(msgs[0].content.contains("summary of conversation"));
        assert_eq!(msgs[1].role, MessageRole::User);
        assert!(msgs[1].content.contains("user msg 4"));
    }

    #[test]
    fn compact_preserves_system_prompt_and_recent() {
        let mut msgs = make_messages(3);
        let result = compact(&mut msgs, Some("system prompt"), 100, 1_000_000, |_text| {
            Some("summarized".to_owned())
        });

        assert_eq!(result.messages_before, 6);
        assert_eq!(result.messages_after, 3);

        assert_eq!(msgs[0].role, MessageRole::System);
        assert!(msgs[0].content.contains("summarized"));
        assert_eq!(msgs[0].name.as_deref(), Some("compact"));

        assert_eq!(msgs[1].content, "user msg 2");
        assert_eq!(msgs[2].content, "assistant msg 2");
    }

    #[test]
    fn needs_compact_returns_true_near_message_limit() {
        let msgs = make_messages(8);
        assert!(needs_compact(&msgs, 10, 1_000_000, 0));
    }

    #[test]
    fn needs_compact_returns_true_near_byte_limit() {
        let msgs = vec![Message {
            role: MessageRole::User,
            content: "a".repeat(900),
            tool_call_id: None,
            name: None,
        }];
        assert!(needs_compact(&msgs, 100, 1000, 0));
    }

    #[test]
    fn needs_compact_returns_false_when_well_within_limits() {
        let msgs = make_messages(2);
        assert!(!needs_compact(&msgs, 100, 1_000_000, 0));
    }

    #[test]
    fn fallback_truncation_when_summarize_fails() {
        let mut msgs = make_messages(4);
        let result = compact(&mut msgs, None, 100, 1_000_000, |_text| None);

        assert_eq!(result.messages_before, 8);
        assert!(result.messages_after >= 3);
        assert_eq!(msgs[0].name.as_deref(), Some("compact"));
        assert!(msgs[0].content.contains("User:") || msgs[0].content.contains("Assistant:"));
    }

    #[test]
    fn compact_with_few_messages_is_noop() {
        let mut msgs = make_messages(1);
        let result = compact(&mut msgs, None, 100, 1_000_000, |_text| {
            panic!("should not be called")
        });

        assert_eq!(result.messages_before, 2);
        assert_eq!(result.messages_after, 2);
    }

    #[test]
    fn compact_respects_byte_limit() {
        let mut msgs: Vec<Message> = (0..20)
            .map(|i| Message {
                role: MessageRole::User,
                content: format!("message {} with some padding text here", i),
                tool_call_id: None,
                name: None,
            })
            .collect();

        let result = compact(&mut msgs, None, 100, 200, |_text| {
            Some("short summary".to_owned())
        });

        assert!(result.messages_after <= result.messages_before);
        let total: usize = msgs.iter().map(|m| m.content.len()).sum();
        assert!(total <= 200, "total bytes {} exceeds 200", total);
    }
}
