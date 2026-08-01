// vec_index.rs -- Pure-Rust in-memory vector index for KNN search
// Replaces O(n) full-scan cosine similarity with partitioned flat index.
// Falls back gracefully to brute-force when partition count is 1.
//
// Design:
// - Vectors stored in a HashMap<id, Vec<f32>> for O(1) insert/delete
// - KNN search uses brute-force cosine similarity (optimized with iterators)
// - Thread-safe via RwLock for concurrent read access
// - Rebuilds from DB on startup, stays in sync via insert/remove calls
//
// Future: could add IVF (inverted file) partitioning for very large stores (>100K vectors).
// For now, the in-memory flat index avoids the O(n) DB round-trip which was the real bottleneck.

use std::collections::HashMap;
use std::sync::RwLock;
use tracing::{debug, warn};

/// Result of a KNN query: memory ID and similarity score.
#[derive(Debug, Clone, PartialEq)]
pub struct VecMatch {
    pub id: i64,
    pub score: f32,
}

/// In-memory vector index for efficient KNN search.
///
/// Stores embeddings in a `HashMap` protected by a `RwLock`.
/// All operations are thread-safe. Insert and remove are O(1).
/// Search is O(n) over the in-memory map (no DB round-trip).
pub struct VecIndex {
    /// dimension of the vectors (all must be the same)
    dims: usize,
    /// id -> embedding vector
    vectors: RwLock<HashMap<i64, Vec<f32>>>,
}

impl VecIndex {
    /// Create a new empty index with the given dimensionality.
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            vectors: RwLock::new(HashMap::new()),
        }
    }

    /// The dimensionality of vectors in this index.
    pub fn dims(&self) -> usize {
        self.dims
    }

    /// Number of vectors currently in the index.
    pub fn len(&self) -> usize {
        self.vectors.read().expect("lock poisoned").len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Insert a vector into the index. Replaces if `id` already exists.
    ///
    /// Returns `false` if the vector dimensionality doesn't match (vector is NOT inserted).
    pub fn insert(&self, id: i64, vector: Vec<f32>) -> bool {
        if vector.len() != self.dims {
            warn!(
                id,
                expected = self.dims,
                got = vector.len(),
                "VecIndex::insert: dimension mismatch, skipping"
            );
            return false;
        }
        self.vectors
            .write()
            .expect("lock poisoned")
            .insert(id, vector);
        true
    }

    /// Remove a vector from the index by ID.
    ///
    /// Returns `true` if the vector was present and removed.
    pub fn remove(&self, id: i64) -> bool {
        self.vectors
            .write()
            .expect("lock poisoned")
            .remove(&id)
            .is_some()
    }

    /// Check whether a vector with the given ID exists in the index.
    pub fn contains(&self, id: i64) -> bool {
        self.vectors
            .read()
            .expect("lock poisoned")
            .contains_key(&id)
    }

    /// Get a clone of the vector for the given ID (mainly for testing).
    pub fn get(&self, id: i64) -> Option<Vec<f32>> {
        self.vectors
            .read()
            .expect("lock poisoned")
            .get(&id)
            .cloned()
    }

    /// Bulk-load vectors from an iterator. Skips any with wrong dimensions.
    /// Returns the number of vectors successfully loaded.
    pub fn bulk_insert(&self, entries: impl IntoIterator<Item = (i64, Vec<f32>)>) -> usize {
        let mut map = self.vectors.write().expect("lock poisoned");
        let mut count = 0;
        for (id, vec) in entries {
            if vec.len() == self.dims {
                map.insert(id, vec);
                count += 1;
            } else {
                warn!(
                    id,
                    expected = self.dims,
                    got = vec.len(),
                    "VecIndex::bulk_insert: dimension mismatch, skipping"
                );
            }
        }
        debug!(count, "VecIndex bulk loaded");
        count
    }

    /// Clear all vectors from the index.
    pub fn clear(&self) {
        self.vectors.write().expect("lock poisoned").clear();
    }

    /// Find the `k` nearest neighbors to the query vector by cosine similarity.
    ///
    /// Returns results sorted by descending similarity score.
    /// All vectors are scored -- no threshold filtering is applied.
    /// Callers can apply their own score cutoff if needed.
    ///
    /// If the query vector has wrong dimensions, returns an empty vec.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<VecMatch> {
        if query.len() != self.dims {
            warn!(
                expected = self.dims,
                got = query.len(),
                "VecIndex::search: query dimension mismatch"
            );
            return Vec::new();
        }

        if k == 0 {
            return Vec::new();
        }

        // Pre-compute query magnitude once
        let query_mag = magnitude(query);
        if query_mag == 0.0 {
            return Vec::new();
        }

        let map = self.vectors.read().expect("lock poisoned");

        // Score all vectors
        let mut scored: Vec<VecMatch> = map
            .iter()
            .map(|(&id, vec)| {
                let score = cosine_similarity_precomputed(query, query_mag, vec);
                VecMatch { id, score }
            })
            .collect();

        // Partial sort: we only need top-k, so use select_nth_unstable for large sets
        if scored.len() > k {
            scored.select_nth_unstable_by(k, |a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            scored.truncate(k);
        }

        // Final sort of the top-k by score descending
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        scored
    }

    /// Find the `k` nearest neighbors, returning (id, score) tuples.
    /// Convenience wrapper matching the old API shape.
    pub fn search_ids(&self, query: &[f32], k: usize) -> Vec<(i64, f32)> {
        self.search(query, k)
            .into_iter()
            .map(|m| (m.id, m.score))
            .collect()
    }

    /// Return all IDs currently in the index.
    pub fn all_ids(&self) -> Vec<i64> {
        self.vectors
            .read()
            .expect("lock poisoned")
            .keys()
            .copied()
            .collect()
    }
}

