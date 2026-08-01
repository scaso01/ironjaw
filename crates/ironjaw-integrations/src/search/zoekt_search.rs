use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Zoekt code-search tool — regex/text across 78 indexed repos
/// (30 user + 48 OSS deps). Daily reindex.
pub struct ZoektSearchTool {
    base_url: String,
    client: reqwest::Client,
}

impl ZoektSearchTool {
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
impl Tool for ZoektSearchTool {
    fn name(&self) -> &str {
        "zoekt_search"
    }

    fn description(&self) -> &str {
        "Regex/text code search across 78 indexed repos (30 user projects + 48 OSS deps like fastapi, scrapy, transformers, tokio, godot). Use for 'how does library X handle Y' or 'where is pattern Z used across our stack'. Returns file:line matches."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Zoekt query (supports `r:repo file:path lang:rust` filters)"
                },
                "num": { "type": "integer", "description": "Max matches (default 50)", "minimum": 1, "maximum": 500 }
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
        let num = params.get("num").and_then(|v| v.as_u64()).unwrap_or(50);

        let url = format!(
            "{}/api/search?q={}&num={}",
            self.base_url,
            urlencode(query),
            num
        );

        let response = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("Zoekt request failed: {e}")),
        };

        if !response.status().is_success() {
            return ToolResult::error(
                "ZOEKT_ERROR",
                format!("Zoekt returned {}", response.status()),
            );
        }

        let body: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => return ToolResult::error("PARSE_ERROR", format!("Zoekt parse failed: {e}")),
        };

        // Zoekt returns: { Result: { Files: [{Repository, FileName, LineMatches: [{LineNumber, Line, ...}]}] } }
        let files = body
            .get("Result")
            .and_then(|r| r.get("Files"))
            .and_then(|f| f.as_array())
            .cloned()
            .unwrap_or_default();

        let mut hits = Vec::new();
        for file in files.iter() {
            let repo = file
                .get("Repository")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let path = file.get("FileName").and_then(|v| v.as_str()).unwrap_or("");
            let lm = file
                .get("LineMatches")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for m in lm {
                let line_num = m.get("LineNumber").and_then(|v| v.as_u64()).unwrap_or(0);
                let line = m.get("Line").and_then(|v| v.as_str()).unwrap_or("");
                hits.push(serde_json::json!({
                    "repo": repo,
                    "path": path,
                    "line": line_num,
                    "content": line.trim(),
                }));
            }
        }

        ToolResult::json(serde_json::json!({
            "count": hits.len(),
            "matches": hits,
            "query": query,
        }))
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
    fn test_zoekt_metadata() {
        let tool = ZoektSearchTool::new("http://192.0.2.20:6070");
        assert_eq!(tool.name(), "zoekt_search");
        assert_eq!(tool.tier(), PermissionTier::Auto);
    }

    #[tokio::test]
    async fn test_zoekt_missing_query() {
        let tool = ZoektSearchTool::new("http://192.0.2.20:6070");
        let r = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(r.is_error());
    }
}
