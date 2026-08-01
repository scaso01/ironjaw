//! Meta-Configuration Optimizer (I22)
//!
//! A/B testing framework for gateway configuration. Define experiment
//! variants with key-value diffs, traffic split percentages, and a target
//! metric. The gateway routes requests transparently to variants and logs
//! results for analysis.
//!
//! RPCs: `config.experiment`, `config.experiment.status`,
//!       `config.experiment.promote`

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single configuration variant in an experiment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variant {
    /// Variant name (e.g. "control", "treatment_a").
    pub name: String,
    /// Config key-value overrides for this variant.
    pub overrides: HashMap<String, serde_json::Value>,
    /// Percentage of traffic routed to this variant (0-100).
    pub traffic_pct: u8,
}

/// State of an experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentState {
    Running,
    Completed,
    Promoted,
    Cancelled,
}

/// Metric observations for a variant during an experiment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantMetrics {
    /// Variant name.
    pub variant: String,
    /// Number of requests served by this variant.
    pub request_count: u64,
    /// Sum of the target metric observations.
    pub metric_sum: f64,
    /// Minimum observed value.
    pub metric_min: f64,
    /// Maximum observed value.
    pub metric_max: f64,
}

impl VariantMetrics {
    fn new(variant: &str) -> Self {
        Self {
            variant: variant.to_string(),
            request_count: 0,
            metric_sum: 0.0,
            metric_min: f64::MAX,
            metric_max: f64::MIN,
        }
    }

    fn record(&mut self, value: f64) {
        self.request_count += 1;
        self.metric_sum += value;
        if value < self.metric_min {
            self.metric_min = value;
        }
        if value > self.metric_max {
            self.metric_max = value;
        }
    }

    /// Mean of observations.
    pub fn mean(&self) -> f64 {
        if self.request_count > 0 {
            self.metric_sum / self.request_count as f64
        } else {
            0.0
        }
    }
}

/// A full experiment definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Experiment {
    /// Unique experiment ID.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Description of what is being tested.
    pub description: String,
    /// The target metric being optimized.
    pub metric: String,
    /// The variants being compared.
    pub variants: Vec<Variant>,
    /// Current experiment state.
    pub state: ExperimentState,
    /// When the experiment was created.
    pub created_at: DateTime<Utc>,
    /// When the experiment should end.
    pub ends_at: DateTime<Utc>,
    /// Per-variant metric observations.
    pub results: HashMap<String, VariantMetrics>,
    /// Which variant was promoted (if any).
    pub promoted_variant: Option<String>,
}

/// Request to create a new experiment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateExperimentRequest {
    /// Experiment name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Target metric (e.g., "latency_ms", "success_rate").
    pub metric: String,
    /// Variants to test.
    pub variants: Vec<Variant>,
    /// Duration in seconds.
    pub duration_secs: u64,
}

/// Summary returned by `config.experiment.status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentStatus {
    pub experiment: Experiment,
    /// Per-variant mean metric values.
    pub variant_means: HashMap<String, f64>,
    /// Remaining time in seconds (0 if completed).
    pub remaining_secs: i64,
}

/// Errors from the meta-config system.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MetaConfigError {
    #[error("experiment not found: {0}")]
    NotFound(String),
    #[error("invalid experiment: {0}")]
    Invalid(String),
    #[error("experiment already completed: {0}")]
    AlreadyCompleted(String),
    #[error("variant not found: {0}")]
    VariantNotFound(String),
    #[error("traffic split must sum to 100, got {0}")]
    InvalidTrafficSplit(u16),
}

// ---------------------------------------------------------------------------
// MetaConfigOptimizer
// ---------------------------------------------------------------------------

/// Manages A/B experiments on gateway configuration.
pub struct MetaConfigOptimizer {
    experiments: RwLock<HashMap<String, Experiment>>,
    /// Simple RNG counter for traffic routing.
    route_counter: RwLock<u64>,
}

impl MetaConfigOptimizer {
    /// Create a new optimizer.
    pub fn new() -> Self {
        Self {
            experiments: RwLock::new(HashMap::new()),
            route_counter: RwLock::new(0),
        }
    }

