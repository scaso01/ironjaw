use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct CveLookupTool {
    base_url: String,
    client: reqwest::Client,
}

impl CveLookupTool {
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
impl Tool for CveLookupTool {
    fn name(&self) -> &str {
        "cve_lookup"
    }
    fn description(&self) -> &str {
        "Query the local cve-search database (offline mirror of NVD). Use for 'what's CVE-XXXX', 'recent CVEs in package X', security advisory questions."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "properties": {
                "action": { "type":"string", "enum": ["cve","search_text","vendor"] },
                "id": { "type":"string", "description": "CVE id, e.g. CVE-2024-1234" },
                "text": { "type":"string", "description": "Free-text search" },
                "vendor": { "type":"string" },
                "product": { "type":"string" }
            },
            "required":["action"]
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
        let action = match params.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => return ToolResult::error("INVALID_PARAMS", "Missing action"),
        };
        let url = match action {
            "cve" => {
                let id = params.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if !id.starts_with("CVE-") {
                    return ToolResult::error("INVALID_PARAMS", "id must start with CVE-");
                }
                format!("{}/api/cve/{}", self.base_url, id)
            }
            "search_text" => {
                let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if text.is_empty() {
                    return ToolResult::error("INVALID_PARAMS", "text required");
                }
                format!("{}/api/search/{}", self.base_url, text)
            }
            "vendor" => {
                let v = params.get("vendor").and_then(|v| v.as_str()).unwrap_or("");
                let p = params.get("product").and_then(|v| v.as_str()).unwrap_or("");
                if v.is_empty() || p.is_empty() {
                    return ToolResult::error("INVALID_PARAMS", "vendor + product required");
                }
                format!("{}/api/search/{}/{}", self.base_url, v, p)
            }
            _ => return ToolResult::error("INVALID_PARAMS", "Invalid action"),
        };
        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("cve-search: {e}")),
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
            CveLookupTool::new("http://192.0.2.20:5555").name(),
            "cve_lookup"
        );
    }
}
