use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Embedded documentation files from the `docs/` directory.
#[derive(Embed)]
#[folder = "docs/"]
struct Docs;

/// A search result from the embedded documentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub file_name: String,
    pub matching_lines: Vec<String>,
    pub relevance_score: f32,
}

/// Tool for searching embedded documentation bundled into the binary.
pub struct SelfKnowledgeTool;

impl Default for SelfKnowledgeTool {
    fn default() -> Self {
        Self
    }
}

impl SelfKnowledgeTool {
    pub fn new() -> Self {
        Self
    }

    /// List all embedded document file names.
    pub fn list_topics(&self) -> Vec<String> {
        Docs::iter().map(|f| f.to_string()).collect()
    }

    /// Search all embedded docs for lines matching the query (case-insensitive keyword match).
    pub fn search(&self, query: &str) -> Vec<SearchResult> {
        if query.is_empty() {
            return Vec::new();
        }

        let query_lower = query.to_lowercase();
        let keywords: Vec<&str> = query_lower.split_whitespace().collect();
        if keywords.is_empty() {
            return Vec::new();
        }

        let mut results = Vec::new();

        for file_name in Docs::iter() {
            if let Some(file) = Docs::get(&file_name) {
                let content = match std::str::from_utf8(file.data.as_ref()) {
                    Ok(s) => s,
                    Err(_) => continue, // skip non-UTF8 files
                };

                let mut matching_lines = Vec::new();
                let mut keyword_hits = 0u32;
                let total_lines = content.lines().count().max(1);

                for line in content.lines() {
                    let line_lower = line.to_lowercase();
                    let mut line_matches = false;
                    for kw in &keywords {
                        if line_lower.contains(kw) {
                            line_matches = true;
                            keyword_hits += 1;
                        }
                    }
                    if line_matches {
                        matching_lines.push(line.to_string());
                    }
                }

                if !matching_lines.is_empty() {
                    // Relevance = (keyword_hits / total_keywords) * (matching_lines / total_lines)
                    let keyword_coverage =
                        keyword_hits as f32 / (keywords.len() as f32 * total_lines as f32);
                    let line_ratio = matching_lines.len() as f32 / total_lines as f32;
                    let relevance_score = (keyword_coverage + line_ratio) / 2.0;

                    results.push(SearchResult {
                        file_name: file_name.to_string(),
                        matching_lines,
                        relevance_score,
                    });
                }
            }
        }

        // Sort by relevance (highest first)
        results.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        results
    }
}

#[async_trait::async_trait]
impl Tool for SelfKnowledgeTool {
    fn name(&self) -> &str {
        "self_knowledge"
    }

