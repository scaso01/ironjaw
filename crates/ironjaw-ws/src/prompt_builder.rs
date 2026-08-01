//! Layered prompt builder for `chat.send`.
//!
//! Assembles system-role messages in a defined priority order before the
//! conversation history is appended.  Each layer is counted separately so
//! callers can report per-layer token budgets without repeating the assembly
//! logic in two (or more) places inside `handler.rs`.
//!
//! ## Layer order (lowest index → first in prompt)
//!
//! | # | Layer | Source |
//! |---|-------|--------|
//! | 1 | **StaticLayer** | CLAUDE.md files from cwd chain (`collect_static_layer`) |
//! | 2 | **Brain** | Identity / memories / context-vars from `build_auto_inject_context` or `build_context` |
//!
//! Dynamic messages (conversation history) are never touched by this builder;
//! the caller appends them after the prefix returned by [`PromptBuilder::build`].

use ironjaw_llm::{tokenizer::count_tokens, ChatMessage, Role};

/// Per-layer token summary attached to every build result.
#[derive(Debug, Clone, Default)]
pub struct LayerTokens {
    /// Token count for all StaticLayer (CLAUDE.md) messages combined.
    pub static_layer: usize,
    /// Token count for the single brain-context message (if any).
    pub brain: usize,
}

impl LayerTokens {
    /// Total tokens across all layers.
    pub fn total(&self) -> usize {
        self.static_layer + self.brain
    }
}

/// Result of [`PromptBuilder::build`].
pub struct PromptPrefix {
    /// The assembled system messages, in layer order.
    pub messages: Vec<ChatMessage>,
    /// Per-layer token counts.
    pub tokens: LayerTokens,
}

/// Assembles the system-message prefix for a `chat.send` turn.
///
/// Construct with [`PromptBuilder::new`], call the layer methods, then call
/// [`PromptBuilder::build`].
pub struct PromptBuilder {
    static_messages: Vec<ChatMessage>,
    brain_message: Option<ChatMessage>,
}

impl PromptBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self {
            static_messages: Vec::new(),
            brain_message: None,
        }
    }

    /// Add static-layer entries (CLAUDE.md files).
    ///
    /// Each entry becomes one `System`-role message prefixed with its source
    /// label.  Call this BEFORE [`PromptBuilder::with_brain`] to guarantee the
    /// correct layer order.
    pub fn with_static_layer(mut self, entries: &[crate::static_layer::StaticLayerEntry]) -> Self {
        for e in entries {
            self.static_messages.push(ChatMessage {
                role: Role::System,
                content: format!("[CLAUDE.md — {}]\n{}", e.source_label, e.content),
                images: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }
        self
    }

    /// Add the brain auto-inject context message (optional).
    ///
    /// Pass `None` to indicate that brain injection was skipped (timeout, voice
    /// session active, `force_fresh`, etc.).
    pub fn with_brain(mut self, brain_msg: Option<ChatMessage>) -> Self {
        self.brain_message = brain_msg;
        self
    }

    /// Consume the builder and return the assembled prefix with token counts.
    pub fn build(self) -> PromptPrefix {
        let static_tokens: usize = self
            .static_messages
            .iter()
            .map(|m| count_tokens(&m.content))
            .sum();

        let brain_tokens = self
            .brain_message
            .as_ref()
            .map(|m| count_tokens(&m.content))
            .unwrap_or(0);

        let mut messages = self.static_messages;
        if let Some(bm) = self.brain_message {
            messages.push(bm);
        }

        PromptPrefix {
            messages,
            tokens: LayerTokens {
                static_layer: static_tokens,
                brain: brain_tokens,
            },
        }
    }
}

impl Default for PromptBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_layer::StaticLayerEntry;
    use std::path::PathBuf;

    fn make_entry(label: &str, content: &str) -> StaticLayerEntry {
        StaticLayerEntry {
            path: PathBuf::from("/fake/CLAUDE.md"),
            content: content.to_string(),
            source_label: label.to_string(),
            token_count: ironjaw_llm::tokenizer::count_tokens(content),
            was_truncated: false,
        }
    }

    fn make_brain_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::System,
            content: content.to_string(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    #[test]
    fn empty_builder_produces_empty_prefix() {
        let prefix = PromptBuilder::new().build();
        assert!(prefix.messages.is_empty());
        assert_eq!(prefix.tokens.total(), 0);
    }

    #[test]
    fn static_layer_only() {
        let entry = make_entry("project", "# Hello world");
        let prefix = PromptBuilder::new().with_static_layer(&[entry]).build();
        assert_eq!(prefix.messages.len(), 1);
        assert!(prefix.messages[0].content.contains("[CLAUDE.md — project]"));
        assert!(prefix.tokens.static_layer > 0);
        assert_eq!(prefix.tokens.brain, 0);
    }

    #[test]
    fn brain_only() {
        let brain = make_brain_msg("You are a helpful assistant with memory context.");
        let prefix = PromptBuilder::new().with_brain(Some(brain)).build();
        assert_eq!(prefix.messages.len(), 1);
        assert_eq!(prefix.messages[0].role, Role::System);
        assert!(prefix.tokens.brain > 0);
        assert_eq!(prefix.tokens.static_layer, 0);
    }

    #[test]
    fn static_layer_before_brain() {
        let entry = make_entry("project", "# Instructions");
        let brain = make_brain_msg("Brain memories here.");
        let prefix = PromptBuilder::new()
            .with_static_layer(&[entry])
            .with_brain(Some(brain))
            .build();
        assert_eq!(prefix.messages.len(), 2);
        // Static layer must come first
        assert!(prefix.messages[0].content.contains("[CLAUDE.md — project]"));
        // Brain comes second
        assert_eq!(prefix.messages[1].content, "Brain memories here.");
        assert!(prefix.tokens.static_layer > 0);
        assert!(prefix.tokens.brain > 0);
        assert_eq!(
            prefix.tokens.total(),
            prefix.tokens.static_layer + prefix.tokens.brain
        );
    }

    #[test]
    fn multiple_static_entries() {
        let e1 = make_entry("project", "# Project");
        let e2 = make_entry("global", "# Global");
        let prefix = PromptBuilder::new().with_static_layer(&[e1, e2]).build();
        assert_eq!(prefix.messages.len(), 2);
        assert!(prefix.tokens.static_layer > 0);
    }

    #[test]
    fn no_brain_message_leaves_brain_tokens_zero() {
        let entry = make_entry("project", "# Hello");
        let prefix = PromptBuilder::new()
            .with_static_layer(&[entry])
            .with_brain(None)
            .build();
        assert_eq!(prefix.tokens.brain, 0);
        assert_eq!(prefix.messages.len(), 1);
    }
}
