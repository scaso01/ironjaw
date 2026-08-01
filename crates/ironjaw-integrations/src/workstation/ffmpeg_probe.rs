use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// ffprobe — read media metadata (duration, codec, resolution, bitrate, streams).
pub struct FfmpegProbeTool;

impl FfmpegProbeTool {
    pub fn new() -> Self {
        Self
    }
}
impl Default for FfmpegProbeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for FfmpegProbeTool {
    fn name(&self) -> &str {
        "ffmpeg_probe"
    }
    fn description(&self) -> &str {
        "Read media file metadata via ffprobe: duration, codecs, resolution, bitrate, stream layout. Path must be a local file under working_dir."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Local media file"}},"required":["path"]})
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let path = match params.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::error("INVALID_PARAMS", "Missing path"),
        };
        let resolved = if std::path::Path::new(path).is_absolute() {
            std::path::PathBuf::from(path)
        } else {
            ctx.working_dir.join(path)
        };
        if !resolved.exists() {
            return ToolResult::error(
                "NOT_FOUND",
                format!("File not found: {}", resolved.display()),
            );
        }
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "quiet",
                    "-print_format",
                    "json",
                    "-show_format",
                    "-show_streams",
                ])
                .arg(&resolved)
                .output(),
        )
        .await;
        match result {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                match serde_json::from_str::<serde_json::Value>(&stdout) {
                    Ok(v) => ToolResult::json(v),
                    Err(e) => ToolResult::error("PARSE_ERROR", format!("ffprobe parse: {e}")),
                }
            }
            Ok(Err(e)) => ToolResult::error("EXEC_ERROR", format!("ffprobe: {e}")),
            Err(_) => ToolResult::error("TIMEOUT", "ffprobe timed out"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meta() {
        assert_eq!(FfmpegProbeTool::new().name(), "ffmpeg_probe");
    }
}
