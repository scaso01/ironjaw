use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// jcm — symbol search across user repos on workstation.
pub struct JcmTool;

impl JcmTool {
    pub fn new() -> Self {
        Self
    }
}
impl Default for JcmTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for JcmTool {
    fn name(&self) -> &str {
        "jcm"
    }
    fn description(&self) -> &str {
        "Symbol search (definitions, references, file outline) across ~25 indexed user repos on workstation. Refreshed every 4h. Use for 'where is function X defined in any of my repos', 'show outline of repo Y', 'find Z across all my projects'. Distinct from grep — this is symbol-aware, not text."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "action":{"type":"string","enum":["list","outline","search-symbols","file-outline","search-text"]},
                "repo":{"type":"string"},
                "query":{"type":"string"},
                "kind":{"type":"string","description":"function|class|method (for search-symbols)"},
                "path":{"type":"string","description":"For file-outline"}
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
        let mut args: Vec<String> = vec![action.to_string()];
        if let Some(r) = params.get("repo").and_then(|v| v.as_str()) {
            if !r
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
            {
                return ToolResult::error("INVALID_PARAMS", "Invalid repo");
            }
            args.push("--repo".into());
            args.push(r.into());
        }
        if let Some(q) = params.get("query").and_then(|v| v.as_str()) {
            args.push("--query".into());
            args.push(q.into());
        }
        if let Some(k) = params.get("kind").and_then(|v| v.as_str()) {
            if ![
                "function",
                "class",
                "method",
                "struct",
                "trait",
                "interface",
            ]
            .contains(&k)
            {
                return ToolResult::error("INVALID_PARAMS", "Invalid kind");
            }
            args.push("--kind".into());
            args.push(k.into());
        }
        if let Some(p) = params.get("path").and_then(|v| v.as_str()) {
            args.push("--path".into());
            args.push(p.into());
        }
        args.push("--json".into());
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new("jcm").args(&args).output(),
        )
        .await;
        match result {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                match serde_json::from_str::<serde_json::Value>(&stdout) {
                    Ok(v) => ToolResult::json(v),
                    Err(_) => ToolResult::text(stdout.to_string()),
                }
            }
            Ok(Err(e)) => ToolResult::error("EXEC_ERROR", format!("jcm: {e}")),
            Err(_) => ToolResult::error("TIMEOUT", "jcm timed out after 30s"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meta() {
        assert_eq!(JcmTool::new().name(), "jcm");
    }
}
