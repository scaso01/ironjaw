//! Brain snapshot / restore infrastructure — D0.
//!
//! Stores point-in-time snapshots of all non-deleted memories plus identity
//! KV pairs and context-variable documents. Snapshots are serialised as
//! JSON (`serde_json::to_vec`) and stored in the `brain_snapshots` table
//! (migration 012, idempotent `CREATE TABLE IF NOT EXISTS`).
//!
//! Four public async methods exposed via `SnapshotStore`:
//!  - `create`  — serialise current brain state → new snapshot row
//!  - `list`    — return all snapshot metadata (no payload)
//!  - `restore` — deserialise a snapshot, soft-delete live memories, bulk-insert
//!  - `delete`  — soft-delete a snapshot row

use crate::store::BrainStore;
use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePool;
use tracing::{debug, info};

// ── Types ────────────────────────────────────────────────────────────────────

/// Metadata row returned by `list` (payload excluded for bandwidth).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainSnapshot {
    pub id: i64,
    pub label: String,
    pub memory_count: i64,
    pub identity_count: i64,
    pub context_var_count: i64,
    pub created_at: String,
    pub deleted: bool,
}

/// Summary of what was written during a restore operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreReport {
    pub snapshot_id: i64,
    pub memories_written: usize,
    pub identities_written: usize,
    pub context_vars_written: usize,
    pub memories_soft_deleted: u64,
}

/// Full payload stored as a JSON blob in `brain_snapshots.payload`.
/// All three collections are serialised atomically.
#[derive(Debug, Serialize, Deserialize)]
struct SnapshotPayload {
    memories: Vec<MemorySnap>,
    identities: Vec<IdentitySnap>,
    context_vars: Vec<ContextVarSnap>,
}

