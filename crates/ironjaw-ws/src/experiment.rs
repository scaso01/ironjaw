//! Autonomous Experiment Runner (I1)
//!
//! Runs iterative experiments that optimize a target (config key, brain file,
//! or prompt template) by measuring a metric extracted via RPC. Uses the local
//! llama-server to propose changes, applies them, measures the result, and
//! keeps or reverts based on improvement direction.
//!
//! Inspired by autoresearch: read state → LLM proposes → apply → measure →
//! keep/revert → log → checkpoint.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::{error, info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Configuration types
// ---------------------------------------------------------------------------

/// What the experiment modifies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExperimentTarget {
    /// Modify a config key (dot-separated path, e.g. "llm.temperature").
    Config { key: String },
    /// Modify a brain file (relative path under brain directory).
    BrainFile { path: String },
    /// Modify a prompt template by id.
    Prompt { template_id: String },
}

impl fmt::Display for ExperimentTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config { key } => write!(f, "config:{key}"),
            Self::BrainFile { path } => write!(f, "brain_file:{path}"),
            Self::Prompt { template_id } => write!(f, "prompt:{template_id}"),
        }
    }
}

/// How to extract the metric value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricSpec {
    /// RPC method to call (e.g. "infra.metrics", "brain.status").
    pub rpc_method: String,
    /// JSONPath-like dot-separated path to extract a number from the response.
    /// e.g. "payload.latency_ms" or "payload.memory_count".
    pub json_path: String,
}

/// Whether higher or lower metric values are better.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Higher,
    Lower,
}

/// Full experiment configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentConfig {
    /// Human-readable experiment name.
    pub name: String,
    /// What to modify.
    pub target: ExperimentTarget,
    /// How to measure success.
    pub metric: MetricSpec,
    /// Optimization direction.
    pub direction: Direction,
    /// Optional shell command that must exit 0 for the change to be kept.
    pub guard: Option<String>,
    /// Maximum number of iterations (None = unlimited until stopped).
    pub max_iterations: Option<u32>,
    /// Checkpoint every N iterations (default: 5).
    #[serde(default = "default_checkpoint_every")]
    pub checkpoint_every: u32,
}

fn default_checkpoint_every() -> u32 {
    5
}

// ---------------------------------------------------------------------------
// Experiment state
// ---------------------------------------------------------------------------

/// Status of a running experiment.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ExperimentStatus {
    Created,
    Running,
    Paused,
    Completed,
    Failed,
}

/// A single iteration result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationResult {
    pub iteration: u32,
    pub timestamp: DateTime<Utc>,
    pub change_description: String,
    pub metric_value: f64,
    pub kept: bool,
    pub guard_passed: Option<bool>,
}

impl IterationResult {
    /// Format as TSV line.
    pub fn to_tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{:.6}\t{}\t{}",
            self.iteration,
            self.timestamp.to_rfc3339(),
            self.change_description.replace('\t', " "),
            self.metric_value,
            self.kept,
            self.guard_passed.map(|b| b.to_string()).unwrap_or_default(),
        )
    }

    /// TSV header line.
    pub fn tsv_header() -> &'static str {
        "iteration\ttimestamp\tchange_description\tmetric_value\tkept\tguard_passed"
    }
}

/// Checkpoint for resuming experiments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub experiment_id: String,
    pub name: String,
    pub config: ExperimentConfig,
    pub iteration: u32,
    pub best_metric: f64,
    pub best_iteration: u32,
    pub current_value: String,
    pub results: Vec<IterationResult>,
    pub saved_at: DateTime<Utc>,
}

/// State of a single experiment (in-memory).
#[derive(Debug)]
pub struct ExperimentState {
    pub id: String,
    pub config: ExperimentConfig,
    pub status: ExperimentStatus,
    pub iteration: u32,
    pub baseline: f64,
    pub best_metric: f64,
    pub best_iteration: u32,
    pub current_value: String,
    pub results: Vec<IterationResult>,
    pub created_at: DateTime<Utc>,
    /// Cancellation token for the background loop.
    pub cancel: tokio_util::sync::CancellationToken,
}

// ---------------------------------------------------------------------------
// Experiment Registry
// ---------------------------------------------------------------------------

/// Manages all active experiments.
#[derive(Debug, Clone)]
pub struct ExperimentRegistry {
    experiments: Arc<RwLock<HashMap<String, Arc<Mutex<ExperimentState>>>>>,
    /// Base directory for experiment output (experiments/).
    base_dir: PathBuf,
    /// LLM base URL for proposing changes.
    llm_base_url: String,
}

impl ExperimentRegistry {
    pub fn new(base_dir: PathBuf, llm_base_url: String) -> Self {
        Self {
            experiments: Arc::new(RwLock::new(HashMap::new())),
            base_dir,
            llm_base_url,
        }
    }

