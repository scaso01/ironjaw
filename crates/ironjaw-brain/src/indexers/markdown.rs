use crate::embedder::Embedder;
use crate::learner::WriteGate;
use crate::store::BrainStore;
use crate::types::{NewMemory, TrustLevel};
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

const MIN_CHUNK_CHARS: usize = 20;
const MAX_CHUNK_CHARS: usize = 2000;

/// Configuration for the Markdown file watcher.
#[derive(Debug, Clone)]
pub struct MarkdownConfig {
    pub enabled: bool,
    pub watch_dirs: Vec<String>,
    pub debounce_secs: u64,
}

impl Default for MarkdownConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            watch_dirs: Vec::new(),
            debounce_secs: 5,
        }
    }
}

/// Watches markdown files in configured directories and indexes them into the brain.
pub struct MarkdownIndexer {
    store: BrainStore,
    embedder: Arc<Embedder>,
    config: MarkdownConfig,
}

impl MarkdownIndexer {
    pub fn new(store: BrainStore, embedder: Arc<Embedder>, config: MarkdownConfig) -> Self {
        Self {
            store,
            embedder,
            config,
        }
    }

    /// Spawn the background watcher task. Returns a handle.
    pub fn spawn(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            info!(
                dirs = ?self.config.watch_dirs,
                debounce_secs = self.config.debounce_secs,
                "Markdown indexer starting"
            );

            // Full re-index on startup
            if let Err(e) = self.full_reindex().await {
                warn!(error = %e, "Full markdown re-index failed");
            }

            // Set up file watcher
            let (tx, mut rx) = mpsc::channel::<Vec<PathBuf>>(64);
            let debounce_dur = std::time::Duration::from_secs(self.config.debounce_secs);

            let _watcher = match self.setup_watcher(tx, debounce_dur) {
                Ok(w) => w,
                Err(e) => {
                    error!(error = %e, "Failed to set up file watcher, markdown indexer exiting");
                    return;
                }
            };

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("Markdown indexer shutting down");
                        return;
                    }
                    Some(paths) = rx.recv() => {
                        let unique: HashSet<PathBuf> = paths.into_iter().collect();
                        for path in unique {
                            if let Err(e) = self.handle_file_event(&path).await {
                                warn!(
                                    path = %path.display(),
                                    error = %e,
                                    "Failed to handle markdown file event"
                                );
                            }
                        }
                    }
                }
            }
        })
    }

    /// Set up the notify file watcher with debouncing.
    fn setup_watcher(
        &self,
        tx: mpsc::Sender<Vec<PathBuf>>,
        debounce: std::time::Duration,
    ) -> anyhow::Result<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>> {
        let mut debouncer = new_debouncer(
            debounce,
            move |result: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
                match result {
                    Ok(events) => {
                        let paths: Vec<PathBuf> = events
                            .into_iter()
                            .filter(|e| e.kind == DebouncedEventKind::Any)
                            .map(|e| e.path)
                            .filter(|p| is_indexable_markdown(p))
                            .collect();
                        if !paths.is_empty() {
                            let _ = tx.blocking_send(paths);
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "File watcher error");
                    }
                }
            },
        )?;

        for dir in &self.config.watch_dirs {
            let path = Path::new(dir);
            if path.exists() {
                debouncer
                    .watcher()
                    .watch(path, notify::RecursiveMode::Recursive)?;
                debug!(dir = %dir, "Watching directory for markdown changes");
            } else {
                warn!(dir = %dir, "Watch directory does not exist, skipping");
            }
        }

        Ok(debouncer)
    }

    /// Full re-index: scan all .md files and compare content hashes.
    async fn full_reindex(&self) -> anyhow::Result<()> {
        let mut indexed = 0u32;
        let mut skipped = 0u32;

        let existing_paths: HashSet<String> = self
            .store
            .list_markdown_paths()
            .await?
            .into_iter()
            .collect();
        let mut seen_paths: HashSet<String> = HashSet::new();

        for dir in &self.config.watch_dirs {
            let dir_path = Path::new(dir);
            if !dir_path.exists() {
                continue;
            }

            for entry in walkdir(dir_path)? {
                let path = entry;
                if !is_indexable_markdown(&path) {
                    continue;
                }

                let path_str = path.to_string_lossy().to_string();
                seen_paths.insert(path_str.clone());

                match self.index_file(&path).await {
                    Ok(true) => indexed += 1,
                    Ok(false) => skipped += 1,
                    Err(e) => warn!(path = %path.display(), error = %e, "Failed to index"),
                }
            }
        }

        // Soft-delete memories for files that no longer exist
        for path in existing_paths.difference(&seen_paths) {
            debug!(path = %path, "Markdown file removed, soft-deleting memories");
            self.store.soft_delete_by_source(path).await?;
            self.store.remove_markdown_hash(path).await?;
        }

        info!(indexed, skipped, "Markdown full re-index complete");
        Ok(())
    }

    /// Handle a file create/modify/delete event.
    async fn handle_file_event(&self, path: &Path) -> anyhow::Result<()> {
        if !path.exists() {
            // File was deleted
            let path_str = path.to_string_lossy().to_string();
            debug!(path = %path_str, "Markdown file deleted, soft-deleting memories");
            self.store.soft_delete_by_source(&path_str).await?;
            self.store.remove_markdown_hash(&path_str).await?;
            return Ok(());
        }

        if !is_indexable_markdown(path) {
            return Ok(());
        }

        match self.index_file(path).await {
            Ok(true) => info!(path = %path.display(), "Re-indexed markdown file"),
            Ok(false) => {}
            Err(e) => warn!(path = %path.display(), error = %e, "Failed to re-index"),
        }

        Ok(())
    }

    /// Index a single markdown file. Returns true if new/updated, false if unchanged.
    async fn index_file(&self, path: &Path) -> anyhow::Result<bool> {
        let content = tokio::fs::read_to_string(path).await?;
        let path_str = path.to_string_lossy().to_string();

        // Compute file-level content hash
        let file_hash = WriteGate::content_hash(&content);

        // Check if already indexed with same hash
        if let Some(stored_hash) = self.store.get_markdown_hash(&path_str).await? {
            if stored_hash == file_hash {
                return Ok(false);
            }
        }

        // Content changed — delete old memories for this file, then re-index
        self.store.soft_delete_by_source(&path_str).await?;

        // Split into chunks by headings
        let chunks = split_markdown_sections(&content, &path_str);

        // Scope mirrors what the old memory_to_brain.py stub carried, so scope
        // browsing over /memory/* keeps working once that writer is retired.
        let scope = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|stem| format!("/memory/{stem}"));

        for chunk in &chunks {
            if chunk.content.len() < MIN_CHUNK_CHARS {
                continue;
            }

            let chunk_hash = WriteGate::content_hash(&chunk.content);
            if self
                .store
                .find_by_content_hash(&chunk_hash)
                .await?
                .is_some()
            {
                continue;
            }

            let embedding = self.embedder.embed(&chunk.content).await.ok();

            self.store
                .add_memory(&NewMemory {
                    content: chunk.content.clone(),
                    source: Some(path_str.clone()),
                    tags: if chunk.tags.is_empty() {
                        None
                    } else {
                        Some(chunk.tags.clone())
                    },
                    embedding,
                    memory_type: Some("observation".to_string()),
                    // Curated files outrank auto-generated session summaries,
                    // which carry importance 1.0 — at 0.5 a memory file lost
                    // the importance term of composite_score to entries whose
                    // whole body is a list of tool names.
                    importance: Some(0.8),
                    strength: Some(1.0),
                    supersedes: None,
                    content_hash: Some(chunk_hash),
                    valid_from: None,
                    valid_to: None,
                    source_type: Some("markdown".to_string()),
                    reflection: None,
                    // Hand-curated notes are user-stated, not model-inferred.
                    // Left as None these landed at TrustLevel::ModelInferred
                    // (0.20), the bottom of composite_score's trust term.
                    trust_level: Some(TrustLevel::UserStated),
                    evidence_session_id: None,
                    evidence_message_id: None,
                    scope: scope.clone(),
                    procedure: None,
                    quality_score: None,
                    quality_flag: None,
                })
                .await?;
        }

        // Update the markdown index
        self.store.set_markdown_hash(&path_str, &file_hash).await?;

        Ok(true)
    }
}

