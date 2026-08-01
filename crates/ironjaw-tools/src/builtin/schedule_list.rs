use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use tokio_util::sync::CancellationToken;

/// Lists all cron schedules from the proactive engine.
pub struct ScheduleListTool;

impl Default for ScheduleListTool {
    fn default() -> Self {
        Self
    }
}

impl ScheduleListTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for ScheduleListTool {
    fn name(&self) -> &str {
        "schedule_list"
    }

    fn description(&self) -> &str {
        "List all cron schedules"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        // Stub: returns empty schedule list.
        // Will be wired to ProactiveEngine later.
        ToolResult::json(serde_json::json!({
            "schedules": [],
            "count": 0
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
    fn test_schedule_list_tier() {
        let tool = ScheduleListTool::new();
        assert_eq!(tool.tier(), PermissionTier::Auto);
    }

    #[test]
    fn test_schedule_list_name() {
        let tool = ScheduleListTool::new();
        assert_eq!(tool.name(), "schedule_list");
    }

    #[tokio::test]
    async fn test_schedule_list_returns_empty() {
        let tool = ScheduleListTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(serde_json::json!({}), &ctx, CancellationToken::new())
            .await;
        assert!(!result.is_error());
        match result {
            ToolResult::Json { content } => {
                assert_eq!(content["count"], 0);
                assert_eq!(content["schedules"].as_array().unwrap().len(), 0);
            }
            _ => panic!("Expected Json result"),
        }
    }

    #[test]
    fn test_schedule_list_schema() {
        let tool = ScheduleListTool::new();
        let schema = tool.schema();
        assert_eq!(schema["type"], "object");
    }
}
