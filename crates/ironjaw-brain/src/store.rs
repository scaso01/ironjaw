use crate::types::{
    ContextVariable, Identity, LearnedWorkflow, Memory, NewMemory, NewPendingMemory, NewWorkflow,
    PendingMemory, ProcedureMeta, TrustLevel, WorkflowAction,
};
use crate::vec_index::VecIndex;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Register the sqlite-vec extension globally so all new SQLite connections load it.
/// Must be called before opening any pools. Safe to call multiple times.
///
/// # Safety
/// The `sqlite_vec::sqlite3_vec_init` Rust binding is declared as `extern "C" fn()`
/// (parameterless) even though the actual C function signature is:
///   `int sqlite3_vec_init(sqlite3*, char**, const sqlite3_api_routines*)`
///
/// This is the standard pattern used by the sqlite-vec crate itself (see its own
/// `test_rusqlite_auto_extension` test). The transmute casts the function pointer
/// to the 3-param form that `sqlite3_auto_extension` expects. The underlying C
/// symbol has the correct ABI — the Rust declaration simply omits parameters for
/// ergonomics since the function is only ever called indirectly by SQLite.
fn register_sqlite_vec_extension() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        unsafe {
            libsqlite3_sys::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut libsqlite3_sys::sqlite3,
                    *mut *mut i8,
                    *const libsqlite3_sys::sqlite3_api_routines,
                ) -> i32,
            >(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
        info!("sqlite-vec extension registered globally");
    });
}

/// SQLite-backed brain store for memories and identity.
///
/// Optionally holds an in-memory `VecIndex` for O(k log n) KNN search
/// instead of O(n) full-table scans. The index is populated from DB on
/// open and kept in sync by `add_memory` / `delete_memory`.
///
/// When sqlite-vec is available, also maintains a `vec_memories` virtual
/// table for sub-linear ANN queries.
#[derive(Clone)]
pub struct BrainStore {
    pool: SqlitePool,
    /// In-memory vector index, shared across clones via Arc.
    /// None if no embedding dimension has been set.
    vec_index: Option<Arc<VecIndex>>,
    /// Whether sqlite-vec extension is loaded and vec_memories table exists.
    sqlite_vec_available: bool,
}

/// Internal row type for sqlx deserialization of pending_memories queries.
#[derive(sqlx::FromRow)]
struct PendingMemoryRow {
    id: i64,
    content: String,
    source: String,
    memory_type: String,
    tags: Option<String>,
    importance: f64,
    confidence: f64,
    evidence_session_id: Option<String>,
    embedding: Option<String>,
    created_at: String,
}

impl BrainStore {
    /// Open or create the brain database at the given path.
    pub async fn open(db_path: &str) -> anyhow::Result<Self> {
        // Register sqlite-vec before opening any connections
        register_sqlite_vec_extension();

        if let Some(parent) = Path::new(db_path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        // FIX #1: WAL + busy_timeout + synchronous=Normal on every pooled
        // connection. The default (rollback journal, synchronous=FULL, 0ms
        // busy) caused 41s stalls under concurrent maintenance writers and an
        // 8.8s COUNT(*) that blew the 3s brain-inject budget → phone turns ran
        // with zero brain context. Matches the session store's WAL setup.
        let options = SqliteConnectOptions::from_str(&format!("sqlite:{db_path}?mode=rwc"))?
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .synchronous(SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        sqlx::query(include_str!("../migrations/001_create_brain.sql"))
            .execute(&pool)
            .await?;

        let mut store = Self {
            pool,
            vec_index: None,
            sqlite_vec_available: false,
        };
        store.run_migrations().await?;
        store.probe_sqlite_vec().await;
        store.auto_init_vec_index().await;

        info!(
            path = db_path,
            vec = store.sqlite_vec_available,
            "Brain store opened"
        );
        Ok(store)
    }

    /// Open an in-memory database (for testing)
    pub async fn open_memory() -> anyhow::Result<Self> {
        // Register sqlite-vec before opening any connections
        register_sqlite_vec_extension();

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;

        sqlx::query(include_str!("../migrations/001_create_brain.sql"))
            .execute(&pool)
            .await?;

        let mut store = Self {
            pool,
            vec_index: None,
            sqlite_vec_available: false,
        };
        store.run_migrations().await?;
        store.probe_sqlite_vec().await;
        store.auto_init_vec_index().await;

        Ok(store)
    }

    /// Check if sqlite-vec extension is loaded and usable.
    async fn probe_sqlite_vec(&mut self) {
        match sqlx::query_scalar::<_, String>("SELECT vec_version()")
            .fetch_one(&self.pool)
            .await
        {
            Ok(version) => {
                info!(version = %version, "sqlite-vec extension available");
                self.sqlite_vec_available = true;
            }
            Err(e) => {
                warn!(error = %e, "sqlite-vec extension not available, using fallback vector search");
                self.sqlite_vec_available = false;
            }
        }
    }

    /// Returns true if sqlite-vec extension is loaded and available.
    pub fn has_sqlite_vec(&self) -> bool {
        self.sqlite_vec_available
    }

    /// Get a reference to the underlying connection pool (for sharing with KnowledgeGraph)
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Run additive migrations that extend the schema.
    /// Safe to call multiple times -- checks for column existence before ALTER.
    async fn run_migrations(&self) -> anyhow::Result<()> {
        // Migration 003: typed memories
        let has_memory_type = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'memory_type'",
        )
        .fetch_one(&self.pool)
        .await?;

        if has_memory_type == 0 {
            sqlx::query("ALTER TABLE memories ADD COLUMN memory_type TEXT NOT NULL DEFAULT 'fact'")
                .execute(&self.pool)
                .await?;
            sqlx::query("ALTER TABLE memories ADD COLUMN importance REAL NOT NULL DEFAULT 0.5")
                .execute(&self.pool)
                .await?;
            sqlx::query("ALTER TABLE memories ADD COLUMN related_to TEXT")
                .execute(&self.pool)
                .await?;
        }

        // Always create indices (IF NOT EXISTS is safe to repeat)
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(memory_type)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_memories_importance ON memories(importance)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_memories_accessed ON memories(accessed_at)")
            .execute(&self.pool)
            .await?;

        // Migration 004: learner redesign (strength, supersedes, content_hash, learn_index)
        let has_strength = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'strength'",
        )
        .fetch_one(&self.pool)
        .await?;

        if has_strength == 0 {
            sqlx::query("ALTER TABLE memories ADD COLUMN strength REAL NOT NULL DEFAULT 1.0")
                .execute(&self.pool)
                .await?;
            sqlx::query("ALTER TABLE memories ADD COLUMN supersedes INTEGER")
                .execute(&self.pool)
                .await?;
            sqlx::query("ALTER TABLE memories ADD COLUMN superseded_by INTEGER")
                .execute(&self.pool)
                .await?;
            sqlx::query("ALTER TABLE memories ADD COLUMN content_hash TEXT")
                .execute(&self.pool)
                .await?;
        }

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memories_content_hash ON memories(content_hash)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_memories_strength ON memories(strength)")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memories_superseded_by ON memories(superseded_by)",
        )
        .execute(&self.pool)
        .await?;

