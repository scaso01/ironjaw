use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Represents a single hunk from a unified diff.
#[derive(Debug, Clone)]
struct Hunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    lines: Vec<DiffLine>,
}

/// A single line in a diff hunk.
#[derive(Debug, Clone)]
enum DiffLine {
    Context(String),
    Add(String),
    Remove(String),
}

/// Parsed unified diff for a single file.
#[derive(Debug)]
struct FilePatch {
    path: String,
    hunks: Vec<Hunk>,
}

/// Parse a unified diff string into a list of file patches.
fn parse_patch(diff: &str) -> Result<Vec<FilePatch>, String> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut patches = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        // Find --- header
        if lines[i].starts_with("--- ") {
            if i + 1 >= lines.len() || !lines[i + 1].starts_with("+++ ") {
                return Err(format!("Expected +++ header after --- at line {}", i + 1));
            }

            // Extract the target file path from +++ line
            let plus_line = lines[i + 1];
            let path = plus_line
                .strip_prefix("+++ ")
                .unwrap()
                .strip_prefix("b/")
                .unwrap_or(plus_line.strip_prefix("+++ ").unwrap())
                .to_string();

            i += 2;

            let mut hunks = Vec::new();

            // Parse hunks for this file
            while i < lines.len() && !lines[i].starts_with("--- ") {
                if lines[i].starts_with("@@ ") {
                    let hunk = parse_hunk_header(lines[i])?;
                    i += 1;

                    let mut hunk_lines = Vec::new();
                    while i < lines.len()
                        && !lines[i].starts_with("@@ ")
                        && !lines[i].starts_with("--- ")
                    {
                        let line = lines[i];
                        if let Some(content) = line.strip_prefix('+') {
                            hunk_lines.push(DiffLine::Add(content.to_string()));
                        } else if let Some(content) = line.strip_prefix('-') {
                            hunk_lines.push(DiffLine::Remove(content.to_string()));
                        } else if let Some(content) = line.strip_prefix(' ') {
                            hunk_lines.push(DiffLine::Context(content.to_string()));
                        } else if line == "\\ No newline at end of file" {
                            // Skip this meta line
                        } else {
                            // Treat unrecognized lines as context (bare lines)
                            hunk_lines.push(DiffLine::Context(line.to_string()));
                        }
                        i += 1;
                    }

                    hunks.push(Hunk {
                        old_start: hunk.old_start,
                        old_count: hunk.old_count,
                        new_start: hunk.new_start,
                        new_count: hunk.new_count,
                        lines: hunk_lines,
                    });
                } else {
                    i += 1;
                }
            }

            if hunks.is_empty() {
                return Err(format!("No hunks found for file '{path}'"));
            }

            patches.push(FilePatch { path, hunks });
        } else {
            i += 1;
        }
    }

    if patches.is_empty() {
        return Err("No file patches found in diff".to_string());
    }

    Ok(patches)
}

/// Parse a @@ hunk header like `@@ -1,3 +1,4 @@`
fn parse_hunk_header(line: &str) -> Result<Hunk, String> {
    // Strip @@ prefix and suffix
    let inner = line
        .strip_prefix("@@ ")
        .and_then(|s| {
            // Find the closing @@, which might be followed by section header
            s.find(" @@").map(|pos| &s[..pos])
        })
        .ok_or_else(|| format!("Invalid hunk header: {line}"))?;

    let parts: Vec<&str> = inner.split(' ').collect();
    if parts.len() != 2 {
        return Err(format!("Invalid hunk header: {line}"));
    }

    let (old_start, old_count) = parse_range(parts[0].strip_prefix('-').unwrap_or(parts[0]))?;
    let (new_start, new_count) = parse_range(parts[1].strip_prefix('+').unwrap_or(parts[1]))?;

    Ok(Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
        lines: Vec::new(),
    })
}

/// Parse "start,count" or "start" (count defaults to 1).
fn parse_range(s: &str) -> Result<(usize, usize), String> {
    if let Some((start, count)) = s.split_once(',') {
        let start: usize = start
            .parse()
            .map_err(|_| format!("Invalid range start: {start}"))?;
        let count: usize = count
            .parse()
            .map_err(|_| format!("Invalid range count: {count}"))?;
        Ok((start, count))
    } else {
        let start: usize = s.parse().map_err(|_| format!("Invalid range: {s}"))?;
        Ok((start, 1))
    }
}

