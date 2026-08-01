use ironjaw_security::PermissionTier;
use ironjaw_tools::{MemoryHint, Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// docker-host status tool — system metrics via SSH.
pub struct RemoteHostStatusTool {
    host: String,
    user: String,
}

impl RemoteHostStatusTool {
    pub fn new(host: &str, user: &str) -> Self {
        Self {
            host: host.to_string(),
            user: user.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for RemoteHostStatusTool {
    fn name(&self) -> &str {
        "remote_host_status"
    }

    fn description(&self) -> &str {
        "Get the docker-host server's status (Linux Docker host at 192.0.2.20, ~50 containers): memory, disk, uptime, container summary. Use for questions about the second machine on the LAN."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "detail": {
                    "type": "string",
                    "description": "What to check: memory, disk, uptime, docker, or all (default: all)",
                    "enum": ["memory", "disk", "uptime", "docker", "all"]
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
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let detail = params
            .get("detail")
            .and_then(|v| v.as_str())
            .unwrap_or("all");

        let cmd = match detail {
            "memory" => "free -h",
            "disk" => "df -h",
            "uptime" => "uptime",
            "docker" => "docker ps --format 'table {{.Names}}\\t{{.Status}}\\t{{.Ports}}'",
            _ => "echo '=== Memory ===' && free -h && echo '\\n=== Disk ===' && df -h && echo '\\n=== Uptime ===' && uptime && echo '\\n=== Docker ===' && docker ps --format 'table {{.Names}}\\t{{.Status}}'",
        };

        let ssh_target = format!("{}@{}", self.user, self.host);
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::process::Command::new("ssh")
                .args([&ssh_target, cmd])
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
                        "SSH_ERROR",
                        format!("SSH command failed (exit {exit_code}): {stderr}"),
                    );
                }

                ToolResult::json(serde_json::json!({
                    "output": stdout,
                    "detail": detail,
                }))
            }
            Ok(Err(e)) => ToolResult::error("SSH_ERROR", format!("SSH failed: {e}")),
            Err(_) => ToolResult::error("TIMEOUT", "SSH command timed out after 15s"),
        }
    }

    fn memory_hints(
        &self,
        _input: &serde_json::Value,
        _output: &serde_json::Value,
    ) -> Vec<MemoryHint> {
        vec![MemoryHint {
            entity: "docker-host".to_string(),
            key: "services".to_string(),
            volatility: Duration::from_secs(300),
            content: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remote_host_status_metadata() {
        let tool = RemoteHostStatusTool::new("192.0.2.20", "deploy");
        assert_eq!(tool.name(), "remote_host_status");
        assert_eq!(tool.tier(), PermissionTier::Auto);
    }

    #[test]
    fn test_remote_host_status_schema() {
        let tool = RemoteHostStatusTool::new("192.0.2.20", "deploy");
        let schema = tool.schema();
        assert!(schema["properties"]["detail"].is_object());
    }
}
