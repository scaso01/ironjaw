use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// GPT-Researcher — alternative deep-research engine (different traversal heuristic).
pub struct GptResearcherTool {
    base_url: String,
    client: reqwest::Client,
}

impl GptResearcherTool {
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
impl Tool for GptResearcherTool {
    fn name(&self) -> &str {
        "gpt_researcher"
    }

    fn description(&self) -> &str {
        "GPT-Researcher: alternative deep-research engine. Use to cross-check or as a fallback when deep_research is unavailable. Slow (3-8 min) — not for simple lookups."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Research question" },
                "report_type": { "type": "string", "description": "research_report | resource_report | outline_report", "default": "research_report" }
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
        let report_type = params
            .get("report_type")
            .and_then(|v| v.as_str())
            .unwrap_or("research_report");

        let body = serde_json::json!({ "task": query, "report_type": report_type, "report_source": "web" });
        let url = format!("{}/report/", self.base_url);
        let response = match self.client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                return ToolResult::error("HTTP_ERROR", format!("GPT-Researcher failed: {e}"))
            }
        };
        if !response.status().is_success() {
            return ToolResult::error("GPTR_ERROR", format!("Returned {}", response.status()));
        }
        let body: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(_) => serde_json::json!({}),
        };
        ToolResult::json(serde_json::json!({
            "report": body.get("report").cloned().unwrap_or(serde_json::Value::Null),
            "sources": body.get("sources").cloned().unwrap_or(serde_json::Value::Null),
            "query": query,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[test]
    fn meta() {
        let t = GptResearcherTool::new("http://192.0.2.20:8301");
        assert_eq!(t.name(), "gpt_researcher");
    }
    #[tokio::test]
    async fn missing() {
        let r = GptResearcherTool::new("http://192.0.2.20:8301")
            .execute(
                serde_json::json!({}),
                &ToolContext::new("t", "t", PathBuf::from("/tmp")),
                CancellationToken::new(),
            )
            .await;
        assert!(r.is_error());
    }
}
