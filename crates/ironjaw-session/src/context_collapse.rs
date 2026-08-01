//! Context Collapse: mid-conversation compression of verbose tool results.
//! Replaces tool result content with a compact summary while maintaining
//! an undo log for restoration if the LLM needs the original.

use std::collections::VecDeque;

/// A single collapse record — stores original content for undo.
#[derive(Debug, Clone)]
pub struct CollapseEntry {
    pub message_id: String,
    pub original_content: String,
    pub summary: String,
    pub collapsed_at: chrono::DateTime<chrono::Utc>,
    pub restored: bool,
}

impl CollapseEntry {
    pub fn new(message_id: &str, original: &str, summary: &str) -> Self {
        Self {
            message_id: message_id.to_string(),
            original_content: original.to_string(),
            summary: summary.to_string(),
            collapsed_at: chrono::Utc::now(),
            restored: false,
        }
    }
}

/// LRU-bounded registry of collapsed tool results.
/// Enables undo: if the LLM needs original content, restore from here.
pub struct CollapseRegistry {
    entries: VecDeque<CollapseEntry>,
    max_entries: usize,
}

impl CollapseRegistry {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            max_entries,
        }
    }

    pub fn store(&mut self, message_id: &str, original: &str, summary: &str) {
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front(); // Evict oldest
        }
        self.entries
            .push_back(CollapseEntry::new(message_id, original, summary));
    }

    pub fn restore(&mut self, message_id: &str) -> Option<String> {
        for entry in self.entries.iter_mut() {
            if entry.message_id == message_id && !entry.restored {
                entry.restored = true;
                return Some(entry.original_content.clone());
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collapse_entry_creation() {
        let original = "very long tool output here".repeat(100);
        let summary = "Tool returned 2500 chars of output about file listing";
        let entry = CollapseEntry::new("msg-123", &original, summary);

        assert_eq!(entry.message_id, "msg-123");
        assert_eq!(entry.original_content, original);
        assert_eq!(entry.summary, summary);
        assert!(!entry.restored);
    }

    #[test]
    fn test_collapse_registry_store_and_restore() {
        let mut registry = CollapseRegistry::new(50);
        let original = "original content".to_string();
        registry.store("msg-1", &original, "summary");

        let restored = registry.restore("msg-1");
        assert_eq!(restored, Some(original));
    }

    #[test]
    fn test_collapse_registry_evicts_oldest() {
        let mut registry = CollapseRegistry::new(2);
        registry.store("msg-1", "content-1", "summary-1");
        registry.store("msg-2", "content-2", "summary-2");
        registry.store("msg-3", "content-3", "summary-3"); // Evicts msg-1

        assert!(registry.restore("msg-1").is_none());
        assert!(registry.restore("msg-2").is_some());
    }
}
