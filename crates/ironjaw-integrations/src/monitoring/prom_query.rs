use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Prometheus PromQL query tool — for ISP-monitor metrics + node-exporter.
pub struct PromQueryTool {
    base_url: String,
    client: reqwest::Client,
}

impl PromQueryTool {
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
impl Tool for PromQueryTool {
    fn name(&self) -> &str {
        "prom_query"
    }
    fn description(&self) -> &str {
        "Run a PromQL query against Prometheus on docker-host (ISP metrics, node-exporter, container metrics). Use for time-series questions: 'CPU over last hour', 'packet loss to gateway', 'disk fill rate'."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "PromQL expression" },
                "range": { "type": "boolean", "description": "If true, range query over last N minutes; else instant.", "default": false },
                "minutes": { "type": "integer", "description": "Range window minutes (default 60)", "default": 60 }
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
        let range = params
            .get("range")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let minutes = params.get("minutes").and_then(|v| v.as_u64()).unwrap_or(60);
        let url = if range {
            let now = chrono::Utc::now().timestamp();
            let start = now - (minutes as i64) * 60;
            format!(
                "{}/api/v1/query_range?query={}&start={}&end={}&step=60",
                self.base_url,
                urlencode(query),
                start,
                now
            )
        } else {
            format!("{}/api/v1/query?query={}", self.base_url, urlencode(query))
        };
        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("Prometheus: {e}")),
        };
        if !resp.status().is_success() {
            return ToolResult::error("PROM_ERROR", format!("Returned {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
        ToolResult::json(body)
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
    #[test]
    fn meta() {
        assert_eq!(
            PromQueryTool::new("http://192.0.2.20:9091").name(),
            "prom_query"
        );
    }
    #[tokio::test]
    async fn missing() {
        let r = PromQueryTool::new("http://192.0.2.20:9091")
            .execute(
                serde_json::json!({}),
                &ToolContext::new("t", "t", PathBuf::from("/tmp")),
                CancellationToken::new(),
            )
            .await;
        assert!(r.is_error());
    }
}
