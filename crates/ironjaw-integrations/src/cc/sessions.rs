use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use tokio_util::sync::CancellationToken;

/// CC Bridge sessions tool — queries CC Bridge for session listing and search.
pub struct CcSessionsTool {
    base_url: String,
    client: reqwest::Client,
}

impl CcSessionsTool {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for CcSessionsTool {
    fn name(&self) -> &str {
        "cc_sessions"
    }

    fn description(&self) -> &str {
        "List or search Claude Code sessions via CC Bridge"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action: list or search",
                    "enum": ["list", "search"]
                },
                "query": {
                    "type": "string",
                    "description": "Search query (required for search action)"
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
            "list" => format!("{}/api/sessions?limit={}", self.base_url, limit),
            "search" => {
                let query = match params.get("query").and_then(|v| v.as_str()) {
                    Some(q) => q,
                    None => {
                        return ToolResult::error(
                            "INVALID_PARAMS",
                            "query is required for search action",
                        )
                    }
                };
                format!("{}/api/search?q={}&limit={}", self.base_url, query, limit)
            }
            _ => return ToolResult::error("INVALID_PARAMS", "action must be 'list' or 'search'"),
        };

        let response = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return ToolResult::error("HTTP_ERROR", format!("CC Bridge request failed: {e}"))
            }
        };

        match response.json::<serde_json::Value>().await {
            Ok(data) => ToolResult::json(data),
            Err(e) => ToolResult::error(
                "PARSE_ERROR",
                format!("Failed to parse CC Bridge response: {e}"),
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
    fn test_cc_sessions_metadata() {
        let tool = CcSessionsTool::new("http://127.0.0.1:18790");
        assert_eq!(tool.name(), "cc_sessions");
        assert_eq!(tool.tier(), PermissionTier::Auto);
    }

    #[tokio::test]
    async fn test_cc_sessions_missing_action() {
        let tool = CcSessionsTool::new("http://127.0.0.1:18790");
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
    }
}