/// Apply a sequence of hunks to file content, returning the patched content.
fn apply_hunks(original: &str, hunks: &[Hunk]) -> Result<String, String> {
    let mut lines: Vec<String> = original.lines().map(|s| s.to_string()).collect();
    let ends_with_newline = original.ends_with('\n');

    // Apply hunks in reverse order so line numbers stay valid
    let mut sorted_hunks: Vec<&Hunk> = hunks.iter().collect();
    sorted_hunks.sort_by_key(|r| std::cmp::Reverse(r.old_start));

    for hunk in sorted_hunks {
        let start = if hunk.old_start == 0 {
            0
        } else {
            hunk.old_start - 1
        };

        // Verify context and remove lines match before applying
        let mut old_idx = start;
        for diff_line in &hunk.lines {
            match diff_line {
                DiffLine::Context(expected) => {
                    if old_idx >= lines.len() {
                        return Err(format!(
                            "Context line beyond end of file at line {}",
                            old_idx + 1
                        ));
                    }
                    if lines[old_idx] != *expected {
                        return Err(format!(
                            "Context mismatch at line {}: expected '{}', got '{}'",
                            old_idx + 1,
                            expected,
                            lines[old_idx]
                        ));
                    }
                    old_idx += 1;
                }
                DiffLine::Remove(expected) => {
                    if old_idx >= lines.len() {
                        return Err(format!(
                            "Remove line beyond end of file at line {}",
                            old_idx + 1
                        ));
                    }
                    if lines[old_idx] != *expected {
                        return Err(format!(
                            "Remove mismatch at line {}: expected '{}', got '{}'",
                            old_idx + 1,
                            expected,
                            lines[old_idx]
                        ));
                    }
                    old_idx += 1;
                }
                DiffLine::Add(_) => {}
            }
        }

        // Build replacement lines (context + additions, skip removals)
        let mut new_lines: Vec<String> = Vec::new();
        for diff_line in &hunk.lines {
            match diff_line {
                DiffLine::Context(line) => new_lines.push(line.clone()),
                DiffLine::Add(line) => new_lines.push(line.clone()),
                DiffLine::Remove(_) => {}
            }
        }

        let end = (start + hunk.old_count).min(lines.len());
        lines.splice(start..end, new_lines);
    }

    let mut result = lines.join("\n");
    if ends_with_newline {
        result.push('\n');
    }
    Ok(result)
}

/// Check that `target` (which may not exist yet) stays under `base`.
fn ensure_patch_path(base: &Path, target: &Path) -> Result<std::path::PathBuf, String> {
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        base.join(target)
    };

    let canon_base = base
        .canonicalize()
        .map_err(|e| format!("Cannot resolve base directory: {e}"))?;

    // For patch, the file should exist
    let canon_target = resolved
        .canonicalize()
        .map_err(|e| format!("Cannot resolve path '{}': {e}", resolved.display()))?;

    if canon_target.starts_with(&canon_base) {
        Ok(canon_target)
    } else {
        Err(format!(
            "Path '{}' is outside working directory",
            target.display()
        ))
    }
}

/// Apply unified diff patches to files.
pub struct ApplyPatchTool;

impl Default for ApplyPatchTool {
    fn default() -> Self {
        Self
    }
}

