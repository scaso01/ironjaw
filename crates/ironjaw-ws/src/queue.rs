use crate::notifications::Priority;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// A queued message for a device.
#[derive(Debug, Clone)]
pub struct QueuedMessage {
    pub frame_json: String,
    pub priority: Priority,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Bounded message queue for offline devices.
/// Messages are stored per device and drained on reconnect.
#[derive(Clone)]
pub struct MessageQueue {
    queues: Arc<RwLock<HashMap<String, VecDeque<QueuedMessage>>>>,
    max_per_device: usize,
    max_age_hours: u64,
}

impl MessageQueue {
    pub fn new(max_per_device: usize, max_age_hours: u64) -> Self {
        Self {
            queues: Arc::new(RwLock::new(HashMap::new())),
            max_per_device,
            max_age_hours,
        }
    }

    /// Enqueue a message for a device. If the queue is full, drops the
    /// lowest-priority oldest message.
    pub async fn enqueue(&self, device_id: &str, frame_json: String, priority: Priority) {
        let mut queues = self.queues.write().await;
        let queue = queues.entry(device_id.to_string()).or_default();

        // If at capacity, evict lowest-priority oldest message
        if queue.len() >= self.max_per_device {
            // Find the index of the lowest-priority oldest message
            let evict_idx = queue
                .iter()
                .enumerate()
                .min_by_key(|(_, m)| (m.priority.numeric(), std::cmp::Reverse(m.created_at)))
                .map(|(i, _)| i);

            if let Some(idx) = evict_idx {
                // Only evict if the new message is higher or equal priority
                if priority.numeric() >= queue[idx].priority.numeric() {
                    queue.remove(idx);
                    warn!(
                        device_id = %device_id,
                        "Queue full, evicted lowest-priority message"
                    );
                } else {
                    // New message is lower priority than everything in queue — drop it
                    warn!(
                        device_id = %device_id,
                        "Queue full, dropping low-priority message"
                    );
                    return;
                }
            }
        }

        queue.push_back(QueuedMessage {
            frame_json,
            priority,
            created_at: chrono::Utc::now(),
        });
    }

    /// Drain all queued messages for a device (on reconnect).
    /// Returns messages ordered by priority desc, then created_at asc.
    pub async fn drain(&self, device_id: &str) -> Vec<QueuedMessage> {
        let mut queues = self.queues.write().await;
        let mut messages: Vec<QueuedMessage> = queues
            .remove(device_id)
            .unwrap_or_default()
            .into_iter()
            .collect();

        // Sort: highest priority first, then oldest first within same priority
        messages.sort_by(|a, b| {
            b.priority
                .numeric()
                .cmp(&a.priority.numeric())
                .then(a.created_at.cmp(&b.created_at))
        });

        if !messages.is_empty() {
            info!(
                device_id = %device_id,
                count = messages.len(),
                "Drained queued messages"
            );
        }
        messages
    }

    /// Remove expired messages from all queues.
    pub async fn cleanup_expired(&self) {
        let cutoff = chrono::Utc::now() - chrono::Duration::hours(self.max_age_hours as i64);
        let mut queues = self.queues.write().await;
        let mut total_removed = 0;

        queues.retain(|_, queue| {
            let before = queue.len();
            queue.retain(|m| m.created_at > cutoff);
            total_removed += before - queue.len();
            !queue.is_empty()
        });

        if total_removed > 0 {
            info!(
                removed = total_removed,
                "Cleaned up expired queued messages"
            );
        }
    }

    /// Get total queued message count across all devices.
    pub async fn total_count(&self) -> usize {
        self.queues.read().await.values().map(|q| q.len()).sum()
    }

    /// Get queued message count for a specific device.
    pub async fn count_for(&self, device_id: &str) -> usize {
        self.queues
            .read()
            .await
            .get(device_id)
            .map(|q| q.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_enqueue_and_drain() {
        let queue = MessageQueue::new(100, 24);
        queue
            .enqueue("device-1", "msg1".to_string(), Priority::Normal)
            .await;
        queue
            .enqueue("device-1", "msg2".to_string(), Priority::Normal)
            .await;

        let messages = queue.drain("device-1").await;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].frame_json, "msg1");
        assert_eq!(messages[1].frame_json, "msg2");

        // Queue should be empty after drain
        let empty = queue.drain("device-1").await;
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn test_drain_priority_ordering() {
        let queue = MessageQueue::new(100, 24);
        queue
            .enqueue("device-1", "low".to_string(), Priority::Low)
            .await;
        queue
            .enqueue("device-1", "critical".to_string(), Priority::Critical)
            .await;
        queue
            .enqueue("device-1", "normal".to_string(), Priority::Normal)
            .await;

        let messages = queue.drain("device-1").await;
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].frame_json, "critical");
        assert_eq!(messages[1].frame_json, "normal");
        assert_eq!(messages[2].frame_json, "low");
    }

    #[tokio::test]
    async fn test_bounded_eviction() {
        let queue = MessageQueue::new(2, 24);
        queue
            .enqueue("device-1", "low".to_string(), Priority::Low)
            .await;
        queue
            .enqueue("device-1", "normal".to_string(), Priority::Normal)
            .await;
        // Queue is full (2). Adding critical should evict the low-priority message.
        queue
            .enqueue("device-1", "critical".to_string(), Priority::Critical)
            .await;

        let messages = queue.drain("device-1").await;
        assert_eq!(messages.len(), 2);
        // Should have critical and normal (low was evicted)
        assert_eq!(messages[0].frame_json, "critical");
        assert_eq!(messages[1].frame_json, "normal");
    }

    #[tokio::test]
    async fn test_separate_device_queues() {
        let queue = MessageQueue::new(100, 24);
        queue
            .enqueue("device-1", "msg1".to_string(), Priority::Normal)
            .await;
        queue
            .enqueue("device-2", "msg2".to_string(), Priority::Normal)
            .await;

        assert_eq!(queue.total_count().await, 2);
        assert_eq!(queue.count_for("device-1").await, 1);
        assert_eq!(queue.count_for("device-2").await, 1);

        let d1 = queue.drain("device-1").await;
        assert_eq!(d1.len(), 1);
        assert_eq!(d1[0].frame_json, "msg1");

        // device-2 still has its message
        assert_eq!(queue.count_for("device-2").await, 1);
    }

    #[tokio::test]
    async fn test_drain_empty() {
        let queue = MessageQueue::new(100, 24);
        let messages = queue.drain("nonexistent").await;
        assert!(messages.is_empty());
    }

    #[tokio::test]
    async fn test_cleanup_expired() {
        let queue = MessageQueue::new(100, 0); // 0 hours = expire immediately
        queue
            .enqueue("device-1", "msg1".to_string(), Priority::Normal)
            .await;

        // Wait a tiny bit so the message is in the past
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        queue.cleanup_expired().await;
        assert_eq!(queue.total_count().await, 0);
    }
}
