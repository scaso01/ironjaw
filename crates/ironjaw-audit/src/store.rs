use crate::types::{AuditEntry, AuditRecord, ChainVerification};
use ring::digest;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::path::Path;
use tracing::info;

/// Append-only audit log backed by SQLite with Merkle hash-chain integrity.
#[derive(Clone)]
pub struct AuditStore {
    pool: SqlitePool,
}

impl AuditStore {
    /// Open or create the audit database at the given path.
    /// Runs migrations automatically.
    pub async fn open(db_path: &str) -> anyhow::Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = Path::new(db_path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        let url = format!("sqlite:{db_path}?mode=rwc");
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await?;

        Self::run_migrations(&pool).await?;

        info!(path = db_path, "Audit store opened");
        Ok(Self { pool })
    }

    /// Open an in-memory database (for testing)
    pub async fn open_memory() -> anyhow::Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;

        Self::run_migrations(&pool).await?;

        Ok(Self { pool })
    }

    /// Run all migrations in order.
    async fn run_migrations(pool: &SqlitePool) -> anyhow::Result<()> {
        // Migration 1: create base table
        sqlx::query(include_str!("../migrations/001_create_audit_log.sql"))
            .execute(pool)
            .await?;

        // Migration 2: add hash chain columns (idempotent via column check)
        // SQLite ALTER TABLE ADD COLUMN is a no-op if column already exists
        // when using IF NOT EXISTS -- but SQLite doesn't support that syntax.
        // Instead, check if the column exists first.
        let has_hash_columns: bool = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pragma_table_info('audit_log') WHERE name = 'entry_hash'",
        )
        .fetch_one(pool)
        .await?
            > 0;

        if !has_hash_columns {
            // Run each ALTER TABLE separately (SQLite requires one per statement)
            sqlx::query("ALTER TABLE audit_log ADD COLUMN prev_hash TEXT NOT NULL DEFAULT ''")
                .execute(pool)
                .await?;
            sqlx::query("ALTER TABLE audit_log ADD COLUMN entry_hash TEXT NOT NULL DEFAULT ''")
                .execute(pool)
                .await?;
        }

        Ok(())
    }

    /// Compute SHA-256 hash for an audit entry in the chain.
    ///
    /// hash = SHA-256(prev_hash + method + timestamp + params_json)
    fn compute_hash(prev_hash: &str, method: &str, timestamp: &str, params_json: &str) -> String {
        let input = format!("{prev_hash}{method}{timestamp}{params_json}");
        let hash = digest::digest(&digest::SHA256, input.as_bytes());
        hex_encode(hash.as_ref())
    }

    /// Append an entry to the audit log with hash-chain linking.
    pub async fn log(&self, entry: &AuditEntry) -> anyhow::Result<i64> {
        let params_json = entry.params.as_ref().map(|v| v.to_string());
        let result_json = entry.result.as_ref().map(|v| {
            let s = v.to_string();
            // Truncate large results to 10KB
            if s.len() > 10_240 {
                format!("{}...(truncated)", &s[..10_240])
            } else {
                s
            }
        });

        // Get the previous hash for chain linking
        let prev_hash = self.get_chain_head().await?.unwrap_or_default();

        // Compute timestamp for both hashing and storage to ensure consistency.
        let now = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3f")
            .to_string();
        let method_str = entry.method.as_deref().unwrap_or("");
        let params_str = params_json.as_deref().unwrap_or("");
        let entry_hash = Self::compute_hash(&prev_hash, method_str, &now, params_str);

        let row = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO audit_log (
                timestamp, action_type, method, tool_name, params, result, ok, error,
                llm_model, tokens_in, tokens_out, cost_usd, latency_ms,
                session_id, device_id, request_id, prev_hash, entry_hash
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17, ?18
            )
            RETURNING id
            "#,
        )
        .bind(&now)
        .bind(entry.action_type.to_string())
        .bind(&entry.method)
        .bind(&entry.tool_name)
        .bind(&params_json)
        .bind(&result_json)
        .bind(entry.ok)
        .bind(&entry.error)
        .bind(&entry.llm_model)
        .bind(entry.tokens_in)
        .bind(entry.tokens_out)
        .bind(entry.cost_usd)
        .bind(entry.latency_ms)
        .bind(&entry.session_id)
        .bind(&entry.device_id)
        .bind(entry.request_id.to_string())
        .bind(&prev_hash)
        .bind(&entry_hash)
        .fetch_one(&self.pool)
        .await?;

        Ok(row)
    }

    /// Get the hash of the most recent entry in the chain.
    pub async fn get_chain_head(&self) -> Result<Option<String>, anyhow::Error> {
        let result = sqlx::query_scalar::<_, String>(
            "SELECT entry_hash FROM audit_log ORDER BY id DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(result)
    }

    /// Verify the integrity of the entire hash chain.
    ///
    /// Reads all entries in order of insertion, recomputes each hash,
    /// and checks it against the stored hash.
    pub async fn verify_chain(&self) -> Result<ChainVerification, anyhow::Error> {
        let rows = sqlx::query(
            "SELECT id, timestamp, method, params, prev_hash, entry_hash \
             FROM audit_log ORDER BY id ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut entries_checked = 0usize;
        let mut expected_prev_hash = String::new();

        for row in &rows {
            use sqlx::Row;
            let id: i64 = row.get("id");
            let timestamp: String = row.get("timestamp");
            let method: Option<String> = row.get("method");
            let params: Option<String> = row.get("params");
            let stored_prev: String = row.get("prev_hash");
            let stored_hash: String = row.get("entry_hash");

            // Verify prev_hash matches the expected chain
            if stored_prev != expected_prev_hash {
                return Ok(ChainVerification {
                    valid: false,
                    entries_checked,
                    first_broken_at: Some(id),
                });
            }

            // Recompute the hash
            let method_str = method.as_deref().unwrap_or("");
            let params_str = params.as_deref().unwrap_or("");
            let recomputed = Self::compute_hash(&stored_prev, method_str, &timestamp, params_str);

            if recomputed != stored_hash {
                return Ok(ChainVerification {
                    valid: false,
                    entries_checked,
                    first_broken_at: Some(id),
                });
            }

            expected_prev_hash = stored_hash;
            entries_checked += 1;
        }

        Ok(ChainVerification {
            valid: true,
            entries_checked,
            first_broken_at: None,
        })
    }

    /// Query recent audit entries (most recent first)
    pub async fn recent(&self, limit: i64) -> anyhow::Result<Vec<AuditRecord>> {
        let records = sqlx::query(
            "SELECT id, timestamp, action_type, method, tool_name, \
             params, result, ok, error, llm_model, tokens_in, tokens_out, \
             cost_usd, latency_ms, session_id, device_id, request_id, \
             entry_hash, prev_hash \
             FROM audit_log ORDER BY id DESC LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            use sqlx::Row;
            AuditRecord {
                id: row.get("id"),
                timestamp: row.get("timestamp"),
                action_type: row.get("action_type"),
                method: row.get("method"),
                tool_name: row.get("tool_name"),
                params: row.get("params"),
                result: row.get("result"),
                ok: row.get("ok"),
                error: row.get("error"),
                llm_model: row.get("llm_model"),
                tokens_in: row.get("tokens_in"),
                tokens_out: row.get("tokens_out"),
                cost_usd: row.get("cost_usd"),
                latency_ms: row.get("latency_ms"),
                session_id: row.get("session_id"),
                device_id: row.get("device_id"),
                request_id: row.get("request_id"),
                entry_hash: row.get("entry_hash"),
                prev_hash: row.get("prev_hash"),
            }
        })
        .collect();
        Ok(records)
    }

    /// Count total entries in the audit log
    pub async fn count(&self) -> anyhow::Result<i64> {
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }
}