impl ApplyPatchTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a unified diff patch to one or more local files. Use for multi-line code edits where exact-string replace is too brittle.\n\nFormat (strict): two header lines then one or more @@ hunks. Each hunk line starts with ' ' (context), '+' (add), or '-' (remove). Counts in @@ -OLD_START,OLD_COUNT +NEW_START,NEW_COUNT @@ must match the hunk body exactly.\n\nWorked example — change line 2 of foo.txt from 'beta' to 'beta CHANGED':\n\n--- a/foo.txt\n+++ b/foo.txt\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+beta CHANGED\n gamma\n\nCommon failures: missing leading space on context lines, OLD/NEW counts that don't match the hunk, or paths not relative to the working directory. If you only need to change one short string, prefer write_file."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "Unified diff content to apply"
                }
            },
            "required": ["patch"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Confirm
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let patch_str = match params.get("patch").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: patch")
            }
        };

        let file_patches = match parse_patch(patch_str) {
            Ok(p) => p,
            Err(e) => {
                return ToolResult::error("PARSE_FAILED", format!("Failed to parse patch: {e}"))
            }
        };

        let mut results = Vec::new();

        for file_patch in &file_patches {
            let path = Path::new(&file_patch.path);
            let resolved = match ensure_patch_path(&ctx.working_dir, path) {
                Ok(p) => p,
                Err(e) => {
                    results.push(serde_json::json!({
                        "path": file_patch.path,
                        "status": "error",
                        "message": e,
                    }));
                    continue;
                }
            };

            let content = match tokio::fs::read_to_string(&resolved).await {
                Ok(c) => c,
                Err(e) => {
                    results.push(serde_json::json!({
                        "path": file_patch.path,
                        "status": "error",
                        "message": format!("Failed to read file: {e}"),
                    }));
                    continue;
                }
            };

            let patched = match apply_hunks(&content, &file_patch.hunks) {
                Ok(p) => p,
                Err(e) => {
                    results.push(serde_json::json!({
                        "path": file_patch.path,
                        "status": "error",
                        "message": format!("Failed to apply hunks: {e}"),
                    }));
                    continue;
                }
            };

            match tokio::fs::write(&resolved, &patched).await {
                Ok(()) => {
                    results.push(serde_json::json!({
                        "path": file_patch.path,
                        "status": "applied",
                        "hunks": file_patch.hunks.len(),
                    }));
                }
                Err(e) => {
                    results.push(serde_json::json!({
                        "path": file_patch.path,
                        "status": "error",
                        "message": format!("Failed to write file: {e}"),
                    }));
                }
            }
        }

        ToolResult::json(serde_json::json!({
            "files": results,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Patch parsing tests ──────────────────────────────────────────────────

    #[test]
    fn test_parse_simple_patch() {
        let diff = "\
--- a/hello.txt
+++ b/hello.txt
@@ -1,3 +1,4 @@
 line1
+inserted
 line2
 line3
";
        let patches = parse_patch(diff).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].path, "hello.txt");
        assert_eq!(patches[0].hunks.len(), 1);
        assert_eq!(patches[0].hunks[0].old_start, 1);
        assert_eq!(patches[0].hunks[0].old_count, 3);
        assert_eq!(patches[0].hunks[0].new_start, 1);
        assert_eq!(patches[0].hunks[0].new_count, 4);
    }

    #[test]
    fn test_parse_patch_no_prefix() {
        let diff = "\
--- hello.txt
+++ hello.txt
@@ -1,2 +1,3 @@
 line1
+added
 line2
";
        let patches = parse_patch(diff).unwrap();
        assert_eq!(patches[0].path, "hello.txt");
    }

    #[test]
    fn test_parse_empty_diff() {
        let result = parse_patch("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_missing_plus_header() {
        let diff = "--- a/file.txt\n";
        let result = parse_patch(diff);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_no_hunks() {
        let diff = "\
--- a/file.txt
+++ b/file.txt
";
        let result = parse_patch(diff);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_range_single() {
        let (start, count) = parse_range("5").unwrap();
        assert_eq!(start, 5);
        assert_eq!(count, 1);
    }

    #[test]
    fn test_parse_range_pair() {
        let (start, count) = parse_range("10,20").unwrap();
        assert_eq!(start, 10);
        assert_eq!(count, 20);
    }

    #[test]
    fn test_parse_range_invalid() {
        assert!(parse_range("abc").is_err());
    }

    // ── Hunk application tests ───────────────────────────────────────────────

    #[test]
    fn test_apply_add_line() {
        let original = "line1\nline2\nline3\n";
        let hunks = vec![Hunk {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 4,
            lines: vec![
                DiffLine::Context("line1".to_string()),
                DiffLine::Add("inserted".to_string()),
                DiffLine::Context("line2".to_string()),
                DiffLine::Context("line3".to_string()),
            ],
        }];
        let result = apply_hunks(original, &hunks).unwrap();
        assert_eq!(result, "line1\ninserted\nline2\nline3\n");
    }

    #[test]
    fn test_apply_remove_line() {
        let original = "line1\nline2\nline3\n";
        let hunks = vec![Hunk {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 2,
            lines: vec![
                DiffLine::Context("line1".to_string()),
                DiffLine::Remove("line2".to_string()),
                DiffLine::Context("line3".to_string()),
            ],
        }];
        let result = apply_hunks(original, &hunks).unwrap();
        assert_eq!(result, "line1\nline3\n");
    }

    #[test]
    fn test_apply_replace_line() {
        let original = "old\n";
        let hunks = vec![Hunk {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![
                DiffLine::Remove("old".to_string()),
                DiffLine::Add("new".to_string()),
            ],
        }];
        let result = apply_hunks(original, &hunks).unwrap();
        assert_eq!(result, "new\n");
    }

    #[test]
    fn test_apply_context_mismatch() {
        let original = "line1\nline2\n";
        let hunks = vec![Hunk {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 2,
            lines: vec![
                DiffLine::Context("wrong".to_string()),
                DiffLine::Context("line2".to_string()),
            ],
        }];
        let result = apply_hunks(original, &hunks);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Context mismatch"));
    }

    #[test]
    fn test_apply_multiple_hunks() {
        let original = "a\nb\nc\nd\ne\n";
        let hunks = vec![
            Hunk {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 3,
                lines: vec![
                    DiffLine::Context("a".to_string()),
                    DiffLine::Add("x".to_string()),
                    DiffLine::Context("b".to_string()),
                ],
            },
            Hunk {
                old_start: 4,
                old_count: 2,
                new_start: 5,
                new_count: 3,
                lines: vec![
                    DiffLine::Context("d".to_string()),
                    DiffLine::Add("y".to_string()),
                    DiffLine::Context("e".to_string()),
                ],
            },
        ];
        let result = apply_hunks(original, &hunks).unwrap();
        assert_eq!(result, "a\nx\nb\nc\nd\ny\ne\n");
    }

    // ── Tool trait tests ─────────────────────────────────────────────────────

    #[test]
    fn test_apply_patch_tier() {
        let tool = ApplyPatchTool::new();
        assert_eq!(tool.tier(), PermissionTier::Confirm);
    }

    #[test]
    fn test_apply_patch_name() {
        let tool = ApplyPatchTool::new();
        assert_eq!(tool.name(), "apply_patch");
    }

    #[tokio::test]
    async fn test_apply_patch_missing_param() {
        let tool = ApplyPatchTool::new();
        let ctx = ToolContext::new("test", "test", std::env::temp_dir());
        let result = tool
            .execute(serde_json::json!({}), &ctx, CancellationToken::new())
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_apply_patch_invalid_diff() {
        let tool = ApplyPatchTool::new();
        let ctx = ToolContext::new("test", "test", std::env::temp_dir());
        let result = tool
            .execute(
                serde_json::json!({"patch": "not a valid diff"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_apply_patch_tool_integration() {
        let dir = std::env::temp_dir().join("ironjaw-test-patch");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("target.txt"), "line1\nline2\nline3\n").unwrap();

        let tool = ApplyPatchTool::new();
        let ctx = ToolContext::new("test", "test", dir.clone());

        let diff = "\
--- a/target.txt
+++ b/target.txt
@@ -1,3 +1,4 @@
 line1
+inserted
 line2
 line3
";
        let result = tool
            .execute(
                serde_json::json!({"patch": diff}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error(), "Result was: {result:?}");

        let content = std::fs::read_to_string(dir.join("target.txt")).unwrap();
        assert_eq!(content, "line1\ninserted\nline2\nline3\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_apply_patch_default() {
        let tool = ApplyPatchTool;
        assert_eq!(tool.name(), "apply_patch");
    }

    #[test]
    fn test_parse_hunk_header_with_section() {
        let hunk = parse_hunk_header("@@ -10,5 +12,7 @@ fn some_function()").unwrap();
        assert_eq!(hunk.old_start, 10);
        assert_eq!(hunk.old_count, 5);
        assert_eq!(hunk.new_start, 12);
        assert_eq!(hunk.new_count, 7);
    }
}