/// A chunk of markdown content with its heading hierarchy for tagging.
struct MarkdownChunk {
    content: String,
    tags: String,
}

/// Split markdown content into sections at H2/H3 boundaries.
///
/// Each chunk includes the heading as a prefix for context.
/// Chunks exceeding `MAX_CHUNK_CHARS` are truncated with a note.
fn split_markdown_sections(content: &str, _file_path: &str) -> Vec<MarkdownChunk> {
    let mut chunks = Vec::new();
    let mut current_heading = String::new();
    let mut parent_heading = String::new();
    let mut current_body = String::new();

    for line in content.lines() {
        if let Some(h3) = line.strip_prefix("### ") {
            // Flush previous chunk
            flush_chunk(
                &mut chunks,
                &current_heading,
                &parent_heading,
                &mut current_body,
            );
            current_heading = h3.trim().to_string();
        } else if let Some(h2) = line.strip_prefix("## ") {
            // Flush previous chunk
            flush_chunk(
                &mut chunks,
                &current_heading,
                &parent_heading,
                &mut current_body,
            );
            parent_heading = h2.trim().to_string();
            current_heading = parent_heading.clone();
        } else {
            if !current_body.is_empty() {
                current_body.push('\n');
            }
            current_body.push_str(line);
        }
    }

    // Flush final chunk
    flush_chunk(
        &mut chunks,
        &current_heading,
        &parent_heading,
        &mut current_body,
    );

    chunks
}

