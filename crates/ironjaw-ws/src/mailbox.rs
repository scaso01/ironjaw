//! Async mailbox for agent communication with claim semantics.
//! High-risk operations queue here for coordinator approval.
//! Atomic claim prevents duplicate handling.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum Priority {
    Low = 0,
    #[default]
    Normal = 1,
    High = 2,
    Critical = 3,
}

#[derive(Debug, Clone, Serialize)]
pub struct MailMessage {
    #[serde(skip_deserializing)]
    pub id: String,
    pub from: String,
    pub subject: String,
    pub body: serde_json::Value,
    pub priority: Priority,
    #[serde(skip)]
    pub claimed_by: Option<String>,
    #[serde(skip)]
    pub sent_at: chrono::DateTime<chrono::Utc>,
}

pub struct Mailbox {
    boxes: RwLock<HashMap<String, Vec<MailMessage>>>,
    max_per_box: usize,
}

impl Mailbox {
    pub fn new(max_per_box: usize) -> Self {
        Self {
            boxes: RwLock::new(HashMap::new()),
            max_per_box,
        }
    }

    pub async fn send(&self, to: &str, mut msg: MailMessage) {
        msg.id = uuid::Uuid::new_v4().to_string();
        msg.sent_at = chrono::Utc::now();
        msg.claimed_by = None;

        let mut boxes = self.boxes.write().await;
        let inbox = boxes.entry(to.to_string()).or_default();

        if inbox.len() >= self.max_per_box {
            inbox.remove(0); // Evict oldest
        }
        inbox.push(msg);
    }

