use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Claude-bridge — dispatch a task to Claude Code subprocess via local HTTP proxy.
pub struct ClaudeSubagentTool {
    base_url: String,
    client: reqwest::Client,
}

impl ClaudeSubagentTool {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for ClaudeSubagentTool {
    fn name(&self) -> &str {
        "claude_subagent"
    }
    fn description(&self) -> &str {
        "Dispatch a hard task to Claude Code (cloud) via the local claude-bridge proxy. Use ONLY when the local LLM cannot solve a problem on its own — complex reasoning, large refactors, multi-file analysis. Slow (30s-5min) and incurs Claude API cost. The subagent has no memory of this conversation; the prompt must be self-contained."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "prompt":{"type":"string","description":"Self-contained task description for the subagent"},
                "cwd":{"type":"string","description":"Working directory for the subagent (default: current)"}
            },
            "required":["prompt"]
        })
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Confirm
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let prompt = match params.get("prompt").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::error("INVALID_PARAMS", "Missing prompt"),
        };
        let cwd = params.get("cwd").and_then(|v| v.as_str());
        let mut body = serde_json::json!({ "prompt": prompt });
        if let Some(c) = cwd {
            body["cwd"] = serde_json::Value::String(c.into());
        }
        let url = format!("{}/run", self.base_url);
        let resp = match self.client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("claude-bridge: {e}")),
        };
        if !resp.status().is_success() {
            return ToolResult::error(
                "CB_ERROR",
                format!("claude-bridge returned {}", resp.status()),
            );
        }
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
            ClaudeSubagentTool::new("http://127.0.0.1:18796").name(),
            "claude_subagent"
        );
    }
}