    /// Create a new experiment.
    pub async fn create(
        &self,
        request: CreateExperimentRequest,
    ) -> Result<Experiment, MetaConfigError> {
        // Validate
        if request.name.trim().is_empty() {
            return Err(MetaConfigError::Invalid(
                "experiment name cannot be empty".to_string(),
            ));
        }
        if request.variants.is_empty() {
            return Err(MetaConfigError::Invalid(
                "must have at least one variant".to_string(),
            ));
        }
        if request.metric.trim().is_empty() {
            return Err(MetaConfigError::Invalid(
                "metric cannot be empty".to_string(),
            ));
        }

        // Validate traffic split sums to 100
        let total_pct: u16 = request.variants.iter().map(|v| v.traffic_pct as u16).sum();
        if total_pct != 100 {
            return Err(MetaConfigError::InvalidTrafficSplit(total_pct));
        }

        // Validate unique variant names
        let mut names = std::collections::HashSet::new();
        for v in &request.variants {
            if v.name.trim().is_empty() {
                return Err(MetaConfigError::Invalid(
                    "variant name cannot be empty".to_string(),
                ));
            }
            if !names.insert(&v.name) {
                return Err(MetaConfigError::Invalid(format!(
                    "duplicate variant name: {}",
                    v.name
                )));
            }
        }

        let now = Utc::now();
        let mut results = HashMap::new();
        for v in &request.variants {
            results.insert(v.name.clone(), VariantMetrics::new(&v.name));
        }

        let experiment = Experiment {
            id: Uuid::new_v4().to_string(),
            name: request.name,
            description: request.description,
            metric: request.metric,
            variants: request.variants,
            state: ExperimentState::Running,
            created_at: now,
            ends_at: now + Duration::seconds(request.duration_secs as i64),
            results,
            promoted_variant: None,
        };

        let result = experiment.clone();
        self.experiments
            .write()
            .await
            .insert(experiment.id.clone(), experiment);
        Ok(result)
    }

    /// Route a request to a variant based on traffic split.
    /// Returns the variant name and its config overrides.
    pub async fn route(
        &self,
        experiment_id: &str,
    ) -> Result<(String, HashMap<String, serde_json::Value>), MetaConfigError> {
        let experiments = self.experiments.read().await;
        let exp = experiments
            .get(experiment_id)
            .ok_or_else(|| MetaConfigError::NotFound(experiment_id.to_string()))?;

        if exp.state != ExperimentState::Running {
            return Err(MetaConfigError::AlreadyCompleted(experiment_id.to_string()));
        }

        // Simple deterministic routing based on counter
        let mut counter = self.route_counter.write().await;
        let bucket = (*counter % 100) as u8;
        *counter = counter.wrapping_add(1);

        let mut cumulative = 0u8;
        for v in &exp.variants {
            cumulative += v.traffic_pct;
            if bucket < cumulative {
                return Ok((v.name.clone(), v.overrides.clone()));
            }
        }

        // Fallback to last variant
        let last = exp.variants.last().unwrap();
        Ok((last.name.clone(), last.overrides.clone()))
    }

    /// Record a metric observation for a variant.
    pub async fn record_metric(
        &self,
        experiment_id: &str,
        variant: &str,
        value: f64,
    ) -> Result<(), MetaConfigError> {
        let mut experiments = self.experiments.write().await;
        let exp = experiments
            .get_mut(experiment_id)
            .ok_or_else(|| MetaConfigError::NotFound(experiment_id.to_string()))?;

        // Auto-complete if past end time
        if exp.state == ExperimentState::Running && Utc::now() > exp.ends_at {
            exp.state = ExperimentState::Completed;
        }

        let metrics = exp
            .results
            .get_mut(variant)
            .ok_or_else(|| MetaConfigError::VariantNotFound(variant.to_string()))?;

        metrics.record(value);
        Ok(())
    }

    /// Get the status of an experiment.
    pub async fn status(&self, experiment_id: &str) -> Result<ExperimentStatus, MetaConfigError> {
        let mut experiments = self.experiments.write().await;
        let exp = experiments
            .get_mut(experiment_id)
            .ok_or_else(|| MetaConfigError::NotFound(experiment_id.to_string()))?;

        // Auto-complete if past end time
        if exp.state == ExperimentState::Running && Utc::now() > exp.ends_at {
            exp.state = ExperimentState::Completed;
        }

        let variant_means: HashMap<String, f64> = exp
            .results
            .iter()
            .map(|(name, m)| (name.clone(), m.mean()))
            .collect();

        let remaining = if exp.state == ExperimentState::Running {
            (exp.ends_at - Utc::now()).num_seconds().max(0)
        } else {
            0
        };

        Ok(ExperimentStatus {
            experiment: exp.clone(),
            variant_means,
            remaining_secs: remaining,
        })
    }