/// Minimal memory fields needed to round-trip a snapshot restore.
/// We store enough to reconstruct a `NewMemory` without requiring the
/// original embedding (embeddings are re-generated on demand).
#[derive(Debug, Serialize, Deserialize)]
struct MemorySnap {
    content: String,
    source: Option<String>,
    embedding: Option<String>, // raw JSON array, e.g. "[0.1, 0.2, …]"
    tags: Option<String>,
    memory_type: String,
    importance: f64,
    trust_level: i32,
    scope: Option<String>,
    created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct IdentitySnap {
    key: String,
    content: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ContextVarSnap {
    name: String,
    content: String,
    loaded: bool,
}

// ── SnapshotStore ─────────────────────────────────────────────────────────────

/// Handles all brain snapshot operations on the shared SQLite pool.
///
/// Constructed by sharing the pool from `BrainStore::pool()`.  The
/// migration for `brain_snapshots` is run inside `BrainStore::run_migrations()`
/// so the table already exists by the time `SnapshotStore` is used.
#[derive(Clone, Debug)]
pub struct SnapshotStore {
    pool: SqlitePool,
}

impl SnapshotStore {
    /// Create a `SnapshotStore` backed by an existing SQLite pool.
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── create ───────────────────────────────────────────────────────────────

    /// Serialise the current brain state into a new snapshot row.
    ///
    /// Reads all non-deleted memories, identity KV pairs, and loaded context
    /// variables from the DB and writes them as a JSON blob.
    pub async fn create(&self, label: &str, store: &BrainStore) -> Result<BrainSnapshot> {
        // 1. Gather memories
        let memories: Vec<MemoryRow> = sqlx::query_as::<_, MemoryRow>(
            r#"SELECT id, content, source, embedding, tags, memory_type,
                      importance, trust_level, scope, created_at
               FROM memories
               WHERE deleted = 0
               ORDER BY created_at ASC"#,
        )
        .fetch_all(store.pool())
        .await
        .context("fetch memories for snapshot")?;

        let memory_snaps: Vec<MemorySnap> = memories
            .iter()
            .map(|r| MemorySnap {
                content: r.content.clone(),
                source: r.source.clone(),
                embedding: r.embedding.clone(),
                tags: r.tags.clone(),
                memory_type: r.memory_type.clone(),
                importance: r.importance,
                trust_level: r.trust_level,
                scope: r.scope.clone(),
                created_at: r.created_at.clone(),
            })
            .collect();

        // 2. Gather identity KV
        let identities: Vec<IdentityRow> =
            sqlx::query_as::<_, IdentityRow>("SELECT key, content FROM identity ORDER BY key")
                .fetch_all(store.pool())
                .await
                .context("fetch identity for snapshot")?;

        let identity_snaps: Vec<IdentitySnap> = identities
            .iter()
            .map(|r| IdentitySnap {
                key: r.key.clone(),
                content: r.content.clone(),
            })
            .collect();

        // 3. Gather context variables
        let context_vars: Vec<ContextVarRow> = sqlx::query_as::<_, ContextVarRow>(
            "SELECT name, content, loaded FROM context_variables ORDER BY name",
        )
        .fetch_all(store.pool())
        .await
        .context("fetch context_variables for snapshot")?;

        let context_var_snaps: Vec<ContextVarSnap> = context_vars
            .iter()
            .map(|r| ContextVarSnap {
                name: r.name.clone(),
                content: r.content.clone(),
                loaded: r.loaded,
            })
            .collect();

        let memory_count = memory_snaps.len() as i64;
        let identity_count = identity_snaps.len() as i64;
        let context_var_count = context_var_snaps.len() as i64;

        let payload = SnapshotPayload {
            memories: memory_snaps,
            identities: identity_snaps,
            context_vars: context_var_snaps,
        };

        let payload_bytes =
            serde_json::to_vec(&payload).context("serialise snapshot payload to JSON")?;

        let now = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();

        let row = sqlx::query_as::<_, InsertedId>(
            r#"INSERT INTO brain_snapshots
                   (label, memory_count, identity_count, context_var_count, payload, created_at, deleted)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
               RETURNING id"#,
        )
        .bind(label)
        .bind(memory_count)
        .bind(identity_count)
        .bind(context_var_count)
        .bind(payload_bytes)
        .bind(&now)
        .fetch_one(&self.pool)
        .await
        .context("insert brain_snapshots row")?;

        info!(
            id = row.id,
            label, memory_count, identity_count, context_var_count, "Brain snapshot created"
        );

        Ok(BrainSnapshot {
            id: row.id,
            label: label.to_string(),
            memory_count,
            identity_count,
            context_var_count,
            created_at: now,
            deleted: false,
        })
    }

    // ── list ─────────────────────────────────────────────────────────────────

    /// Return all non-deleted snapshot rows (metadata only, no payload).
    pub async fn list(&self) -> Result<Vec<BrainSnapshot>> {
        let rows: Vec<SnapshotRow> = sqlx::query_as::<_, SnapshotRow>(
            r#"SELECT id, label, memory_count, identity_count, context_var_count, created_at
               FROM brain_snapshots
               WHERE deleted = 0
               ORDER BY created_at DESC, id DESC"#,
        )
        .fetch_all(&self.pool)
        .await
        .context("list brain_snapshots")?;

        Ok(rows
            .into_iter()
            .map(|r| BrainSnapshot {
                id: r.id,
                label: r.label,
                memory_count: r.memory_count,
                identity_count: r.identity_count,
                context_var_count: r.context_var_count,
                created_at: r.created_at,
                deleted: false,
            })
            .collect())
    }

    // ── restore ───────────────────────────────────────────────────────────────

    /// Restore the brain to the state captured in snapshot `snapshot_id`.
    ///
    /// Steps (atomic transaction):
    ///  1. Load payload from `brain_snapshots`.
    ///  2. Soft-delete all current non-deleted memories (`deleted = 1`).
    ///  3. Clear identity table.
    ///  4. Clear context_variables table.
    ///  5. Bulk-insert memories from payload.
    ///  6. Bulk-insert identities from payload.
    ///  7. Bulk-insert context_variables from payload.
    ///
    /// Returns a `RestoreReport` with counts.
    pub async fn restore(&self, snapshot_id: i64) -> Result<RestoreReport> {
        // Load payload
        let row: Option<PayloadRow> = sqlx::query_as::<_, PayloadRow>(
            "SELECT payload FROM brain_snapshots WHERE id = ?1 AND deleted = 0",
        )
        .bind(snapshot_id)
        .fetch_optional(&self.pool)
        .await
        .context("fetch snapshot payload")?;

        let row = row.with_context(|| format!("snapshot {snapshot_id} not found or deleted"))?;

        let payload: SnapshotPayload =
            serde_json::from_slice(&row.payload).context("deserialise snapshot payload")?;

        // All mutations in one transaction
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin restore transaction")?;

        // 2. Soft-delete live memories
        let del_result = sqlx::query("UPDATE memories SET deleted = 1 WHERE deleted = 0")
            .execute(&mut *tx)
            .await
            .context("soft-delete memories")?;
        let memories_soft_deleted: u64 = del_result.rows_affected();

        // 3. Clear identity
        sqlx::query("DELETE FROM identity")
            .execute(&mut *tx)
            .await
            .context("clear identity table")?;

        // 4. Clear context_variables
        sqlx::query("DELETE FROM context_variables")
            .execute(&mut *tx)
            .await
            .context("clear context_variables table")?;

        // 5. Insert memories
        let memories_written: usize = payload.memories.len();
        for mem in &payload.memories {
            sqlx::query(
                r#"INSERT INTO memories
                       (content, source, embedding, tags, memory_type, importance,
                        trust_level, scope, created_at, deleted,
                        strength, access_count, quality_score)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, 1.0, 0, 0.5)"#,
            )
            .bind(&mem.content)
            .bind(&mem.source)
            .bind(&mem.embedding)
            .bind(&mem.tags)
            .bind(&mem.memory_type)
            .bind(mem.importance)
            .bind(mem.trust_level)
            .bind(&mem.scope)
            .bind(&mem.created_at)
            .execute(&mut *tx)
            .await
            .context("insert restored memory")?;
        }

        // 6. Insert identities
        let identities_written: usize = payload.identities.len();
        for id in &payload.identities {
            sqlx::query(
                "INSERT OR REPLACE INTO identity (key, content, updated_at) \
                 VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%f', 'now'))",
            )
            .bind(&id.key)
            .bind(&id.content)
            .execute(&mut *tx)
            .await
            .context("insert restored identity")?;
        }

        // 7. Insert context variables
        let context_vars_written: usize = payload.context_vars.len();
        for cv in &payload.context_vars {
            let loaded_int: i64 = if cv.loaded { 1 } else { 0 };
            sqlx::query(
                r#"INSERT OR REPLACE INTO context_variables
                       (name, content, loaded, updated_at)
                   VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%f', 'now'))"#,
            )
            .bind(&cv.name)
            .bind(&cv.content)
            .bind(loaded_int)
            .execute(&mut *tx)
            .await
            .context("insert restored context_variable")?;
        }

        tx.commit().await.context("commit restore transaction")?;

        info!(
            snapshot_id,
            memories_written,
            identities_written,
            context_vars_written,
            memories_soft_deleted,
            "Brain snapshot restored"
        );

        Ok(RestoreReport {
            snapshot_id,
            memories_written,
            identities_written,
            context_vars_written,
            memories_soft_deleted,
        })
    }

    // ── delete ────────────────────────────────────────────────────────────────

    /// Soft-delete a snapshot (sets `deleted = 1`).
    /// Returns `true` if the row existed and was updated, `false` if not found.
    pub async fn delete(&self, snapshot_id: i64) -> Result<bool> {
        let result =
            sqlx::query("UPDATE brain_snapshots SET deleted = 1 WHERE id = ?1 AND deleted = 0")
                .bind(snapshot_id)
                .execute(&self.pool)
                .await
                .context("soft-delete brain_snapshot")?;

        let deleted = result.rows_affected() > 0;
        debug!(snapshot_id, deleted, "snapshot delete attempted");
        Ok(deleted)
    }
}

// ── Private sqlx row types ────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct MemoryRow {
    #[allow(dead_code)]
    id: i64,
    content: String,
    source: Option<String>,
    embedding: Option<String>,
    tags: Option<String>,
    memory_type: String,
    importance: f64,
    trust_level: i32,
    scope: Option<String>,
    created_at: String,
}

