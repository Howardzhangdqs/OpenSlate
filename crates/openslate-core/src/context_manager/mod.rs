//! Multi-turn conversation context management.
//!
//! Manages message history accumulation with configurable context window limits.
//! Applies truncation strategy that always preserves the system prompt and keeps
//! the most recent messages when limits are exceeded.
//!
//! Also provides `/compact` context compression: when context nears limits,
//! older messages can be replaced with a model-generated summary, preserving
//! the system prompt and the most recent exchange.

pub mod compact;

use crate::types::{Message, MessageRole, ToolCallId};

pub use compact::{compact, needs_compact, CompactResult};

const TRUNCATED_MARKER: &str = "[TRUNCATED]";

/// Manages conversation context with window limits and truncation strategy.
///
/// The context manager accumulates messages across turns and enforces two limits:
/// - `max_context_messages`: maximum number of non-system messages to keep
/// - `max_context_bytes`: maximum total byte count of all messages
///
/// Truncation strategy:
/// - The system prompt is **always** preserved as the first message
/// - When limits are exceeded, oldest user/assistant/tool messages are dropped first
/// - If a single message exceeds `max_context_bytes`, its content is truncated
///   with a `[TRUNCATED]` marker
#[derive(Debug, Clone)]
pub struct ContextManager {
    /// Accumulated conversation history (excluding system prompt).
    messages: Vec<Message>,
    /// Maximum number of messages to keep (excluding system prompt).
    max_context_messages: usize,
    /// Maximum total bytes across all messages (including system prompt).
    max_context_bytes: usize,
    /// System prompt, always preserved as the first message in context.
    system_prompt: Option<String>,
}

impl ContextManager {
    /// Create a new `ContextManager` with the given limits.
    pub fn new(
        max_context_messages: usize,
        max_context_bytes: usize,
        system_prompt: Option<String>,
    ) -> Self {
        Self {
            messages: Vec::new(),
            max_context_messages,
            max_context_bytes,
            system_prompt,
        }
    }

    /// Create a `ContextManager` from the default config limits.
    pub fn from_config(
        max_context_messages: u32,
        max_context_bytes: u32,
        system_prompt: Option<String>,
    ) -> Self {
        Self::new(
            max_context_messages as usize,
            max_context_bytes as usize,
            system_prompt,
        )
    }

    /// Append a user message to the history.
    pub fn add_user_message(&mut self, content: impl Into<String>) {
        self.messages.push(Message {
            role: MessageRole::User,
            content: content.into(),
            tool_call_id: None,
            name: None,
        });
    }

    /// Append an assistant message to the history.
    pub fn add_assistant_message(&mut self, content: impl Into<String>) {
        self.messages.push(Message {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_call_id: None,
            name: None,
        });
    }

    /// Append a tool result message to the history.
    pub fn add_tool_message(&mut self, content: impl Into<String>, tool_call_id: ToolCallId) {
        self.messages.push(Message {
            role: MessageRole::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id),
            name: None,
        });
    }

    /// Append a tool result message with an optional tool name.
    pub fn add_tool_message_with_name(
        &mut self,
        content: impl Into<String>,
        tool_call_id: ToolCallId,
        name: Option<String>,
    ) {
        self.messages.push(Message {
            role: MessageRole::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id),
            name,
        });
    }

    /// Return the trimmed context ready for a model call.
    ///
    /// The returned vector:
    /// 1. Always starts with the system prompt (if set)
    /// 2. Contains at most `max_context_messages` messages from history
    /// 3. Total byte count does not exceed `max_context_bytes`
    /// 4. Oldest messages are dropped first when truncating
    pub fn get_context(&self) -> Vec<Message> {
        let mut result = Vec::new();
        let mut total_bytes = 0usize;

        // Always include system prompt first
        if let Some(ref prompt) = self.system_prompt {
            let msg = Message {
                role: MessageRole::System,
                content: prompt.clone(),
                tool_call_id: None,
                name: None,
            };
            total_bytes += msg.content.len();
            result.push(msg);
        }

        if total_bytes > self.max_context_bytes && !result.is_empty() {
            let available = self.max_context_bytes.saturating_sub(TRUNCATED_MARKER.len());
            result[0].content = if available > 0 {
                format!(
                    "{}{}",
                    &prompt_substring(&self.system_prompt, available),
                    TRUNCATED_MARKER
                )
            } else if self.max_context_bytes > 0 {
                let end = find_char_boundary(&self.system_prompt.clone().unwrap_or_default(), self.max_context_bytes);
                self.system_prompt.as_deref().map(|s| s[..end].to_owned()).unwrap_or_default()
            } else {
                String::new()
            };
            return result;
        }

        // Take the most recent N messages
        let start = if self.messages.len() > self.max_context_messages {
            self.messages.len() - self.max_context_messages
        } else {
            0
        };
        let recent = &self.messages[start..];

        // Build from newest to oldest to prioritize recent messages
        let mut temp = Vec::new();
        let mut temp_bytes = 0usize;

        for msg in recent.iter().rev() {
            let msg_bytes = msg.content.len();
            let remaining = self.max_context_bytes.saturating_sub(total_bytes + temp_bytes);

            if msg_bytes <= remaining {
                temp_bytes += msg_bytes;
                temp.push(msg.clone());
            } else if remaining > 0 {
                let truncated = truncate_message(msg, remaining);
                if let Some(t) = truncated {
                    temp_bytes += t.content.len();
                    temp.push(t);
                }
            }
        }

        temp.reverse();
        let _ = total_bytes + temp_bytes;
        result.extend(temp);

        result
    }

    /// Reset all conversation history. Keeps system prompt and limits.
    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Set or update the system prompt.
    pub fn set_system_prompt(&mut self, prompt: Option<String>) {
        self.system_prompt = prompt;
    }

    /// Number of accumulated messages (excluding system prompt).
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Total bytes of all accumulated messages (excluding system prompt).
    pub fn total_bytes(&self) -> usize {
        self.messages.iter().map(|m| m.content.len()).sum()
    }

    /// Get the system prompt, if set.
    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub fn needs_compact(&self) -> bool {
        let system_bytes = self.system_prompt.as_deref().map(|s| s.len()).unwrap_or(0);
        compact::needs_compact(
            &self.messages,
            self.max_context_messages,
            self.max_context_bytes,
            system_bytes,
        )
    }

    pub fn compact<F>(&mut self, summarize: F) -> CompactResult
    where
        F: FnOnce(&str) -> Option<String>,
    {
        compact::compact(
            &mut self.messages,
            self.system_prompt.as_deref(),
            self.max_context_messages,
            self.max_context_bytes,
            summarize,
        )
    }
}

