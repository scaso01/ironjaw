//! File snapshot system: LRU cache of file contents before modification.
//! Enables undo/rollback for write_file and patch tools.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct FileSnapshot {
    pub content: Vec<u8>,
    pub taken_at: chrono::DateTime<chrono::Utc>,
}

pub struct FileSnapshotStore {
    snapshots: HashMap<PathBuf, VecDeque<FileSnapshot>>,
    access_order: VecDeque<PathBuf>, // LRU tracking
    max_files: usize,
    max_bytes: usize,
    current_bytes: usize,
}

impl FileSnapshotStore {
    pub fn new(max_files: usize, max_bytes: usize) -> Self {
        Self {
            snapshots: HashMap::new(),
            access_order: VecDeque::new(),
            max_files,
            max_bytes,
            current_bytes: 0,
        }
    }

    pub fn snapshot(&mut self, path: &Path, content: &[u8]) {
        let size = content.len();

        // Evict until we have room for this snapshot
        while self.current_bytes + size > self.max_bytes && !self.access_order.is_empty() {
            self.evict_oldest();
        }

        // Evict by file count
        while self.snapshots.len() >= self.max_files && !self.snapshots.contains_key(path) {
            self.evict_oldest();
        }

        let entry = FileSnapshot {
            content: content.to_vec(),
            taken_at: chrono::Utc::now(),
        };

        self.current_bytes += size;
        self.access_order.retain(|p| p != path);
        self.access_order.push_back(path.to_path_buf());

        self.snapshots
            .entry(path.to_path_buf())
            .or_default()
            .push_back(entry);
    }

    pub fn get_latest(&self, path: &Path) -> Option<Vec<u8>> {
        self.snapshots
            .get(path)
            .and_then(|versions| versions.back())
            .map(|s| s.content.clone())
    }

    pub fn get_history(&self, path: &Path) -> Vec<&FileSnapshot> {
        self.snapshots
            .get(path)
            .map(|versions| versions.iter().collect())
            .unwrap_or_default()
    }

    pub fn file_count(&self) -> usize {
        self.snapshots.len()
    }

    pub fn total_bytes(&self) -> usize {
        self.current_bytes
    }

    fn evict_oldest(&mut self) {
        if let Some(oldest_path) = self.access_order.pop_front() {
            if let Some(versions) = self.snapshots.remove(&oldest_path) {
                let freed: usize = versions.iter().map(|s| s.content.len()).sum();
                self.current_bytes = self.current_bytes.saturating_sub(freed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_store_and_retrieve() {
        let mut store = FileSnapshotStore::new(100, 25 * 1024 * 1024);
        let path = PathBuf::from("/tmp/test.txt");
        store.snapshot(&path, b"original content");

        let restored = store.get_latest(&path);
        assert_eq!(restored, Some(b"original content".to_vec()));
    }

    #[test]
    fn test_snapshot_preserves_multiple_versions() {
        let mut store = FileSnapshotStore::new(100, 25 * 1024 * 1024);
        let path = PathBuf::from("/tmp/test.txt");
        store.snapshot(&path, b"version 1");
        store.snapshot(&path, b"version 2");

        // Latest should be version 2
        assert_eq!(store.get_latest(&path), Some(b"version 2".to_vec()));
        // History should have both
        let history = store.get_history(&path);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn test_snapshot_evicts_when_over_file_limit() {
        let mut store = FileSnapshotStore::new(2, 25 * 1024 * 1024); // Max 2 files
        store.snapshot(&PathBuf::from("/a"), b"a");
        store.snapshot(&PathBuf::from("/b"), b"b");
        store.snapshot(&PathBuf::from("/c"), b"c"); // Evicts /a

        assert!(store.get_latest(&PathBuf::from("/a")).is_none());
        assert!(store.get_latest(&PathBuf::from("/b")).is_some());
        assert!(store.get_latest(&PathBuf::from("/c")).is_some());
    }

    #[test]
    fn test_snapshot_evicts_when_over_byte_limit() {
        let mut store = FileSnapshotStore::new(100, 20); // Max 20 bytes
        store.snapshot(&PathBuf::from("/a"), b"12345678901"); // 11 bytes
        store.snapshot(&PathBuf::from("/b"), b"12345678901"); // 11 bytes, evicts /a

        assert!(store.get_latest(&PathBuf::from("/a")).is_none());
        assert!(store.get_latest(&PathBuf::from("/b")).is_some());
    }

    #[test]
    fn test_file_count_and_total_bytes() {
        let mut store = FileSnapshotStore::new(100, 25 * 1024 * 1024);
        store.snapshot(&PathBuf::from("/a"), b"hello");
        store.snapshot(&PathBuf::from("/b"), b"world");

        assert_eq!(store.file_count(), 2);
        assert_eq!(store.total_bytes(), 10);
    }

    #[test]
    fn test_empty_store() {
        let store = FileSnapshotStore::new(100, 25 * 1024 * 1024);
        assert!(store.get_latest(&PathBuf::from("/a")).is_none());
        assert_eq!(store.get_history(&PathBuf::from("/a")).len(), 0);
        assert_eq!(store.file_count(), 0);
        assert_eq!(store.total_bytes(), 0);
    }
}
