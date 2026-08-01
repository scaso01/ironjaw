use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct WhoisLookupTool {
    base_url: String,
    client: reqwest::Client,
}

impl WhoisLookupTool {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for WhoisLookupTool {
    fn name(&self) -> &str {
        "whois_lookup"
    }
    fn description(&self) -> &str {
        "WHOIS lookup for a domain (registrar, creation/expiry dates, nameservers, registrant) via local Whodat container."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"domain":{"type":"string"}},"required":["domain"]})
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
        let domain = match params.get("domain").and_then(|v| v.as_str()) {
            Some(d) => d,
            None => return ToolResult::error("INVALID_PARAMS", "Missing domain"),
        };
        if !domain
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '.')
        {
            return ToolResult::error("INVALID_PARAMS", "Invalid domain");
        }
        let url = format!("{}/api/whois/{}", self.base_url, domain);
        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("Whodat: {e}")),
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
            WhoisLookupTool::new("http://192.0.2.20:8083").name(),
            "whois_lookup"
        );
    }
}
