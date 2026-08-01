use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// What's-Up-Docker (WUD) — list containers with available image updates.
pub struct ContainerUpdatesTool {
    base_url: String,
    client: reqwest::Client,
}

impl ContainerUpdatesTool {
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
impl Tool for ContainerUpdatesTool {
    fn name(&self) -> &str {
        "container_updates"
    }
    fn description(&self) -> &str {
        "List Docker containers on docker-host that have new image versions available (via What's-Up-Docker). Use for 'what containers need updates', 'show pending updates'."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "only_with_updates": { "type": "boolean", "description": "Show only containers with updates (default true)", "default": true }
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
        let only = params
            .get("only_with_updates")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let url = format!("{}/api/containers", self.base_url);
        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("WUD: {e}")),
        };
        if !resp.status().is_success() {
            return ToolResult::error("WUD_ERROR", format!("WUD returned {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!([]));
        let containers = body.as_array().cloned().unwrap_or_default();
        let filtered: Vec<_> = containers
            .into_iter()
            .filter(|c| {
                !only
                    || c.get("updateAvailable")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
            })
            .collect();
        ToolResult::json(serde_json::json!({ "count": filtered.len(), "containers": filtered }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[test]
    fn meta() {
        assert_eq!(
            ContainerUpdatesTool::new("http://192.0.2.20:3000").name(),
            "container_updates"
        );
    }
    #[tokio::test]
    async fn smoke() {
        let _ = ContainerUpdatesTool::new("http://127.0.0.1:1") // unreachable
            .execute(
                serde_json::json!({}),
                &ToolContext::new("t", "t", PathBuf::from("/tmp")),
                CancellationToken::new(),
            )
            .await;
    }
}
