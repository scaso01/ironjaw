//! Prefetch system: pre-computes brain context and prompt cache
//! during user idle time to reduce perceived latency.

use std::collections::HashMap;
use tokio::sync::RwLock;

pub struct PrefetchEntry {
    pub context_messages: Vec<String>,
    pub prefetched_at: chrono::DateTime<chrono::Utc>,
}

pub struct PrefetchManager {
    cache: RwLock<HashMap<String, PrefetchEntry>>,
    debounce: RwLock<HashMap<String, chrono::DateTime<chrono::Utc>>>,
    debounce_secs: i64,
}

impl PrefetchManager {
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            debounce: RwLock::new(HashMap::new()),
            debounce_secs: 5,
        }
    }

    pub async fn store_prefetched(&self, session_id: &str, context: Vec<String>) {
        let mut cache = self.cache.write().await;
        cache.insert(
            session_id.to_string(),
            PrefetchEntry {
                context_messages: context,
                prefetched_at: chrono::Utc::now(),
            },
        );
    }

    pub async fn get_prefetched(&self, session_id: &str) -> Option<Vec<String>> {
        let cache = self.cache.read().await;
        cache.get(session_id).map(|e| e.context_messages.clone())
    }

    pub async fn invalidate(&self, session_id: &str) {
        let mut cache = self.cache.write().await;
        cache.remove(session_id);
    }

    pub async fn should_prefetch(&self, session_id: &str) -> bool {
        let debounce = self.debounce.read().await;
        if let Some(last) = debounce.get(session_id) {
            let elapsed = chrono::Utc::now()
                .signed_duration_since(*last)
                .num_seconds();
            elapsed >= self.debounce_secs
        } else {
            true
        }
    }

    pub async fn mark_prefetched(&self, session_id: &str) {
        let mut debounce = self.debounce.write().await;
        debounce.insert(session_id.to_string(), chrono::Utc::now());
    }
}

impl Default for PrefetchManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_prefetch_stores_context() {
        let pfm = PrefetchManager::new();
        pfm.store_prefetched(
            "session-1",
            vec!["memory 1".to_string(), "memory 2".to_string()],
        )
        .await;

        let cached = pfm.get_prefetched("session-1").await;
        assert_eq!(cached.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_prefetch_invalidates_on_new_message() {
        let pfm = PrefetchManager::new();
        pfm.store_prefetched("session-1", vec!["old context".to_string()])
            .await;
        pfm.invalidate("session-1").await;

        assert!(pfm.get_prefetched("session-1").await.is_none());
    }

    #[tokio::test]
    async fn test_prefetch_debounce() {
        let pfm = PrefetchManager::new();
        assert!(pfm.should_prefetch("session-1").await);
        pfm.mark_prefetched("session-1").await;
        // Should not prefetch again immediately
        assert!(!pfm.should_prefetch("session-1").await);
    }

    #[tokio::test]
    async fn test_prefetch_returns_none_for_unknown_session() {
        let pfm = PrefetchManager::new();
        assert!(pfm.get_prefetched("unknown").await.is_none());
    }
}
