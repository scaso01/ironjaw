use crate::brain::Brain;
use crate::types::NewMemory;
use serde::Deserialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

const SYNC_SOURCE: &str = "chronicle";

/// Configuration for the Chronicle indexer.
#[derive(Debug, Clone)]
pub struct ChronicleConfig {
    pub enabled: bool,
    pub db_path: String,
    pub poll_interval_secs: u64,
}

impl Default for ChronicleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            db_path: String::new(),
            poll_interval_secs: 60,
        }
    }
}

/// A session summary row from cc-chronicle's `summaries` table.
#[derive(Debug, Deserialize)]
struct ChronicleSummary {
    session_id: String,
    title: String,
    summary: String,
    topics: Option<String>,
    key_decisions: Option<String>,
}

/// A pattern row from cc-chronicle's `patterns` table.
#[derive(Debug, Deserialize)]
struct ChroniclePattern {
    session_id: String,
    category: String,
    content: String,
    context: Option<String>,
}

/// Indexes cc-chronicle's SQLite database into the brain.
///
/// Connects READ-ONLY to the chronicle DB, polls for new sessions,
/// and stores summaries/patterns as brain memories.
///
/// Routes all storage through `Brain::remember_full()` to enforce
/// quality gating and novelty deduplication.
pub struct ChronicleIndexer {
    brain: Arc<Brain>,
    config: ChronicleConfig,
}

impl ChronicleIndexer {
    pub fn new(brain: Arc<Brain>, config: ChronicleConfig) -> Self {
        Self { brain, config }
    }

