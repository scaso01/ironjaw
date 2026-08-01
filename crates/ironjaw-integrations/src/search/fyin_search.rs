use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Fyin — custom search CLI on docker-host, invoked via SSH.
pub struct FyinSearchTool {
    host: String,
    user: String,
}

impl FyinSearchTool {
    pub fn new(host: &str, user: &str) -> Self {
        Self {
            host: host.to_string(),
            user: user.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for FyinSearchTool {
    fn name(&self) -> &str {
        "fyin_search"
    }

    fn description(&self) -> &str {
        "Run fyin (custom docker-host-side search CLI) over a query. Useful for fyin's specific corpus/heuristics not covered by searxng/vane. Slow (~2 min)."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" }
            },
            "required": ["query"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let query = match params.get("query").and_then(|v| v.as_str()) {
            Some(q) => q,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: query")
            }
        };
        // Quote query to avoid shell injection
        if query.contains('\'') {
            return ToolResult::error("INVALID_PARAMS", "Query may not contain single quotes");
        }
        let cmd = format!("fyin '{query}'");
        let ssh_target = format!("{}@{}", self.user, self.host);
        let result = tokio::time::timeout(
            Duration::from_secs(180),
            tokio::process::Command::new("ssh")
                .args([&ssh_target, &cmd])
                .output(),
        )
        .await;
        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                if !output.status.success() && stdout.is_empty() {
                    return ToolResult::error("FYIN_ERROR", stderr);
                }
                ToolResult::json(serde_json::json!({ "output": stdout, "query": query }))
            }
            Ok(Err(e)) => ToolResult::error("SSH_ERROR", format!("SSH failed: {e}")),
            Err(_) => ToolResult::error("TIMEOUT", "fyin timed out after 180s"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[test]
    fn meta() {
        let t = FyinSearchTool::new("192.0.2.20", "deploy");
        assert_eq!(t.name(), "fyin_search");
    }
    #[tokio::test]
    async fn missing() {
        let r = FyinSearchTool::new("192.0.2.20", "deploy")
            .execute(
                serde_json::json!({}),
                &ToolContext::new("t", "t", PathBuf::from("/tmp")),
                CancellationToken::new(),
            )
            .await;
        assert!(r.is_error());
    }
}