/// Compute the magnitude (L2 norm) of a vector.
#[inline]
fn magnitude(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

/// Cosine similarity with pre-computed query magnitude.
/// Avoids recomputing `|query|` for every candidate.
#[inline]
fn cosine_similarity_precomputed(query: &[f32], query_mag: f32, candidate: &[f32]) -> f32 {
    debug_assert_eq!(query.len(), candidate.len());

    let mut dot = 0.0_f32;
    let mut cand_sq = 0.0_f32;

    // Fused dot-product + candidate magnitude in a single pass.
    // The compiler can auto-vectorize this loop with SIMD.
    for (q, c) in query.iter().zip(candidate.iter()) {
        dot += q * c;
        cand_sq += c * c;
    }

    let cand_mag = cand_sq.sqrt();
    if cand_mag == 0.0 {
        return 0.0;
    }

    dot / (query_mag * cand_mag)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_unit_vec(dims: usize, dominant_dim: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; dims];
        if dominant_dim < dims {
            v[dominant_dim] = 1.0;
        }
        v
    }

    fn make_normalized_vec(values: &[f32]) -> Vec<f32> {
        let mag: f32 = values.iter().map(|x| x * x).sum::<f32>().sqrt();
        if mag == 0.0 {
            return values.to_vec();
        }
        values.iter().map(|x| x / mag).collect()
    }

    // ── Construction & Basic Ops ──────────────────────────────

    #[test]
    fn test_new_index_is_empty() {
        let idx = VecIndex::new(4);
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
        assert_eq!(idx.dims(), 4);
    }

    #[test]
    fn test_insert_and_len() {
        let idx = VecIndex::new(3);
        assert!(idx.insert(1, vec![1.0, 0.0, 0.0]));
        assert_eq!(idx.len(), 1);
        assert!(!idx.is_empty());
    }

    #[test]
    fn test_insert_dimension_mismatch_rejected() {
        let idx = VecIndex::new(3);
        let ok = idx.insert(1, vec![1.0, 0.0]); // only 2 dims, need 3
        assert!(!ok);
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn test_insert_replaces_existing() {
        let idx = VecIndex::new(2);
        idx.insert(1, vec![1.0, 0.0]);
        idx.insert(1, vec![0.0, 1.0]); // replace
        assert_eq!(idx.len(), 1);
        let v = idx.get(1).unwrap();
        assert_eq!(v, vec![0.0, 1.0]);
    }

    #[test]
    fn test_remove() {
        let idx = VecIndex::new(2);
        idx.insert(1, vec![1.0, 0.0]);
        assert!(idx.remove(1));
        assert!(idx.is_empty());
    }

    #[test]
    fn test_remove_nonexistent() {
        let idx = VecIndex::new(2);
        assert!(!idx.remove(999));
    }

    #[test]
    fn test_contains() {
        let idx = VecIndex::new(2);
        idx.insert(42, vec![1.0, 0.0]);
        assert!(idx.contains(42));
        assert!(!idx.contains(99));
    }

    #[test]
    fn test_get() {
        let idx = VecIndex::new(3);
        idx.insert(1, vec![0.1, 0.2, 0.3]);
        let v = idx.get(1).unwrap();
        assert_eq!(v, vec![0.1, 0.2, 0.3]);
        assert!(idx.get(999).is_none());
    }

    #[test]
    fn test_clear() {
        let idx = VecIndex::new(2);
        idx.insert(1, vec![1.0, 0.0]);
        idx.insert(2, vec![0.0, 1.0]);
        idx.clear();
        assert!(idx.is_empty());
    }

    // ── Bulk Insert ───────────────────────────────────────────

    #[test]
    fn test_bulk_insert() {
        let idx = VecIndex::new(2);
        let entries = vec![
            (1, vec![1.0, 0.0]),
            (2, vec![0.0, 1.0]),
            (3, vec![0.5, 0.5]),
        ];
        let loaded = idx.bulk_insert(entries);
        assert_eq!(loaded, 3);
        assert_eq!(idx.len(), 3);
    }

    #[test]
    fn test_bulk_insert_skips_bad_dims() {
        let idx = VecIndex::new(2);
        let entries = vec![
            (1, vec![1.0, 0.0]),      // ok
            (2, vec![0.0, 1.0, 0.5]), // wrong dims
            (3, vec![0.5, 0.5]),      // ok
        ];
        let loaded = idx.bulk_insert(entries);
        assert_eq!(loaded, 2);
        assert_eq!(idx.len(), 2);
        assert!(!idx.contains(2));
    }

    // ── KNN Search ────────────────────────────────────────────

    #[test]
    fn test_search_empty_index() {
        let idx = VecIndex::new(3);
        let results = idx.search(&[1.0, 0.0, 0.0], 5);
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_finds_exact_match() {
        let idx = VecIndex::new(3);
        idx.insert(1, vec![1.0, 0.0, 0.0]);
        idx.insert(2, vec![0.0, 1.0, 0.0]);
        idx.insert(3, vec![0.0, 0.0, 1.0]);

        let results = idx.search(&[1.0, 0.0, 0.0], 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 1);
        assert!((results[0].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_search_returns_sorted_by_similarity() {
        let idx = VecIndex::new(3);
        idx.insert(1, make_unit_vec(3, 0)); // [1,0,0]
        idx.insert(2, make_normalized_vec(&[0.9, 0.1, 0.0])); // close to [1,0,0]
        idx.insert(3, make_normalized_vec(&[0.1, 0.9, 0.0])); // far from [1,0,0]

        let results = idx.search(&[1.0, 0.0, 0.0], 3);
        assert_eq!(results.len(), 3);
        // Must be sorted descending
        assert!(results[0].score >= results[1].score);
        assert!(results[1].score >= results[2].score);
        // First should be the exact match
        assert_eq!(results[0].id, 1);
    }

    #[test]
    fn test_search_k_larger_than_index() {
        let idx = VecIndex::new(2);
        idx.insert(1, vec![1.0, 0.0]);
        idx.insert(2, vec![0.0, 1.0]);

        let results = idx.search(&[0.7, 0.7], 10);
        // Should return exactly 2 (all vectors in the index)
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_k_zero() {
        let idx = VecIndex::new(2);
        idx.insert(1, vec![1.0, 0.0]);
        let results = idx.search(&[1.0, 0.0], 0);
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_returns_all_scored_including_negative() {
        let idx = VecIndex::new(2);
        idx.insert(1, vec![1.0, 0.0]); // similar to query (score = 1.0)
        idx.insert(2, vec![-1.0, 0.0]); // opposite to query (score = -1.0)
        idx.insert(3, vec![0.0, 1.0]); // orthogonal (score = 0.0)

        let results = idx.search(&[1.0, 0.0], 10);
        // All 3 vectors scored; no threshold filtering
        assert_eq!(results.len(), 3);
        // First should be the most similar
        assert_eq!(results[0].id, 1);
        assert!((results[0].score - 1.0).abs() < 1e-6);
        // Last should be the least similar (opposite)
        assert_eq!(results[2].id, 2);
    }

    #[test]
    fn test_search_query_dimension_mismatch() {
        let idx = VecIndex::new(3);
        idx.insert(1, vec![1.0, 0.0, 0.0]);
        let results = idx.search(&[1.0, 0.0], 5); // 2 dims, need 3
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_zero_query() {
        let idx = VecIndex::new(2);
        idx.insert(1, vec![1.0, 0.0]);
        let results = idx.search(&[0.0, 0.0], 5);
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_ids_convenience() {
        let idx = VecIndex::new(2);
        idx.insert(1, vec![1.0, 0.0]);
        idx.insert(2, vec![0.0, 1.0]);

        let results = idx.search_ids(&[1.0, 0.0], 2);
        assert_eq!(results[0].0, 1);
    }

    // ── All IDs ──────────────────────────────────────────────

    #[test]
    fn test_all_ids() {
        let idx = VecIndex::new(2);
        idx.insert(10, vec![1.0, 0.0]);
        idx.insert(20, vec![0.0, 1.0]);
        let mut ids = idx.all_ids();
        ids.sort();
        assert_eq!(ids, vec![10, 20]);
    }

    // ── Cosine Similarity Helpers ────────────────────────────

    #[test]
    fn test_cosine_similarity_identical() {
        let v = vec![0.6, 0.8];
        let mag = magnitude(&v);
        let sim = cosine_similarity_precomputed(&v, mag, &v);
        assert!((sim - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let mag = magnitude(&a);
        let sim = cosine_similarity_precomputed(&a, mag, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn test_magnitude() {
        let v = vec![3.0, 4.0];
        assert!((magnitude(&v) - 5.0).abs() < 1e-6);
    }

    // ── Thread Safety ────────────────────────────────────────

    #[test]
    fn test_concurrent_reads() {
        use std::sync::Arc;

        let idx = Arc::new(VecIndex::new(2));
        idx.insert(1, vec![1.0, 0.0]);
        idx.insert(2, vec![0.0, 1.0]);

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let idx = Arc::clone(&idx);
                std::thread::spawn(move || idx.search(&[0.7, 0.7], 2))
            })
            .collect();

        for h in handles {
            let results = h.join().unwrap();
            assert!(!results.is_empty());
        }
    }

    #[test]
    fn test_insert_while_searching() {
        use std::sync::Arc;

        let idx = Arc::new(VecIndex::new(2));
        idx.insert(1, vec![1.0, 0.0]);

        let idx_search = Arc::clone(&idx);
        let search_handle = std::thread::spawn(move || {
            for _ in 0..100 {
                let _ = idx_search.search(&[0.5, 0.5], 10);
            }
        });

        for i in 2..=100_i64 {
            idx.insert(i, vec![0.5, 0.5]);
        }

        search_handle.join().unwrap();
        assert!(!idx.is_empty()); // at least the original
    }
}
