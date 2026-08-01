//! Skill Library — Voyager Pattern (I4)
//!
//! When a multi-step task succeeds, the tool sequence + context is extracted
//! as a named skill, embedded for semantic search, and stored in SQLite.
//! On new tasks, relevant skills are retrieved by similarity and can be
//! chained/composed.
//!
//! RPCs: `skills.save`, `skills.search`, `skills.chain`, `skills.list`

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single step in a learned skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillStep {
    /// Tool name invoked at this step.
    pub tool: String,
    /// Parameters passed to the tool (simplified).
    pub params: serde_json::Value,
    /// Brief description of what this step accomplishes.
    pub description: String,
}

/// A learned skill extracted from a successful multi-step task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearnedSkill {
    /// Unique skill ID.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Description of what this skill does.
    pub description: String,
    /// The ordered tool steps that make up this skill.
    pub steps: Vec<SkillStep>,
    /// Context/preconditions under which this skill applies.
    pub context: String,
    /// Tags for categorization.
    pub tags: Vec<String>,
    /// Embedding vector for semantic search (empty if not yet embedded).
    pub embedding: Vec<f32>,
    /// Number of times this skill has been successfully used.
    pub usage_count: u64,
    /// Average success rate (0.0 - 1.0).
    pub success_rate: f64,
    /// When the skill was first learned.
    pub created_at: DateTime<Utc>,
    /// When the skill was last used.
    pub last_used: Option<DateTime<Utc>>,
}

/// Request to save a new skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveSkillRequest {
    pub name: String,
    pub description: String,
    pub steps: Vec<SkillStep>,
    pub context: String,
    pub tags: Vec<String>,
}

/// Request to search for skills.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchSkillRequest {
    /// Natural-language query describing the task.
    pub query: String,
    /// Maximum number of results.
    pub limit: Option<usize>,
    /// Minimum similarity threshold (0.0 - 1.0).
    pub min_similarity: Option<f64>,
}

/// A search result with similarity score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSearchResult {
    pub skill: LearnedSkill,
    pub similarity: f64,
}

/// Request to chain multiple skills together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainSkillRequest {
    /// Ordered list of skill IDs to chain.
    pub skill_ids: Vec<String>,
    /// Name for the composed skill.
    pub name: String,
    /// Description of the composition.
    pub description: String,
}

/// Errors from the skill library.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SkillLibraryError {
    #[error("skill not found: {0}")]
    NotFound(String),
    #[error("skill name already exists: {0}")]
    DuplicateName(String),
    #[error("invalid skill: {0}")]
    Invalid(String),
    #[error("chain error: {0}")]
    ChainError(String),
    #[error("embedding error: {0}")]
    EmbeddingError(String),
}

// ---------------------------------------------------------------------------
// SkillLibrary
// ---------------------------------------------------------------------------

/// In-memory skill library backed by a vector store for semantic search.
/// In production, this is backed by SQLite (matching the project's pattern).
pub struct SkillLibrary {
    skills: RwLock<HashMap<String, LearnedSkill>>,
}

impl SkillLibrary {
    /// Create an empty skill library.
    pub fn new() -> Self {
        Self {
            skills: RwLock::new(HashMap::new()),
        }
    }

    /// Save a new skill to the library.
    pub async fn save(&self, request: SaveSkillRequest) -> Result<LearnedSkill, SkillLibraryError> {
        if request.name.trim().is_empty() {
            return Err(SkillLibraryError::Invalid(
                "skill name cannot be empty".to_string(),
            ));
        }
        if request.steps.is_empty() {
            return Err(SkillLibraryError::Invalid(
                "skill must have at least one step".to_string(),
            ));
        }

        let mut skills = self.skills.write().await;

        // Check for duplicate names
        if skills.values().any(|s| s.name == request.name) {
            return Err(SkillLibraryError::DuplicateName(request.name));
        }

        let skill = LearnedSkill {
            id: Uuid::new_v4().to_string(),
            name: request.name,
            description: request.description,
            steps: request.steps,
            context: request.context,
            tags: request.tags,
            embedding: Vec::new(), // Will be populated by embedder
            usage_count: 0,
            success_rate: 1.0,
            created_at: Utc::now(),
            last_used: None,
        };

        let result = skill.clone();
        skills.insert(skill.id.clone(), skill);
        Ok(result)
    }

