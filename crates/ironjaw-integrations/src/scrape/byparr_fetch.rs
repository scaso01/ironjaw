use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Byparr — Cloudflare-bypass scraper (FlareSolverr fork).
pub struct ByparrFetchTool {
    base_url: String,
    client: reqwest::Client,
}

impl ByparrFetchTool {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(90))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for ByparrFetchTool {
    fn name(&self) -> &str {
        "byparr_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL through Byparr (FlareSolverr fork) to bypass Cloudflare/anti-bot protection. Use when crawl_page or normal HTTP fetch is blocked by a CAPTCHA challenge."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to fetch" },
                "max_timeout": { "type": "integer", "description": "Max wait ms (default 60000)" }
            },
            "required": ["url"]
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
        let url = match params.get("url").and_then(|v| v.as_str()) {
            Some(u) => u,
            None => return ToolResult::error("INVALID_PARAMS", "Missing required parameter: url"),
        };
        let max_timeout = params
            .get("max_timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(60000);

        let body = serde_json::json!({
            "cmd": "request.get",
            "url": url,
            "maxTimeout": max_timeout,
        });

        let endpoint = format!("{}/v1", self.base_url);
        let response = match self.client.post(&endpoint).json(&body).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("Byparr failed: {e}")),
        };

        if !response.status().is_success() {
            return ToolResult::error(
                "BYPARR_ERROR",
                format!("Byparr returned {}", response.status()),
            );
        }

        let body: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => return ToolResult::error("PARSE_ERROR", format!("Byparr parse failed: {e}")),
        };

        let solution = body
            .get("solution")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        ToolResult::json(serde_json::json!({
            "url": url,
            "status_code": solution.get("status").cloned().unwrap_or(serde_json::Value::Null),
            "response": solution.get("response").cloned().unwrap_or(serde_json::Value::Null),
            "cookies": solution.get("cookies").cloned().unwrap_or(serde_json::Value::Null),
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
    fn test_byparr_metadata() {
        let t = ByparrFetchTool::new("http://192.0.2.20:8191");
        assert_eq!(t.name(), "byparr_fetch");
    }

    #[tokio::test]
    async fn test_byparr_missing_url() {
        let t = ByparrFetchTool::new("http://192.0.2.20:8191");
        let r = t
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(r.is_error());
    }
}
