// Message coalescing buffer — batches incoming messages per session.
// Flushes on timeout expiry or when batch reaches max size.

use std::collections::HashMap;

/// Configuration for the coalescing buffer.
#[derive(Debug, Clone)]
pub struct CoalescingConfig {
    /// Delay in milliseconds before auto-flushing a batch.
    pub delay_ms: u64,
    /// Maximum number of messages per batch before forced flush.
    pub max_batch_size: usize,
}

impl Default for CoalescingConfig {
    fn default() -> Self {
        Self {
            delay_ms: 100,
            max_batch_size: 10,
        }
    }
}

/// Buffers incoming messages per session, flushing when batch is full or on demand.
#[derive(Debug)]
pub struct CoalescingBuffer {
    config: CoalescingConfig,
    buffers: HashMap<String, Vec<serde_json::Value>>,
}

impl CoalescingBuffer {
    /// Create a new coalescing buffer with the given configuration.
    pub fn new(config: CoalescingConfig) -> Self {
        Self {
            config,
            buffers: HashMap::new(),
        }
    }

    /// Create a new coalescing buffer with default settings.
    pub fn with_defaults() -> Self {
        Self::new(CoalescingConfig::default())
    }

    /// Add a message to the buffer for the given session.
    ///
    /// Returns `Some(batch)` when the batch reaches `max_batch_size`, triggering a flush.
    /// Returns `None` if the message was buffered without reaching the threshold.
    pub fn add_message(
        &mut self,
        session_id: &str,
        msg: serde_json::Value,
    ) -> Option<Vec<serde_json::Value>> {
        let buffer = self.buffers.entry(session_id.to_string()).or_default();
        buffer.push(msg);

        if buffer.len() >= self.config.max_batch_size {
            Some(self.flush(session_id))
        } else {
            None
        }
    }

    /// Force-flush all buffered messages for a session.
    ///
    /// Returns the accumulated messages (may be empty if no messages were buffered).
    pub fn flush(&mut self, session_id: &str) -> Vec<serde_json::Value> {
        self.buffers.remove(session_id).unwrap_or_default()
    }

    /// Returns the number of buffered messages for a session.
    pub fn pending_count(&self, session_id: &str) -> usize {
        self.buffers.get(session_id).map(|b| b.len()).unwrap_or(0)
    }

    /// Returns the configured delay in milliseconds.
    pub fn delay_ms(&self) -> u64 {
        self.config.delay_ms
    }

    /// Returns the configured max batch size.
    pub fn max_batch_size(&self) -> usize {
        self.config.max_batch_size
    }

    /// Returns the number of sessions with buffered messages.
    pub fn active_sessions(&self) -> usize {
        self.buffers.len()
    }

    /// Remove all state for a session (call on disconnect).
    pub fn cleanup_session(&mut self, session_id: &str) {
        self.buffers.remove(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_default_config() {
        let config = CoalescingConfig::default();
        assert_eq!(config.delay_ms, 100);
        assert_eq!(config.max_batch_size, 10);
    }

    #[test]
    fn test_custom_config() {
        let config = CoalescingConfig {
            delay_ms: 200,
            max_batch_size: 5,
        };
        assert_eq!(config.delay_ms, 200);
        assert_eq!(config.max_batch_size, 5);
    }

    #[test]
    fn test_empty_buffer_flush() {
        let mut buf = CoalescingBuffer::with_defaults();
        let result = buf.flush("session-1");
        assert!(result.is_empty());
    }

    #[test]
    fn test_single_message_no_flush() {
        let mut buf = CoalescingBuffer::with_defaults();
        let result = buf.add_message("s1", json!({"text": "hello"}));
        assert!(result.is_none());
        assert_eq!(buf.pending_count("s1"), 1);
    }

    #[test]
    fn test_batch_full_triggers_flush() {
        let config = CoalescingConfig {
            delay_ms: 100,
            max_batch_size: 3,
        };
        let mut buf = CoalescingBuffer::new(config);

        assert!(buf.add_message("s1", json!(1)).is_none());
        assert!(buf.add_message("s1", json!(2)).is_none());

        let batch = buf.add_message("s1", json!(3));
        assert!(batch.is_some());
        let batch = batch.unwrap();
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0], json!(1));
        assert_eq!(batch[1], json!(2));
        assert_eq!(batch[2], json!(3));
    }

