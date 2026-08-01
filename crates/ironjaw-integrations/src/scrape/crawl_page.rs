use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Crawl4AI tool — JS-rendered page extraction with markdown output.
pub struct CrawlPageTool {
    base_url: String,
    client: reqwest::Client,
}

impl CrawlPageTool {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for CrawlPageTool {
    fn name(&self) -> &str {
        "crawl_page"
    }

    fn description(&self) -> &str {
        "Fetch a web page with full JavaScript rendering via Crawl4AI. Returns clean markdown. Use this when raw HTTP fetch returns mostly JS skeleton or when you need post-render content."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to crawl" },
                "css_selector": { "type": "string", "description": "Optional CSS selector to focus extraction" },
                "wait_for": { "type": "string", "description": "Optional CSS selector to wait for before extracting" }
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

        let mut body = serde_json::json!({
            "urls": [url],
            "crawler_config": { "type": "CrawlerRunConfig", "params": {} },
        });
        if let Some(sel) = params.get("css_selector").and_then(|v| v.as_str()) {
            body["crawler_config"]["params"]["css_selector"] =
                serde_json::Value::String(sel.into());
        }
        if let Some(wait) = params.get("wait_for").and_then(|v| v.as_str()) {
            body["crawler_config"]["params"]["wait_for"] = serde_json::Value::String(wait.into());
        }

        let endpoint = format!("{}/crawl", self.base_url);
        let response = match self.client.post(&endpoint).json(&body).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("Crawl4AI failed: {e}")),
        };

        if !response.status().is_success() {
            return ToolResult::error(
                "CRAWL_ERROR",
                format!("Crawl4AI returned {}", response.status()),
            );
        }

        let body: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                return ToolResult::error("PARSE_ERROR", format!("Crawl4AI parse failed: {e}"))
            }
        };

        // Crawl4AI returns { results: [{ url, markdown, html, ... }] }
        let results = body
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let first = results.first().cloned().unwrap_or(serde_json::Value::Null);

        ToolResult::json(serde_json::json!({
            "url": url,
            "markdown": first.get("markdown").cloned().unwrap_or(serde_json::Value::Null),
            "links": first.get("links").cloned().unwrap_or(serde_json::Value::Null),
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
    fn test_crawl_metadata() {
        let t = CrawlPageTool::new("http://192.0.2.20:11235");
        assert_eq!(t.name(), "crawl_page");
        assert_eq!(t.tier(), PermissionTier::Notify);
    }

    #[tokio::test]
    async fn test_crawl_missing_url() {
        let t = CrawlPageTool::new("http://192.0.2.20:11235");
        let r = t
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(r.is_error());
    }
}
