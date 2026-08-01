use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use super::shell::sanitize_env;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_OUTPUT_BYTES: usize = 64 * 1024; // 64 KB

/// Execute PowerShell commands on Windows.
pub struct PowerShellTool;

impl Default for PowerShellTool {
    fn default() -> Self {
        Self
    }
}

impl PowerShellTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for PowerShellTool {
    fn name(&self) -> &str {
        "powershell"
    }

    fn description(&self) -> &str {
        "Execute a PowerShell command on Windows. Use this for cmdlets like `Get-Process`, \
         `Get-ChildItem`, `Get-Content`, `Get-Date`, `Select-Object`, `Where-Object`, etc. \
         Returns stdout, stderr, exit_code. Output capped at 64KB; 30s timeout. \
         On non-Windows, returns an error."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The PowerShell command or expression to execute"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds (default 30)",
                    "minimum": 1,
                    "maximum": 300
                }
            },
            "required": ["command"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Confirm
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult {
        // Non-Windows: return UNSUPPORTED immediately.
        #[cfg(not(windows))]
        {
            let _ = (params, ctx, cancel);
            return ToolResult::error("UNSUPPORTED", "powershell tool only available on Windows");
        }

        #[cfg(windows)]
        {
            let command = match params.get("command").and_then(|v| v.as_str()) {
                Some(cmd) => cmd,
                None => {
                    return ToolResult::error(
                        "INVALID_PARAMS",
                        "Missing required parameter: command",
                    )
                }
            };

            let timeout_secs = params
                .get("timeout_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_TIMEOUT_SECS);

            let timeout = Duration::from_secs(timeout_secs);

            // Streaming path: emit stdout lines in real-time via stream_tx
            if let Some(stream_tx) = ctx.stream_tx.clone() {
                use std::process::Stdio;
                use tokio::io::{AsyncBufReadExt, BufReader};

                let mut cmd = tokio::process::Command::new("powershell.exe");
                cmd.args(["-NoProfile", "-NonInteractive", "-Command", command])
                    .current_dir(&ctx.working_dir)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                sanitize_env(&mut cmd);

                let spawn_result = cmd.spawn();
                let mut child = match spawn_result {
                    Ok(c) => c,
                    Err(e) => {
                        return ToolResult::error(
                            "EXEC_FAILED",
                            format!("Failed to spawn powershell.exe: {e}"),
                        )
                    }
                };

                let stdout_handle = child.stdout.take().map(BufReader::new);
                let stderr_handle = child.stderr.take().map(BufReader::new);

                let mut stdout_lines: Vec<String> = Vec::new();
                let mut stderr_lines: Vec<String> = Vec::new();

                let drain_fut = tokio::time::timeout(timeout, async {
                    if let Some(mut reader) = stdout_handle {
                        let mut line = String::new();
                        loop {
                            line.clear();
                            match reader.read_line(&mut line).await {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {
                                    let trimmed =
                                        line.trim_end_matches('\n').trim_end_matches('\r');
                                    let _ = stream_tx.send(trimmed.to_string());
                                    stdout_lines.push(trimmed.to_string());
                                }
                            }
                        }
                    }
                    if let Some(mut reader) = stderr_handle {
                        let mut line = String::new();
                        loop {
                            line.clear();
                            match reader.read_line(&mut line).await {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {
                                    stderr_lines.push(
                                        line.trim_end_matches('\n')
                                            .trim_end_matches('\r')
                                            .to_string(),
                                    );
                                }
                            }
                        }
                    }
                    child.wait().await
                });

                let exit_code = tokio::select! {
                    stream_timeout = drain_fut => match stream_timeout {
                        Ok(Ok(status)) => status.code().unwrap_or(-1),
                        Ok(Err(_)) => -1,
                        Err(_) => {
                            let _ = child.kill().await;
                            return ToolResult::error(
                                "TIMEOUT",
                                format!("Command timed out after {timeout_secs} seconds"),
                            );
                        }
                    },
                    _ = cancel.cancelled() => {
                        let _ = child.kill().await;
                        return ToolResult::cancelled("PowerShell command was cancelled");
                    },
                };

                let mut stdout = stdout_lines.join("\n");
                let mut stderr = stderr_lines.join("\n");

                if stdout.len() > MAX_OUTPUT_BYTES {
                    stdout.truncate(MAX_OUTPUT_BYTES);
                    stdout.push_str("\n... (output truncated at 64KB)");
                }
                if stderr.len() > MAX_OUTPUT_BYTES {
                    stderr.truncate(MAX_OUTPUT_BYTES);
                    stderr.push_str("\n... (output truncated at 64KB)");
                }

                return ToolResult::json(serde_json::json!({
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                }));
            }

            // Non-streaming path
            let mut cmd = tokio::process::Command::new("powershell.exe");
            cmd.args(["-NoProfile", "-NonInteractive", "-Command", command])
                .current_dir(&ctx.working_dir)
                .kill_on_drop(true);
            sanitize_env(&mut cmd);

            let exec_fut = tokio::time::timeout(timeout, cmd.output());

            let result = tokio::select! {
                r = exec_fut => r,
                _ = cancel.cancelled() => {
                    return ToolResult::cancelled("PowerShell command was cancelled");
                },
            };

            match result {
                Ok(Ok(output)) => {
                    let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
                    let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();

                    if stdout.len() > MAX_OUTPUT_BYTES {
                        stdout.truncate(MAX_OUTPUT_BYTES);
                        stdout.push_str("\n... (output truncated at 64KB)");
                    }
                    if stderr.len() > MAX_OUTPUT_BYTES {
                        stderr.truncate(MAX_OUTPUT_BYTES);
                        stderr.push_str("\n... (output truncated at 64KB)");
                    }

                    let exit_code = output.status.code().unwrap_or(-1);

                    ToolResult::json(serde_json::json!({
                        "exit_code": exit_code,
                        "stdout": stdout,
                        "stderr": stderr,
                    }))
                }
                Ok(Err(e)) => {
                    ToolResult::error("EXEC_FAILED", format!("Failed to execute command: {e}"))
                }
                Err(_) => ToolResult::error(
                    "TIMEOUT",
                    format!("Command timed out after {timeout_secs} seconds"),
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> ToolContext {
        ToolContext::new("test", "test", std::env::temp_dir())
    }

    #[test]
    fn test_powershell_tier() {
        let tool = PowerShellTool::new();
        assert_eq!(tool.tier(), PermissionTier::Confirm);
    }

    #[test]
    fn test_powershell_name() {
        let tool = PowerShellTool::new();
        assert_eq!(tool.name(), "powershell");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_powershell_get_date_windows() {
        let tool = PowerShellTool::new();
        let result = tool
            .execute(
                serde_json::json!({"command": "Get-Date"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error(), "Get-Date should not error: {result:?}");
        match &result {
            ToolResult::Json { content } => {
                assert_eq!(content["exit_code"], 0, "exit_code should be 0");
                let stdout = content["stdout"].as_str().unwrap_or("");
                assert!(
                    !stdout.is_empty(),
                    "stdout should be non-empty for Get-Date"
                );
            }
            _ => panic!("Expected Json result, got: {result:?}"),
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_powershell_unsupported_on_non_windows() {
        let tool = PowerShellTool::new();
        let result = tool
            .execute(
                serde_json::json!({"command": "Get-Date"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error(), "Should return error on non-Windows");
        match &result {
            ToolResult::Error { code, .. } => {
                assert_eq!(code, "UNSUPPORTED");
            }
            _ => panic!("Expected Error result"),
        }
    }

    #[test]
    fn test_powershell_missing_command() {
        // Schema validation — just verify schema has required "command"
        let tool = PowerShellTool::new();
        let schema = tool.schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("command")));
    }
}
