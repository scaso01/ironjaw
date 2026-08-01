use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct RansomwatchTool {
    base_url: String,
    client: reqwest::Client,
}

impl RansomwatchTool {
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
impl Tool for RansomwatchTool {
    fn name(&self) -> &str {
        "ransomwatch"
    }
    fn description(&self) -> &str {
        "Query the local ransomwatch container — recent ransomware-leak postings tracked from public TOR sites. Use for cyber-intel research questions."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "properties": {
                "action": { "type":"string", "enum":["recent","groups","search"], "default":"recent" },
                "query": { "type":"string", "description":"For action=search" },
                "limit": { "type":"integer", "default":50 }
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
            .unwrap_or("recent");
        let url = match action {
            "recent" => format!(
                "{}/posts?limit={}",
                self.base_url,
                params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50)
            ),
            "groups" => format!("{}/groups", self.base_url),
            "search" => {
                let q = params.get("query").and_then(|v| v.as_str()).unwrap_or("");
                if q.is_empty() {
                    return ToolResult::error("INVALID_PARAMS", "query required");
                }
                format!("{}/search?q={}", self.base_url, q)
            }
            _ => return ToolResult::error("INVALID_PARAMS", "Invalid action"),
        };
        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("Ransomwatch: {e}")),
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
            RansomwatchTool::new("http://192.0.2.20:8000").name(),
            "ransomwatch"
        );
    }
}