    /// Peek at unclaimed messages, sorted by priority (highest first).
    pub async fn peek(&self, recipient: &str, limit: usize) -> Vec<MailMessage> {
        let boxes = self.boxes.read().await;
        let mut msgs: Vec<MailMessage> = boxes
            .get(recipient)
            .map(|inbox| {
                inbox
                    .iter()
                    .filter(|m| m.claimed_by.is_none())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        msgs.sort_by_key(|b| std::cmp::Reverse(b.priority));
        msgs.truncate(limit);
        msgs
    }

    /// Atomically claim a message. Returns false if already claimed.
    pub async fn claim(&self, message_id: &str, claimer: &str) -> bool {
        let mut boxes = self.boxes.write().await;
        for inbox in boxes.values_mut() {
            for msg in inbox.iter_mut() {
                if msg.id == message_id {
                    if msg.claimed_by.is_some() {
                        return false; // Already claimed
                    }
                    msg.claimed_by = Some(claimer.to_string());
                    return true;
                }
            }
        }
        false
    }

    pub async fn unclaimed_count(&self, recipient: &str) -> usize {
        let boxes = self.boxes.read().await;
        boxes
            .get(recipient)
            .map(|inbox| inbox.iter().filter(|m| m.claimed_by.is_none()).count())
            .unwrap_or(0)
    }

    /// List all recipients with pending unclaimed messages.
    pub async fn active_recipients(&self) -> Vec<String> {
        let boxes = self.boxes.read().await;
        boxes
            .iter()
            .filter(|(_, msgs)| msgs.iter().any(|m| m.claimed_by.is_none()))
            .map(|(k, _)| k.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mailbox_send_and_receive() {
        let mb = Mailbox::new(100);
        mb.send(
            "coordinator",
            MailMessage {
                id: String::new(),
                from: "worker-1".to_string(),
                subject: "permission_request".to_string(),
                body: serde_json::json!({"tool": "shell", "command": "rm -rf /tmp"}),
                priority: Priority::High,
                claimed_by: None,
                sent_at: chrono::Utc::now(),
            },
        )
        .await;

        let msgs = mb.peek("coordinator", 10).await;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, "worker-1");
    }

    #[tokio::test]
    async fn test_mailbox_claim_prevents_duplicate_handling() {
        let mb = Mailbox::new(100);
        mb.send(
            "coordinator",
            MailMessage {
                id: String::new(),
                from: "worker-1".to_string(),
                subject: "approval".to_string(),
                body: serde_json::json!({}),
                priority: Priority::Normal,
                claimed_by: None,
                sent_at: chrono::Utc::now(),
            },
        )
        .await;

        let msgs = mb.peek("coordinator", 10).await;
        let msg_id = &msgs[0].id;

        // First claim succeeds
        assert!(mb.claim(msg_id, "handler-A").await);
        // Second claim fails (already claimed)
        assert!(!mb.claim(msg_id, "handler-B").await);
    }

    #[tokio::test]
    async fn test_mailbox_priority_ordering() {
        let mb = Mailbox::new(100);
        mb.send(
            "coord",
            MailMessage {
                id: String::new(),
                from: "w1".to_string(),
                subject: "low".to_string(),
                body: serde_json::json!({}),
                priority: Priority::Low,
                claimed_by: None,
                sent_at: chrono::Utc::now(),
            },
        )
        .await;
        mb.send(
            "coord",
            MailMessage {
                id: String::new(),
                from: "w2".to_string(),
                subject: "high".to_string(),
                body: serde_json::json!({}),
                priority: Priority::High,
                claimed_by: None,
                sent_at: chrono::Utc::now(),
            },
        )
        .await;

        let msgs = mb.peek("coord", 10).await;
        assert_eq!(msgs[0].priority, Priority::High);
        assert_eq!(msgs[1].priority, Priority::Low);
    }

    #[tokio::test]
    async fn test_mailbox_unclaimed_count() {
        let mb = Mailbox::new(100);
        mb.send(
            "coord",
            MailMessage {
                id: String::new(),
                from: "w1".to_string(),
                subject: "msg1".to_string(),
                body: serde_json::json!({}),
                priority: Priority::Normal,
                claimed_by: None,
                sent_at: chrono::Utc::now(),
            },
        )
        .await;
        mb.send(
            "coord",
            MailMessage {
                id: String::new(),
                from: "w2".to_string(),
                subject: "msg2".to_string(),
                body: serde_json::json!({}),
                priority: Priority::Normal,
                claimed_by: None,
                sent_at: chrono::Utc::now(),
            },
        )
        .await;

        assert_eq!(mb.unclaimed_count("coord").await, 2);

        let msgs = mb.peek("coord", 10).await;
        mb.claim(&msgs[0].id, "handler").await;

        assert_eq!(mb.unclaimed_count("coord").await, 1);
    }

    #[tokio::test]
    async fn test_mailbox_empty_recipient() {
        let mb = Mailbox::new(100);
        let msgs = mb.peek("nobody", 10).await;
        assert_eq!(msgs.len(), 0);
        assert_eq!(mb.unclaimed_count("nobody").await, 0);
    }

    #[tokio::test]
    async fn test_mailbox_evicts_oldest_when_full() {
        let mb = Mailbox::new(2); // Max 2 per box
        for i in 0..3 {
            mb.send(
                "coord",
                MailMessage {
                    id: String::new(),
                    from: format!("w{i}"),
                    subject: format!("msg{i}"),
                    body: serde_json::json!({}),
                    priority: Priority::Normal,
                    claimed_by: None,
                    sent_at: chrono::Utc::now(),
                },
            )
            .await;
        }

        let msgs = mb.peek("coord", 10).await;
        assert_eq!(msgs.len(), 2);
        // First message (w0) should have been evicted
        assert_eq!(msgs[0].from, "w1");
    }

    #[tokio::test]
    async fn test_claim_nonexistent_returns_false() {
        let mb = Mailbox::new(100);
        assert!(!mb.claim("nonexistent-id", "handler").await);
    }

    #[tokio::test]
    async fn test_active_recipients() {
        let mb = Mailbox::new(100);
        mb.send(
            "agent-a",
            MailMessage {
                id: String::new(),
                from: "sender".to_string(),
                subject: "test".to_string(),
                body: serde_json::json!({}),
                priority: Priority::Normal,
                claimed_by: None,
                sent_at: chrono::Utc::now(),
            },
        )
        .await;

        let recipients = mb.active_recipients().await;
        assert_eq!(recipients.len(), 1);
        assert!(recipients.contains(&"agent-a".to_string()));
    }

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::Critical > Priority::High);
        assert!(Priority::High > Priority::Normal);
        assert!(Priority::Normal > Priority::Low);
    }

    #[test]
    fn test_priority_default() {
        assert_eq!(Priority::default(), Priority::Normal);
    }
}
