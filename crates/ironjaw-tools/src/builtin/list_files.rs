use crate::types::{MemoryHint, Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Lists files and directories in the server filesystem.
pub struct ListFilesTool;

impl Default for ListFilesTool {
    fn default() -> Self {
        Self
    }
}

impl ListFilesTool {
    pub fn new() -> Self {
        Self
    }
}

/// Check that `target` is under `base` after canonicalization.
fn ensure_under(base: &Path, target: &Path) -> Result<PathBuf, String> {
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        base.join(target)
    };

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

async fn list_dir(path: &Path) -> Result<Vec<serde_json::Value>, String> {
    let mut entries = Vec::new();
    let mut read_dir = tokio::fs::read_dir(path)
        .await
        .map_err(|e| format!("Failed to read directory: {e}"))?;

    while let Some(entry) = read_dir
        .next_entry()
        .await
        .map_err(|e| format!("Failed to read entry: {e}"))?
    {
        let meta = entry
            .metadata()
            .await
            .map_err(|e| format!("Failed to get metadata: {e}"))?;

        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = meta.is_dir();
        let size = if is_dir { 0 } else { meta.len() };

        entries.push(serde_json::json!({
            "name": name,
            "is_dir": is_dir,
            "size": size,
        }));
    }

    entries.sort_by(|a, b| {
        let a_dir = a["is_dir"].as_bool().unwrap_or(false);
        let b_dir = b["is_dir"].as_bool().unwrap_or(false);
        b_dir.cmp(&a_dir).then_with(|| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        })
    });

    Ok(entries)
}

async fn list_dir_recursive(path: &Path, base: &Path) -> Result<Vec<serde_json::Value>, String> {
    let mut entries = Vec::new();
    let mut stack = vec![path.to_path_buf()];

    while let Some(current) = stack.pop() {
        let mut read_dir = tokio::fs::read_dir(&current)
            .await
            .map_err(|e| format!("Failed to read directory: {e}"))?;

        while let Some(entry) = read_dir
            .next_entry()
            .await
            .map_err(|e| format!("Failed to read entry: {e}"))?
        {
            let meta = entry
                .metadata()
                .await
                .map_err(|e| format!("Failed to get metadata: {e}"))?;

            let full_path = entry.path();
            let rel_path = full_path
                .strip_prefix(base)
                .unwrap_or(&full_path)
                .to_string_lossy()
                .to_string();

            let is_dir = meta.is_dir();
            let size = if is_dir { 0 } else { meta.len() };

            entries.push(serde_json::json!({
                "name": rel_path,
                "is_dir": is_dir,
                "size": size,
            }));

            if is_dir {
                stack.push(full_path);
            }
        }
    }

    entries.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });

    Ok(entries)
}

