use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use std::process::Stdio;
use tokio_util::sync::CancellationToken;

const DEFAULT_HEAD_LIMIT: usize = 250;

/// Resolve the ripgrep binary path.
///
/// Checks `RG_PATH` env var first (useful in tests and non-standard installs),
/// then falls back to bare `"rg"` which relies on the system PATH.
fn find_rg() -> std::ffi::OsString {
    if let Ok(p) = std::env::var("RG_PATH") {
        if !p.is_empty() {
            return p.into();
        }
    }
    "rg".into()
}

/// Wraps the ripgrep binary for fast file search.
pub struct GrepTool;

impl Default for GrepTool {
    fn default() -> Self {
        Self
    }
}

impl GrepTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents (regex/text) across local projects via ripgrep. Use for 'where is X defined', 'which files mention Y', or any code-search question. Faster than read_file when you don't know the file."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regular expression pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Path to search (relative to working directory, defaults to '.')"
                },
                "glob": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. '*.rs', '**/*.ts')"
                },
                "type": {
                    "type": "string",
                    "description": "File type filter for ripgrep (e.g. 'rust', 'py', 'js')"
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["files", "content", "count"],
                    "description": "Output mode: 'files' (default) lists matching file paths, 'content' shows matching lines, 'count' shows match counts per file"
                },
                "context": {
                    "type": "integer",
                    "description": "Number of context lines to show before and after each match (content mode only)",
                    "minimum": 0
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Case insensitive search (default false)"
                },
                "head_limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default 250)",
                    "minimum": 1
                }
            },
            "required": ["pattern"]
        })
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
        let pattern = match params.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: pattern")
            }
        };

        let search_path = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let output_mode = params
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("files");

        let head_limit = params
            .get("head_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_HEAD_LIMIT as u64) as usize;

        let case_insensitive = params
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let context_lines = params.get("context").and_then(|v| v.as_u64());

        // Resolve the search path relative to working_dir
        let resolved_path = if std::path::Path::new(search_path).is_absolute() {
            std::path::PathBuf::from(search_path)
        } else {
            ctx.working_dir.join(search_path)
        };

        let mut cmd = tokio::process::Command::new(find_rg());
        cmd.current_dir(&ctx.working_dir);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Output mode flags
        match output_mode {
            "files" => {
                cmd.arg("--files-with-matches");
            }
            "count" => {
                cmd.arg("--count");
            }
            "content" => {
                // Use JSON output to avoid Windows path parsing issues (drive letter colons)
                cmd.arg("--json");
                if let Some(ctx_lines) = context_lines {
                    cmd.arg(format!("-C{ctx_lines}"));
                }
            }
            _ => {
                return ToolResult::error(
                    "INVALID_PARAMS",
                    format!(
                        "Invalid output_mode '{output_mode}': must be 'files', 'content', or 'count'"
                    ),
                );
            }
        }

        if case_insensitive {
            cmd.arg("--ignore-case");
        }

        if let Some(glob) = params.get("glob").and_then(|v| v.as_str()) {
            cmd.arg("--glob").arg(glob);
        }

        if let Some(file_type) = params.get("type").and_then(|v| v.as_str()) {
            cmd.arg("--type").arg(file_type);
        }

        cmd.arg("--").arg(&pattern).arg(&resolved_path);

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) => {
                return ToolResult::error(
                    "EXEC_FAILED",
                    format!("Failed to run rg: {e}. Is ripgrep installed?"),
                );
            }
        };

        // rg exits 1 when no matches, 2 on error
        let exit_code = output.status.code().unwrap_or(-1);
        if exit_code == 2 {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return ToolResult::error("RG_ERROR", format!("ripgrep error: {stderr}"));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();

        match output_mode {
            "files" => {
                let paths: Vec<serde_json::Value> = lines
                    .iter()
                    .take(head_limit)
                    .map(|l| serde_json::Value::String(l.to_string()))
                    .collect();
                ToolResult::json(serde_json::json!({
                    "output_mode": "files",
                    "count": paths.len(),
                    "truncated": lines.len() > head_limit,
                    "results": paths
                }))
            }
            "count" => {
                let entries: Vec<serde_json::Value> = lines
                    .iter()
                    .take(head_limit)
                    .filter_map(|l| {
                        // rg --count output: "filepath:N"
                        let (path, count) = l.rsplit_once(':')?;
                        let n: u64 = count.trim().parse().ok()?;
                        Some(serde_json::json!({"file": path, "count": n}))
                    })
                    .collect();
                ToolResult::json(serde_json::json!({
                    "output_mode": "count",
                    "count": entries.len(),
                    "truncated": lines.len() > head_limit,
                    "results": entries
                }))
            }
            "content" => {
                // rg --json emits one JSON object per line; "match" type carries path/line/text.
                // This avoids Windows drive-letter colon ambiguity in text output.
                let mut results: Vec<serde_json::Value> = Vec::new();
                for line in lines.iter() {
                    if results.len() >= head_limit {
                        break;
                    }
                    let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
                        continue;
                    };
                    if obj.get("type").and_then(|t| t.as_str()) != Some("match") {
                        continue;
                    }
                    let data = &obj["data"];
                    let file = data["path"]["text"].as_str().unwrap_or("").to_string();
                    let line_number = data["line_number"].as_u64().unwrap_or(0);
                    let text = data["lines"]["text"]
                        .as_str()
                        .unwrap_or("")
                        .trim_end_matches('\n')
                        .to_string();
                    results.push(serde_json::json!({
                        "file": file,
                        "line_number": line_number,
                        "text": text
                    }));
                }
                let total_raw = lines.len();
                ToolResult::json(serde_json::json!({
                    "output_mode": "content",
                    "count": results.len(),
                    "truncated": total_raw > head_limit,
                    "results": results
                }))
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Whether ripgrep can be found, so the tests that shell out to it can skip
    /// rather than fail on a machine that simply does not have it installed.
    ///
    /// `RG_PATH` wins if set, which is also how [`GrepTool`] itself resolves the
    /// binary. Otherwise this is a plain PATH lookup. A missing ripgrep is an
    /// environment gap, not a defect in this crate, and a suite that goes red for
    /// it teaches people to ignore red suites.
    fn rg_available() -> bool {
        if let Ok(p) = std::env::var("RG_PATH") {
            if Path::new(&p).exists() {
                return true;
            }
        }
        std::process::Command::new("rg")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// Skip the body of a ripgrep-backed test when ripgrep is not installed.
    macro_rules! require_rg {
        () => {
            if !rg_available() {
                eprintln!("skipping: ripgrep not found on PATH and RG_PATH is unset");
                return;
            }
        };
    }

    fn test_ctx_in(dir: &Path) -> ToolContext {
        ToolContext::new("test", "test", dir.to_path_buf())
    }

    async fn setup_test_dir(prefix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ironjaw-grep-{prefix}"));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("alpha.txt"), "hello world\nfoo bar\n")
            .await
            .unwrap();
        tokio::fs::write(dir.join("beta.txt"), "hello universe\nbaz\n")
            .await
            .unwrap();
        tokio::fs::write(dir.join("gamma.rs"), "fn hello() {}\n")
            .await
            .unwrap();
        dir
    }

    #[tokio::test]
    async fn test_grep_files_mode() {
        require_rg!();
        let dir = setup_test_dir("files").await;
        let tool = GrepTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(
                serde_json::json!({"pattern": "hello", "output_mode": "files"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error(), "should not error: {result:?}");
        match result {
            ToolResult::Json { content } => {
                assert_eq!(content["output_mode"], "files");
                let results = content["results"].as_array().unwrap();
                assert!(results.len() >= 3, "all 3 files contain 'hello'");
            }
            _ => panic!("Expected Json result"),
        }
        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn test_grep_content_mode() {
        require_rg!();
        let dir = setup_test_dir("content").await;
        let tool = GrepTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(
                serde_json::json!({"pattern": "hello", "output_mode": "content"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error(), "should not error: {result:?}");
        match result {
            ToolResult::Json { content } => {
                assert_eq!(content["output_mode"], "content");
                let results = content["results"].as_array().unwrap();
                assert!(!results.is_empty());
                // Each result should have file, line_number, text
                let first = &results[0];
                assert!(first.get("file").is_some());
                assert!(first.get("line_number").is_some());
                assert!(first.get("text").is_some());
            }
            _ => panic!("Expected Json result"),
        }
        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn test_grep_count_mode() {
        require_rg!();
        let dir = setup_test_dir("count").await;
        let tool = GrepTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(
                serde_json::json!({"pattern": "hello", "output_mode": "count"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error(), "should not error: {result:?}");
        match result {
            ToolResult::Json { content } => {
                assert_eq!(content["output_mode"], "count");
                let results = content["results"].as_array().unwrap();
                assert!(!results.is_empty());
                let first = &results[0];
                assert!(first.get("file").is_some());
                assert!(first.get("count").is_some());
            }
            _ => panic!("Expected Json result"),
        }
        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn test_grep_glob_filter() {
        require_rg!();
        let dir = setup_test_dir("glob").await;
        let tool = GrepTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(
                serde_json::json!({"pattern": "hello", "output_mode": "files", "glob": "*.rs"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error(), "should not error: {result:?}");
        match result {
            ToolResult::Json { content } => {
                let results = content["results"].as_array().unwrap();
                // Only gamma.rs should match
                assert_eq!(results.len(), 1);
                assert!(results[0].as_str().unwrap().ends_with("gamma.rs"));
            }
            _ => panic!("Expected Json result"),
        }
        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn test_grep_head_limit() {
        require_rg!();
        let dir = setup_test_dir("headlimit").await;
        let tool = GrepTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(
                serde_json::json!({"pattern": "hello", "output_mode": "files", "head_limit": 1}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error(), "should not error: {result:?}");
        match result {
            ToolResult::Json { content } => {
                let results = content["results"].as_array().unwrap();
                assert_eq!(results.len(), 1, "head_limit=1 should truncate to 1 result");
                // truncated should be true since there are 3 matching files
                assert_eq!(content["truncated"], true);
            }
            _ => panic!("Expected Json result"),
        }
        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn test_grep_missing_pattern() {
        let dir = std::env::temp_dir();
        let tool = GrepTool::new();
        let ctx = test_ctx_in(&dir);
        let result = tool
            .execute(serde_json::json!({}), &ctx, CancellationToken::new())
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, .. } => assert_eq!(code, "INVALID_PARAMS"),
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn test_grep_tier() {
        let tool = GrepTool::new();
        assert_eq!(tool.tier(), PermissionTier::Auto);
    }
}
