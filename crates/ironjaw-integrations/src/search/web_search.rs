use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use tokio_util::sync::CancellationToken;

use super::searxng::SearxngTool;
use super::vane::VaneTool;

/// Aggregator search tool — dispatches to SearXNG, Vane, or both in parallel.
pub struct WebSearchTool {
    searxng: SearxngTool,
    vane: VaneTool,
}

impl WebSearchTool {
    pub fn new(searxng_url: &str, vane_url: &str) -> Self {
        Self {
            searxng: SearxngTool::new(searxng_url),
            vane: VaneTool::new(vane_url),
        }
    }
}

#[async_trait::async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the public web using SearXNG (fast metasearch) + Vane (AI-synthesized) in parallel. ONLY for genuinely external questions (news, public docs, prices, weather). For local project questions use list_files / read_file / grep instead."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "engine": {
                    "type": "string",
                    "description": "Engine: searxng, vane, or both (default: both in parallel)",
                    "enum": ["searxng", "vane", "both"]
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
        ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let query = match params.get("query").and_then(|v| v.as_str()) {
            Some(q) => q,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: query")
            }
        };
        let engine = params
            .get("engine")
            .and_then(|v| v.as_str())
            .unwrap_or("both");
        let p = serde_json::json!({ "query": query });

        match engine {
            "searxng" => self.searxng.execute(p, ctx, CancellationToken::new()).await,
            "vane" => self.vane.execute(p, ctx, CancellationToken::new()).await,
            _ => {
                let (s, v) = tokio::join!(
                    self.searxng.execute(
                        serde_json::json!({"query": query}),
                        ctx,
                        CancellationToken::new()
                    ),
                    self.vane.execute(
                        serde_json::json!({"query": query}),
                        ctx,
                        CancellationToken::new()
                    ),
                );
                let s_data = match &s {
                    ToolResult::Json { content } => content.clone(),
                    ToolResult::Error { code, message } => {
                        serde_json::json!({"error":{"code":code,"message":message}})
                    }
                    _ => serde_json::Value::Null,
                };
                let v_data = match &v {
                    ToolResult::Json { content } => content.clone(),
                    ToolResult::Error { code, message } => {
                        serde_json::json!({"error":{"code":code,"message":message}})
                    }
                    _ => serde_json::Value::Null,
                };
                ToolResult::json(
                    serde_json::json!({"searxng": s_data, "vane": v_data, "query": query}),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn ctx() -> ToolContext {
        ToolContext::new("t", "t", PathBuf::from("/tmp"))
    }
    #[test]
    fn meta() {
        let t = WebSearchTool::new("http://192.0.2.20:8080", "http://192.0.2.20:3004");
        assert_eq!(t.name(), "web_search");
        assert_eq!(t.tier(), PermissionTier::Auto);
    }
    #[tokio::test]
    async fn missing_query() {
        let t = WebSearchTool::new("http://192.0.2.20:8080", "http://192.0.2.20:3004");
        let r = t
            .execute(serde_json::json!({}), &ctx(), CancellationToken::new())
            .await;
        assert!(r.is_error());
    }
}