#[derive(sqlx::FromRow)]
struct IdentityRow {
    key: String,
    content: String,
}

#[derive(sqlx::FromRow)]
struct ContextVarRow {
    name: String,
    content: String,
    loaded: bool,
}

#[derive(sqlx::FromRow)]
struct SnapshotRow {
    id: i64,
    label: String,
    memory_count: i64,
    identity_count: i64,
    context_var_count: i64,
    created_at: String,
}

#[derive(sqlx::FromRow)]
struct InsertedId {
    id: i64,
}

#[derive(sqlx::FromRow)]
struct PayloadRow {
    payload: Vec<u8>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BrainStore;

    async fn test_store() -> BrainStore {
        BrainStore::open_memory()
            .await
            .expect("open in-memory brain store")
    }

    async fn test_snapshot_store(store: &BrainStore) -> SnapshotStore {
        SnapshotStore::from_pool(store.pool().clone())
    }

    #[tokio::test]
    async fn test_snapshot_create_empty_brain() {
        let store = test_store().await;
        let ss = test_snapshot_store(&store).await;
        let snap = ss.create("empty", &store).await.expect("create snapshot");
        assert_eq!(snap.memory_count, 0);
        assert_eq!(snap.identity_count, 0);
        assert_eq!(snap.label, "empty");
        assert!(snap.id > 0);
    }