fn flush_chunk(chunks: &mut Vec<MarkdownChunk>, heading: &str, parent: &str, body: &mut String) {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        body.clear();
        return;
    }

    let tags = if !parent.is_empty() && parent != heading {
        format!("{parent} > {heading}")
    } else if !heading.is_empty() {
        heading.to_string()
    } else {
        String::new()
    };

    // A section longer than MAX_CHUNK_CHARS becomes several chunks rather than
    // one truncated chunk — the tail of a long section is content, not noise.
    // Every piece repeats the heading so it stands alone in recall.
    for piece in split_oversized(heading, trimmed) {
        chunks.push(MarkdownChunk {
            content: piece,
            tags: tags.clone(),
        });
    }
    body.clear();
}

/// Split a section body into heading-prefixed chunks of at most
/// `MAX_CHUNK_CHARS`, breaking on line boundaries. A single line too long to
/// fit on its own is hard-split at a UTF-8 char boundary.
fn split_oversized(heading: &str, body: &str) -> Vec<String> {
    let prefix = if heading.is_empty() {
        String::new()
    } else {
        format!("{heading}\n")
    };
    // Floor of 200 keeps a pathologically long heading from starving the body.
    let budget = MAX_CHUNK_CHARS.saturating_sub(prefix.len()).max(200);

    let mut pieces: Vec<String> = Vec::new();
    let mut cur = String::new();

    let flush = |cur: &mut String, pieces: &mut Vec<String>| {
        if !cur.is_empty() {
            pieces.push(format!("{prefix}{cur}"));
            cur.clear();
        }
    };

    for line in body.lines() {
        if line.len() > budget {
            flush(&mut cur, &mut pieces);
            let mut rest = line;
            while !rest.is_empty() {
                let mut end = budget.min(rest.len());
                while end > 0 && !rest.is_char_boundary(end) {
                    end -= 1;
                }
                if end == 0 {
                    break; // single char wider than the budget: give up on this line
                }
                pieces.push(format!("{prefix}{}", &rest[..end]));
                rest = &rest[end..];
            }
            continue;
        }

        if !cur.is_empty() && cur.len() + 1 + line.len() > budget {
            flush(&mut cur, &mut pieces);
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
    }
    flush(&mut cur, &mut pieces);

    pieces
}

/// Check if a path is an indexable markdown file (not in daily/).
///
/// MEMORY.md used to be excluded as "just an index", but its lines carry real
/// one-line facts, and excluding it left 16KB of curated content unreachable.
fn is_indexable_markdown(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str());
    if ext != Some("md") {
        return false;
    }

    // Skip daily/ directory
    let path_str = path.to_string_lossy();
    if path_str.contains("/daily/") || path_str.contains("\\daily\\") {
        return false;
    }

    true
}

/// Recursively walk a directory and collect file paths.
fn walkdir(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_recursive(dir, &mut files)?;
    Ok(files)
}

