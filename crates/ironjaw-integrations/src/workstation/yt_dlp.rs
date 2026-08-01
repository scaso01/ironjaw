use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// yt-dlp — fetch metadata or download from YouTube + 1000 sites.
pub struct YtDlpTool;

impl YtDlpTool {
    pub fn new() -> Self {
        Self
    }
}
impl Default for YtDlpTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for YtDlpTool {
    fn name(&self) -> &str {
        "yt_dlp"
    }
    fn description(&self) -> &str {
        "Fetch video metadata via yt-dlp (titles, descriptions, durations, formats, comment counts). Default action 'info' returns JSON metadata only — does NOT download files. Action 'transcript' returns auto-captions when available."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "action":{"type":"string","enum":["info","transcript"],"default":"info"},
                "url":{"type":"string"}
            },
            "required":["url"]
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
        let url = match params.get("url").and_then(|v| v.as_str()) {
            Some(u) => u,
            None => return ToolResult::error("INVALID_PARAMS", "Missing url"),
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return ToolResult::error("INVALID_PARAMS", "url must be http(s)://");
        }
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("info");
        let args: Vec<&str> = match action {
            "info" => vec![
                "--dump-single-json",
                "--no-playlist",
                "--skip-download",
                url,
            ],
            "transcript" => vec![
                "--write-auto-sub",
                "--sub-lang",
                "en",
                "--skip-download",
                "--no-warnings",
                "-q",
                "--get-description",
                url,
            ],
            _ => return ToolResult::error("INVALID_PARAMS", "Invalid action"),
        };
        let result = tokio::time::timeout(
            Duration::from_secs(60),
            tokio::process::Command::new("yt-dlp").args(&args).output(),
        )
        .await;
        match result {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if action == "info" {
                    match serde_json::from_str::<serde_json::Value>(&stdout) {
                        Ok(v) => ToolResult::json(v),
                        Err(_) => ToolResult::text(stdout.to_string()),
                    }
                } else {
                    ToolResult::text(stdout.to_string())
                }
            }
            Ok(Err(e)) => ToolResult::error("EXEC_ERROR", format!("yt-dlp: {e}")),
            Err(_) => ToolResult::error("TIMEOUT", "yt-dlp timed out after 60s"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meta() {
        assert_eq!(YtDlpTool::new().name(), "yt_dlp");
    }
}
