use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// AdGuard Home — DNS analytics + blocklist queries.
pub struct AdguardStatsTool {
    base_url: String,
    auth: Option<String>,
    client: reqwest::Client,
}

impl AdguardStatsTool {
    pub fn new(base_url: &str, auth_basic: Option<&str>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth: auth_basic.map(|s| s.to_string()),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for AdguardStatsTool {
    fn name(&self) -> &str {
        "adguard_stats"
    }
    fn description(&self) -> &str {
        "Query AdGuard Home DNS server: stats (queries/blocked counts), top clients, top blocked domains, querylog. Use for 'how many DNS queries today', 'what's getting blocked', 'who's querying X'."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["stats", "querylog", "top_clients", "top_blocked"], "default": "stats" },
                "limit": { "type": "integer", "default": 50 }
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
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("stats");
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50);
        let path = match action {
            "stats" => "/control/stats".to_string(),
            "querylog" => format!("/control/querylog?limit={limit}"),
            "top_clients" | "top_blocked" => "/control/stats".to_string(),
            _ => return ToolResult::error("INVALID_PARAMS", "Invalid action"),
        };
        let url = format!("{}{}", self.base_url, path);
        let mut req = self.client.get(&url);
        if let Some(a) = &self.auth {
            req = req.header("Authorization", format!("Basic {a}"));
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("AdGuard: {e}")),
        };
        if !resp.status().is_success() {
            return ToolResult::error("AG_ERROR", format!("AdGuard returned {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
        let result = match action {
            "top_clients" => body
                .get("top_clients")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            "top_blocked" => body
                .get("top_blocked_domains")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            _ => body,
        };
        ToolResult::json(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meta() {
        assert_eq!(
            AdguardStatsTool::new("http://192.0.2.20:3006", None).name(),
            "adguard_stats"
        );
    }
}
