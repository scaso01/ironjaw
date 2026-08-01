//! Shared Blackboard for Multi-Agent Tasks (I7)
//!
//! An in-memory keyed store per task_id with pub/sub notifications.
//! Each task workspace has its own `RwLock` for concurrent access and
//! a broadcast channel for subscription notifications. Workspaces
//! auto-expire after a configurable TTL (default 1 hour).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A change notification sent to subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlackboardChange {
    pub task_id: String,
    pub key: String,
    pub value: serde_json::Value,
    pub changed_at: DateTime<Utc>,
    pub changed_by: Option<String>,
}

/// A single task workspace on the blackboard.
struct TaskWorkspace {
    entries: HashMap<String, serde_json::Value>,
    #[allow(dead_code)]
    created_at: DateTime<Utc>,
    last_accessed: DateTime<Utc>,
    ttl_secs: u64,
    /// Broadcast sender for pub/sub notifications.
    notify_tx: broadcast::Sender<BlackboardChange>,
}

impl TaskWorkspace {
    fn new(ttl_secs: u64) -> Self {
        let (notify_tx, _) = broadcast::channel(256);
        Self {
            entries: HashMap::new(),
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            ttl_secs,
            notify_tx,
        }
    }

    fn is_expired(&self) -> bool {
        let elapsed_ms = (Utc::now() - self.last_accessed).num_milliseconds();
        elapsed_ms > 0 && elapsed_ms as u64 > self.ttl_secs * 1000
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from blackboard operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlackboardError {
    /// The task workspace does not exist.
    TaskNotFound(String),
    /// The task workspace already exists.
    TaskAlreadyExists(String),
    /// The key was not found in the workspace.
    KeyNotFound(String),
}

impl std::fmt::Display for BlackboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TaskNotFound(id) => write!(f, "blackboard task not found: {id}"),
            Self::TaskAlreadyExists(id) => write!(f, "blackboard task already exists: {id}"),
            Self::KeyNotFound(key) => write!(f, "blackboard key not found: {key}"),
        }
    }
}

impl std::error::Error for BlackboardError {}

// ---------------------------------------------------------------------------
// Blackboard
// ---------------------------------------------------------------------------

/// Shared blackboard for multi-agent task collaboration.
pub struct Blackboard {
    workspaces: Arc<RwLock<HashMap<String, TaskWorkspace>>>,
    default_ttl_secs: u64,
}

impl Blackboard {
    /// Create a new blackboard with the given default TTL in seconds.
    pub fn new(default_ttl_secs: u64) -> Self {
        Self {
            workspaces: Arc::new(RwLock::new(HashMap::new())),
            default_ttl_secs,
        }
    }

    /// Create a new task workspace. Returns the task_id.
    pub async fn create(&self, task_id: Option<String>) -> Result<String, BlackboardError> {
        let id = task_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let mut ws = self.workspaces.write().await;
        if ws.contains_key(&id) {
            return Err(BlackboardError::TaskAlreadyExists(id));
        }
        ws.insert(id.clone(), TaskWorkspace::new(self.default_ttl_secs));
        info!(task_id = %id, "Blackboard workspace created");
        Ok(id)
    }

    /// Write a key-value pair to a task workspace.
    pub async fn write(
        &self,
        task_id: &str,
        key: &str,
        value: serde_json::Value,
        changed_by: Option<String>,
    ) -> Result<(), BlackboardError> {
        let mut ws = self.workspaces.write().await;
        let workspace = ws
            .get_mut(task_id)
            .ok_or_else(|| BlackboardError::TaskNotFound(task_id.to_string()))?;

        workspace.last_accessed = Utc::now();
        workspace.entries.insert(key.to_string(), value.clone());

        // Notify subscribers (ignore send errors — no receivers is fine)
        let change = BlackboardChange {
            task_id: task_id.to_string(),
            key: key.to_string(),
            value,
            changed_at: Utc::now(),
            changed_by,
        };
        let _ = workspace.notify_tx.send(change);

        debug!(task_id = %task_id, key = %key, "Blackboard entry written");
        Ok(())
    }

