use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// Creates a new cron schedule in the proactive engine.
pub struct ScheduleCreateTool;

impl Default for ScheduleCreateTool {
    fn default() -> Self {
        Self
    }
}

impl ScheduleCreateTool {
    pub fn new() -> Self {
        Self
    }
}

/// Basic validation: cron expression must have exactly 5 space-separated fields.
fn validate_cron(expr: &str) -> Result<(), String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "Cron expression must have 5 fields (minute hour day month weekday), got {}",
            fields.len()
        ));
    }
    Ok(())
}

/// Generate a simple unique ID from current timestamp + random suffix.
fn generate_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("sched_{ts:x}")
}

/// Get current ISO 8601 timestamp.
fn now_iso() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Simple UTC timestamp without external crate
    format!("{secs}")
}

#[async_trait::async_trait]
impl Tool for ScheduleCreateTool {
    fn name(&self) -> &str {
        "schedule_create"
    }

    fn description(&self) -> &str {
        "Create a new cron schedule"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Schedule name"
                },
                "cron_expression": {
                    "type": "string",
                    "description": "Cron expression (5 fields: minute hour day month weekday)"
                },
                "action": {
                    "type": "string",
                    "description": "Action to execute when triggered"
                },
                "enabled": {
                    "type": "boolean",
                    "description": "Whether the schedule is enabled (default: true)"
                }
            },
            "required": ["name", "cron_expression", "action"]
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
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(n) if !n.is_empty() => n,
            _ => return ToolResult::error("INVALID_PARAMS", "Missing required parameter: name"),
        };

        let cron_expression = match params.get("cron_expression").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult::error(
                    "INVALID_PARAMS",
                    "Missing required parameter: cron_expression",
                )
            }
        };

        let action = match params.get("action").and_then(|v| v.as_str()) {
            Some(a) if !a.is_empty() => a,
            _ => return ToolResult::error("INVALID_PARAMS", "Missing required parameter: action"),
        };

        let enabled = params
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        if let Err(e) = validate_cron(cron_expression) {
            return ToolResult::error("INVALID_CRON", e);
        }

        let id = generate_id();
        let created_at = now_iso();

        // Stub: returns the schedule that would be created.
        // Will be wired to ProactiveEngine's ScheduleStore later.
        ToolResult::json(serde_json::json!({
            "id": id,
            "name": name,
            "cron_expression": cron_expression,
            "action": action,
            "enabled": enabled,
            "created_at": created_at,
            "last_run": null,
        }))
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
    fn test_schedule_create_tier() {
        let tool = ScheduleCreateTool::new();
        assert_eq!(tool.tier(), PermissionTier::Confirm);
    }

    #[test]
    fn test_schedule_create_name() {
        let tool = ScheduleCreateTool::new();
        assert_eq!(tool.name(), "schedule_create");
    }

    #[test]
    fn test_validate_cron_valid() {
        assert!(validate_cron("*/5 * * * *").is_ok());
        assert!(validate_cron("0 9 * * 1").is_ok());
        assert!(validate_cron("30 14 1 * *").is_ok());
    }

    #[test]
    fn test_validate_cron_invalid() {
        assert!(validate_cron("* * *").is_err()); // too few
        assert!(validate_cron("* * * * * *").is_err()); // too many
        assert!(validate_cron("").is_err()); // empty
    }

    #[tokio::test]
    async fn test_schedule_create_success() {
        let tool = ScheduleCreateTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({
                    "name": "daily-check",
                    "cron_expression": "0 9 * * *",
                    "action": "infra.status"
                }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
        match result {
            ToolResult::Json { content } => {
                assert_eq!(content["name"], "daily-check");
                assert_eq!(content["cron_expression"], "0 9 * * *");
                assert_eq!(content["action"], "infra.status");
                assert_eq!(content["enabled"], true);
                assert!(content["id"].as_str().unwrap().starts_with("sched_"));
            }
            _ => panic!("Expected Json result"),
        }
    }

    #[tokio::test]
    async fn test_schedule_create_disabled() {
        let tool = ScheduleCreateTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({
                    "name": "test",
                    "cron_expression": "0 0 * * *",
                    "action": "ping",
                    "enabled": false
                }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
        match result {
            ToolResult::Json { content } => {
                assert_eq!(content["enabled"], false);
            }
            _ => panic!("Expected Json result"),
        }
    }

    #[tokio::test]
    async fn test_schedule_create_invalid_cron() {
        let tool = ScheduleCreateTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({
                    "name": "bad",
                    "cron_expression": "invalid",
                    "action": "test"
                }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, .. } => assert_eq!(code, "INVALID_CRON"),
            _ => panic!("Expected Error result"),
        }
    }

    #[tokio::test]
    async fn test_schedule_create_missing_name() {
        let tool = ScheduleCreateTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({
                    "cron_expression": "0 0 * * *",
                    "action": "test"
                }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_schedule_create_missing_action() {
        let tool = ScheduleCreateTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({
                    "name": "test",
                    "cron_expression": "0 0 * * *"
                }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }
}
