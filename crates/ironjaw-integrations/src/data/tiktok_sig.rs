use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct TiktokSigTool {
    base_url: String,
    client: reqwest::Client,
}

impl TiktokSigTool {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for TiktokSigTool {
    fn name(&self) -> &str {
        "tiktok_sig"
    }
    fn description(&self) -> &str {
        "Generate a signed TikTok request URL via the local tiktok-signature service. Used for TikTok scraping pipelines (e.g. fetching trending videos)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"url":{"type":"string","description":"TikTok URL to sign"}},"required":["url"]})
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
        let url = match params.get("url").and_then(|v| v.as_str()) {
            Some(u) => u,
            None => return ToolResult::error("INVALID_PARAMS", "Missing url"),
        };
        let body = serde_json::json!({ "url": url });
        let endpoint = format!("{}/signature", self.base_url);
        let resp = match self.client.post(&endpoint).json(&body).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("tiktok-sig: {e}")),
        };
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
            TiktokSigTool::new("http://192.0.2.20:18800").name(),
            "tiktok_sig"
        );
    }
}