    #[test]
    fn test_manual_flush_returns_buffered() {
        let mut buf = CoalescingBuffer::with_defaults();
        buf.add_message("s1", json!("a"));
        buf.add_message("s1", json!("b"));

        let batch = buf.flush("s1");
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0], json!("a"));
        assert_eq!(batch[1], json!("b"));

        // Buffer is now empty
        assert_eq!(buf.pending_count("s1"), 0);
    }

    #[test]
    fn test_flush_clears_buffer() {
        let mut buf = CoalescingBuffer::with_defaults();
        buf.add_message("s1", json!(1));
        buf.flush("s1");

        let result = buf.flush("s1");
        assert!(result.is_empty());
    }

    #[test]
    fn test_multiple_sessions_independent() {
        let config = CoalescingConfig {
            delay_ms: 100,
            max_batch_size: 2,
        };
        let mut buf = CoalescingBuffer::new(config);

        // Fill s1 partially
        assert!(buf.add_message("s1", json!("a")).is_none());

        // Fill s2 to capacity
        assert!(buf.add_message("s2", json!("x")).is_none());
        let batch = buf.add_message("s2", json!("y"));
        assert!(batch.is_some());
        assert_eq!(batch.unwrap().len(), 2);

        // s1 should still have its message
        assert_eq!(buf.pending_count("s1"), 1);
        assert_eq!(buf.pending_count("s2"), 0);
    }

    #[test]
    fn test_batch_size_one_immediate_flush() {
        let config = CoalescingConfig {
            delay_ms: 100,
            max_batch_size: 1,
        };
        let mut buf = CoalescingBuffer::new(config);

        let batch = buf.add_message("s1", json!("immediate"));
        assert!(batch.is_some());
        assert_eq!(batch.unwrap().len(), 1);
    }

    #[test]
    fn test_active_sessions_count() {
        let mut buf = CoalescingBuffer::with_defaults();
        assert_eq!(buf.active_sessions(), 0);

        buf.add_message("s1", json!(1));
        assert_eq!(buf.active_sessions(), 1);

        buf.add_message("s2", json!(2));
        assert_eq!(buf.active_sessions(), 2);

        buf.flush("s1");
        assert_eq!(buf.active_sessions(), 1);
    }

    #[test]
    fn test_cleanup_session() {
        let mut buf = CoalescingBuffer::with_defaults();
        buf.add_message("s1", json!(1));
        buf.add_message("s2", json!(2));

        buf.cleanup_session("s1");
        assert_eq!(buf.pending_count("s1"), 0);
        assert_eq!(buf.pending_count("s2"), 1);
        assert_eq!(buf.active_sessions(), 1);
    }

    #[test]
    fn test_accessor_methods() {
        let config = CoalescingConfig {
            delay_ms: 250,
            max_batch_size: 20,
        };
        let buf = CoalescingBuffer::new(config);
        assert_eq!(buf.delay_ms(), 250);
        assert_eq!(buf.max_batch_size(), 20);
    }

    #[test]
    fn test_sequential_batches() {
        let config = CoalescingConfig {
            delay_ms: 100,
            max_batch_size: 2,
        };
        let mut buf = CoalescingBuffer::new(config);

        // First batch
        buf.add_message("s1", json!(1));
        let batch1 = buf.add_message("s1", json!(2)).unwrap();
        assert_eq!(batch1, vec![json!(1), json!(2)]);

        // Second batch from same session
        buf.add_message("s1", json!(3));
        let batch2 = buf.add_message("s1", json!(4)).unwrap();
        assert_eq!(batch2, vec![json!(3), json!(4)]);
    }
}