/// Safely extract a substring from an optional string.
fn prompt_substring(opt: &Option<String>, len: usize) -> String {
    match opt {
        Some(s) => {
            let end = find_char_boundary(s, len);
            s[..end].to_owned()
        }
        None => String::new(),
    }
}

/// Find the nearest valid UTF-8 char boundary at or before `byte_index`.
fn find_char_boundary(s: &str, byte_index: usize) -> usize {
    let mut idx = byte_index.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Truncate a message's content to fit within `max_bytes`.
/// Returns None if even a truncated version can't fit.
fn truncate_message(msg: &Message, max_bytes: usize) -> Option<Message> {
    if max_bytes == 0 {
        return None;
    }

    let content = if max_bytes >= TRUNCATED_MARKER.len() {
        let available = max_bytes - TRUNCATED_MARKER.len();
        let end = find_char_boundary(&msg.content, available);
        format!("{}{}", &msg.content[..end], TRUNCATED_MARKER)
    } else {
        let end = find_char_boundary(&msg.content, max_bytes);
        if end == 0 {
            return None;
        }
        msg.content[..end].to_owned()
    };

    Some(Message {
        role: msg.role.clone(),
        content,
        tool_call_id: msg.tool_call_id.clone(),
        name: msg.name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manager(max_msgs: usize, max_bytes: usize) -> ContextManager {
        ContextManager::new(max_msgs, max_bytes, None)
    }

    fn make_manager_with_prompt(max_msgs: usize, max_bytes: usize, prompt: &str) -> ContextManager {
        ContextManager::new(max_msgs, max_bytes, Some(prompt.to_owned()))
    }

    // ── Accumulation tests ──

    #[test]
    fn messages_accumulate_correctly() {
        let mut cm = make_manager(100, 100_000);
        assert_eq!(cm.message_count(), 0);

        cm.add_user_message("Hello");
        assert_eq!(cm.message_count(), 1);

        cm.add_assistant_message("Hi there!");
        assert_eq!(cm.message_count(), 2);

        cm.add_user_message("How are you?");
        assert_eq!(cm.message_count(), 3);

        let ctx = cm.get_context();
        assert_eq!(ctx.len(), 3);
        assert_eq!(ctx[0].role, MessageRole::User);
        assert_eq!(ctx[0].content, "Hello");
        assert_eq!(ctx[1].role, MessageRole::Assistant);
        assert_eq!(ctx[1].content, "Hi there!");
        assert_eq!(ctx[2].role, MessageRole::User);
        assert_eq!(ctx[2].content, "How are you?");
    }

    #[test]
    fn tool_messages_accumulate() {
        let mut cm = make_manager(100, 100_000);
        cm.add_user_message("run tool");
        cm.add_tool_message("result data", ToolCallId::from("tc-1"));

        assert_eq!(cm.message_count(), 2);
        let ctx = cm.get_context();
        assert_eq!(ctx[1].role, MessageRole::Tool);
        assert_eq!(ctx[1].tool_call_id.as_ref().map(|id| id.0.as_str()), Some("tc-1"));
    }

    // ── Truncation by message count ──

    #[test]
    fn truncation_respects_max_context_messages() {
        let mut cm = make_manager(3, 1_000_000);

        // Add 5 messages
        for i in 0..5 {
            cm.add_user_message(format!("msg {}", i));
        }
        assert_eq!(cm.message_count(), 5);

        let ctx = cm.get_context();
        // Should keep only last 3 messages
        assert_eq!(ctx.len(), 3);
        assert_eq!(ctx[0].content, "msg 2");
        assert_eq!(ctx[1].content, "msg 3");
        assert_eq!(ctx[2].content, "msg 4");
    }

    #[test]
    fn truncation_drops_oldest_first() {
        let mut cm = make_manager(2, 1_000_000);

        cm.add_user_message("old");
        cm.add_assistant_message("mid");
        cm.add_user_message("new");

        let ctx = cm.get_context();
        assert_eq!(ctx.len(), 2);
        assert_eq!(ctx[0].content, "mid");
        assert_eq!(ctx[1].content, "new");
    }

    // ── Truncation by byte count ──

    #[test]
    fn truncation_respects_max_context_bytes() {
        let mut cm = make_manager(100, 20);

        cm.add_user_message("0123456789"); // 10 bytes
        cm.add_user_message("abcdefghij"); // 10 bytes
        cm.add_user_message("XYZ"); // 3 bytes

        let ctx = cm.get_context();
        let total: usize = ctx.iter().map(|m| m.content.len()).sum();
        assert!(total <= 20, "total bytes {} exceeds limit 20", total);
        assert!(
            ctx.last().map(|m| m.content.as_str()) == Some("XYZ"),
            "most recent message should be preserved"
        );
    }

    #[test]
    fn byte_limit_keeps_recent_messages() {
        let mut cm = make_manager(100, 15);

        cm.add_user_message("12345"); // 5 bytes
        cm.add_user_message("67890"); // 5 bytes
        cm.add_user_message("abcde"); // 5 bytes — total would be 15 if we include all

        let ctx = cm.get_context();
        // Should keep the most recent that fit
        assert!(
            ctx.iter().any(|m| m.content == "abcde"),
            "should contain most recent message"
        );
    }

    // ── System prompt preservation ──

    #[test]
    fn system_prompt_always_preserved() {
        let mut cm = make_manager_with_prompt(2, 1_000_000, "You are helpful.");

        cm.add_user_message("hello");
        cm.add_assistant_message("hi");
        cm.add_user_message("how are you?");
        cm.add_assistant_message("fine");

        let ctx = cm.get_context();
        // First message must be system prompt
        assert_eq!(ctx[0].role, MessageRole::System);
        assert_eq!(ctx[0].content, "You are helpful.");

        // Total should be 1 (system) + 2 (last 2 messages)
        assert_eq!(ctx.len(), 3);
    }

    #[test]
    fn system_prompt_preserved_even_when_byte_limit_tight() {
        let mut cm = make_manager_with_prompt(100, 30, "sys");
        cm.set_system_prompt(Some("System instructions here".to_owned())); // 24 bytes

        cm.add_user_message("hi"); // 2 bytes — 26 total, fits in 30

        let ctx = cm.get_context();
        assert_eq!(ctx[0].role, MessageRole::System);
        assert_eq!(ctx[0].content, "System instructions here");
    }

    // ── Clear ──

    #[test]
    fn clear_resets_history() {
        let mut cm = make_manager(100, 100_000);
        cm.add_user_message("hello");
        cm.add_assistant_message("hi");
        assert_eq!(cm.message_count(), 2);

        cm.clear();
        assert_eq!(cm.message_count(), 0);
        assert_eq!(cm.total_bytes(), 0);

        let ctx = cm.get_context();
        assert!(ctx.is_empty());
    }

    #[test]
    fn clear_preserves_system_prompt() {
        let mut cm = make_manager_with_prompt(100, 100_000, "system");
        cm.add_user_message("hello");
        cm.clear();

        let ctx = cm.get_context();
        assert_eq!(ctx.len(), 1);
        assert_eq!(ctx[0].role, MessageRole::System);
        assert_eq!(ctx[0].content, "system");
    }

    // ── Single oversized message ──

    #[test]
    fn single_oversized_message_truncated() {
        let mut cm = make_manager(100, 30);
        cm.add_user_message("This is a very long message that exceeds the limit");

        let ctx = cm.get_context();
        assert_eq!(ctx.len(), 1);
        assert!(ctx[0].content.contains(TRUNCATED_MARKER));
        assert!(ctx[0].content.len() <= 30);
    }

    #[test]
    fn oversized_message_with_system_prompt_truncated() {
        let mut cm = make_manager_with_prompt(100, 15, "sys"); // 3 bytes
        cm.add_user_message("This is way too long for the context window");

        let ctx = cm.get_context();
        assert_eq!(ctx[0].role, MessageRole::System);
        assert_eq!(ctx[0].content, "sys");
        // Second message should be truncated
        assert_eq!(ctx.len(), 2);
        assert!(ctx[1].content.contains(TRUNCATED_MARKER));
    }

    // ── Empty manager ──

    #[test]
    fn empty_manager_returns_nothing_without_prompt() {
        let cm = make_manager(100, 100_000);
        let ctx = cm.get_context();
        assert!(ctx.is_empty());
    }

    #[test]
    fn empty_manager_returns_system_prompt_only() {
        let cm = make_manager_with_prompt(100, 100_000, "You are helpful.");
        let ctx = cm.get_context();
        assert_eq!(ctx.len(), 1);
        assert_eq!(ctx[0].role, MessageRole::System);
    }

    // ── total_bytes and message_count ──

    #[test]
    fn total_bytes_tracks_content_size() {
        let mut cm = make_manager(100, 100_000);
        cm.add_user_message("hello"); // 5 bytes
        cm.add_assistant_message("world!"); // 6 bytes
        assert_eq!(cm.total_bytes(), 11);
    }

    #[test]
    fn message_count_excludes_system_prompt() {
        let mut cm = make_manager_with_prompt(100, 100_000, "system");
        assert_eq!(cm.message_count(), 0);
        cm.add_user_message("hello");
        assert_eq!(cm.message_count(), 1);
    }

    // ── from_config ──

    #[test]
    fn from_config_creates_manager() {
        let cm = ContextManager::from_config(16, 64_000, Some("prompt".to_owned()));
        assert_eq!(cm.message_count(), 0);
        assert_eq!(cm.system_prompt(), Some("prompt"));
    }

    // ── set_system_prompt ──

    #[test]
    fn set_system_prompt_updates() {
        let mut cm = make_manager(100, 100_000);
        assert!(cm.system_prompt().is_none());

        cm.set_system_prompt(Some("new prompt".to_owned()));
        assert_eq!(cm.system_prompt(), Some("new prompt"));

        cm.set_system_prompt(None);
        assert!(cm.system_prompt().is_none());
    }

    // ── add_tool_message_with_name ──

    #[test]
    fn tool_message_with_name() {
        let mut cm = make_manager(100, 100_000);
        cm.add_tool_message_with_name("output", ToolCallId::from("tc-1"), Some("bash".to_owned()));

        let ctx = cm.get_context();
        assert_eq!(ctx[0].role, MessageRole::Tool);
        assert_eq!(ctx[0].name.as_deref(), Some("bash"));
        assert_eq!(ctx[0].tool_call_id.as_ref().map(|id| id.0.as_str()), Some("tc-1"));
    }

    // ── Truncation with mixed roles ──

    #[test]
    fn truncation_with_mixed_roles() {
        let mut cm = make_manager_with_prompt(3, 1_000_000, "system");
        cm.add_user_message("u1");
        cm.add_assistant_message("a1");
        cm.add_tool_message("t1", ToolCallId::from("tc-1"));
        cm.add_user_message("u2");
        cm.add_assistant_message("a2");

        let ctx = cm.get_context();
        // system + last 3 messages
        assert_eq!(ctx.len(), 4);
        assert_eq!(ctx[0].role, MessageRole::System);
        assert_eq!(ctx[1].role, MessageRole::Tool);
        assert_eq!(ctx[1].content, "t1");
        assert_eq!(ctx[2].role, MessageRole::User);
        assert_eq!(ctx[2].content, "u2");
        assert_eq!(ctx[3].role, MessageRole::Assistant);
        assert_eq!(ctx[3].content, "a2");
    }

    // ── Unicode safety ──

    #[test]
    fn unicode_content_truncation_safe() {
        let mut cm = make_manager(100, 50);
        cm.add_user_message("🎉🎉🎉🎉🎉"); // 20 bytes

        let ctx = cm.get_context();
        assert_eq!(ctx.len(), 1);
        assert!(ctx[0].content.len() <= 50);
        assert!(ctx[0].content.chars().count() > 0);
    }

    // ── Zero limits ──

    #[test]
    fn zero_max_messages_returns_empty() {
        let mut cm = make_manager(0, 100_000);
        cm.add_user_message("hello");
        let ctx = cm.get_context();
        assert!(ctx.is_empty());
    }

    #[test]
    fn zero_max_bytes_returns_empty() {
        let mut cm = make_manager(100, 0);
        cm.add_user_message("hello");
        let ctx = cm.get_context();
        assert!(ctx.is_empty());
    }
}