        // Migration 005: temporal facts + context variables
        let has_valid_from = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'valid_from'",
        )
        .fetch_one(&self.pool)
        .await?;

        if has_valid_from == 0 {
            sqlx::query("ALTER TABLE memories ADD COLUMN valid_from TEXT")
                .execute(&self.pool)
                .await?;
            sqlx::query("ALTER TABLE memories ADD COLUMN valid_to TEXT")
                .execute(&self.pool)
                .await?;
        }

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_memories_valid_from ON memories(valid_from)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_memories_valid_to ON memories(valid_to)")
            .execute(&self.pool)
            .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS context_variables (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                content TEXT NOT NULL,
                token_count INTEGER NOT NULL DEFAULT 0,
                source_path TEXT,
                loaded INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
            )",
        )
        .execute(&self.pool)
        .await?;

        // learn_index table for incremental indexing
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS learn_index (
                session_id TEXT PRIMARY KEY,
                last_processed_turn INTEGER NOT NULL DEFAULT 0,
                content_hash TEXT NOT NULL DEFAULT '',
                indexed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
            )",
        )
        .execute(&self.pool)
        .await?;

        // Migration 006: source_type column + sqlite-vec virtual table
        let has_source_type = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'source_type'",
        )
        .fetch_one(&self.pool)
        .await?;

        if has_source_type == 0 {
            sqlx::query("ALTER TABLE memories ADD COLUMN source_type TEXT DEFAULT 'brain'")
                .execute(&self.pool)
                .await?;
        }

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_memories_source_type ON memories(source_type)")
            .execute(&self.pool)
            .await?;

        // Create vec_memories virtual table if sqlite-vec is available.
        // This query will fail gracefully if the extension isn't loaded.
        if let Err(e) = sqlx::query(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_memories \
             USING vec0(embedding float[768], +memory_id INTEGER)",
        )
        .execute(&self.pool)
        .await
        {
            warn!(error = %e, "Could not create vec_memories table (sqlite-vec may not be loaded)");
        }

        // Migration 007: indexer sync tables
        sqlx::query(include_str!("../migrations/007_indexer_tables.sql"))
            .execute(&self.pool)
            .await?;

        // Migration 008: reflection, trust, evidence columns
        let has_trust_level = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'trust_level'",
        )
        .fetch_one(&self.pool)
        .await?;

        if has_trust_level == 0 {
            sqlx::query("ALTER TABLE memories ADD COLUMN reflection_json TEXT")
                .execute(&self.pool)
                .await?;
            sqlx::query("ALTER TABLE memories ADD COLUMN trust_level INTEGER NOT NULL DEFAULT 1")
                .execute(&self.pool)
                .await?;
            sqlx::query("ALTER TABLE memories ADD COLUMN evidence_session_id TEXT")
                .execute(&self.pool)
                .await?;
            sqlx::query("ALTER TABLE memories ADD COLUMN evidence_message_id TEXT")
                .execute(&self.pool)
                .await?;
        }

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_memories_trust_level ON memories(trust_level)")
            .execute(&self.pool)
            .await?;

        // Migration 009: scope, procedure_json columns + workflows table (I19/I20/I15)
        let has_scope = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'scope'",
        )
        .fetch_one(&self.pool)
        .await?;

        if has_scope == 0 {
            sqlx::query("ALTER TABLE memories ADD COLUMN scope TEXT")
                .execute(&self.pool)
                .await?;
            sqlx::query("ALTER TABLE memories ADD COLUMN procedure_json TEXT")
                .execute(&self.pool)
                .await?;
        }

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_memories_scope ON memories(scope)")
            .execute(&self.pool)
            .await?;

        // Workflows table for teach-and-repeat (I15)
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS workflows (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                actions_json TEXT NOT NULL,
                occurrence_count INTEGER NOT NULL DEFAULT 1,
                is_candidate INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
            )",
        )
        .execute(&self.pool)
        .await?;

        // Action log table for workflow pattern detection (I15)
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS action_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                method TEXT NOT NULL,
                param_fingerprint TEXT,
                session_id TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_action_log_session ON action_log(session_id)")
            .execute(&self.pool)
            .await?;

        // Migration 010: quality_score + quality_flag columns
        let has_quality_score = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'quality_score'",
        )
        .fetch_one(&self.pool)
        .await?;

        if has_quality_score == 0 {
            sqlx::query("ALTER TABLE memories ADD COLUMN quality_score REAL NOT NULL DEFAULT 0.5")
                .execute(&self.pool)
                .await?;
            sqlx::query("ALTER TABLE memories ADD COLUMN quality_flag TEXT")
                .execute(&self.pool)
                .await?;
        }

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memories_quality_score ON memories(quality_score)",
        )
        .execute(&self.pool)
        .await?;

        // Migration 011: pending_memories table (HITL approval queue)
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS pending_memories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                content TEXT NOT NULL,
                source TEXT NOT NULL,
                memory_type TEXT NOT NULL DEFAULT 'fact',
                tags TEXT,
                importance REAL NOT NULL DEFAULT 0.5,
                confidence REAL NOT NULL DEFAULT 0.7,
                evidence_session_id TEXT,
                embedding TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
                status TEXT NOT NULL DEFAULT 'pending'
            )",
        )
        .execute(&self.pool)
        .await?;

        // Migration 012: brain_snapshots table (D0: snapshot/restore infrastructure)
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS brain_snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                label TEXT NOT NULL,
                memory_count INTEGER NOT NULL DEFAULT 0,
                identity_count INTEGER NOT NULL DEFAULT 0,
                context_var_count INTEGER NOT NULL DEFAULT 0,
                payload BLOB NOT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
                deleted INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // ── Vector Index ────────────────────────────────────────────

    /// Initialize the in-memory vector index with the given embedding dimension.
    /// Loads all existing embeddings from the database into the index.
    /// Call this once after opening the store when embeddings are enabled.
    pub async fn init_vec_index(&mut self, dims: usize) -> anyhow::Result<usize> {
        let index = Arc::new(VecIndex::new(dims));
        let all_embeddings = self.get_all_embeddings().await?;
        let loaded = index.bulk_insert(all_embeddings);
        self.vec_index = Some(index);
        debug!(dims, loaded, "VecIndex initialized from DB");
        Ok(loaded)
    }

    /// Auto-initialize the in-memory VecIndex by peeking at the first stored
    /// embedding to learn the dimension, then bulk-loading all embeddings.
    ///
    /// Called eagerly on `open()` and `open_memory()` so the VecIndex is always
    /// populated before the first recall.  When the database has no embeddings
    /// yet (e.g. brand-new brain), this is a no-op and the index is initialised
    /// by `add_memory` on the first write that includes an embedding.
    async fn auto_init_vec_index(&mut self) {
        let peek = sqlx::query(
            "SELECT embedding FROM memories WHERE deleted = 0 AND embedding IS NOT NULL LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await;

        let dims = match peek {
            Ok(Some(row)) => {
                use sqlx::Row;
                let json: String = row.get("embedding");
                match serde_json::from_str::<Vec<f32>>(&json) {
                    Ok(v) if !v.is_empty() => v.len(),
                    _ => {
                        debug!("auto_init_vec_index: could not determine dims from first row");
                        return;
                    }
                }
            }
            Ok(None) => {
                debug!("auto_init_vec_index: no embeddings in DB, skipping pre-load");
                return;
            }
            Err(e) => {
                warn!(error = %e, "auto_init_vec_index: failed to peek at embedding row");
                return;
            }
        };

        match self.init_vec_index(dims).await {
            Ok(loaded) => {
                info!(dims, loaded, "Embedding cache pre-loaded on startup");
            }
            Err(e) => {
                warn!(error = %e, "auto_init_vec_index: init_vec_index failed, recall will use DB fallback");
            }
        }
    }

    /// Get a reference to the VecIndex, if initialized.
    pub fn vec_index(&self) -> Option<&VecIndex> {
        self.vec_index.as_deref()
    }

    /// Returns true if the vector index is initialized and available.
    pub fn has_vec_index(&self) -> bool {
        self.vec_index.is_some()
    }

    // ── Memory CRUD ──────────────────────────────────────────────

    /// Insert a new memory, returning its ID.
    /// If the VecIndex is initialized and the memory has an embedding, it is
    /// also inserted into the in-memory index for fast KNN search.
    /// If sqlite-vec is available, also inserts into the vec_memories virtual table.
    pub async fn add_memory(&self, mem: &NewMemory) -> anyhow::Result<i64> {
        let embedding_json = mem
            .embedding
            .as_ref()
            .map(|e| serde_json::to_string(e).unwrap());
        let memory_type = mem.memory_type.as_deref().unwrap_or("fact");
        let importance = mem.importance.unwrap_or(0.5);
        let strength = mem.strength.unwrap_or(1.0);
        let source_type = mem.source_type.as_deref().unwrap_or("brain");
        let reflection_json = mem
            .reflection
            .as_ref()
            .map(|r| serde_json::to_string(r).unwrap());
        let trust_level = mem.trust_level.unwrap_or(TrustLevel::ModelInferred) as i32;
        let procedure_json = mem
            .procedure
            .as_ref()
            .map(|p| serde_json::to_string(p).unwrap());

        // Quality gate: compute score if not pre-set
        let quality = crate::quality::assess_quality(&mem.content);
        let quality_score = mem.quality_score.unwrap_or(quality.score as f64);
        let quality_flag = mem.quality_flag.clone().or(quality.flag);

        let id = sqlx::query_scalar::<_, i64>(
            r#"INSERT INTO memories (content, source, embedding, tags, memory_type, importance, strength, supersedes, content_hash, valid_from, valid_to, source_type, reflection_json, trust_level, evidence_session_id, evidence_message_id, scope, procedure_json, quality_score, quality_flag)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, COALESCE(?10, strftime('%Y-%m-%dT%H:%M:%f', 'now')), ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
               RETURNING id"#,
        )
        .bind(&mem.content)
        .bind(&mem.source)
        .bind(&embedding_json)
        .bind(&mem.tags)
        .bind(memory_type)
        .bind(importance)
        .bind(strength)
        .bind(mem.supersedes)
        .bind(&mem.content_hash)
        .bind(&mem.valid_from)
        .bind(&mem.valid_to)
        .bind(source_type)
        .bind(&reflection_json)
        .bind(trust_level)
        .bind(&mem.evidence_session_id)
        .bind(&mem.evidence_message_id)
        .bind(&mem.scope)
        .bind(&procedure_json)
        .bind(quality_score)
        .bind(&quality_flag)
        .fetch_one(&self.pool)
        .await?;

        // If this memory supersedes an older one, set the back-link and close validity
        if let Some(old_id) = mem.supersedes {
            self.set_superseded_by(old_id, id).await?;
            self.close_validity(old_id).await?;
        }

        // Keep VecIndex in sync
        if let (Some(idx), Some(emb)) = (&self.vec_index, &mem.embedding) {
            idx.insert(id, emb.clone());
        }

        // Keep sqlite-vec in sync
        if self.sqlite_vec_available {
            if let Some(emb) = &mem.embedding {
                self.insert_vec_memory(id, emb).await;
            }
        }

        Ok(id)
    }

    /// Column list shared by all memory SELECT queries.
    const MEMORY_COLS: &str = "id, content, source, embedding, tags, created_at, accessed_at, \
             access_count, memory_type, importance, related_to, strength, supersedes, \
             superseded_by, content_hash, valid_from, valid_to, source_type, \
             reflection_json, trust_level, evidence_session_id, evidence_message_id, \
             scope, procedure_json, quality_score, quality_flag";

    /// Get a single memory by ID (non-deleted only).
    pub async fn get_memory(&self, id: i64) -> anyhow::Result<Option<Memory>> {
        let sql = format!(
            "SELECT {} FROM memories WHERE id = ?1 AND deleted = 0",
            Self::MEMORY_COLS
        );
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.map(|r| self.row_to_memory(&r)))
    }

    /// Get all non-deleted memories.
    pub async fn all_memories(&self) -> anyhow::Result<Vec<Memory>> {
        let sql = format!(
            "SELECT {} FROM memories WHERE deleted = 0 ORDER BY id",
            Self::MEMORY_COLS
        );
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;

        Ok(rows.iter().map(|r| self.row_to_memory(r)).collect())
    }

    /// Soft-delete a memory. Also removes from VecIndex if present.
    pub async fn delete_memory(&self, id: i64) -> anyhow::Result<bool> {
        let result = sqlx::query("UPDATE memories SET deleted = 1 WHERE id = ?1 AND deleted = 0")
            .bind(id)
            .execute(&self.pool)
            .await?;

        let deleted = result.rows_affected() > 0;

        // Keep VecIndex in sync
        if deleted {
            if let Some(idx) = &self.vec_index {
                idx.remove(id);
            }
            // Keep sqlite-vec in sync
            if self.sqlite_vec_available {
                self.remove_vec_memory(id).await;
            }
        }

        Ok(deleted)
    }

    /// Update accessed_at timestamp and increment access_count.
    pub async fn update_accessed(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE memories SET accessed_at = strftime('%Y-%m-%dT%H:%M:%f', 'now'), \
             access_count = access_count + 1 WHERE id = ?1",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Return all non-deleted memories that have no embedding (id, content).
    pub async fn query_unembedded_memories(&self) -> anyhow::Result<Vec<(i64, String)>> {
        let rows =
            sqlx::query("SELECT id, content FROM memories WHERE deleted = 0 AND embedding IS NULL")
                .fetch_all(&self.pool)
                .await?;

        use sqlx::Row;
        Ok(rows
            .iter()
            .map(|r| (r.get("id"), r.get("content")))
            .collect())
    }

    /// Update the embedding for an existing memory and sync to VecIndex + vec_memories.
    pub async fn update_embedding_and_vec(&self, id: i64, embedding: &[f32]) -> anyhow::Result<()> {
        let emb_json = serde_json::to_string(embedding)?;
        sqlx::query("UPDATE memories SET embedding = ?1 WHERE id = ?2")
            .bind(&emb_json)
            .bind(id)
            .execute(&self.pool)
            .await?;
        // Keep in-memory VecIndex in sync
        if let Some(idx) = &self.vec_index {
            idx.insert(id, embedding.to_vec());
        }
        // Keep sqlite-vec in sync
        self.insert_vec_memory(id, embedding).await;
        Ok(())
    }

    /// Count non-deleted memories that have no embedding.
    pub async fn count_unembedded(&self) -> anyhow::Result<i64> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM memories WHERE deleted = 0 AND embedding IS NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Count non-deleted memories.
    pub async fn count_memories(&self) -> anyhow::Result<i64> {
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM memories WHERE deleted = 0")
            .fetch_one(&self.pool)
            .await?;

        Ok(count)
    }

    /// Get all (id, embedding) pairs for similarity search.
    pub async fn get_all_embeddings(&self) -> anyhow::Result<Vec<(i64, Vec<f32>)>> {
        let rows = sqlx::query(
            "SELECT id, embedding FROM memories WHERE deleted = 0 AND embedding IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?;

        use sqlx::Row;
        let mut result = Vec::new();
        for row in &rows {
            let emb_json: String = row.get("embedding");
            if let Ok(emb) = serde_json::from_str::<Vec<f32>>(&emb_json) {
                let id: i64 = row.get("id");
                result.push((id, emb));
            }
        }

        Ok(result)
    }

    /// Soft-delete oldest-accessed memories when count exceeds `keep`.
    /// Also removes evicted entries from the VecIndex.
    pub async fn evict_lru(&self, keep: usize) -> anyhow::Result<u64> {
        let count = self.count_memories().await?;
        if count <= keep as i64 {
            return Ok(0);
        }

        let to_evict = count - keep as i64;

        // If VecIndex or sqlite-vec is active, collect the IDs that will be evicted
        // so we can remove them from the index after the DB update.
        let evicted_ids: Vec<i64> = if self.vec_index.is_some() || self.sqlite_vec_available {
            sqlx::query_scalar::<_, i64>(
                "SELECT id FROM memories WHERE deleted = 0 ORDER BY accessed_at ASC LIMIT ?1",
            )
            .bind(to_evict)
            .fetch_all(&self.pool)
            .await?
        } else {
            Vec::new()
        };

        let result = sqlx::query(
            "UPDATE memories SET deleted = 1 WHERE id IN \
             (SELECT id FROM memories WHERE deleted = 0 ORDER BY accessed_at ASC LIMIT ?1)",
        )
        .bind(to_evict)
        .execute(&self.pool)
        .await?;

        // Sync VecIndex
        if let Some(idx) = &self.vec_index {
            for id in &evicted_ids {
                idx.remove(*id);
            }
        }

        // Sync sqlite-vec
        if self.sqlite_vec_available {
            for id in &evicted_ids {
                self.remove_vec_memory(*id).await;
            }
        }

        Ok(result.rows_affected())
    }

    /// KNN search using the in-memory VecIndex.
    ///
    /// If the VecIndex is initialized, performs fast in-memory search.
    /// Otherwise, falls back to loading all embeddings from DB (O(n)).
    ///
    /// Returns `(id, similarity_score)` pairs sorted by descending score.
    /// Results are NOT filtered by score threshold -- callers decide their own cutoff.
    pub async fn knn_search(
        &self,
        query_embedding: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<(i64, f32)>> {
        if let Some(idx) = &self.vec_index {
            // Fast path: in-memory VecIndex
            Ok(idx.search_ids(query_embedding, k))
        } else {
            // Slow path: load all from DB
            use crate::embedder::cosine_similarity;
            let all_embeddings = self.get_all_embeddings().await?;
            let mut scored: Vec<(i64, f32)> = all_embeddings
                .iter()
                .map(|(id, emb)| (*id, cosine_similarity(query_embedding, emb)))
                .collect();

            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(k);
            Ok(scored)
        }
    }

    // ── sqlite-vec operations ──────────────────────────────────────

    /// Insert an embedding into the sqlite-vec virtual table.
    /// Fails silently if sqlite-vec is not available.
    async fn insert_vec_memory(&self, memory_id: i64, embedding: &[f32]) {
        let blob = embedding_to_blob(embedding);
        if let Err(e) =
            sqlx::query("INSERT INTO vec_memories (embedding, memory_id) VALUES (?1, ?2)")
                .bind(&blob)
                .bind(memory_id)
                .execute(&self.pool)
                .await
        {
            warn!(memory_id, error = %e, "Failed to insert into vec_memories");
        }
    }

    /// Remove an embedding from the sqlite-vec virtual table.
    async fn remove_vec_memory(&self, memory_id: i64) {
        if let Err(e) = sqlx::query("DELETE FROM vec_memories WHERE memory_id = ?1")
            .bind(memory_id)
            .execute(&self.pool)
            .await
        {
            warn!(memory_id, error = %e, "Failed to remove from vec_memories");
        }
    }

    /// KNN search using sqlite-vec virtual table.
    ///
    /// Returns `(memory_id, distance)` pairs sorted by ascending distance.
    /// Distance is L2 (Euclidean) from sqlite-vec, not cosine similarity.
    pub async fn knn_search_vec(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> anyhow::Result<Vec<(i64, f32)>> {
        if !self.sqlite_vec_available {
            return self.knn_search(query_embedding, limit).await;
        }

        let blob = embedding_to_blob(query_embedding);
        let rows = sqlx::query(
            "SELECT memory_id, distance \
             FROM vec_memories \
             WHERE embedding MATCH ?1 \
             ORDER BY distance \
             LIMIT ?2",
        )
        .bind(&blob)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        use sqlx::Row;
        Ok(rows
            .iter()
            .map(|r| {
                let memory_id: i64 = r.get("memory_id");
                let distance: f32 = r.get("distance");
                (memory_id, distance)
            })
            .collect())
    }

    /// Find semantically similar memories using vector search.
    /// Returns `(memory_id, distance)` pairs within the given distance threshold.
    pub async fn find_similar(
        &self,
        embedding: &[f32],
        threshold: f32,
    ) -> anyhow::Result<Vec<(i64, f32)>> {
        // Search a generous limit and filter by threshold
        let results = self.knn_search_vec(embedding, 20).await?;
        Ok(results
            .into_iter()
            .filter(|(_, dist)| *dist <= threshold)
            .collect())
    }

    /// Find a memory by its content hash. Returns the full Memory if found.
    pub async fn find_by_hash(&self, hash: &str) -> anyhow::Result<Option<Memory>> {
        let sql = format!(
            "SELECT {} FROM memories WHERE content_hash = ?1 AND deleted = 0 LIMIT 1",
            Self::MEMORY_COLS
        );
        let row = sqlx::query(&sql)
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.map(|r| self.row_to_memory(&r)))
    }

    /// Backfill the vec_memories virtual table from existing JSON embeddings.
    /// Safe to call multiple times — skips memories already in vec_memories.
    /// Returns the number of embeddings backfilled.
    pub async fn backfill_vec_memories(&self) -> anyhow::Result<usize> {
        if !self.sqlite_vec_available {
            return Ok(0);
        }

        // vec0 virtual tables don't support efficient JOINs — a LEFT JOIN
        // causes a full vector-index scan per probe, hanging on large tables.
        // Instead, collect existing memory_ids from vec_memories first, then
        // query memories NOT IN that set.
        use sqlx::Row;

        let existing: Vec<i64> = sqlx::query("SELECT memory_id FROM vec_memories")
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| r.get::<i64, _>("memory_id"))
            .collect();

        let existing_set: std::collections::HashSet<i64> = existing.into_iter().collect();

        let rows = sqlx::query(
            "SELECT id, embedding FROM memories \
             WHERE deleted = 0 AND embedding IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut count = 0;
        for row in &rows {
            let id: i64 = row.get("id");
            if existing_set.contains(&id) {
                continue;
            }
            let emb_json: String = row.get("embedding");
            if let Ok(emb) = serde_json::from_str::<Vec<f32>>(&emb_json) {
                self.insert_vec_memory(id, &emb).await;
                count += 1;
            }
        }

        if count > 0 {
            info!(count, "Backfilled vec_memories from JSON embeddings");
        }
        Ok(count)
    }

    // ── Identity CRUD ────────────────────────────────────────────

    /// Set an identity key-value pair (upsert).
    pub async fn set_identity(&self, key: &str, content: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO identity (key, content, updated_at) VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%f', 'now')) \
             ON CONFLICT(key) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(content)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get a single identity entry by key.
    pub async fn get_identity(&self, key: &str) -> anyhow::Result<Option<Identity>> {
        let row = sqlx::query("SELECT key, content, updated_at FROM identity WHERE key = ?1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;

        use sqlx::Row;
        Ok(row.map(|r| Identity {
            key: r.get("key"),
            content: r.get("content"),
            updated_at: r.get("updated_at"),
        }))
    }

    /// Get all identity entries.
    pub async fn get_all_identity(&self) -> anyhow::Result<Vec<Identity>> {
        let rows = sqlx::query("SELECT key, content, updated_at FROM identity ORDER BY key")
            .fetch_all(&self.pool)
            .await?;

        use sqlx::Row;
        Ok(rows
            .iter()
            .map(|r| Identity {
                key: r.get("key"),
                content: r.get("content"),
                updated_at: r.get("updated_at"),
            })
            .collect())
    }

    // ── Helpers ──────────────────────────────────────────────────

    fn row_to_memory(&self, row: &sqlx::sqlite::SqliteRow) -> Memory {
        use sqlx::Row;
        let emb_json: Option<String> = row.get("embedding");
        let embedding = emb_json.and_then(|j| serde_json::from_str(&j).ok());
        let source_type: Option<String> = row.get("source_type");
        let reflection_json: Option<String> = row.get("reflection_json");
        let reflection = reflection_json.and_then(|j| serde_json::from_str(&j).ok());
        let trust_level_raw: i32 = row.get("trust_level");
        let procedure_json: Option<String> = row.get("procedure_json");
        let procedure: Option<ProcedureMeta> =
            procedure_json.and_then(|j| serde_json::from_str(&j).ok());

        Memory {
            id: row.get("id"),
            content: row.get("content"),
            source: row.get("source"),
            embedding,
            tags: row.get("tags"),
            created_at: row.get("created_at"),
            accessed_at: row.get("accessed_at"),
            access_count: row.get("access_count"),
            memory_type: row.get("memory_type"),
            importance: row.get("importance"),
            related_to: row.get("related_to"),
            strength: row.get("strength"),
            supersedes: row.get("supersedes"),
            superseded_by: row.get("superseded_by"),
            content_hash: row.get("content_hash"),
            valid_from: row.get("valid_from"),
            valid_to: row.get("valid_to"),
            source_type: source_type.unwrap_or_else(|| "brain".to_string()),
            reflection,
            trust_level: TrustLevel::from(trust_level_raw),
            evidence_session_id: row.get("evidence_session_id"),
            evidence_message_id: row.get("evidence_message_id"),
            scope: row.get("scope"),
            procedure,
            quality_score: row.get("quality_score"),
            quality_flag: row.get("quality_flag"),
        }
    }

    // ── Learner support queries ────────────────────────────────

    /// Find a memory by exact content hash. Returns the ID if found.
    pub async fn find_by_content_hash(&self, hash: &str) -> anyhow::Result<Option<i64>> {
        let id = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM memories WHERE content_hash = ?1 AND deleted = 0 LIMIT 1",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?;

        Ok(id)
    }

    /// Set the superseded_by back-link on an older memory.
    pub async fn set_superseded_by(&self, old_id: i64, new_id: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE memories SET superseded_by = ?1 WHERE id = ?2")
            .bind(new_id)
            .bind(old_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Find non-deleted memories matching any of the given tags (comma-separated in DB).
    /// Returns (id, tags) pairs for tag overlap checking.
    pub async fn find_by_tags(&self, tags: &[String]) -> anyhow::Result<Vec<(i64, String)>> {
        if tags.is_empty() {
            return Ok(Vec::new());
        }
        // Build a LIKE-based OR query for tag matching
        let conditions: Vec<String> = tags
            .iter()
            .enumerate()
            .map(|(i, _)| format!("tags LIKE '%' || ?{} || '%'", i + 1))
            .collect();
        let sql = format!(
            "SELECT id, tags FROM memories WHERE deleted = 0 AND ({}) LIMIT 100",
            conditions.join(" OR ")
        );

        let mut query = sqlx::query(&sql);
        for tag in tags {
            query = query.bind(tag);
        }

        let rows = query.fetch_all(&self.pool).await?;

        use sqlx::Row;
        Ok(rows
            .iter()
            .map(|r| {
                let id: i64 = r.get("id");
                let tags: String = r.get::<Option<String>, _>("tags").unwrap_or_default();
                (id, tags)
            })
            .collect())
    }

    /// Follow the supersession chain to find the latest version of a memory.
    pub async fn follow_supersession_chain(&self, id: i64) -> anyhow::Result<i64> {
        let mut current = id;
        let mut visited = std::collections::HashSet::new();
        visited.insert(current);

        loop {
            let next = sqlx::query_scalar::<_, i64>(
                "SELECT superseded_by FROM memories WHERE id = ?1 AND superseded_by IS NOT NULL",
            )
            .bind(current)
            .fetch_optional(&self.pool)
            .await?;

            match next {
                Some(next_id) if !visited.contains(&next_id) => {
                    visited.insert(next_id);
                    current = next_id;
                }
                _ => break,
            }
        }

        Ok(current)
    }

    /// Batch-update strength for multiple memories (used by consolidation).
    pub async fn update_strength(&self, updates: &[(i64, f64)]) -> anyhow::Result<u64> {
        let mut affected = 0u64;
        for (id, strength) in updates {
            let result = sqlx::query("UPDATE memories SET strength = ?1 WHERE id = ?2")
                .bind(strength)
                .bind(id)
                .execute(&self.pool)
                .await?;
            affected += result.rows_affected();
        }
        Ok(affected)
    }

    /// Get all non-superseded memories with their strength for decay calculations.
    pub async fn get_active_memories_for_decay(&self) -> anyhow::Result<Vec<(i64, f64, String)>> {
        let rows = sqlx::query(
            // File-backed memories are excluded: they are refreshed by their
            // source file, not by access, so decaying them only sinks curated
            // content in ranking and feeds the strength prune.
            // See `maintenance::is_file_backed`.
            "SELECT id, strength, accessed_at FROM memories \
             WHERE deleted = 0 AND superseded_by IS NULL AND source_type != 'markdown'",
        )
        .fetch_all(&self.pool)
        .await?;

        use sqlx::Row;
        Ok(rows
            .iter()
            .map(|r| {
                let id: i64 = r.get("id");
                let strength: f64 = r.get("strength");
                let accessed_at: String = r.get("accessed_at");
                (id, strength, accessed_at)
            })
            .collect())
    }

    /// Get all non-superseded memories with extended info for tiered decay.
    /// Returns `(id, strength, accessed_at, access_count)`.
    pub async fn get_active_memories_for_tiered_decay(
        &self,
    ) -> anyhow::Result<Vec<(i64, f64, String, i64)>> {
        let rows = sqlx::query(
            // File-backed memories excluded — see `get_active_memories_for_decay`.
            "SELECT id, strength, accessed_at, access_count FROM memories \
             WHERE deleted = 0 AND superseded_by IS NULL AND source_type != 'markdown'",
        )
        .fetch_all(&self.pool)
        .await?;

        use sqlx::Row;
        Ok(rows
            .iter()
            .map(|r| {
                let id: i64 = r.get("id");
                let strength: f64 = r.get("strength");
                let accessed_at: String = r.get("accessed_at");
                let access_count: i64 = r.get("access_count");
                (id, strength, accessed_at, access_count)
            })
            .collect())
    }

    /// Get memories that are frequently accessed (for boosting).
    pub async fn get_frequently_accessed(
        &self,
        min_access: i64,
    ) -> anyhow::Result<Vec<(i64, f64)>> {
        let rows = sqlx::query(
            "SELECT id, importance FROM memories \
             WHERE deleted = 0 AND access_count >= ?1 AND superseded_by IS NULL",
        )
        .bind(min_access)
        .fetch_all(&self.pool)
        .await?;

        use sqlx::Row;
        Ok(rows
            .iter()
            .map(|r| {
                let id: i64 = r.get("id");
                let importance: f64 = r.get("importance");
                (id, importance)
            })
            .collect())
    }

    /// Update importance for a memory (used by boost/consolidation).
    pub async fn update_importance(&self, id: i64, importance: f64) -> anyhow::Result<()> {
        sqlx::query("UPDATE memories SET importance = ?1 WHERE id = ?2")
            .bind(importance)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Learn Index CRUD ─────────────────────────────────────

    /// Get the learn index entry for a session.
    pub async fn get_learn_index(&self, session_id: &str) -> anyhow::Result<Option<(i64, String)>> {
        let row = sqlx::query(
            "SELECT last_processed_turn, content_hash FROM learn_index WHERE session_id = ?1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;

        use sqlx::Row;
        Ok(row.map(|r| {
            let turn: i64 = r.get("last_processed_turn");
            let hash: String = r.get("content_hash");
            (turn, hash)
        }))
    }

    /// Set the learn index entry for a session (upsert).
    pub async fn set_learn_index(
        &self,
        session_id: &str,
        last_turn: i64,
        content_hash: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO learn_index (session_id, last_processed_turn, content_hash, indexed_at) \
             VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%f', 'now')) \
             ON CONFLICT(session_id) DO UPDATE SET \
             last_processed_turn = excluded.last_processed_turn, \
             content_hash = excluded.content_hash, \
             indexed_at = excluded.indexed_at",
        )
        .bind(session_id)
        .bind(last_turn)
        .bind(content_hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Count memories grouped by type (for status reporting).
    pub async fn count_by_type(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let rows = sqlx::query(
            "SELECT memory_type, COUNT(*) as cnt FROM memories \
             WHERE deleted = 0 GROUP BY memory_type ORDER BY cnt DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        use sqlx::Row;
        Ok(rows
            .iter()
            .map(|r| {
                let mt: String = r.get("memory_type");
                let cnt: i64 = r.get("cnt");
                (mt, cnt)
            })
            .collect())
    }

    /// Get the most recent learn index timestamp.
    pub async fn last_learn_at(&self) -> anyhow::Result<Option<String>> {
        let ts = sqlx::query_scalar::<_, String>(
            "SELECT indexed_at FROM learn_index ORDER BY indexed_at DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(ts)
    }
    // ── Temporal Facts ─────────────────────────────────────

    /// Close the validity window of a memory by setting `valid_to = NOW()`.
    /// Used when a newer memory supersedes this one.
    pub async fn close_validity(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE memories SET valid_to = strftime('%Y-%m-%dT%H:%M:%f', 'now') WHERE id = ?1",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Query memories valid at a specific point in time.
    /// Returns memories where `valid_from <= at` AND (`valid_to IS NULL` OR `valid_to > at`).
    pub async fn query_point_in_time(&self, at: &str, limit: usize) -> anyhow::Result<Vec<Memory>> {
        let sql = format!(
            "SELECT {} FROM memories \
             WHERE deleted = 0 \
               AND valid_from IS NOT NULL AND valid_from <= ?1 \
               AND (valid_to IS NULL OR valid_to > ?1) \
             ORDER BY importance DESC \
             LIMIT ?2",
            Self::MEMORY_COLS
        );
        let rows = sqlx::query(&sql)
            .bind(at)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.iter().map(|r| self.row_to_memory(r)).collect())
    }

    /// Boost strength of a memory by a given amount, capped at 1.0.
    pub async fn boost_strength(&self, id: i64, boost: f64) -> anyhow::Result<()> {
        sqlx::query("UPDATE memories SET strength = MIN(1.0, strength + ?1) WHERE id = ?2")
            .bind(boost)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Context Variables CRUD ───────────────────────────

    /// Set (upsert) a context variable.
    pub async fn set_context_var(
        &self,
        name: &str,
        content: &str,
        source_path: Option<&str>,
        token_count: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO context_variables (name, content, source_path, token_count, \
             created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, \
                     strftime('%Y-%m-%dT%H:%M:%f', 'now'), \
                     strftime('%Y-%m-%dT%H:%M:%f', 'now')) \
             ON CONFLICT(name) DO UPDATE SET \
                 content = excluded.content, \
                 source_path = excluded.source_path, \
                 token_count = excluded.token_count, \
                 updated_at = excluded.updated_at",
        )
        .bind(name)
        .bind(content)
        .bind(source_path)
        .bind(token_count)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get a context variable by name.
    pub async fn get_context_var(&self, name: &str) -> anyhow::Result<Option<ContextVariable>> {
        let row = sqlx::query(
            "SELECT id, name, content, token_count, source_path, loaded, created_at, updated_at \
             FROM context_variables WHERE name = ?1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;

        use sqlx::Row;
        Ok(row.map(|r| ContextVariable {
            id: r.get("id"),
            name: r.get("name"),
            content: r.get("content"),
            token_count: r.get("token_count"),
            source_path: r.get("source_path"),
            loaded: r.get::<i64, _>("loaded") != 0,
            created_at: r.get("created_at"),
            updated_at: r.get("updated_at"),
        }))
    }

    /// List all context variables.
    pub async fn list_context_vars(&self) -> anyhow::Result<Vec<ContextVariable>> {
        let rows = sqlx::query(
            "SELECT id, name, content, token_count, source_path, loaded, created_at, updated_at \
             FROM context_variables ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;

        use sqlx::Row;
        Ok(rows
            .iter()
            .map(|r| ContextVariable {
                id: r.get("id"),
                name: r.get("name"),
                content: r.get("content"),
                token_count: r.get("token_count"),
                source_path: r.get("source_path"),
                loaded: r.get::<i64, _>("loaded") != 0,
                created_at: r.get("created_at"),
                updated_at: r.get("updated_at"),
            })
            .collect())
    }

    /// Set the loaded state of a context variable.
    pub async fn set_context_var_loaded(&self, name: &str, loaded: bool) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE context_variables SET loaded = ?1, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') \
             WHERE name = ?2",
        )
        .bind(loaded as i64)
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete a context variable by name.
    pub async fn delete_context_var(&self, name: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM context_variables WHERE name = ?1")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Get all loaded context variables.
    pub async fn get_loaded_context_vars(&self) -> anyhow::Result<Vec<ContextVariable>> {
        let rows = sqlx::query(
            "SELECT id, name, content, token_count, source_path, loaded, created_at, updated_at \
             FROM context_variables WHERE loaded = 1 ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;

        use sqlx::Row;
        Ok(rows
            .iter()
            .map(|r| ContextVariable {
                id: r.get("id"),
                name: r.get("name"),
                content: r.get("content"),
                token_count: r.get("token_count"),
                source_path: r.get("source_path"),
                loaded: true,
                created_at: r.get("created_at"),
                updated_at: r.get("updated_at"),
            })
            .collect())
    }

    /// Get a windowed slice of a context variable's content.
    /// Returns `(slice, total_len)`.
    pub async fn get_context_var_slice(
        &self,
        name: &str,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<Option<(String, usize)>> {
        let var = self.get_context_var(name).await?;
        Ok(var.map(|v| {
            let total = v.content.len();
            let start = offset.min(total);
            let end = (start + limit).min(total);
            (v.content[start..end].to_string(), total)
        }))
    }

    // ── Session Summaries ───────────────────────────

    /// Get session summary memories, optionally filtered by source session_id.
    pub async fn get_summaries(
        &self,
        session_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<Memory>> {
        let rows = if let Some(sid) = session_id {
            let sql = format!(
                "SELECT {} FROM memories WHERE deleted = 0 AND memory_type = 'session_summary' \
                 AND source = ?1 ORDER BY created_at DESC LIMIT ?2",
                Self::MEMORY_COLS
            );
            sqlx::query(&sql)
                .bind(sid)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
        } else {
            let sql = format!(
                "SELECT {} FROM memories WHERE deleted = 0 AND memory_type = 'session_summary' \
                 ORDER BY created_at DESC LIMIT ?1",
                Self::MEMORY_COLS
            );
            sqlx::query(&sql)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
        };

        Ok(rows.iter().map(|r| self.row_to_memory(r)).collect())
    }

    // ── Temporal & Context Stats ───────────────────────────

    /// Count memories with active temporal validity (valid_to IS NULL).
    pub async fn count_temporal_active(&self) -> anyhow::Result<i64> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM memories WHERE deleted = 0 \
             AND valid_from IS NOT NULL AND valid_to IS NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Count memories with expired temporal validity (valid_to IS NOT NULL).
    pub async fn count_temporal_expired(&self) -> anyhow::Result<i64> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM memories WHERE deleted = 0 \
             AND valid_to IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Count context variables and their total token usage.
    /// Returns `(count, total_tokens)`.
    pub async fn count_context_vars(&self) -> anyhow::Result<(i64, i64)> {
        let row = sqlx::query(
            "SELECT COUNT(*) as cnt, COALESCE(SUM(token_count), 0) as total_tokens \
             FROM context_variables",
        )
        .fetch_one(&self.pool)
        .await?;
        use sqlx::Row;
        Ok((row.get("cnt"), row.get("total_tokens")))
    }

    // ── Health metrics ──────────────────────────────────────

    /// Comprehensive health metrics snapshot, cached for 60 seconds.
    pub async fn health_metrics(&self) -> anyhow::Result<crate::types::BrainHealthMetrics> {
        use crate::types::{
            AgeBuckets, BrainHealthMetrics, SourceCount, TopMemory, TrustCount, TypeCount,
        };
        use sqlx::Row;
        use std::sync::Mutex;
        use std::time::Instant;

        // 60-second cache
        static CACHE: std::sync::OnceLock<Mutex<(Instant, Option<BrainHealthMetrics>)>> =
            std::sync::OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new((Instant::now(), None)));

        {
            let guard = cache.lock().unwrap();
            if let (ts, Some(ref cached)) = &*guard {
                if ts.elapsed().as_secs() < 60 {
                    return Ok(cached.clone());
                }
            }
        }

        // Run all queries in parallel
        let (
            active_res,
            retired_res,
            type_dist_res,
            trust_dist_res,
            age_24h_res,
            age_week_res,
            age_month_res,
            age_older_res,
            source_dist_res,
            with_emb_res,
            without_emb_res,
            avg_imp_res,
            avg_acc_res,
            top_acc_res,
        ) = tokio::join!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM memories WHERE deleted = 0")
                .fetch_one(&self.pool),
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM memories WHERE deleted = 1")
                .fetch_one(&self.pool),
            sqlx::query(
                "SELECT memory_type, COUNT(*) as cnt FROM memories \
                 WHERE deleted = 0 GROUP BY memory_type ORDER BY cnt DESC"
            )
            .fetch_all(&self.pool),
            sqlx::query(
                "SELECT COALESCE(trust_level, 'model_inferred') as tl, COUNT(*) as cnt \
                 FROM memories WHERE deleted = 0 GROUP BY tl ORDER BY cnt DESC"
            )
            .fetch_all(&self.pool),
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM memories WHERE deleted = 0 \
                 AND created_at >= strftime('%Y-%m-%dT%H:%M:%f', 'now', '-1 day')"
            )
            .fetch_one(&self.pool),
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM memories WHERE deleted = 0 \
                 AND created_at >= strftime('%Y-%m-%dT%H:%M:%f', 'now', '-7 days') \
                 AND created_at < strftime('%Y-%m-%dT%H:%M:%f', 'now', '-1 day')"
            )
            .fetch_one(&self.pool),
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM memories WHERE deleted = 0 \
                 AND created_at >= strftime('%Y-%m-%dT%H:%M:%f', 'now', '-30 days') \
                 AND created_at < strftime('%Y-%m-%dT%H:%M:%f', 'now', '-7 days')"
            )
            .fetch_one(&self.pool),
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM memories WHERE deleted = 0 \
                 AND created_at < strftime('%Y-%m-%dT%H:%M:%f', 'now', '-30 days')"
            )
            .fetch_one(&self.pool),
            sqlx::query(
                "SELECT COALESCE(source_type, 'brain') as st, COUNT(*) as cnt \
                 FROM memories WHERE deleted = 0 GROUP BY st ORDER BY cnt DESC"
            )
            .fetch_all(&self.pool),
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM memories WHERE deleted = 0 AND embedding IS NOT NULL"
            )
            .fetch_one(&self.pool),
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM memories WHERE deleted = 0 AND embedding IS NULL"
            )
            .fetch_one(&self.pool),
            sqlx::query_scalar::<_, f64>(
                "SELECT COALESCE(AVG(importance), 0.0) FROM memories WHERE deleted = 0"
            )
            .fetch_one(&self.pool),
            sqlx::query_scalar::<_, f64>(
                "SELECT COALESCE(AVG(access_count), 0.0) FROM memories WHERE deleted = 0"
            )
            .fetch_one(&self.pool),
            sqlx::query(
                "SELECT id, content, access_count, memory_type FROM memories \
                 WHERE deleted = 0 ORDER BY access_count DESC LIMIT 10"
            )
            .fetch_all(&self.pool),
        );

        let active = active_res.unwrap_or(0);
        let retired = retired_res.unwrap_or(0);

        let type_distribution = type_dist_res
            .unwrap_or_default()
            .iter()
            .map(|r| TypeCount {
                memory_type: r.get("memory_type"),
                count: r.get("cnt"),
            })
            .collect();

        let trust_distribution = trust_dist_res
            .unwrap_or_default()
            .iter()
            .map(|r| TrustCount {
                trust_level: r.get("tl"),
                count: r.get("cnt"),
            })
            .collect();

        let source_distribution = source_dist_res
            .unwrap_or_default()
            .iter()
            .map(|r| SourceCount {
                source_type: r.get("st"),
                count: r.get("cnt"),
            })
            .collect();

        let top_accessed = top_acc_res
            .unwrap_or_default()
            .iter()
            .map(|r| {
                let content: String = r.get("content");
                TopMemory {
                    id: r.get("id"),
                    content_preview: content.chars().take(120).collect(),
                    access_count: r.get("access_count"),
                    memory_type: r.get("memory_type"),
                }
            })
            .collect();

        let metrics = BrainHealthMetrics {
            active_memories: active,
            retired_memories: retired,
            total_memories: active + retired,
            type_distribution,
            trust_distribution,
            age_distribution: AgeBuckets {
                last_24h: age_24h_res.unwrap_or(0),
                last_week: age_week_res.unwrap_or(0),
                last_month: age_month_res.unwrap_or(0),
                older: age_older_res.unwrap_or(0),
            },
            source_distribution,
            with_embedding: with_emb_res.unwrap_or(0),
            without_embedding: without_emb_res.unwrap_or(0),
            avg_importance: avg_imp_res.unwrap_or(0.0),
            avg_access_count: avg_acc_res.unwrap_or(0.0),
            top_accessed,
        };

        // Update cache
        {
            let mut guard = cache.lock().unwrap();
            *guard = (Instant::now(), Some(metrics.clone()));
        }

        Ok(metrics)
    }

    // ── Sleep-agent queries ──────────────────────────────────────

    /// Get the N most recently created active memories.
    /// Used by sleep agents (contradiction detection, relationship discovery).
    pub async fn get_recent_active_memories(&self, limit: usize) -> anyhow::Result<Vec<Memory>> {
        let sql = format!(
            "SELECT {} FROM memories WHERE deleted = 0 AND valid_to IS NULL \
             ORDER BY created_at DESC, id DESC LIMIT ?1",
            Self::MEMORY_COLS
        );
        let rows = sqlx::query(&sql)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.iter().map(|r| self.row_to_memory(r)).collect())
    }

    /// Get memories that are still "open" (valid_to IS NULL) but older than
    /// `min_age_days` days. Used by the temporal verification sleep agent.
    pub async fn get_stale_open_memories(
        &self,
        min_age_days: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<Memory>> {
        let sql = format!(
            "SELECT {} FROM memories WHERE deleted = 0 AND valid_to IS NULL \
             AND created_at < strftime('%Y-%m-%dT%H:%M:%f', 'now', '-' || ?1 || ' days') \
             ORDER BY created_at ASC LIMIT ?2",
            Self::MEMORY_COLS
        );
        let rows = sqlx::query(&sql)
            .bind(min_age_days as i64)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.iter().map(|r| self.row_to_memory(r)).collect())
    }

    /// Get session summary memories from the last N days.
    /// Used by the pattern detection sleep agent.
    pub async fn get_recent_summaries(
        &self,
        days: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<Memory>> {
        let sql = format!(
            "SELECT {} FROM memories WHERE deleted = 0 AND memory_type = 'session_summary' \
             AND created_at > strftime('%Y-%m-%dT%H:%M:%f', 'now', '-' || ?1 || ' days') \
             ORDER BY created_at DESC LIMIT ?2",
            Self::MEMORY_COLS
        );
        let rows = sqlx::query(&sql)
            .bind(days as i64)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.iter().map(|r| self.row_to_memory(r)).collect())
    }

    // ── Indexer Sync State ──────────────────────────────────────

    /// Get the last-synced ID for a given source (e.g. "chronicle").
    pub async fn get_sync_state(&self, source: &str) -> anyhow::Result<Option<String>> {
        let id = sqlx::query_scalar::<_, String>(
            "SELECT last_synced_id FROM sync_state WHERE source = ?1",
        )
        .bind(source)
        .fetch_optional(&self.pool)
        .await?;
        Ok(id)
    }

    /// Upsert the sync state for a source.
    pub async fn set_sync_state(&self, source: &str, last_id: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO sync_state (source, last_synced_id, last_synced_at) \
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%f', 'now')) \
             ON CONFLICT(source) DO UPDATE SET \
             last_synced_id = excluded.last_synced_id, \
             last_synced_at = excluded.last_synced_at",
        )
        .bind(source)
        .bind(last_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── Markdown Index CRUD ─────────────────────────────────────

    /// Get the stored content hash for a markdown file.
    pub async fn get_markdown_hash(&self, file_path: &str) -> anyhow::Result<Option<String>> {
        let hash = sqlx::query_scalar::<_, String>(
            "SELECT content_hash FROM markdown_index WHERE file_path = ?1",
        )
        .bind(file_path)
        .fetch_optional(&self.pool)
        .await?;
        Ok(hash)
    }

    /// Upsert the markdown index entry for a file.
    pub async fn set_markdown_hash(
        &self,
        file_path: &str,
        content_hash: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO markdown_index (file_path, content_hash, last_indexed_at) \
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%f', 'now')) \
             ON CONFLICT(file_path) DO UPDATE SET \
             content_hash = excluded.content_hash, \
             last_indexed_at = excluded.last_indexed_at",
        )
        .bind(file_path)
        .bind(content_hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove a markdown file from the index.
    pub async fn remove_markdown_hash(&self, file_path: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM markdown_index WHERE file_path = ?1")
            .bind(file_path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// List all indexed markdown file paths.
    pub async fn list_markdown_paths(&self) -> anyhow::Result<Vec<String>> {
        let paths = sqlx::query_scalar::<_, String>(
            "SELECT file_path FROM markdown_index ORDER BY file_path",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(paths)
    }

    /// Soft-delete all memories matching a given source path.
    /// Returns the number of memories deleted.
    pub async fn soft_delete_by_source(&self, source: &str) -> anyhow::Result<u64> {
        let result =
            sqlx::query("UPDATE memories SET deleted = 1 WHERE source = ?1 AND deleted = 0")
                .bind(source)
                .execute(&self.pool)
                .await?;

        let count = result.rows_affected();

        // Remove from VecIndex
        if count > 0 {
            let ids = sqlx::query_scalar::<_, i64>(
                "SELECT id FROM memories WHERE source = ?1 AND deleted = 1",
            )
            .bind(source)
            .fetch_all(&self.pool)
            .await?;

            for id in &ids {
                if let Some(idx) = &self.vec_index {
                    idx.remove(*id);
                }
                if self.sqlite_vec_available {
                    self.remove_vec_memory(*id).await;
                }
            }
        }

        Ok(count)
    }

    /// Update the content of an existing memory (for merge/update operations).
    pub async fn update_content(&self, id: i64, content: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE memories SET content = ?1 WHERE id = ?2 AND deleted = 0")
            .bind(content)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── I20: Scope-filtered queries ────────────────────────────

    /// Query memories matching a scope prefix (e.g. `/project/ironjaw`).
    /// Matches memories whose scope starts with the given prefix.
    pub async fn query_by_scope(
        &self,
        scope_prefix: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Memory>> {
        let pattern = format!("{scope_prefix}%");
        let sql = format!(
            "SELECT {} FROM memories WHERE deleted = 0 \
             AND scope LIKE ?1 \
             ORDER BY importance DESC, created_at DESC \
             LIMIT ?2",
            Self::MEMORY_COLS
        );
        let rows = sqlx::query(&sql)
            .bind(&pattern)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.iter().map(|r| self.row_to_memory(r)).collect())
    }

    /// Update the scope of an existing memory.
    pub async fn set_scope(&self, id: i64, scope: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE memories SET scope = ?1 WHERE id = ?2")
            .bind(scope)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── I19: Procedure queries ────────────────────────────────

    /// Query all Procedure memories, optionally filtered by scope.
    pub async fn query_procedures(
        &self,
        scope_filter: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<Memory>> {
        let sql = if let Some(scope) = scope_filter {
            let pattern = format!("{scope}%");
            let sql = format!(
                "SELECT {} FROM memories WHERE deleted = 0 AND memory_type = 'procedure' \
                 AND scope LIKE ?1 \
                 ORDER BY importance DESC LIMIT ?2",
                Self::MEMORY_COLS
            );
            return Ok(sqlx::query(&sql)
                .bind(&pattern)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
                .iter()
                .map(|r| self.row_to_memory(r))
                .collect());
        } else {
            format!(
                "SELECT {} FROM memories WHERE deleted = 0 AND memory_type = 'procedure' \
                 ORDER BY importance DESC LIMIT ?1",
                Self::MEMORY_COLS
            )
        };
        let rows = sqlx::query(&sql)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.iter().map(|r| self.row_to_memory(r)).collect())
    }

    // ── Cleanup queries ─────────────────────────────────────────

    /// Find stale memories: older than `days` with access_count < `min_accesses`.
    /// Returns non-deleted memories sorted oldest-first.
    pub async fn query_stale_memories(
        &self,
        days: u64,
        min_accesses: i64,
    ) -> anyhow::Result<Vec<Memory>> {
        let sql = format!(
            "SELECT {} FROM memories WHERE deleted = 0 \
             AND accessed_at < strftime('%Y-%m-%dT%H:%M:%f', 'now', '-' || ?1 || ' days') \
             AND access_count < ?2 \
             ORDER BY accessed_at ASC",
            Self::MEMORY_COLS
        );
        let rows = sqlx::query(&sql)
            .bind(days as i64)
            .bind(min_accesses)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| self.row_to_memory(r)).collect())
    }

    /// Retire (soft-delete) a batch of memories by ID.
    /// Returns the count of successfully retired memories.
    pub async fn retire_batch(&self, ids: &[i64]) -> anyhow::Result<u64> {
        let mut retired = 0u64;
        for id in ids {
            if self.delete_memory(*id).await? {
                retired += 1;
            }
        }
        Ok(retired)
    }

    // ── Pending Memories (HITL approval queue) ──────────────────

    /// Insert a memory into the pending approval queue.
    pub async fn add_pending_memory(&self, mem: &NewPendingMemory) -> anyhow::Result<i64> {
        let embedding_json = mem
            .embedding
            .as_ref()
            .map(|e| serde_json::to_string(e).unwrap());
        let id = sqlx::query_scalar::<_, i64>(
            r#"INSERT INTO pending_memories (content, source, memory_type, tags, importance, confidence, evidence_session_id, embedding)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
               RETURNING id"#,
        )
        .bind(&mem.content)
        .bind(&mem.source)
        .bind(&mem.memory_type)
        .bind(&mem.tags)
        .bind(mem.importance)
        .bind(mem.confidence)
        .bind(&mem.evidence_session_id)
        .bind(&embedding_json)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// List pending memories awaiting approval.
    pub async fn list_pending_memories(
        &self,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<PendingMemory>> {
        let rows = sqlx::query_as::<_, PendingMemoryRow>(
            "SELECT id, content, source, memory_type, tags, importance, confidence, \
             evidence_session_id, embedding, created_at \
             FROM pending_memories WHERE status = 'pending' \
             ORDER BY created_at DESC LIMIT ?1 OFFSET ?2",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| PendingMemory {
                id: r.id,
                content: r.content,
                source: r.source,
                memory_type: r.memory_type,
                tags: r.tags,
                importance: r.importance,
                confidence: r.confidence,
                evidence_session_id: r.evidence_session_id,
                created_at: r.created_at,
            })
            .collect())
    }

    /// Approve a pending memory — moves it to the real memories table.
    pub async fn approve_pending_memory(&self, pending_id: i64) -> anyhow::Result<i64> {
        let row = sqlx::query_as::<_, PendingMemoryRow>(
            "SELECT id, content, source, memory_type, tags, importance, confidence, \
             evidence_session_id, embedding, created_at \
             FROM pending_memories WHERE id = ?1 AND status = 'pending'",
        )
        .bind(pending_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("Pending memory {pending_id} not found or already processed")
        })?;

        let embedding: Option<Vec<f32>> = row
            .embedding
            .as_ref()
            .and_then(|s| serde_json::from_str(s).ok());
        let new_mem = NewMemory {
            content: row.content,
            source: Some(row.source),
            embedding,
            tags: row.tags,
            memory_type: Some(row.memory_type),
            importance: Some(row.importance),
            strength: Some(1.0),
            supersedes: None,
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: Some("pending-approved".to_string()),
            reflection: None,
            trust_level: Some(TrustLevel::UserStated),
            evidence_session_id: row.evidence_session_id,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: None,
            quality_flag: None,
        };
        let mem_id = self.add_memory(&new_mem).await?;

        sqlx::query("UPDATE pending_memories SET status = 'approved' WHERE id = ?1")
            .bind(pending_id)
            .execute(&self.pool)
            .await?;
        Ok(mem_id)
    }

    /// Reject a pending memory — marks it as rejected (soft delete).
    pub async fn reject_pending_memory(&self, pending_id: i64) -> anyhow::Result<()> {
        let affected = sqlx::query(
            "UPDATE pending_memories SET status = 'rejected' \
             WHERE id = ?1 AND status = 'pending'",
        )
        .bind(pending_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if affected == 0 {
            anyhow::bail!("Pending memory {pending_id} not found or already processed");
        }
        Ok(())
    }

    // ── I15: Workflow CRUD ──────────────────────────────────────

    /// Record a user action in the action log.
    pub async fn log_action(
        &self,
        method: &str,
        param_fingerprint: Option<&str>,
        session_id: Option<&str>,
    ) -> anyhow::Result<i64> {
        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO action_log (method, param_fingerprint, session_id) \
             VALUES (?1, ?2, ?3) RETURNING id",
        )
        .bind(method)
        .bind(param_fingerprint)
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Get the last N actions from the action log.
    pub async fn get_recent_actions(&self, limit: usize) -> anyhow::Result<Vec<WorkflowAction>> {
        let rows = sqlx::query(
            "SELECT method, param_fingerprint FROM action_log \
             ORDER BY id DESC LIMIT ?1",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        use sqlx::Row;
        Ok(rows
            .iter()
            .map(|r| WorkflowAction {
                method: r.get("method"),
                param_fingerprint: r.get("param_fingerprint"),
            })
            .collect())
    }

    /// Add or increment a learned workflow. If a workflow with the same action
    /// sequence already exists, increments its occurrence count and returns the ID.
    /// If count >= 3, marks it as a candidate.
    pub async fn upsert_workflow(&self, wf: &NewWorkflow) -> anyhow::Result<(i64, i64)> {
        let actions_json = serde_json::to_string(&wf.actions).unwrap();

        // Check if same action sequence exists
        let existing =
            sqlx::query("SELECT id, occurrence_count FROM workflows WHERE actions_json = ?1")
                .bind(&actions_json)
                .fetch_optional(&self.pool)
                .await?;

        use sqlx::Row;
        if let Some(row) = existing {
            let id: i64 = row.get("id");
            let count: i64 = row.get("occurrence_count");
            let new_count = count + 1;
            let is_candidate = if new_count >= 3 { 1 } else { 0 };
            sqlx::query(
                "UPDATE workflows SET occurrence_count = ?1, is_candidate = ?2, \
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') WHERE id = ?3",
            )
            .bind(new_count)
            .bind(is_candidate)
            .bind(id)
            .execute(&self.pool)
            .await?;
            Ok((id, new_count))
        } else {
            let id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO workflows (name, actions_json) VALUES (?1, ?2) RETURNING id",
            )
            .bind(&wf.name)
            .bind(&actions_json)
            .fetch_one(&self.pool)
            .await?;
            Ok((id, 1))
        }
    }

    /// List all learned workflows, optionally only candidates (occurrence_count >= 3).
    pub async fn list_workflows(
        &self,
        candidates_only: bool,
    ) -> anyhow::Result<Vec<LearnedWorkflow>> {
        let rows = if candidates_only {
            sqlx::query(
                "SELECT id, name, actions_json, occurrence_count, is_candidate, \
                 created_at, updated_at FROM workflows WHERE is_candidate = 1 \
                 ORDER BY occurrence_count DESC",
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT id, name, actions_json, occurrence_count, is_candidate, \
                 created_at, updated_at FROM workflows ORDER BY occurrence_count DESC",
            )
            .fetch_all(&self.pool)
            .await?
        };

        use sqlx::Row;
        Ok(rows
            .iter()
            .map(|r| {
                let actions_json: String = r.get("actions_json");
                let actions: Vec<WorkflowAction> =
                    serde_json::from_str(&actions_json).unwrap_or_default();
                LearnedWorkflow {
                    id: r.get("id"),
                    name: r.get("name"),
                    actions,
                    occurrence_count: r.get("occurrence_count"),
                    is_candidate: r.get::<i64, _>("is_candidate") != 0,
                    created_at: r.get("created_at"),
                    updated_at: r.get("updated_at"),
                }
            })
            .collect())
    }

    /// Get a single workflow by ID.
    pub async fn get_workflow(&self, id: i64) -> anyhow::Result<Option<LearnedWorkflow>> {
        let row = sqlx::query(
            "SELECT id, name, actions_json, occurrence_count, is_candidate, \
             created_at, updated_at FROM workflows WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        use sqlx::Row;
        Ok(row.map(|r| {
            let actions_json: String = r.get("actions_json");
            let actions: Vec<WorkflowAction> =
                serde_json::from_str(&actions_json).unwrap_or_default();
            LearnedWorkflow {
                id: r.get("id"),
                name: r.get("name"),
                actions,
                occurrence_count: r.get("occurrence_count"),
                is_candidate: r.get::<i64, _>("is_candidate") != 0,
                created_at: r.get("created_at"),
                updated_at: r.get("updated_at"),
            }
        }))
    }

    /// Delete a workflow by ID.
    pub async fn delete_workflow(&self, id: i64) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM workflows WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// Convert a float embedding to a little-endian byte blob for sqlite-vec.
fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> BrainStore {
        BrainStore::open_memory().await.unwrap()
    }

    fn test_memory() -> NewMemory {
        NewMemory {
            content: "Rust is a systems programming language".to_string(),
            source: Some("user".to_string()),
            tags: Some("programming,rust".to_string()),
            embedding: None,
            memory_type: None,
            importance: None,
            strength: None,
            supersedes: None,
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: None,
            reflection: None,
            trust_level: None,
            evidence_session_id: None,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: None,
            quality_flag: None,
        }
    }

    #[tokio::test]
    async fn test_open_memory() {
        let store = test_store().await;
        let count = store.count_memories().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_add_memory() {
        let store = test_store().await;
        let id = store.add_memory(&test_memory()).await.unwrap();
        assert_eq!(id, 1);
        assert_eq!(store.count_memories().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_get_memory() {
        let store = test_store().await;
        let id = store.add_memory(&test_memory()).await.unwrap();

        let mem = store.get_memory(id).await.unwrap().unwrap();
        assert_eq!(mem.content, "Rust is a systems programming language");
        assert_eq!(mem.source.as_deref(), Some("user"));
        assert_eq!(mem.tags.as_deref(), Some("programming,rust"));
        assert_eq!(mem.access_count, 0);
    }

    #[tokio::test]
    async fn test_get_memory_not_found() {
        let store = test_store().await;
        let mem = store.get_memory(999).await.unwrap();
        assert!(mem.is_none());
    }

    #[tokio::test]
    async fn test_all_memories() {
        let store = test_store().await;
        store.add_memory(&test_memory()).await.unwrap();
        store
            .add_memory(&NewMemory {
                content: "Python is interpreted".to_string(),
                source: None,
                tags: None,
                embedding: None,
                memory_type: None,
                importance: None,
                strength: None,
                supersedes: None,
                content_hash: None,
                valid_from: None,
                valid_to: None,
                source_type: None,
                reflection: None,
                trust_level: None,
                evidence_session_id: None,
                evidence_message_id: None,
                scope: None,
                procedure: None,
                quality_score: None,
                quality_flag: None,
            })
            .await
            .unwrap();

        let all = store.all_memories().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_delete_memory() {
        let store = test_store().await;
        let id = store.add_memory(&test_memory()).await.unwrap();

        let deleted = store.delete_memory(id).await.unwrap();
        assert!(deleted);
        assert_eq!(store.count_memories().await.unwrap(), 0);

        // Get should return None for deleted memory
        let mem = store.get_memory(id).await.unwrap();
        assert!(mem.is_none());
    }

    #[tokio::test]
    async fn test_delete_memory_not_found() {
        let store = test_store().await;
        let deleted = store.delete_memory(999).await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test]
    async fn test_update_accessed() {
        let store = test_store().await;
        let id = store.add_memory(&test_memory()).await.unwrap();

        let before = store.get_memory(id).await.unwrap().unwrap();
        assert_eq!(before.access_count, 0);

        store.update_accessed(id).await.unwrap();
        store.update_accessed(id).await.unwrap();

        let after = store.get_memory(id).await.unwrap().unwrap();
        assert_eq!(after.access_count, 2);
    }

    #[tokio::test]
    async fn test_memory_with_embedding() {
        let store = test_store().await;
        let mem = NewMemory {
            content: "test".to_string(),
            source: None,
            tags: None,
            embedding: Some(vec![0.1, 0.2, 0.3, 0.4]),
            memory_type: None,
            importance: None,
            strength: None,
            supersedes: None,
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: None,
            reflection: None,
            trust_level: None,
            evidence_session_id: None,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: None,
            quality_flag: None,
        };
        let id = store.add_memory(&mem).await.unwrap();

        let stored = store.get_memory(id).await.unwrap().unwrap();
        let emb = stored.embedding.unwrap();
        assert_eq!(emb.len(), 4);
        assert!((emb[0] - 0.1).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_get_all_embeddings() {
        let store = test_store().await;
        // One with embedding, one without
        store
            .add_memory(&NewMemory {
                content: "with emb".to_string(),
                source: None,
                tags: None,
                embedding: Some(vec![1.0, 2.0]),
                memory_type: None,
                importance: None,
                strength: None,
                supersedes: None,
                content_hash: None,
                valid_from: None,
                valid_to: None,
                source_type: None,
                reflection: None,
                trust_level: None,
                evidence_session_id: None,
                evidence_message_id: None,
                scope: None,
                procedure: None,
                quality_score: None,
                quality_flag: None,
            })
            .await
            .unwrap();
        store
            .add_memory(&NewMemory {
                content: "no emb".to_string(),
                source: None,
                tags: None,
                embedding: None,
                memory_type: None,
                importance: None,
                strength: None,
                supersedes: None,
                content_hash: None,
                valid_from: None,
                valid_to: None,
                source_type: None,
                reflection: None,
                trust_level: None,
                evidence_session_id: None,
                evidence_message_id: None,
                scope: None,
                procedure: None,
                quality_score: None,
                quality_flag: None,
            })
            .await
            .unwrap();

        let embeddings = store.get_all_embeddings().await.unwrap();
        assert_eq!(embeddings.len(), 1); // only the one with embedding
        assert_eq!(embeddings[0].1, vec![1.0, 2.0]);
    }

    #[tokio::test]
    async fn test_evict_lru() {
        let store = test_store().await;

        // Add 5 memories
        for i in 0..5 {
            store
                .add_memory(&NewMemory {
                    content: format!("memory {i}"),
                    source: None,
                    tags: None,
                    embedding: None,
                    memory_type: None,
                    importance: None,
                    strength: None,
                    supersedes: None,
                    content_hash: None,
                    valid_from: None,
                    valid_to: None,
                    source_type: None,
                    reflection: None,
                    trust_level: None,
                    evidence_session_id: None,
                    evidence_message_id: None,
                    scope: None,
                    procedure: None,
                    quality_score: None,
                    quality_flag: None,
                })
                .await
                .unwrap();
        }
        assert_eq!(store.count_memories().await.unwrap(), 5);

        // Access memories 4 and 5 to make them "recent"
        store.update_accessed(4).await.unwrap();
        store.update_accessed(5).await.unwrap();

        // Evict to keep only 3
        let evicted = store.evict_lru(3).await.unwrap();
        assert_eq!(evicted, 2);
        assert_eq!(store.count_memories().await.unwrap(), 3);

        // The least-recently-accessed (1, 2) should be gone
        assert!(store.get_memory(1).await.unwrap().is_none());
        assert!(store.get_memory(2).await.unwrap().is_none());
        // 3, 4, 5 should remain
        assert!(store.get_memory(3).await.unwrap().is_some());
        assert!(store.get_memory(4).await.unwrap().is_some());
        assert!(store.get_memory(5).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_evict_lru_no_op() {
        let store = test_store().await;
        store.add_memory(&test_memory()).await.unwrap();

        // Keep limit > count — no eviction
        let evicted = store.evict_lru(10).await.unwrap();
        assert_eq!(evicted, 0);
        assert_eq!(store.count_memories().await.unwrap(), 1);
    }

    // ── Identity tests ───────────────────────────────────────────

    #[tokio::test]
    async fn test_set_and_get_identity() {
        let store = test_store().await;
        store
            .set_identity("personality", "I am Ironjaw, a helpful AI assistant")
            .await
            .unwrap();

        let id = store.get_identity("personality").await.unwrap().unwrap();
        assert_eq!(id.key, "personality");
        assert_eq!(id.content, "I am Ironjaw, a helpful AI assistant");
    }

    #[tokio::test]
    async fn test_identity_upsert() {
        let store = test_store().await;
        store.set_identity("rules", "Be helpful").await.unwrap();
        store
            .set_identity("rules", "Be helpful and concise")
            .await
            .unwrap();

        let id = store.get_identity("rules").await.unwrap().unwrap();
        assert_eq!(id.content, "Be helpful and concise");
    }

    #[tokio::test]
    async fn test_get_identity_not_found() {
        let store = test_store().await;
        let id = store.get_identity("nonexistent").await.unwrap();
        assert!(id.is_none());
    }

    #[tokio::test]
    async fn test_get_all_identity() {
        let store = test_store().await;
        store.set_identity("personality", "Helpful").await.unwrap();
        store.set_identity("rules", "Be concise").await.unwrap();
        store.set_identity("preferences", "Use Rust").await.unwrap();

        let all = store.get_all_identity().await.unwrap();
        assert_eq!(all.len(), 3);
        // Ordered by key
        assert_eq!(all[0].key, "personality");
        assert_eq!(all[1].key, "preferences");
        assert_eq!(all[2].key, "rules");
    }

    // ── I20: Scope tests ──────────────────────────────────────

    #[tokio::test]
    async fn test_scope_query() {
        let store = test_store().await;

        // Add memories with different scopes
        store
            .add_memory(&NewMemory {
                content: "Ironjaw uses Rust".to_string(),
                scope: Some("/project/ironjaw".to_string()),
                ..test_memory()
            })
            .await
            .unwrap();
        store
            .add_memory(&NewMemory {
                content: "Ironjaw decision: use axum".to_string(),
                scope: Some("/project/ironjaw/decisions".to_string()),
                ..test_memory()
            })
            .await
            .unwrap();
        store
            .add_memory(&NewMemory {
                content: "JobHunter uses Python".to_string(),
                scope: Some("/project/jobhunter".to_string()),
                ..test_memory()
            })
            .await
            .unwrap();

        // Query by scope prefix
        let ironjaw = store.query_by_scope("/project/ironjaw", 10).await.unwrap();
        assert_eq!(ironjaw.len(), 2);

        let decisions = store
            .query_by_scope("/project/ironjaw/decisions", 10)
            .await
            .unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].content, "Ironjaw decision: use axum");

        let jobhunter = store
            .query_by_scope("/project/jobhunter", 10)
            .await
            .unwrap();
        assert_eq!(jobhunter.len(), 1);
    }

    #[tokio::test]
    async fn test_set_scope() {
        let store = test_store().await;
        let id = store.add_memory(&test_memory()).await.unwrap();

        // Memory starts without scope
        let mem = store.get_memory(id).await.unwrap().unwrap();
        assert!(mem.scope.is_none());

        // Set scope
        store.set_scope(id, "/project/test").await.unwrap();
        let mem = store.get_memory(id).await.unwrap().unwrap();
        assert_eq!(mem.scope.as_deref(), Some("/project/test"));
    }

    // ── I19: Procedure tests ──────────────────────────────────

    #[tokio::test]
    async fn test_procedure_memory() {
        use crate::types::ProcedureMeta;

        let store = test_store().await;
        store
            .add_memory(&NewMemory {
                content: "How to deploy Ironjaw".to_string(),
                memory_type: Some("procedure".to_string()),
                importance: Some(0.8),
                procedure: Some(ProcedureMeta {
                    trigger: "deploy ironjaw".to_string(),
                    steps: vec![
                        "cargo build --release".to_string(),
                        "scp target/release/ironjaw workstation:".to_string(),
                        "schtasks /Run /TN Ironjaw".to_string(),
                    ],
                    prerequisites: vec!["Rust toolchain installed".to_string()],
                    output_format: Some("shell commands".to_string()),
                }),
                ..test_memory()
            })
            .await
            .unwrap();

        let procedures = store.query_procedures(None, 10).await.unwrap();
        assert_eq!(procedures.len(), 1);
        assert_eq!(procedures[0].memory_type, "procedure");
        assert!(procedures[0].procedure.is_some());

        let proc = procedures[0].procedure.as_ref().unwrap();
        assert_eq!(proc.steps.len(), 3);
        assert_eq!(proc.trigger, "deploy ironjaw");
    }

    #[tokio::test]
    async fn test_procedure_query_with_scope() {
        use crate::types::ProcedureMeta;

        let store = test_store().await;
        store
            .add_memory(&NewMemory {
                content: "Deploy Ironjaw procedure".to_string(),
                memory_type: Some("procedure".to_string()),
                scope: Some("/project/ironjaw".to_string()),
                procedure: Some(ProcedureMeta {
                    trigger: "deploy".to_string(),
                    steps: vec!["build".to_string(), "deploy".to_string()],
                    prerequisites: vec![],
                    output_format: None,
                }),
                ..test_memory()
            })
            .await
            .unwrap();
        store
            .add_memory(&NewMemory {
                content: "Deploy JobHunter procedure".to_string(),
                memory_type: Some("procedure".to_string()),
                scope: Some("/project/jobhunter".to_string()),
                procedure: Some(ProcedureMeta {
                    trigger: "deploy".to_string(),
                    steps: vec!["test".to_string(), "push".to_string()],
                    prerequisites: vec![],
                    output_format: None,
                }),
                ..test_memory()
            })
            .await
            .unwrap();

        // Query procedures filtered by scope
        let ironjaw = store
            .query_procedures(Some("/project/ironjaw"), 10)
            .await
            .unwrap();
        assert_eq!(ironjaw.len(), 1);
        assert!(ironjaw[0].content.contains("Ironjaw"));

        // No scope filter returns all
        let all = store.query_procedures(None, 10).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    // ── I15: Workflow tests ──────────────────────────────────

    #[tokio::test]
    async fn test_action_log() {
        let store = test_store().await;

        store
            .log_action("brain.remember", None, Some("sess-1"))
            .await
            .unwrap();
        store
            .log_action("brain.recall", Some("query=test"), Some("sess-1"))
            .await
            .unwrap();
        store
            .log_action("tools.execute", None, Some("sess-1"))
            .await
            .unwrap();

        let recent = store.get_recent_actions(10).await.unwrap();
        assert_eq!(recent.len(), 3);
        // Most recent first
        assert_eq!(recent[0].method, "tools.execute");
        assert_eq!(recent[1].method, "brain.recall");
    }

    #[tokio::test]
    async fn test_workflow_upsert() {
        use crate::types::WorkflowAction;

        let store = test_store().await;

        let actions = vec![
            WorkflowAction {
                method: "brain.recall".to_string(),
                param_fingerprint: None,
            },
            WorkflowAction {
                method: "tools.execute".to_string(),
                param_fingerprint: None,
            },
            WorkflowAction {
                method: "brain.remember".to_string(),
                param_fingerprint: None,
            },
        ];

        let wf = NewWorkflow {
            name: "recall -> execute -> remember".to_string(),
            actions: actions.clone(),
        };

        // First insert
        let (id, count) = store.upsert_workflow(&wf).await.unwrap();
        assert_eq!(count, 1);

        // Second occurrence
        let (id2, count2) = store.upsert_workflow(&wf).await.unwrap();
        assert_eq!(id2, id);
        assert_eq!(count2, 2);

        // Third occurrence -> becomes candidate
        let (_, count3) = store.upsert_workflow(&wf).await.unwrap();
        assert_eq!(count3, 3);

        // Verify it's now a candidate
        let candidates = store.list_workflows(true).await.unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].is_candidate);
        assert_eq!(candidates[0].occurrence_count, 3);
    }

    #[tokio::test]
    async fn test_workflow_get_and_delete() {
        use crate::types::WorkflowAction;

        let store = test_store().await;

        let wf = NewWorkflow {
            name: "test workflow".to_string(),
            actions: vec![WorkflowAction {
                method: "test.method".to_string(),
                param_fingerprint: None,
            }],
        };

        let (id, _) = store.upsert_workflow(&wf).await.unwrap();

        // Get by ID
        let fetched = store.get_workflow(id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "test workflow");
        assert_eq!(fetched.actions.len(), 1);

        // Delete
        assert!(store.delete_workflow(id).await.unwrap());
        assert!(store.get_workflow(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_list_workflows_all_vs_candidates() {
        use crate::types::WorkflowAction;

        let store = test_store().await;

        let wf1 = NewWorkflow {
            name: "wf1".to_string(),
            actions: vec![WorkflowAction {
                method: "a".to_string(),
                param_fingerprint: None,
            }],
        };
        let wf2 = NewWorkflow {
            name: "wf2".to_string(),
            actions: vec![WorkflowAction {
                method: "b".to_string(),
                param_fingerprint: None,
            }],
        };

        // wf1: 1 occurrence (not candidate)
        store.upsert_workflow(&wf1).await.unwrap();

        // wf2: 3 occurrences (candidate)
        store.upsert_workflow(&wf2).await.unwrap();
        store.upsert_workflow(&wf2).await.unwrap();
        store.upsert_workflow(&wf2).await.unwrap();

        let all = store.list_workflows(false).await.unwrap();
        assert_eq!(all.len(), 2);

        let candidates = store.list_workflows(true).await.unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].name, "wf2");
    }

    // ── Pending Memory tests ─────────────────────────────────

    #[tokio::test]
    async fn pending_memory_roundtrip() {
        let store = test_store().await;
        let pending = NewPendingMemory {
            content: "test pending fact".to_string(),
            source: "extraction-pipeline".to_string(),
            memory_type: "fact".to_string(),
            tags: Some("test".to_string()),
            importance: 0.5,
            confidence: 0.7,
            evidence_session_id: Some("sess-1".to_string()),
            embedding: None,
        };
        let id = store.add_pending_memory(&pending).await.unwrap();
        assert!(id > 0);
        let list = store.list_pending_memories(50, 0).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].content, "test pending fact");
        assert_eq!(list[0].source, "extraction-pipeline");

        // Approve -> should move to real memories
        let mem_id = store.approve_pending_memory(id).await.unwrap();
        assert!(mem_id > 0);
        let list = store.list_pending_memories(50, 0).await.unwrap();
        assert!(list.is_empty());
        let mem = store.get_memory(mem_id).await.unwrap();
        assert!(mem.is_some());
        assert_eq!(mem.unwrap().content, "test pending fact");
    }

    #[tokio::test]
    async fn reject_pending_memory() {
        let store = test_store().await;
        let pending = NewPendingMemory {
            content: "hallucinated garbage".to_string(),
            source: "auto-learn".to_string(),
            memory_type: "correction".to_string(),
            tags: None,
            importance: 0.3,
            confidence: 0.4,
            evidence_session_id: None,
            embedding: None,
        };
        let id = store.add_pending_memory(&pending).await.unwrap();
        store.reject_pending_memory(id).await.unwrap();
        let list = store.list_pending_memories(50, 0).await.unwrap();
        assert!(list.is_empty());
    }

    // ── Embedding cache (VecIndex) tests ─────────────────────────

    fn make_mem_with_embedding(content: &str, emb: Vec<f32>) -> NewMemory {
        NewMemory {
            content: content.to_string(),
            embedding: Some(emb),
            source: None,
            tags: None,
            memory_type: None,
            importance: None,
            strength: None,
            supersedes: None,
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: None,
            reflection: None,
            trust_level: None,
            evidence_session_id: None,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: None,
            quality_flag: None,
        }
    }

    /// Cache populated on init: after inserting embeddings and calling
    /// `init_vec_index`, the VecIndex is populated — mirroring what
    /// `auto_init_vec_index` does on startup when the DB already has embeddings.
    #[tokio::test]
    async fn test_vec_cache_populated_on_init() {
        let mut store = test_store().await;
        // Fresh empty store → auto_init skipped (no embeddings in DB)
        assert!(store.vec_index.is_none());

        // Insert two memories with embeddings (direct DB insert, bypassing cache sync)
        store
            .add_memory(&make_mem_with_embedding(
                "first fact",
                vec![1.0_f32, 0.0, 0.0],
            ))
            .await
            .unwrap();
        store
            .add_memory(&make_mem_with_embedding(
                "second fact",
                vec![0.0_f32, 1.0, 0.0],
            ))
            .await
            .unwrap();

        // Simulate what auto_init_vec_index does on next startup with pre-existing data
        let loaded = store.init_vec_index(3).await.unwrap();

        assert_eq!(
            loaded, 2,
            "init_vec_index should load both stored embeddings"
        );
        let idx = store
            .vec_index
            .as_ref()
            .expect("VecIndex should be Some after init_vec_index");
        assert_eq!(
            idx.len(),
            2,
            "VecIndex should contain both pre-loaded embeddings"
        );
    }

    /// Cache hit avoids SQL: after init_vec_index, knn_search uses the fast
    /// in-memory path and returns the correct top result.
    #[tokio::test]
    async fn test_vec_cache_hit_uses_fast_path() {
        let mut store = test_store().await;

        let embedding = vec![1.0_f32, 0.0, 0.0, 0.0];
        let id = store
            .add_memory(&make_mem_with_embedding(
                "fast path test",
                embedding.clone(),
            ))
            .await
            .unwrap();

        // Explicitly initialize VecIndex (simulates startup with pre-existing data)
        store.init_vec_index(4).await.unwrap();
        assert!(
            store.vec_index.is_some(),
            "VecIndex must be Some for fast-path test to be valid"
        );

        let results = store.knn_search(&embedding, 5).await.unwrap();
        assert!(!results.is_empty(), "knn_search must return results");
        assert_eq!(results[0].0, id, "Top result should be the inserted memory");
        assert!(
            (results[0].1 - 1.0).abs() < 1e-5,
            "Cosine similarity of identical vectors should be ~1.0"
        );
    }

    /// Cache invalidation on insert: after VecIndex is initialized, each subsequent
    /// `add_memory` with an embedding grows VecIndex via the write-path sync.
    #[tokio::test]
    async fn test_vec_cache_invalidation_on_insert() {
        let mut store = test_store().await;

        // Seed one entry and init the cache
        store
            .add_memory(&make_mem_with_embedding("seed", vec![0.5_f32, 0.5]))
            .await
            .unwrap();
        store.init_vec_index(2).await.unwrap();
        let idx = store
            .vec_index
            .as_ref()
            .expect("VecIndex should be Some after init_vec_index");
        assert_eq!(idx.len(), 1, "One seed entry loaded");

        // Each subsequent insert should be reflected immediately in VecIndex
        store
            .add_memory(&make_mem_with_embedding("second", vec![0.0_f32, 1.0]))
            .await
            .unwrap();
        assert_eq!(idx.len(), 2, "VecIndex should grow after second insert");

        store
            .add_memory(&make_mem_with_embedding("third", vec![1.0_f32, 0.0]))
            .await
            .unwrap();
        assert_eq!(idx.len(), 3, "VecIndex should grow after third insert");
    }
}
