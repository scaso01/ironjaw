use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use ring::digest;
use tokio::sync::Mutex;

/// Configuration for the LLM response cache.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub enabled: bool,
    pub capacity: usize,
    pub ttl_secs: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            capacity: 100,
            ttl_secs: 300,
        }
    }
}

/// A cached LLM response with insertion timestamp.
#[derive(Debug, Clone)]
struct CacheEntry {
    response: String,
    inserted_at: Instant,
}

/// Inner state for the LRU cache.
#[derive(Debug)]
struct CacheInner {
    map: HashMap<String, CacheEntry>,
    order: VecDeque<String>,
    capacity: usize,
    ttl_secs: u64,
}

/// Thread-safe LRU cache for LLM responses.
///
/// Uses `HashMap` + `VecDeque` for O(n) LRU eviction (sufficient for
/// the expected capacity of ~100 entries). Cache keys are SHA-256
/// hashes of (model + messages JSON).
#[derive(Debug, Clone)]
pub struct LlmCache {
    inner: Arc<Mutex<CacheInner>>,
}

impl LlmCache {
    /// Create a new cache with the given configuration.
    pub fn new(config: CacheConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(CacheInner {
                map: HashMap::new(),
                order: VecDeque::new(),
                capacity: config.capacity,
                ttl_secs: config.ttl_secs,
            })),
        }
    }

    /// Look up a cached response by key.
    ///
    /// Returns `None` if the key is not present or has expired.
    /// On a hit, the entry is moved to the back of the LRU queue
    /// (most-recently-used position).
    pub async fn get(&self, key: &str) -> Option<String> {
        let mut inner = self.inner.lock().await;
        let entry = inner.map.get(key)?;

        // Check TTL
        if entry.inserted_at.elapsed().as_secs() >= inner.ttl_secs {
            // Expired -- remove it
            inner.map.remove(key);
            inner.order.retain(|k| k != key);
            return None;
        }

        let response = entry.response.clone();

        // Move to back (most-recently-used)
        inner.order.retain(|k| k != key);
        inner.order.push_back(key.to_string());

        Some(response)
    }

    /// Insert a response into the cache.
    ///
    /// If the cache is at capacity, the least-recently-used entry is
    /// evicted first. If the key already exists, its value and
    /// timestamp are updated.
    pub async fn put(&self, key: &str, response: String) {
        let mut inner = self.inner.lock().await;

        // If key already exists, remove old order entry
        if inner.map.contains_key(key) {
            inner.order.retain(|k| k != key);
        } else if inner.map.len() >= inner.capacity {
            // Evict the least-recently-used (front of queue)
            if let Some(evicted) = inner.order.pop_front() {
                inner.map.remove(&evicted);
            }
        }

        inner.map.insert(
            key.to_string(),
            CacheEntry {
                response,
                inserted_at: Instant::now(),
            },
        );
        inner.order.push_back(key.to_string());
    }

    /// Compute a cache key from a model name and message array.
    ///
    /// The key is the hex-encoded SHA-256 digest of `model` concatenated
    /// with the compact JSON serialisation of `messages`.
    pub fn make_key(model: &str, messages: &[serde_json::Value]) -> String {
        let messages_json = serde_json::to_string(messages).unwrap_or_default();
        let input = format!("{model}{messages_json}");
        let digest = digest::digest(&digest::SHA256, input.as_bytes());
        hex_encode(digest.as_ref())
    }

    /// Remove all entries from the cache.
    pub async fn clear(&self) {
        let mut inner = self.inner.lock().await;
        inner.map.clear();
        inner.order.clear();
    }

    /// Return the number of entries currently in the cache.
    pub async fn len(&self) -> usize {
        let inner = self.inner.lock().await;
        inner.map.len()
    }

    /// Return whether the cache is empty.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

