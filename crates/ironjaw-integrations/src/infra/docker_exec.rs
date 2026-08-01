use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Docker exec tool — execute commands inside Docker containers on docker-host via SSH.
pub struct DockerExecTool {
    host: String,
    user: String,
}

impl DockerExecTool {
    pub fn new(host: &str, user: &str) -> Self {
        Self {
            host: host.to_string(),
            user: user.to_string(),
        }
    }

    /// Build the SSH command string for docker exec.
    /// Exposed for testing command construction without executing.
    pub fn build_command(container: &str, command: &str) -> String {
        format!("docker exec {container} {command}")
    }

    /// Validate container name — alphanumeric, hyphens, underscores, dots only.
    pub fn validate_container_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    }

    /// Validate that a docker command does not contain shell metacharacters
    /// that could enable command injection via the SSH transport.
    pub fn validate_docker_command(cmd: &str) -> Result<(), String> {
        const DANGEROUS: &[char] = &[';', '|', '&', '`', '$', '(', ')', '{', '}', '<', '>'];
        if cmd.chars().any(|c| DANGEROUS.contains(&c)) {
            return Err("Command contains disallowed shell metacharacter".to_string());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Tool for DockerExecTool {
    fn name(&self) -> &str {
        "docker_exec"
    }

    fn description(&self) -> &str {
        "Execute a command inside a Docker container on docker-host via SSH"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "container": {
                    "type": "string",
                    "description": "Docker container name to exec into"
                },
                "command": {
                    "type": "string",
                    "description": "Command to execute inside the container"
                }
            },
            "required": ["container", "command"]
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
        let container = match params.get("container").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult::error(
                    "INVALID_PARAMS",
                    "Missing required parameter: container",
                );
            }
        };

        let command = match params.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: command");
            }
        };

        // Sanitize container name to prevent command injection
        if !Self::validate_container_name(container) {
            return ToolResult::error(
                "INVALID_PARAMS",
                "Container name contains invalid characters",
            );
        }

        // Validate command does not contain shell metacharacters
        if let Err(msg) = Self::validate_docker_command(command) {
            tracing::warn!(
                container = container,
                command = command,
                "Docker exec command rejected: {msg}"
            );
            return ToolResult::error("INVALID_PARAMS", msg);
        }

        let docker_cmd = Self::build_command(container, command);
        let ssh_target = format!("{}@{}", self.user, self.host);

        let result = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new("ssh")
                .args([&ssh_target, &docker_cmd])
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
                        "DOCKER_EXEC_ERROR",
                        format!("docker exec failed (exit {exit_code}): {stderr}"),
                    );
                }

                ToolResult::json(serde_json::json!({
                    "container": container,
                    "command": command,
                    "output": stdout,
                    "stderr": stderr,
                }))
            }
            Ok(Err(e)) => ToolResult::error("SSH_ERROR", format!("SSH failed: {e}")),
            Err(_) => ToolResult::error("TIMEOUT", "docker exec timed out after 30s"),
        }
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
    fn test_docker_exec_metadata() {
        let tool = DockerExecTool::new("192.0.2.20", "deploy");
        assert_eq!(tool.name(), "docker_exec");
        assert_eq!(tool.tier(), PermissionTier::Confirm);
    }

    #[test]
    fn test_docker_exec_schema() {
        let tool = DockerExecTool::new("192.0.2.20", "deploy");
        let schema = tool.schema();
        assert!(schema["properties"]["container"].is_object());
        assert!(schema["properties"]["command"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("container")));
        assert!(required.contains(&serde_json::json!("command")));
    }

    #[test]
    fn test_build_command() {
        let cmd = DockerExecTool::build_command("searxng", "cat /etc/hostname");
        assert_eq!(cmd, "docker exec searxng cat /etc/hostname");
    }

    #[test]
    fn test_build_command_complex() {
        let cmd = DockerExecTool::build_command("uptime-kuma", "sh -c 'echo hello'");
        assert_eq!(cmd, "docker exec uptime-kuma sh -c 'echo hello'");
    }

    #[test]
    fn test_validate_container_name_valid() {
        assert!(DockerExecTool::validate_container_name("searxng"));
        assert!(DockerExecTool::validate_container_name("uptime-kuma"));
        assert!(DockerExecTool::validate_container_name("my_container"));
        assert!(DockerExecTool::validate_container_name("container.v2"));
        assert!(DockerExecTool::validate_container_name("abc123"));
    }

    #[test]
    fn test_validate_container_name_invalid() {
        assert!(!DockerExecTool::validate_container_name(""));
        assert!(!DockerExecTool::validate_container_name("foo;rm -rf /"));
        assert!(!DockerExecTool::validate_container_name("$(whoami)"));
        assert!(!DockerExecTool::validate_container_name("con tainer"));
        assert!(!DockerExecTool::validate_container_name("a&b"));
    }

    #[tokio::test]
    async fn test_docker_exec_missing_container() {
        let tool = DockerExecTool::new("192.0.2.20", "deploy");
        let result = tool
            .execute(
                serde_json::json!({"command": "ls"}),
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

    #[tokio::test]
    async fn test_docker_exec_missing_command() {
        let tool = DockerExecTool::new("192.0.2.20", "deploy");
        let result = tool
            .execute(
                serde_json::json!({"container": "searxng"}),
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

    #[tokio::test]
    async fn test_docker_exec_invalid_container_name() {
        let tool = DockerExecTool::new("192.0.2.20", "deploy");
        let result = tool
            .execute(
                serde_json::json!({"container": "foo;rm -rf /", "command": "ls"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, message } => {
                assert_eq!(code, "INVALID_PARAMS");
                assert!(message.contains("invalid characters"));
            }
            _ => panic!("Expected error"),
        }
    }

    #[tokio::test]
    async fn test_docker_exec_empty_params() {
        let tool = DockerExecTool::new("192.0.2.20", "deploy");
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
    }

    #[test]
    fn test_validate_docker_command_safe() {
        assert!(DockerExecTool::validate_docker_command("cat /etc/hostname").is_ok());
        assert!(DockerExecTool::validate_docker_command("ls -la /tmp").is_ok());
        assert!(DockerExecTool::validate_docker_command("echo hello world").is_ok());
    }

    #[test]
    fn test_validate_docker_command_dangerous() {
        assert!(DockerExecTool::validate_docker_command("cat /etc/passwd; rm -rf /").is_err());
        assert!(DockerExecTool::validate_docker_command("echo $(whoami)").is_err());
        assert!(DockerExecTool::validate_docker_command("ls | grep foo").is_err());
        assert!(DockerExecTool::validate_docker_command("cmd & bg").is_err());
        assert!(DockerExecTool::validate_docker_command("echo `id`").is_err());
        assert!(DockerExecTool::validate_docker_command("echo ${HOME}").is_err());
        assert!(DockerExecTool::validate_docker_command("cat < /etc/shadow").is_err());
        assert!(DockerExecTool::validate_docker_command("echo > /tmp/out").is_err());
    }

    #[tokio::test]
    async fn test_docker_exec_rejects_dangerous_command() {
        let tool = DockerExecTool::new("192.0.2.20", "deploy");
        let result = tool
            .execute(
                serde_json::json!({"container": "searxng", "command": "cat /etc/passwd; rm -rf /"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, message } => {
                assert_eq!(code, "INVALID_PARAMS");
                assert!(message.contains("disallowed"));
            }
            _ => panic!("Expected error"),
        }
    }
}
