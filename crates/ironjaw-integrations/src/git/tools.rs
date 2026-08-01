use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Git status tool — runs `git status --porcelain` in working directory.
pub struct GitStatusTool;

impl Default for GitStatusTool {
    fn default() -> Self {
        Self
    }
}

impl GitStatusTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for GitStatusTool {
    fn name(&self) -> &str {
        "git_status"
    }

    fn description(&self) -> &str {
        "Show git status (modified, staged, untracked files) for a local repo. Run inside the repo's working_dir."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path (defaults to working directory)"
                }
            }
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let dir = params
            .get("path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.working_dir.clone());

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new("git")
                .args(["status", "--porcelain", "--branch"])
                .current_dir(&dir)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                ToolResult::json(serde_json::json!({
                    "exit_code": output.status.code().unwrap_or(-1),
                    "output": stdout,
                    "stderr": stderr,
                }))
            }
            Ok(Err(e)) => ToolResult::error("EXEC_FAILED", format!("git status failed: {e}")),
            Err(_) => ToolResult::error("TIMEOUT", "git status timed out"),
        }
    }
}

/// Git log tool — runs `git log --oneline` in working directory.
pub struct GitLogTool;

impl Default for GitLogTool {
    fn default() -> Self {
        Self
    }
}

impl GitLogTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for GitLogTool {
    fn name(&self) -> &str {
        "git_log"
    }

    fn description(&self) -> &str {
        "Show recent git log entries for a local repo (commits, authors, dates). Run inside the repo's working_dir."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path (defaults to working directory)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Number of log entries (default 20)",
                    "minimum": 1,
                    "maximum": 100
                }
            }
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let dir = params
            .get("path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.working_dir.clone());

        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20);

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new("git")
                .args(["log", "--oneline", "-n", &limit.to_string()])
                .current_dir(&dir)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                ToolResult::json(serde_json::json!({
                    "exit_code": output.status.code().unwrap_or(-1),
                    "output": stdout,
                    "stderr": stderr,
                }))
            }
            Ok(Err(e)) => ToolResult::error("EXEC_FAILED", format!("git log failed: {e}")),
            Err(_) => ToolResult::error("TIMEOUT", "git log timed out"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_git_status_metadata() {
        let tool = GitStatusTool::new();
        assert_eq!(tool.name(), "git_status");
        assert_eq!(tool.tier(), PermissionTier::Auto);
    }

    #[test]
    fn test_git_log_metadata() {
        let tool = GitLogTool::new();
        assert_eq!(tool.name(), "git_log");
        assert_eq!(tool.tier(), PermissionTier::Auto);
    }

    #[tokio::test]
    async fn test_git_status_in_repo() {
        // Skip if git is not available in PATH
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        // Should work in any git repo (including this project)
        let ctx = ToolContext::new(
            "test",
            "test",
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        );
        let tool = GitStatusTool::new();
        let result = tool
            .execute(serde_json::json!({}), &ctx, CancellationToken::new())
            .await;
        assert!(!result.is_error());
    }
}
