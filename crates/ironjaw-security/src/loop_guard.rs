use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

/// Result of a loop guard check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopGuardResult {
    /// Tool call is fine.
    Ok,
    /// Repeated tool call detected but below threshold.
    Warning(usize),
    /// Tool call blocked due to excessive repetition.
    Blocked(String),
}

/// Fingerprint of a tool call (hash of tool name + sorted params).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCallFingerprint {
    hash: u64,
}

impl ToolCallFingerprint {
    /// Create a fingerprint from a tool name and its JSON parameters.
    pub fn new(tool_name: &str, params: &serde_json::Value) -> Self {
        let sorted = canonicalize_params(params);
        let mut hasher = DefaultHasher::new();
        tool_name.hash(&mut hasher);
        sorted.hash(&mut hasher);
        Self {
            hash: hasher.finish(),
        }
    }
}

/// Canonicalize JSON params by sorting object keys recursively.
fn canonicalize_params(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut pairs: Vec<_> = map.iter().collect();
            pairs.sort_by_key(|(k, _)| (*k).clone());
            let inner: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("{k}:{}", canonicalize_params(v)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        serde_json::Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(canonicalize_params).collect();
            format!("[{}]", inner.join(","))
        }
        other => other.to_string(),
    }
}

/// Detects when an LLM is repeating the same tool calls in a loop.
#[derive(Debug, Clone)]
pub struct LoopGuard {
    window_size: usize,
    max_repeats: usize,
    history: VecDeque<u64>,
}

impl LoopGuard {
    /// Create a new loop guard with default settings (window=10, max_repeats=3).
    pub fn new() -> Self {
        Self {
            window_size: 10,
            max_repeats: 3,
            history: VecDeque::with_capacity(10),
        }
    }

    /// Create a loop guard with custom settings.
    pub fn with_config(window_size: usize, max_repeats: usize) -> Self {
        Self {
            window_size,
            max_repeats,
            history: VecDeque::with_capacity(window_size),
        }
    }

    /// Check a tool call fingerprint against the sliding window.
    pub fn check(&mut self, fingerprint: &ToolCallFingerprint) -> LoopGuardResult {
        let hash = fingerprint.hash;

        // Add to history, evict oldest if full
        if self.history.len() >= self.window_size {
            self.history.pop_front();
        }
        self.history.push_back(hash);

        // Count consecutive repeats of this fingerprint at the tail
        let repeat_count = self
            .history
            .iter()
            .rev()
            .take_while(|&&h| h == hash)
            .count();

        if repeat_count >= self.max_repeats {
            tracing::warn!(
                repeat_count,
                "Loop guard blocked: tool call repeated {repeat_count} times"
            );
            return LoopGuardResult::Blocked(format!(
                "Tool call repeated {repeat_count} consecutive times (max {max})",
                max = self.max_repeats,
            ));
        }

        // Detect ping-pong: A B A B A B pattern
        if self.history.len() >= 6 {
            if let Some(result) = self.detect_ping_pong() {
                return result;
            }
        }

        if repeat_count >= 2 {
            return LoopGuardResult::Warning(repeat_count);
        }

        LoopGuardResult::Ok
    }

    /// Detect alternating A->B->A->B pattern (3+ full cycles = 6+ entries).
    fn detect_ping_pong(&self) -> Option<LoopGuardResult> {
        let len = self.history.len();
        if len < 6 {
            return None;
        }

        let a = self.history[len - 2];
        let b = self.history[len - 1];
        if a == b {
            return None;
        }

        let mut cycles = 0;
        let mut i = len;
        while i >= 2 {
            let pos_a = self.history[i - 2];
            let pos_b = self.history[i - 1];
            if pos_a == a && pos_b == b {
                cycles += 1;
                i -= 2;
            } else {
                break;
            }
        }

        if cycles >= 3 {
            tracing::warn!(cycles, "Loop guard: ping-pong pattern detected");
            return Some(LoopGuardResult::Blocked(format!(
                "Ping-pong pattern detected: 2 alternating tool calls repeated {cycles} times"
            )));
        }

        None
    }

