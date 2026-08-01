use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Local Deep Researcher — LangGraph multi-step iterative research (slow, 3-8 min).
pub struct DeepResearchTool {
    base_url: String,
    client: reqwest::Client,
}

impl DeepResearchTool {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for DeepResearchTool {
    fn name(&self) -> &str {
        "deep_research"
    }

    fn description(&self) -> &str {
        "LangGraph iterative deep-web research (3-8 min). Use for research questions where breadth + depth matters: 'survey X across Y sources', 'comprehensive overview of Z'. Do NOT use for simple lookups — use searxng or vane instead."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Research question" },
                "iterations": { "type": "integer", "description": "Max research loops (default 3)", "minimum": 1, "maximum": 6 }
            },
            "required": ["query"]
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
        let query = match params.get("query").and_then(|v| v.as_str()) {
            Some(q) => q,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: query")
            }
        };
        let iters = params
            .get("iterations")
            .and_then(|v| v.as_u64())
            .unwrap_or(3);

        let body = serde_json::json!({ "input": { "research_topic": query, "max_loops": iters } });
        let url = format!("{}/runs/wait", self.base_url);
        let response = match self.client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("DeepResearch failed: {e}")),
        };
        if !response.status().is_success() {
            return ToolResult::error(
                "DR_ERROR",
                format!("DeepResearch returned {}", response.status()),
            );
        }
        let body: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                return ToolResult::error("PARSE_ERROR", format!("DeepResearch parse failed: {e}"))
            }
        };
        ToolResult::json(serde_json::json!({
            "summary": body.get("running_summary").cloned().unwrap_or(serde_json::Value::Null),
            "sources": body.get("sources_gathered").cloned().unwrap_or(serde_json::Value::Null),
            "query": query,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn test_ctx() -> ToolContext {
        ToolContext::new("t", "t", PathBuf::from("/tmp"))
    }
    #[test]
    fn meta() {
        let t = DeepResearchTool::new("http://192.0.2.20:8300");
        assert_eq!(t.name(), "deep_research");
    }
    #[tokio::test]
    async fn missing() {
        let r = DeepResearchTool::new("http://192.0.2.20:8300")
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(r.is_error());
    }
}
