//! In-memory cache for static-layer file contents.
//!
//! Re-reading CLAUDE.md files on every `chat.send` is wasteful.  This cache
//! stores the raw file text keyed by absolute path, with a TTL so that edits
//! to a CLAUDE.md are picked up without a gateway restart.
//!
//! Design goals:
//! - Lock-free reads via `DashMap`
//! - TTL-based invalidation (default 60 s)
//! - Zero dependency on the filesystem inside the hot path — callers pre-load
//!   via `insert()`, the cache hands back `Option<&str>`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A single cached entry: the file content plus the instant it was inserted.
#[derive(Clone)]
struct CacheEntry {
    content: Arc<String>,
    inserted_at: Instant,
}

/// Thread-safe, TTL-backed cache for static-layer file contents.
///
/// Keyed by `PathBuf` (canonicalized path of the CLAUDE.md file).
pub struct StaticLayerCache {
    store: dashmap::DashMap<PathBuf, CacheEntry>,
    ttl: Duration,
}

impl StaticLayerCache {
    /// Create a new cache with the given TTL.
    pub fn new(ttl: Duration) -> Self {
        Self {
            store: dashmap::DashMap::new(),
            ttl,
        }
    }

    /// Create a new cache with a 60-second TTL (production default).
    pub fn default_ttl() -> Self {
        Self::new(Duration::from_secs(60))
    }

    /// Insert or replace the cached content for `path`.
    pub fn insert(&self, path: &Path, content: String) {
        self.store.insert(
            path.to_path_buf(),
            CacheEntry {
                content: Arc::new(content),
                inserted_at: Instant::now(),
            },
        );
    }

    /// Return the cached content for `path` if it exists and is not expired.
    pub fn get(&self, path: &Path) -> Option<Arc<String>> {
        let entry = self.store.get(path)?;
        if entry.inserted_at.elapsed() < self.ttl {
            Some(Arc::clone(&entry.content))
        } else {
            drop(entry); // release the read lock before removing
            self.store.remove(path);
            None
        }
    }

    /// Evict all expired entries.  Useful to call periodically to keep memory
    /// bounded (though the cache is typically small: 2-5 files per project).
    pub fn evict_expired(&self) {
        self.store
            .retain(|_, entry| entry.inserted_at.elapsed() < self.ttl);
    }

    /// Number of currently cached entries (including potentially expired ones).
    pub fn len(&self) -> usize {
        self.store.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }
}

impl Default for StaticLayerCache {
    fn default() -> Self {
        Self::default_ttl()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn insert_and_get() {
        let cache = StaticLayerCache::new(Duration::from_secs(60));
        let path = PathBuf::from("/tmp/CLAUDE.md");
        cache.insert(&path, "# Hello".to_string());
        let got = cache.get(&path);
        assert!(got.is_some());
        assert_eq!(got.unwrap().as_str(), "# Hello");
    }

    #[test]
    fn miss_on_unknown_path() {
        let cache = StaticLayerCache::new(Duration::from_secs(60));
        let got = cache.get(&PathBuf::from("/nonexistent"));
        assert!(got.is_none());
    }

    #[test]
    fn expired_entry_returns_none() {
        // TTL of 1 ns — will expire immediately
        let cache = StaticLayerCache::new(Duration::from_nanos(1));
        let path = PathBuf::from("/tmp/CLAUDE.md");
        cache.insert(&path, "# Hello".to_string());
        // Spin until at least 2 ns have passed
        let start = Instant::now();
        while start.elapsed() < Duration::from_nanos(2) {
            std::hint::spin_loop();
        }
        let got = cache.get(&path);
        assert!(got.is_none(), "entry should have expired");
        assert!(cache.is_empty(), "expired entry should be evicted on get");
    }

    #[test]
    fn evict_expired_cleans_up() {
        let cache = StaticLayerCache::new(Duration::from_nanos(1));
        let path = PathBuf::from("/tmp/CLAUDE.md");
        cache.insert(&path, "# Hello".to_string());
        let start = Instant::now();
        while start.elapsed() < Duration::from_nanos(2) {
            std::hint::spin_loop();
        }
        cache.evict_expired();
        assert!(cache.is_empty());
    }
}