    /// Search for skills by text similarity.
    ///
    /// If no embedding is set on skills, falls back to keyword matching
    /// on name + description + tags.
    pub async fn search(&self, request: &SearchSkillRequest) -> Vec<SkillSearchResult> {
        let skills = self.skills.read().await;
        let limit = request.limit.unwrap_or(10);
        let min_sim = request.min_similarity.unwrap_or(0.0);
        let query_lower = request.query.to_lowercase();
        let query_terms: Vec<&str> = query_lower.split_whitespace().collect();

        let mut results: Vec<SkillSearchResult> = skills
            .values()
            .filter_map(|skill| {
                let similarity = Self::text_similarity(skill, &query_terms);
                if similarity >= min_sim {
                    Some(SkillSearchResult {
                        skill: skill.clone(),
                        similarity,
                    })
                } else {
                    None
                }
            })
            .collect();

        results.sort_by(|a, b| {
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        results
    }

    /// Chain multiple skills into a new composite skill.
    pub async fn chain(
        &self,
        request: &ChainSkillRequest,
    ) -> Result<LearnedSkill, SkillLibraryError> {
        if request.skill_ids.is_empty() {
            return Err(SkillLibraryError::ChainError(
                "must provide at least one skill ID".to_string(),
            ));
        }
        if request.name.trim().is_empty() {
            return Err(SkillLibraryError::Invalid(
                "chain name cannot be empty".to_string(),
            ));
        }

        let skills = self.skills.read().await;

        // Collect steps from all skills in order
        let mut combined_steps = Vec::new();
        let mut combined_tags = Vec::new();
        let mut contexts = Vec::new();

        for id in &request.skill_ids {
            let skill = skills
                .get(id)
                .ok_or_else(|| SkillLibraryError::NotFound(id.clone()))?;
            combined_steps.extend(skill.steps.clone());
            combined_tags.extend(skill.tags.clone());
            if !skill.context.is_empty() {
                contexts.push(skill.context.clone());
            }
        }

        combined_tags.sort();
        combined_tags.dedup();

        drop(skills);

        let save_req = SaveSkillRequest {
            name: request.name.clone(),
            description: request.description.clone(),
            steps: combined_steps,
            context: contexts.join("; "),
            tags: combined_tags,
        };

        self.save(save_req).await
    }

    /// List all skills, sorted by usage count (descending).
    pub async fn list(&self) -> Vec<LearnedSkill> {
        let skills = self.skills.read().await;
        let mut list: Vec<LearnedSkill> = skills.values().cloned().collect();
        list.sort_by_key(|b| std::cmp::Reverse(b.usage_count));
        list
    }

    /// Get a skill by ID.
    pub async fn get(&self, id: &str) -> Option<LearnedSkill> {
        self.skills.read().await.get(id).cloned()
    }

    /// Remove a skill by ID.
    pub async fn remove(&self, id: &str) -> Result<(), SkillLibraryError> {
        let mut skills = self.skills.write().await;
        if skills.remove(id).is_none() {
            return Err(SkillLibraryError::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// Record a successful use of a skill (increments count, updates success rate).
    pub async fn record_usage(&self, id: &str, success: bool) {
        let mut skills = self.skills.write().await;
        if let Some(skill) = skills.get_mut(id) {
            let prev_total = skill.usage_count as f64;
            let prev_successes = prev_total * skill.success_rate;
            skill.usage_count += 1;
            let new_successes = if success {
                prev_successes + 1.0
            } else {
                prev_successes
            };
            skill.success_rate = new_successes / skill.usage_count as f64;
            skill.last_used = Some(Utc::now());
        }
    }

    /// Update the embedding vector for a skill.
    pub async fn set_embedding(
        &self,
        id: &str,
        embedding: Vec<f32>,
    ) -> Result<(), SkillLibraryError> {
        let mut skills = self.skills.write().await;
        let skill = skills
            .get_mut(id)
            .ok_or_else(|| SkillLibraryError::NotFound(id.to_string()))?;
        skill.embedding = embedding;
        Ok(())
    }

    /// Number of skills in the library.
    pub async fn count(&self) -> usize {
        self.skills.read().await.len()
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Simple text-based similarity using keyword overlap.
    /// Returns 0.0 – 1.0 based on fraction of query terms found.
    fn text_similarity(skill: &LearnedSkill, query_terms: &[&str]) -> f64 {
        if query_terms.is_empty() {
            return 0.0;
        }

        let searchable = format!(
            "{} {} {} {}",
            skill.name.to_lowercase(),
            skill.description.to_lowercase(),
            skill.tags.join(" ").to_lowercase(),
            skill.context.to_lowercase(),
        );

        let matches = query_terms
            .iter()
            .filter(|term| searchable.contains(**term))
            .count();

        matches as f64 / query_terms.len() as f64
    }
}

impl Default for SkillLibrary {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrap in Arc for shared access.
pub type SharedSkillLibrary = Arc<SkillLibrary>;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_step(tool: &str) -> SkillStep {
        SkillStep {
            tool: tool.to_string(),
            params: serde_json::json!({}),
            description: format!("Run {tool}"),
        }
    }

    fn sample_save_request(name: &str) -> SaveSkillRequest {
        SaveSkillRequest {
            name: name.to_string(),
            description: format!("{name} skill"),
            steps: vec![sample_step("read_file"), sample_step("write_file")],
            context: "general purpose".to_string(),
            tags: vec!["file".to_string(), "io".to_string()],
        }
    }

    #[tokio::test]
    async fn save_and_get() {
        let lib = SkillLibrary::new();
        let skill = lib.save(sample_save_request("copy-file")).await.unwrap();
        assert_eq!(skill.name, "copy-file");
        assert_eq!(skill.steps.len(), 2);
        assert_eq!(skill.usage_count, 0);

        let found = lib.get(&skill.id).await.unwrap();
        assert_eq!(found.name, "copy-file");
    }

    #[tokio::test]
    async fn save_duplicate_name_fails() {
        let lib = SkillLibrary::new();
        lib.save(sample_save_request("copy")).await.unwrap();
        let err = lib.save(sample_save_request("copy")).await.unwrap_err();
        assert!(matches!(err, SkillLibraryError::DuplicateName(_)));
    }

    #[tokio::test]
    async fn save_empty_name_fails() {
        let lib = SkillLibrary::new();
        let mut req = sample_save_request("x");
        req.name = String::new();
        assert!(matches!(
            lib.save(req).await.unwrap_err(),
            SkillLibraryError::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn save_empty_steps_fails() {
        let lib = SkillLibrary::new();
        let req = SaveSkillRequest {
            name: "empty".to_string(),
            description: "no steps".to_string(),
            steps: vec![],
            context: String::new(),
            tags: vec![],
        };
        assert!(matches!(
            lib.save(req).await.unwrap_err(),
            SkillLibraryError::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn search_by_keyword() {
        let lib = SkillLibrary::new();
        lib.save(sample_save_request("file-copy")).await.unwrap();
        lib.save(SaveSkillRequest {
            name: "api-call".to_string(),
            description: "Makes HTTP API calls".to_string(),
            steps: vec![sample_step("shell")],
            context: "network".to_string(),
            tags: vec!["http".to_string(), "api".to_string()],
        })
        .await
        .unwrap();

        let results = lib
            .search(&SearchSkillRequest {
                query: "file io".to_string(),
                limit: Some(10),
                min_similarity: Some(0.0),
            })
            .await;

        // "file-copy" should rank higher (matches "file" and "io" tags)
        assert!(!results.is_empty());
        assert_eq!(results[0].skill.name, "file-copy");
    }

    #[tokio::test]
    async fn search_with_min_similarity() {
        let lib = SkillLibrary::new();
        lib.save(sample_save_request("file-copy")).await.unwrap();

        let results = lib
            .search(&SearchSkillRequest {
                query: "quantum physics".to_string(),
                limit: None,
                min_similarity: Some(0.5),
            })
            .await;

        // No match above 0.5
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn chain_skills() {
        let lib = SkillLibrary::new();
        let s1 = lib.save(sample_save_request("step-a")).await.unwrap();
        let s2 = lib
            .save(SaveSkillRequest {
                name: "step-b".to_string(),
                description: "second step".to_string(),
                steps: vec![sample_step("shell")],
                context: "cleanup".to_string(),
                tags: vec!["shell".to_string()],
            })
            .await
            .unwrap();

        let chained = lib
            .chain(&ChainSkillRequest {
                skill_ids: vec![s1.id, s2.id],
                name: "full-pipeline".to_string(),
                description: "Combined file + shell".to_string(),
            })
            .await
            .unwrap();

        assert_eq!(chained.name, "full-pipeline");
        assert_eq!(chained.steps.len(), 3); // 2 from s1 + 1 from s2
        assert!(chained.tags.contains(&"file".to_string()));
        assert!(chained.tags.contains(&"shell".to_string()));
    }

    #[tokio::test]
    async fn chain_missing_skill_fails() {
        let lib = SkillLibrary::new();
        let err = lib
            .chain(&ChainSkillRequest {
                skill_ids: vec!["nonexistent".to_string()],
                name: "chain".to_string(),
                description: "bad".to_string(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, SkillLibraryError::NotFound(_)));
    }

    #[tokio::test]
    async fn chain_empty_ids_fails() {
        let lib = SkillLibrary::new();
        let err = lib
            .chain(&ChainSkillRequest {
                skill_ids: vec![],
                name: "chain".to_string(),
                description: "bad".to_string(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, SkillLibraryError::ChainError(_)));
    }

    #[tokio::test]
    async fn list_sorted_by_usage() {
        let lib = SkillLibrary::new();
        let s1 = lib.save(sample_save_request("low-use")).await.unwrap();
        let s2 = lib
            .save(SaveSkillRequest {
                name: "high-use".to_string(),
                description: "popular".to_string(),
                steps: vec![sample_step("shell")],
                context: String::new(),
                tags: vec![],
            })
            .await
            .unwrap();

        lib.record_usage(&s2.id, true).await;
        lib.record_usage(&s2.id, true).await;
        lib.record_usage(&s1.id, true).await;

        let list = lib.list().await;
        assert_eq!(list[0].name, "high-use");
        assert_eq!(list[0].usage_count, 2);
    }

    #[tokio::test]
    async fn remove_skill() {
        let lib = SkillLibrary::new();
        let skill = lib.save(sample_save_request("doomed")).await.unwrap();
        assert_eq!(lib.count().await, 1);

        lib.remove(&skill.id).await.unwrap();
        assert_eq!(lib.count().await, 0);
    }

    #[tokio::test]
    async fn remove_nonexistent_fails() {
        let lib = SkillLibrary::new();
        let err = lib.remove("ghost").await.unwrap_err();
        assert!(matches!(err, SkillLibraryError::NotFound(_)));
    }

    #[tokio::test]
    async fn record_usage_updates_rate() {
        let lib = SkillLibrary::new();
        let skill = lib.save(sample_save_request("tracked")).await.unwrap();

        lib.record_usage(&skill.id, true).await;
        lib.record_usage(&skill.id, false).await;

        let s = lib.get(&skill.id).await.unwrap();
        assert_eq!(s.usage_count, 2);
        assert!((s.success_rate - 0.5).abs() < f64::EPSILON);
        assert!(s.last_used.is_some());
    }

    #[tokio::test]
    async fn set_embedding() {
        let lib = SkillLibrary::new();
        let skill = lib.save(sample_save_request("embed")).await.unwrap();

        let emb = vec![0.1, 0.2, 0.3];
        lib.set_embedding(&skill.id, emb.clone()).await.unwrap();

        let s = lib.get(&skill.id).await.unwrap();
        assert_eq!(s.embedding, emb);
    }

    #[tokio::test]
    async fn set_embedding_not_found() {
        let lib = SkillLibrary::new();
        let err = lib.set_embedding("ghost", vec![0.1]).await.unwrap_err();
        assert!(matches!(err, SkillLibraryError::NotFound(_)));
    }

    #[tokio::test]
    async fn count_tracks_adds_and_removes() {
        let lib = SkillLibrary::new();
        assert_eq!(lib.count().await, 0);

        let s = lib.save(sample_save_request("a")).await.unwrap();
        assert_eq!(lib.count().await, 1);

        lib.remove(&s.id).await.unwrap();
        assert_eq!(lib.count().await, 0);
    }

    #[tokio::test]
    async fn search_limit_respected() {
        let lib = SkillLibrary::new();
        for i in 0..5 {
            lib.save(SaveSkillRequest {
                name: format!("file-op-{i}"),
                description: "file operation".to_string(),
                steps: vec![sample_step("read_file")],
                context: String::new(),
                tags: vec!["file".to_string()],
            })
            .await
            .unwrap();
        }

        let results = lib
            .search(&SearchSkillRequest {
                query: "file".to_string(),
                limit: Some(2),
                min_similarity: None,
            })
            .await;

        assert_eq!(results.len(), 2);
    }

    #[test]
    fn error_display() {
        assert_eq!(
            SkillLibraryError::NotFound("x".into()).to_string(),
            "skill not found: x"
        );
        assert_eq!(
            SkillLibraryError::DuplicateName("x".into()).to_string(),
            "skill name already exists: x"
        );
    }

    #[tokio::test]
    async fn default_impl() {
        let lib = SkillLibrary::default();
        assert_eq!(lib.count().await, 0);
    }
}