#[async_trait::async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }

    fn description(&self) -> &str {
        "List entries in a local directory on this machine. Use this FIRST for any question about projects, folders, or files under C:\\Users\\deploy\\Projects. Use bare folder names (e.g. 'example-project'), not nested paths. Returns a 'count' field — use it directly without recounting."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path (relative to working directory, default: '.')"
                },
                "recursive": {
                    "type": "boolean",
                    "description": "List recursively (default: false)"
                }
            }
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }

    /// Emit a `ToolVerified` memory hint so the agentic writeback path records
    /// what files were present in the listed directory at the time of the call.
    /// Volatility is 5 minutes — directory contents change infrequently but the
    /// live-state filter should still treat this as perishable.
    fn memory_hints(
        &self,
        input: &serde_json::Value,
        output: &serde_json::Value,
    ) -> Vec<MemoryHint> {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        // Build a compact human-readable summary from the tool output.
        let content = output
            .get("content")
            .and_then(|c| c.get("summary"))
            .and_then(|s| s.as_str())
            .map(|summary| summary.to_string());

        vec![MemoryHint {
            entity: "filesystem".to_string(),
            key: format!("list_files:{path}"),
            volatility: Duration::from_secs(300),
            content,
        }]
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let path_str = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let recursive = params
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let path = Path::new(path_str);
        let resolved = match ensure_under(&ctx.working_dir, path) {
            Ok(p) => p,
            Err(e) => return ToolResult::error("PATH_DENIED", e),
        };

        let entries = if recursive {
            list_dir_recursive(&resolved, &resolved).await
        } else {
            list_dir(&resolved).await
        };

        match entries {
            Ok(items) => {
                let total = items.len();
                let folders: Vec<&str> = items
                    .iter()
                    .filter(|e| e["is_dir"].as_bool().unwrap_or(false))
                    .filter_map(|e| e["name"].as_str())
                    .collect();
                let files: Vec<&str> = items
                    .iter()
                    .filter(|e| !e["is_dir"].as_bool().unwrap_or(false))
                    .filter_map(|e| e["name"].as_str())
                    .collect();
                let summary = format!(
                    "Directory '{}' contains {} entries: {} folders, {} files. \
                     Use this 'count' field directly — do NOT recount.",
                    resolved.display(),
                    total,
                    folders.len(),
                    files.len()
                );
                ToolResult::json(serde_json::json!({
                    "summary": summary,
                    "path": resolved.display().to_string(),
                    "count": total,
                    "folders_count": folders.len(),
                    "files_count": files.len(),
                    "folders": folders,
                    "files": files,
                    "entries": items,
                }))
            }
            Err(e) => ToolResult::error("LIST_FAILED", e),
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
    async fn test_list_files() {
        let dir = std::env::temp_dir().join("ironjaw-test-list");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "aaa").unwrap();
        std::fs::write(dir.join("b.txt"), "bbb").unwrap();
        std::fs::create_dir_all(dir.join("subdir")).unwrap();

        let tool = ListFilesTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(serde_json::json!({}), &ctx, CancellationToken::new())
            .await;
        assert!(!result.is_error());
        match &result {
            ToolResult::Json { content } => {
                let count = content["count"].as_u64().unwrap();
                assert!(count >= 3); // a.txt, b.txt, subdir
                                     // Directories should come first
                let entries = content["entries"].as_array().unwrap();
                let first = &entries[0];
                assert!(first["is_dir"].as_bool().unwrap());
            }
            _ => panic!("Expected Json result"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_list_files_recursive() {
        let dir = std::env::temp_dir().join("ironjaw-test-list-rec");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("top.txt"), "top").unwrap();
        std::fs::write(dir.join("sub").join("nested.txt"), "nested").unwrap();

        let tool = ListFilesTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(
                serde_json::json!({"recursive": true}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
        match &result {
            ToolResult::Json { content } => {
                let entries = content["entries"].as_array().unwrap();
                let names: Vec<&str> = entries
                    .iter()
                    .map(|e| e["name"].as_str().unwrap())
                    .collect();
                assert!(names.iter().any(|n| n.contains("nested.txt")));
            }
            _ => panic!("Expected Json result"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_list_files_traversal_blocked() {
        let dir = std::env::temp_dir().join("ironjaw-test-list-trav");
        std::fs::create_dir_all(&dir).unwrap();

        let tool = ListFilesTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(
                serde_json::json!({"path": "../.."}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_list_files_tier() {
        let tool = ListFilesTool::new();
        assert_eq!(tool.tier(), PermissionTier::Auto);
    }

    #[tokio::test]
    async fn test_list_files_pretty_summary() {
        let dir = std::env::temp_dir().join("ironjaw-test-list-summary");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "aaa").unwrap();
        std::fs::write(dir.join("b.txt"), "bbb").unwrap();
        std::fs::create_dir_all(dir.join("dirA")).unwrap();
        std::fs::create_dir_all(dir.join("dirB")).unwrap();

        let tool = ListFilesTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(serde_json::json!({}), &ctx, CancellationToken::new())
            .await;
        assert!(!result.is_error());
        match &result {
            ToolResult::Json { content } => {
                assert_eq!(content["count"].as_u64().unwrap(), 4);
                assert_eq!(content["folders_count"].as_u64().unwrap(), 2);
                assert_eq!(content["files_count"].as_u64().unwrap(), 2);
                let summary = content["summary"].as_str().unwrap();
                assert!(summary.contains("4 entries"));
                assert!(summary.contains("2 folders"));
                assert!(summary.contains("2 files"));
                let folders = content["folders"].as_array().unwrap();
                assert_eq!(folders.len(), 2);
                let files = content["files"].as_array().unwrap();
                assert_eq!(files.len(), 2);
            }
            _ => panic!("Expected Json result"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