    /// Open a read-only connection to the chronicle SQLite database.
    async fn open_chronicle_db(&self) -> anyhow::Result<SqlitePool> {
        let options =
            SqliteConnectOptions::from_str(&format!("sqlite:{}?mode=ro", self.config.db_path))?
                .read_only(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;

        Ok(pool)
    }

    /// Spawn the background polling task. Returns a handle.
    pub fn spawn(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            info!(
                db_path = %self.config.db_path,
                poll_secs = self.config.poll_interval_secs,
                "Chronicle indexer starting"
            );

            let interval = tokio::time::Duration::from_secs(self.config.poll_interval_secs);

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("Chronicle indexer shutting down");
                        return;
                    }
                    _ = tokio::time::sleep(interval) => {
                        if let Err(e) = self.poll_once().await {
                            warn!(error = %e, "Chronicle poll cycle failed");
                        }
                    }
                }
            }
        })
    }

    /// Single poll cycle: find new sessions and index them.
    async fn poll_once(&self) -> anyhow::Result<()> {
        let chronicle_pool = match self.open_chronicle_db().await {
            Ok(p) => p,
            Err(e) => {
                debug!(error = %e, "Cannot open chronicle DB (may not exist yet)");
                return Ok(());
            }
        };

        let last_synced = self
            .brain
            .store()
            .get_sync_state(SYNC_SOURCE)
            .await?
            .unwrap_or_default();

        // Fetch new summaries since last sync
        let summaries = self
            .fetch_new_summaries(&chronicle_pool, &last_synced)
            .await?;

        if summaries.is_empty() {
            return Ok(());
        }

        let mut newest_session_id = last_synced.clone();
        let mut indexed_count = 0u32;

        for summary in &summaries {
            if let Err(e) = self.index_summary(summary).await {
                error!(
                    session_id = %summary.session_id,
                    error = %e,
                    "Failed to index chronicle summary"
                );
                continue;
            }

            // Fetch and index patterns for this session
            let patterns = self
                .fetch_patterns(&chronicle_pool, &summary.session_id)
                .await?;

            for pattern in &patterns {
                if let Err(e) = self.index_pattern(pattern).await {
                    warn!(
                        session_id = %pattern.session_id,
                        error = %e,
                        "Failed to index chronicle pattern"
                    );
                }
            }

            if summary.session_id > newest_session_id {
                newest_session_id.clone_from(&summary.session_id);
            }
            indexed_count += 1;
        }

        if indexed_count > 0 {
            self.brain
                .store()
                .set_sync_state(SYNC_SOURCE, &newest_session_id)
                .await?;
            info!(
                indexed = indexed_count,
                last_id = %newest_session_id,
                "Chronicle indexer synced"
            );
        }

        Ok(())
    }

    /// Fetch summaries with session_id > last_synced, ordered ascending.
    async fn fetch_new_summaries(
        &self,
        pool: &SqlitePool,
        last_synced: &str,
    ) -> anyhow::Result<Vec<ChronicleSummary>> {
        let rows = sqlx::query_as::<_, ChronicleSummaryRow>(
            "SELECT s.session_id, s.title, s.summary, s.topics, s.key_decisions \
             FROM summaries s \
             WHERE s.session_id > ?1 \
             ORDER BY s.session_id ASC \
             LIMIT 50",
        )
        .bind(last_synced)
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| ChronicleSummary {
                session_id: r.session_id,
                title: r.title,
                summary: r.summary,
                topics: r.topics,
                key_decisions: r.key_decisions,
            })
            .collect())
    }

    /// Fetch patterns for a specific session.
    async fn fetch_patterns(
        &self,
        pool: &SqlitePool,
        session_id: &str,
    ) -> anyhow::Result<Vec<ChroniclePattern>> {
        let rows = sqlx::query_as::<_, ChroniclePatternRow>(
            "SELECT session_id, category, content, context \
             FROM patterns WHERE session_id = ?1",
        )
        .bind(session_id)
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| ChroniclePattern {
                session_id: r.session_id,
                category: r.category,
                content: r.content,
                context: r.context,
            })
            .collect())
    }

    /// Index a single session summary into the brain.
    ///
    /// Routes through `Brain::remember_full()` for quality + novelty gating.
    async fn index_summary(&self, summary: &ChronicleSummary) -> anyhow::Result<()> {
        let content = format!(
            "Session: {}\n{}\n\nTopics: {}",
            summary.title,
            summary.summary,
            summary.topics.as_deref().unwrap_or("[]")
        );

        let topics_tag = summary.topics.as_deref().unwrap_or("");

        let mut new_mem = NewMemory {
            content,
            source: Some(format!("chronicle:{}", summary.session_id)),
            tags: if topics_tag.is_empty() {
                None
            } else {
                Some(topics_tag.to_string())
            },
            embedding: None,
            memory_type: Some("session_summary".to_string()),
            importance: Some(0.65),
            strength: Some(1.0),
            supersedes: None,
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: Some("cc_chronicle".to_string()),
            reflection: None,
            trust_level: None,
            evidence_session_id: None,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: None,
            quality_flag: None,
        };

        match self.brain.remember_full(&mut new_mem).await {
            Ok(_) => {}
            Err(e) => {
                debug!(error = %e, session_id = %summary.session_id, "chronicle: summary rejected by gate");
            }
        }

        // Index key decisions as separate memories
        if let Some(decisions_json) = &summary.key_decisions {
            if let Ok(decisions) = serde_json::from_str::<Vec<String>>(decisions_json) {
                for decision in &decisions {
                    if decision.len() < 10 {
                        continue;
                    }
                    let mut dec_mem = NewMemory {
                        content: decision.clone(),
                        source: Some(format!("chronicle:{}", summary.session_id)),
                        tags: None,
                        embedding: None,
                        memory_type: Some("decision".to_string()),
                        importance: Some(0.8),
                        strength: Some(1.0),
                        supersedes: None,
                        content_hash: None,
                        valid_from: None,
                        valid_to: None,
                        source_type: Some("cc_chronicle".to_string()),
                        reflection: None,
                        trust_level: None,
                        evidence_session_id: None,
                        evidence_message_id: None,
                        scope: None,
                        procedure: None,
                        quality_score: None,
                        quality_flag: None,
                    };
                    match self.brain.remember_full(&mut dec_mem).await {
                        Ok(_) => {}
                        Err(e) => {
                            debug!(error = %e, "chronicle: decision rejected by gate");
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Index a single pattern into the brain.
    ///
    /// Routes through `Brain::remember_full()` for quality + novelty gating.
    async fn index_pattern(&self, pattern: &ChroniclePattern) -> anyhow::Result<()> {
        let content = if let Some(ctx) = &pattern.context {
            format!("{}\n\nContext: {ctx}", pattern.content)
        } else {
            pattern.content.clone()
        };

        let memory_type = match pattern.category.as_str() {
            "decision" => "decision",
            "mistake" => "correction",
            "todo" => "todo",
            _ => "observation",
        };

        let mut new_mem = NewMemory {
            content,
            source: Some(format!("chronicle:{}", pattern.session_id)),
            tags: Some(pattern.category.clone()),
            embedding: None,
            memory_type: Some(memory_type.to_string()),
            importance: Some(memory_type_importance(memory_type)),
            strength: Some(1.0),
            supersedes: None,
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: Some("cc_chronicle".to_string()),
            reflection: None,
            trust_level: None,
            evidence_session_id: None,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: None,
            quality_flag: None,
        };

        match self.brain.remember_full(&mut new_mem).await {
            Ok(_) => {}
            Err(e) => {
                debug!(error = %e, session_id = %pattern.session_id, "chronicle: pattern rejected by gate");
            }
        }

        Ok(())
    }
}

fn memory_type_importance(mt: &str) -> f64 {
    match mt {
        "decision" => 0.8,
        "correction" => 0.75,
        "todo" => 0.6,
        "session_summary" => 0.65,
        _ => 0.5,
    }
}

// ── sqlx row types for query_as ─────────────────────────────

#[derive(sqlx::FromRow)]
struct ChronicleSummaryRow {
    session_id: String,
    title: String,
    summary: String,
    topics: Option<String>,
    key_decisions: Option<String>,
}

#[derive(sqlx::FromRow)]
struct ChroniclePatternRow {
    session_id: String,
    category: String,
    content: String,
    context: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::BrainConfig;
    use crate::embedder::Embedder;
    use crate::store::BrainStore;

    async fn setup_test_brain() -> Arc<Brain> {
        let store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let config = BrainConfig {
            memory_relevance_threshold: 0.0,
            ..Default::default()
        };
        Arc::new(Brain::new(store, embedder, config))
    }

    /// Create a temp chronicle database with test data.
    async fn create_test_chronicle(dir: &std::path::Path) -> String {
        let db_path = dir.join("chronicle_test.db");
        let db_path_str = db_path.to_string_lossy().to_string();

        let options =
            SqliteConnectOptions::from_str(&format!("sqlite:{db_path_str}?mode=rwc")).unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();

        // Create chronicle schema
        sqlx::query(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                project TEXT,
                started_at TEXT,
                ended_at TEXT,
                duration_seconds INTEGER,
                message_count INTEGER
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "CREATE TABLE summaries (
                session_id TEXT,
                title TEXT,
                summary TEXT,
                topics TEXT,
                key_decisions TEXT,
                FOREIGN KEY (session_id) REFERENCES sessions(id)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "CREATE TABLE patterns (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT,
                project TEXT,
                category TEXT,
                content TEXT,
                context TEXT,
                confidence REAL,
                source TEXT,
                FOREIGN KEY (session_id) REFERENCES sessions(id)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Insert test data
        sqlx::query(
            "INSERT INTO sessions (id, project, started_at, ended_at, duration_seconds, message_count) \
             VALUES ('sess-001', 'ironjaw', '2026-03-25T10:00:00', '2026-03-25T11:00:00', 3600, 20)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO summaries (session_id, title, summary, topics, key_decisions) \
             VALUES ('sess-001', 'Implement brain indexers on port 18794', \
                     'Built chronicle and markdown indexers for the brain crate at /data/brain.db with v0.8 quality gates.', \
                     '[\"brain\", \"indexers\"]', \
                     '[\"Use polling every 60 seconds for chronicle sync on port 18794\", \"Debounce file watch events with 500ms delay on /data path\"]')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO patterns (session_id, project, category, content, context, confidence, source) \
             VALUES ('sess-001', 'ironjaw', 'decision', \
                     'Poll chronicle DB every 60 seconds on port 18794 instead of using triggers', \
                     'Triggers would require write access to the chronicle DB at /data/chronicle.db', \
                     0.9, 'llm')",
        )
        .execute(&pool)
        .await
        .unwrap();

        pool.close().await;
        db_path_str
    }

    #[tokio::test]
    async fn test_chronicle_poll_indexes_summaries_and_decisions() {
        let brain = setup_test_brain().await;
        let dir = tempfile::tempdir().unwrap();
        let chronicle_path = create_test_chronicle(dir.path()).await;

        let indexer = ChronicleIndexer::new(
            brain.clone(),
            ChronicleConfig {
                enabled: true,
                db_path: chronicle_path,
                poll_interval_secs: 60,
            },
        );

        indexer.poll_once().await.unwrap();

        // Should have indexed: 1 summary + 2 decisions + 1 pattern = 4 memories
        let count = brain.store().count_memories().await.unwrap();
        assert_eq!(
            count, 4,
            "Expected 4 memories (1 summary + 2 decisions + 1 pattern)"
        );

        // Verify sync state was set
        let sync_id = brain.store().get_sync_state(SYNC_SOURCE).await.unwrap();
        assert_eq!(sync_id, Some("sess-001".to_string()));
    }

    #[tokio::test]
    async fn test_chronicle_dedup_on_second_poll() {
        let brain = setup_test_brain().await;
        let dir = tempfile::tempdir().unwrap();
        let chronicle_path = create_test_chronicle(dir.path()).await;

        let indexer = ChronicleIndexer::new(
            brain.clone(),
            ChronicleConfig {
                enabled: true,
                db_path: chronicle_path,
                poll_interval_secs: 60,
            },
        );

        // Poll twice
        indexer.poll_once().await.unwrap();
        let count1 = brain.store().count_memories().await.unwrap();

        // Reset sync state to force re-processing
        brain.store().set_sync_state(SYNC_SOURCE, "").await.unwrap();
        indexer.poll_once().await.unwrap();
        let count2 = brain.store().count_memories().await.unwrap();

        // Counts should be equal due to dedup
        assert_eq!(count1, count2, "Dedup should prevent duplicate memories");
    }

    #[tokio::test]
    async fn test_chronicle_missing_db_is_nonfatal() {
        let brain = setup_test_brain().await;

        let indexer = ChronicleIndexer::new(
            brain,
            ChronicleConfig {
                enabled: true,
                db_path: "/nonexistent/path/chronicle.db".to_string(),
                poll_interval_secs: 60,
            },
        );

        // Should not error — just skip
        let result = indexer.poll_once().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_chronicle_incremental_sync() {
        let brain = setup_test_brain().await;
        let dir = tempfile::tempdir().unwrap();
        let chronicle_path = create_test_chronicle(dir.path()).await;

        let indexer = ChronicleIndexer::new(
            brain.clone(),
            ChronicleConfig {
                enabled: true,
                db_path: chronicle_path.clone(),
                poll_interval_secs: 60,
            },
        );

        // First poll
        indexer.poll_once().await.unwrap();
        let count1 = brain.store().count_memories().await.unwrap();

        // Second poll should find nothing new
        indexer.poll_once().await.unwrap();
        let count2 = brain.store().count_memories().await.unwrap();
        assert_eq!(
            count1, count2,
            "No new sessions should mean no new memories"
        );

        // Add a new session to the chronicle
        let options =
            SqliteConnectOptions::from_str(&format!("sqlite:{chronicle_path}?mode=rwc")).unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO sessions VALUES ('sess-002', 'ironjaw', '2026-03-26', '2026-03-26', 1800, 10)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO summaries VALUES ('sess-002', 'Fix brain tests on port 18794', 'Fixed failing brain tests in /crates/ironjaw-brain v0.8 quality gates.', '[\"tests\"]', '[]')",
        )
        .execute(&pool)
        .await
        .unwrap();

        pool.close().await;

        // Third poll should pick up new session
        indexer.poll_once().await.unwrap();
        let count3 = brain.store().count_memories().await.unwrap();
        assert!(count3 > count2, "New session should produce new memories");

        let sync_id = brain.store().get_sync_state(SYNC_SOURCE).await.unwrap();
        assert_eq!(sync_id, Some("sess-002".to_string()));
    }
}