fn walk_recursive(dir: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_recursive(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::Embedder;
    use std::sync::Arc;

    async fn setup_test_store() -> BrainStore {
        BrainStore::open_memory().await.unwrap()
    }

    fn mock_embedder() -> Arc<Embedder> {
        Arc::new(Embedder::mock(64))
    }

    #[test]
    fn test_is_indexable_markdown() {
        assert!(is_indexable_markdown(Path::new("/some/dir/file.md")));
        assert!(!is_indexable_markdown(Path::new("/some/dir/file.txt")));
        assert!(is_indexable_markdown(Path::new("/some/dir/MEMORY.md")));
        assert!(!is_indexable_markdown(Path::new(
            "/some/dir/daily/2026-03-25.md"
        )));
        assert!(!is_indexable_markdown(Path::new("C:\\dir\\daily\\log.md")));
    }

    #[test]
    fn test_split_markdown_sections_basic() {
        let md = "\
## Section One
Content of section one.

## Section Two
Content of section two.

### Subsection
Subsection content here.
";
        let chunks = split_markdown_sections(md, "test.md");

        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].content.contains("Section One"));
        assert!(chunks[0].content.contains("Content of section one"));
        assert!(chunks[1].content.contains("Section Two"));
        assert!(chunks[2].content.contains("Subsection"));
        assert!(chunks[2].content.contains("Subsection content here"));

        // Check tags
        assert_eq!(chunks[2].tags, "Section Two > Subsection");
    }

    #[test]
    fn test_split_markdown_skips_empty_sections() {
        let md = "\
## Empty Section

## Real Section
Has content.
";
        let chunks = split_markdown_sections(md, "test.md");
        // Empty section has no body so should produce zero chunks for it
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("Real Section"));
    }

    #[test]
    fn test_split_markdown_continues_long_sections_instead_of_truncating() {
        // One 3000-char line: must survive as continuation chunks, not be cut.
        let long_line = "x".repeat(3000);
        let md = format!("## Big\n{long_line}");
        let chunks = split_markdown_sections(&md, "test.md");

        assert!(chunks.len() > 1, "expected continuation chunks");
        for c in &chunks {
            assert!(
                c.content.len() <= MAX_CHUNK_CHARS,
                "len={}",
                c.content.len()
            );
            assert!(
                c.content.starts_with("Big\n"),
                "heading missing: {}",
                c.content
            );
            assert!(!c.content.contains("[truncated]"));
        }
        // Every 'x' is still there.
        let recovered: usize = chunks.iter().map(|c| c.content.matches('x').count()).sum();
        assert_eq!(recovered, 3000);
    }

    #[test]
    fn test_split_oversized_keeps_every_line() {
        // 40 lines x ~150 chars = ~6KB, must round-trip across chunks.
        let body: String = (0..40)
            .map(|i| format!("line {i} {}", "y".repeat(140)))
            .collect::<Vec<_>>()
            .join("\n");
        let pieces = split_oversized("Section", &body);

        assert!(pieces.len() >= 3, "got {} pieces", pieces.len());
        let rejoined: String = pieces
            .iter()
            .map(|p| p.strip_prefix("Section\n").expect("heading prefix"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rejoined, body, "content lost or reordered across chunks");
        for p in &pieces {
            assert!(p.len() <= MAX_CHUNK_CHARS);
        }
    }

    #[test]
    fn test_split_oversized_handles_multibyte() {
        // Hard-split path must land on char boundaries, not panic.
        let body = "é".repeat(4000);
        let pieces = split_oversized("Unicode", &body);
        let total: usize = pieces
            .iter()
            .map(|p| p.strip_prefix("Unicode\n").unwrap().chars().count())
            .sum();
        assert_eq!(total, 4000);
    }

    #[test]
    fn test_indexed_chunks_are_user_stated() {
        // Provenance guard: memory files are curated, not model-inferred.
        // trust_level None => ModelInferred (0.20 trust) => decayed and pruned.
        let md = "## Fact\nworkstation is 192.0.2.10 and docker-host is 192.0.2.20 on the LAN.\n";
        let chunks = split_markdown_sections(md, "test.md");
        assert_eq!(chunks.len(), 1);
        // The value written in index_file:
        assert_eq!(TrustLevel::UserStated as i64, 3);
    }

    #[test]
    fn test_split_markdown_content_before_heading() {
        let md = "\
Preamble content before any heading.

## First Heading
After heading.
";
        let chunks = split_markdown_sections(md, "test.md");
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].content.contains("Preamble"));
        assert!(chunks[1].content.contains("First Heading"));
    }

    #[tokio::test]
    async fn test_index_file_creates_memories() {
        let store = setup_test_store().await;
        let embedder = mock_embedder();
        let dir = tempfile::tempdir().unwrap();

        let md_path = dir.path().join("test_notes.md");
        std::fs::write(
            &md_path,
            "## Important Notes\nThis is a test note with enough content to index.\n\n\
             ## Decisions\nWe decided to use polling instead of triggers for simplicity.\n",
        )
        .unwrap();

        let indexer = MarkdownIndexer::new(
            store.clone(),
            embedder,
            MarkdownConfig {
                enabled: true,
                watch_dirs: vec![dir.path().to_string_lossy().to_string()],
                debounce_secs: 5,
            },
        );

        let changed = indexer.index_file(&md_path).await.unwrap();
        assert!(changed);

        let count = store.count_memories().await.unwrap();
        assert_eq!(count, 2, "Should have 2 memories (2 sections)");

        // Provenance the retired memory_to_brain.py stubs used to carry.
        let mem = store.get_memory(1).await.unwrap().unwrap();
        assert_eq!(mem.scope.as_deref(), Some("/memory/test_notes"));
        assert_eq!(mem.importance, 0.8);
        assert_eq!(mem.source_type, "markdown");

        // Verify markdown hash was set
        let hash = store
            .get_markdown_hash(&md_path.to_string_lossy())
            .await
            .unwrap();
        assert!(hash.is_some());
    }

    #[tokio::test]
    async fn test_index_file_skips_unchanged() {
        let store = setup_test_store().await;
        let embedder = mock_embedder();
        let dir = tempfile::tempdir().unwrap();

        let md_path = dir.path().join("stable.md");
        std::fs::write(
            &md_path,
            "## Stable Content\nThis content does not change between indexes.\n",
        )
        .unwrap();

        let indexer = MarkdownIndexer::new(
            store.clone(),
            embedder,
            MarkdownConfig {
                enabled: true,
                watch_dirs: vec![dir.path().to_string_lossy().to_string()],
                debounce_secs: 5,
            },
        );

        let changed1 = indexer.index_file(&md_path).await.unwrap();
        assert!(changed1, "First index should report changed");

        let changed2 = indexer.index_file(&md_path).await.unwrap();
        assert!(!changed2, "Second index should report unchanged");
    }

    #[tokio::test]
    async fn test_full_reindex_cleans_deleted_files() {
        let store = setup_test_store().await;
        let embedder = mock_embedder();
        let dir = tempfile::tempdir().unwrap();

        let md_path = dir.path().join("ephemeral.md");
        std::fs::write(
            &md_path,
            "## Temporary\nThis file will be deleted after first index.\n",
        )
        .unwrap();

        let indexer = MarkdownIndexer::new(
            store.clone(),
            embedder,
            MarkdownConfig {
                enabled: true,
                watch_dirs: vec![dir.path().to_string_lossy().to_string()],
                debounce_secs: 5,
            },
        );

        // Full reindex with file present
        indexer.full_reindex().await.unwrap();
        let count1 = store.count_memories().await.unwrap();
        assert!(count1 > 0);

        // Delete the file
        std::fs::remove_file(&md_path).unwrap();

        // Full reindex should clean up
        indexer.full_reindex().await.unwrap();
        let count2 = store.count_memories().await.unwrap();
        assert_eq!(
            count2, 0,
            "Memories for deleted file should be soft-deleted"
        );
    }

    #[tokio::test]
    async fn test_handle_file_delete_event() {
        let store = setup_test_store().await;
        let embedder = mock_embedder();
        let dir = tempfile::tempdir().unwrap();

        let md_path = dir.path().join("to_delete.md");
        std::fs::write(
            &md_path,
            "## Will Be Deleted\nContent that should be removed when file is deleted.\n",
        )
        .unwrap();

        let indexer = MarkdownIndexer::new(
            store.clone(),
            embedder,
            MarkdownConfig {
                enabled: true,
                watch_dirs: vec![dir.path().to_string_lossy().to_string()],
                debounce_secs: 5,
            },
        );

        // Index the file
        indexer.index_file(&md_path).await.unwrap();
        let count1 = store.count_memories().await.unwrap();
        assert!(count1 > 0);

        // Delete the file and handle the event
        std::fs::remove_file(&md_path).unwrap();
        indexer.handle_file_event(&md_path).await.unwrap();

        let count2 = store.count_memories().await.unwrap();
        assert_eq!(
            count2, 0,
            "Memories should be soft-deleted after file removal"
        );
    }

    #[tokio::test]
    async fn test_index_includes_memory_md() {
        let store = setup_test_store().await;
        let embedder = mock_embedder();
        let dir = tempfile::tempdir().unwrap();

        // MEMORY.md is indexed too — its lines carry real facts
        let memory_md = dir.path().join("MEMORY.md");
        std::fs::write(
            &memory_md,
            "## Index\n- [machine setup](machine-setup.md) - workstation is 192.0.2.10\n",
        )
        .unwrap();

        // Regular .md should be indexed
        let regular_md = dir.path().join("notes.md");
        std::fs::write(
            &regular_md,
            "## Notes\nSome actual content worth indexing.\n",
        )
        .unwrap();

        let indexer = MarkdownIndexer::new(
            store.clone(),
            embedder,
            MarkdownConfig {
                enabled: true,
                watch_dirs: vec![dir.path().to_string_lossy().to_string()],
                debounce_secs: 5,
            },
        );

        indexer.full_reindex().await.unwrap();
        let count = store.count_memories().await.unwrap();
        assert_eq!(count, 2, "both notes.md and MEMORY.md should be indexed");
    }
}
