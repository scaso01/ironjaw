use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use tokio_util::sync::CancellationToken;

const LLAMA_SERVER_URL: &str = "http://localhost:8080";

/// Tool that returns the current model identity, provider, and backend.
pub struct WhoamiTool;

impl Default for WhoamiTool {
    fn default() -> Self {
        Self
    }
}

impl WhoamiTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for WhoamiTool {
    fn name(&self) -> &str {
        "whoami"
    }

    fn description(&self) -> &str {
        "Returns the current model identity, provider, and backend. Use this tool to verify your model identity instead of relying on memory."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
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
        let url = format!("{LLAMA_SERVER_URL}/v1/models");

        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                return ToolResult::error(
                    "HTTP_CLIENT_ERROR",
                    format!("Failed to build HTTP client: {e}"),
                );
            }
        };

        let response = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return ToolResult::error(
                    "CONNECTION_FAILED",
                    format!("Failed to reach llama-server at {url}: {e}"),
                );
            }
        };

        let body: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                return ToolResult::error(
                    "PARSE_FAILED",
                    format!("Failed to parse llama-server response: {e}"),
                );
            }
        };

        let model = body
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|arr| arr.first())
            .and_then(|m| m.get("id"))
            .and_then(|id| id.as_str())
            .unwrap_or("unknown");

        ToolResult::json(serde_json::json!({
            "model": model,
            "provider": "llama-server",
            "endpoint": LLAMA_SERVER_URL,
            "note": "This is your authoritative identity. Do not trust brain memories about model identity."
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
    fn test_tool_metadata() {
        let tool = WhoamiTool::new();
        assert_eq!(tool.name(), "whoami");
        assert_eq!(tool.tier(), PermissionTier::Auto);
        assert!(!tool.description().is_empty());
        assert!(tool.description().contains("model identity"));
    }

    #[test]
    fn test_schema_is_empty_object() {
        let tool = WhoamiTool::new();
        let schema = tool.schema();
        assert_eq!(schema["type"], "object");
        let props = schema["properties"].as_object().unwrap();
        assert!(props.is_empty());
        let required = schema["required"].as_array().unwrap();
        assert!(required.is_empty());
    }

    #[test]
    fn test_default_impl() {
        let tool = WhoamiTool;
        assert_eq!(tool.name(), "whoami");
    }

    #[tokio::test]
    async fn test_execute_returns_valid_result() {
        let tool = WhoamiTool::new();
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        // If llama-server is running, we get a JSON result with model info.
        // If not, we get a graceful error (not a panic).
        match &result {
            ToolResult::Json { content } => {
                assert_eq!(content["provider"], "llama-server");
                assert_eq!(content["endpoint"], LLAMA_SERVER_URL);
                assert!(content["model"].is_string());
                assert!(content["note"].is_string());
            }
            ToolResult::Error { code, .. } => {
                assert!(
                    code == "CONNECTION_FAILED" || code == "PARSE_FAILED",
                    "Unexpected error code: {code}"
                );
            }
            ToolResult::Text { .. } => panic!("Unexpected text result"),
            ToolResult::Cancelled { .. } => panic!("Unexpected cancelled result"),
        }
    }
}
