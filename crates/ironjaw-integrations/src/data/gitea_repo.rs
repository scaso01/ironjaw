use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Gitea — read-only access to self-hosted git repos.
pub struct GiteaRepoTool {
    base_url: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl GiteaRepoTool {
    pub fn new(base_url: &str, token: Option<&str>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.map(|s| s.to_string()),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait::async_trait]
impl Tool for GiteaRepoTool {
    fn name(&self) -> &str {
        "gitea_repo"
    }
    fn description(&self) -> &str {
        "Query the self-hosted Gitea instance: list user repos, list commits, read a file at a ref. Read-only. Use for any question about code in private/self-hosted repos that aren't on GitHub."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["list_repos", "list_commits", "get_file"] },
                "owner": { "type": "string" },
                "repo": { "type": "string" },
                "path": { "type": "string", "description": "File path (for get_file)" },
                "ref": { "type": "string", "description": "Branch/tag/sha (default main)" }
            },
            "required": ["action"]
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
        let url = match action {
            "list_repos" => format!("{}/api/v1/repos/search?limit=50", self.base_url),
            "list_commits" => {
                let owner = params.get("owner").and_then(|v| v.as_str()).unwrap_or("");
                let repo = params.get("repo").and_then(|v| v.as_str()).unwrap_or("");
                if owner.is_empty() || repo.is_empty() {
                    return ToolResult::error("INVALID_PARAMS", "owner and repo required");
                }
                format!(
                    "{}/api/v1/repos/{}/{}/commits?limit=20",
                    self.base_url, owner, repo
                )
            }
            "get_file" => {
                let owner = params.get("owner").and_then(|v| v.as_str()).unwrap_or("");
                let repo = params.get("repo").and_then(|v| v.as_str()).unwrap_or("");
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let r = params.get("ref").and_then(|v| v.as_str()).unwrap_or("main");
                if owner.is_empty() || repo.is_empty() || path.is_empty() {
                    return ToolResult::error("INVALID_PARAMS", "owner, repo, path required");
                }
                format!(
                    "{}/api/v1/repos/{}/{}/raw/{}?ref={}",
                    self.base_url, owner, repo, path, r
                )
            }
            _ => return ToolResult::error("INVALID_PARAMS", "Invalid action"),
        };
        let mut req = self.client.get(&url);
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("token {t}"));
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error("HTTP_ERROR", format!("Gitea: {e}")),
        };
        if !resp.status().is_success() {
            return ToolResult::error("GITEA_ERROR", format!("Returned {}", resp.status()));
        }
        if action == "get_file" {
            let text = resp.text().await.unwrap_or_default();
            ToolResult::text(text)
        } else {
            let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
            ToolResult::json(body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meta() {
        assert_eq!(
            GiteaRepoTool::new("http://192.0.2.20:3300", None).name(),
            "gitea_repo"
        );
    }
}
