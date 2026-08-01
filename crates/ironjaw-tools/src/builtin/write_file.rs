use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Writes content to a file on the server filesystem.
pub struct WriteFileTool;

impl Default for WriteFileTool {
    fn default() -> Self {
        Self
    }
}

impl WriteFileTool {
    pub fn new() -> Self {
        Self
    }
}

/// Check that `target` (which may not exist yet) stays under `base`.
/// For new files, we verify the parent directory is under base.
fn ensure_write_path(base: &Path, target: &Path) -> Result<PathBuf, String> {
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        base.join(target)
    };

    let canon_base = base
        .canonicalize()
        .map_err(|e| format!("Cannot resolve base directory: {e}"))?;

    // For write, the file may not exist yet — canonicalize the parent instead
    let parent = resolved
        .parent()
        .ok_or_else(|| "Invalid path: no parent directory".to_string())?;

    let canon_parent = parent.canonicalize().map_err(|e| {
        format!(
            "Cannot resolve parent directory '{}': {}",
            parent.display(),
            e
        )
    })?;

    if canon_parent.starts_with(&canon_base) {
        // Return the canonicalized parent + original filename
        let filename = resolved
            .file_name()
            .ok_or_else(|| "Invalid path: no filename".to_string())?;
        Ok(canon_parent.join(filename))
    } else {
        Err(format!(
            "Path '{}' is outside working directory",
            target.display()
        ))
    }
}

#[async_trait::async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a local file (create or overwrite). Path resolves against working_dir. Requires HITL approval — only use when the user has explicitly asked to create or modify a file."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file (relative to working directory)"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Confirm
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let path_str = match params.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::error("INVALID_PARAMS", "Missing required parameter: path"),
        };

        let content = match params.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: content")
            }
        };

        let path = Path::new(path_str);
        let resolved = match ensure_write_path(&ctx.working_dir, path) {
            Ok(p) => p,
            Err(e) => return ToolResult::error("PATH_DENIED", e),
        };

        match tokio::fs::write(&resolved, content).await {
            Ok(()) => ToolResult::json(serde_json::json!({
                "path": resolved.display().to_string(),
                "bytes_written": content.len(),
            })),
            Err(e) => ToolResult::error("WRITE_FAILED", format!("Failed to write file: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx_in(dir: &Path) -> ToolContext {
        ToolContext::new("test", "test", dir.to_path_buf())
    }

    #[tokio::test]
    async fn test_write_file() {
        let dir = std::env::temp_dir().join("ironjaw-test-write");
        std::fs::create_dir_all(&dir).unwrap();

        let tool = WriteFileTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(
                serde_json::json!({"path": "test.txt", "content": "hello"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());

        let written = std::fs::read_to_string(dir.join("test.txt")).unwrap();
        assert_eq!(written, "hello");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_write_file_missing_content() {
        let dir = std::env::temp_dir().join("ironjaw-test-write-mc");
        std::fs::create_dir_all(&dir).unwrap();

        let tool = WriteFileTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(
                serde_json::json!({"path": "test.txt"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_write_file_traversal_blocked() {
        let dir = std::env::temp_dir().join("ironjaw-test-write-trav");
        std::fs::create_dir_all(&dir).unwrap();

        let tool = WriteFileTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(
                serde_json::json!({"path": "../../evil.txt", "content": "bad"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_write_file_tier() {
        let tool = WriteFileTool::new();
        assert_eq!(tool.tier(), PermissionTier::Confirm);
    }
}
