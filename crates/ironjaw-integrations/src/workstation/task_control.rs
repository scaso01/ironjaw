use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// schtasks wrapper — query/run/end Windows scheduled tasks.
pub struct TaskControlTool;

impl TaskControlTool {
    pub fn new() -> Self {
        Self
    }
}
impl Default for TaskControlTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for TaskControlTool {
    fn name(&self) -> &str {
        "task_control"
    }
    fn description(&self) -> &str {
        "Manage Windows scheduled tasks on workstation: query (list/status), run (start), end (terminate). Use for operational control of workstation services like Ironjaw Gateway, CC Bridge, Caddy, Llama Server, JobHunter Dashboard."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "action":{"type":"string","enum":["query","run","end"]},
                "task":{"type":"string","description":"Scheduled task name (omit for query/list)"}
            },
            "required":["action"]
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
        let action = match params.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => return ToolResult::error("INVALID_PARAMS", "Missing action"),
        };
        let task = params.get("task").and_then(|v| v.as_str());
        if let Some(t) = task {
            if !t.chars().all(|c| {
                c.is_alphanumeric()
                    || c == ' '
                    || c == '-'
                    || c == '_'
                    || c == '\\'
                    || c == '/'
                    || c == '.'
            }) {
                return ToolResult::error("INVALID_PARAMS", "Invalid task name characters");
            }
        }
        let args: Vec<String> = match action {
            "query" => match task {
                Some(t) => vec![
                    "/Query".into(),
                    "/TN".into(),
                    t.into(),
                    "/FO".into(),
                    "LIST".into(),
                    "/V".into(),
                ],
                None => vec!["/Query".into(), "/FO".into(), "CSV".into()],
            },
            "run" => {
                let t = match task {
                    Some(t) => t,
                    None => return ToolResult::error("INVALID_PARAMS", "task required for run"),
                };
                vec!["/Run".into(), "/TN".into(), t.into()]
            }
            "end" => {
                let t = match task {
                    Some(t) => t,
                    None => return ToolResult::error("INVALID_PARAMS", "task required for end"),
                };
                vec!["/End".into(), "/TN".into(), t.into()]
            }
            _ => return ToolResult::error("INVALID_PARAMS", "Invalid action"),
        };
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::process::Command::new("schtasks")
                .args(&args)
                .output(),
        )
        .await;
        match result {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                ToolResult::json(
                    serde_json::json!({"action": action, "task": task, "stdout": stdout, "stderr": stderr, "exit": out.status.code().unwrap_or(-1)}),
                )
            }
            Ok(Err(e)) => ToolResult::error("EXEC_ERROR", format!("schtasks: {e}")),
            Err(_) => ToolResult::error("TIMEOUT", "schtasks timed out"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meta() {
        assert_eq!(TaskControlTool::new().name(), "task_control");
    }
}
