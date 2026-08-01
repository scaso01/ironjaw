use crate::types::{MemoryHint, Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DEFAULT_LIMIT_BYTES: u64 = 1024 * 1024; // 1 MB

/// Reads file contents from the server filesystem.
pub struct ReadFileTool;

impl Default for ReadFileTool {
    fn default() -> Self {
        Self
    }
}

impl ReadFileTool {
    pub fn new() -> Self {
        Self
    }
}

/// Check that `target` is under `base` after canonicalization.
/// Returns the canonicalized path on success.
fn ensure_under(base: &Path, target: &Path) -> Result<PathBuf, String> {
    // Resolve relative paths against the base
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        base.join(target)
    };

    // Canonicalize both — requires they exist for base, but target may not exist yet
    let canon_base = base
        .canonicalize()
        .map_err(|e| format!("Cannot resolve base directory: {e}"))?;

    let canon_target = resolved
        .canonicalize()
        .map_err(|e| format!("Cannot resolve path '{}': {}", resolved.display(), e))?;

    if canon_target.starts_with(&canon_base) {
        Ok(canon_target)
    } else {
        Err(format!(
            "Path '{}' is outside working directory",
            target.display()
        ))
    }
}

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a local file on this machine — project source, config, README, design doc. Path resolves against the working directory (C:\\Users\\deploy\\Projects)."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file (relative to working directory)"
                },
                "limit_bytes": {
                    "type": "integer",
                    "description": "Maximum bytes to read (default 1MB)",
                    "minimum": 1
                }
            },
            "required": ["path"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }

    /// Emit a `ToolVerified` memory hint so the agentic writeback path records
    /// that a specific file was read and what it contained.  Content is capped
    /// to a 512-character excerpt so the memory stays compact; volatility is
    /// 10 minutes (file contents change rarely but shouldn't be treated as
    /// permanent).
    fn memory_hints(
        &self,
        input: &serde_json::Value,
        output: &serde_json::Value,
    ) -> Vec<MemoryHint> {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return vec![],
        };

        // Build a compact excerpt from the file content.
        let content = output.get("content").and_then(|c| c.as_str()).map(|text| {
            if text.len() > 512 {
                format!("{}… [truncated]", &text[..512])
            } else {
                text.to_string()
            }
        });

        vec![MemoryHint {
            entity: "filesystem".to_string(),
            key: format!("read_file:{path}"),
            volatility: Duration::from_secs(600),
            content,
        }]
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

        let limit = params
            .get("limit_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_LIMIT_BYTES);

        let path = Path::new(path_str);
        let resolved = match ensure_under(&ctx.working_dir, path) {
            Ok(p) => p,
            Err(e) => return ToolResult::error("PATH_DENIED", e),
        };

        // Check file size first
        match tokio::fs::metadata(&resolved).await {
            Ok(meta) => {
                if meta.len() > limit {
                    return ToolResult::error(
                        "FILE_TOO_LARGE",
                        format!(
                            "File is {} bytes, exceeds limit of {} bytes",
                            meta.len(),
                            limit
                        ),
                    );
                }
            }
            Err(e) => {
                return ToolResult::error("READ_FAILED", format!("Cannot stat file: {e}"));
            }
        }

        match tokio::fs::read_to_string(&resolved).await {
            Ok(content) => ToolResult::text(content),
            Err(e) => ToolResult::error("READ_FAILED", format!("Failed to read file: {e}")),
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
    async fn test_read_file() {
        let dir = std::env::temp_dir().join("ironjaw-test-read");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        std::fs::write(&file, "Hello, world!").unwrap();

        let tool = ReadFileTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(
                serde_json::json!({"path": "hello.txt"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
        match result {
            ToolResult::Text { content } => assert_eq!(content, "Hello, world!"),
            _ => panic!("Expected Text result"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_read_file_not_found() {
        let dir = std::env::temp_dir().join("ironjaw-test-read-nf");
        std::fs::create_dir_all(&dir).unwrap();

        let tool = ReadFileTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(
                serde_json::json!({"path": "nonexistent.txt"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_read_file_traversal_blocked() {
        let dir = std::env::temp_dir().join("ironjaw-test-read-trav");
        std::fs::create_dir_all(&dir).unwrap();

        let tool = ReadFileTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(
                serde_json::json!({"path": "../../etc/passwd"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, .. } => {
                // Could be PATH_DENIED or READ_FAILED depending on OS
                assert!(code == "PATH_DENIED" || code == "READ_FAILED");
            }
            _ => panic!("Expected Error result"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_read_file_tier() {
        let tool = ReadFileTool::new();
        assert_eq!(tool.tier(), PermissionTier::Auto);
    }
}
