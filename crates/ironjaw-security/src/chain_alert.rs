use std::collections::VecDeque;

/// Result of a chain alert check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainAlertResult {
    /// Sequence is safe.
    Ok,
    /// Potentially dangerous sequence detected.
    Warn(String),
    /// Dangerous sequence blocked.
    Block(String),
}

/// Tracks tool execution sequences and detects dangerous patterns.
#[derive(Debug, Clone)]
pub struct ChainAlertGuard {
    history: VecDeque<String>,
    max_history: usize,
}

/// Web search tool names that indicate possible data exfiltration.
const SEARCH_TOOLS: &[&str] = &["web_search", "searxng", "perplexica"];

impl ChainAlertGuard {
    /// Create a new chain alert guard.
    pub fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(16),
            max_history: 16,
        }
    }

    /// Check whether adding this tool to the sequence triggers an alert.
    pub fn check_chain(&mut self, tool_name: &str) -> ChainAlertResult {
        let result = if let Some(prev) = self.history.back() {
            check_pair(prev, tool_name)
        } else {
            ChainAlertResult::Ok
        };

        // Record in history
        if self.history.len() >= self.max_history {
            self.history.pop_front();
        }
        self.history.push_back(tool_name.to_string());

        if let ChainAlertResult::Warn(ref reason) | ChainAlertResult::Block(ref reason) = result {
            tracing::warn!(
                prev = self
                    .history
                    .get(self.history.len().saturating_sub(2))
                    .map(|s| s.as_str()),
                current = tool_name,
                reason,
                "Chain alert triggered"
            );
        }

        result
    }

    /// Reset the tool sequence history.
    pub fn reset(&mut self) {
        self.history.clear();
    }

    /// Get the current tool history for inspection.
    pub fn history(&self) -> &VecDeque<String> {
        &self.history
    }
}

impl Default for ChainAlertGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Check a (previous_tool, current_tool) pair for dangerous sequences.
fn check_pair(prev: &str, current: &str) -> ChainAlertResult {
    // secrets_get -> shell: BLOCK
    if prev == "secrets_get" && current == "shell" {
        return ChainAlertResult::Block("Secret access followed by shell execution".to_string());
    }

    // secrets_get -> web search: BLOCK
    if prev == "secrets_get" && is_search_tool(current) {
        return ChainAlertResult::Block("Secret access followed by web search".to_string());
    }

    // write_file -> shell: WARN
    if prev == "write_file" && current == "shell" {
        return ChainAlertResult::Warn("File write followed by shell execution".to_string());
    }

    // shell -> web search: WARN
    if prev == "shell" && is_search_tool(current) {
        return ChainAlertResult::Warn(
            "Shell execution followed by web search - possible data exfiltration".to_string(),
        );
    }

    // brain.recall -> shell: WARN
    if prev == "brain.recall" && current == "shell" {
        return ChainAlertResult::Warn("Memory recall followed by shell execution".to_string());
    }

    ChainAlertResult::Ok
}

/// Check if a tool name is a search/web tool.
fn is_search_tool(name: &str) -> bool {
    SEARCH_TOOLS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_sequence() {
        let mut guard = ChainAlertGuard::new();
        assert_eq!(guard.check_chain("read_file"), ChainAlertResult::Ok);
        assert_eq!(guard.check_chain("write_file"), ChainAlertResult::Ok);
        assert_eq!(guard.check_chain("read_file"), ChainAlertResult::Ok);
    }

    #[test]
    fn test_write_then_shell_warns() {
        let mut guard = ChainAlertGuard::new();
        assert_eq!(guard.check_chain("write_file"), ChainAlertResult::Ok);
        assert!(matches!(
            guard.check_chain("shell"),
            ChainAlertResult::Warn(_)
        ));
    }

    #[test]
    fn test_shell_then_search_warns() {
        let mut guard = ChainAlertGuard::new();
        guard.check_chain("shell");
        assert!(matches!(
            guard.check_chain("web_search"),
            ChainAlertResult::Warn(_)
        ));
    }

    #[test]
    fn test_shell_then_searxng_warns() {
        let mut guard = ChainAlertGuard::new();
        guard.check_chain("shell");
        assert!(matches!(
            guard.check_chain("searxng"),
            ChainAlertResult::Warn(_)
        ));
    }

    #[test]
    fn test_shell_then_perplexica_warns() {
        let mut guard = ChainAlertGuard::new();
        guard.check_chain("shell");
        assert!(matches!(
            guard.check_chain("perplexica"),
            ChainAlertResult::Warn(_)
        ));
    }

    #[test]
    fn test_secrets_then_shell_blocks() {
        let mut guard = ChainAlertGuard::new();
        guard.check_chain("secrets_get");
        assert!(matches!(
            guard.check_chain("shell"),
            ChainAlertResult::Block(_)
        ));
    }

    #[test]
    fn test_secrets_then_search_blocks() {
        let mut guard = ChainAlertGuard::new();
        guard.check_chain("secrets_get");
        assert!(matches!(
            guard.check_chain("web_search"),
            ChainAlertResult::Block(_)
        ));
    }

    #[test]
    fn test_secrets_then_searxng_blocks() {
        let mut guard = ChainAlertGuard::new();
        guard.check_chain("secrets_get");
        assert!(matches!(
            guard.check_chain("searxng"),
            ChainAlertResult::Block(_)
        ));
    }

    #[test]
    fn test_brain_recall_then_shell_warns() {
        let mut guard = ChainAlertGuard::new();
        guard.check_chain("brain.recall");
        assert!(matches!(
            guard.check_chain("shell"),
            ChainAlertResult::Warn(_)
        ));
    }

    #[test]
    fn test_non_adjacent_no_alert() {
        let mut guard = ChainAlertGuard::new();
        guard.check_chain("secrets_get");
        guard.check_chain("read_file"); // safe interleaving
        assert_eq!(guard.check_chain("shell"), ChainAlertResult::Ok);
    }

    #[test]
    fn test_reset_clears_history() {
        let mut guard = ChainAlertGuard::new();
        guard.check_chain("secrets_get");
        guard.reset();
        assert_eq!(guard.check_chain("shell"), ChainAlertResult::Ok);
    }

    #[test]
    fn test_first_call_always_ok() {
        let mut guard = ChainAlertGuard::new();
        assert_eq!(guard.check_chain("shell"), ChainAlertResult::Ok);
        assert_eq!(guard.history().len(), 1);
    }

    #[test]
    fn test_history_bounded() {
        let mut guard = ChainAlertGuard::new();
        for i in 0..32 {
            guard.check_chain(&format!("tool_{i}"));
        }
        assert!(guard.history().len() <= 16);
    }
}
