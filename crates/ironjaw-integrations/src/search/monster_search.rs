use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// MonsterSearch tool — unified web research via the `monster-search` CLI.
pub struct MonsterSearchTool;

impl MonsterSearchTool {
    pub fn new() -> Self {
        Self
    }

    /// Build the monster-search command arguments from parameters.
    /// Returns (binary_name, args) or an error message.
    pub fn build_args(params: &serde_json::Value) -> Result<Vec<String>, String> {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: query")?;

        if query.trim().is_empty() {
            return Err("query must not be empty".to_string());
        }

        let mut args = vec![query.to_string()];

        if let Some(engine) = params.get("engine").and_then(|v| v.as_str()) {
            args.push("--engine".to_string());
            args.push(engine.to_string());
        }

        if let Some(time_range) = params.get("time_range").and_then(|v| v.as_str()) {
            args.push("--time-range".to_string());
            args.push(time_range.to_string());
        }

        Ok(args)
    }
}

impl Default for MonsterSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for MonsterSearchTool {
    fn name(&self) -> &str {
        "monster_search"
    }

    fn description(&self) -> &str {
        "Unified PUBLIC-WEB research across 36+ engines (SearXNG, Marginalia, news, academic, \
         code search, AI synthesis). RRF-fused, deduped. ONLY for genuinely external questions \
         (news, public docs, prices, software). NOT for the user's own projects/files/local state — \
         those have local tools. Returns synthesized results from multiple sources."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                },
                "engine": {
                    "type": "string",
                    "description": "Search engine (default: all = all 6 query engines in parallel)",
                    "enum": ["all", "searxng", "marginalia", "perplexity", "perplexica", "local_researcher", "news", "crawl"]
                },
                "time_range": {
                    "type": "string",
                    "description": "Time range filter (e.g., 'day', 'week', 'month', 'year')"
                }
            }
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
        let args = match Self::build_args(&params) {
            Ok(args) => args,
            Err(msg) => return ToolResult::error("INVALID_PARAMS", msg),
        };

        let result = tokio::time::timeout(
            Duration::from_secs(600),
            tokio::process::Command::new("monster-search")
                .args(&args)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let exit_code = output.status.code().unwrap_or(-1);

                if exit_code != 0 {
                    return ToolResult::error(
                        "SEARCH_ERROR",
                        format!("monster-search failed (exit {exit_code}): {stderr}"),
                    );
                }

                ToolResult::json(serde_json::json!({
                    "results": stdout,
                    "stderr": stderr,
                }))
            }
            Ok(Err(e)) => {
                ToolResult::error("SEARCH_ERROR", format!("Failed to run monster-search: {e}"))
            }
            Err(_) => ToolResult::error("TIMEOUT", "monster-search timed out after 600s"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_ctx() -> ToolContext {
        ToolContext::new("test", "test", PathBuf::from("/tmp"))
    }

    // -- metadata tests --

    #[test]
    fn test_monster_search_metadata() {
        let tool = MonsterSearchTool::new();
        assert_eq!(tool.name(), "monster_search");
        assert_eq!(tool.tier(), PermissionTier::Auto);
    }

    #[test]
    fn test_monster_search_schema() {
        let tool = MonsterSearchTool::new();
        let schema = tool.schema();
        assert!(schema["properties"]["query"].is_object());
        assert!(schema["properties"]["engine"].is_object());
        assert!(schema["properties"]["time_range"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("query")));
    }

    #[test]
    fn test_monster_search_default() {
        let tool = MonsterSearchTool;
        assert_eq!(tool.name(), "monster_search");
    }

    // -- build_args tests --

    #[test]
    fn test_build_args_query_only() {
        let params = serde_json::json!({"query": "rust async patterns"});
        let args = MonsterSearchTool::build_args(&params).unwrap();
        assert_eq!(args, vec!["rust async patterns"]);
    }

    #[test]
    fn test_build_args_with_engine() {
        let params = serde_json::json!({"query": "rust async", "engine": "searxng"});
        let args = MonsterSearchTool::build_args(&params).unwrap();
        assert_eq!(args, vec!["rust async", "--engine", "searxng"]);
    }

    #[test]
    fn test_build_args_with_time_range() {
        let params = serde_json::json!({"query": "rust news", "time_range": "week"});
        let args = MonsterSearchTool::build_args(&params).unwrap();
        assert_eq!(args, vec!["rust news", "--time-range", "week"]);
    }

    #[test]
    fn test_build_args_all_params() {
        let params = serde_json::json!({
            "query": "rust async",
            "engine": "perplexica",
            "time_range": "month"
        });
        let args = MonsterSearchTool::build_args(&params).unwrap();
        assert_eq!(
            args,
            vec![
                "rust async",
                "--engine",
                "perplexica",
                "--time-range",
                "month"
            ]
        );
    }

    #[test]
    fn test_build_args_missing_query() {
        let params = serde_json::json!({"engine": "searxng"});
        let err = MonsterSearchTool::build_args(&params).unwrap_err();
        assert!(err.contains("query"));
    }

    #[test]
    fn test_build_args_empty_query() {
        let params = serde_json::json!({"query": "  "});
        let err = MonsterSearchTool::build_args(&params).unwrap_err();
        assert!(err.contains("empty"));
    }

    // -- execute parameter validation tests --

    #[tokio::test]
    async fn test_execute_missing_query() {
        let tool = MonsterSearchTool::new();
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, .. } => assert_eq!(code, "INVALID_PARAMS"),
            _ => panic!("Expected error"),
        }
    }

    #[tokio::test]
    async fn test_execute_empty_query() {
        let tool = MonsterSearchTool::new();
        let result = tool
            .execute(
                serde_json::json!({"query": ""}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, message } => {
                assert_eq!(code, "INVALID_PARAMS");
                assert!(message.contains("empty"));
            }
            _ => panic!("Expected error"),
        }
    }
}
