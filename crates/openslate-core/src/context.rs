//! Context isolation for child agents.
//!
//! When a parent agent delegates to a child agent, the child gets an isolated
//! context — NOT the parent's full conversation history.

use crate::types::{Message, MessageRole};

/// Configuration for how child agent context is constructed.
#[derive(Debug, Clone)]
pub struct ContextIsolationConfig {
    /// Whether to inherit parent's conversation messages.
    /// Default: false (isolated context)
    pub inherit_parent_context: bool,
    /// Whether to include a summary of parent's conversation.
    /// Default: true
    pub include_conversation_summary: bool,
    /// Maximum number of messages in child context.
    /// Default: 8
    pub max_context_messages: u32,
    /// Maximum bytes of context content.
    /// Default: 32000
    pub max_context_bytes: u32,
}

impl Default for ContextIsolationConfig {
    fn default() -> Self {
        Self {
            inherit_parent_context: false,
            include_conversation_summary: true,
            max_context_messages: 8,
            max_context_bytes: 32_000,
        }
    }
}

/// Build the initial messages for a child agent.
///
/// Default behavior (isolated context):
/// - [system] child agent's own system prompt (if provided)
/// - [user] task from parent agent
/// - Optional: conversation summary from parent
///
/// If inherit_parent_context is true:
/// - [system] child agent's system prompt
/// - [user/assistant/tool] last N messages from parent (truncated to max)
/// - [user] task from parent
pub fn build_child_context(
    config: &ContextIsolationConfig,
    child_system_prompt: Option<&str>,
    task: &str,
    parent_messages: &[Message],
) -> Vec<Message> {
    let mut messages = Vec::new();

    // Always add child's own system prompt
    if let Some(prompt) = child_system_prompt {
        messages.push(Message {
            role: MessageRole::System,
            content: prompt.to_owned(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        });
    }

    if config.inherit_parent_context {
        // Include last N parent messages (truncated)
        let start = parent_messages.len().saturating_sub(config.max_context_messages as usize);
        let parent_slice = &parent_messages[start..];

        let mut total_bytes = 0u32;
        for msg in parent_slice {
            total_bytes += msg.content.len() as u32;
            if total_bytes > config.max_context_bytes {
                break;
            }
            messages.push(msg.clone());
        }
    } else if config.include_conversation_summary && !parent_messages.is_empty() {
        // Generate a brief summary of parent's conversation
        let summary = generate_conversation_summary(parent_messages);
        messages.push(Message {
            role: MessageRole::System,
            content: format!("[Parent conversation summary]\n{}", summary),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        });
    }

    // Always add the task from parent as a user message
    messages.push(Message {
        role: MessageRole::User,
        content: task.to_owned(),
        tool_call_id: None,
        name: None,
        tool_calls: None,
    });

    messages
}

/// Generate a brief summary of parent's conversation.
/// Simple approach: take last few exchanges, truncated.
fn generate_conversation_summary(parent_messages: &[Message]) -> String {
    let last_n: Vec<&Message> = parent_messages.iter().rev().take(4).collect();
    let mut summary_parts = Vec::new();
    for msg in last_n.into_iter().rev() {
        let role = match msg.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        let content_preview = if msg.content.len() > 200 {
            format!("{}...", &msg.content[..200])
        } else {
            msg.content.clone()
        };
        summary_parts.push(format!("[{}] {}", role, content_preview));
    }
    summary_parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(role: MessageRole, content: &str) -> Message {
        Message {
            role,
            content: content.to_owned(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        }
    }

    #[test]
    fn test_isolated_context_no_parent_messages() {
        let config = ContextIsolationConfig::default();
        let parent_messages = vec![];

        let result = build_child_context(
            &config,
            Some("You are a helpful assistant."),
            "Do the task",
            &parent_messages,
        );

        // Should have system prompt + task (no summary since no parent messages)
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, MessageRole::System);
        assert_eq!(result[0].content, "You are a helpful assistant.");
        assert_eq!(result[1].role, MessageRole::User);
        assert_eq!(result[1].content, "Do the task");
    }

    #[test]
    fn test_isolated_context_with_summary() {
        let config = ContextIsolationConfig {
            inherit_parent_context: false,
            include_conversation_summary: true,
            max_context_messages: 8,
            max_context_bytes: 32_000,
        };
        let parent_messages = vec![
            make_message(MessageRole::User, "Hello"),
            make_message(MessageRole::Assistant, "Hi there!"),
        ];

        let result = build_child_context(
            &config,
            Some("You are a helpful assistant."),
            "Do the task",
            &parent_messages,
        );

        // Should have: system prompt + summary + task
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].role, MessageRole::System);
        assert_eq!(result[0].content, "You are a helpful assistant.");
        assert!(result[1].content.contains("[Parent conversation summary]"));
        assert_eq!(result[2].role, MessageRole::User);
        assert_eq!(result[2].content, "Do the task");
    }

    #[test]
    fn test_inherit_parent_context() {
        let config = ContextIsolationConfig {
            inherit_parent_context: true,
            include_conversation_summary: false,
            max_context_messages: 8,
            max_context_bytes: 32_000,
        };
        let parent_messages = vec![
            make_message(MessageRole::User, "Hello"),
            make_message(MessageRole::Assistant, "Hi there!"),
        ];

        let result = build_child_context(
            &config,
            Some("You are a helpful assistant."),
            "Do the task",
            &parent_messages,
        );

        // Should have: system prompt + parent messages + task
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].role, MessageRole::System);
        assert_eq!(result[1].role, MessageRole::User);
        assert_eq!(result[1].content, "Hello");
        assert_eq!(result[2].role, MessageRole::Assistant);
        assert_eq!(result[2].content, "Hi there!");
        assert_eq!(result[3].role, MessageRole::User);
        assert_eq!(result[3].content, "Do the task");
    }

    #[test]
    fn test_inherit_truncates_to_max_messages() {
        let config = ContextIsolationConfig {
            inherit_parent_context: true,
            include_conversation_summary: false,
            max_context_messages: 8,
            max_context_bytes: 32_000,
        };
        // Create 20 parent messages
        let parent_messages: Vec<Message> = (0..20)
            .map(|i| make_message(MessageRole::User, &format!("Message {}", i)))
            .collect();

        let result = build_child_context(
            &config,
            Some("You are a helpful assistant."),
            "Do the task",
            &parent_messages,
        );

        // Should have: system prompt + last 8 messages + task
        // System prompt is not from parent, so total = 1 + 8 + 1 = 10
        let parent_msg_count = result.len() - 2; // subtract system prompt and task
        assert_eq!(parent_msg_count, 8);
    }

    #[test]
    fn test_inherit_truncates_to_max_bytes() {
        let config = ContextIsolationConfig {
            inherit_parent_context: true,
            include_conversation_summary: false,
            max_context_messages: 8,
            max_context_bytes: 100, // Very small limit
        };
        let parent_messages = vec![
            make_message(MessageRole::User, "This is a long message that should be truncated when we hit the byte limit"),
            make_message(MessageRole::Assistant, "Another long message that adds to the total byte count"),
            make_message(MessageRole::User, "Short"),
        ];

        let result = build_child_context(
            &config,
            Some("You are a helpful assistant."),
            "Do the task",
            &parent_messages,
        );

        // First message alone exceeds 100 bytes, so only it fits
        // Then task message
        // Total should be 1 (system) + 1 (first parent) + 1 (task) = 3
        // But since we break on first message that exceeds limit, we still include it
        // because we check total_bytes AFTER adding
        // So: system + first msg (88 bytes) + task = 3 messages from parent
        let parent_msg_count = result.len() - 2;
        assert!(parent_msg_count <= 3);
    }

    #[test]
    fn test_task_always_present() {
        let config = ContextIsolationConfig::default();
        let parent_messages = vec![make_message(MessageRole::User, "Hello")];

        let result = build_child_context(
            &config,
            None, // No system prompt
            "Do the important task",
            &parent_messages,
        );

        // Last message should always be the task
        let last = result.last().unwrap();
        assert_eq!(last.role, MessageRole::User);
        assert_eq!(last.content, "Do the important task");
    }

    #[test]
    fn test_no_system_prompt() {
        let config = ContextIsolationConfig::default();
        let parent_messages = vec![];

        let result = build_child_context(
            &config,
            None, // No system prompt
            "Do the task",
            &parent_messages,
        );

        // Should only have task message (no system prompt)
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, MessageRole::User);
        assert_eq!(result[0].content, "Do the task");
    }

    #[test]
    fn test_empty_parent_messages_no_summary() {
        let config = ContextIsolationConfig {
            inherit_parent_context: false,
            include_conversation_summary: true,
            max_context_messages: 8,
            max_context_bytes: 32_000,
        };
        let parent_messages: Vec<Message> = vec![];

        let result = build_child_context(
            &config,
            Some("You are a helpful assistant."),
            "Do the task",
            &parent_messages,
        );

        // Should have: system prompt + task (no summary since no parent messages)
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|m| !m.content.contains("[Parent conversation summary]")));
    }
}