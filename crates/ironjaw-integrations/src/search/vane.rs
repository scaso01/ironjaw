use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Vane search tool — AI-rewritten queries via the self-hosted Vane container
/// (Perplexica fork with 4 patches for context overflow, 131K limit).
///
/// API contract (vane:slim-latest, 2026-07): POST /api/search requires
/// `sources: ["web"|"academic"|"discussions"]` plus `chatModel`/`embeddingModel`
/// objects carrying a `providerId` + model `key` discovered via GET /api/providers.
/// The pre-rebrand `focusMode` body gets 400 "Missing sources or query".
pub struct VaneTool {
    base_url: String,
    client: reqwest::Client,
}

/// Maps the tool's legacy `focus_mode` param onto the new `sources` array.
pub(crate) fn sources_for_focus_mode(focus_mode: &str) -> Vec<&'static str> {
    match focus_mode {
        "academicSearch" => vec!["academic"],
        "redditSearch" => vec!["discussions"],
        _ => vec!["web"],
    }
}

impl VaneTool {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(180))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    /// Discovers (providerId, model key) pairs for chat + embedding from
    /// GET /api/providers. Picks the first provider offering each model kind.
    async fn discover_models(&self) -> Result<(serde_json::Value, serde_json::Value), ToolResult> {
        let url = format!("{}/api/providers", self.base_url);
        let response = self.client.get(&url).send().await.map_err(|e| {
            ToolResult::error("HTTP_ERROR", format!("Vane providers request failed: {e}"))
        })?;
        if !response.status().is_success() {
            let status = response.status();
            return Err(ToolResult::error(
                "VANE_ERROR",
                format!("Vane providers returned {status}"),
            ));
        }
        let body: serde_json::Value = response.json().await.map_err(|e| {
            ToolResult::error("PARSE_ERROR", format!("Vane providers parse failed: {e}"))
        })?;

        let empty = vec![];
        let providers = body
            .get("providers")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty);

        let pick = |kind: &str| -> Option<serde_json::Value> {
            providers.iter().find_map(|p| {
                let models = p.get(kind)?.as_array()?;
                let key = models.first()?.get("key")?.as_str()?;
                let provider_id = p.get("id")?.as_str()?;
                Some(serde_json::json!({ "providerId": provider_id, "key": key }))
            })
        };

        let chat = pick("chatModels").ok_or_else(|| {
            ToolResult::error(
                "VANE_ERROR",
                "Vane has no provider with chat models configured",
            )
        })?;
        let embedding = pick("embeddingModels").ok_or_else(|| {
            ToolResult::error(
                "VANE_ERROR",
                "Vane has no provider with embedding models configured",
            )
        })?;
        Ok((chat, embedding))
    }
}

#[async_trait::async_trait]
impl Tool for VaneTool {
    fn name(&self) -> &str {
        "vane"
    }

    fn description(&self) -> &str {
        "AI-synthesized public-web search via Vane (Perplexica fork, AI-rewritten queries, ~2 min). For external questions only — not for local projects/files."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "focus_mode": {
                    "type": "string",
                    "description": "Search focus: webSearch (default), academicSearch, redditSearch",
                    "default": "webSearch"
                }
            },
            "required": ["query"]
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
        let query = match params.get("query").and_then(|v| v.as_str()) {
            Some(q) => q,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: query")
            }
        };
        let focus_mode = params
            .get("focus_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("webSearch");

        let (chat_model, embedding_model) = match self.discover_models().await {
            Ok(pair) => pair,
            Err(e) => return e,
        };

        let body = serde_json::json!({
            "chatModel": chat_model,
            "embeddingModel": embedding_model,
            "sources": sources_for_focus_mode(focus_mode),
            "query": query,
            "optimizationMode": "speed",
            "history": [],
        });

        let url = format!("{}/api/search", self.base_url);
        let response = match self.client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("Vane request failed: {e}")),
        };

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return ToolResult::error("VANE_ERROR", format!("Vane returned {status}: {text}"));
        }

        let body: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => return ToolResult::error("PARSE_ERROR", format!("Vane parse failed: {e}")),
        };

        ToolResult::json(serde_json::json!({
            "answer": body.get("message").and_then(|v| v.as_str()).unwrap_or(""),
            "sources": body.get("sources").cloned().unwrap_or(serde_json::Value::Null),
            "query": query,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_ctx() -> ToolContext {
        ToolContext::new("test", "test", PathBuf::from("/tmp"))
    }

    #[test]
    fn test_vane_metadata() {
        let tool = VaneTool::new("http://192.0.2.20:3004");
        assert_eq!(tool.name(), "vane");
        assert_eq!(tool.tier(), PermissionTier::Auto);
    }

    #[tokio::test]
    async fn test_vane_missing_query() {
        let tool = VaneTool::new("http://192.0.2.20:3004");
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
    }

    #[test]
    fn test_sources_for_focus_mode() {
        assert_eq!(sources_for_focus_mode("webSearch"), vec!["web"]);
        assert_eq!(sources_for_focus_mode("academicSearch"), vec!["academic"]);
        assert_eq!(sources_for_focus_mode("redditSearch"), vec!["discussions"]);
        // Legacy modes with no new-API equivalent fall back to web
        assert_eq!(sources_for_focus_mode("wolframAlphaSearch"), vec!["web"]);
        assert_eq!(sources_for_focus_mode("writingAssistant"), vec!["web"]);
    }
}
