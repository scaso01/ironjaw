use crate::types::{NewMessage, Session, SessionMessage, SessionMode};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::path::Path;
use tracing::info;

/// Persistent session store backed by SQLite
#[derive(Clone)]
pub struct SessionStore {
    pool: SqlitePool,
}

impl SessionStore {
    /// Open or create the session database at the given path.
    /// Runs migrations automatically.
    pub async fn open(db_path: &str) -> anyhow::Result<Self> {
        if let Some(parent) = Path::new(db_path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        let url = format!("sqlite:{db_path}?mode=rwc");
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await?;

        Self::run_migrations(&pool).await?;

        info!(path = db_path, "Session store opened");
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

    async fn run_migrations(pool: &SqlitePool) -> anyhow::Result<()> {
        // WAL mode must be set via a separate pragma call (not inside CREATE TABLE)
        sqlx::query("PRAGMA journal_mode=WAL").execute(pool).await?;

        sqlx::query(include_str!("../migrations/001_create_sessions.sql"))
            .execute(pool)
            .await?;

        // Migration 002: add pinned and archived columns
        sqlx::query("ALTER TABLE sessions ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0")
            .execute(pool)
            .await
            .ok(); // Ignore if column already exists

        sqlx::query("ALTER TABLE sessions ADD COLUMN archived INTEGER NOT NULL DEFAULT 0")
            .execute(pool)
            .await
            .ok(); // Ignore if column already exists

        // Migration 003: add tool_calls and tool_call_id columns to messages
        sqlx::query("ALTER TABLE messages ADD COLUMN tool_calls TEXT")
            .execute(pool)
            .await
            .ok(); // Ignore if column already exists

        sqlx::query("ALTER TABLE messages ADD COLUMN tool_call_id TEXT")
            .execute(pool)
            .await
            .ok(); // Ignore if column already exists

        // Migration 004: expand role CHECK constraint to include 'tool' role.
        // SQLite does not support ALTER TABLE ... ALTER COLUMN, so we use the
        // recommended rename-create-copy-drop sequence wrapped in a transaction
        // for atomicity.  The migration is guarded: we check whether 'tool' is
        // already allowed before doing any DDL so the migration is a no-op on
        // databases that already have the expanded constraint.
        let needs_migration: bool = {
            // Try inserting a sentinel row with role='tool' into a temp table
            // to probe the constraint.  Cheaper and safer: just check the
            // sqlite_master schema text for the old constraint string.
            let schema: Option<String> = sqlx::query_scalar(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='messages'",
            )
            .fetch_optional(pool)
            .await?;
            match schema {
                Some(s) => !s.contains("'tool'"),
                None => false, // table doesn't exist yet — nothing to migrate
            }
        };

        if needs_migration {
            // Perform the rename-create-copy-drop inside a transaction.
            let mut tx = pool.begin().await?;

            sqlx::query(
                "CREATE TABLE messages_new (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id  TEXT NOT NULL REFERENCES sessions(id),
                    role        TEXT NOT NULL CHECK (role IN ('system', 'user', 'assistant', 'tool')),
                    content     TEXT NOT NULL,
                    model       TEXT,
                    tokens      INTEGER,
                    complete    INTEGER NOT NULL DEFAULT 1,
                    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
                    tool_calls  TEXT,
                    tool_call_id TEXT
                )",
            )
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                "INSERT INTO messages_new
                 (id, session_id, role, content, model, tokens, complete, created_at, tool_calls, tool_call_id)
                 SELECT id, session_id, role, content, model, tokens, complete, created_at,
                        tool_calls, tool_call_id
                 FROM messages",
            )
            .execute(&mut *tx)
            .await?;

            sqlx::query("DROP TABLE messages").execute(&mut *tx).await?;
            sqlx::query("ALTER TABLE messages_new RENAME TO messages")
                .execute(&mut *tx)
                .await?;

            tx.commit().await?;

            // Recreate indexes (outside the transaction is fine for indexes).
            sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id)")
                .execute(pool)
                .await?;
        }

        // Migration 005: add session mode column
        sqlx::query("ALTER TABLE sessions ADD COLUMN mode TEXT NOT NULL DEFAULT 'default'")
            .execute(pool)
            .await
            .ok(); // Ignore if column already exists

