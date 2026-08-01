use ironjaw_security::PermissionTier;
use ironjaw_tools::{MemoryHint, Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Docker management tool — manage containers on docker-host via SSH.
pub struct DockerManageTool {
    host: String,
    user: String,
}

impl DockerManageTool {
    pub fn new(host: &str, user: &str) -> Self {
        Self {
            host: host.to_string(),
            user: user.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for DockerManageTool {
    fn name(&self) -> &str {
        "docker_manage"
    }

    fn description(&self) -> &str {
        "Manage Docker containers on the docker-host server. Actions: list, logs, restart, stop, start. Use for 'is searxng running', 'show vane logs', 'restart khoj' type questions."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action to perform",
                    "enum": ["list", "logs", "restart", "stop", "start"]
                },
                "container": {
                    "type": "string",
                    "description": "Container name (required for logs, restart, stop, start)"
                },
                "lines": {
                    "type": "integer",
                    "description": "Number of log lines (default 50, only for logs action)",
                    "minimum": 1,
                    "maximum": 500
                }
            },
            "required": ["action"]
        })
    }

    fn tier(&self) -> PermissionTier {
        // Destructive actions (restart, stop, start) go through HITL anyway
        // since the method-level tools.execute already handles per-tool tier.
        PermissionTier::Confirm
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let action = match params.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: action")
            }
        };

        let container = params.get("container").and_then(|v| v.as_str());
        let lines = params.get("lines").and_then(|v| v.as_u64()).unwrap_or(50);

        // Validate container is required for non-list actions
        if action != "list" && container.is_none() {
            return ToolResult::error("INVALID_PARAMS", "container is required for this action");
        }

        // Sanitize container name to prevent command injection
        if let Some(name) = container {
            if !name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
            {
                return ToolResult::error(
                    "INVALID_PARAMS",
                    "Container name contains invalid characters",
                );
            }
        }

        let cmd = match action {
            "list" => {
                "docker ps --format 'table {{.Names}}\\t{{.Status}}\\t{{.Ports}}'".to_string()
            }
            "logs" => {
                let c = container.unwrap();
                format!("docker logs --tail {lines} {c}")
            }
            "restart" => format!("docker restart {}", container.unwrap()),
            "stop" => format!("docker stop {}", container.unwrap()),
            "start" => format!("docker start {}", container.unwrap()),
            _ => return ToolResult::error("INVALID_PARAMS", "Invalid action"),
        };

        let ssh_target = format!("{}@{}", self.user, self.host);
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new("ssh")
                .args([&ssh_target, &cmd])
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let exit_code = output.status.code().unwrap_or(-1);

                if exit_code != 0 {
                    return ToolResult::error(
                        "DOCKER_ERROR",
                        format!("Docker command failed (exit {exit_code}): {stderr}"),
                    );
                }

                ToolResult::json(serde_json::json!({
                    "action": action,
                    "container": container,
                    "output": stdout,
                }))
            }
            Ok(Err(e)) => ToolResult::error("SSH_ERROR", format!("SSH failed: {e}")),
            Err(_) => ToolResult::error("TIMEOUT", "Docker command timed out after 30s"),
        }
    }

    fn memory_hints(
        &self,
        _input: &serde_json::Value,
        _output: &serde_json::Value,
    ) -> Vec<MemoryHint> {
        vec![MemoryHint {
            entity: "docker-host".to_string(),
            key: "containers".to_string(),
            volatility: Duration::from_secs(300),
            content: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_ctx() -> ToolContext {
        ToolContext::new("test", "test", PathBuf::from("/tmp"))
    }

    #[test]
    fn test_docker_manage_metadata() {
        let tool = DockerManageTool::new("192.0.2.20", "deploy");
        assert_eq!(tool.name(), "docker_manage");
        assert_eq!(tool.tier(), PermissionTier::Confirm);
    }

    #[tokio::test]
    async fn test_docker_manage_missing_action() {
        let tool = DockerManageTool::new("192.0.2.20", "deploy");
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_docker_manage_missing_container() {
        let tool = DockerManageTool::new("192.0.2.20", "deploy");
        let result = tool
            .execute(
                serde_json::json!({"action": "restart"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, .. } => assert_eq!(code, "INVALID_PARAMS"),
            _ => panic!("Expected error"),
        }
    }
}
