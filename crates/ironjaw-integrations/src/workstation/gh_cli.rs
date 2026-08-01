use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// `gh` GitHub CLI — read-only PR/issue queries.
pub struct GhCliTool;

impl GhCliTool {
    pub fn new() -> Self {
        Self
    }
}
impl Default for GhCliTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for GhCliTool {
    fn name(&self) -> &str {
        "gh_cli"
    }
    fn description(&self) -> &str {
        "GitHub CLI — list/view PRs and issues across the user's repos. Read-only actions only. Use for 'open PRs in repo X', 'recent issues mentioning Y', 'CI status for PR #Z'."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "action":{"type":"string","enum":["pr_list","pr_view","issue_list","issue_view","run_list"]},
                "repo":{"type":"string","description":"owner/name (e.g. scaso01/ironjaw)"},
                "number":{"type":"integer"}
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
        let repo = params.get("repo").and_then(|v| v.as_str());
        let number = params.get("number").and_then(|v| v.as_u64());
        if let Some(r) = repo {
            if !r
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '/' || c == '.')
            {
                return ToolResult::error("INVALID_PARAMS", "Invalid repo");
            }
        }
        let mut args: Vec<String> = match action {
            "pr_list" => vec![
                "pr".into(),
                "list".into(),
                "--json".into(),
                "number,title,state,headRefName,author".into(),
            ],
            "pr_view" => {
                let n = match number {
                    Some(n) => n,
                    None => return ToolResult::error("INVALID_PARAMS", "number required"),
                };
                vec![
                    "pr".into(),
                    "view".into(),
                    n.to_string(),
                    "--json".into(),
                    "number,title,body,state,checks".into(),
                ]
            }
            "issue_list" => vec![
                "issue".into(),
                "list".into(),
                "--json".into(),
                "number,title,state,labels,author".into(),
            ],
            "issue_view" => {
                let n = match number {
                    Some(n) => n,
                    None => return ToolResult::error("INVALID_PARAMS", "number required"),
                };
                vec![
                    "issue".into(),
                    "view".into(),
                    n.to_string(),
                    "--json".into(),
                    "number,title,body,state,comments".into(),
                ]
            }
            "run_list" => vec![
                "run".into(),
                "list".into(),
                "--limit".into(),
                "10".into(),
                "--json".into(),
                "name,status,conclusion,createdAt".into(),
            ],
            _ => return ToolResult::error("INVALID_PARAMS", "Invalid action"),
        };
        if let Some(r) = repo {
            args.push("--repo".into());
            args.push(r.into());
        }
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new("gh").args(&args).output(),
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
            Ok(Err(e)) => ToolResult::error("EXEC_ERROR", format!("gh: {e}")),
            Err(_) => ToolResult::error("TIMEOUT", "gh timed out"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meta() {
        assert_eq!(GhCliTool::new().name(), "gh_cli");
    }
}