    /// Reset the guard history.
    pub fn reset(&mut self) {
        self.history.clear();
    }
}

impl Default for LoopGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fp(name: &str, params: serde_json::Value) -> ToolCallFingerprint {
        ToolCallFingerprint::new(name, &params)
    }

    #[test]
    fn test_no_loop_different_calls() {
        let mut guard = LoopGuard::new();
        assert_eq!(
            guard.check(&fp("read", json!({"path": "/a"}))),
            LoopGuardResult::Ok
        );
        assert_eq!(
            guard.check(&fp("read", json!({"path": "/b"}))),
            LoopGuardResult::Ok
        );
        assert_eq!(
            guard.check(&fp("write", json!({"path": "/c"}))),
            LoopGuardResult::Ok
        );
    }

    #[test]
    fn test_warning_on_two_repeats() {
        let mut guard = LoopGuard::new();
        let f = fp("read", json!({"path": "/a"}));
        assert_eq!(guard.check(&f), LoopGuardResult::Ok);
        assert_eq!(guard.check(&f), LoopGuardResult::Warning(2));
    }

    #[test]
    fn test_blocked_on_three_repeats() {
        let mut guard = LoopGuard::new();
        let f = fp("read", json!({"path": "/a"}));
        assert_eq!(guard.check(&f), LoopGuardResult::Ok);
        assert_eq!(guard.check(&f), LoopGuardResult::Warning(2));
        assert!(matches!(guard.check(&f), LoopGuardResult::Blocked(_)));
    }

    #[test]
    fn test_custom_config() {
        let mut guard = LoopGuard::with_config(5, 4);
        let f = fp("shell", json!({"cmd": "ls"}));
        assert_eq!(guard.check(&f), LoopGuardResult::Ok);
        assert_eq!(guard.check(&f), LoopGuardResult::Warning(2));
        assert_eq!(guard.check(&f), LoopGuardResult::Warning(3));
        assert!(matches!(guard.check(&f), LoopGuardResult::Blocked(_)));
    }

    #[test]
    fn test_ping_pong_detection() {
        let mut guard = LoopGuard::new();
        let a = fp("read", json!({"path": "/a"}));
        let b = fp("write", json!({"path": "/b"}));

        // 3 cycles of A->B
        assert_eq!(guard.check(&a), LoopGuardResult::Ok);
        assert_eq!(guard.check(&b), LoopGuardResult::Ok);
        assert_eq!(guard.check(&a), LoopGuardResult::Ok);
        assert_eq!(guard.check(&b), LoopGuardResult::Ok);
        assert_eq!(guard.check(&a), LoopGuardResult::Ok);
        assert!(matches!(guard.check(&b), LoopGuardResult::Blocked(_)));
    }

    #[test]
    fn test_reset_clears_history() {
        let mut guard = LoopGuard::new();
        let f = fp("read", json!({"path": "/a"}));
        guard.check(&f);
        guard.check(&f);
        guard.reset();
        assert_eq!(guard.check(&f), LoopGuardResult::Ok);
    }

    #[test]
    fn test_fingerprint_param_order_irrelevant() {
        let f1 = ToolCallFingerprint::new("tool", &json!({"a": 1, "b": 2}));
        let f2 = ToolCallFingerprint::new("tool", &json!({"b": 2, "a": 1}));
        assert_eq!(f1, f2);
    }

    #[test]
    fn test_different_tools_different_fingerprints() {
        let f1 = ToolCallFingerprint::new("read", &json!({"path": "/a"}));
        let f2 = ToolCallFingerprint::new("write", &json!({"path": "/a"}));
        assert_ne!(f1, f2);
    }

    #[test]
    fn test_window_eviction() {
        let mut guard = LoopGuard::with_config(4, 3);
        let f = fp("read", json!({"path": "/a"}));

        // Fill with repeats, then break with different calls
        guard.check(&f);
        guard.check(&f);
        // Insert different calls to push old ones out
        guard.check(&fp("x", json!({})));
        guard.check(&fp("y", json!({})));
        // Now the two old repeats are evicted; this is a fresh start
        assert_eq!(guard.check(&f), LoopGuardResult::Ok);
    }
}
