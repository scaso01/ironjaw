use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use tokio_util::sync::CancellationToken;

/// CC Chronicle tool — queries CC Chronicle for session summaries and search.
pub struct CcChronicleTool {
    base_url: String,
    client: reqwest::Client,
}

impl CcChronicleTool {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for CcChronicleTool {
    fn name(&self) -> &str {
        "cc_chronicle"
    }

    fn description(&self) -> &str {
        "Query CC Chronicle for session summaries, search across history"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action: query, recent, or insights",
                    "enum": ["query", "recent", "insights"]
                },
                "query": {
                    "type": "string",
                    "description": "Search query (required for query action)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results (default 20)",
                    "minimum": 1,
                    "maximum": 100
                }
            },
            "required": ["action"]
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
        let action = match params.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: action")
            }
        };

        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20);

        let url = match action {
            "recent" => format!("{}/api/sessions?limit={}", self.base_url, limit),
            "query" => {
                let query = match params.get("query").and_then(|v| v.as_str()) {
                    Some(q) => q,
                    None => {
                        return ToolResult::error(
                            "INVALID_PARAMS",
                            "query is required for query action",
                        )
                    }
                };
                format!("{}/api/search?q={}&limit={}", self.base_url, query, limit)
            }
            "insights" => format!("{}/api/insights", self.base_url),
            _ => {
                return ToolResult::error(
                    "INVALID_PARAMS",
                    "action must be 'query', 'recent', or 'insights'",
                )
            }
        };

        let response = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return ToolResult::error("HTTP_ERROR", format!("CC Chronicle request failed: {e}"))
            }
        };

        match response.json::<serde_json::Value>().await {
            Ok(data) => ToolResult::json(data),
            Err(e) => ToolResult::error(
                "PARSE_ERROR",
                format!("Failed to parse CC Chronicle response: {e}"),
            ),
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
    fn test_cc_chronicle_metadata() {
        let tool = CcChronicleTool::new("http://127.0.0.1:18793");
        assert_eq!(tool.name(), "cc_chronicle");
        assert_eq!(tool.tier(), PermissionTier::Auto);
    }

    #[tokio::test]
    async fn test_cc_chronicle_missing_action() {
        let tool = CcChronicleTool::new("http://127.0.0.1:18793");
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
    }
}