/// Encode bytes as a lowercase hex string.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(capacity: usize, ttl_secs: u64) -> CacheConfig {
        CacheConfig {
            enabled: true,
            capacity,
            ttl_secs,
        }
    }

    #[tokio::test]
    async fn test_basic_put_and_get() {
        let cache = LlmCache::new(config(10, 300));
        cache.put("k1", "response-1".into()).await;
        assert_eq!(cache.get("k1").await, Some("response-1".into()));
    }

    #[tokio::test]
    async fn test_cache_miss() {
        let cache = LlmCache::new(config(10, 300));
        assert_eq!(cache.get("nonexistent").await, None);
    }

    #[tokio::test]
    async fn test_ttl_expiry() {
        let cache = LlmCache::new(config(10, 0)); // TTL = 0 seconds
        cache.put("k1", "response".into()).await;
        // With TTL=0, the entry should be expired immediately
        assert_eq!(cache.get("k1").await, None);
    }

    #[tokio::test]
    async fn test_lru_eviction() {
        let cache = LlmCache::new(config(2, 300));
        cache.put("k1", "r1".into()).await;
        cache.put("k2", "r2".into()).await;
        // Cache is full (2/2). Insert k3 -> should evict k1 (LRU).
        cache.put("k3", "r3".into()).await;

        assert_eq!(cache.get("k1").await, None);
        assert_eq!(cache.get("k2").await, Some("r2".into()));
        assert_eq!(cache.get("k3").await, Some("r3".into()));
    }

    #[tokio::test]
    async fn test_lru_access_refreshes_order() {
        let cache = LlmCache::new(config(2, 300));
        cache.put("k1", "r1".into()).await;
        cache.put("k2", "r2".into()).await;

        // Access k1 to make it most-recently-used
        let _ = cache.get("k1").await;

        // Insert k3 -> should evict k2 (now LRU, not k1)
        cache.put("k3", "r3".into()).await;

        assert_eq!(cache.get("k1").await, Some("r1".into()));
        assert_eq!(cache.get("k2").await, None);
        assert_eq!(cache.get("k3").await, Some("r3".into()));
    }

    #[tokio::test]
    async fn test_clear() {
        let cache = LlmCache::new(config(10, 300));
        cache.put("k1", "r1".into()).await;
        cache.put("k2", "r2".into()).await;
        assert_eq!(cache.len().await, 2);

        cache.clear().await;
        assert_eq!(cache.len().await, 0);
        assert_eq!(cache.get("k1").await, None);
    }

    #[tokio::test]
    async fn test_len() {
        let cache = LlmCache::new(config(10, 300));
        assert_eq!(cache.len().await, 0);
        cache.put("k1", "r1".into()).await;
        assert_eq!(cache.len().await, 1);
        cache.put("k2", "r2".into()).await;
        assert_eq!(cache.len().await, 2);
    }

    #[tokio::test]
    async fn test_make_key_deterministic() {
        let msgs = vec![json!({"role": "user", "content": "hello"})];
        let key1 = LlmCache::make_key("llama", &msgs);
        let key2 = LlmCache::make_key("llama", &msgs);
        assert_eq!(key1, key2);
    }

    #[tokio::test]
    async fn test_make_key_different_for_different_input() {
        let msgs = vec![json!({"role": "user", "content": "hello"})];
        let key1 = LlmCache::make_key("model-a", &msgs);
        let key2 = LlmCache::make_key("model-b", &msgs);
        assert_ne!(key1, key2);

        let msgs2 = vec![json!({"role": "user", "content": "goodbye"})];
        let key3 = LlmCache::make_key("model-a", &msgs2);
        assert_ne!(key1, key3);
    }

    #[tokio::test]
    async fn test_make_key_is_valid_sha256_hex() {
        let msgs = vec![json!({"role": "user", "content": "test"})];
        let key = LlmCache::make_key("model", &msgs);
        // SHA-256 produces 32 bytes = 64 hex chars
        assert_eq!(key.len(), 64);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn test_overwrite_existing_key() {
        let cache = LlmCache::new(config(10, 300));
        cache.put("k1", "old".into()).await;
        cache.put("k1", "new".into()).await;
        assert_eq!(cache.get("k1").await, Some("new".into()));
        assert_eq!(cache.len().await, 1);
    }

    #[tokio::test]
    async fn test_capacity_enforcement() {
        let cache = LlmCache::new(config(3, 300));
        for i in 0..10 {
            cache.put(&format!("k{i}"), format!("r{i}")).await;
        }
        // Should never exceed capacity
        assert_eq!(cache.len().await, 3);
    }

    #[tokio::test]
    async fn test_default_config() {
        let cfg = CacheConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.capacity, 100);
        assert_eq!(cfg.ttl_secs, 300);
    }
}
