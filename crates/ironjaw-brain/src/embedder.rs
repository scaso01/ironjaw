use moka::future::Cache;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Error type for embedding operations
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("embedding request failed: {0}")]
    Request(String),
    #[error("embedding response parse error: {0}")]
    Parse(String),
    #[error("fastembed error: {0}")]
    FastEmbed(String),
}

/// Embedding client — supports multiple backends for generating vector embeddings.
///
/// Tracks the expected embedding dimension after the first successful call.
/// Subsequent embeddings are validated for dimension consistency.
pub struct Embedder {
    kind: EmbedderKind,
    /// Expected embedding dimension (0 = not yet determined).
    expected_dims: AtomicUsize,
    /// LRU embedding cache: text hash -> embedding vector.
    /// Shared across all backends — avoids redundant compute for repeated texts.
    cache: Cache<u64, Vec<f32>>,
}

fn build_embed_cache() -> Cache<u64, Vec<f32>> {
    Cache::builder()
        .max_capacity(10_000)
        .time_to_live(Duration::from_secs(24 * 60 * 60))
        .build()
}

#[derive(Clone)]
enum EmbedderKind {
    /// Real HTTP client targeting llama-server
    Http {
        client: reqwest::Client,
        base_url: String,
        model: String,
    },
    /// Local fastembed-rs backend (ONNX runtime, no server needed).
    ///
    /// Uses `std::sync::Mutex` rather than `tokio::sync::Mutex` because the lock
    /// is held only during CPU-bound ONNX inference inside `spawn_blocking` — no
    /// `.await` occurs inside the critical section. `std::sync::Mutex` has lower
    /// overhead in this scenario. If the critical section ever needs to await
    /// (e.g., async model loading), switch to `tokio::sync::Mutex`.
    FastEmbed {
        model: Arc<std::sync::Mutex<fastembed::TextEmbedding>>,
    },
    /// Deterministic mock for testing (hash-based, consistent for same input)
    Mock { dims: usize },
}

impl Clone for Embedder {
    fn clone(&self) -> Self {
        Self {
            kind: self.kind.clone(),
            expected_dims: AtomicUsize::new(self.expected_dims.load(Ordering::Relaxed)),
            cache: self.cache.clone(),
        }
    }
}

#[derive(Serialize)]
struct EmbedRequest {
    model: String,
    input: String,
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

#[derive(Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
}

impl Embedder {
    /// Create a real embedder targeting llama-server's `/v1/embeddings`.
    pub fn new(base_url: &str, model: &str) -> Self {
        Self {
            kind: EmbedderKind::Http {
                client: reqwest::Client::new(),
                base_url: base_url.trim_end_matches('/').to_string(),
                model: model.to_string(),
            },
            expected_dims: AtomicUsize::new(0),
            cache: build_embed_cache(),
        }
    }

    /// Create a local fastembed-rs embedder using the given model.
    ///
    /// Initializes the ONNX runtime and downloads the model on first use.
    /// If initialization fails, logs a warning and falls back to the HTTP backend.
    pub fn new_fastembed(http_base_url: &str, http_model: &str) -> Self {
        use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

        let options =
            InitOptions::new(EmbeddingModel::NomicEmbedTextV15).with_show_download_progress(true);

        match TextEmbedding::try_new(options) {
            Ok(model) => {
                info!("FastEmbed initialized with nomic-embed-text-v1.5");
                Self {
                    kind: EmbedderKind::FastEmbed {
                        model: Arc::new(std::sync::Mutex::new(model)),
                    },
                    expected_dims: AtomicUsize::new(0),
                    cache: build_embed_cache(),
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "FastEmbed init failed, falling back to HTTP embedder"
                );
                Self::new(http_base_url, http_model)
            }
        }
    }

    /// Create a mock embedder that returns deterministic hash-based embeddings.
    pub fn mock(dims: usize) -> Self {
        Self {
            kind: EmbedderKind::Mock { dims },
            expected_dims: AtomicUsize::new(dims),
            cache: build_embed_cache(),
        }
    }

    /// Embed a text string, returning a float vector.
    ///
    /// On the first successful call, records the embedding dimension.
    /// Subsequent calls validate that dimensions are consistent.
    /// Returns an error if a dimension mismatch is detected (would corrupt the index).
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        // Shared cache lookup (all backends)
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let text_hash = hasher.finish();

        if let Some(cached) = self.cache.get(&text_hash).await {
            let prev = self.expected_dims.load(Ordering::Relaxed);
            if prev == 0 || cached.len() == prev {
                debug!(hash = text_hash, dims = cached.len(), "Embedding cache hit");
                return Ok(cached);
            }
            warn!(
                expected = prev,
                cached = cached.len(),
                "Embedding cache dim mismatch, evicting"
            );
            self.cache.invalidate(&text_hash).await;
        }

        let embedding = match &self.kind {
            EmbedderKind::Http {
                client,
                base_url,
                model,
            } => {
                let url = format!("{base_url}/v1/embeddings");
                let body = EmbedRequest {
                    model: model.clone(),
                    input: text.to_string(),
                };

                let resp = client
                    .post(&url)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| EmbedError::Request(e.to_string()))?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    return Err(EmbedError::Request(format!("HTTP {status} — {body}")));
                }

                let parsed: EmbedResponse = resp
                    .json()
                    .await
                    .map_err(|e| EmbedError::Parse(e.to_string()))?;

