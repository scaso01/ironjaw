//! Session history repair pipeline.
//!
//! Cleans conversation history before sending to an LLM.
//! The LLM expects well-formed message sequences (alternating user/assistant,
//! no empty messages, etc.).

use crate::types::SessionMessage;

/// Lightweight message used during history repair.
/// Converts to/from [`SessionMessage`] for pipeline use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    /// Create a new message.
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }

    /// Create a synthetic assistant "[continued]" message.
    fn synthetic_assistant() -> Self {
        Self::new("assistant", "[continued]")
    }

    /// Create a synthetic user "[continued]" message.
    fn synthetic_user() -> Self {
        Self::new("user", "[continued]")
    }

    /// Create a synthetic user "[conversation start]" message.
    fn conversation_start() -> Self {
        Self::new("user", "[conversation start]")
    }

    /// Create a synthetic tool-result "[result not available]" message.
    fn missing_tool_result() -> Self {
        Self::new("tool", "[result not available]")
    }
}

impl From<&SessionMessage> for Message {
    fn from(msg: &SessionMessage) -> Self {
        Self {
            role: msg.role.clone(),
            content: msg.content.clone(),
        }
    }
}

/// Run the full 7-phase repair pipeline on a message history.
///
/// Phases are applied in order:
/// 1. Remove empty messages
/// 2. Fix orphaned tool results
/// 3. Merge consecutive same-role messages
/// 4. Ensure strict alternation
/// 5. Validate tool-use / tool-result pairs
/// 6. Trim whitespace
/// 7. Ensure first message is from the user
pub fn repair_history(messages: &mut Vec<Message>) {
    remove_empty(messages);
    fix_orphans(messages);
    merge_consecutive(messages);
    ensure_alternation(messages);
    validate_pairs(messages);
    trim(messages);
    ensure_user_first(messages);
}

/// Returns `true` if the history passes all validation checks without needing repair.
pub fn is_history_valid(messages: &[Message]) -> bool {
    let mut clone = messages.to_vec();
    repair_history(&mut clone);
    clone == messages
}

// --- Phase 1: Remove empty messages ---

/// Remove messages with empty or whitespace-only content.
fn remove_empty(messages: &mut Vec<Message>) {
    messages.retain(|msg| !msg.content.trim().is_empty());
}

// --- Phase 2: Fix orphaned tool results ---

/// Remove tool-result messages that have no preceding tool-use message.
fn fix_orphans(messages: &mut Vec<Message>) {
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role == "tool" {
            // Check if there is a preceding tool-use (assistant with tool_use role)
            let has_preceding_tool_use = i > 0 && messages[i - 1].role == "tool_use";
            if !has_preceding_tool_use {
                messages.remove(i);
                continue;
            }
        }
        i += 1;
    }
}

// --- Phase 3: Merge consecutive same-role messages ---

/// Merge consecutive messages from the same role by concatenating content with "\n\n".
fn merge_consecutive(messages: &mut Vec<Message>) {
    if messages.len() <= 1 {
        return;
    }
    let mut i = 0;
    while i + 1 < messages.len() {
        if messages[i].role == messages[i + 1].role {
            let appended = messages.remove(i + 1).content;
            messages[i].content = format!("{}\n\n{}", messages[i].content, appended);
        } else {
            i += 1;
        }
    }
}

// --- Phase 4: Ensure alternation ---

/// If two user messages are adjacent (after merge), insert a synthetic assistant message.
/// Similarly for adjacent assistant messages, insert a synthetic user message.
fn ensure_alternation(messages: &mut Vec<Message>) {
    let mut i = 0;
    while i + 1 < messages.len() {
        let current_role = &messages[i].role;
        let next_role = &messages[i + 1].role;
        if current_role == next_role {
            let synthetic = if current_role == "user" {
                Message::synthetic_assistant()
            } else {
                Message::synthetic_user()
            };
            messages.insert(i + 1, synthetic);
            // Skip past the inserted message
            i += 2;
        } else {
            i += 1;
        }
    }
}

// --- Phase 5: Validate tool-use / tool-result pairs ---

/// Ensure tool-use messages are followed by tool-result messages.
/// If a tool-use lacks a result, append a synthetic tool-result.
fn validate_pairs(messages: &mut Vec<Message>) {
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role == "tool_use" {
            let has_result = i + 1 < messages.len() && messages[i + 1].role == "tool";
            if !has_result {
                messages.insert(i + 1, Message::missing_tool_result());
            }
            // Skip past the pair
            i += 2;
        } else {
            i += 1;
        }
    }
}

