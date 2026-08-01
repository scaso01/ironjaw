use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Khoj personal-AI query tool — searches the user's personal vault/notes.
pub struct KhojQueryTool {
    base_url: String,
    client: reqwest::Client,
}

impl KhojQueryTool {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(180))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for KhojQueryTool {
    fn name(&self) -> &str {
        "khoj_query"
    }

    fn description(&self) -> &str {
        "Query Khoj personal AI over the user's vault/notes/markdown. Use for questions whose answer is in personal documents (research notes, prior decisions, organized knowledge). Anonymous mode."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Question to ask Khoj" },
                "n": { "type": "integer", "description": "Max source documents (default 5)", "minimum": 1, "maximum": 20 }
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
        let n = params.get("n").and_then(|v| v.as_u64()).unwrap_or(5);

        let url = format!(
            "{}/api/chat?q={}&n={}&stream=false&client=ironjaw",
            self.base_url,
            urlencode(query),
            n
        );

        let response = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("Khoj request failed: {e}")),
        };

        if !response.status().is_success() {
            return ToolResult::error("KHOJ_ERROR", format!("Khoj returned {}", response.status()));
        }

        let text = response.text().await.unwrap_or_default();
        // Khoj may return JSON or text depending on endpoint mode.
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => ToolResult::json(serde_json::json!({
                "response": v.get("response").cloned().unwrap_or(serde_json::Value::String(text.clone())),
                "context": v.get("context").cloned().unwrap_or(serde_json::Value::Null),
                "query": query,
            })),
            Err(_) => ToolResult::json(serde_json::json!({
                "response": text,
                "query": query,
            })),
        }
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                String::from(b as char)
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_ctx() -> ToolContext {
        ToolContext::new("test", "test", PathBuf::from("/tmp"))
    }

    #[test]
    fn test_khoj_metadata() {
        let t = KhojQueryTool::new("http://192.0.2.20:42110");
        assert_eq!(t.name(), "khoj_query");
        assert_eq!(t.tier(), PermissionTier::Auto);
    }

    #[tokio::test]
    async fn test_khoj_missing_query() {
        let t = KhojQueryTool::new("http://192.0.2.20:42110");
        let r = t
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(r.is_error());
    }
}