        // Migration 006 (Wave C / C1): per-session working_dir override.
        // Nullable TEXT — None means "fall back to [tools] config or current_dir".
        sqlx::query("ALTER TABLE sessions ADD COLUMN working_dir TEXT")
            .execute(pool)
            .await
            .ok(); // Ignore if column already exists

        // Migration 007: cumulative per-session LLM usage. Persisted after each
        // turn so clients can show lifetime session usage across restarts
        // (in-memory client counters only ever saw turns they were awake for).
        sqlx::query("ALTER TABLE sessions ADD COLUMN input_tokens INTEGER NOT NULL DEFAULT 0")
            .execute(pool)
            .await
            .ok();
        sqlx::query("ALTER TABLE sessions ADD COLUMN output_tokens INTEGER NOT NULL DEFAULT 0")
            .execute(pool)
            .await
            .ok();

        Ok(())
    }

    // -- Session CRUD --

    /// Create a new session, returning it
    pub async fn create_session(
        &self,
        id: &str,
        user_id: &str,
        title: Option<&str>,
    ) -> anyhow::Result<Session> {
        sqlx::query("INSERT INTO sessions (id, user_id, title) VALUES (?1, ?2, ?3)")
            .bind(id)
            .bind(user_id)
            .bind(title)
            .execute(&self.pool)
            .await?;

        self.get_session(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Session not found after insert"))
    }

    /// Get a session by ID (excluding soft-deleted)
    pub async fn get_session(&self, id: &str) -> anyhow::Result<Option<Session>> {
        let row = sqlx::query(
            "SELECT s.id, s.user_id, s.title, s.created_at, s.updated_at, \
             s.context_tokens, s.compaction_count, s.input_tokens, s.output_tokens, \
             s.pinned, s.archived, s.mode, s.working_dir, \
             (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id) as message_count \
             FROM sessions s WHERE s.id = ?1 AND s.deleted = 0",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| {
            use sqlx::Row;
            let pinned_int: i64 = r.get("pinned");
            let archived_int: i64 = r.get("archived");
            let mode_str: String = r.try_get("mode").unwrap_or_default();
            let mode = serde_json::from_value(serde_json::Value::String(mode_str))
                .unwrap_or(SessionMode::Default);
            let working_dir: Option<String> = r.try_get("working_dir").ok().flatten();
            Session {
                id: r.get("id"),
                user_id: r.get("user_id"),
                title: r.get("title"),
                created_at: r.get("created_at"),
                updated_at: r.get("updated_at"),
                context_tokens: r.get("context_tokens"),
                compaction_count: r.get("compaction_count"),
                input_tokens: r.get("input_tokens"),
                output_tokens: r.get("output_tokens"),
                message_count: r.get("message_count"),
                pinned: pinned_int != 0,
                archived: archived_int != 0,
                mode,
                working_dir,
            }
        }))
    }

    /// List sessions (most recently updated first), excluding soft-deleted.
    /// Pinned sessions sort first, then by updated_at descending.
    pub async fn list_sessions(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<Session>> {
        let rows = sqlx::query(
            "SELECT s.id, s.user_id, s.title, s.created_at, s.updated_at, \
             s.context_tokens, s.compaction_count, s.input_tokens, s.output_tokens, \
             s.pinned, s.archived, s.mode, s.working_dir, \
             (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id) as message_count \
             FROM sessions s WHERE s.deleted = 0 \
             ORDER BY s.pinned DESC, s.updated_at DESC LIMIT ?1 OFFSET ?2",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                use sqlx::Row;
                let pinned_int: i64 = r.get("pinned");
                let archived_int: i64 = r.get("archived");
                let mode_str: String = r.try_get("mode").unwrap_or_default();
                let mode = serde_json::from_value(serde_json::Value::String(mode_str))
                    .unwrap_or(SessionMode::Default);
                let working_dir: Option<String> = r.try_get("working_dir").ok().flatten();
                Session {
                    id: r.get("id"),
                    user_id: r.get("user_id"),
                    title: r.get("title"),
                    created_at: r.get("created_at"),
                    updated_at: r.get("updated_at"),
                    context_tokens: r.get("context_tokens"),
                    compaction_count: r.get("compaction_count"),
                    input_tokens: r.get("input_tokens"),
                    output_tokens: r.get("output_tokens"),
                    message_count: r.get("message_count"),
                    pinned: pinned_int != 0,
                    archived: archived_int != 0,
                    mode,
                    working_dir,
                }
            })
            .collect())
    }

    /// Count non-deleted sessions
    pub async fn count_sessions(&self) -> anyhow::Result<i64> {
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions WHERE deleted = 0")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// Soft-delete a session
    pub async fn delete_session(&self, id: &str) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE sessions SET deleted = 1, updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') WHERE id = ?1 AND deleted = 0",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Rename a session
    pub async fn rename_session(&self, id: &str, title: &str) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE sessions SET title = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') WHERE id = ?1 AND deleted = 0",
        )
        .bind(id)
        .bind(title)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Pin or unpin a session
    pub async fn pin_session(&self, id: &str, pinned: bool) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE sessions SET pinned = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') WHERE id = ?1 AND deleted = 0",
        )
        .bind(id)
        .bind(pinned as i64)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Archive or unarchive a session
    pub async fn archive_session(&self, id: &str, archived: bool) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE sessions SET archived = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') WHERE id = ?1 AND deleted = 0",
        )
        .bind(id)
        .bind(archived as i64)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Get the HITL mode for a session.
    pub async fn get_mode(&self, id: &str) -> anyhow::Result<Option<SessionMode>> {
        let row = sqlx::query_scalar::<_, String>(
            "SELECT mode FROM sessions WHERE id = ?1 AND deleted = 0",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|s| {
            serde_json::from_value(serde_json::Value::String(s)).unwrap_or(SessionMode::Default)
        }))
    }

    /// Set the HITL mode for a session. Returns false if the session was not found.
    pub async fn set_mode(&self, id: &str, mode: &SessionMode) -> anyhow::Result<bool> {
        let mode_str = serde_json::to_value(mode)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_owned()))
            .unwrap_or_else(|| "default".to_owned());
        let result = sqlx::query(
            "UPDATE sessions SET mode = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') WHERE id = ?1 AND deleted = 0",
        )
        .bind(id)
        .bind(mode_str)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// C1: Set the per-session working directory override. Pass `None` to
    /// clear the override and fall back to the `[tools]` config / current_dir.
    /// Returns false if the session was not found.
    pub async fn set_working_dir(
        &self,
        id: &str,
        working_dir: Option<&str>,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE sessions SET working_dir = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') WHERE id = ?1 AND deleted = 0",
        )
        .bind(id)
        .bind(working_dir)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Touch session updated_at timestamp
    pub async fn touch_session(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE sessions SET updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') WHERE id = ?1",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Accumulate per-turn LLM usage onto the session's lifetime counters.
    /// Does not touch updated_at — the turn's update_context_tokens already did.
    pub async fn add_usage_tokens(&self, id: &str, input: i64, output: i64) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE sessions SET input_tokens = input_tokens + ?2, \
             output_tokens = output_tokens + ?3 WHERE id = ?1",
        )
        .bind(id)
        .bind(input)
        .bind(output)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update context_tokens for a session
    pub async fn update_context_tokens(&self, id: &str, tokens: i64) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE sessions SET context_tokens = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') WHERE id = ?1",
        )
        .bind(id)
        .bind(tokens)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // -- Message CRUD --

    /// Add a message to a session, returning its ID
    pub async fn add_message(&self, msg: &NewMessage) -> anyhow::Result<i64> {
        // Serialize tool_calls as a JSON string; NULL when None.
        let tool_calls_json: Option<String> = msg
            .tool_calls
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO messages (session_id, role, content, model, tokens, complete, tool_calls, tool_call_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) RETURNING id",
        )
        .bind(&msg.session_id)
        .bind(&msg.role)
        .bind(&msg.content)
        .bind(&msg.model)
        .bind(msg.tokens)
        .bind(msg.complete)
        .bind(&tool_calls_json)
        .bind(&msg.tool_call_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Persist a message with WAL checkpoint — blocks until durably committed.
    /// Use for user messages to enable crash recovery.
    pub async fn add_message_sync(&self, msg: &NewMessage) -> anyhow::Result<i64> {
        let id = self.add_message(msg).await?;

        // Force WAL checkpoint to ensure durability
        sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
            .execute(&self.pool)
            .await?;

        Ok(id)
    }

    /// Record a tool denial as a system message in the conversation.
    /// This enables the LLM to see the denial and adjust its strategy.
    pub async fn add_denial(
        &self,
        session_id: &str,
        tool_name: &str,
        args: &str,
        reason: &str,
    ) -> anyhow::Result<i64> {
        let content = format!(
            "[TOOL DENIED] Tool `{tool_name}` was denied by the user.\n\
             Arguments: {args}\n\
             Reason: {reason}\n\
             Adjust your approach — do not retry the same operation."
        );
        self.add_message(&NewMessage {
            session_id: session_id.to_string(),
            role: "system".to_string(),
            content,
            model: None,
            tokens: None,
            complete: true,
            tool_calls: None,
            tool_call_id: None,
        })
        .await
    }

    /// Get messages for a session with pagination (cursor-based via before_id)
    pub async fn get_messages(
        &self,
        session_id: &str,
        limit: i64,
        before_id: Option<i64>,
    ) -> anyhow::Result<Vec<SessionMessage>> {
        let rows = if let Some(before) = before_id {
            sqlx::query(
                "SELECT id, session_id, role, content, model, tokens, complete, created_at, \
                 tool_calls, tool_call_id \
                 FROM messages WHERE session_id = ?1 AND id < ?2 \
                 ORDER BY id DESC LIMIT ?3",
            )
            .bind(session_id)
            .bind(before)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT id, session_id, role, content, model, tokens, complete, created_at, \
                 tool_calls, tool_call_id \
                 FROM messages WHERE session_id = ?1 \
                 ORDER BY id DESC LIMIT ?2",
            )
            .bind(session_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };

        // Reverse to chronological order
        let mut messages: Vec<SessionMessage> = rows
            .into_iter()
            .map(|r| {
                use sqlx::Row;
                let tool_calls_json: Option<String> = r.get("tool_calls");
                let tool_calls = tool_calls_json
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .unwrap_or(None);
                SessionMessage {
                    id: r.get("id"),
                    session_id: r.get("session_id"),
                    role: r.get("role"),
                    content: r.get("content"),
                    model: r.get("model"),
                    tokens: r.get("tokens"),
                    complete: r.get("complete"),
                    created_at: r.get("created_at"),
                    tool_calls,
                    tool_call_id: r.get("tool_call_id"),
                }
            })
            .collect();
        messages.reverse();
        Ok(messages)
    }

    /// Get last N messages for a session in chronological order
    pub async fn recent_messages(
        &self,
        session_id: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<SessionMessage>> {
        self.get_messages(session_id, limit, None).await
    }

    /// Search messages by content (case-insensitive LIKE query).
    /// If `session_id` is provided, filters to that session only.
    pub async fn search_messages(
        &self,
        query: &str,
        session_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<SessionMessage>> {
        let rows = if let Some(sid) = session_id {
            sqlx::query(
                "SELECT id, session_id, role, content, model, tokens, complete, created_at, \
                 tool_calls, tool_call_id \
                 FROM messages WHERE session_id = ?1 AND content LIKE '%' || ?2 || '%' \
                 ORDER BY created_at DESC LIMIT ?3",
            )
            .bind(sid)
            .bind(query)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT id, session_id, role, content, model, tokens, complete, created_at, \
                 tool_calls, tool_call_id \
                 FROM messages WHERE content LIKE '%' || ?1 || '%' \
                 ORDER BY created_at DESC LIMIT ?2",
            )
            .bind(query)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows
            .into_iter()
            .map(|r| {
                use sqlx::Row;
                let tool_calls_json: Option<String> = r.get("tool_calls");
                let tool_calls = tool_calls_json
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .unwrap_or(None);
                SessionMessage {
                    id: r.get("id"),
                    session_id: r.get("session_id"),
                    role: r.get("role"),
                    content: r.get("content"),
                    model: r.get("model"),
                    tokens: r.get("tokens"),
                    complete: r.get("complete"),
                    created_at: r.get("created_at"),
                    tool_calls,
                    tool_call_id: r.get("tool_call_id"),
                }
            })
            .collect())
    }

    /// Clone all messages from one session to another. Used for fork-with-cache-sharing.
    /// The child session must already exist. Returns the number of messages copied.
    pub async fn clone_session_messages(
        &self,
        parent_session_id: &str,
        child_session_id: &str,
    ) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "INSERT INTO messages (session_id, role, content, model, tokens, complete, created_at) \
             SELECT ?2, role, content, model, tokens, complete, created_at \
             FROM messages WHERE session_id = ?1 ORDER BY id ASC",
        )
        .bind(parent_session_id)
        .bind(child_session_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    /// Count messages in a session
    pub async fn message_count(&self, session_id: &str) -> anyhow::Result<i64> {
        let count =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages WHERE session_id = ?1")
                .bind(session_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> SessionStore {
        SessionStore::open_memory().await.unwrap()
    }

    #[tokio::test]
    async fn test_open_memory() {
        let store = test_store().await;
        let count = store.count_sessions().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_wal_mode() {
        // WAL mode only works with file-based databases, not in-memory
        let dir = std::env::temp_dir().join("ironjaw-test-wal");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test.db");
        let store = SessionStore::open(db_path.to_str().unwrap()).await.unwrap();
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        // cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_create_session() {
        let store = test_store().await;
        let session = store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();
        assert_eq!(session.id, "sess-1");
        assert_eq!(session.user_id, "default");
        assert!(session.title.is_none());
        assert_eq!(session.context_tokens, 0);
        assert_eq!(session.compaction_count, 0);
    }

    #[tokio::test]
    async fn test_create_session_with_title() {
        let store = test_store().await;
        let session = store
            .create_session("sess-1", "default", Some("My Chat"))
            .await
            .unwrap();
        assert_eq!(session.title.as_deref(), Some("My Chat"));
    }

    #[tokio::test]
    async fn test_get_session() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();

        let found = store.get_session("sess-1").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "sess-1");

        let not_found = store.get_session("nonexistent").await.unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_list_sessions() {
        let store = test_store().await;
        for i in 0..5 {
            store
                .create_session(&format!("sess-{i}"), "default", None)
                .await
                .unwrap();
        }

        let all = store.list_sessions(100, 0).await.unwrap();
        assert_eq!(all.len(), 5);

        let page = store.list_sessions(2, 0).await.unwrap();
        assert_eq!(page.len(), 2);

        let page2 = store.list_sessions(2, 2).await.unwrap();
        assert_eq!(page2.len(), 2);
    }

    #[tokio::test]
    async fn test_delete_session() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();
        assert_eq!(store.count_sessions().await.unwrap(), 1);

        let deleted = store.delete_session("sess-1").await.unwrap();
        assert!(deleted);
        assert_eq!(store.count_sessions().await.unwrap(), 0);

        // get_session should not find deleted session
        let found = store.get_session("sess-1").await.unwrap();
        assert!(found.is_none());

        // Double-delete returns false
        let deleted2 = store.delete_session("sess-1").await.unwrap();
        assert!(!deleted2);
    }

    #[tokio::test]
    async fn test_rename_session() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();

        let renamed = store.rename_session("sess-1", "New Title").await.unwrap();
        assert!(renamed);

        let session = store.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(session.title.as_deref(), Some("New Title"));
    }

    #[tokio::test]
    async fn test_update_context_tokens() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();

        store.update_context_tokens("sess-1", 5000).await.unwrap();
        let session = store.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(session.context_tokens, 5000);
    }

    #[tokio::test]
    async fn test_add_usage_tokens_accumulates() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();

        let session = store.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(session.input_tokens, 0);
        assert_eq!(session.output_tokens, 0);

        store.add_usage_tokens("sess-1", 1200, 300).await.unwrap();
        store.add_usage_tokens("sess-1", 800, 150).await.unwrap();

        let session = store.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(session.input_tokens, 2000);
        assert_eq!(session.output_tokens, 450);

        // list_sessions must carry the same columns
        let listed = store.list_sessions(10, 0).await.unwrap();
        assert_eq!(listed[0].input_tokens, 2000);
        assert_eq!(listed[0].output_tokens, 450);
    }

    #[tokio::test]
    async fn test_add_message() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();

        let msg = NewMessage::new("sess-1", "user", "Hello!");
        let id = store.add_message(&msg).await.unwrap();
        assert_eq!(id, 1);

        let count = store.message_count("sess-1").await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_add_message_incomplete() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();

        let msg = NewMessage {
            session_id: "sess-1".to_string(),
            role: "assistant".to_string(),
            content: "partial response...".to_string(),
            model: Some("qwen3-coder".to_string()),
            tokens: Some(10),
            complete: false,
            tool_calls: None,
            tool_call_id: None,
        };
        store.add_message(&msg).await.unwrap();

        let messages = store.recent_messages("sess-1", 10).await.unwrap();
        assert_eq!(messages.len(), 1);
        assert!(!messages[0].complete);
        assert_eq!(messages[0].model.as_deref(), Some("qwen3-coder"));
        assert_eq!(messages[0].tokens, Some(10));
    }

    #[tokio::test]
    async fn test_get_messages_chronological() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();

        for i in 0..5 {
            let msg = NewMessage::new("sess-1", "user", format!("msg-{i}"));
            store.add_message(&msg).await.unwrap();
        }

        let messages = store.recent_messages("sess-1", 10).await.unwrap();
        assert_eq!(messages.len(), 5);
        // Should be in chronological order
        assert_eq!(messages[0].content, "msg-0");
        assert_eq!(messages[4].content, "msg-4");
    }

    #[tokio::test]
    async fn test_get_messages_pagination() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();

        for i in 0..10 {
            let msg = NewMessage::new("sess-1", "user", format!("msg-{i}"));
            store.add_message(&msg).await.unwrap();
        }

        // Get last 3 messages
        let page1 = store.get_messages("sess-1", 3, None).await.unwrap();
        assert_eq!(page1.len(), 3);
        assert_eq!(page1[0].content, "msg-7");
        assert_eq!(page1[2].content, "msg-9");

        // Get 3 before the first of page1
        let page2 = store
            .get_messages("sess-1", 3, Some(page1[0].id))
            .await
            .unwrap();
        assert_eq!(page2.len(), 3);
        assert_eq!(page2[0].content, "msg-4");
        assert_eq!(page2[2].content, "msg-6");
    }

    #[tokio::test]
    async fn test_message_count_across_sessions() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();
        store
            .create_session("sess-2", "default", None)
            .await
            .unwrap();

        store
            .add_message(&NewMessage::new("sess-1", "user", "a"))
            .await
            .unwrap();
        store
            .add_message(&NewMessage::new("sess-1", "user", "b"))
            .await
            .unwrap();
        store
            .add_message(&NewMessage::new("sess-2", "user", "c"))
            .await
            .unwrap();

        assert_eq!(store.message_count("sess-1").await.unwrap(), 2);
        assert_eq!(store.message_count("sess-2").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_session_message_count_field() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();

        store
            .add_message(&NewMessage::new("sess-1", "user", "hello"))
            .await
            .unwrap();
        store
            .add_message(&NewMessage::new("sess-1", "assistant", "hi"))
            .await
            .unwrap();

        let session = store.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(session.message_count, Some(2));
    }

    #[tokio::test]
    async fn test_touch_session() {
        let store = test_store().await;
        let session = store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();
        let original_updated = session.updated_at.clone();

        // Small delay to ensure timestamp changes
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        store.touch_session("sess-1").await.unwrap();
        let updated = store.get_session("sess-1").await.unwrap().unwrap();
        assert_ne!(updated.updated_at, original_updated);
    }

    #[tokio::test]
    async fn test_search_messages_all_sessions() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();
        store
            .create_session("sess-2", "default", None)
            .await
            .unwrap();

        store
            .add_message(&NewMessage::new("sess-1", "user", "hello world"))
            .await
            .unwrap();
        store
            .add_message(&NewMessage::new("sess-1", "assistant", "hi there"))
            .await
            .unwrap();
        store
            .add_message(&NewMessage::new("sess-2", "user", "hello again"))
            .await
            .unwrap();

        let results = store.search_messages("hello", None, 50).await.unwrap();
        assert_eq!(results.len(), 2);

        let results = store.search_messages("world", None, 50).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, "sess-1");
    }

    #[tokio::test]
    async fn test_search_messages_single_session() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();
        store
            .create_session("sess-2", "default", None)
            .await
            .unwrap();

        store
            .add_message(&NewMessage::new("sess-1", "user", "hello world"))
            .await
            .unwrap();
        store
            .add_message(&NewMessage::new("sess-2", "user", "hello again"))
            .await
            .unwrap();

        let results = store
            .search_messages("hello", Some("sess-1"), 50)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, "sess-1");
    }

    #[tokio::test]
    async fn test_search_messages_respects_limit() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();

        for i in 0..10 {
            store
                .add_message(&NewMessage::new("sess-1", "user", format!("hello msg {i}")))
                .await
                .unwrap();
        }

        let results = store.search_messages("hello", None, 3).await.unwrap();
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn test_pin_session() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();

        let session = store.get_session("sess-1").await.unwrap().unwrap();
        assert!(!session.pinned);

        let pinned = store.pin_session("sess-1", true).await.unwrap();
        assert!(pinned);

        let session = store.get_session("sess-1").await.unwrap().unwrap();
        assert!(session.pinned);

        // Unpin
        store.pin_session("sess-1", false).await.unwrap();
        let session = store.get_session("sess-1").await.unwrap().unwrap();
        assert!(!session.pinned);
    }

    #[tokio::test]
    async fn test_archive_session() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", None)
            .await
            .unwrap();

        let session = store.get_session("sess-1").await.unwrap().unwrap();
        assert!(!session.archived);

        let archived = store.archive_session("sess-1", true).await.unwrap();
        assert!(archived);

        let session = store.get_session("sess-1").await.unwrap().unwrap();
        assert!(session.archived);

        // Unarchive
        store.archive_session("sess-1", false).await.unwrap();
        let session = store.get_session("sess-1").await.unwrap().unwrap();
        assert!(!session.archived);
    }

    #[tokio::test]
    async fn test_pin_nonexistent_returns_false() {
        let store = test_store().await;
        let result = store.pin_session("nonexistent", true).await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_pinned_sessions_sort_first() {
        let store = test_store().await;
        store
            .create_session("sess-1", "default", Some("First"))
            .await
            .unwrap();
        store
            .create_session("sess-2", "default", Some("Second"))
            .await
            .unwrap();
        store
            .create_session("sess-3", "default", Some("Third"))
            .await
            .unwrap();

        store.pin_session("sess-1", true).await.unwrap();

        let sessions = store.list_sessions(100, 0).await.unwrap();
        assert_eq!(sessions[0].id, "sess-1");
        assert!(sessions[0].pinned);
    }

    #[tokio::test]
    async fn test_add_message_sync_blocks_until_committed() {
        let store = test_store().await;
        store.create_session("s1", "default", None).await.unwrap();

        let _id = store
            .add_message_sync(&NewMessage {
                session_id: "s1".to_string(),
                role: "user".to_string(),
                content: "important user input".to_string(),
                model: None,
                tokens: Some(10),
                complete: true,
                tool_calls: None,
                tool_call_id: None,
            })
            .await
            .unwrap();

        let msgs = store.recent_messages("s1", 1).await.unwrap();
        assert_eq!(msgs[0].content, "important user input");
    }

    #[tokio::test]
    async fn test_denial_persisted_as_system_message() {
        let store = test_store().await;
        store.create_session("s1", "default", None).await.unwrap();

        store
            .add_denial(
                "s1",
                "shell",
                "rm -rf /",
                "User denied: destructive operation",
            )
            .await
            .unwrap();

        let msgs = store.recent_messages("s1", 10).await.unwrap();
        let denial = msgs
            .iter()
            .find(|m| m.role == "system" && m.content.contains("DENIED"))
            .unwrap();
        assert!(denial.content.contains("shell"));
        assert!(denial.content.contains("rm -rf"));
    }

    #[tokio::test]
    async fn test_clone_session_messages() {
        let store = test_store().await;
        store
            .create_session("parent", "default", Some("test"))
            .await
            .unwrap();
        store
            .add_message(&NewMessage {
                session_id: "parent".to_string(),
                role: "user".to_string(),
                content: "hello".to_string(),
                model: None,
                tokens: Some(5),
                complete: true,
                tool_calls: None,
                tool_call_id: None,
            })
            .await
            .unwrap();
        store
            .add_message(&NewMessage {
                session_id: "parent".to_string(),
                role: "assistant".to_string(),
                content: "hi there".to_string(),
                model: Some("test-model".to_string()),
                tokens: Some(3),
                complete: true,
                tool_calls: None,
                tool_call_id: None,
            })
            .await
            .unwrap();

        // Create child session and clone messages
        store
            .create_session("child", "default", Some("forked"))
            .await
            .unwrap();
        let count = store
            .clone_session_messages("parent", "child")
            .await
            .unwrap();
        assert_eq!(count, 2);

        let child_msgs = store.recent_messages("child", 10).await.unwrap();
        assert_eq!(child_msgs.len(), 2);
        assert_eq!(child_msgs[0].content, "hello");
        assert_eq!(child_msgs[1].content, "hi there");
        assert_eq!(child_msgs[0].session_id, "child");
    }

    #[tokio::test]
    async fn test_clone_session_messages_empty_source() {
        let store = test_store().await;
        store
            .create_session("empty-parent", "default", None)
            .await
            .unwrap();
        store
            .create_session("child", "default", None)
            .await
            .unwrap();

        let count = store
            .clone_session_messages("empty-parent", "child")
            .await
            .unwrap();
        assert_eq!(count, 0);

        let child_msgs = store.recent_messages("child", 10).await.unwrap();
        assert_eq!(child_msgs.len(), 0);
    }

    #[tokio::test]
    async fn test_tool_calls_persist_and_round_trip() {
        use ironjaw_llm::types::{ToolCall, ToolCallFunction};

        let store = test_store().await;
        store
            .create_session("tc-sess", "default", None)
            .await
            .unwrap();

        let tool_calls = vec![
            ToolCall {
                id: "call_001".to_string(),
                type_: "function".to_string(),
                function: ToolCallFunction {
                    name: "shell".to_string(),
                    arguments: "{\"command\":\"ls -la\"}".to_string(),
                },
            },
            ToolCall {
                id: "call_002".to_string(),
                type_: "function".to_string(),
                function: ToolCallFunction {
                    name: "read_file".to_string(),
                    arguments: "{\"path\":\"/etc/hosts\"}".to_string(),
                },
            },
        ];

        store
            .add_message(&NewMessage {
                session_id: "tc-sess".to_string(),
                role: "assistant".to_string(),
                content: String::new(),
                model: Some("qwen3-coder".to_string()),
                tokens: Some(42),
                complete: true,
                tool_calls: Some(tool_calls.clone()),
                tool_call_id: None,
            })
            .await
            .unwrap();

        let msgs = store.recent_messages("tc-sess", 10).await.unwrap();
        assert_eq!(msgs.len(), 1);
        let retrieved = &msgs[0];
        assert_eq!(retrieved.role, "assistant");

        let retrieved_calls = retrieved
            .tool_calls
            .as_ref()
            .expect("tool_calls should be Some");
        assert_eq!(retrieved_calls.len(), 2);
        assert_eq!(retrieved_calls[0].id, "call_001");
        assert_eq!(retrieved_calls[0].function.name, "shell");
        assert_eq!(retrieved_calls[1].id, "call_002");
        assert_eq!(retrieved_calls[1].function.name, "read_file");

        // Verify the full round-trip matches the original
        assert_eq!(*retrieved_calls, tool_calls);
    }

    #[tokio::test]
    async fn test_tool_call_id_persists_and_round_trips() {
        let store = test_store().await;
        store
            .create_session("tcid-sess", "default", None)
            .await
            .unwrap();

        store
            .add_message(&NewMessage {
                session_id: "tcid-sess".to_string(),
                role: "tool".to_string(),
                content: "file contents here".to_string(),
                model: None,
                tokens: None,
                complete: true,
                tool_calls: None,
                tool_call_id: Some("call_001".to_string()),
            })
            .await
            .unwrap();

        let msgs = store.recent_messages("tcid-sess", 10).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "tool");
        assert_eq!(msgs[0].tool_call_id.as_deref(), Some("call_001"));
        assert!(msgs[0].tool_calls.is_none());
    }

    #[tokio::test]
    async fn test_messages_without_tool_calls_have_none() {
        let store = test_store().await;
        store
            .create_session("notc-sess", "default", None)
            .await
            .unwrap();

        store
            .add_message(&NewMessage {
                session_id: "notc-sess".to_string(),
                role: "user".to_string(),
                content: "hello world".to_string(),
                model: None,
                tokens: Some(5),
                complete: true,
                tool_calls: None,
                tool_call_id: None,
            })
            .await
            .unwrap();

        let msgs = store.recent_messages("notc-sess", 10).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].tool_calls.is_none());
        assert!(msgs[0].tool_call_id.is_none());
    }
}
