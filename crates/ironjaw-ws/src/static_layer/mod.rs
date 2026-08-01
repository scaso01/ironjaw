//! Static layer: discovers and loads CLAUDE.md files from the session working directory
//! and its ancestor chain, deduplicating by content hash. Results are injected into the
//! prompt BEFORE the brain auto-inject so that project-level instructions take the highest
//! system-prompt priority.
//!
//! Search order (first match wins per unique content hash):
//!   1. `<cwd>/CLAUDE.md`
//!   2. `<cwd>/.claude/CLAUDE.md`
//!   3. Parent directories (walking up until `.git/` dir or filesystem root)
//!      - `<parent>/CLAUDE.md`
//!      - `<parent>/.claude/CLAUDE.md`
//!   4. `~/.claude/CLAUDE.md` (global user config)
//!
//! ## Token budget
//!
//! Each discovered file is subject to a per-file token cap (`PER_FILE_TOKEN_CAP = 4096`)
//! enforced by [`ironjaw_llm::tokenizer::apply_budget`].  The aggregate of all files is
//! further capped at `AGGREGATE_TOKEN_CAP = 16384`.  Content that exceeds a cap is
//! truncated at the nearest newline boundary with a notice appended.

pub mod cache;

use ironjaw_llm::tokenizer::apply_budget;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A single discovered CLAUDE.md entry.
#[derive(Debug, Clone)]
pub struct StaticLayerEntry {
    /// Absolute path to the file on disk.
    pub path: PathBuf,
    /// Full text content of the file (possibly truncated to per-file token cap).
    pub content: String,
    /// Human-readable label for telemetry / prompt annotation
    /// (e.g. `"project"`, `"parent:<dir>"`, `"global"`).
    pub source_label: String,
    /// Token count for this entry after budget enforcement.
    pub token_count: usize,
    /// `true` if the content was truncated from its original form.
    pub was_truncated: bool,
}

/// Discover and load all CLAUDE.md files relevant to `cwd`.
///
/// Files are deduplicated by SHA-256 content hash so that symlinks or
/// duplicate content do not inject the same text twice.
///
/// Token budgets are enforced: per-file cap = 4 096 tokens, aggregate cap = 16 384 tokens.
///
/// `override_home` — when `Some`, used as the home directory instead of the
/// platform-detected one.  Pass `None` for production; pass a temp path in
/// tests to prevent global `~/.claude/CLAUDE.md` from leaking in.
pub async fn collect_static_layer_with_home(
    cwd: &Path,
    override_home: Option<&Path>,
) -> Vec<StaticLayerEntry> {
    let mut seen_hashes: HashSet<[u8; 32]> = HashSet::new();

    // Candidates: (path, label)
    let mut candidates: Vec<(PathBuf, String)> = Vec::new();

    // 1+2: direct in cwd
    candidates.push((cwd.join("CLAUDE.md"), "project".to_string()));
    candidates.push((
        cwd.join(".claude").join("CLAUDE.md"),
        "project/.claude".to_string(),
    ));

    // 3: walk parent chain, stopping before crossing a .git boundary.
    //    Semantics: a directory containing `.git` is the project root.
    //    We do NOT include CLAUDE.md files from the project root or its
    //    ancestors when we are working inside a subdirectory of that root.
    //    So we stop as soon as the PARENT we would add has a .git entry.
    let mut current = cwd.to_path_buf();
    loop {
        // Do not walk past a .git root — if current already has .git, stop.
        if current.join(".git").exists() {
            break;
        }
        let parent = match current.parent() {
            Some(p) if p != current => p.to_path_buf(),
            _ => break,
        };
        // Also stop before adding a parent that IS a git root.
        if parent.join(".git").exists() {
            break;
        }
        let dir_name = parent
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| parent.to_string_lossy().into_owned());
        let label = format!("parent:{dir_name}");
        candidates.push((parent.join("CLAUDE.md"), label.clone()));
        candidates.push((
            parent.join(".claude").join("CLAUDE.md"),
            format!("{label}/.claude"),
        ));
        current = parent;
    }

    // 4: global ~/.claude/CLAUDE.md
    let home = override_home
        .map(|p| p.to_path_buf())
        .or_else(platform_home_dir);
    if let Some(home) = home {
        candidates.push((home.join(".claude").join("CLAUDE.md"), "global".to_string()));
    }

    // Load candidates in order, dedup by content hash, collect (content, label) pairs
    let mut raw_pairs: Vec<(PathBuf, String, String)> = Vec::new(); // (path, content, label)
    for (path, label) in candidates {
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            if content.trim().is_empty() {
                continue;
            }
            let hash = sha256_bytes(content.as_bytes());
            if seen_hashes.insert(hash) {
                raw_pairs.push((path, content, label));
            }
        }
    }

    // Apply token budget across all discovered files
    let budget_input = raw_pairs
        .iter()
        .map(|(_, content, label)| (content.clone(), label.clone()));
    let budget_result = apply_budget(budget_input);

    // Reconstruct StaticLayerEntry with paths attached
    budget_result
        .entries
        .into_iter()
        .enumerate()
        .map(|(i, be)| StaticLayerEntry {
            path: raw_pairs[i].0.clone(),
            content: be.content,
            source_label: be.source_label,
            token_count: be.token_count,
            was_truncated: be.was_truncated,
        })
        .collect()
}