/// Encode bytes as lowercase hex string.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ActionType;
    use uuid::Uuid;

    async fn test_store() -> AuditStore {
        AuditStore::open_memory().await.unwrap()
    }

    fn test_entry() -> AuditEntry {
        AuditEntry {
            action_type: ActionType::Request,
            method: Some("ping".to_string()),
            tool_name: None,
            params: Some(serde_json::json!({})),
            result: Some(serde_json::json!({"pong": true})),
            ok: true,
            error: None,
            llm_model: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            latency_ms: Some(1),
            session_id: None,
            device_id: None,
            request_id: Uuid::new_v4(),
            iteration: None,
            tool_calls: None,
        }
    }

    #[tokio::test]
    async fn test_open_memory() {
        let store = test_store().await;
        let count = store.count().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_log_entry() {
        let store = test_store().await;
        let entry = test_entry();
        let id = store.log(&entry).await.unwrap();
        assert_eq!(id, 1);

        let count = store.count().await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_log_multiple_and_query() {
        let store = test_store().await;

        for i in 0..5 {
            let mut entry = test_entry();
            entry.method = Some(format!("method.{i}"));
            store.log(&entry).await.unwrap();
        }

        let records = store.recent(3).await.unwrap();
        assert_eq!(records.len(), 3);
        // Most recent first
        assert_eq!(records[0].method.as_deref(), Some("method.4"));
        assert_eq!(records[2].method.as_deref(), Some("method.2"));
    }

    #[tokio::test]
    async fn test_log_error_entry() {
        let store = test_store().await;
        let entry = AuditEntry {
            action_type: ActionType::Error,
            method: Some("chat.send".to_string()),
            tool_name: None,
            params: None,
            result: None,
            ok: false,
            error: Some("LLM timeout".to_string()),
            llm_model: Some("llama-server/qwen3-coder".to_string()),
            tokens_in: Some(500),
            tokens_out: None,
            cost_usd: Some(0.0),
            latency_ms: Some(30000),
            session_id: Some("sess-123".to_string()),
            device_id: Some("s22-ultra".to_string()),
            request_id: Uuid::new_v4(),
            iteration: None,
            tool_calls: None,
        };
        let id = store.log(&entry).await.unwrap();
        assert!(id > 0);

        let records = store.recent(1).await.unwrap();
        assert!(!records[0].ok);
        assert_eq!(records[0].error.as_deref(), Some("LLM timeout"));
    }

    #[tokio::test]
    async fn test_auth_action_type() {
        let store = test_store().await;
        let entry = AuditEntry {
            action_type: ActionType::Auth,
            method: Some("auth.connect".to_string()),
            tool_name: None,
            params: Some(serde_json::json!({"device": "s22-ultra"})),
            result: Some(serde_json::json!({"session_id": "new-sess"})),
            ok: true,
            error: None,
            llm_model: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            latency_ms: Some(2),
            session_id: None,
            device_id: Some("s22-ultra".to_string()),
            request_id: Uuid::new_v4(),
            iteration: None,
            tool_calls: None,
        };
        store.log(&entry).await.unwrap();

        let records = store.recent(1).await.unwrap();
        assert_eq!(records[0].action_type, "auth");
    }

    // ---- Hash chain tests ----

    #[tokio::test]
    async fn test_chain_head_empty() {
        let store = test_store().await;
        let head = store.get_chain_head().await.unwrap();
        assert!(head.is_none());
    }

    #[tokio::test]
    async fn test_chain_head_after_insert() {
        let store = test_store().await;
        store.log(&test_entry()).await.unwrap();

        let head = store.get_chain_head().await.unwrap();
        assert!(head.is_some());
        // SHA-256 hex is 64 chars
        assert_eq!(head.unwrap().len(), 64);
    }

    #[tokio::test]
    async fn test_hash_chain_links() {
        let store = test_store().await;

        // Insert 3 entries
        for i in 0..3 {
            let mut entry = test_entry();
            entry.method = Some(format!("method.{i}"));
            store.log(&entry).await.unwrap();
        }

        // Fetch all records and verify prev_hash chain
        let records = store.recent(10).await.unwrap();
        assert_eq!(records.len(), 3);

        // Records are DESC order, reverse for chain check
        let mut ordered: Vec<_> = records.iter().collect();
        ordered.sort_by_key(|r| r.id);

        // First entry should have empty prev_hash
        assert_eq!(ordered[0].prev_hash.as_deref(), Some(""));

        // Each subsequent entry's prev_hash should match prior entry's hash
        assert_eq!(ordered[1].prev_hash, ordered[0].entry_hash);
        assert_eq!(ordered[2].prev_hash, ordered[1].entry_hash);
    }

    #[tokio::test]
    async fn test_verify_chain_valid() {
        let store = test_store().await;

        for i in 0..5 {
            let mut entry = test_entry();
            entry.method = Some(format!("method.{i}"));
            store.log(&entry).await.unwrap();
        }

        let verification = store.verify_chain().await.unwrap();
        assert!(verification.valid);
        assert_eq!(verification.entries_checked, 5);
        assert!(verification.first_broken_at.is_none());
    }

    #[tokio::test]
    async fn test_verify_chain_empty() {
        let store = test_store().await;
        let verification = store.verify_chain().await.unwrap();
        assert!(verification.valid);
        assert_eq!(verification.entries_checked, 0);
    }

    #[tokio::test]
    async fn test_verify_chain_detects_tampering() {
        let store = test_store().await;

        for i in 0..3 {
            let mut entry = test_entry();
            entry.method = Some(format!("method.{i}"));
            store.log(&entry).await.unwrap();
        }

        // Tamper with entry 2's hash
        sqlx::query("UPDATE audit_log SET entry_hash = 'tampered' WHERE id = 2")
            .execute(&store.pool)
            .await
            .unwrap();

        let verification = store.verify_chain().await.unwrap();
        assert!(!verification.valid);
        // Should detect the break at entry 2
        assert_eq!(verification.first_broken_at, Some(2));
    }

    #[tokio::test]
    async fn test_hash_is_deterministic() {
        let h1 = AuditStore::compute_hash("prev", "method", "2026-01-01T00:00:00.000", "{}");
        let h2 = AuditStore::compute_hash("prev", "method", "2026-01-01T00:00:00.000", "{}");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[tokio::test]
    async fn test_hash_changes_with_different_inputs() {
        let h1 = AuditStore::compute_hash("", "ping", "2026-01-01", "{}");
        let h2 = AuditStore::compute_hash("", "pong", "2026-01-01", "{}");
        assert_ne!(h1, h2);
    }

    #[tokio::test]
    async fn test_record_includes_hash_fields() {
        let store = test_store().await;
        store.log(&test_entry()).await.unwrap();

        let records = store.recent(1).await.unwrap();
        assert!(records[0].entry_hash.is_some());
        assert!(records[0].prev_hash.is_some());
        assert_eq!(records[0].prev_hash.as_deref(), Some(""));
    }
}
