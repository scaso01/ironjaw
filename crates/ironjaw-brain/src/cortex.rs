//! Memory Bulletin / Cortex (Stream 8.4)
//!
//! A shared memory bulletin board where agents can publish and subscribe
//! to memory updates. Acts as a pub/sub layer on top of the memory store.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A bulletin posted to the cortex bulletin board.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bulletin {
    pub id: String,
    pub topic: String,
    pub content: String,
    /// Agent or user who posted this bulletin.
    pub author: String,
    pub posted_at: DateTime<Utc>,
    /// Optional expiration time. `None` means the bulletin never expires.
    pub expires_at: Option<DateTime<Utc>>,
    pub metadata: HashMap<String, serde_json::Value>,
    /// Pinned bulletins are protected from eviction when the max is reached.
    pub pinned: bool,
}

/// A subscription linking a subscriber to a topic pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub subscriber_id: String,
    /// Exact topic match, or `"*"` for all topics.
    pub topic_pattern: String,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Cortex
// ---------------------------------------------------------------------------

/// Shared memory bulletin board with pub/sub semantics.
///
/// When `max_bulletins` is reached, the oldest non-pinned bulletins are
/// removed to make room for new ones.
pub struct Cortex {
    bulletins: Arc<RwLock<Vec<Bulletin>>>,
    subscriptions: Arc<RwLock<Vec<Subscription>>>,
    max_bulletins: usize,
}

impl Cortex {
    /// Create a new cortex with the given maximum bulletin capacity.
    pub fn new(max_bulletins: usize) -> Self {
        Self {
            bulletins: Arc::new(RwLock::new(Vec::new())),
            subscriptions: Arc::new(RwLock::new(Vec::new())),
            max_bulletins,
        }
    }

    // ── Bulletin Operations ────────────────────────────────────────

    /// Publish a bulletin, assigning it a unique ID. Returns the bulletin ID.
    ///
    /// If the bulletin count exceeds `max_bulletins`, the oldest non-pinned
    /// bulletin is removed to make room.
    pub async fn publish(&self, mut bulletin: Bulletin) -> String {
        let id = Uuid::new_v4().to_string();
        bulletin.id = id.clone();

        let mut bulletins = self.bulletins.write().await;

        // Evict oldest non-pinned bulletins if at capacity
        while bulletins.len() >= self.max_bulletins {
            let oldest_non_pinned = bulletins
                .iter()
                .enumerate()
                .find(|(_, b)| !b.pinned)
                .map(|(i, _)| i);

            match oldest_non_pinned {
                Some(idx) => {
                    bulletins.remove(idx);
                }
                None => break, // all pinned, cannot evict
            }
        }

        bulletins.push(bulletin);
        id
    }

    /// Get a bulletin by ID.
    pub async fn get(&self, id: &str) -> Option<Bulletin> {
        let bulletins = self.bulletins.read().await;
        bulletins.iter().find(|b| b.id == id).cloned()
    }

    /// List all bulletins for a given topic.
    pub async fn list_topic(&self, topic: &str) -> Vec<Bulletin> {
        let bulletins = self.bulletins.read().await;
        bulletins
            .iter()
            .filter(|b| b.topic == topic)
            .cloned()
            .collect()
    }

    /// List all bulletins.
    pub async fn list_all(&self) -> Vec<Bulletin> {
        let bulletins = self.bulletins.read().await;
        bulletins.clone()
    }

    /// Pin a bulletin. Returns `true` if the bulletin was found and pinned.
    pub async fn pin(&self, id: &str) -> bool {
        let mut bulletins = self.bulletins.write().await;
        if let Some(b) = bulletins.iter_mut().find(|b| b.id == id) {
            b.pinned = true;
            return true;
        }
        false
    }

    /// Unpin a bulletin. Returns `true` if the bulletin was found and unpinned.
    pub async fn unpin(&self, id: &str) -> bool {
        let mut bulletins = self.bulletins.write().await;
        if let Some(b) = bulletins.iter_mut().find(|b| b.id == id) {
            b.pinned = false;
            return true;
        }
        false
    }