    /// Read a key from a task workspace.
    pub async fn read(
        &self,
        task_id: &str,
        key: &str,
    ) -> Result<serde_json::Value, BlackboardError> {
        let mut ws = self.workspaces.write().await;
        let workspace = ws
            .get_mut(task_id)
            .ok_or_else(|| BlackboardError::TaskNotFound(task_id.to_string()))?;

        workspace.last_accessed = Utc::now();
        workspace
            .entries
            .get(key)
            .cloned()
            .ok_or_else(|| BlackboardError::KeyNotFound(key.to_string()))
    }

    /// Read all entries from a task workspace.
    pub async fn read_all(
        &self,
        task_id: &str,
    ) -> Result<HashMap<String, serde_json::Value>, BlackboardError> {
        let mut ws = self.workspaces.write().await;
        let workspace = ws
            .get_mut(task_id)
            .ok_or_else(|| BlackboardError::TaskNotFound(task_id.to_string()))?;

        workspace.last_accessed = Utc::now();
        Ok(workspace.entries.clone())
    }

    /// Subscribe to changes on a task workspace.
    /// Returns a broadcast receiver that yields `BlackboardChange` events.
    pub async fn subscribe(
        &self,
        task_id: &str,
    ) -> Result<broadcast::Receiver<BlackboardChange>, BlackboardError> {
        let ws = self.workspaces.read().await;
        let workspace = ws
            .get(task_id)
            .ok_or_else(|| BlackboardError::TaskNotFound(task_id.to_string()))?;

        Ok(workspace.notify_tx.subscribe())
    }

    /// Destroy a task workspace. Returns `true` if it existed.
    pub async fn destroy(&self, task_id: &str) -> bool {
        let mut ws = self.workspaces.write().await;
        let existed = ws.remove(task_id).is_some();
        if existed {
            info!(task_id = %task_id, "Blackboard workspace destroyed");
        }
        existed
    }

    /// List all active task workspace IDs.
    pub async fn list_tasks(&self) -> Vec<String> {
        let ws = self.workspaces.read().await;
        ws.keys().cloned().collect()
    }

    /// Get the number of entries in a task workspace.
    pub async fn entry_count(&self, task_id: &str) -> Result<usize, BlackboardError> {
        let ws = self.workspaces.read().await;
        let workspace = ws
            .get(task_id)
            .ok_or_else(|| BlackboardError::TaskNotFound(task_id.to_string()))?;
        Ok(workspace.entries.len())
    }

    /// Clean up expired workspaces. Returns the number removed.
    pub async fn cleanup_expired(&self) -> usize {
        let mut ws = self.workspaces.write().await;
        let before = ws.len();
        ws.retain(|id, workspace| {
            let expired = workspace.is_expired();
            if expired {
                warn!(task_id = %id, "Blackboard workspace expired and removed");
            }
            !expired
        });
        before - ws.len()
    }

    /// Get the total number of active workspaces.
    pub async fn workspace_count(&self) -> usize {
        let ws = self.workspaces.read().await;
        ws.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_workspace() {
        let bb = Blackboard::new(3600);
        let id = bb.create(Some("task-1".into())).await.unwrap();
        assert_eq!(id, "task-1");
        assert_eq!(bb.workspace_count().await, 1);
    }

    #[tokio::test]
    async fn test_create_auto_id() {
        let bb = Blackboard::new(3600);
        let id = bb.create(None).await.unwrap();
        assert!(!id.is_empty());
    }

    #[tokio::test]
    async fn test_create_duplicate() {
        let bb = Blackboard::new(3600);
        bb.create(Some("task-1".into())).await.unwrap();
        let err = bb.create(Some("task-1".into())).await.unwrap_err();
        assert_eq!(err, BlackboardError::TaskAlreadyExists("task-1".into()));
    }

    #[tokio::test]
    async fn test_write_and_read() {
        let bb = Blackboard::new(3600);
        bb.create(Some("t1".into())).await.unwrap();

        bb.write("t1", "key1", serde_json::json!("value1"), None)
            .await
            .unwrap();

        let val = bb.read("t1", "key1").await.unwrap();
        assert_eq!(val, serde_json::json!("value1"));
    }

    #[tokio::test]
    async fn test_read_missing_key() {
        let bb = Blackboard::new(3600);
        bb.create(Some("t1".into())).await.unwrap();
        let err = bb.read("t1", "nope").await.unwrap_err();
        assert_eq!(err, BlackboardError::KeyNotFound("nope".into()));
    }

    #[tokio::test]
    async fn test_read_missing_task() {
        let bb = Blackboard::new(3600);
        let err = bb.read("missing", "k").await.unwrap_err();
        assert_eq!(err, BlackboardError::TaskNotFound("missing".into()));
    }

    #[tokio::test]
    async fn test_write_missing_task() {
        let bb = Blackboard::new(3600);
        let err = bb
            .write("missing", "k", serde_json::json!(1), None)
            .await
            .unwrap_err();
        assert_eq!(err, BlackboardError::TaskNotFound("missing".into()));
    }

    #[tokio::test]
    async fn test_read_all() {
        let bb = Blackboard::new(3600);
        bb.create(Some("t1".into())).await.unwrap();
        bb.write("t1", "a", serde_json::json!(1), None)
            .await
            .unwrap();
        bb.write("t1", "b", serde_json::json!(2), None)
            .await
            .unwrap();

        let all = bb.read_all("t1").await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all["a"], serde_json::json!(1));
        assert_eq!(all["b"], serde_json::json!(2));
    }

