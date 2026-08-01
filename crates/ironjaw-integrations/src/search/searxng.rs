use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use tokio_util::sync::CancellationToken;

/// SearXNG search tool — queries the self-hosted SearXNG instance on docker-host.
pub struct SearxngTool {
    base_url: String,
    client: reqwest::Client,
}

impl SearxngTool {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for SearxngTool {
    fn name(&self) -> &str {
        "searxng"
    }

    fn description(&self) -> &str {
        "Public-web metasearch via self-hosted SearXNG (~25 backend engines, fast, 3-4s). ONLY for genuinely external questions — not for local content. For project/file questions use list_files or grep."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                },
                "categories": {
                    "type": "string",
                    "description": "Comma-separated categories (general, news, images, etc.)"
                },
                "time_range": {
                    "type": "string",
                    "description": "Time filter: day, week, month, year"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default 10)",
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": ["query"]
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
        let query = match params.get("query").and_then(|v| v.as_str()) {
            Some(q) => q,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: query")
            }
        };

        let categories = params
            .get("categories")
            .and_then(|v| v.as_str())
            .unwrap_or("general");
        let time_range = params.get("time_range").and_then(|v| v.as_str());
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

        let mut url = format!(
            "{}/search?q={}&format=json&categories={}",
            self.base_url, query, categories
        );
        if let Some(tr) = time_range {
            url.push_str(&format!("&time_range={tr}"));
        }

        let response = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return ToolResult::error("HTTP_ERROR", format!("SearXNG request failed: {e}"))
            }
        };

        let body: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                return ToolResult::error(
                    "PARSE_ERROR",
                    format!("Failed to parse SearXNG response: {e}"),
                )
            }
        };

        let results = body
            .get("results")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .take(limit)
                    .map(|r| {
                        serde_json::json!({
                            "title": r.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                            "url": r.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                            "content": r.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                            "engine": r.get("engine").and_then(|v| v.as_str()).unwrap_or(""),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        ToolResult::json(serde_json::json!({
            "results": results,
            "count": results.len(),
            "query": query,
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
    fn test_searxng_tier() {
        let tool = SearxngTool::new("http://192.0.2.20:8080");
        assert_eq!(tool.tier(), PermissionTier::Auto);
        assert_eq!(tool.name(), "searxng");
    }

    #[tokio::test]
    async fn test_searxng_missing_query() {
        let tool = SearxngTool::new("http://192.0.2.20:8080");
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
    }

    #[test]
    fn test_searxng_schema_has_required() {
        let tool = SearxngTool::new("http://192.0.2.20:8080");
        let schema = tool.schema();
        assert_eq!(schema["required"], serde_json::json!(["query"]));
    }
}