    /// Remove a bulletin by ID. Returns `true` if it was found and removed.
    pub async fn remove(&self, id: &str) -> bool {
        let mut bulletins = self.bulletins.write().await;
        let len_before = bulletins.len();
        bulletins.retain(|b| b.id != id);
        bulletins.len() < len_before
    }

    /// Remove all expired bulletins. Returns the count of removed bulletins.
    pub async fn remove_expired(&self) -> usize {
        let now = Utc::now();
        let mut bulletins = self.bulletins.write().await;
        let len_before = bulletins.len();
        bulletins.retain(|b| match b.expires_at {
            Some(exp) => exp > now,
            None => true,
        });
        len_before - bulletins.len()
    }

    // ── Subscription Operations ────────────────────────────────────

    /// Subscribe a subscriber to a topic pattern.
    ///
    /// Use `"*"` as the pattern to subscribe to all topics.
    /// Duplicate subscriptions (same subscriber + pattern) are ignored.
    pub async fn subscribe(&self, subscriber_id: &str, topic_pattern: &str) {
        let mut subs = self.subscriptions.write().await;

        // Prevent duplicate subscriptions
        let exists = subs
            .iter()
            .any(|s| s.subscriber_id == subscriber_id && s.topic_pattern == topic_pattern);
        if exists {
            return;
        }

        subs.push(Subscription {
            subscriber_id: subscriber_id.to_string(),
            topic_pattern: topic_pattern.to_string(),
            created_at: Utc::now(),
        });
    }

    /// Unsubscribe from a specific topic pattern.
    /// Returns `true` if the subscription was found and removed.
    pub async fn unsubscribe(&self, subscriber_id: &str, topic_pattern: &str) -> bool {
        let mut subs = self.subscriptions.write().await;
        let len_before = subs.len();
        subs.retain(|s| !(s.subscriber_id == subscriber_id && s.topic_pattern == topic_pattern));
        subs.len() < len_before
    }

    /// Get all subscriptions for a given subscriber.
    pub async fn get_subscriptions(&self, subscriber_id: &str) -> Vec<Subscription> {
        let subs = self.subscriptions.read().await;
        subs.iter()
            .filter(|s| s.subscriber_id == subscriber_id)
            .cloned()
            .collect()
    }

    /// Get all subscriber IDs that match the given topic.
    ///
    /// A subscriber matches if they have an exact topic subscription or
    /// a `"*"` wildcard subscription.
    pub async fn get_subscribers(&self, topic: &str) -> Vec<String> {
        let subs = self.subscriptions.read().await;
        let mut ids: Vec<String> = subs
            .iter()
            .filter(|s| s.topic_pattern == "*" || s.topic_pattern == topic)
            .map(|s| s.subscriber_id.clone())
            .collect();
        ids.dedup();
        ids
    }

