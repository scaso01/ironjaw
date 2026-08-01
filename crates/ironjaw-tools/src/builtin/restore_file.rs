//! Restore a file to its previous state from snapshot.

use crate::snapshots::FileSnapshotStore;
use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Restore a file to its previous state from the snapshot store.
pub struct RestoreFileTool {
    snapshots: Arc<Mutex<FileSnapshotStore>>,
}

impl RestoreFileTool {
    pub fn new(snapshots: Arc<Mutex<FileSnapshotStore>>) -> Self {
        Self { snapshots }
    }
}

#[async_trait::async_trait]
impl Tool for RestoreFileTool {
    fn name(&self) -> &str {
        "restore_file"
    }

    fn description(&self) -> &str {
        "Restore a file to its previous state from snapshot"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to restore"
                }
            },
            "required": ["path"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Confirm
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let path_str = match params.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::error("INVALID_PARAMS", "Missing required parameter: path"),
        };
        let path = std::path::PathBuf::from(path_str);

        let content = {
            let store = self.snapshots.lock().await;
            store.get_latest(&path)
        };

        match content {
            Some(data) => match tokio::fs::write(&path, &data).await {
                Ok(()) => ToolResult::text(format!("Restored {} ({} bytes)", path_str, data.len())),
                Err(e) => ToolResult::error("RESTORE_FAILED", format!("Failed to restore: {e}")),
            },
            None => ToolResult::error("NO_SNAPSHOT", format!("No snapshot found for {path_str}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_ctx() -> ToolContext {
        ToolContext::new("test", "test", PathBuf::from("."))
    }

    #[tokio::test]
    async fn test_restore_no_snapshot() {
        let store = Arc::new(Mutex::new(FileSnapshotStore::new(100, 25 * 1024 * 1024)));
        let tool = RestoreFileTool::new(store);
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({"path": "/nonexistent"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_restore_with_snapshot() {
        let dir = std::env::temp_dir().join("ironjaw-test-restore");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "modified content").unwrap();

        let store = Arc::new(Mutex::new(FileSnapshotStore::new(100, 25 * 1024 * 1024)));
        {
            let mut s = store.lock().await;
            s.snapshot(&file_path, b"original content");
        }

        let tool = RestoreFileTool::new(store);
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({"path": file_path.to_str().unwrap()}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());

        let restored = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(restored, "original content");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_restore_file_tier() {
        let store = Arc::new(Mutex::new(FileSnapshotStore::new(100, 25 * 1024 * 1024)));
        let tool = RestoreFileTool::new(store);
        assert_eq!(tool.tier(), PermissionTier::Confirm);
    }

    #[test]
    fn test_restore_file_metadata() {
        let store = Arc::new(Mutex::new(FileSnapshotStore::new(100, 25 * 1024 * 1024)));
        let tool = RestoreFileTool::new(store);
        assert_eq!(tool.name(), "restore_file");
        assert!(!tool.description().is_empty());
        assert!(tool.schema().is_object());
    }
}
