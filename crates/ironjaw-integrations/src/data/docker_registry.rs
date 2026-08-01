use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct DockerRegistryTool {
    base_url: String,
    client: reqwest::Client,
}

impl DockerRegistryTool {
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
impl Tool for DockerRegistryTool {
    fn name(&self) -> &str {
        "docker_registry"
    }
    fn description(&self) -> &str {
        "Query the private Docker registry on docker-host: list images (catalog), list tags for an image, inspect a manifest. Use for 'what images do we have', 'what versions of X are pushed'."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "action":{"type":"string","enum":["catalog","tags","manifest"],"default":"catalog"},
                "image":{"type":"string","description":"Image name (for tags/manifest)"},
                "tag":{"type":"string","description":"Tag (for manifest)"}
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
            .unwrap_or("catalog");
        let url = match action {
            "catalog" => format!("{}/v2/_catalog", self.base_url),
            "tags" => {
                let img = params.get("image").and_then(|v| v.as_str()).unwrap_or("");
                if img.is_empty() {
                    return ToolResult::error("INVALID_PARAMS", "image required");
                }
                format!("{}/v2/{}/tags/list", self.base_url, img)
            }
            "manifest" => {
                let img = params.get("image").and_then(|v| v.as_str()).unwrap_or("");
                let tag = params
                    .get("tag")
                    .and_then(|v| v.as_str())
                    .unwrap_or("latest");
                if img.is_empty() {
                    return ToolResult::error("INVALID_PARAMS", "image required");
                }
                format!("{}/v2/{}/manifests/{}", self.base_url, img, tag)
            }
            _ => return ToolResult::error("INVALID_PARAMS", "Invalid action"),
        };
        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("Registry: {e}")),
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
            DockerRegistryTool::new("http://192.0.2.20:5000").name(),
            "docker_registry"
        );
    }
}
