use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Uptime Kuma — query monitored-service health.
pub struct UptimeStatusTool {
    base_url: String,
    client: reqwest::Client,
}

impl UptimeStatusTool {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for UptimeStatusTool {
    fn name(&self) -> &str {
        "uptime_status"
    }
    fn description(&self) -> &str {
        "Get health of services monitored by Uptime Kuma on docker-host. Use for 'is X up', 'when did Y go down', 'show all monitored services' questions. Reads the public status page."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "status_page": { "type": "string", "description": "Status page slug (default: 'main')", "default": "main" }
            }
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
        let slug = params
            .get("status_page")
            .and_then(|v| v.as_str())
            .unwrap_or("main");
        if !slug
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return ToolResult::error("INVALID_PARAMS", "Invalid status page slug");
        }
        let url = format!("{}/api/status-page/{}", self.base_url, slug);
        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("UptimeKuma: {e}")),
        };
        if !resp.status().is_success() {
            return ToolResult::error("UK_ERROR", format!("UptimeKuma returned {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
        ToolResult::json(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[test]
    fn meta() {
        assert_eq!(
            UptimeStatusTool::new("http://192.0.2.20:3002").name(),
            "uptime_status"
        );
    }
    #[tokio::test]
    async fn invalid_slug() {
        let r = UptimeStatusTool::new("http://192.0.2.20:3002")
            .execute(
                serde_json::json!({"status_page": "bad slug!"}),
                &ToolContext::new("t", "t", PathBuf::from("/tmp")),
                CancellationToken::new(),
            )
            .await;
        assert!(r.is_error());
    }
}