                parsed
                    .data
                    .into_iter()
                    .next()
                    .map(|d| d.embedding)
                    .ok_or_else(|| EmbedError::Parse("empty data array".to_string()))?
            }
            EmbedderKind::FastEmbed { model } => {
                let text_owned = text.to_string();
                let model = Arc::clone(model);
                // Run fastembed in a blocking task since ONNX inference is CPU-bound
                tokio::task::spawn_blocking(move || {
                    let mut model = model
                        .lock()
                        .map_err(|e| EmbedError::FastEmbed(format!("lock poisoned: {e}")))?;
                    let mut results = model
                        .embed(vec![text_owned], None)
                        .map_err(|e| EmbedError::FastEmbed(e.to_string()))?;
                    results
                        .pop()
                        .ok_or_else(|| EmbedError::FastEmbed("empty result".to_string()))
                })
                .await
                .map_err(|e| EmbedError::FastEmbed(format!("spawn_blocking failed: {e}")))??
            }
            EmbedderKind::Mock { dims } => mock_embedding(text, *dims),
        };

        // Dimension validation
        let dims = embedding.len();
        let prev = self.expected_dims.load(Ordering::Relaxed);
        if prev == 0 {
            // First successful embed — record the dimension
            self.expected_dims.store(dims, Ordering::Relaxed);
            if dims != 768 {
                error!(
                    actual = dims,
                    expected = 768,
                    "Embedding dimension does not match expected 768"
                );
            }
            info!(dims, "Embedding dimension recorded");
        } else if dims != prev {
            warn!(
                expected = prev,
                actual = dims,
                "Embedding dimension mismatch — not storing (would corrupt index)"
            );
            return Err(EmbedError::Parse(format!(
                "dimension mismatch: expected {prev}, got {dims}"
            )));
        }

        // Store in shared cache after successful dimension validation
        self.cache.insert(text_hash, embedding.clone()).await;
        debug!(hash = text_hash, dims, "Embedding cached");

        Ok(embedding)
    }

    /// Returns the recorded embedding dimension, or None if no embedding has been generated yet.
    pub fn dims(&self) -> Option<usize> {
        let d = self.expected_dims.load(Ordering::Relaxed);
        if d == 0 {
            None
        } else {
            Some(d)
        }
    }

    /// Returns true if this is a mock embedder.
    pub fn is_mock(&self) -> bool {
        matches!(self.kind, EmbedderKind::Mock { .. })
    }

    /// Returns true if this is a fastembed embedder.
    pub fn is_fastembed(&self) -> bool {
        matches!(self.kind, EmbedderKind::FastEmbed { .. })
    }
}

/// Generate a deterministic mock embedding from a text hash.
/// Same input always produces the same output.
fn mock_embedding(text: &str, dims: usize) -> Vec<f32> {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let seed = hasher.finish();

    let mut vec = Vec::with_capacity(dims);
    let mut state = seed;
    for _ in 0..dims {
        // Simple xorshift-style PRNG for deterministic values
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        // Map to [-1, 1] range
        let val = ((state as f64) / (u64::MAX as f64)) * 2.0 - 1.0;
        vec.push(val as f32);
    }

    // Normalize to unit vector
    let mag = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag > 0.0 {
        for x in &mut vec {
            *x /= mag;
        }
    }

    vec
}

/// Cosine similarity between two vectors: dot(a, b) / (|a| * |b|)
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        warn!(
            a_len = a.len(),
            b_len = b.len(),
            "cosine_similarity: mismatched or empty vectors"
        );
        return 0.0;
    }

    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let mag_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if mag_a == 0.0 || mag_b == 0.0 {
        return 0.0;
    }

    dot / (mag_a * mag_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn test_cosine_similarity_mismatched_length() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn test_mock_deterministic() {
        let emb1 = mock_embedding("hello world", 128);
        let emb2 = mock_embedding("hello world", 128);
        assert_eq!(emb1, emb2);
        assert_eq!(emb1.len(), 128);
    }

    #[test]
    fn test_mock_different_inputs() {
        let emb1 = mock_embedding("hello", 64);
        let emb2 = mock_embedding("world", 64);
        assert_ne!(emb1, emb2);
    }

    #[test]
    fn test_mock_unit_vector() {
        let emb = mock_embedding("test", 128);
        let mag: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (mag - 1.0).abs() < 1e-5,
            "mock embedding should be normalized, got mag={mag}"
        );
    }

    #[tokio::test]
    async fn test_mock_embedder() {
        let embedder = Embedder::mock(64);
        assert!(embedder.is_mock());

        let emb = embedder.embed("test text").await.unwrap();
        assert_eq!(emb.len(), 64);

        // Same input → same output
        let emb2 = embedder.embed("test text").await.unwrap();
        assert_eq!(emb, emb2);
    }

    #[tokio::test]
    async fn test_mock_similar_texts_different() {
        let embedder = Embedder::mock(128);
        let emb_a = embedder.embed("Rust programming").await.unwrap();
        let emb_b = embedder.embed("Python programming").await.unwrap();

        // Different texts produce different embeddings
        let sim = cosine_similarity(&emb_a, &emb_b);
        assert!(
            sim < 0.99,
            "different texts should have <1.0 similarity, got {sim}"
        );
    }
}
