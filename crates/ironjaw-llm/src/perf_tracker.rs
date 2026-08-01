use serde::Serialize;
use std::collections::HashMap;
use std::sync::RwLock;

/// Per-model inference performance statistics.
#[derive(Debug, Clone, Serialize)]
pub struct ModelPerf {
    pub model: String,
    pub avg_tokens_per_sec: f64,
    pub total_requests: u64,
    pub total_tokens: u64,
    pub total_duration_ms: u64,
    pub last_tokens_per_sec: f64,
}

/// Tracks inference speed (tokens/sec) per model.
///
/// Thread-safe via interior `RwLock`. Designed to be held in `AppState`
/// behind an `Arc` and called from the streaming completion path.
pub struct PerfTracker {
    data: RwLock<HashMap<String, PerfData>>,
}

struct PerfData {
    total_tokens: u64,
    total_duration_ms: u64,
    request_count: u64,
    last_tps: f64,
}

impl PerfTracker {
    pub fn new() -> Self {
        Self {
            data: RwLock::new(HashMap::new()),
        }
    }

    /// Record an inference completion for a model.
    pub fn record(&self, model: &str, tokens: u64, duration_ms: u64) {
        if duration_ms == 0 {
            return;
        }
        let tps = (tokens as f64 / duration_ms as f64) * 1000.0;
        let mut data = self.data.write().unwrap();
        let entry = data.entry(model.to_string()).or_insert(PerfData {
            total_tokens: 0,
            total_duration_ms: 0,
            request_count: 0,
            last_tps: 0.0,
        });
        entry.total_tokens += tokens;
        entry.total_duration_ms += duration_ms;
        entry.request_count += 1;
        entry.last_tps = tps;
    }

    /// Get performance stats for all tracked models.
    pub fn summary(&self) -> Vec<ModelPerf> {
        let data = self.data.read().unwrap();
        data.iter()
            .map(|(model, d)| {
                let avg_tps = if d.total_duration_ms > 0 {
                    (d.total_tokens as f64 / d.total_duration_ms as f64) * 1000.0
                } else {
                    0.0
                };
                ModelPerf {
                    model: model.clone(),
                    avg_tokens_per_sec: avg_tps,
                    total_requests: d.request_count,
                    total_tokens: d.total_tokens,
                    total_duration_ms: d.total_duration_ms,
                    last_tokens_per_sec: d.last_tps,
                }
            })
            .collect()
    }

    /// Get performance for a specific model.
    pub fn get(&self, model: &str) -> Option<ModelPerf> {
        let data = self.data.read().unwrap();
        data.get(model).map(|d| {
            let avg_tps = if d.total_duration_ms > 0 {
                (d.total_tokens as f64 / d.total_duration_ms as f64) * 1000.0
            } else {
                0.0
            };
            ModelPerf {
                model: model.to_string(),
                avg_tokens_per_sec: avg_tps,
                total_requests: d.request_count,
                total_tokens: d.total_tokens,
                total_duration_ms: d.total_duration_ms,
                last_tokens_per_sec: d.last_tps,
            }
        })
    }
}

impl Default for PerfTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_tracker() {
        let tracker = PerfTracker::new();
        let summary = tracker.summary();
        assert!(summary.is_empty());
        assert!(tracker.get("nonexistent").is_none());
    }

    #[test]
    fn test_record_and_summary() {
        let tracker = PerfTracker::new();
        // 100 tokens in 2000ms = 50 tok/s
        tracker.record("model-a", 100, 2000);

        let summary = tracker.summary();
        assert_eq!(summary.len(), 1);
        let perf = &summary[0];
        assert_eq!(perf.model, "model-a");
        assert_eq!(perf.total_requests, 1);
        assert_eq!(perf.total_tokens, 100);
        assert_eq!(perf.total_duration_ms, 2000);
        assert!((perf.avg_tokens_per_sec - 50.0).abs() < 0.01);
        assert!((perf.last_tokens_per_sec - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_multiple_records_same_model() {
        let tracker = PerfTracker::new();
        // First: 100 tokens in 2000ms = 50 tok/s
        tracker.record("model-a", 100, 2000);
        // Second: 200 tokens in 1000ms = 200 tok/s
        tracker.record("model-a", 200, 1000);

        let perf = tracker.get("model-a").unwrap();
        assert_eq!(perf.total_requests, 2);
        assert_eq!(perf.total_tokens, 300);
        assert_eq!(perf.total_duration_ms, 3000);
        // avg = 300/3000 * 1000 = 100 tok/s
        assert!((perf.avg_tokens_per_sec - 100.0).abs() < 0.01);
        // last = 200/1000 * 1000 = 200 tok/s
        assert!((perf.last_tokens_per_sec - 200.0).abs() < 0.01);
    }

    #[test]
    fn test_multiple_models() {
        let tracker = PerfTracker::new();
        tracker.record("model-a", 100, 2000);
        tracker.record("model-b", 50, 500);

        let summary = tracker.summary();
        assert_eq!(summary.len(), 2);

        let a = tracker.get("model-a").unwrap();
        let b = tracker.get("model-b").unwrap();
        assert_eq!(a.total_tokens, 100);
        assert_eq!(b.total_tokens, 50);
        // b: 50/500*1000 = 100 tok/s
        assert!((b.avg_tokens_per_sec - 100.0).abs() < 0.01);
    }

    #[test]
    fn test_zero_duration_ignored() {
        let tracker = PerfTracker::new();
        tracker.record("model-a", 100, 0);
        assert!(tracker.summary().is_empty());
        assert!(tracker.get("model-a").is_none());
    }

    #[test]
    fn test_get_nonexistent_model() {
        let tracker = PerfTracker::new();
        tracker.record("model-a", 100, 1000);
        assert!(tracker.get("model-b").is_none());
    }

    #[test]
    fn test_default_trait() {
        let tracker = PerfTracker::default();
        assert!(tracker.summary().is_empty());
    }

    #[test]
    fn test_model_perf_serializes() {
        let tracker = PerfTracker::new();
        tracker.record("test-model", 42, 100);
        let summary = tracker.summary();
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("test-model"));
        assert!(json.contains("avg_tokens_per_sec"));
    }
}