    /// Promote a variant's configuration (marks experiment as promoted).
    pub async fn promote(
        &self,
        experiment_id: &str,
        variant_name: &str,
    ) -> Result<Experiment, MetaConfigError> {
        let mut experiments = self.experiments.write().await;
        let exp = experiments
            .get_mut(experiment_id)
            .ok_or_else(|| MetaConfigError::NotFound(experiment_id.to_string()))?;

        if exp.state == ExperimentState::Promoted {
            return Err(MetaConfigError::AlreadyCompleted(
                "already promoted".to_string(),
            ));
        }

        // Verify variant exists
        if !exp.variants.iter().any(|v| v.name == variant_name) {
            return Err(MetaConfigError::VariantNotFound(variant_name.to_string()));
        }

        exp.state = ExperimentState::Promoted;
        exp.promoted_variant = Some(variant_name.to_string());
        Ok(exp.clone())
    }

    /// Cancel a running experiment.
    pub async fn cancel(&self, experiment_id: &str) -> Result<(), MetaConfigError> {
        let mut experiments = self.experiments.write().await;
        let exp = experiments
            .get_mut(experiment_id)
            .ok_or_else(|| MetaConfigError::NotFound(experiment_id.to_string()))?;

        if exp.state != ExperimentState::Running {
            return Err(MetaConfigError::AlreadyCompleted(experiment_id.to_string()));
        }

        exp.state = ExperimentState::Cancelled;
        Ok(())
    }

    /// List all experiments, sorted by creation date (newest first).
    pub async fn list(&self) -> Vec<Experiment> {
        let experiments = self.experiments.read().await;
        let mut list: Vec<Experiment> = experiments.values().cloned().collect();
        list.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        list
    }

    /// Get an experiment by ID.
    pub async fn get(&self, id: &str) -> Option<Experiment> {
        self.experiments.read().await.get(id).cloned()
    }

    /// Count of experiments.
    pub async fn count(&self) -> usize {
        self.experiments.read().await.len()
    }
}

impl Default for MetaConfigOptimizer {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrap in Arc for shared access.
pub type SharedMetaConfig = Arc<MetaConfigOptimizer>;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_variants() -> Vec<Variant> {
        vec![
            Variant {
                name: "control".to_string(),
                overrides: HashMap::new(),
                traffic_pct: 50,
            },
            Variant {
                name: "treatment".to_string(),
                overrides: HashMap::from([("temperature".to_string(), serde_json::json!(0.7))]),
                traffic_pct: 50,
            },
        ]
    }

    fn sample_request() -> CreateExperimentRequest {
        CreateExperimentRequest {
            name: "temp-experiment".to_string(),
            description: "Test temperature effect".to_string(),
            metric: "latency_ms".to_string(),
            variants: sample_variants(),
            duration_secs: 3600,
        }
    }

    #[tokio::test]
    async fn create_experiment() {
        let opt = MetaConfigOptimizer::new();
        let exp = opt.create(sample_request()).await.unwrap();
        assert_eq!(exp.name, "temp-experiment");
        assert_eq!(exp.state, ExperimentState::Running);
        assert_eq!(exp.variants.len(), 2);
        assert!(exp.promoted_variant.is_none());
    }