    /// Get all bulletins relevant to a subscriber based on their
    /// subscriptions. Returns bulletins whose topic matches any of
    /// the subscriber's topic patterns.
    pub async fn inbox(&self, subscriber_id: &str) -> Vec<Bulletin> {
        let subs = self.subscriptions.read().await;
        let patterns: Vec<&str> = subs
            .iter()
            .filter(|s| s.subscriber_id == subscriber_id)
            .map(|s| s.topic_pattern.as_str())
            .collect();

        if patterns.is_empty() {
            return Vec::new();
        }

        let has_wildcard = patterns.contains(&"*");

        let bulletins = self.bulletins.read().await;
        bulletins
            .iter()
            .filter(|b| has_wildcard || patterns.contains(&b.topic.as_str()))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bulletin(topic: &str, content: &str, author: &str) -> Bulletin {
        Bulletin {
            id: String::new(), // assigned by publish()
            topic: topic.to_string(),
            content: content.to_string(),
            author: author.to_string(),
            posted_at: Utc::now(),
            expires_at: None,
            metadata: HashMap::new(),
            pinned: false,
        }
    }

    #[tokio::test]
    async fn test_publish_and_get() {
        let cortex = Cortex::new(100);
        let id = cortex
            .publish(make_bulletin("status", "all systems go", "agent-1"))
            .await;

        let bulletin = cortex.get(&id).await.unwrap();
        assert_eq!(bulletin.topic, "status");
        assert_eq!(bulletin.content, "all systems go");
        assert_eq!(bulletin.author, "agent-1");
        assert!(!bulletin.id.is_empty());
    }

    #[tokio::test]
    async fn test_get_not_found() {
        let cortex = Cortex::new(100);
        assert!(cortex.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_list_all() {
        let cortex = Cortex::new(100);
        cortex.publish(make_bulletin("a", "first", "agent-1")).await;
        cortex
            .publish(make_bulletin("b", "second", "agent-2"))
            .await;

        let all = cortex.list_all().await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_list_topic() {
        let cortex = Cortex::new(100);
        cortex
            .publish(make_bulletin("status", "ok", "agent-1"))
            .await;
        cortex
            .publish(make_bulletin("alert", "fire", "agent-2"))
            .await;
        cortex
            .publish(make_bulletin("status", "running", "agent-3"))
            .await;

        let status = cortex.list_topic("status").await;
        assert_eq!(status.len(), 2);
        assert!(status.iter().all(|b| b.topic == "status"));

        let alert = cortex.list_topic("alert").await;
        assert_eq!(alert.len(), 1);

        let empty = cortex.list_topic("missing").await;
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn test_pin_and_unpin() {
        let cortex = Cortex::new(100);
        let id = cortex
            .publish(make_bulletin("status", "test", "agent-1"))
            .await;

        assert!(!cortex.get(&id).await.unwrap().pinned);

        assert!(cortex.pin(&id).await);
        assert!(cortex.get(&id).await.unwrap().pinned);

        assert!(cortex.unpin(&id).await);
        assert!(!cortex.get(&id).await.unwrap().pinned);
    }

    #[tokio::test]
    async fn test_pin_not_found() {
        let cortex = Cortex::new(100);
        assert!(!cortex.pin("nonexistent").await);
        assert!(!cortex.unpin("nonexistent").await);
    }

    #[tokio::test]
    async fn test_remove() {
        let cortex = Cortex::new(100);
        let id = cortex
            .publish(make_bulletin("status", "test", "agent-1"))
            .await;

        assert!(cortex.remove(&id).await);
        assert!(cortex.get(&id).await.is_none());
        assert!(!cortex.remove(&id).await); // already removed
    }

    #[tokio::test]
    async fn test_remove_expired() {
        let cortex = Cortex::new(100);

        // Publish a bulletin that is already expired
        let mut expired = make_bulletin("status", "old", "agent-1");
        expired.expires_at = Some(Utc::now() - chrono::Duration::hours(1));
        cortex.publish(expired).await;

        // Publish a bulletin that is still valid
        let mut valid = make_bulletin("status", "new", "agent-2");
        valid.expires_at = Some(Utc::now() + chrono::Duration::hours(1));
        cortex.publish(valid).await;

        // Publish a bulletin with no expiration
        cortex
            .publish(make_bulletin("status", "forever", "agent-3"))
            .await;

        let removed = cortex.remove_expired().await;
        assert_eq!(removed, 1);
        assert_eq!(cortex.list_all().await.len(), 2);
    }

    #[tokio::test]
    async fn test_max_bulletins_eviction() {
        let cortex = Cortex::new(3);

        cortex.publish(make_bulletin("a", "first", "agent-1")).await;
        cortex
            .publish(make_bulletin("b", "second", "agent-1"))
            .await;
        cortex.publish(make_bulletin("c", "third", "agent-1")).await;

        // At capacity. Next publish should evict the oldest non-pinned.
        cortex
            .publish(make_bulletin("d", "fourth", "agent-1"))
            .await;

        let all = cortex.list_all().await;
        assert_eq!(all.len(), 3);
        // "first" (oldest, unpinned) should be gone
        assert!(all.iter().all(|b| b.content != "first"));
    }

    #[tokio::test]
    async fn test_max_bulletins_pinned_protected() {
        let cortex = Cortex::new(3);

        let id1 = cortex.publish(make_bulletin("a", "first", "agent-1")).await;
        cortex.pin(&id1).await;

        cortex
            .publish(make_bulletin("b", "second", "agent-1"))
            .await;
        cortex.publish(make_bulletin("c", "third", "agent-1")).await;

        // At capacity. "first" is pinned, so "second" should be evicted.
        cortex
            .publish(make_bulletin("d", "fourth", "agent-1"))
            .await;

        let all = cortex.list_all().await;
        assert_eq!(all.len(), 3);
        // "first" should still exist (pinned)
        assert!(all.iter().any(|b| b.content == "first"));
        // "second" should be evicted (oldest non-pinned)
        assert!(all.iter().all(|b| b.content != "second"));
    }

    #[tokio::test]
    async fn test_subscribe_and_get_subscriptions() {
        let cortex = Cortex::new(100);

        cortex.subscribe("agent-1", "status").await;
        cortex.subscribe("agent-1", "alerts").await;
        cortex.subscribe("agent-2", "status").await;

        let subs = cortex.get_subscriptions("agent-1").await;
        assert_eq!(subs.len(), 2);

        let subs2 = cortex.get_subscriptions("agent-2").await;
        assert_eq!(subs2.len(), 1);

        let empty = cortex.get_subscriptions("agent-3").await;
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn test_subscribe_dedup() {
        let cortex = Cortex::new(100);

        cortex.subscribe("agent-1", "status").await;
        cortex.subscribe("agent-1", "status").await; // duplicate

        let subs = cortex.get_subscriptions("agent-1").await;
        assert_eq!(subs.len(), 1);
    }

    #[tokio::test]
    async fn test_unsubscribe() {
        let cortex = Cortex::new(100);

        cortex.subscribe("agent-1", "status").await;
        cortex.subscribe("agent-1", "alerts").await;

        assert!(cortex.unsubscribe("agent-1", "status").await);
        assert!(!cortex.unsubscribe("agent-1", "status").await); // already gone

        let subs = cortex.get_subscriptions("agent-1").await;
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].topic_pattern, "alerts");
    }

    #[tokio::test]
    async fn test_get_subscribers() {
        let cortex = Cortex::new(100);

        cortex.subscribe("agent-1", "status").await;
        cortex.subscribe("agent-2", "status").await;
        cortex.subscribe("agent-3", "alerts").await;
        cortex.subscribe("agent-4", "*").await; // wildcard

        let status_subs = cortex.get_subscribers("status").await;
        assert_eq!(status_subs.len(), 3); // agent-1, agent-2, agent-4
        assert!(status_subs.contains(&"agent-1".to_string()));
        assert!(status_subs.contains(&"agent-2".to_string()));
        assert!(status_subs.contains(&"agent-4".to_string()));

        let alert_subs = cortex.get_subscribers("alerts").await;
        assert_eq!(alert_subs.len(), 2); // agent-3, agent-4
    }

    #[tokio::test]
    async fn test_inbox() {
        let cortex = Cortex::new(100);

        cortex
            .publish(make_bulletin("status", "ok", "system"))
            .await;
        cortex
            .publish(make_bulletin("alerts", "warning", "system"))
            .await;
        cortex
            .publish(make_bulletin("logs", "debug info", "system"))
            .await;

        cortex.subscribe("agent-1", "status").await;
        cortex.subscribe("agent-1", "alerts").await;

        let inbox = cortex.inbox("agent-1").await;
        assert_eq!(inbox.len(), 2);
        assert!(inbox.iter().any(|b| b.topic == "status"));
        assert!(inbox.iter().any(|b| b.topic == "alerts"));
    }

    #[tokio::test]
    async fn test_inbox_wildcard() {
        let cortex = Cortex::new(100);

        cortex
            .publish(make_bulletin("status", "ok", "system"))
            .await;
        cortex
            .publish(make_bulletin("alerts", "warning", "system"))
            .await;
        cortex
            .publish(make_bulletin("logs", "debug", "system"))
            .await;

        cortex.subscribe("agent-1", "*").await;

        let inbox = cortex.inbox("agent-1").await;
        assert_eq!(inbox.len(), 3); // all bulletins
    }

    #[tokio::test]
    async fn test_inbox_no_subscriptions() {
        let cortex = Cortex::new(100);

        cortex
            .publish(make_bulletin("status", "ok", "system"))
            .await;

        let inbox = cortex.inbox("unsubscribed-agent").await;
        assert!(inbox.is_empty());
    }
}