// --- Phase 6: Trim whitespace ---

/// Trim leading/trailing whitespace from all message contents.
fn trim(messages: &mut [Message]) {
    for msg in messages.iter_mut() {
        msg.content = msg.content.trim().to_string();
    }
}

// --- Phase 7: Ensure user first ---

/// If the first message is not from the user role, prepend a synthetic user message.
fn ensure_user_first(messages: &mut Vec<Message>) {
    if messages.is_empty() {
        return;
    }
    if messages[0].role != "user" {
        messages.insert(0, Message::conversation_start());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> Message {
        Message::new(role, content)
    }

    // --- Phase 1 tests ---

    #[test]
    fn test_remove_empty_strips_empty() {
        let mut msgs = vec![
            msg("user", "hello"),
            msg("assistant", ""),
            msg("user", "world"),
        ];
        remove_empty(&mut msgs);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "hello");
        assert_eq!(msgs[1].content, "world");
    }

    #[test]
    fn test_remove_empty_strips_whitespace_only() {
        let mut msgs = vec![
            msg("user", "   "),
            msg("assistant", "\n\t"),
            msg("user", "ok"),
        ];
        remove_empty(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "ok");
    }

    #[test]
    fn test_remove_empty_preserves_valid() {
        let mut msgs = vec![msg("user", "hello"), msg("assistant", "hi")];
        remove_empty(&mut msgs);
        assert_eq!(msgs.len(), 2);
    }

    // --- Phase 2 tests ---

    #[test]
    fn test_fix_orphans_removes_orphaned_tool_result() {
        let mut msgs = vec![
            msg("user", "hello"),
            msg("tool", "orphaned result"),
            msg("assistant", "hi"),
        ];
        fix_orphans(&mut msgs);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
    }

    #[test]
    fn test_fix_orphans_preserves_valid_pair() {
        let mut msgs = vec![
            msg("tool_use", "call function"),
            msg("tool", "result"),
            msg("assistant", "done"),
        ];
        fix_orphans(&mut msgs);
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn test_fix_orphans_tool_at_start() {
        let mut msgs = vec![msg("tool", "no preceding tool_use")];
        fix_orphans(&mut msgs);
        assert!(msgs.is_empty());
    }

    // --- Phase 3 tests ---

    #[test]
    fn test_merge_consecutive_same_role() {
        let mut msgs = vec![
            msg("user", "hello"),
            msg("user", "world"),
            msg("assistant", "hi"),
        ];
        merge_consecutive(&mut msgs);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "hello\n\nworld");
        assert_eq!(msgs[1].content, "hi");
    }

    #[test]
    fn test_merge_consecutive_three_in_row() {
        let mut msgs = vec![msg("user", "a"), msg("user", "b"), msg("user", "c")];
        merge_consecutive(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "a\n\nb\n\nc");
    }

    #[test]
    fn test_merge_consecutive_no_merge_needed() {
        let mut msgs = vec![msg("user", "hello"), msg("assistant", "hi")];
        merge_consecutive(&mut msgs);
        assert_eq!(msgs.len(), 2);
    }

    // --- Phase 4 tests ---

    #[test]
    fn test_ensure_alternation_inserts_assistant() {
        let mut msgs = vec![msg("user", "a"), msg("user", "b")];
        ensure_alternation(&mut msgs);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content, "[continued]");
        assert_eq!(msgs[2].role, "user");
    }

    #[test]
    fn test_ensure_alternation_inserts_user() {
        let mut msgs = vec![msg("assistant", "a"), msg("assistant", "b")];
        ensure_alternation(&mut msgs);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "assistant");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content, "[continued]");
        assert_eq!(msgs[2].role, "assistant");
    }

    #[test]
    fn test_ensure_alternation_already_alternating() {
        let mut msgs = vec![msg("user", "a"), msg("assistant", "b")];
        ensure_alternation(&mut msgs);
        assert_eq!(msgs.len(), 2);
    }

    // --- Phase 5 tests ---

    #[test]
    fn test_validate_pairs_adds_missing_result() {
        let mut msgs = vec![msg("tool_use", "call fn"), msg("assistant", "done")];
        validate_pairs(&mut msgs);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "tool_use");
        assert_eq!(msgs[1].role, "tool");
        assert_eq!(msgs[1].content, "[result not available]");
        assert_eq!(msgs[2].role, "assistant");
    }

    #[test]
    fn test_validate_pairs_existing_result() {
        let mut msgs = vec![msg("tool_use", "call fn"), msg("tool", "result here")];
        validate_pairs(&mut msgs);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].content, "result here");
    }

    #[test]
    fn test_validate_pairs_tool_use_at_end() {
        let mut msgs = vec![msg("tool_use", "call fn")];
        validate_pairs(&mut msgs);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, "tool");
        assert_eq!(msgs[1].content, "[result not available]");
    }

    // --- Phase 6 tests ---

    #[test]
    fn test_trim_whitespace() {
        let mut msgs = vec![msg("user", "  hello  "), msg("assistant", "\n hi \n")];
        trim(&mut msgs);
        assert_eq!(msgs[0].content, "hello");
        assert_eq!(msgs[1].content, "hi");
    }

    // --- Phase 7 tests ---

    #[test]
    fn test_ensure_user_first_already_user() {
        let mut msgs = vec![msg("user", "hello"), msg("assistant", "hi")];
        ensure_user_first(&mut msgs);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn test_ensure_user_first_prepends() {
        let mut msgs = vec![msg("assistant", "hi")];
        ensure_user_first(&mut msgs);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "[conversation start]");
    }

    #[test]
    fn test_ensure_user_first_empty() {
        let mut msgs: Vec<Message> = vec![];
        ensure_user_first(&mut msgs);
        assert!(msgs.is_empty());
    }

    // --- Full pipeline tests ---

    #[test]
    fn test_repair_history_full_pipeline() {
        let mut msgs = vec![
            msg("assistant", "I start first"),
            msg("user", "  "),
            msg("user", "hello"),
            msg("user", "  how are you  "),
            msg("tool", "orphaned"),
            msg("assistant", "I'm fine"),
            msg("tool_use", "search"),
            msg("assistant", "next"),
        ];
        repair_history(&mut msgs);

        // Phase 1: removes empty " "
        // Phase 2: removes orphaned "tool"
        // Phase 3: merges consecutive users "hello" + "how are you"
        // Phase 4: handled by merge already
        // Phase 5: tool_use "search" gets synthetic tool result
        // Phase 6: trim
        // Phase 7: assistant first -> prepend user

        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "[conversation start]");

        // Validate all messages have non-empty trimmed content
        for m in &msgs {
            assert!(!m.content.trim().is_empty(), "empty content found: {m:?}");
        }
    }

    #[test]
    fn test_repair_history_already_valid() {
        let mut msgs = vec![msg("user", "hello"), msg("assistant", "hi there")];
        let original = msgs.clone();
        repair_history(&mut msgs);
        assert_eq!(msgs, original);
    }

    #[test]
    fn test_is_history_valid_true() {
        let msgs = vec![msg("user", "hello"), msg("assistant", "hi there")];
        assert!(is_history_valid(&msgs));
    }

    #[test]
    fn test_is_history_valid_false_empty_content() {
        let msgs = vec![
            msg("user", "hello"),
            msg("assistant", ""),
            msg("user", "world"),
        ];
        assert!(!is_history_valid(&msgs));
    }

    #[test]
    fn test_is_history_valid_false_wrong_first() {
        let msgs = vec![msg("assistant", "hello")];
        assert!(!is_history_valid(&msgs));
    }

    #[test]
    fn test_is_history_valid_false_consecutive_same_role() {
        let msgs = vec![msg("user", "a"), msg("user", "b")];
        assert!(!is_history_valid(&msgs));
    }

    #[test]
    fn test_repair_empty_history() {
        let mut msgs: Vec<Message> = vec![];
        repair_history(&mut msgs);
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_repair_single_user_message() {
        let mut msgs = vec![msg("user", "solo")];
        repair_history(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "solo");
    }

    #[test]
    fn test_from_session_message() {
        let sm = SessionMessage {
            id: 1,
            session_id: "s1".to_string(),
            role: "user".to_string(),
            content: "hello".to_string(),
            model: None,
            tokens: None,
            complete: true,
            created_at: "2026-01-01".to_string(),
            tool_calls: None,
            tool_call_id: None,
        };
        let m = Message::from(&sm);
        assert_eq!(m.role, "user");
        assert_eq!(m.content, "hello");
    }

    #[test]
    fn test_tool_use_followed_by_tool_preserved() {
        let mut msgs = vec![
            msg("user", "run search"),
            msg("tool_use", "search web"),
            msg("tool", "results here"),
            msg("assistant", "found it"),
        ];
        repair_history(&mut msgs);
        // tool_use/tool pair should remain intact
        assert_eq!(msgs[1].role, "tool_use");
        assert_eq!(msgs[2].role, "tool");
        assert_eq!(msgs[2].content, "results here");
    }
}