    #[tokio::test]
    async fn create_empty_name_fails() {
        let opt = MetaConfigOptimizer::new();
        let mut req = sample_request();
        req.name = String::new();
        assert!(matches!(
            opt.create(req).await.unwrap_err(),
            MetaConfigError::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn create_no_variants_fails() {
        let opt = MetaConfigOptimizer::new();
        let mut req = sample_request();
        req.variants = vec![];
        assert!(matches!(
            opt.create(req).await.unwrap_err(),
            MetaConfigError::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn create_empty_metric_fails() {
        let opt = MetaConfigOptimizer::new();
        let mut req = sample_request();
        req.metric = String::new();
        assert!(matches!(
            opt.create(req).await.unwrap_err(),
            MetaConfigError::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn create_bad_traffic_split_fails() {
        let opt = MetaConfigOptimizer::new();
        let mut req = sample_request();
        req.variants[0].traffic_pct = 60; // 60 + 50 = 110
        let err = opt.create(req).await.unwrap_err();
        assert!(matches!(err, MetaConfigError::InvalidTrafficSplit(110)));
    }

    #[tokio::test]
    async fn create_duplicate_variant_names_fails() {
        let opt = MetaConfigOptimizer::new();
        let mut req = sample_request();
        req.variants[1].name = "control".to_string(); // same as first
        assert!(matches!(
            opt.create(req).await.unwrap_err(),
            MetaConfigError::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn route_distributes_traffic() {
        let opt = MetaConfigOptimizer::new();
        let exp = opt.create(sample_request()).await.unwrap();

        let mut control_count = 0;
        let mut treatment_count = 0;

        for _ in 0..100 {
            let (variant, _) = opt.route(&exp.id).await.unwrap();
            match variant.as_str() {
                "control" => control_count += 1,
                "treatment" => treatment_count += 1,
                _ => panic!("unexpected variant"),
            }
        }

        // 50/50 split should give exactly 50 each with deterministic counter
        assert_eq!(control_count, 50);
        assert_eq!(treatment_count, 50);
    }

    #[tokio::test]
    async fn route_nonexistent_fails() {
        let opt = MetaConfigOptimizer::new();
        let err = opt.route("ghost").await.unwrap_err();
        assert!(matches!(err, MetaConfigError::NotFound(_)));
    }

    #[tokio::test]
    async fn record_and_status() {
        let opt = MetaConfigOptimizer::new();
        let exp = opt.create(sample_request()).await.unwrap();

        opt.record_metric(&exp.id, "control", 100.0).await.unwrap();
        opt.record_metric(&exp.id, "control", 200.0).await.unwrap();
        opt.record_metric(&exp.id, "treatment", 50.0).await.unwrap();

        let status = opt.status(&exp.id).await.unwrap();
        assert!((status.variant_means["control"] - 150.0).abs() < f64::EPSILON);
        assert!((status.variant_means["treatment"] - 50.0).abs() < f64::EPSILON);
        assert!(status.remaining_secs > 0);
    }

    #[tokio::test]
    async fn record_bad_variant_fails() {
        let opt = MetaConfigOptimizer::new();
        let exp = opt.create(sample_request()).await.unwrap();

        let err = opt
            .record_metric(&exp.id, "nonexistent", 1.0)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaConfigError::VariantNotFound(_)));
    }

    #[tokio::test]
    async fn promote_experiment() {
        let opt = MetaConfigOptimizer::new();
        let exp = opt.create(sample_request()).await.unwrap();

        let promoted = opt.promote(&exp.id, "treatment").await.unwrap();
        assert_eq!(promoted.state, ExperimentState::Promoted);
        assert_eq!(promoted.promoted_variant, Some("treatment".to_string()));
    }

    #[tokio::test]
    async fn promote_bad_variant_fails() {
        let opt = MetaConfigOptimizer::new();
        let exp = opt.create(sample_request()).await.unwrap();

        let err = opt.promote(&exp.id, "ghost").await.unwrap_err();
        assert!(matches!(err, MetaConfigError::VariantNotFound(_)));
    }

    #[tokio::test]
    async fn promote_twice_fails() {
        let opt = MetaConfigOptimizer::new();
        let exp = opt.create(sample_request()).await.unwrap();

        opt.promote(&exp.id, "control").await.unwrap();
        let err = opt.promote(&exp.id, "treatment").await.unwrap_err();
        assert!(matches!(err, MetaConfigError::AlreadyCompleted(_)));
    }

    #[tokio::test]
    async fn cancel_experiment() {
        let opt = MetaConfigOptimizer::new();
        let exp = opt.create(sample_request()).await.unwrap();

        opt.cancel(&exp.id).await.unwrap();
        let found = opt.get(&exp.id).await.unwrap();
        assert_eq!(found.state, ExperimentState::Cancelled);
    }

    #[tokio::test]
    async fn cancel_nonexistent_fails() {
        let opt = MetaConfigOptimizer::new();
        let err = opt.cancel("ghost").await.unwrap_err();
        assert!(matches!(err, MetaConfigError::NotFound(_)));
    }

    #[tokio::test]
    async fn cancel_completed_fails() {
        let opt = MetaConfigOptimizer::new();
        let exp = opt.create(sample_request()).await.unwrap();
        opt.cancel(&exp.id).await.unwrap();

        let err = opt.cancel(&exp.id).await.unwrap_err();
        assert!(matches!(err, MetaConfigError::AlreadyCompleted(_)));
    }

    #[tokio::test]
    async fn list_sorted_newest_first() {
        let opt = MetaConfigOptimizer::new();

        let mut req1 = sample_request();
        req1.name = "first".to_string();
        opt.create(req1).await.unwrap();

        let mut req2 = sample_request();
        req2.name = "second".to_string();
        opt.create(req2).await.unwrap();

        let list = opt.list().await;
        assert_eq!(list.len(), 2);
        // second created later, should be first
        assert_eq!(list[0].name, "second");
    }

    #[tokio::test]
    async fn count_tracks() {
        let opt = MetaConfigOptimizer::new();
        assert_eq!(opt.count().await, 0);

        opt.create(sample_request()).await.unwrap();
        assert_eq!(opt.count().await, 1);
    }

    #[tokio::test]
    async fn variant_metrics_min_max() {
        let opt = MetaConfigOptimizer::new();
        let exp = opt.create(sample_request()).await.unwrap();

        opt.record_metric(&exp.id, "control", 10.0).await.unwrap();
        opt.record_metric(&exp.id, "control", 50.0).await.unwrap();
        opt.record_metric(&exp.id, "control", 30.0).await.unwrap();

        let found = opt.get(&exp.id).await.unwrap();
        let control = &found.results["control"];
        assert!((control.metric_min - 10.0).abs() < f64::EPSILON);
        assert!((control.metric_max - 50.0).abs() < f64::EPSILON);
        assert_eq!(control.request_count, 3);
    }

    #[tokio::test]
    async fn status_nonexistent_fails() {
        let opt = MetaConfigOptimizer::new();
        let err = opt.status("ghost").await.unwrap_err();
        assert!(matches!(err, MetaConfigError::NotFound(_)));
    }

    #[test]
    fn variant_metrics_mean_zero_requests() {
        let m = VariantMetrics::new("test");
        assert!((m.mean() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn error_display() {
        assert_eq!(
            MetaConfigError::NotFound("x".into()).to_string(),
            "experiment not found: x"
        );
        assert_eq!(
            MetaConfigError::InvalidTrafficSplit(110).to_string(),
            "traffic split must sum to 100, got 110"
        );
    }

    #[tokio::test]
    async fn default_impl() {
        let opt = MetaConfigOptimizer::default();
        assert_eq!(opt.count().await, 0);
    }

    #[tokio::test]
    async fn auto_complete_on_expired() {
        let opt = MetaConfigOptimizer::new();
        let mut req = sample_request();
        req.duration_secs = 0; // expires immediately
        let exp = opt.create(req).await.unwrap();

        // Small sleep to ensure time passes
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let status = opt.status(&exp.id).await.unwrap();
        assert_eq!(status.experiment.state, ExperimentState::Completed);
        assert_eq!(status.remaining_secs, 0);
    }

    #[tokio::test]
    async fn route_returns_overrides() {
        let opt = MetaConfigOptimizer::new();
        let mut req = sample_request();
        // 100% to treatment to guarantee we get its overrides
        req.variants = vec![Variant {
            name: "only".to_string(),
            overrides: HashMap::from([("key".to_string(), serde_json::json!("value"))]),
            traffic_pct: 100,
        }];
        let exp = opt.create(req).await.unwrap();

        let (name, overrides) = opt.route(&exp.id).await.unwrap();
        assert_eq!(name, "only");
        assert_eq!(overrides["key"], serde_json::json!("value"));
    }

    #[tokio::test]
    async fn empty_variant_name_fails() {
        let opt = MetaConfigOptimizer::new();
        let mut req = sample_request();
        req.variants[0].name = String::new();
        assert!(matches!(
            opt.create(req).await.unwrap_err(),
            MetaConfigError::Invalid(_)
        ));
    }
}
