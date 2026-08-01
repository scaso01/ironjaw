use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// changedetection.io — query watched URLs and recent changes.
pub struct WatchedUrlsTool {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl WatchedUrlsTool {
    pub fn new(base_url: &str, api_key: Option<&str>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.map(|s| s.to_string()),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for WatchedUrlsTool {
    fn name(&self) -> &str {
        "watched_urls"
    }
    fn description(&self) -> &str {
        "List URLs being monitored by changedetection.io and their last-change status. Use for 'what URLs am I tracking', 'has X site changed recently'."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
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
        let url = format!("{}/api/v1/watch", self.base_url);
        let mut req = self.client.get(&url);
        if let Some(k) = &self.api_key {
            req = req.header("x-api-key", k);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("changedetection: {e}")),
        };
        if !resp.status().is_success() {
            return ToolResult::error("CD_ERROR", format!("Returned {}", resp.status()));
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
            WatchedUrlsTool::new("http://192.0.2.20:8086", None).name(),
            "watched_urls"
        );
    }
}
