//! Prompt-Space Evolution (I13 — GEPA Pattern)
//!
//! Maintains a population of brain file variants and evolves them through
//! LLM-driven mutation, evaluation, and tournament selection. Each variant
//! gets an Elo rating based on head-to-head comparisons on shared test prompts.
//!
//! Flow per generation:
//! 1. **Mutate**: LLM generates variant of each population member
//! 2. **Evaluate**: Run test conversations with each variant, score outcomes
//! 3. **Tournament**: Pairs compete head-to-head on same prompts
//! 4. **Select**: Keep top performers, discard bottom
//! 5. **Record**: Elo/Bradley-Terry ratings per variant

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::{error, info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for a prompt evolution run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionConfig {
    /// Human-readable name for this evolution run.
    pub name: String,
    /// Path to the brain file being evolved (relative to brain dir).
    pub target_file: String,
    /// Number of variants to maintain in the population.
    #[serde(default = "default_population_size")]
    pub population_size: usize,
    /// Test prompts to evaluate variants against.
    pub test_prompts: Vec<String>,
    /// Metric to evaluate responses (e.g. "relevance", "helpfulness", "conciseness").
    #[serde(default = "default_eval_metric")]
    pub eval_metric: String,
    /// Maximum number of generations (None = unlimited until stopped).
    pub max_generations: Option<u32>,
    /// Elo K-factor for rating updates.
    #[serde(default = "default_k_factor")]
    pub k_factor: f64,
    /// Initial Elo rating for new variants.
    #[serde(default = "default_initial_elo")]
    pub initial_elo: f64,
}

fn default_population_size() -> usize {
    4
}

fn default_eval_metric() -> String {
    "overall_quality".to_string()
}

fn default_k_factor() -> f64 {
    32.0
}

fn default_initial_elo() -> f64 {
    1500.0
}

// ---------------------------------------------------------------------------
// Variant & ratings
// ---------------------------------------------------------------------------

/// A single variant in the population.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variant {
    pub id: String,
    pub content: String,
    pub elo_rating: f64,
    pub generation_born: u32,
    pub parent_id: Option<String>,
    pub wins: u32,
    pub losses: u32,
    pub draws: u32,
}

/// Result of a head-to-head tournament match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchResult {
    pub prompt: String,
    pub variant_a_id: String,
    pub variant_b_id: String,
    /// Score for variant A: 1.0 = win, 0.0 = loss, 0.5 = draw.
    pub score_a: f64,
    pub generation: u32,
    pub timestamp: DateTime<Utc>,
    pub reasoning: String,
}

/// A generation record for history tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationRecord {
    pub generation: u32,
    pub timestamp: DateTime<Utc>,
    pub variants: Vec<VariantSnapshot>,
    pub matches: Vec<MatchResult>,
    pub best_variant_id: String,
    pub best_elo: f64,
}

/// Snapshot of a variant's state at a given generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantSnapshot {
    pub id: String,
    pub elo_rating: f64,
    pub content_preview: String,
}

// ---------------------------------------------------------------------------
// Evolution state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum EvolutionStatus {
    Created,
    Running,
    Paused,
    Completed,
    Failed,
}

/// Full evolution state.
#[derive(Debug)]
pub struct EvolutionState {
    pub id: String,
    pub config: EvolutionConfig,
    pub status: EvolutionStatus,
    pub generation: u32,
    pub population: Vec<Variant>,
    pub history: Vec<GenerationRecord>,
    pub created_at: DateTime<Utc>,
    pub cancel: tokio_util::sync::CancellationToken,
}

