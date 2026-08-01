use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// Publishes a named event for proactive workflows.
pub struct EventPublishTool;

impl Default for EventPublishTool {
    fn default() -> Self {
        Self
    }
}

impl EventPublishTool {
    pub fn new() -> Self {
        Self
    }
}

/// Validate that the event name contains only alphanumeric characters and underscores.
fn validate_event_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Event name cannot be empty".to_string());
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err(format!(
            "Event name '{name}' contains invalid characters (only alphanumeric and underscores allowed)"
        ));
    }
    Ok(())
}

/// Get current timestamp as epoch seconds string.
fn now_timestamp() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", dur.as_secs())
}

#[async_trait::async_trait]
impl Tool for EventPublishTool {
    fn name(&self) -> &str {
        "event_publish"
    }

    fn description(&self) -> &str {
        "Publish a named event for proactive workflows"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "event_name": {
                    "type": "string",
                    "description": "Name of the event (alphanumeric and underscores only)"
                },
                "payload": {
                    "type": "object",
                    "description": "Optional payload data for the event"
                }
            },
            "required": ["event_name"]
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
        let event_name = match params.get("event_name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => {
                return ToolResult::error(
                    "INVALID_PARAMS",
                    "Missing required parameter: event_name",
                )
            }
        };

        if let Err(e) = validate_event_name(event_name) {
            return ToolResult::error("INVALID_EVENT_NAME", e);
        }

        let payload = params
            .get("payload")
            .cloned()
            .unwrap_or(serde_json::json!(null));
        let timestamp = now_timestamp();

        // Stub: returns confirmation.
        // Will be wired to ProactiveEngine's event bus later.
        ToolResult::json(serde_json::json!({
            "published": true,
            "event_name": event_name,
            "payload": payload,
            "timestamp": timestamp,
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
    fn test_event_publish_tier() {
        let tool = EventPublishTool::new();
        assert_eq!(tool.tier(), PermissionTier::Notify);
    }

    #[test]
    fn test_event_publish_name() {
        let tool = EventPublishTool::new();
        assert_eq!(tool.name(), "event_publish");
    }

    #[test]
    fn test_validate_event_name_valid() {
        assert!(validate_event_name("build_complete").is_ok());
        assert!(validate_event_name("test123").is_ok());
        assert!(validate_event_name("A").is_ok());
        assert!(validate_event_name("health_check_passed").is_ok());
    }

    #[test]
    fn test_validate_event_name_invalid() {
        assert!(validate_event_name("").is_err());
        assert!(validate_event_name("has space").is_err());
        assert!(validate_event_name("has-dash").is_err());
        assert!(validate_event_name("has.dot").is_err());
        assert!(validate_event_name("has/slash").is_err());
    }

    #[tokio::test]
    async fn test_event_publish_success() {
        let tool = EventPublishTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({
                    "event_name": "build_complete",
                    "payload": {"status": "ok"}
                }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
        match result {
            ToolResult::Json { content } => {
                assert_eq!(content["published"], true);
                assert_eq!(content["event_name"], "build_complete");
                assert_eq!(content["payload"]["status"], "ok");
                assert!(content["timestamp"].as_str().is_some());
            }
            _ => panic!("Expected Json result"),
        }
    }

    #[tokio::test]
    async fn test_event_publish_no_payload() {
        let tool = EventPublishTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({"event_name": "simple_event"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
        match result {
            ToolResult::Json { content } => {
                assert_eq!(content["published"], true);
                assert_eq!(content["event_name"], "simple_event");
            }
            _ => panic!("Expected Json result"),
        }
    }

    #[tokio::test]
    async fn test_event_publish_invalid_name() {
        let tool = EventPublishTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({"event_name": "bad-name"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, .. } => assert_eq!(code, "INVALID_EVENT_NAME"),
            _ => panic!("Expected Error result"),
        }
    }

    #[tokio::test]
    async fn test_event_publish_empty_name() {
        let tool = EventPublishTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(
                serde_json::json!({"event_name": ""}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_event_publish_missing_name() {
        let tool = EventPublishTool::new();
        let ctx = test_ctx();
        let result = tool
            .execute(serde_json::json!({}), &ctx, CancellationToken::new())
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, .. } => assert_eq!(code, "INVALID_PARAMS"),
            _ => panic!("Expected Error result"),
        }
    }
}
