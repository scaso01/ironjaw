use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use std::path::Path;
use tracing::info;

/// Tracks LLM usage per session and enforces budgets.
#[derive(Clone)]
pub struct UsageTracker {
    pool: SqlitePool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEvent {
    pub id: i64,
    pub session_id: String,
    pub model: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    pub id: i64,
    pub scope: String, // "global", "session:{id}", "model:{name}"
    pub max_tokens: i64,
    pub period_secs: Option<i64>, // None = lifetime
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    pub total_tokens: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub request_count: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    #[error("budget exceeded: {scope} used {used} of {limit} tokens")]
    BudgetExceeded {
        scope: String,
        used: i64,
        limit: i64,
    },
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS usage_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    model TEXT NOT NULL,
    prompt_tokens INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
);

CREATE TABLE IF NOT EXISTS budgets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL UNIQUE,
    max_tokens INTEGER NOT NULL,
    period_secs INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_usage_session ON usage_events(session_id);
CREATE INDEX IF NOT EXISTS idx_usage_model ON usage_events(model);
CREATE INDEX IF NOT EXISTS idx_usage_created ON usage_events(created_at);
"#;

impl UsageTracker {
    /// Open or create the usage database at the given path.
    pub async fn open(db_path: &str) -> anyhow::Result<Self> {
        if let Some(parent) = Path::new(db_path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let url = format!("sqlite:{db_path}?mode=rwc");
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await?;
        sqlx::query(SCHEMA).execute(&pool).await?;
        info!(path = db_path, "Usage tracker opened");
        Ok(Self { pool })
    }

    /// Open an in-memory database (for testing).
    pub async fn open_memory() -> anyhow::Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        sqlx::query(SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    /// Record a usage event after an LLM call completes.
    pub async fn record(
        &self,
        session_id: &str,
        model: &str,
        prompt_tokens: u32,
        completion_tokens: u32,
    ) -> Result<i64, UsageError> {
        let total = prompt_tokens + completion_tokens;
        let id = sqlx::query_scalar::<_, i64>(
            r#"INSERT INTO usage_events (session_id, model, prompt_tokens, completion_tokens, total_tokens)
               VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id"#,
        )
        .bind(session_id)
        .bind(model)
        .bind(prompt_tokens as i64)
        .bind(completion_tokens as i64)
        .bind(total as i64)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Check if a request would exceed any applicable budget.
    pub async fn check_quota(&self, session_id: &str, model: &str) -> Result<(), UsageError> {
        let budgets = self.get_applicable_budgets(session_id, model).await?;
        for budget in budgets {
            let used = self
                .get_usage_for_scope(&budget.scope, budget.period_secs)
                .await?;
            if used >= budget.max_tokens {
                return Err(UsageError::BudgetExceeded {
                    scope: budget.scope,
                    used,
                    limit: budget.max_tokens,
                });
            }
        }
        Ok(())
    }

    /// Set a budget for a scope (upsert).
    pub async fn set_budget(
        &self,
        scope: &str,
        max_tokens: i64,
        period_secs: Option<i64>,
    ) -> Result<(), UsageError> {
        sqlx::query(
            r#"INSERT INTO budgets (scope, max_tokens, period_secs)
               VALUES (?1, ?2, ?3)
               ON CONFLICT(scope) DO UPDATE SET
                 max_tokens = excluded.max_tokens,
                 period_secs = excluded.period_secs"#,
        )
        .bind(scope)
        .bind(max_tokens)
        .bind(period_secs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove a budget.
    pub async fn remove_budget(&self, scope: &str) -> Result<bool, UsageError> {
        let result = sqlx::query("DELETE FROM budgets WHERE scope = ?1")
            .bind(scope)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Get usage summary for a session.
    pub async fn session_summary(&self, session_id: &str) -> Result<UsageSummary, UsageError> {
        let row = sqlx::query(
            r#"SELECT
                 COALESCE(SUM(total_tokens), 0) as total_tokens,
                 COALESCE(SUM(prompt_tokens), 0) as prompt_tokens,
                 COALESCE(SUM(completion_tokens), 0) as completion_tokens,
                 COUNT(*) as request_count
               FROM usage_events WHERE session_id = ?1"#,
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(UsageSummary {
            total_tokens: row.get("total_tokens"),
            prompt_tokens: row.get("prompt_tokens"),
            completion_tokens: row.get("completion_tokens"),
            request_count: row.get("request_count"),
        })
    }

    /// Get global usage summary (optional time window in seconds).
    pub async fn global_summary(
        &self,
        since_secs: Option<i64>,
    ) -> Result<UsageSummary, UsageError> {
        let row = if let Some(secs) = since_secs {
            sqlx::query(
                r#"SELECT
                     COALESCE(SUM(total_tokens), 0) as total_tokens,
                     COALESCE(SUM(prompt_tokens), 0) as prompt_tokens,
                     COALESCE(SUM(completion_tokens), 0) as completion_tokens,
                     COUNT(*) as request_count
                   FROM usage_events
                   WHERE created_at >= strftime('%Y-%m-%dT%H:%M:%f', 'now', '-' || ?1 || ' seconds')"#,
            )
            .bind(secs)
            .fetch_one(&self.pool)
            .await?
        } else {
            sqlx::query(
                r#"SELECT
                     COALESCE(SUM(total_tokens), 0) as total_tokens,
                     COALESCE(SUM(prompt_tokens), 0) as prompt_tokens,
                     COALESCE(SUM(completion_tokens), 0) as completion_tokens,
                     COUNT(*) as request_count
                   FROM usage_events"#,
            )
            .fetch_one(&self.pool)
            .await?
        };

        Ok(UsageSummary {
            total_tokens: row.get("total_tokens"),
            prompt_tokens: row.get("prompt_tokens"),
            completion_tokens: row.get("completion_tokens"),
            request_count: row.get("request_count"),
        })
    }

    /// List all budgets.
    pub async fn list_budgets(&self) -> Result<Vec<Budget>, UsageError> {
        let rows = sqlx::query(
            "SELECT id, scope, max_tokens, period_secs, created_at FROM budgets ORDER BY scope",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| Budget {
                id: r.get("id"),
                scope: r.get("scope"),
                max_tokens: r.get("max_tokens"),
                period_secs: r.get("period_secs"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    // -- Internal helpers ------------------------------------------------

    async fn get_applicable_budgets(
        &self,
        session_id: &str,
        model: &str,
    ) -> Result<Vec<Budget>, UsageError> {
        let scope_global = "global";
        let scope_session = format!("session:{session_id}");
        let scope_model = format!("model:{model}");

        let rows = sqlx::query(
            "SELECT id, scope, max_tokens, period_secs, created_at \
             FROM budgets WHERE scope IN (?1, ?2, ?3)",
        )
        .bind(scope_global)
        .bind(&scope_session)
        .bind(&scope_model)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| Budget {
                id: r.get("id"),
                scope: r.get("scope"),
                max_tokens: r.get("max_tokens"),
                period_secs: r.get("period_secs"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    async fn get_usage_for_scope(
        &self,
        scope: &str,
        period_secs: Option<i64>,
    ) -> Result<i64, UsageError> {
        // Determine the filter and value based on scope
        let (filter_col, filter_val) = if scope == "global" {
            (None, None)
        } else if let Some(session_id) = scope.strip_prefix("session:") {
            (Some("session_id"), Some(session_id.to_string()))
        } else if let Some(model) = scope.strip_prefix("model:") {
            (Some("model"), Some(model.to_string()))
        } else {
            return Ok(0);
        };

        // Build query based on filter and period
        let total = match (filter_col, period_secs) {
            (None, None) => {
                // Global, no period
                sqlx::query_scalar::<_, i64>(
                    "SELECT COALESCE(SUM(total_tokens), 0) FROM usage_events",
                )
                .fetch_one(&self.pool)
                .await?
            }
            (None, Some(secs)) => {
                // Global with period
                sqlx::query_scalar::<_, i64>(
                    "SELECT COALESCE(SUM(total_tokens), 0) FROM usage_events \
                     WHERE created_at >= strftime('%Y-%m-%dT%H:%M:%f', 'now', '-' || ?1 || ' seconds')",
                )
                .bind(secs)
                .fetch_one(&self.pool)
                .await?
            }
            (Some(col), None) => {
                // Scoped, no period -- use separate queries per column to stay parameterized
                let q = match col {
                    "session_id" => {
                        "SELECT COALESCE(SUM(total_tokens), 0) FROM usage_events WHERE session_id = ?1"
                    }
                    "model" => {
                        "SELECT COALESCE(SUM(total_tokens), 0) FROM usage_events WHERE model = ?1"
                    }
                    _ => return Ok(0),
                };
                sqlx::query_scalar::<_, i64>(q)
                    .bind(filter_val.as_deref().unwrap_or(""))
                    .fetch_one(&self.pool)
                    .await?
            }
            (Some(col), Some(secs)) => {
                // Scoped with period
                let q = match col {
                    "session_id" => {
                        "SELECT COALESCE(SUM(total_tokens), 0) FROM usage_events \
                         WHERE session_id = ?1 \
                         AND created_at >= strftime('%Y-%m-%dT%H:%M:%f', 'now', '-' || ?2 || ' seconds')"
                    }
                    "model" => {
                        "SELECT COALESCE(SUM(total_tokens), 0) FROM usage_events \
                         WHERE model = ?1 \
                         AND created_at >= strftime('%Y-%m-%dT%H:%M:%f', 'now', '-' || ?2 || ' seconds')"
                    }
                    _ => return Ok(0),
                };
                sqlx::query_scalar::<_, i64>(q)
                    .bind(filter_val.as_deref().unwrap_or(""))
                    .bind(secs)
                    .fetch_one(&self.pool)
                    .await?
            }
        };

        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_tracker() -> UsageTracker {
        UsageTracker::open_memory().await.unwrap()
    }

    #[tokio::test]
    async fn test_record_usage() {
        let tracker = test_tracker().await;
        let id = tracker
            .record("sess-1", "qwen3-coder", 100, 50)
            .await
            .unwrap();
        assert_eq!(id, 1);
    }

    #[tokio::test]
    async fn test_session_summary() {
        let tracker = test_tracker().await;
        tracker
            .record("sess-1", "qwen3-coder", 100, 50)
            .await
            .unwrap();
        tracker
            .record("sess-1", "qwen3-coder", 200, 100)
            .await
            .unwrap();
        tracker
            .record("sess-2", "qwen3-coder", 50, 25)
            .await
            .unwrap();

        let summary = tracker.session_summary("sess-1").await.unwrap();
        assert_eq!(summary.total_tokens, 450);
        assert_eq!(summary.prompt_tokens, 300);
        assert_eq!(summary.completion_tokens, 150);
        assert_eq!(summary.request_count, 2);
    }

    #[tokio::test]
    async fn test_global_summary() {
        let tracker = test_tracker().await;
        tracker.record("sess-1", "model-a", 100, 50).await.unwrap();
        tracker.record("sess-2", "model-b", 200, 100).await.unwrap();

        let summary = tracker.global_summary(None).await.unwrap();
        assert_eq!(summary.total_tokens, 450);
        assert_eq!(summary.request_count, 2);
    }

    #[tokio::test]
    async fn test_budget_enforcement() {
        let tracker = test_tracker().await;
        tracker.set_budget("global", 500, None).await.unwrap();

        tracker.record("sess-1", "model-a", 200, 100).await.unwrap();
        // 300 used of 500 -- should pass
        assert!(tracker.check_quota("sess-1", "model-a").await.is_ok());

        tracker.record("sess-1", "model-a", 100, 100).await.unwrap();
        // 500 used of 500 -- should fail
        let result = tracker.check_quota("sess-1", "model-a").await;
        assert!(matches!(result, Err(UsageError::BudgetExceeded { .. })));
    }

    #[tokio::test]
    async fn test_session_budget() {
        let tracker = test_tracker().await;
        tracker
            .set_budget("session:sess-1", 200, None)
            .await
            .unwrap();

        tracker.record("sess-1", "model-a", 100, 50).await.unwrap();
        // 150 used of 200 -- pass
        assert!(tracker.check_quota("sess-1", "model-a").await.is_ok());

        tracker.record("sess-1", "model-a", 30, 20).await.unwrap();
        // 200 used of 200 -- fail
        let result = tracker.check_quota("sess-1", "model-a").await;
        assert!(matches!(result, Err(UsageError::BudgetExceeded { .. })));

        // Different session should pass (no budget for sess-2)
        assert!(tracker.check_quota("sess-2", "model-a").await.is_ok());
    }

    #[tokio::test]
    async fn test_model_budget() {
        let tracker = test_tracker().await;
        tracker
            .set_budget("model:expensive", 100, None)
            .await
            .unwrap();

        tracker.record("sess-1", "expensive", 50, 30).await.unwrap();
        assert!(tracker.check_quota("sess-1", "expensive").await.is_ok());

        tracker.record("sess-2", "expensive", 10, 10).await.unwrap();
        // 100 total across sessions -- fail
        let result = tracker.check_quota("sess-1", "expensive").await;
        assert!(matches!(result, Err(UsageError::BudgetExceeded { .. })));

        // Different model should pass
        assert!(tracker.check_quota("sess-1", "cheap").await.is_ok());
    }

    #[tokio::test]
    async fn test_no_budget_always_passes() {
        let tracker = test_tracker().await;
        tracker
            .record("sess-1", "model-a", 999_999, 999_999)
            .await
            .unwrap();
        assert!(tracker.check_quota("sess-1", "model-a").await.is_ok());
    }

    #[tokio::test]
    async fn test_remove_budget() {
        let tracker = test_tracker().await;
        tracker.set_budget("global", 100, None).await.unwrap();
        let removed = tracker.remove_budget("global").await.unwrap();
        assert!(removed);
        let removed = tracker.remove_budget("global").await.unwrap();
        assert!(!removed);
    }

    #[tokio::test]
    async fn test_list_budgets() {
        let tracker = test_tracker().await;
        tracker.set_budget("global", 1000, None).await.unwrap();
        tracker
            .set_budget("session:s1", 500, Some(3600))
            .await
            .unwrap();

        let budgets = tracker.list_budgets().await.unwrap();
        assert_eq!(budgets.len(), 2);
        assert_eq!(budgets[0].scope, "global");
        assert_eq!(budgets[1].scope, "session:s1");
    }

    #[tokio::test]
    async fn test_budget_upsert() {
        let tracker = test_tracker().await;
        tracker.set_budget("global", 100, None).await.unwrap();
        tracker.set_budget("global", 500, Some(3600)).await.unwrap();

        let budgets = tracker.list_budgets().await.unwrap();
        assert_eq!(budgets.len(), 1);
        assert_eq!(budgets[0].max_tokens, 500);
        assert_eq!(budgets[0].period_secs, Some(3600));
    }
}
