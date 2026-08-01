use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use tokio_util::sync::CancellationToken;

/// Enables or disables an existing cron schedule.
pub struct ScheduleToggleTool;

impl Default for ScheduleToggleTool {
    fn default() -> Self {
        Self
    }
}

impl ScheduleToggleTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for ScheduleToggleTool {
    fn name(&self) -> &str {
        "schedule_toggle"
    }

    fn description(&self) -> &str {
        "Enable or disable a cron schedule"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "schedule_id": {
                    "type": "string",
                    "description": "ID of the schedule to toggle"
                },
                "enabled": {
                    "type": "boolean",
                    "description": "Whether the schedule should be enabled"
                }
            },
            "required": ["schedule_id", "enabled"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Notify
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let schedule_id = match params.get("schedule_id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id,
            _ => {
                return ToolResult::error(
                    "INVALID_PARAMS",
                    "Missing required parameter: schedule_id",
                )
            }
        };

        let enabled = match params.get("enabled").and_then(|v| v.as_bool()) {
            Some(e) => e,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: enabled")
            }
        };

        // Stub: returns the toggled state.
        // Will be wired to ProactiveEngine's ScheduleStore later.
        ToolResult::json(serde_json::json!({
            "schedule_id": schedule_id,
            "enabled": enabled,
            "updated": true,
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
    fn test_schedule_toggle_tier() {
        let tool = ScheduleToggleTool::new();
        assert_eq!(tool.tier(), PermissionTier::Notify);
    }

    #[test]
    fn test_schedule_toggle_name() {
        let tool = ScheduleToggleTool::new();
        assert_eq!(tool.name(), "schedule_toggle");
    }

    #[tokio::test]
    async fn test_schedule_toggle_enable() {
        let tool = ScheduleToggleTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({
                    "schedule_id": "sched_abc123",
                    "enabled": true
                }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
        match result {
            ToolResult::Json { content } => {
                assert_eq!(content["schedule_id"], "sched_abc123");
                assert_eq!(content["enabled"], true);
                assert_eq!(content["updated"], true);
            }
            _ => panic!("Expected Json result"),
        }
    }

    #[tokio::test]
    async fn test_schedule_toggle_disable() {
        let tool = ScheduleToggleTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({
                    "schedule_id": "sched_xyz",
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
    async fn test_schedule_toggle_missing_id() {
        let tool = ScheduleToggleTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({"enabled": true}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_schedule_toggle_missing_enabled() {
        let tool = ScheduleToggleTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({"schedule_id": "sched_1"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }
}