/// Public production entry-point — uses the real home directory.
pub async fn collect_static_layer(cwd: &Path) -> Vec<StaticLayerEntry> {
    collect_static_layer_with_home(cwd, None).await
}

/// Returns the user's home directory, platform-aware.
fn platform_home_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

/// Compute SHA-256 of `bytes`, returning the 32-byte digest.
fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_sync(path: &PathBuf, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// Helper: collect with no home dir so global ~/.claude/CLAUDE.md never leaks in.
    /// Also places a .git sentinel at the root of `dir` so parent walking stops there.
    async fn collect_isolated(dir: &TempDir, cwd: &Path) -> Vec<StaticLayerEntry> {
        // Create a fake empty home dir inside the temp tree so the global lookup
        // hits a directory that doesn't contain CLAUDE.md.
        let fake_home = dir.path().join("_home");
        std::fs::create_dir_all(&fake_home).unwrap();
        // Anchor the parent walk: mark the temp root as a git boundary.
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        collect_static_layer_with_home(cwd, Some(&fake_home)).await
    }

    #[tokio::test]
    async fn collects_project_claude_md() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().to_path_buf();
        write_sync(&cwd.join("CLAUDE.md"), "# Project");
        let entries = collect_isolated(&dir, &cwd).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source_label, "project");
        assert!(entries[0].content.contains("# Project"));
    }

    #[tokio::test]
    async fn collects_dot_claude_variant() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().to_path_buf();
        write_sync(&cwd.join(".claude").join("CLAUDE.md"), "# Dot-Claude");
        let entries = collect_isolated(&dir, &cwd).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source_label, "project/.claude");
    }

    #[tokio::test]
    async fn deduplicates_identical_content() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().to_path_buf();
        let same = "# Same content";
        write_sync(&cwd.join("CLAUDE.md"), same);
        write_sync(&cwd.join(".claude").join("CLAUDE.md"), same);
        let entries = collect_isolated(&dir, &cwd).await;
        // Two files with identical content → only one entry after dedup
        assert_eq!(entries.len(), 1, "duplicate content must be deduped");
    }

    #[tokio::test]
    async fn stops_at_git_boundary() {
        // layout: root/ (has .git) -> child/ (our cwd)
        let root = TempDir::new().unwrap();
        let root_path = root.path().to_path_buf();
        // Mark root as git root
        std::fs::create_dir_all(root_path.join(".git")).unwrap();
        // Write a CLAUDE.md in root (should NOT be collected since we stop at .git)
        write_sync(&root_path.join("CLAUDE.md"), "# Root above git");
        // cwd is a child of root, no .git here
        let cwd = root_path.join("child_project");
        std::fs::create_dir_all(&cwd).unwrap();
        write_sync(&cwd.join("CLAUDE.md"), "# Child");
        // Use a fake home dir that has no CLAUDE.md
        let fake_home = root_path.join("_home");
        std::fs::create_dir_all(&fake_home).unwrap();
        let entries = collect_static_layer_with_home(&cwd, Some(&fake_home)).await;
        let labels: Vec<&str> = entries.iter().map(|e| e.source_label.as_str()).collect();
        // "project" is the child's own CLAUDE.md
        assert!(
            labels.contains(&"project"),
            "expected 'project' label in {labels:?}"
        );
        // Root CLAUDE.md must NOT appear — child walks to root but stops because root has .git
        let has_root = entries.iter().any(|e| e.content.contains("Root above git"));
        assert!(!has_root, "should not cross .git boundary");
    }

    #[tokio::test]
    async fn empty_file_is_skipped() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().to_path_buf();
        write_sync(&cwd.join("CLAUDE.md"), "   \n  \n");
        let entries = collect_isolated(&dir, &cwd).await;
        assert!(entries.is_empty(), "whitespace-only file should be skipped");
    }

    #[tokio::test]
    async fn token_count_populated() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().to_path_buf();
        write_sync(&cwd.join("CLAUDE.md"), "# Hello world\nSome content here.");
        let entries = collect_isolated(&dir, &cwd).await;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].token_count > 0, "token_count must be populated");
        assert!(
            !entries[0].was_truncated,
            "short file must not be truncated"
        );
    }
}
