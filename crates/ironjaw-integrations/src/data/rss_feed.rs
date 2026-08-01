use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// RSSHub — pull RSS for any source supported by the local container.
pub struct RssFeedTool {
    base_url: String,
    client: reqwest::Client,
}

impl RssFeedTool {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for RssFeedTool {
    fn name(&self) -> &str {
        "rss_feed"
    }
    fn description(&self) -> &str {
        "Fetch an RSS/Atom feed from RSSHub (e.g. /github/issue/anthropics/claude-code, /reddit/r/rust, /twitter/user/elonmusk). Returns recent entries. Use for any 'latest from X' question."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"RSSHub path beginning with /"},"limit":{"type":"integer","default":10}},"required":["path"]})
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
        let path = match params.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::error("INVALID_PARAMS", "Missing path"),
        };
        if !path.starts_with('/') {
            return ToolResult::error("INVALID_PARAMS", "path must start with /");
        }
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(10);
        let url = format!("{}{}?format=json&limit={}", self.base_url, path, limit);
        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("RSSHub: {e}")),
        };
        if !resp.status().is_success() {
            return ToolResult::error("RSS_ERROR", format!("Returned {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
        ToolResult::json(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meta() {
        assert_eq!(
            RssFeedTool::new("http://192.0.2.20:1200").name(),
            "rss_feed"
        );
    }
}
