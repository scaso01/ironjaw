use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// MeiliSearch tool — fast typo-tolerant full-text search over indexed content.
pub struct MeiliSearchTool {
    base_url: String,
    client: reqwest::Client,
}

impl MeiliSearchTool {
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
impl Tool for MeiliSearchTool {
    fn name(&self) -> &str {
        "meili_search"
    }

    fn description(&self) -> &str {
        "MeiliSearch full-text search (typo-tolerant, fast) over a named index. Use when you know the index name — call with index='my-docs', q='query'. Use list action with no index to discover available indexes."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["search", "list_indexes"], "default": "search" },
                "index": { "type": "string", "description": "Index name (required for search)" },
                "q": { "type": "string", "description": "Query string" },
                "limit": { "type": "integer", "default": 10 }
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
            .unwrap_or("search");
        match action {
            "list_indexes" => {
                let url = format!("{}/indexes", self.base_url);
                let resp = match self.client.get(&url).send().await {
                    Ok(r) => r,
                    Err(e) => return ToolResult::error("HTTP_ERROR", format!("Meili: {e}")),
                };
                let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
                ToolResult::json(body)
            }
            "search" => {
                let index = match params.get("index").and_then(|v| v.as_str()) {
                    Some(i) => i,
                    None => return ToolResult::error("INVALID_PARAMS", "search requires index"),
                };
                if !index
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                {
                    return ToolResult::error("INVALID_PARAMS", "Invalid index name");
                }
                let q = params.get("q").and_then(|v| v.as_str()).unwrap_or("");
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(10);
                let url = format!("{}/indexes/{}/search", self.base_url, index);
                let body = serde_json::json!({ "q": q, "limit": limit });
                let resp = match self.client.post(&url).json(&body).send().await {
                    Ok(r) => r,
                    Err(e) => return ToolResult::error("HTTP_ERROR", format!("Meili: {e}")),
                };
                if !resp.status().is_success() {
                    return ToolResult::error("MEILI_ERROR", format!("Returned {}", resp.status()));
                }
                let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
                ToolResult::json(body)
            }
            _ => ToolResult::error("INVALID_PARAMS", "Unknown action"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[test]
    fn meta() {
        assert_eq!(
            MeiliSearchTool::new("http://192.0.2.20:7700").name(),
            "meili_search"
        );
    }
    #[tokio::test]
    async fn search_missing_index() {
        let r = MeiliSearchTool::new("http://192.0.2.20:7700")
            .execute(
                serde_json::json!({"action":"search"}),
                &ToolContext::new("t", "t", PathBuf::from("/tmp")),
                CancellationToken::new(),
            )
            .await;
        assert!(r.is_error());
    }
}