    fn description(&self) -> &str {
        "Search embedded documentation bundled with the server"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["search", "list"],
                    "description": "Action: 'search' for keyword search, 'list' for all topics"
                },
                "query": {
                    "type": "string",
                    "description": "Search query (required for 'search' action)"
                }
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
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: action")
            }
        };

        match action {
            "list" => {
                let topics = self.list_topics();
                ToolResult::json(serde_json::json!({
                    "topics": topics,
                    "count": topics.len(),
                }))
            }
            "search" => {
                let query = match params.get("query").and_then(|v| v.as_str()) {
                    Some(q) => q,
                    None => {
                        return ToolResult::error(
                            "INVALID_PARAMS",
                            "Missing required parameter: query (for search action)",
                        )
                    }
                };

                let results = self.search(query);
                match serde_json::to_value(&results) {
                    Ok(v) => ToolResult::json(serde_json::json!({
                        "results": v,
                        "count": results.len(),
                    })),
                    Err(e) => ToolResult::error(
                        "SERIALIZE_FAILED",
                        format!("Failed to serialize results: {e}"),
                    ),
                }
            }
            other => ToolResult::error(
                "INVALID_PARAMS",
                format!("Unknown action: '{other}'. Must be 'search' or 'list'"),
            ),
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

    #[test]
    fn test_list_topics_not_empty() {
        let tool = SelfKnowledgeTool::new();
        let topics = tool.list_topics();
        assert!(!topics.is_empty(), "Should have embedded docs");
    }

    #[test]
    fn test_list_topics_contains_known_files() {
        let tool = SelfKnowledgeTool::new();
        let topics = tool.list_topics();
        assert!(
            topics.iter().any(|t| t.contains("overview")),
            "Should contain overview.md"
        );
        assert!(
            topics.iter().any(|t| t.contains("api")),
            "Should contain api.md"
        );
    }

    #[test]
    fn test_search_finds_content() {
        let tool = SelfKnowledgeTool::new();
        let results = tool.search("WebSocket");
        assert!(
            !results.is_empty(),
            "Should find 'WebSocket' in embedded docs"
        );
        // Check that matching lines actually contain the keyword
        for r in &results {
            for line in &r.matching_lines {
                assert!(
                    line.to_lowercase().contains("websocket"),
                    "Line should contain keyword"
                );
            }
        }
    }

    #[test]
    fn test_search_case_insensitive() {
        let tool = SelfKnowledgeTool::new();
        let upper = tool.search("IRONJAW");
        let lower = tool.search("ironjaw");
        // Both should find results
        assert!(!upper.is_empty());
        assert!(!lower.is_empty());
        // Same number of total matching files
        assert_eq!(upper.len(), lower.len());
    }

    #[test]
    fn test_search_no_match_returns_empty() {
        let tool = SelfKnowledgeTool::new();
        let results = tool.search("zyxwvutsrqp_nonexistent_term_12345");
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_empty_query() {
        let tool = SelfKnowledgeTool::new();
        let results = tool.search("");
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_whitespace_only() {
        let tool = SelfKnowledgeTool::new();
        let results = tool.search("   ");
        assert!(results.is_empty());
    }

    #[test]
    fn test_relevance_score_range() {
        let tool = SelfKnowledgeTool::new();
        let results = tool.search("protocol");
        for r in &results {
            assert!(r.relevance_score >= 0.0, "Score should be non-negative");
            assert!(r.relevance_score <= 1.0, "Score should be <= 1.0");
        }
    }

    #[test]
    fn test_relevance_sorted_descending() {
        let tool = SelfKnowledgeTool::new();
        let results = tool.search("gateway");
        if results.len() >= 2 {
            for window in results.windows(2) {
                assert!(
                    window[0].relevance_score >= window[1].relevance_score,
                    "Results should be sorted by relevance descending"
                );
            }
        }
    }

    #[test]
    fn test_search_result_has_file_name() {
        let tool = SelfKnowledgeTool::new();
        let results = tool.search("crate");
        for r in &results {
            assert!(!r.file_name.is_empty());
        }
    }

    #[test]
    fn test_search_multi_keyword() {
        let tool = SelfKnowledgeTool::new();
        let results = tool.search("WebSocket gateway");
        assert!(!results.is_empty());
    }

    #[test]
    fn test_tool_metadata() {
        let tool = SelfKnowledgeTool::new();
        assert_eq!(tool.name(), "self_knowledge");
        assert_eq!(tool.tier(), PermissionTier::Auto);
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn test_execute_list() {
        let tool = SelfKnowledgeTool::new();
        let result = tool
            .execute(
                serde_json::json!({"action": "list"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
        match result {
            ToolResult::Json { content } => {
                let count = content["count"].as_u64().unwrap();
                assert!(count > 0);
            }
            _ => panic!("Expected Json result"),
        }
    }

    #[tokio::test]
    async fn test_execute_search() {
        let tool = SelfKnowledgeTool::new();
        let result = tool
            .execute(
                serde_json::json!({"action": "search", "query": "protocol"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
    }

    #[tokio::test]
    async fn test_execute_search_missing_query() {
        let tool = SelfKnowledgeTool::new();
        let result = tool
            .execute(
                serde_json::json!({"action": "search"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_execute_invalid_action() {
        let tool = SelfKnowledgeTool::new();
        let result = tool
            .execute(
                serde_json::json!({"action": "delete"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_execute_missing_action() {
        let tool = SelfKnowledgeTool::new();
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
    }
}