// ---------------------------------------------------------------------------
// Status report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionStatusReport {
    pub id: String,
    pub name: String,
    pub status: EvolutionStatus,
    pub generation: u32,
    pub population: Vec<VariantSummary>,
    pub best_variant_id: String,
    pub best_elo: f64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantSummary {
    pub id: String,
    pub elo_rating: f64,
    pub wins: u32,
    pub losses: u32,
    pub draws: u32,
    pub content_preview: String,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum EvolutionError {
    #[error("Evolution run not found: {0}")]
    NotFound(String),
    #[error("Already running: {0}")]
    AlreadyRunning(String),
    #[error("Invalid state: {0}")]
    InvalidState(String),
    #[error("IO error: {0}")]
    Io(String),
    #[error("LLM error: {0}")]
    Llm(String),
    #[error("Population error: {0}")]
    Population(String),
}

// ---------------------------------------------------------------------------
// Elo rating math
// ---------------------------------------------------------------------------

/// Calculate expected score for player A against player B.
fn expected_score(rating_a: f64, rating_b: f64) -> f64 {
    1.0 / (1.0 + 10.0_f64.powf((rating_b - rating_a) / 400.0))
}

/// Update Elo ratings after a match. Returns (new_a, new_b).
fn update_elo(rating_a: f64, rating_b: f64, score_a: f64, k: f64) -> (f64, f64) {
    let expected_a = expected_score(rating_a, rating_b);
    let expected_b = 1.0 - expected_a;
    let score_b = 1.0 - score_a;

    let new_a = rating_a + k * (score_a - expected_a);
    let new_b = rating_b + k * (score_b - expected_b);

    (new_a, new_b)
}

// ---------------------------------------------------------------------------
// Evolution Registry
// ---------------------------------------------------------------------------

/// Manages all active evolution runs.
#[derive(Debug, Clone)]
pub struct EvolutionRegistry {
    runs: Arc<RwLock<HashMap<String, Arc<Mutex<EvolutionState>>>>>,
    base_dir: PathBuf,
    llm_base_url: String,
}

impl EvolutionRegistry {
    pub fn new(base_dir: PathBuf, llm_base_url: String) -> Self {
        Self {
            runs: Arc::new(RwLock::new(HashMap::new())),
            base_dir,
            llm_base_url,
        }
    }

    /// Create a new evolution run. Reads the target file to seed the initial population.
    pub async fn create(&self, config: EvolutionConfig) -> Result<String, EvolutionError> {
        let id = Uuid::new_v4().to_string();
        let name = config.name.clone();

        // Read the original brain file
        let target_path = self.base_dir.join(&config.target_file);
        let original_content = tokio::fs::read_to_string(&target_path).await.map_err(|e| {
            EvolutionError::Io(format!(
                "Failed to read target file '{}': {e}",
                config.target_file
            ))
        })?;

        // Create evolution output directory
        let evo_dir = self.base_dir.join("evolution").join(&name);
        tokio::fs::create_dir_all(&evo_dir)
            .await
            .map_err(|e| EvolutionError::Io(format!("Failed to create evolution dir: {e}")))?;

        if config.test_prompts.is_empty() {
            return Err(EvolutionError::Population(
                "At least one test prompt is required".to_string(),
            ));
        }

        if config.population_size < 2 {
            return Err(EvolutionError::Population(
                "Population size must be at least 2".to_string(),
            ));
        }

        // Seed population: the original is variant 0, rest are copies initially
        let mut population = Vec::new();
        for i in 0..config.population_size {
            population.push(Variant {
                id: format!("v0-{i}"),
                content: original_content.clone(),
                elo_rating: config.initial_elo,
                generation_born: 0,
                parent_id: None,
                wins: 0,
                losses: 0,
                draws: 0,
            });
        }

        let state = EvolutionState {
            id: id.clone(),
            config,
            status: EvolutionStatus::Created,
            generation: 0,
            population,
            history: Vec::new(),
            created_at: Utc::now(),
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        self.runs
            .write()
            .await
            .insert(id.clone(), Arc::new(Mutex::new(state)));

        info!(evolution_id = %id, evolution_name = %name, "Evolution run created");

        Ok(id)
    }

    /// Start the evolution loop as a background task.
    pub async fn start(&self, evolution_id: &str) -> Result<(), EvolutionError> {
        let runs = self.runs.read().await;
        let state = runs
            .get(evolution_id)
            .ok_or_else(|| EvolutionError::NotFound(evolution_id.to_string()))?
            .clone();

        {
            let mut s = state.lock().await;
            match s.status {
                EvolutionStatus::Created | EvolutionStatus::Paused => {
                    s.status = EvolutionStatus::Running;
                }
                EvolutionStatus::Running => {
                    return Err(EvolutionError::AlreadyRunning(evolution_id.to_string()));
                }
                EvolutionStatus::Completed | EvolutionStatus::Failed => {
                    return Err(EvolutionError::InvalidState(
                        "Cannot start a completed/failed evolution".to_string(),
                    ));
                }
            }
        }

        let base_dir = self.base_dir.clone();
        let llm_url = self.llm_base_url.clone();

        tokio::spawn(async move {
            if let Err(e) = run_evolution_loop(state, base_dir, llm_url).await {
                error!(error = %e, "Evolution loop failed");
            }
        });

        Ok(())
    }

    /// Get current status.
    pub async fn status(
        &self,
        evolution_id: &str,
    ) -> Result<EvolutionStatusReport, EvolutionError> {
        let runs = self.runs.read().await;
        let state = runs
            .get(evolution_id)
            .ok_or_else(|| EvolutionError::NotFound(evolution_id.to_string()))?;

        let s = state.lock().await;
        let best = s
            .population
            .iter()
            .max_by(|a, b| a.elo_rating.partial_cmp(&b.elo_rating).unwrap())
            .map(|v| (v.id.clone(), v.elo_rating))
            .unwrap_or_default();

        let population: Vec<_> = s
            .population
            .iter()
            .map(|v| VariantSummary {
                id: v.id.clone(),
                elo_rating: v.elo_rating,
                wins: v.wins,
                losses: v.losses,
                draws: v.draws,
                content_preview: truncate(&v.content, 200),
            })
            .collect();

        Ok(EvolutionStatusReport {
            id: s.id.clone(),
            name: s.config.name.clone(),
            status: s.status,
            generation: s.generation,
            population,
            best_variant_id: best.0,
            best_elo: best.1,
            created_at: s.created_at,
        })
    }

    /// Stop a running evolution.
    pub async fn stop(&self, evolution_id: &str) -> Result<EvolutionStatusReport, EvolutionError> {
        let runs = self.runs.read().await;
        let state = runs
            .get(evolution_id)
            .ok_or_else(|| EvolutionError::NotFound(evolution_id.to_string()))?;

        let s = state.lock().await;
        if s.status != EvolutionStatus::Running {
            return Err(EvolutionError::InvalidState(format!(
                "Evolution is {:?}, not running",
                s.status
            )));
        }
        s.cancel.cancel();

        let best = s
            .population
            .iter()
            .max_by(|a, b| a.elo_rating.partial_cmp(&b.elo_rating).unwrap())
            .map(|v| (v.id.clone(), v.elo_rating))
            .unwrap_or_default();

        let population: Vec<_> = s
            .population
            .iter()
            .map(|v| VariantSummary {
                id: v.id.clone(),
                elo_rating: v.elo_rating,
                wins: v.wins,
                losses: v.losses,
                draws: v.draws,
                content_preview: truncate(&v.content, 200),
            })
            .collect();

        Ok(EvolutionStatusReport {
            id: s.id.clone(),
            name: s.config.name.clone(),
            status: EvolutionStatus::Completed,
            generation: s.generation,
            population,
            best_variant_id: best.0,
            best_elo: best.1,
            created_at: s.created_at,
        })
    }

    /// Promote the best variant: replace the target brain file with it.
    pub async fn promote(&self, evolution_id: &str) -> Result<PromoteResult, EvolutionError> {
        let runs = self.runs.read().await;
        let state = runs
            .get(evolution_id)
            .ok_or_else(|| EvolutionError::NotFound(evolution_id.to_string()))?;

        let s = state.lock().await;
        let best = s
            .population
            .iter()
            .max_by(|a, b| a.elo_rating.partial_cmp(&b.elo_rating).unwrap())
            .ok_or_else(|| EvolutionError::Population("Empty population".to_string()))?;

        let target_path = self.base_dir.join(&s.config.target_file);

        // Backup current file
        let backup_path = self
            .base_dir
            .join("evolution")
            .join(&s.config.name)
            .join(format!("backup_gen{}.txt", s.generation));

        if target_path.exists() {
            let current = tokio::fs::read_to_string(&target_path)
                .await
                .unwrap_or_default();
            tokio::fs::write(&backup_path, &current)
                .await
                .map_err(|e| EvolutionError::Io(format!("Backup failed: {e}")))?;
        }

        // Write best variant to target
        tokio::fs::write(&target_path, &best.content)
            .await
            .map_err(|e| EvolutionError::Io(format!("Promote write failed: {e}")))?;

        info!(
            evolution = %s.config.name,
            variant = %best.id,
            elo = best.elo_rating,
            "Promoted best variant to target file"
        );

        Ok(PromoteResult {
            variant_id: best.id.clone(),
            elo_rating: best.elo_rating,
            target_file: s.config.target_file.clone(),
            backup_path: backup_path.to_string_lossy().to_string(),
        })
    }

    /// Get full evolution history.
    pub async fn history(
        &self,
        evolution_id: &str,
    ) -> Result<Vec<GenerationRecord>, EvolutionError> {
        let runs = self.runs.read().await;
        let state = runs
            .get(evolution_id)
            .ok_or_else(|| EvolutionError::NotFound(evolution_id.to_string()))?;

        let s = state.lock().await;
        Ok(s.history.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromoteResult {
    pub variant_id: String,
    pub elo_rating: f64,
    pub target_file: String,
    pub backup_path: String,
}

// ---------------------------------------------------------------------------
// Evolution loop (background tokio task)
// ---------------------------------------------------------------------------

async fn run_evolution_loop(
    state: Arc<Mutex<EvolutionState>>,
    base_dir: PathBuf,
    llm_url: String,
) -> Result<(), EvolutionError> {
    let http = reqwest::Client::new();

    loop {
        let (cancel, generation, max_generations, config) = {
            let s = state.lock().await;
            if s.status != EvolutionStatus::Running {
                break;
            }
            (
                s.cancel.clone(),
                s.generation,
                s.config.max_generations,
                s.config.clone(),
            )
        };

        // Check max generations
        if let Some(max) = max_generations {
            if generation >= max {
                let mut s = state.lock().await;
                s.status = EvolutionStatus::Completed;
                info!(evolution = %config.name, gen = generation, "Evolution completed (max generations)");
                break;
            }
        }

        if cancel.is_cancelled() {
            let mut s = state.lock().await;
            s.status = EvolutionStatus::Completed;
            info!(evolution = %config.name, "Evolution stopped by user");
            break;
        }

        let next_gen = generation + 1;
        info!(evolution = %config.name, generation = next_gen, "Starting generation");

        // Step 1: Mutate — generate variants from each population member
        let current_pop = {
            let s = state.lock().await;
            s.population.clone()
        };

        let mut new_variants = Vec::new();
        for variant in &current_pop {
            match mutate_variant(&http, &llm_url, variant, &config, next_gen).await {
                Ok(mutated) => new_variants.push(mutated),
                Err(e) => {
                    warn!(
                        variant = %variant.id,
                        error = %e,
                        "Mutation failed, keeping original"
                    );
                    // Keep the original if mutation fails
                    new_variants.push(variant.clone());
                }
            }
        }

        // Combine original and mutated into full candidate pool
        let mut candidates: Vec<Variant> = current_pop.clone();
        candidates.extend(new_variants);

        // Step 2+3: Tournament — all pairs compete on shared prompts
        let mut matches = Vec::new();
        let n = candidates.len();
        for i in 0..n {
            for j in (i + 1)..n {
                // Pick a random test prompt (use generation as seed for determinism)
                let prompt_idx = (i * n + j + next_gen as usize) % config.test_prompts.len();
                let prompt = &config.test_prompts[prompt_idx];

                match run_match(
                    &http,
                    &llm_url,
                    &candidates[i],
                    &candidates[j],
                    prompt,
                    &config,
                )
                .await
                {
                    Ok(result) => {
                        // Update Elo
                        let (new_a, new_b) = update_elo(
                            candidates[i].elo_rating,
                            candidates[j].elo_rating,
                            result.score_a,
                            config.k_factor,
                        );
                        candidates[i].elo_rating = new_a;
                        candidates[j].elo_rating = new_b;

                        // Update win/loss
                        if result.score_a > 0.5 {
                            candidates[i].wins += 1;
                            candidates[j].losses += 1;
                        } else if result.score_a < 0.5 {
                            candidates[i].losses += 1;
                            candidates[j].wins += 1;
                        } else {
                            candidates[i].draws += 1;
                            candidates[j].draws += 1;
                        }

                        matches.push(result);
                    }
                    Err(e) => {
                        warn!(error = %e, "Match failed, skipping pair");
                    }
                }

                // Check cancellation between matches
                if cancel.is_cancelled() {
                    let mut s = state.lock().await;
                    s.status = EvolutionStatus::Completed;
                    return Ok(());
                }
            }
        }

        // Step 4: Select — keep top N by Elo
        candidates.sort_by(|a, b| b.elo_rating.partial_cmp(&a.elo_rating).unwrap());
        candidates.truncate(config.population_size);

        // Step 5: Record generation
        let best = candidates
            .first()
            .map(|v| (v.id.clone(), v.elo_rating))
            .unwrap_or_default();

        let record = GenerationRecord {
            generation: next_gen,
            timestamp: Utc::now(),
            variants: candidates
                .iter()
                .map(|v| VariantSnapshot {
                    id: v.id.clone(),
                    elo_rating: v.elo_rating,
                    content_preview: truncate(&v.content, 200),
                })
                .collect(),
            matches: matches.clone(),
            best_variant_id: best.0.clone(),
            best_elo: best.1,
        };

        // Save generation record to disk
        let evo_dir = base_dir.join("evolution").join(&config.name);
        let gen_path = evo_dir.join(format!("gen_{next_gen}.json"));
        if let Ok(json) = serde_json::to_string_pretty(&record) {
            let _ = tokio::fs::write(&gen_path, json).await;
        }

        // Update state
        {
            let mut s = state.lock().await;
            s.generation = next_gen;
            s.population = candidates;
            s.history.push(record);

            info!(
                evolution = %config.name,
                generation = next_gen,
                best_variant = %best.0,
                best_elo = best.1,
                "Generation complete"
            );
        }

        // Delay between generations
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {},
            _ = cancel.cancelled() => {
                let mut s = state.lock().await;
                s.status = EvolutionStatus::Completed;
                break;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// LLM operations
// ---------------------------------------------------------------------------

/// Mutate a variant: ask the LLM to produce an improved version.
async fn mutate_variant(
    http: &reqwest::Client,
    llm_url: &str,
    variant: &Variant,
    config: &EvolutionConfig,
    generation: u32,
) -> Result<Variant, EvolutionError> {
    let prompt = format!(
        r#"You are a prompt engineer optimizing a brain file for an AI assistant.
The brain file is evaluated on: {metric}

Current variant (generation {gen}, Elo {elo:.0}):
---
{content}
---

Generate an IMPROVED version of this brain file. Make ONE meaningful change:
- Clarify instructions
- Add/remove/reorder sections
- Adjust tone or emphasis
- Fix ambiguities

Reply with ONLY the complete new brain file content, nothing else."#,
        metric = config.eval_metric,
        gen = generation,
        elo = variant.elo_rating,
        content = variant.content,
    );

    let new_content = llm_complete(http, llm_url, &prompt).await?;

    Ok(Variant {
        id: format!("v{generation}-{}", &Uuid::new_v4().to_string()[..8]),
        content: new_content,
        elo_rating: config.initial_elo,
        generation_born: generation,
        parent_id: Some(variant.id.clone()),
        wins: 0,
        losses: 0,
        draws: 0,
    })
}

/// Run a head-to-head match between two variants on a given prompt.
async fn run_match(
    http: &reqwest::Client,
    llm_url: &str,
    variant_a: &Variant,
    variant_b: &Variant,
    test_prompt: &str,
    config: &EvolutionConfig,
) -> Result<MatchResult, EvolutionError> {
    // Step 1: Generate responses using each variant as system prompt
    let response_a = llm_chat(http, llm_url, &variant_a.content, test_prompt).await?;
    let response_b = llm_chat(http, llm_url, &variant_b.content, test_prompt).await?;

    // Step 2: Judge which response is better
    let judge_prompt = format!(
        r#"You are a fair judge evaluating two AI assistant responses.
Evaluation criteria: {metric}

User prompt: {test_prompt}

Response A:
{response_a}

Response B:
{response_b}

Which response is better? Reply with ONLY JSON:
{{"winner": "A" or "B" or "tie", "reasoning": "brief explanation"}}"#,
        metric = config.eval_metric,
        test_prompt = test_prompt,
        response_a = response_a,
        response_b = response_b,
    );

    let judge_result = llm_complete(http, llm_url, &judge_prompt).await?;

    // Parse judge result
    let cleaned = judge_result
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let parsed: serde_json::Value = serde_json::from_str(cleaned)
        .unwrap_or(serde_json::json!({"winner": "tie", "reasoning": "parse error"}));

    let winner = parsed["winner"].as_str().unwrap_or("tie");
    let reasoning = parsed["reasoning"]
        .as_str()
        .unwrap_or("no reasoning")
        .to_string();

    let score_a = match winner.to_uppercase().as_str() {
        "A" => 1.0,
        "B" => 0.0,
        _ => 0.5,
    };

    Ok(MatchResult {
        prompt: test_prompt.to_string(),
        variant_a_id: variant_a.id.clone(),
        variant_b_id: variant_b.id.clone(),
        score_a,
        generation: 0, // Filled by caller
        timestamp: Utc::now(),
        reasoning,
    })
}

/// Simple LLM completion (system+user message → response text).
async fn llm_chat(
    http: &reqwest::Client,
    llm_url: &str,
    system_prompt: &str,
    user_message: &str,
) -> Result<String, EvolutionError> {
    let body = serde_json::json!({
        "model": "default",
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_message}
        ],
        "temperature": 0.7,
        "max_tokens": 1024,
    });

    let response = http
        .post(format!("{llm_url}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .map_err(|e| EvolutionError::Llm(format!("HTTP request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(EvolutionError::Llm(format!(
            "LLM returned {status}: {text}"
        )));
    }

    let json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| EvolutionError::Llm(format!("Parse failed: {e}")))?;

    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| EvolutionError::Llm("No content in response".to_string()))
}

/// Simple completion (user message only → response text).
async fn llm_complete(
    http: &reqwest::Client,
    llm_url: &str,
    prompt: &str,
) -> Result<String, EvolutionError> {
    llm_chat(
        http,
        llm_url,
        "You are a helpful assistant. Follow instructions precisely.",
        prompt,
    )
    .await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expected_score_equal() {
        let e = expected_score(1500.0, 1500.0);
        assert!((e - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_expected_score_higher_rated() {
        let e = expected_score(1700.0, 1500.0);
        assert!(e > 0.7);
        assert!(e < 0.8);
    }

    #[test]
    fn test_expected_score_lower_rated() {
        let e = expected_score(1300.0, 1500.0);
        assert!(e > 0.2);
        assert!(e < 0.3);
    }

    #[test]
    fn test_update_elo_win() {
        let (new_a, new_b) = update_elo(1500.0, 1500.0, 1.0, 32.0);
        assert!(new_a > 1500.0);
        assert!(new_b < 1500.0);
        // Total Elo is conserved
        assert!((new_a + new_b - 3000.0).abs() < 0.001);
    }

    #[test]
    fn test_update_elo_loss() {
        let (new_a, new_b) = update_elo(1500.0, 1500.0, 0.0, 32.0);
        assert!(new_a < 1500.0);
        assert!(new_b > 1500.0);
    }

    #[test]
    fn test_update_elo_draw() {
        let (new_a, new_b) = update_elo(1500.0, 1500.0, 0.5, 32.0);
        assert!((new_a - 1500.0).abs() < 0.001);
        assert!((new_b - 1500.0).abs() < 0.001);
    }

    #[test]
    fn test_update_elo_upset() {
        // Lower-rated player wins — bigger Elo shift
        let (new_a, _new_b) = update_elo(1300.0, 1700.0, 1.0, 32.0);
        let gain_a = new_a - 1300.0;
        assert!(gain_a > 20.0, "Upset should yield large gain: {gain_a}");
    }

    #[test]
    fn test_evolution_config_serde() {
        let json = serde_json::json!({
            "name": "brain_v2_evolution",
            "target_file": "system.md",
            "population_size": 4,
            "test_prompts": ["What is Rust?", "Explain async/await"],
            "eval_metric": "helpfulness",
            "max_generations": 20,
            "k_factor": 32.0,
            "initial_elo": 1500.0
        });

        let config: EvolutionConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.name, "brain_v2_evolution");
        assert_eq!(config.population_size, 4);
        assert_eq!(config.test_prompts.len(), 2);
        assert_eq!(config.max_generations, Some(20));
    }

    #[test]
    fn test_evolution_config_defaults() {
        let json = serde_json::json!({
            "name": "test",
            "target_file": "system.md",
            "test_prompts": ["hello"]
        });

        let config: EvolutionConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.population_size, 4);
        assert_eq!(config.eval_metric, "overall_quality");
        assert_eq!(config.k_factor, 32.0);
        assert_eq!(config.initial_elo, 1500.0);
        assert!(config.max_generations.is_none());
    }

    #[test]
    fn test_variant_serde() {
        let v = Variant {
            id: "v1-abc".to_string(),
            content: "You are a helpful AI.".to_string(),
            elo_rating: 1532.5,
            generation_born: 3,
            parent_id: Some("v2-def".to_string()),
            wins: 5,
            losses: 2,
            draws: 1,
        };

        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["id"], "v1-abc");
        assert_eq!(json["elo_rating"], 1532.5);
        assert_eq!(json["wins"], 5);

        let restored: Variant = serde_json::from_value(json).unwrap();
        assert_eq!(restored.id, v.id);
    }

    #[test]
    fn test_match_result_serde() {
        let m = MatchResult {
            prompt: "What is Rust?".to_string(),
            variant_a_id: "v1".to_string(),
            variant_b_id: "v2".to_string(),
            score_a: 1.0,
            generation: 5,
            timestamp: Utc::now(),
            reasoning: "A was more detailed".to_string(),
        };

        let json = serde_json::to_string(&m).unwrap();
        let restored: MatchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.score_a, 1.0);
        assert_eq!(restored.variant_a_id, "v1");
    }

    #[test]
    fn test_evolution_status_serde() {
        let s: EvolutionStatus = serde_json::from_str("\"running\"").unwrap();
        assert_eq!(s, EvolutionStatus::Running);

        let json = serde_json::to_string(&EvolutionStatus::Completed).unwrap();
        assert_eq!(json, "\"completed\"");
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello...");
        assert_eq!(truncate("", 5), "");
    }

    #[test]
    fn test_generation_record_serde() {
        let record = GenerationRecord {
            generation: 1,
            timestamp: Utc::now(),
            variants: vec![VariantSnapshot {
                id: "v1-abc".to_string(),
                elo_rating: 1516.0,
                content_preview: "You are helpful...".to_string(),
            }],
            matches: vec![],
            best_variant_id: "v1-abc".to_string(),
            best_elo: 1516.0,
        };

        let json = serde_json::to_string(&record).unwrap();
        let restored: GenerationRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.generation, 1);
        assert_eq!(restored.best_elo, 1516.0);
    }

    #[test]
    fn test_promote_result_serde() {
        let pr = PromoteResult {
            variant_id: "v3-abc".to_string(),
            elo_rating: 1600.0,
            target_file: "system.md".to_string(),
            backup_path: "/tmp/backup.txt".to_string(),
        };

        let json = serde_json::to_value(&pr).unwrap();
        assert_eq!(json["variant_id"], "v3-abc");
        assert_eq!(json["elo_rating"], 1600.0);
    }

    #[test]
    fn test_variant_summary_serde() {
        let vs = VariantSummary {
            id: "v1".to_string(),
            elo_rating: 1500.0,
            wins: 3,
            losses: 1,
            draws: 0,
            content_preview: "preview...".to_string(),
        };

        let json = serde_json::to_value(&vs).unwrap();
        assert_eq!(json["wins"], 3);
    }

    #[test]
    fn test_evolution_error_display() {
        let e = EvolutionError::NotFound("xyz".to_string());
        assert!(e.to_string().contains("xyz"));

        let e = EvolutionError::AlreadyRunning("abc".to_string());
        assert!(e.to_string().contains("abc"));
    }

    #[tokio::test]
    async fn test_registry_create() {
        let dir = tempfile::tempdir().unwrap();

        // Create a target brain file
        let target = dir.path().join("system.md");
        tokio::fs::write(&target, "You are a helpful assistant.")
            .await
            .unwrap();

        let registry = EvolutionRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = EvolutionConfig {
            name: "test_evo".to_string(),
            target_file: "system.md".to_string(),
            population_size: 3,
            test_prompts: vec!["Hello".to_string()],
            eval_metric: "helpfulness".to_string(),
            max_generations: Some(5),
            k_factor: 32.0,
            initial_elo: 1500.0,
        };

        let evo_id = registry.create(config).await.unwrap();
        assert!(!evo_id.is_empty());

        let status = registry.status(&evo_id).await.unwrap();
        assert_eq!(status.status, EvolutionStatus::Created);
        assert_eq!(status.generation, 0);
        assert_eq!(status.population.len(), 3);
    }

    #[tokio::test]
    async fn test_registry_create_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let registry = EvolutionRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = EvolutionConfig {
            name: "test".to_string(),
            target_file: "nonexistent.md".to_string(),
            population_size: 3,
            test_prompts: vec!["Hello".to_string()],
            eval_metric: "helpfulness".to_string(),
            max_generations: Some(5),
            k_factor: 32.0,
            initial_elo: 1500.0,
        };

        let result = registry.create(config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), EvolutionError::Io(_)));
    }

    #[tokio::test]
    async fn test_registry_create_no_prompts() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.md"), "content")
            .await
            .unwrap();

        let registry = EvolutionRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = EvolutionConfig {
            name: "test".to_string(),
            target_file: "test.md".to_string(),
            population_size: 3,
            test_prompts: vec![],
            eval_metric: "helpfulness".to_string(),
            max_generations: Some(5),
            k_factor: 32.0,
            initial_elo: 1500.0,
        };

        let result = registry.create(config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), EvolutionError::Population(_)));
    }

    #[tokio::test]
    async fn test_registry_create_small_population() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.md"), "content")
            .await
            .unwrap();

        let registry = EvolutionRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = EvolutionConfig {
            name: "test".to_string(),
            target_file: "test.md".to_string(),
            population_size: 1,
            test_prompts: vec!["Hello".to_string()],
            eval_metric: "helpfulness".to_string(),
            max_generations: Some(5),
            k_factor: 32.0,
            initial_elo: 1500.0,
        };

        let result = registry.create(config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), EvolutionError::Population(_)));
    }

    #[tokio::test]
    async fn test_registry_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let registry = EvolutionRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let result = registry.status("nonexistent").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), EvolutionError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_registry_stop_not_running() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.md"), "content")
            .await
            .unwrap();

        let registry = EvolutionRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = EvolutionConfig {
            name: "stop_test".to_string(),
            target_file: "test.md".to_string(),
            population_size: 2,
            test_prompts: vec!["Hello".to_string()],
            eval_metric: "helpfulness".to_string(),
            max_generations: Some(5),
            k_factor: 32.0,
            initial_elo: 1500.0,
        };

        let evo_id = registry.create(config).await.unwrap();
        let result = registry.stop(&evo_id).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            EvolutionError::InvalidState(_)
        ));
    }

    #[tokio::test]
    async fn test_registry_double_start() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.md"), "content")
            .await
            .unwrap();

        let registry = EvolutionRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = EvolutionConfig {
            name: "double_start".to_string(),
            target_file: "test.md".to_string(),
            population_size: 2,
            test_prompts: vec!["Hello".to_string()],
            eval_metric: "helpfulness".to_string(),
            max_generations: Some(5),
            k_factor: 32.0,
            initial_elo: 1500.0,
        };

        let evo_id = registry.create(config).await.unwrap();
        registry.start(&evo_id).await.unwrap();

        let result = registry.start(&evo_id).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            EvolutionError::AlreadyRunning(_)
        ));
    }

    #[tokio::test]
    async fn test_promote() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("system.md");
        tokio::fs::write(&target, "Original content").await.unwrap();

        let registry = EvolutionRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = EvolutionConfig {
            name: "promote_test".to_string(),
            target_file: "system.md".to_string(),
            population_size: 2,
            test_prompts: vec!["Hello".to_string()],
            eval_metric: "helpfulness".to_string(),
            max_generations: Some(5),
            k_factor: 32.0,
            initial_elo: 1500.0,
        };

        let evo_id = registry.create(config).await.unwrap();

        // Manually boost one variant's Elo to simulate evolution
        {
            let runs = registry.runs.read().await;
            let state = runs.get(&evo_id).unwrap();
            let mut s = state.lock().await;
            s.population[0].content = "Improved content".to_string();
            s.population[0].elo_rating = 1600.0;
        }

        let result = registry.promote(&evo_id).await.unwrap();
        assert_eq!(result.elo_rating, 1600.0);
        assert_eq!(result.target_file, "system.md");

        // Verify the file was updated
        let content = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(content, "Improved content");

        // Verify backup was created
        let backup = dir.path().join("evolution/promote_test/backup_gen0.txt");
        let backup_content = tokio::fs::read_to_string(&backup).await.unwrap();
        assert_eq!(backup_content, "Original content");
    }

    #[tokio::test]
    async fn test_history_empty() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.md"), "content")
            .await
            .unwrap();

        let registry = EvolutionRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = EvolutionConfig {
            name: "history_test".to_string(),
            target_file: "test.md".to_string(),
            population_size: 2,
            test_prompts: vec!["Hello".to_string()],
            eval_metric: "helpfulness".to_string(),
            max_generations: Some(5),
            k_factor: 32.0,
            initial_elo: 1500.0,
        };

        let evo_id = registry.create(config).await.unwrap();
        let history = registry.history(&evo_id).await.unwrap();
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn test_status_report_best_variant() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.md"), "content")
            .await
            .unwrap();

        let registry = EvolutionRegistry::new(
            dir.path().to_path_buf(),
            "http://localhost:8080".to_string(),
        );

        let config = EvolutionConfig {
            name: "best_test".to_string(),
            target_file: "test.md".to_string(),
            population_size: 3,
            test_prompts: vec!["Hello".to_string()],
            eval_metric: "helpfulness".to_string(),
            max_generations: Some(5),
            k_factor: 32.0,
            initial_elo: 1500.0,
        };

        let evo_id = registry.create(config).await.unwrap();

        // Boost variant 1 Elo
        {
            let runs = registry.runs.read().await;
            let state = runs.get(&evo_id).unwrap();
            let mut s = state.lock().await;
            s.population[1].elo_rating = 1600.0;
        }

        let status = registry.status(&evo_id).await.unwrap();
        assert_eq!(status.best_elo, 1600.0);
        assert_eq!(status.best_variant_id, "v0-1");
    }

    #[test]
    fn test_status_report_serde() {
        let report = EvolutionStatusReport {
            id: "test".to_string(),
            name: "my_evo".to_string(),
            status: EvolutionStatus::Running,
            generation: 5,
            population: vec![],
            best_variant_id: "v3-abc".to_string(),
            best_elo: 1580.0,
            created_at: Utc::now(),
        };

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["status"], "running");
        assert_eq!(json["generation"], 5);
        assert_eq!(json["best_elo"], 1580.0);
    }
}