    #[tokio::test]
    async fn test_destroy() {
        let bb = Blackboard::new(3600);
        bb.create(Some("t1".into())).await.unwrap();
        assert!(bb.destroy("t1").await);
        assert!(!bb.destroy("t1").await);
        assert_eq!(bb.workspace_count().await, 0);
    }

    #[tokio::test]
    async fn test_list_tasks() {
        let bb = Blackboard::new(3600);
        bb.create(Some("a".into())).await.unwrap();
        bb.create(Some("b".into())).await.unwrap();

        let mut tasks = bb.list_tasks().await;
        tasks.sort();
        assert_eq!(tasks, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn test_entry_count() {
        let bb = Blackboard::new(3600);
        bb.create(Some("t1".into())).await.unwrap();
        assert_eq!(bb.entry_count("t1").await.unwrap(), 0);

        bb.write("t1", "x", serde_json::json!(1), None)
            .await
            .unwrap();
        assert_eq!(bb.entry_count("t1").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_subscribe_receives_changes() {
        let bb = Blackboard::new(3600);
        bb.create(Some("t1".into())).await.unwrap();

        let mut rx = bb.subscribe("t1").await.unwrap();

        bb.write(
            "t1",
            "key",
            serde_json::json!("val"),
            Some("agent-1".into()),
        )
        .await
        .unwrap();

        let change = rx.recv().await.unwrap();
        assert_eq!(change.task_id, "t1");
        assert_eq!(change.key, "key");
        assert_eq!(change.value, serde_json::json!("val"));
        assert_eq!(change.changed_by, Some("agent-1".into()));
    }

    #[tokio::test]
    async fn test_subscribe_missing_task() {
        let bb = Blackboard::new(3600);
        let err = bb.subscribe("nope").await.unwrap_err();
        assert_eq!(err, BlackboardError::TaskNotFound("nope".into()));
    }

    #[tokio::test]
    async fn test_overwrite_key() {
        let bb = Blackboard::new(3600);
        bb.create(Some("t1".into())).await.unwrap();

        bb.write("t1", "k", serde_json::json!(1), None)
            .await
            .unwrap();
        bb.write("t1", "k", serde_json::json!(2), None)
            .await
            .unwrap();

        let val = bb.read("t1", "k").await.unwrap();
        assert_eq!(val, serde_json::json!(2));
    }

    #[tokio::test]
    async fn test_cleanup_expired() {
        let bb = Blackboard::new(0); // TTL of 0 = immediately expired
        bb.create(Some("t1".into())).await.unwrap();

        // Wait a tiny bit so the workspace is past its TTL
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let removed = bb.cleanup_expired().await;
        assert_eq!(removed, 1);
        assert_eq!(bb.workspace_count().await, 0);
    }

    #[tokio::test]
    async fn test_error_display() {
        let e1 = BlackboardError::TaskNotFound("t".into());
        assert_eq!(e1.to_string(), "blackboard task not found: t");

        let e2 = BlackboardError::TaskAlreadyExists("t".into());
        assert_eq!(e2.to_string(), "blackboard task already exists: t");

        let e3 = BlackboardError::KeyNotFound("k".into());
        assert_eq!(e3.to_string(), "blackboard key not found: k");
    }
}