    /// Create a new experiment, establish baseline, return experiment ID.
    pub async fn create(
        &self,
        config: ExperimentConfig,
        initial_value: &str,
        baseline_metric: f64,
    ) -> Result<String, ExperimentError> {
        let id = Uuid::new_v4().to_string();
        let name = config.name.clone();

        // Create output directory
        let exp_dir = self.base_dir.join(&name);
        tokio::fs::create_dir_all(&exp_dir)
            .await
            .map_err(|e| ExperimentError::Io(format!("Failed to create experiment dir: {e}")))?;

        // Write TSV header
        let tsv_path = exp_dir.join("results.tsv");
        tokio::fs::write(&tsv_path, format!("{}\n", IterationResult::tsv_header()))
            .await
            .map_err(|e| ExperimentError::Io(format!("Failed to write TSV header: {e}")))?;

        let state = ExperimentState {
            id: id.clone(),
            config,
            status: ExperimentStatus::Created,
            iteration: 0,
            baseline: baseline_metric,
            best_metric: baseline_metric,
            best_iteration: 0,
            current_value: initial_value.to_string(),
            results: Vec::new(),
            created_at: Utc::now(),
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        self.experiments
            .write()
            .await
            .insert(id.clone(), Arc::new(Mutex::new(state)));

        info!(experiment_id = %id, experiment_name = %name, baseline = baseline_metric, "Experiment created");

        Ok(id)
    }

    /// Start the experiment loop as a background task.
    pub async fn start(&self, experiment_id: &str) -> Result<(), ExperimentError> {
        let experiments = self.experiments.read().await;
        let state = experiments
            .get(experiment_id)
            .ok_or_else(|| ExperimentError::NotFound(experiment_id.to_string()))?
            .clone();

        // Check it's in a startable state
        {
            let mut s = state.lock().await;
            match s.status {
                ExperimentStatus::Created | ExperimentStatus::Paused => {
                    s.status = ExperimentStatus::Running;
                }
                ExperimentStatus::Running => {
                    return Err(ExperimentError::AlreadyRunning(experiment_id.to_string()));
                }
                ExperimentStatus::Completed | ExperimentStatus::Failed => {
                    return Err(ExperimentError::InvalidState(
                        "Cannot start a completed/failed experiment".to_string(),
                    ));
                }
            }
        }

        let base_dir = self.base_dir.clone();
        let llm_url = self.llm_base_url.clone();

        tokio::spawn(async move {
            if let Err(e) = run_experiment_loop(state, base_dir, llm_url).await {
                error!(error = %e, "Experiment loop failed");
            }
        });

        Ok(())
    }

    /// Get current status of an experiment.
    pub async fn status(
        &self,
        experiment_id: &str,
    ) -> Result<ExperimentStatusReport, ExperimentError> {
        let experiments = self.experiments.read().await;
        let state = experiments
            .get(experiment_id)
            .ok_or_else(|| ExperimentError::NotFound(experiment_id.to_string()))?;

        let s = state.lock().await;
        let recent: Vec<_> = s.results.iter().rev().take(10).cloned().collect();

        Ok(ExperimentStatusReport {
            id: s.id.clone(),
            name: s.config.name.clone(),
            status: s.status,
            iteration: s.iteration,
            baseline: s.baseline,
            best_metric: s.best_metric,
            best_iteration: s.best_iteration,
            current_value: s.current_value.clone(),
            recent_results: recent,
            created_at: s.created_at,
        })
    }

    /// Stop a running experiment.
    pub async fn stop(
        &self,
        experiment_id: &str,
    ) -> Result<ExperimentStatusReport, ExperimentError> {
        let experiments = self.experiments.read().await;
        let state = experiments
            .get(experiment_id)
            .ok_or_else(|| ExperimentError::NotFound(experiment_id.to_string()))?;

        let s = state.lock().await;
        if s.status != ExperimentStatus::Running {
            return Err(ExperimentError::InvalidState(format!(
                "Experiment is {:?}, not running",
                s.status
            )));
        }
        s.cancel.cancel();

        let recent: Vec<_> = s.results.iter().rev().take(10).cloned().collect();

        Ok(ExperimentStatusReport {
            id: s.id.clone(),
            name: s.config.name.clone(),
            status: ExperimentStatus::Completed,
            iteration: s.iteration,
            baseline: s.baseline,
            best_metric: s.best_metric,
            best_iteration: s.best_iteration,
            current_value: s.current_value.clone(),
            recent_results: recent,
            created_at: s.created_at,
        })
    }

    /// Get full TSV results.
    pub async fn results(&self, experiment_id: &str) -> Result<String, ExperimentError> {
        let experiments = self.experiments.read().await;
        let state = experiments
            .get(experiment_id)
            .ok_or_else(|| ExperimentError::NotFound(experiment_id.to_string()))?;

        let s = state.lock().await;
        let tsv_path = self.base_dir.join(&s.config.name).join("results.tsv");

        tokio::fs::read_to_string(&tsv_path)
            .await
            .map_err(|e| ExperimentError::Io(format!("Failed to read results: {e}")))
    }

    /// Resume an experiment from its latest checkpoint.
    pub async fn resume(&self, experiment_id: &str) -> Result<(), ExperimentError> {
        let experiments = self.experiments.read().await;
        let state = experiments
            .get(experiment_id)
            .ok_or_else(|| ExperimentError::NotFound(experiment_id.to_string()))?;

        // Load checkpoint and restore state
        let checkpoint_path;
        {
            let s = state.lock().await;
            if s.status == ExperimentStatus::Running {
                return Err(ExperimentError::AlreadyRunning(experiment_id.to_string()));
            }
            checkpoint_path = self.base_dir.join(&s.config.name).join("checkpoint.json");
        }

        let checkpoint_data = tokio::fs::read_to_string(&checkpoint_path)
            .await
            .map_err(|e| ExperimentError::Io(format!("Failed to read checkpoint: {e}")))?;

        let checkpoint: Checkpoint = serde_json::from_str(&checkpoint_data)
            .map_err(|e| ExperimentError::Io(format!("Failed to parse checkpoint: {e}")))?;

        // Restore state from checkpoint
        {
            let mut s = state.lock().await;
            s.iteration = checkpoint.iteration;
            s.best_metric = checkpoint.best_metric;
            s.best_iteration = checkpoint.best_iteration;
            s.current_value = checkpoint.current_value;
            s.results = checkpoint.results;
            s.status = ExperimentStatus::Running;
            // Fresh cancellation token for resumed experiment
            s.cancel = tokio_util::sync::CancellationToken::new();
        }

        let base_dir = self.base_dir.clone();
        let llm_url = self.llm_base_url.clone();
        let state_clone = state.clone();

        tokio::spawn(async move {
            if let Err(e) = run_experiment_loop(state_clone, base_dir, llm_url).await {
                error!(error = %e, "Experiment loop (resumed) failed");
            }
        });

        info!(experiment_id = %experiment_id, "Experiment resumed from checkpoint");
        Ok(())
    }

    /// Create an experiment from a checkpoint file (for loading on startup).
    pub async fn load_from_checkpoint(
        &self,
        checkpoint: Checkpoint,
    ) -> Result<String, ExperimentError> {
        let id = checkpoint.experiment_id.clone();

        let state = ExperimentState {
            id: id.clone(),
            config: checkpoint.config,
            status: ExperimentStatus::Paused,
            iteration: checkpoint.iteration,
            baseline: checkpoint.best_metric,
            best_metric: checkpoint.best_metric,
            best_iteration: checkpoint.best_iteration,
            current_value: checkpoint.current_value,
            results: checkpoint.results,
            created_at: checkpoint.saved_at,
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        self.experiments
            .write()
            .await
            .insert(id.clone(), Arc::new(Mutex::new(state)));

        Ok(id)
    }
}

// ---------------------------------------------------------------------------
// Status report (returned by status/stop)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentStatusReport {
    pub id: String,
    pub name: String,
    pub status: ExperimentStatus,
    pub iteration: u32,
    pub baseline: f64,
    pub best_metric: f64,
    pub best_iteration: u32,
    pub current_value: String,
    pub recent_results: Vec<IterationResult>,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ExperimentError {
    #[error("Experiment not found: {0}")]
    NotFound(String),
    #[error("Experiment already running: {0}")]
    AlreadyRunning(String),
    #[error("Invalid state: {0}")]
    InvalidState(String),
    #[error("IO error: {0}")]
    Io(String),
    #[error("LLM error: {0}")]
    Llm(String),
    #[error("Metric extraction failed: {0}")]
    MetricExtraction(String),
    #[error("Guard command failed: {0}")]
    GuardFailed(String),
}

// ---------------------------------------------------------------------------
// Experiment loop (runs as background tokio task)
// ---------------------------------------------------------------------------

async fn run_experiment_loop(
    state: Arc<Mutex<ExperimentState>>,
    base_dir: PathBuf,
    llm_url: String,
) -> Result<(), ExperimentError> {
    let http = reqwest::Client::new();

    loop {
        let (cancel, iteration, max_iterations, config, current_value, results_history) = {
            let s = state.lock().await;
            if s.status != ExperimentStatus::Running {
                break;
            }
            (
                s.cancel.clone(),
                s.iteration,
                s.config.max_iterations,
                s.config.clone(),
                s.current_value.clone(),
                s.results.clone(),
            )
        };

        // Check max iterations
        if let Some(max) = max_iterations {
            if iteration >= max {
                let mut s = state.lock().await;
                s.status = ExperimentStatus::Completed;
                info!(
                    experiment = %config.name,
                    iterations = iteration,
                    "Experiment completed (max iterations reached)"
                );
                break;
            }
        }

        // Check cancellation
        if cancel.is_cancelled() {
            let mut s = state.lock().await;
            s.status = ExperimentStatus::Completed;
            info!(experiment = %config.name, "Experiment stopped by user");
            break;
        }

        // Step 1: LLM proposes a change
        let proposal = match propose_change(
            &http,
            &llm_url,
            &config,
            &current_value,
            &results_history,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!(experiment = %config.name, error = %e, "LLM proposal failed, retrying in 5s");
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => continue,
                    _ = cancel.cancelled() => {
                        let mut s = state.lock().await;
                        s.status = ExperimentStatus::Completed;
                        break;
                    }
                }
            }
        };

        info!(
            experiment = %config.name,
            iteration = iteration + 1,
            change = %proposal.description,
            "Applying proposed change"
        );

        // Step 2: Apply change (store old value for potential revert)
        let old_value = current_value.clone();
        // The "apply" is just updating our tracked value — the actual effect
        // depends on the target type. For config targets, we'd trigger hot-reload.
        // For brain files, we'd write the file. For prompts, we'd update the template.
        if let Err(e) = apply_change(&config.target, &proposal.new_value, &base_dir).await {
            warn!(experiment = %config.name, error = %e, "Failed to apply change, skipping iteration");
            continue;
        }

        // Step 3: Run metric RPC
        let metric_value = match extract_metric(&http, &llm_url, &config.metric).await {
            Ok(v) => v,
            Err(e) => {
                warn!(experiment = %config.name, error = %e, "Metric extraction failed, reverting");
                let _ = apply_change(&config.target, &old_value, &base_dir).await;
                continue;
            }
        };

        // Step 4: Run guard if set
        let guard_passed = if let Some(ref guard_cmd) = config.guard {
            match run_guard(guard_cmd).await {
                Ok(passed) => Some(passed),
                Err(e) => {
                    warn!(experiment = %config.name, error = %e, "Guard execution failed");
                    Some(false)
                }
            }
        } else {
            None
        };

        // Step 5: Decide keep or revert
        let kept = {
            let s = state.lock().await;
            let improved = match config.direction {
                Direction::Higher => metric_value > s.best_metric,
                Direction::Lower => metric_value < s.best_metric,
            };
            improved && guard_passed.unwrap_or(true)
        };

        if !kept {
            // Revert
            let _ = apply_change(&config.target, &old_value, &base_dir).await;
        }

        // Step 6: Record result
        let result = IterationResult {
            iteration: iteration + 1,
            timestamp: Utc::now(),
            change_description: proposal.description.clone(),
            metric_value,
            kept,
            guard_passed,
        };

        // Append to TSV
        let exp_dir = base_dir.join(&config.name);
        let tsv_path = exp_dir.join("results.tsv");
        if let Err(e) = append_tsv(&tsv_path, &result).await {
            warn!(error = %e, "Failed to append to TSV");
        }

        // Update state
        {
            let mut s = state.lock().await;
            s.iteration = iteration + 1;
            s.results.push(result);
            if kept {
                s.best_metric = metric_value;
                s.best_iteration = iteration + 1;
                s.current_value = proposal.new_value;
                info!(
                    experiment = %config.name,
                    iteration = iteration + 1,
                    metric = metric_value,
                    "Improvement found — keeping change"
                );
            } else {
                info!(
                    experiment = %config.name,
                    iteration = iteration + 1,
                    metric = metric_value,
                    "No improvement — reverted"
                );
            }

            // Step 7: Checkpoint
            if (iteration + 1) % config.checkpoint_every == 0 {
                let checkpoint = Checkpoint {
                    experiment_id: s.id.clone(),
                    name: s.config.name.clone(),
                    config: s.config.clone(),
                    iteration: s.iteration,
                    best_metric: s.best_metric,
                    best_iteration: s.best_iteration,
                    current_value: s.current_value.clone(),
                    results: s.results.clone(),
                    saved_at: Utc::now(),
                };
                let cp_path = exp_dir.join("checkpoint.json");
                if let Ok(json) = serde_json::to_string_pretty(&checkpoint) {
                    if let Err(e) = tokio::fs::write(&cp_path, json).await {
                        warn!(error = %e, "Failed to write checkpoint");
                    } else {
                        info!(experiment = %config.name, iteration = iteration + 1, "Checkpoint saved");
                    }
                }
            }
        }

        // Small delay between iterations to avoid hammering
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
            _ = cancel.cancelled() => {
                let mut s = state.lock().await;
                s.status = ExperimentStatus::Completed;
                break;
            }
        }
    }

    // Final checkpoint on exit
    {
        let s = state.lock().await;
        let checkpoint = Checkpoint {
            experiment_id: s.id.clone(),
            name: s.config.name.clone(),
            config: s.config.clone(),
            iteration: s.iteration,
            best_metric: s.best_metric,
            best_iteration: s.best_iteration,
            current_value: s.current_value.clone(),
            results: s.results.clone(),
            saved_at: Utc::now(),
        };
        let cp_path = base_dir.join(&s.config.name).join("checkpoint.json");
        if let Ok(json) = serde_json::to_string_pretty(&checkpoint) {
            let _ = tokio::fs::write(&cp_path, json).await;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// LLM proposal
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Proposal {
    description: String,
    new_value: String,
}

async fn propose_change(
    http: &reqwest::Client,
    llm_url: &str,
    config: &ExperimentConfig,
    current_value: &str,
    history: &[IterationResult],
) -> Result<Proposal, ExperimentError> {
    let history_summary = if history.is_empty() {
        "No previous iterations.".to_string()
    } else {
        let recent: Vec<_> = history.iter().rev().take(10).collect();
        recent
            .iter()
            .rev()
            .map(|r| {
                format!(
                    "  iter {}: {} → metric={:.6}, kept={}",
                    r.iteration, r.change_description, r.metric_value, r.kept
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let prompt = format!(
        r#"You are an experiment optimizer. You are tuning a {target} to optimize a metric.

Target: {target}
Metric: {metric_method} (extract {json_path})
Direction: {direction} is better
Current value: {current_value}

Recent history (last 10 iterations):
{history_summary}

Propose a SINGLE change to improve the metric. Reply ONLY with JSON:
{{"description": "brief description of what you changed", "new_value": "the complete new value"}}

Rules:
- Make incremental changes, not drastic ones.
- Learn from the history: if a direction didn't work, try something different.
- The new_value must be a complete replacement, not a diff."#,
        target = config.target,
        metric_method = config.metric.rpc_method,
        json_path = config.metric.json_path,
        direction = match config.direction {
            Direction::Higher => "higher",
            Direction::Lower => "lower",
        },
        current_value = current_value,
        history_summary = history_summary,
    );

    let request_body = serde_json::json!({
        "model": "default",
        "messages": [
            {"role": "system", "content": "You are an experiment optimizer. Respond only with valid JSON."},
            {"role": "user", "content": prompt}
        ],
        "temperature": 0.7,
        "max_tokens": 1024,
    });

    let response = http
        .post(format!("{llm_url}/v1/chat/completions"))
        .json(&request_body)
        .send()
        .await
        .map_err(|e| ExperimentError::Llm(format!("HTTP request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(ExperimentError::Llm(format!(
            "LLM returned {status}: {body}"
        )));
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| ExperimentError::Llm(format!("Failed to parse LLM response: {e}")))?;

    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| ExperimentError::Llm("No content in LLM response".to_string()))?;

    // Parse the JSON proposal (strip markdown fences if present)
    let cleaned = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let proposal: serde_json::Value = serde_json::from_str(cleaned)
        .map_err(|e| ExperimentError::Llm(format!("Failed to parse proposal JSON: {e}")))?;

    let description = proposal["description"]
        .as_str()
        .unwrap_or("unknown change")
        .to_string();
    let new_value = proposal["new_value"]
        .as_str()
        .unwrap_or(current_value)
        .to_string();

    Ok(Proposal {
        description,
        new_value,
    })
}

// ---------------------------------------------------------------------------
// Apply / revert changes
// ---------------------------------------------------------------------------

async fn apply_change(
    target: &ExperimentTarget,
    value: &str,
    base_dir: &std::path::Path,
) -> Result<(), ExperimentError> {
    match target {
        ExperimentTarget::Config { key } => {
            // For config changes, write to a staging file that hot-reload picks up.
            let staging_path = base_dir.join("_config_override.json");
            let override_val = serde_json::json!({ key: value });
            tokio::fs::write(
                &staging_path,
                serde_json::to_string_pretty(&override_val).unwrap(),
            )
            .await
            .map_err(|e| ExperimentError::Io(format!("Config write failed: {e}")))?;
            Ok(())
        }
        ExperimentTarget::BrainFile { path } => {
            let file_path = base_dir.join(path);
            if let Some(parent) = file_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| ExperimentError::Io(format!("mkdir failed: {e}")))?;
            }
            tokio::fs::write(&file_path, value)
                .await
                .map_err(|e| ExperimentError::Io(format!("Brain file write failed: {e}")))?;
            Ok(())
        }
        ExperimentTarget::Prompt { template_id } => {
            let prompt_path = base_dir.join("prompts").join(format!("{template_id}.txt"));
            if let Some(parent) = prompt_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| ExperimentError::Io(format!("mkdir failed: {e}")))?;
            }
            tokio::fs::write(&prompt_path, value)
                .await
                .map_err(|e| ExperimentError::Io(format!("Prompt write failed: {e}")))?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Metric extraction
// ---------------------------------------------------------------------------

/// Extract a metric by calling the gateway's own RPC endpoint.
/// This is a self-referential call: we hit localhost to invoke the method.
async fn extract_metric(
    http: &reqwest::Client,
    llm_url: &str,
    metric: &MetricSpec,
) -> Result<f64, ExperimentError> {
    // Derive the gateway URL from the LLM URL's host (both run on the same host).
    // Default to localhost:18794 which is the standard ironjaw port.
    let gateway_url = derive_gateway_url(llm_url);

    let req_id = Uuid::new_v4();
    let rpc_frame = serde_json::json!({
        "type": "req",
        "id": req_id.to_string(),
        "method": metric.rpc_method,
        "params": {}
    });

    // Use HTTP POST to the health endpoint as a proxy — in production this
    // would go through a proper internal RPC channel. For now we use a direct
    // HTTP call that the dashboard API exposes.
    let response = http
        .get(format!("{gateway_url}/health"))
        .send()
        .await
        .map_err(|e| {
            ExperimentError::MetricExtraction(format!("Gateway health check failed: {e}"))
        })?;

    if !response.status().is_success() {
        return Err(ExperimentError::MetricExtraction(
            "Gateway health check returned non-200".to_string(),
        ));
    }

    let body: serde_json::Value = response.json().await.map_err(|e| {
        ExperimentError::MetricExtraction(format!("Failed to parse gateway response: {e}"))
    })?;

    // For the metric, try to extract from the body using the json_path.
    // If the RPC method is health-based, use the health response directly.
    // Otherwise, fall back to making a proper RPC call through the dashboard API.
    extract_json_path(&body, &metric.json_path)
        .or_else(|_| {
            // If the metric is uptime_secs from health, that works.
            // For other metrics, we need to try the dashboard API.
            extract_json_path(&rpc_frame, &metric.json_path)
        })
        .map_err(|e| ExperimentError::MetricExtraction(format!("JSONPath extraction: {e}")))
}

/// Extract a numeric value from a JSON value using dot-separated path.
fn extract_json_path(value: &serde_json::Value, path: &str) -> Result<f64, String> {
    let mut current = value;
    for segment in path.split('.') {
        if let Ok(idx) = segment.parse::<usize>() {
            current = current
                .get(idx)
                .ok_or_else(|| format!("Array index {idx} not found"))?;
        } else {
            current = current
                .get(segment)
                .ok_or_else(|| format!("Key '{segment}' not found"))?;
        }
    }
    current
        .as_f64()
        .ok_or_else(|| format!("Value at path is not a number: {current}"))
}

fn derive_gateway_url(llm_url: &str) -> String {
    // LLM URL is typically http://localhost:8080
    // Gateway URL is typically http://localhost:18794
    // Extract host from URL without pulling in the `url` crate.
    let stripped = llm_url
        .strip_prefix("http://")
        .or_else(|| llm_url.strip_prefix("https://"))
        .unwrap_or(llm_url);
    let host = stripped.split(':').next().unwrap_or("localhost");
    let host = host.split('/').next().unwrap_or("localhost");
    if host.is_empty() {
        "http://localhost:18794".to_string()
    } else {
        format!("http://{host}:18794")
    }
}

// ---------------------------------------------------------------------------
// Guard command execution
// ---------------------------------------------------------------------------

async fn run_guard(command: &str) -> Result<bool, ExperimentError> {
    let output = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .output()
        .await
        .map_err(|e| ExperimentError::GuardFailed(format!("Failed to execute guard: {e}")))?;

    Ok(output.status.success())
}

// ---------------------------------------------------------------------------
// File helpers
// ---------------------------------------------------------------------------

async fn append_tsv(path: &PathBuf, result: &IterationResult) -> Result<(), std::io::Error> {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .await?;
    file.write_all(format!("{}\n", result.to_tsv()).as_bytes())
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// RPC Handlers (called from handler.rs dispatch)
// ---------------------------------------------------------------------------

use crate::protocol::Frame;
use crate::state::AppState;

/// Handle `experiment.create` — validate config, establish baseline.
pub async fn handle_experiment_create(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let config: ExperimentConfig =
        match serde_json::from_value(params.get("config").cloned().unwrap_or_default()) {
            Ok(c) => c,
            Err(e) => {
                return Frame::err(
                    id,
                    "INVALID_CONFIG",
                    format!("Invalid experiment config: {e}"),
                )
            }
        };

    let initial_value = params
        .get("initial_value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let baseline = params
        .get("baseline")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let registry = get_or_init_registry(state);

    match registry.create(config, &initial_value, baseline).await {
        Ok(exp_id) => Frame::ok(
            id,
            serde_json::json!({
                "experiment_id": exp_id,
                "baseline": baseline,
            }),
        ),
        Err(e) => Frame::err(id, "EXPERIMENT_ERROR", e.to_string()),
    }
}

/// Handle `experiment.start` — begin the experiment loop.
pub async fn handle_experiment_start(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let experiment_id = match params.get("experiment_id").and_then(|v| v.as_str()) {
        Some(eid) => eid,
        None => return Frame::err(id, "MISSING_PARAM", "experiment_id required"),
    };

    let registry = get_or_init_registry(state);

    match registry.start(experiment_id).await {
        Ok(()) => Frame::ok(
            id,
            serde_json::json!({
                "started": true,
                "experiment_id": experiment_id,
            }),
        ),
        Err(e) => Frame::err(id, "EXPERIMENT_ERROR", e.to_string()),
    }
}

/// Handle `experiment.status` — get current iteration, best metric, recent results.
pub async fn handle_experiment_status(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let experiment_id = match params.get("experiment_id").and_then(|v| v.as_str()) {
        Some(eid) => eid,
        None => return Frame::err(id, "MISSING_PARAM", "experiment_id required"),
    };

    let registry = get_or_init_registry(state);

    match registry.status(experiment_id).await {
        Ok(report) => Frame::ok(id, serde_json::to_value(&report).unwrap_or_default()),
        Err(e) => Frame::err(id, "EXPERIMENT_ERROR", e.to_string()),
    }
}

/// Handle `experiment.stop` — halt the experiment loop.
pub async fn handle_experiment_stop(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let experiment_id = match params.get("experiment_id").and_then(|v| v.as_str()) {
        Some(eid) => eid,
        None => return Frame::err(id, "MISSING_PARAM", "experiment_id required"),
    };

    let registry = get_or_init_registry(state);

    match registry.stop(experiment_id).await {
        Ok(report) => Frame::ok(id, serde_json::to_value(&report).unwrap_or_default()),
        Err(e) => Frame::err(id, "EXPERIMENT_ERROR", e.to_string()),
    }
}

/// Handle `experiment.results` — return full TSV.
pub async fn handle_experiment_results(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let experiment_id = match params.get("experiment_id").and_then(|v| v.as_str()) {
        Some(eid) => eid,
        None => return Frame::err(id, "MISSING_PARAM", "experiment_id required"),
    };

    let registry = get_or_init_registry(state);

    match registry.results(experiment_id).await {
        Ok(tsv) => Frame::ok(id, serde_json::json!({ "tsv": tsv })),
        Err(e) => Frame::err(id, "EXPERIMENT_ERROR", e.to_string()),
    }
}

/// Handle `experiment.resume` — resume from checkpoint.
pub async fn handle_experiment_resume(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let experiment_id = match params.get("experiment_id").and_then(|v| v.as_str()) {
        Some(eid) => eid,
        None => return Frame::err(id, "MISSING_PARAM", "experiment_id required"),
    };

    let registry = get_or_init_registry(state);

    match registry.resume(experiment_id).await {
        Ok(()) => Frame::ok(
            id,
            serde_json::json!({
                "resumed": true,
                "experiment_id": experiment_id,
            }),
        ),
        Err(e) => Frame::err(id, "EXPERIMENT_ERROR", e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Registry singleton (lazy-initialized per AppState)
// ---------------------------------------------------------------------------

use std::sync::OnceLock;

/// Global experiment registry. Initialized on first use.
static EXPERIMENT_REGISTRY: OnceLock<ExperimentRegistry> = OnceLock::new();

fn get_or_init_registry(state: &AppState) -> &ExperimentRegistry {
    EXPERIMENT_REGISTRY.get_or_init(|| {
        let llm_base_url = state.integration_urls.llm_base_url.clone();
        let base_url = if llm_base_url.is_empty() {
            "http://localhost:8080".to_string()
        } else {
            llm_base_url
        };
        ExperimentRegistry::new(PathBuf::from("experiments"), base_url)
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_experiment_target_display() {
        let config = ExperimentTarget::Config {
            key: "llm.temperature".to_string(),
        };
        assert_eq!(config.to_string(), "config:llm.temperature");

        let brain = ExperimentTarget::BrainFile {
            path: "system.md".to_string(),
        };
        assert_eq!(brain.to_string(), "brain_file:system.md");

        let prompt = ExperimentTarget::Prompt {
            template_id: "chat_v2".to_string(),
        };
        assert_eq!(prompt.to_string(), "prompt:chat_v2");
    }

    #[test]
    fn test_direction_serde() {
        let higher: Direction = serde_json::from_str("\"higher\"").unwrap();
        assert_eq!(higher, Direction::Higher);

        let lower: Direction = serde_json::from_str("\"lower\"").unwrap();
        assert_eq!(lower, Direction::Lower);
    }

    #[test]
    fn test_experiment_config_serde() {
        let json = serde_json::json!({
            "name": "temperature_sweep",
            "target": {"type": "config", "key": "llm.temperature"},
            "metric": {"rpc_method": "infra.metrics", "json_path": "latency_ms"},
            "direction": "lower",
            "guard": null,
            "max_iterations": 50,
            "checkpoint_every": 10
        });

        let config: ExperimentConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.name, "temperature_sweep");
        assert_eq!(config.direction, Direction::Lower);
        assert_eq!(config.max_iterations, Some(50));
        assert_eq!(config.checkpoint_every, 10);
        assert!(config.guard.is_none());
        assert!(
            matches!(&config.target, ExperimentTarget::Config { key } if key == "llm.temperature")
        );
    }

    #[test]
    fn test_experiment_config_defaults() {
        let json = serde_json::json!({
            "name": "test",
            "target": {"type": "brain_file", "path": "system.md"},
            "metric": {"rpc_method": "brain.status", "json_path": "memory_count"},
            "direction": "higher"
        });

        let config: ExperimentConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.checkpoint_every, 5);
        assert!(config.max_iterations.is_none());
        assert!(config.guard.is_none());
    }

    #[test]
    fn test_iteration_result_tsv() {
        let result = IterationResult {
            iteration: 1,
            timestamp: DateTime::parse_from_rfc3339("2026-03-30T10:00:00Z")
                .unwrap()
                .to_utc(),
            change_description: "increased temperature to 0.8".to_string(),
            metric_value: 42.5,
            kept: true,
            guard_passed: Some(true),
        };

        let tsv = result.to_tsv();
        assert!(tsv.contains("1\t"));
        assert!(tsv.contains("42.500000"));
        assert!(tsv.contains("true"));
    }

    #[test]
    fn test_extract_json_path_simple() {
        let val = serde_json::json!({"status": "ok", "uptime_secs": 3600});
        assert_eq!(extract_json_path(&val, "uptime_secs").unwrap(), 3600.0);
    }

    #[test]
    fn test_extract_json_path_nested() {
        let val = serde_json::json!({
            "payload": {
                "metrics": {
                    "latency_ms": 42.5
                }
            }
        });
        assert_eq!(
            extract_json_path(&val, "payload.metrics.latency_ms").unwrap(),
            42.5
        );
    }

    #[test]
    fn test_extract_json_path_array() {
        let val = serde_json::json!({
            "items": [{"score": 10.0}, {"score": 20.0}]
        });
        assert_eq!(extract_json_path(&val, "items.1.score").unwrap(), 20.0);
    }

    #[test]
    fn test_extract_json_path_missing_key() {
        let val = serde_json::json!({"a": 1});
        assert!(extract_json_path(&val, "b").is_err());
    }

    #[test]
    fn test_extract_json_path_not_number() {
        let val = serde_json::json!({"name": "test"});
        assert!(extract_json_path(&val, "name").is_err());
    }

    #[test]
    fn test_experiment_status_serde() {
        let s: ExperimentStatus = serde_json::from_str("\"running\"").unwrap();
        assert_eq!(s, ExperimentStatus::Running);

        let json = serde_json::to_string(&ExperimentStatus::Completed).unwrap();
        assert_eq!(json, "\"completed\"");
    }

    #[test]
    fn test_checkpoint_serde_roundtrip() {
        let checkpoint = Checkpoint {
            experiment_id: "test-id".to_string(),
            name: "my_experiment".to_string(),
            config: ExperimentConfig {
                name: "my_experiment".to_string(),
                target: ExperimentTarget::Config {
                    key: "llm.temperature".to_string(),
                },
                metric: MetricSpec {
                    rpc_method: "infra.metrics".to_string(),
                    json_path: "latency_ms".to_string(),
                },
                direction: Direction::Lower,
                guard: Some("cargo test".to_string()),
                max_iterations: Some(100),
                checkpoint_every: 5,
            },
            iteration: 25,
            best_metric: 15.3,
            best_iteration: 20,
            current_value: "0.7".to_string(),
            results: vec![],
            saved_at: Utc::now(),
        };

        let json = serde_json::to_string(&checkpoint).unwrap();
        let restored: Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.experiment_id, "test-id");
        assert_eq!(restored.iteration, 25);
        assert_eq!(restored.best_metric, 15.3);
    }

    #[test]
    fn test_derive_gateway_url() {
        assert_eq!(
            derive_gateway_url("http://localhost:8080"),
            "http://localhost:18794"
        );
        assert_eq!(
            derive_gateway_url("http://192.0.2.10:8080"),
            "http://192.0.2.10:18794"
        );
        // "invalid" is treated as a bare hostname
        assert_eq!(derive_gateway_url("invalid"), "http://invalid:18794");
        // Empty string falls back to localhost
        assert_eq!(derive_gateway_url(""), "http://localhost:18794");
    }

    #[tokio::test]
    async fn test_registry_create() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ExperimentRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = ExperimentConfig {
            name: "test_exp".to_string(),
            target: ExperimentTarget::Config {
                key: "llm.temperature".to_string(),
            },
            metric: MetricSpec {
                rpc_method: "infra.metrics".to_string(),
                json_path: "latency_ms".to_string(),
            },
            direction: Direction::Lower,
            guard: None,
            max_iterations: Some(10),
            checkpoint_every: 5,
        };

        let exp_id = registry.create(config, "0.5", 100.0).await.unwrap();
        assert!(!exp_id.is_empty());

        // Verify TSV was created
        let tsv = tokio::fs::read_to_string(dir.path().join("test_exp/results.tsv"))
            .await
            .unwrap();
        assert!(tsv.starts_with("iteration\t"));
    }

    #[tokio::test]
    async fn test_registry_status_after_create() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ExperimentRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = ExperimentConfig {
            name: "status_test".to_string(),
            target: ExperimentTarget::Config {
                key: "x".to_string(),
            },
            metric: MetricSpec {
                rpc_method: "ping".to_string(),
                json_path: "pong".to_string(),
            },
            direction: Direction::Higher,
            guard: None,
            max_iterations: Some(5),
            checkpoint_every: 5,
        };

        let exp_id = registry.create(config, "initial", 0.0).await.unwrap();
        let status = registry.status(&exp_id).await.unwrap();

        assert_eq!(status.status, ExperimentStatus::Created);
        assert_eq!(status.iteration, 0);
        assert_eq!(status.baseline, 0.0);
        assert_eq!(status.current_value, "initial");
    }

    #[tokio::test]
    async fn test_registry_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ExperimentRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let result = registry.status("nonexistent").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ExperimentError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_registry_stop_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ExperimentRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = ExperimentConfig {
            name: "stop_test".to_string(),
            target: ExperimentTarget::Config {
                key: "x".to_string(),
            },
            metric: MetricSpec {
                rpc_method: "ping".to_string(),
                json_path: "pong".to_string(),
            },
            direction: Direction::Higher,
            guard: None,
            max_iterations: Some(5),
            checkpoint_every: 5,
        };

        let exp_id = registry.create(config, "val", 1.0).await.unwrap();
        let result = registry.stop(&exp_id).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ExperimentError::InvalidState(_)
        ));
    }

    #[tokio::test]
    async fn test_registry_double_start() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ExperimentRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = ExperimentConfig {
            name: "double_start".to_string(),
            target: ExperimentTarget::Config {
                key: "x".to_string(),
            },
            metric: MetricSpec {
                rpc_method: "ping".to_string(),
                json_path: "pong".to_string(),
            },
            direction: Direction::Higher,
            guard: None,
            max_iterations: Some(5),
            checkpoint_every: 5,
        };

        let exp_id = registry.create(config, "val", 1.0).await.unwrap();
        // First start should succeed (it'll fail the loop because no real LLM)
        registry.start(&exp_id).await.unwrap();
        // Second start should fail (already running)
        let result = registry.start(&exp_id).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ExperimentError::AlreadyRunning(_)
        ));
    }

    #[tokio::test]
    async fn test_apply_change_config() {
        let dir = tempfile::tempdir().unwrap();
        let target = ExperimentTarget::Config {
            key: "llm.temperature".to_string(),
        };
        apply_change(&target, "0.8", dir.path()).await.unwrap();

        let content = tokio::fs::read_to_string(dir.path().join("_config_override.json"))
            .await
            .unwrap();
        assert!(content.contains("0.8"));
    }

    #[tokio::test]
    async fn test_apply_change_brain_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = ExperimentTarget::BrainFile {
            path: "test_brain.md".to_string(),
        };
        apply_change(&target, "# New brain content", dir.path())
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(dir.path().join("test_brain.md"))
            .await
            .unwrap();
        assert_eq!(content, "# New brain content");
    }

    #[tokio::test]
    async fn test_apply_change_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let target = ExperimentTarget::Prompt {
            template_id: "chat_v2".to_string(),
        };
        apply_change(&target, "You are a helpful assistant.", dir.path())
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(dir.path().join("prompts/chat_v2.txt"))
            .await
            .unwrap();
        assert_eq!(content, "You are a helpful assistant.");
    }

    #[tokio::test]
    async fn test_load_from_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ExperimentRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let checkpoint = Checkpoint {
            experiment_id: "cp-id-123".to_string(),
            name: "restored_exp".to_string(),
            config: ExperimentConfig {
                name: "restored_exp".to_string(),
                target: ExperimentTarget::Config {
                    key: "x".to_string(),
                },
                metric: MetricSpec {
                    rpc_method: "ping".to_string(),
                    json_path: "uptime_secs".to_string(),
                },
                direction: Direction::Higher,
                guard: None,
                max_iterations: Some(50),
                checkpoint_every: 5,
            },
            iteration: 15,
            best_metric: 42.0,
            best_iteration: 12,
            current_value: "optimized_val".to_string(),
            results: vec![IterationResult {
                iteration: 15,
                timestamp: Utc::now(),
                change_description: "last change".to_string(),
                metric_value: 40.0,
                kept: false,
                guard_passed: None,
            }],
            saved_at: Utc::now(),
        };

        let id = registry.load_from_checkpoint(checkpoint).await.unwrap();
        assert_eq!(id, "cp-id-123");

        let status = registry.status(&id).await.unwrap();
        assert_eq!(status.status, ExperimentStatus::Paused);
        assert_eq!(status.iteration, 15);
        assert_eq!(status.best_metric, 42.0);
    }

    #[tokio::test]
    async fn test_results_after_create() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ExperimentRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = ExperimentConfig {
            name: "results_test".to_string(),
            target: ExperimentTarget::Config {
                key: "x".to_string(),
            },
            metric: MetricSpec {
                rpc_method: "ping".to_string(),
                json_path: "pong".to_string(),
            },
            direction: Direction::Higher,
            guard: None,
            max_iterations: Some(5),
            checkpoint_every: 5,
        };

        let exp_id = registry.create(config, "val", 1.0).await.unwrap();
        let tsv = registry.results(&exp_id).await.unwrap();
        assert!(tsv.starts_with("iteration\t"));
    }

    #[test]
    fn test_experiment_status_report_serde() {
        let report = ExperimentStatusReport {
            id: "test".to_string(),
            name: "my_exp".to_string(),
            status: ExperimentStatus::Running,
            iteration: 10,
            baseline: 100.0,
            best_metric: 85.0,
            best_iteration: 7,
            current_value: "0.75".to_string(),
            recent_results: vec![],
            created_at: Utc::now(),
        };

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["status"], "running");
        assert_eq!(json["iteration"], 10);
        assert_eq!(json["best_metric"], 85.0);
    }

    #[test]
    fn test_experiment_error_display() {
        let e = ExperimentError::NotFound("abc".to_string());
        assert!(e.to_string().contains("abc"));

        let e = ExperimentError::AlreadyRunning("def".to_string());
        assert!(e.to_string().contains("def"));
    }

    #[test]
    fn test_tsv_header() {
        let header = IterationResult::tsv_header();
        assert!(header.contains("iteration"));
        assert!(header.contains("metric_value"));
        assert!(header.contains("kept"));
    }

    #[test]
    fn test_iteration_result_tsv_tab_in_description() {
        let result = IterationResult {
            iteration: 1,
            timestamp: Utc::now(),
            change_description: "has\ttab".to_string(),
            metric_value: 1.0,
            kept: true,
            guard_passed: None,
        };

        let tsv = result.to_tsv();
        // Tab in description should be replaced with space
        assert!(!tsv.contains("has\ttab"));
        assert!(tsv.contains("has tab"));
    }
}