    #[tokio::test]
    async fn test_snapshot_list() {
        let store = test_store().await;
        let ss = test_snapshot_store(&store).await;

        let _ = ss.create("first", &store).await.expect("first");
        let _ = ss.create("second", &store).await.expect("second");

        let list = ss.list().await.expect("list");
        assert_eq!(list.len(), 2);
        // Most recent first
        assert_eq!(list[0].label, "second");
        assert_eq!(list[1].label, "first");
    }

    #[tokio::test]
    async fn test_snapshot_delete() {
        let store = test_store().await;
        let ss = test_snapshot_store(&store).await;

        let snap = ss.create("to_delete", &store).await.expect("create");
        let deleted = ss.delete(snap.id).await.expect("delete");
        assert!(deleted);

        // Second delete returns false (already deleted)
        let deleted_again = ss.delete(snap.id).await.expect("delete again");
        assert!(!deleted_again);

        let list = ss.list().await.expect("list after delete");
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn test_snapshot_delete_nonexistent() {
        let store = test_store().await;
        let ss = test_snapshot_store(&store).await;

        let deleted = ss.delete(9999).await.expect("delete nonexistent");
        assert!(!deleted);
    }

    #[tokio::test]
    async fn test_restore_report_fields() {
        let store = test_store().await;
        let ss = test_snapshot_store(&store).await;

        // Add an identity entry to verify counts
        sqlx::query(
            "INSERT OR REPLACE INTO identity (key, content, updated_at) \
             VALUES ('name', 'TestBrain', strftime('%Y-%m-%dT%H:%M:%f', 'now'))",
        )
        .execute(store.pool())
        .await
        .expect("insert identity");

        let snap = ss.create("with_identity", &store).await.expect("create");
        assert_eq!(snap.identity_count, 1);

        let report = ss.restore(snap.id).await.expect("restore");
        assert_eq!(report.snapshot_id, snap.id);
        assert_eq!(report.identities_written, 1);
        assert_eq!(report.memories_written, 0);
        assert_eq!(report.context_vars_written, 0);
    }

    #[tokio::test]
    async fn test_restore_nonexistent_snapshot_returns_error() {
        let store = test_store().await;
        let ss = test_snapshot_store(&store).await;

        let result = ss.restore(99999).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("99999"), "error should name the id: {msg}");
    }

    #[tokio::test]
    async fn test_snapshot_with_memories_roundtrip() {
        let store = test_store().await;
        let ss = test_snapshot_store(&store).await;

        // Insert two test memories directly via SQL
        sqlx::query(
            r#"INSERT INTO memories
                   (content, source, memory_type, importance, trust_level, deleted,
                    strength, access_count, created_at)
               VALUES ('memory A', 'test', 'fact', 0.8, 3, 0, 1.0, 0,
                       strftime('%Y-%m-%dT%H:%M:%f', 'now'))"#,
        )
        .execute(store.pool())
        .await
        .expect("insert memory A");

        sqlx::query(
            r#"INSERT INTO memories
                   (content, source, memory_type, importance, trust_level, deleted,
                    strength, access_count, created_at)
               VALUES ('memory B', 'test', 'fact', 0.6, 2, 0, 1.0, 0,
                       strftime('%Y-%m-%dT%H:%M:%f', 'now'))"#,
        )
        .execute(store.pool())
        .await
        .expect("insert memory B");

        let snap = ss.create("two_memories", &store).await.expect("create");
        assert_eq!(snap.memory_count, 2);

        // Restore — this soft-deletes the originals and re-inserts
        let report = ss.restore(snap.id).await.expect("restore");
        assert_eq!(report.memories_written, 2);
        assert_eq!(report.memories_soft_deleted, 2); // the originals

        // Verify memories are present after restore
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memories WHERE deleted = 0")
            .fetch_one(store.pool())
            .await
            .expect("count after restore");
        assert_eq!(count, 2);
    }
}
