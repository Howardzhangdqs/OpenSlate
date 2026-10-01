use super::find_char_boundary;
use crate::types::{Message, MessageRole};
use std::future::Future;

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

/// Replace older messages with a summary, keeping the most recent
/// [`KEEP_RECENT_COUNT`] messages.
///
/// `summarize` is async so callers can route it through a real (fast) model;
/// when it resolves to `None` — alias missing, provider error, empty reply —
/// a mechanical concatenation of the tail of the older messages is used
/// instead (degraded-but-working fallback, never an error).
///
/// # In-memory vs. on-disk history — intentional fork
///
/// After a compact, the in-memory history is the summarized version while
/// any persisted transcript keeps the full, uncompressed messages. This is
/// BY DESIGN, not a bug: `/resume` restores the uncompressed full history,
/// trading a larger context for zero information loss across restarts.
pub async fn compact<F, Fut>(
    messages: &mut Vec<Message>,
    system_prompt: Option<&str>,
    max_context_messages: usize,
    max_context_bytes: usize,
    summarize: F,
) -> CompactResult
where
    F: FnOnce(&str) -> Fut,
    Fut: Future<Output = Option<String>>,
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

    // `conversation_text` is an owned local, so the returned future is free
    // to outlive the `&str` (and never borrows `messages`): the closure
    // copies the text before awaiting.
    let summary = summarize(&conversation_text).await.unwrap_or_else(|| {
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
        tool_calls: None,
        reasoning_content: None,
    };

    let recent = messages.split_off(split_point);
    *messages = vec![summary_msg];
    messages.extend(recent);

    let system_bytes = system_prompt.map(|s| s.len()).unwrap_or(0);

    // Enforce the byte budget: shrink the summary message itself first
    // (char-boundary safe), and only then drop the oldest of the kept
    // messages so the most recent ones survive the longest. Re-check
    // after every step until the budget is met or nothing is left to trim.
    let mut summary_end = summary.len();
    loop {
        let total: usize = system_bytes + messages.iter().map(|m| m.content.len()).sum::<usize>();
        if total <= max_context_bytes {
            break;
        }

        let rest_bytes: usize =
            system_bytes + messages[1..].iter().map(|m| m.content.len()).sum::<usize>();
        let budget = max_context_bytes
            .saturating_sub(rest_bytes + SUMMARY_PREFIX.len() + SUMMARY_SUFFIX.len());

        if budget < summary_end {
            // `total > max` implies `budget < summary_end` unless the budget
            // saturated, so this strictly shrinks the summary each round.
            summary_end = find_char_boundary(&summary, budget);
            messages[0].content = format!(
                "{}{}{}",
                SUMMARY_PREFIX,
                &summary[..summary_end],
                SUMMARY_SUFFIX
            );
        } else if messages.len() > 1 {
            // Even an empty summary leaves no room: drop the oldest kept
            // message, keeping the newest ones.
            messages.remove(1);
        } else {
            // Only the minimal summary remains — nothing left to trim.
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
                        tool_calls: None,
                        reasoning_content: None,
                    },
                    Message {
                        role: MessageRole::Assistant,
                        content: format!("assistant msg {}", i),
                        tool_call_id: None,
                        name: None,
                        tool_calls: None,
                        reasoning_content: None,
                    },
                ]
            })
            .collect()
    }

    #[tokio::test]
    async fn compact_replaces_older_with_summary() {
        let mut msgs = make_messages(5);
        let result = compact(&mut msgs, None, 100, 1_000_000, |_text| async {
            Some("summary of conversation".to_owned())
        })
        .await;

        assert_eq!(result.messages_before, 10);
        assert_eq!(result.messages_after, 3);
        assert_eq!(msgs[0].name.as_deref(), Some("compact"));
        assert!(msgs[0].content.contains("summary of conversation"));
        assert_eq!(msgs[1].role, MessageRole::User);
        assert!(msgs[1].content.contains("user msg 4"));
    }

    #[tokio::test]
    async fn compact_uses_async_summarize_future() {
        // The summarize callback resolves a real future (with an actual
        // await point) and receives the formatted older conversation.
        let mut msgs = make_messages(3);
        let result = compact(&mut msgs, None, 100, 1_000_000, |text: &str| {
            let text = text.to_owned();
            async move {
                tokio::task::yield_now().await;
                assert!(text.contains("User: user msg 0"));
                assert!(text.contains("Assistant: assistant msg 1"));
                Some("async summary".to_owned())
            }
        })
        .await;

        assert_eq!(result.messages_before, 6);
        assert_eq!(result.messages_after, 3);
        assert!(msgs[0].content.contains("async summary"));
        assert_eq!(msgs[1].content, "user msg 2");
        assert_eq!(msgs[2].content, "assistant msg 2");
    }

    #[tokio::test]
    async fn compact_preserves_system_prompt_and_recent() {
        let mut msgs = make_messages(3);
        let result = compact(
            &mut msgs,
            Some("system prompt"),
            100,
            1_000_000,
            |_text| async { Some("summarized".to_owned()) },
        )
        .await;

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
            tool_calls: None,
            reasoning_content: None,
        }];
        assert!(needs_compact(&msgs, 100, 1000, 0));
    }

    #[test]
    fn needs_compact_returns_false_when_well_within_limits() {
        let msgs = make_messages(2);
        assert!(!needs_compact(&msgs, 100, 1_000_000, 0));
    }

    #[tokio::test]
    async fn fallback_truncation_when_summarize_fails() {
        let mut msgs = make_messages(4);
        let result = compact(&mut msgs, None, 100, 1_000_000, |_text| async { None }).await;

        assert_eq!(result.messages_before, 8);
        assert!(result.messages_after >= 3);
        assert_eq!(msgs[0].name.as_deref(), Some("compact"));
        assert!(msgs[0].content.contains("User:") || msgs[0].content.contains("Assistant:"));
    }

    #[tokio::test]
    async fn compact_with_few_messages_is_noop() {
        let mut msgs = make_messages(1);
        let result = compact(&mut msgs, None, 100, 1_000_000, |_text| async {
            panic!("should not be called")
        })
        .await;

        assert_eq!(result.messages_before, 2);
        assert_eq!(result.messages_after, 2);
    }

    #[tokio::test]
    async fn compact_respects_byte_limit() {
        let mut msgs: Vec<Message> = (0..20)
            .map(|i| Message {
                role: MessageRole::User,
                content: format!("message {} with some padding text here", i),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            })
            .collect();

        let result = compact(&mut msgs, None, 100, 200, |_text| async {
            Some("short summary".to_owned())
        })
        .await;

        assert!(result.messages_after <= result.messages_before);
        let total: usize = msgs.iter().map(|m| m.content.len()).sum();
        assert!(total <= 200, "total bytes {} exceeds 200", total);
    }

    #[tokio::test]
    async fn compact_oversized_summary_converges_and_keeps_newest() {
        // Summary far larger than the whole byte budget (3000 bytes of
        // multi-byte content — also exercises char-boundary truncation).
        let mut msgs = make_messages(3);
        let result = compact(&mut msgs, None, 100, 400, |_text| async {
            Some("中".repeat(1000))
        })
        .await;

        assert_eq!(result.messages_before, 6);
        let total: usize = msgs.iter().map(|m| m.content.len()).sum();
        assert!(total <= 400, "total bytes {} exceeds 400", total);
        // The summary itself was truncated, not the recent messages:
        // both newest messages survive verbatim.
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1].content, "user msg 2");
        assert_eq!(msgs[2].content, "assistant msg 2");
        assert_eq!(msgs[0].name.as_deref(), Some("compact"));
        assert!(msgs[0].content.starts_with(SUMMARY_PREFIX));
        assert!(msgs[0].content.ends_with(SUMMARY_SUFFIX));
        // Truncation kept whole characters (no mid-character slicing).
        let body = msgs[0]
            .content
            .strip_prefix(SUMMARY_PREFIX)
            .and_then(|s| s.strip_suffix(SUMMARY_SUFFIX))
            .unwrap();
        assert!(body.chars().all(|c| c == '中'));
    }

    #[tokio::test]
    async fn compact_byte_limit_drops_oldest_kept_first() {
        // The older of the two kept messages is huge; the budget cannot
        // hold it, so it must be dropped while the newest survives
        // (dropping from the newest end would lose "newest message").
        let big = "x".repeat(300);
        let mut msgs = vec![
            Message {
                role: MessageRole::User,
                content: "old one".to_owned(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "old two".to_owned(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            },
            Message {
                role: MessageRole::User,
                content: big,
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "newest message".to_owned(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            },
        ];
        let result = compact(&mut msgs, None, 100, 100, |_text| async {
            Some("tiny summary".to_owned())
        })
        .await;

        assert_eq!(result.messages_before, 4);
        let total: usize = msgs.iter().map(|m| m.content.len()).sum();
        assert!(total <= 100, "total bytes {} exceeds 100", total);
        // Newest message kept, oversized older-kept message dropped.
        assert_eq!(msgs.last().unwrap().content, "newest message");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].name.as_deref(), Some("compact"));
    }
}
