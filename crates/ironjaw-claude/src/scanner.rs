use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use tracing::{debug, warn};

use crate::types::HistoricalSession;

/// Scans `~/.claude/projects/` directory structure for Claude Code session JSONL files.
pub struct SessionScanner;

/// Maximum number of lines to read from the beginning of a JSONL file for metadata extraction.
const MAX_SCAN_LINES: usize = 50;

/// Maximum number of lines to reverse-scan from the end for usage/context data.
const MAX_TAIL_LINES: usize = 100;

/// Maximum file size (bytes) for tail reading. Larger files skip tail scan.
const MAX_FILE_SIZE_FOR_TAIL: u64 = 10 * 1024 * 1024; // 10 MB

/// Maximum length of `first_message` before truncation.
const MAX_FIRST_MESSAGE_LEN: usize = 200;

/// Default context window size for percentage calculation.
const DEFAULT_CONTEXT_WINDOW: f64 = 200_000.0;

impl SessionScanner {
    /// Scan a base path (typically `~/.claude/projects/`) for session JSONL files.
    ///
    /// Each subdirectory under `base_path` represents a project. Inside each
    /// project directory, `.jsonl` files are session transcripts.
    ///
    /// This is a best-effort scanner -- files that cannot be parsed are skipped
    /// with a warning log.
    pub fn scan(base_path: &Path) -> Vec<HistoricalSession> {
        let mut sessions = Vec::new();

        // Load the shared LLM title cache once per scan (cheap; a few ms to parse ~250KB).
        let titles = load_titles_cache();

        let project_dirs = match std::fs::read_dir(base_path) {
            Ok(entries) => entries,
            Err(e) => {
                warn!("failed to read base path {}: {e}", base_path.display());
                return sessions;
            }
        };

        for project_entry in project_dirs.flatten() {
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }

            let project_name = project_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();

            debug!("scanning project: {project_name}");

            let jsonl_files = match Self::find_jsonl_files(&project_path) {
                Ok(files) => files,
                Err(e) => {
                    warn!("failed to scan project dir {}: {e}", project_path.display());
                    continue;
                }
            };

            for jsonl_path in jsonl_files {
                match Self::parse_session_file(&jsonl_path, &project_name, &titles) {
                    Ok(session) => sessions.push(session),
                    Err(e) => {
                        warn!("failed to parse {}: {e}", jsonl_path.display());
                    }
                }
            }
        }

        sessions
    }

    /// Find all `.jsonl` files within a project directory (non-recursive).
    fn find_jsonl_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    if ext == "jsonl" {
                        files.push(path);
                    }
                }
            }
        }
        Ok(files)
    }

    /// Parse a single JSONL session file to extract metadata.
    ///
    /// Returns `Err` if the file cannot be read or if the session should be
    /// filtered out (e.g. agent sub-sessions with `agent-` prefix).
    fn parse_session_file(
        path: &Path,
        project: &str,
        titles: &HashMap<String, String>,
    ) -> Result<HistoricalSession, Box<dyn std::error::Error>> {
        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        // Filter out agent sub-sessions (subagent JSONL files).
        if session_id.starts_with("agent-") {
            return Err("agent sub-session filtered".into());
        }

        let content = std::fs::read_to_string(path)?;
        let all_lines: Vec<&str> = content.lines().collect();
        let head_lines = all_lines.iter().take(MAX_SCAN_LINES);

        let mut cwd: Option<String> = None;
        let mut model: Option<String> = None;
        let mut first_message: Option<String> = None;
        let mut compaction_count: u32 = 0;

        for line in head_lines {
            let parsed: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // CC JSONL uses top-level "type" (user/assistant/summary/progress),
            // with nested "message" containing role, content, model, etc.
            let entry_type = parsed.get("type").and_then(|r| r.as_str()).unwrap_or("");
            let message = parsed.get("message");

            // Extract cwd from top-level field (present on user/assistant entries)
            if cwd.is_none() {
                if let Some(c) = parsed.get("cwd").and_then(|c| c.as_str()) {
                    cwd = Some(c.to_string());
                }
            }

            // Extract model from assistant messages (nested in message.model)
            if entry_type == "assistant" && model.is_none() {
                if let Some(m) = message
                    .and_then(|msg| msg.get("model"))
                    .and_then(|m| m.as_str())
                {
                    model = Some(m.to_string());
                }
            }

            // Extract first user message (nested in message.content)
            if entry_type == "user" && first_message.is_none() {
                if let Some(text) = message
                    .and_then(|msg| msg.get("content"))
                    .and_then(|c| c.as_str())
                {
                    first_message = Some(truncate_str(text, MAX_FIRST_MESSAGE_LEN));
                }
            }

            // Count compaction boundaries (summary type with compact indicators)
            if entry_type == "summary" {
                let is_compact_subtype = parsed
                    .get("subtype")
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| s == "compact_boundary");

                let content_has_compact = message
                    .and_then(|msg| msg.get("content"))
                    .and_then(|c| c.as_str())
                    .is_some_and(|text| text.to_lowercase().contains("compact"));

                if is_compact_subtype || content_has_compact {
                    compaction_count += 1;
                }
            }
        }

        // Read usage tokens from the LAST assistant message for accurate context%.
        // The most recent assistant message has the current context window usage.
        let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let (last_input_tokens, last_cache_creation, last_cache_read) =
            if file_size <= MAX_FILE_SIZE_FOR_TAIL {
                extract_tail_usage(&all_lines)
            } else {
                debug!(
                    "skipping tail read for {} ({}MB > 10MB limit)",
                    path.display(),
                    file_size / (1024 * 1024)
                );
                (None, None, None)
            };

        // Calculate context percentage
        let context_pct =
            calculate_context_pct(last_input_tokens, last_cache_creation, last_cache_read);

        // Get last_activity from file mtime
        let last_activity = std::fs::metadata(path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .map(|mtime| {
                let duration = mtime
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                Utc.timestamp_opt(duration.as_secs() as i64, duration.subsec_nanos())
                    .single()
                    .unwrap_or_else(Utc::now)
            });

        // Title resolution: titles.json cache, then session-memory/summary.md, then fall back
        let title = resolve_title(path, &first_message, titles);

        Ok(HistoricalSession {
            session_id,
            project: project.to_string(),
            cwd,
            title,
            model,
            context_pct,
            compaction_count,
            last_activity,
            first_message,
            machine: "workstation".to_string(),
        })
    }
}

/// Extract usage tokens from the last assistant message by scanning lines in reverse.
///
/// Iterates backwards through at most `MAX_TAIL_LINES` lines to find the most recent
/// assistant message with a `usage` block containing token counts.
fn extract_tail_usage(lines: &[&str]) -> (Option<u64>, Option<u64>, Option<u64>) {
    let start = lines.len().saturating_sub(MAX_TAIL_LINES);
    for line in lines[start..].iter().rev() {
        let parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = parsed.get("type").and_then(|r| r.as_str()).unwrap_or("");
        if entry_type != "assistant" {
            continue;
        }

        // Usage is nested inside message.usage in CC JSONL
        if let Some(usage) = parsed
            .get("message")
            .and_then(|msg| msg.get("usage"))
            .or_else(|| parsed.get("usage"))
        {
            let input = usage.get("input_tokens").and_then(|t| t.as_u64());
            if input.is_some() {
                let creation = usage
                    .get("cache_creation_input_tokens")
                    .and_then(|t| t.as_u64());
                let read = usage
                    .get("cache_read_input_tokens")
                    .and_then(|t| t.as_u64());
                return (input, creation, read);
            }
        }
    }
    (None, None, None)
}

/// Try to extract a CWD from free-text system message content.
fn extract_cwd_from_content(text: &str) -> Option<String> {
    // Look for patterns like "cwd: /path/to/dir" or "Working directory: /path"
    for line in text.lines() {
        let lower = line.to_lowercase();
        if lower.contains("cwd:") || lower.contains("working directory:") {
            if let Some(idx) = line.find(':') {
                let path = line[idx + 1..].trim();
                if !path.is_empty() {
                    return Some(path.to_string());
                }
            }
        }
    }
    None
}

/// Calculate context percentage from token usage fields.
pub fn calculate_context_pct(
    input_tokens: Option<u64>,
    cache_creation: Option<u64>,
    cache_read: Option<u64>,
) -> Option<f32> {
    let input = input_tokens?;
    let creation = cache_creation.unwrap_or(0);
    let read = cache_read.unwrap_or(0);
    let total = input + creation + read;
    Some((total as f64 / DEFAULT_CONTEXT_WINDOW * 100.0) as f32)
}

/// Default location of the shared title cache, the canonical store of
/// LLM-generated session titles. cc-bridge writes it; the gateway only reads it.
/// Relative to the gateway's working directory. Point `IRONJAW_TITLES_CACHE` at
/// cc-bridge's own data directory if it runs somewhere else.
const DEFAULT_TITLES_CACHE: &str = "data/titles.json";

/// Load the shared title cache as a `session_id -> title` map.
///
/// Best-effort: a missing or unparseable file yields an empty map and callers
/// fall back to their own title resolution. The cache only stores upgraded
/// titles (llm / ollama / manual sources), so any non-empty entry is preferred.
// ponytail: parses the whole file per scan() call. ~250KB / a few ms is fine for
// cc.sessions request rates; add an mtime-gated cache if that ever gets hot.
fn load_titles_cache() -> HashMap<String, String> {
    let path =
        std::env::var("IRONJAW_TITLES_CACHE").unwrap_or_else(|_| DEFAULT_TITLES_CACHE.to_string());
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_titles_json(&content),
        Err(_) => HashMap::new(), // not present (e.g. non-workstation host) — silent, callers fall back
    }
}

/// Parse `titles.json` content into a `session_id -> title` map.
/// Skips blank titles; malformed JSON yields an empty map.
fn parse_titles_json(content: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            warn!("failed to parse titles cache: {e}");
            return map;
        }
    };
    if let Some(titles) = parsed.get("titles").and_then(|t| t.as_object()) {
        for (session_id, entry) in titles {
            if let Some(title) = entry.get("title").and_then(|t| t.as_str()) {
                let trimmed = title.trim();
                if !trimmed.is_empty() {
                    map.insert(session_id.clone(), trimmed.to_string());
                }
            }
        }
    }
    map
}

/// Resolve the session title.
///
/// Priority order:
/// 1. Shared LLM title cache (`titles.json`, keyed by session id)
/// 2. `session-memory/summary.md` (line 4 if it exists)
/// 3. First user message (truncated)
fn resolve_title(
    jsonl_path: &Path,
    first_message: &Option<String>,
    titles: &HashMap<String, String>,
) -> Option<String> {
    let session_id = jsonl_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    // 1. Shared LLM title cache — the canonical good titles.
    if let Some(cached) = titles.get(session_id) {
        return Some(cached.clone());
    }

    // Check for summary.md in sibling session-memory directory
    if let Some(parent) = jsonl_path.parent() {
        let summary_path = parent
            .join("session-memory")
            .join(format!("{session_id}-summary.md"));

        if let Ok(content) = std::fs::read_to_string(&summary_path) {
            // Line 4 (0-indexed line 3) typically has the title
            if let Some(line) = content.lines().nth(3) {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }

        // Also try summary.md directly
        let summary_path = parent.join("session-memory").join("summary.md");
        if let Ok(content) = std::fs::read_to_string(&summary_path) {
            if let Some(line) = content.lines().nth(3) {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }

    // Fall back to first user message
    first_message.clone()
}

/// Look up the CWD for a session by scanning the sessions directory.
///
/// Checks each project directory for a JSONL file matching `session_id`.
/// Returns the CWD from the session's JSONL metadata if available, otherwise
/// attempts to decode the project directory name back to a filesystem path.
///
/// This is a lightweight scan -- it only checks for file existence and reads
/// the first few lines of the matching JSONL, not all sessions.
pub fn lookup_session_cwd(session_id: &str, sessions_dir: &Path) -> Option<String> {
    let project_dirs = std::fs::read_dir(sessions_dir).ok()?;

    for entry in project_dirs.flatten() {
        let project_path = entry.path();
        if !project_path.is_dir() {
            continue;
        }

        let jsonl_path = project_path.join(format!("{session_id}.jsonl"));
        if !jsonl_path.is_file() {
            continue;
        }

        // Found the session file. Try to extract the CWD from the JSONL content.
        if let Ok(content) = std::fs::read_to_string(&jsonl_path) {
            for line in content.lines().take(MAX_SCAN_LINES) {
                let parsed: serde_json::Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let role = parsed.get("role").and_then(|r| r.as_str()).unwrap_or("");
                if role == "system" {
                    // Try params.cwd first
                    if let Some(c) = parsed
                        .get("params")
                        .and_then(|p| p.get("cwd"))
                        .and_then(|c| c.as_str())
                    {
                        debug!("lookup_session_cwd: found cwd={c} for session {session_id}");
                        return Some(c.to_string());
                    }
                    // Try free-text extraction
                    if let Some(c) = parsed
                        .get("content")
                        .and_then(|c| c.as_str())
                        .and_then(extract_cwd_from_content)
                    {
                        debug!("lookup_session_cwd: found cwd={c} for session {session_id}");
                        return Some(c);
                    }
                }
            }
        }

        // JSONL didn't have a CWD -- fall back to decoding the project directory name.
        let project_name = entry.file_name().to_str().unwrap_or("").to_string();
        if let Some(decoded) = decode_project_dir(&project_name) {
            debug!(
                "lookup_session_cwd: decoded project dir '{project_name}' -> '{decoded}' \
                 for session {session_id}"
            );
            return Some(decoded);
        }

        // Could not determine CWD from this project directory.
        warn!(
            "lookup_session_cwd: found session {session_id} in project '{project_name}' \
             but could not determine CWD"
        );
        return None;
    }

    None
}

/// Decode a Claude Code project directory name back to a filesystem path.
///
/// Claude Code encodes paths like `C:\code\my-app` as `C--code-my-app`
/// (drive colon becomes `--`, path separators become `-`).
///
/// This is a best-effort decode: since `-` in directory names is ambiguous with the
/// path separator encoding, we try the naive decode first and verify the path exists.
/// Returns `None` if the decoded path doesn't exist on disk.
pub fn decode_project_dir(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }

    // On Windows, the format is: `X--rest-of-path` where X is the drive letter.
    // On Unix, it would be something else, but Claude Code on Unix uses different encoding.
    if cfg!(windows) {
        // Look for drive letter pattern: single char followed by `--`
        if name.len() >= 3 && name.as_bytes()[1] == b'-' && name.as_bytes()[2] == b'-' {
            let drive = &name[..1];
            let rest = &name[3..]; // after the `--`

            // Naive decode: replace all `-` with `\`
            let naive_path = format!("{}:\\{}", drive, rest.replace('-', "\\"));
            if Path::new(&naive_path).is_dir() {
                return Some(naive_path);
            }

            // If naive decode failed (hyphenated directory names), try the raw
            // project dir name as-is -- it won't be a valid path, so return None
            // and let the caller fall through to default_cwd.
            return None;
        }
    }

    // Non-Windows or unrecognized format -- try replacing `-` with `/`
    let decoded = name.replace('-', "/");
    if Path::new(&decoded).is_dir() {
        return Some(decoded);
    }

    None
}

/// Check if a CC session is actively running in a terminal (interactive mode).
///
/// Scans `~/.claude/sessions/` for JSON files containing PID + sessionId pairs.
/// If the session is found and the PID is alive, returns `true`.
pub fn is_session_active_in_terminal(session_id: &str) -> bool {
    let home = if cfg!(windows) {
        std::env::var("USERPROFILE").unwrap_or_default()
    } else {
        std::env::var("HOME").unwrap_or_default()
    };
    if home.is_empty() {
        return false;
    }

    let sessions_dir = Path::new(&home).join(".claude").join("sessions");
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(_) => return false,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let parsed: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let file_session_id = parsed.get("sessionId").and_then(|v| v.as_str());
        if file_session_id != Some(session_id) {
            continue;
        }

        let pid = match parsed.get("pid").and_then(|v| v.as_u64()) {
            Some(p) => p as u32,
            None => continue,
        };

        return is_pid_alive(pid);
    }

    false
}

/// Check if a process with the given PID is alive.
fn is_pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        // Use tasklist with PID filter -- exit code 0 means found
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|output| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                // tasklist returns the PID in the output if found
                stdout.contains(&pid.to_string())
            })
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        // On Unix, signal 0 checks if process exists
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
}

/// Truncate a string to at most `max_len` characters, appending "..." if truncated.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_titles_json_extracts_nonempty_titles() {
        let json = r#"{"version":1,"titles":{
            "s1":{"title":"Nice LLM Title","source":"llm"},
            "s2":{"title":"   ","source":"llm"},
            "s3":{"source":"llm"}
        }}"#;
        let map = parse_titles_json(json);
        assert_eq!(map.get("s1"), Some(&"Nice LLM Title".to_string()));
        assert_eq!(map.get("s2"), None, "blank title skipped");
        assert_eq!(map.get("s3"), None, "missing title skipped");
    }

    #[test]
    fn parse_titles_json_handles_garbage_and_empty() {
        assert!(parse_titles_json("not json at all").is_empty());
        assert!(parse_titles_json("{}").is_empty());
    }

    #[test]
    fn resolve_title_prefers_titles_cache_over_first_message() {
        let mut titles = HashMap::new();
        titles.insert("abc-123".to_string(), "Cached LLM Title".to_string());
        let path = Path::new("/nonexistent/abc-123.jsonl");
        let got = resolve_title(path, &Some("first user message".into()), &titles);
        assert_eq!(got, Some("Cached LLM Title".to_string()));
    }

    #[test]
    fn resolve_title_falls_back_to_first_message_when_not_cached() {
        let titles: HashMap<String, String> = HashMap::new();
        let path = Path::new("/nonexistent/missing.jsonl");
        let got = resolve_title(path, &Some("first user message".into()), &titles);
        assert_eq!(got, Some("first user message".to_string()));
    }

    #[test]
    fn test_truncate_str_short() {
        assert_eq!(truncate_str("hello", 200), "hello");
    }

    #[test]
    fn test_truncate_str_exact() {
        let s = "a".repeat(200);
        assert_eq!(truncate_str(&s, 200), s);
    }

    #[test]
    fn test_truncate_str_long() {
        let s = "a".repeat(300);
        let result = truncate_str(&s, 200);
        assert!(result.ends_with("..."));
        assert_eq!(result.len(), 200);
    }

    #[test]
    fn test_calculate_context_pct_none_input() {
        assert!(calculate_context_pct(None, None, None).is_none());
    }

    #[test]
    fn test_calculate_context_pct_basic() {
        // 100_000 tokens out of 200_000 = 50%
        let pct = calculate_context_pct(Some(100_000), None, None).unwrap();
        assert!((pct - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_calculate_context_pct_with_cache() {
        // 50K input + 30K creation + 20K read = 100K total => 50%
        let pct = calculate_context_pct(Some(50_000), Some(30_000), Some(20_000)).unwrap();
        assert!((pct - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_calculate_context_pct_full() {
        // 200K tokens = 100%
        let pct = calculate_context_pct(Some(200_000), None, None).unwrap();
        assert!((pct - 100.0).abs() < 0.01);
    }

    #[test]
    fn test_calculate_context_pct_over_limit() {
        // Can exceed 100% if tokens > context window
        let pct = calculate_context_pct(Some(300_000), None, None).unwrap();
        assert!(pct > 100.0);
    }

    #[test]
    fn test_extract_cwd_from_content_found() {
        let text = "System initialized\ncwd: /home/user/projects/test\nReady.";
        assert_eq!(
            extract_cwd_from_content(text),
            Some("/home/user/projects/test".to_string())
        );
    }

    #[test]
    fn test_extract_cwd_from_content_working_dir() {
        let text = "Working directory: C:\\Users\\sean\\Projects\\ironjaw";
        assert_eq!(
            extract_cwd_from_content(text),
            Some("C:\\Users\\sean\\Projects\\ironjaw".to_string())
        );
    }

    #[test]
    fn test_extract_cwd_from_content_not_found() {
        let text = "Hello world\nNo path here.";
        assert!(extract_cwd_from_content(text).is_none());
    }

    #[test]
    fn test_scan_empty_dir() {
        let tmp = std::env::temp_dir().join("ironjaw-claude-test-scan-empty");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let sessions = SessionScanner::scan(&tmp);
        assert!(sessions.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_scan_nonexistent_dir() {
        let sessions = SessionScanner::scan(Path::new("/nonexistent/path/for/test"));
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_scan_with_session_file() {
        let tmp = std::env::temp_dir().join("ironjaw-claude-test-scan-session");
        let _ = fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("my-project");
        fs::create_dir_all(&project_dir).unwrap();

        // Write a minimal JSONL session file (real CC JSONL format)
        let jsonl_content = [
            r#"{"type":"user","cwd":"/home/user/myproject","message":{"role":"user","content":"Hello, build me a thing"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","model":"claude-3-5-sonnet","content":"Sure!","usage":{"input_tokens":1000,"cache_creation_input_tokens":500,"cache_read_input_tokens":200}}}"#,
        ]
        .join("\n");

        fs::write(project_dir.join("sess-abc-123.jsonl"), &jsonl_content).unwrap();

        let sessions = SessionScanner::scan(&tmp);
        assert_eq!(sessions.len(), 1);

        let s = &sessions[0];
        assert_eq!(s.session_id, "sess-abc-123");
        assert_eq!(s.project, "my-project");
        assert_eq!(s.cwd, Some("/home/user/myproject".to_string()));
        assert_eq!(s.model, Some("claude-3-5-sonnet".to_string()));
        assert_eq!(s.first_message, Some("Hello, build me a thing".to_string()));
        assert_eq!(s.compaction_count, 0);
        assert!(s.context_pct.is_some());
        assert_eq!(s.machine, "workstation");

        // context_pct: (1000 + 500 + 200) / 200000 * 100 = 0.85%
        let pct = s.context_pct.unwrap();
        assert!((pct - 0.85).abs() < 0.01);

        assert!(s.last_activity.is_some());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_scan_counts_compactions() {
        let tmp = std::env::temp_dir().join("ironjaw-claude-test-compaction");
        let _ = fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("proj");
        fs::create_dir_all(&project_dir).unwrap();

        let jsonl_content = [
            r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
            r#"{"type":"summary","subtype":"compact_boundary","message":{"content":"compacted"}}"#,
            r#"{"type":"summary","message":{"content":"another compact message"}}"#,
        ]
        .join("\n");

        fs::write(project_dir.join("sess-1.jsonl"), &jsonl_content).unwrap();

        let sessions = SessionScanner::scan(&tmp);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].compaction_count, 2);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_scan_skips_non_jsonl_files() {
        let tmp = std::env::temp_dir().join("ironjaw-claude-test-skip-nonjsonl");
        let _ = fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("proj");
        fs::create_dir_all(&project_dir).unwrap();

        fs::write(project_dir.join("notes.txt"), "not a session").unwrap();
        fs::write(project_dir.join("data.json"), "{}").unwrap();

        let sessions = SessionScanner::scan(&tmp);
        assert!(sessions.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_scan_skips_malformed_lines() {
        let tmp = std::env::temp_dir().join("ironjaw-claude-test-malformed");
        let _ = fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("proj");
        fs::create_dir_all(&project_dir).unwrap();

        let jsonl_content = [
            "this is not json",
            "also {not valid",
            r#"{"type":"user","message":{"role":"user","content":"valid line"}}"#,
        ]
        .join("\n");

        fs::write(project_dir.join("sess-2.jsonl"), &jsonl_content).unwrap();

        let sessions = SessionScanner::scan(&tmp);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].first_message, Some("valid line".to_string()));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_scan_filters_agent_sessions() {
        let tmp = std::env::temp_dir().join("ironjaw-claude-test-agent-filter");
        let _ = fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("proj");
        fs::create_dir_all(&project_dir).unwrap();

        // Agent session -- should be filtered
        let agent_content =
            [r#"{"type":"user","message":{"role":"user","content":"sub-task"}}"#].join("\n");
        fs::write(project_dir.join("agent-abc-123.jsonl"), &agent_content).unwrap();

        // Normal session -- should be kept
        let normal_content =
            [r#"{"type":"user","message":{"role":"user","content":"main task"}}"#].join("\n");
        fs::write(project_dir.join("sess-xyz-789.jsonl"), &normal_content).unwrap();

        let sessions = SessionScanner::scan(&tmp);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "sess-xyz-789");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_extract_tail_usage_finds_last_assistant() {
        // The last assistant message (at end) should be picked, not the first
        let lines = vec![
            r#"{"type":"user","message":{"role":"user","content":"init"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":"early","usage":{"input_tokens":1000,"cache_creation_input_tokens":100,"cache_read_input_tokens":50}}}"#,
            r#"{"type":"user","message":{"role":"user","content":"more work"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":"later","usage":{"input_tokens":80000,"cache_creation_input_tokens":5000,"cache_read_input_tokens":15000}}}"#,
        ];

        let (input, creation, read) = extract_tail_usage(&lines);
        assert_eq!(input, Some(80_000));
        assert_eq!(creation, Some(5_000));
        assert_eq!(read, Some(15_000));
    }

    #[test]
    fn test_extract_tail_usage_no_assistant() {
        let lines = vec![
            r#"{"type":"user","message":{"role":"user","content":"init"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
        ];

        let (input, creation, read) = extract_tail_usage(&lines);
        assert!(input.is_none());
        assert!(creation.is_none());
        assert!(read.is_none());
    }

    #[test]
    fn test_extract_tail_usage_assistant_without_usage() {
        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":"no usage field"}}"#,
        ];

        let (input, _, _) = extract_tail_usage(&lines);
        assert!(input.is_none());
    }

    #[test]
    fn test_context_pct_uses_tail_not_head() {
        // Verify that context% comes from the LAST assistant message, not the first.
        // Put a small-usage assistant in the head (first 50 lines) and a large-usage
        // assistant at the end (line 60+).
        let tmp = std::env::temp_dir().join("ironjaw-claude-test-tail-context");
        let _ = fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("proj");
        fs::create_dir_all(&project_dir).unwrap();

        let mut lines = Vec::new();
        lines.push(
            r#"{"type":"user","cwd":"/tmp/test","message":{"role":"user","content":"start"}}"#
                .to_string(),
        );
        // Early assistant with small usage (within first 50 lines)
        lines.push(
            r#"{"type":"assistant","message":{"role":"assistant","model":"test","content":"early","usage":{"input_tokens":1000}}}"#
                .to_string(),
        );
        // Pad to push past MAX_SCAN_LINES
        for i in 0..55 {
            lines.push(format!(
                r#"{{"type":"user","message":{{"role":"user","content":"padding {i}"}}}}"#
            ));
        }
        // Late assistant with large usage (well past line 50)
        lines.push(
            r#"{"type":"assistant","message":{"role":"assistant","content":"late","usage":{"input_tokens":150000,"cache_creation_input_tokens":10000,"cache_read_input_tokens":5000}}}"#
                .to_string(),
        );

        let content = lines.join("\n");
        fs::write(project_dir.join("sess-tail.jsonl"), &content).unwrap();

        let sessions = SessionScanner::scan(&tmp);
        assert_eq!(sessions.len(), 1);

        // Should use the LAST assistant: (150000 + 10000 + 5000) / 200000 * 100 = 82.5%
        let pct = sessions[0].context_pct.unwrap();
        assert!(
            (pct - 82.5).abs() < 0.1,
            "expected ~82.5% from tail, got {pct}%"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_scan_output_serializes_camel_case() {
        let tmp = std::env::temp_dir().join("ironjaw-claude-test-scan-camelcase");
        let _ = fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("camel-project");
        fs::create_dir_all(&project_dir).unwrap();

        let jsonl_content = [
            r#"{"type":"user","cwd":"/home/user/test","message":{"role":"user","content":"Hello from the scan pipeline"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","model":"claude-3-opus","content":"Hi!","usage":{"input_tokens":2000,"cache_creation_input_tokens":300,"cache_read_input_tokens":100}}}"#,
        ]
        .join("\n");

        fs::write(project_dir.join("sess-camel-001.jsonl"), &jsonl_content).unwrap();

        let sessions = SessionScanner::scan(&tmp);
        assert_eq!(sessions.len(), 1);

        // Serialize the scanned session and verify camelCase field names
        let value = serde_json::to_value(&sessions[0]).unwrap();
        let obj = value.as_object().unwrap();

        // camelCase keys must be present
        assert!(
            obj.contains_key("sessionId"),
            "scan output missing sessionId"
        );
        assert!(
            obj.contains_key("contextPct"),
            "scan output missing contextPct"
        );
        assert!(
            obj.contains_key("compactionCount"),
            "scan output missing compactionCount"
        );
        assert!(
            obj.contains_key("lastActivity"),
            "scan output missing lastActivity"
        );
        assert!(
            obj.contains_key("firstMessage"),
            "scan output missing firstMessage"
        );

        // snake_case keys must NOT be present
        assert!(
            !obj.contains_key("session_id"),
            "scan output has snake_case session_id"
        );
        assert!(
            !obj.contains_key("context_pct"),
            "scan output has snake_case context_pct"
        );
        assert!(
            !obj.contains_key("compaction_count"),
            "scan output has snake_case compaction_count"
        );
        assert!(
            !obj.contains_key("last_activity"),
            "scan output has snake_case last_activity"
        );
        assert!(
            !obj.contains_key("first_message"),
            "scan output has snake_case first_message"
        );

        // Verify values from the scanned file
        assert_eq!(obj["sessionId"], "sess-camel-001");
        assert_eq!(obj["project"], "camel-project");
        assert_eq!(obj["firstMessage"], "Hello from the scan pipeline");
        assert_eq!(obj["machine"], "workstation");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_lookup_session_cwd_finds_cwd_from_jsonl() {
        let tmp = std::env::temp_dir().join("ironjaw-claude-test-lookup-cwd");
        let _ = fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("C--Users-deploy");
        fs::create_dir_all(&project_dir).unwrap();

        let jsonl_content = [
            r#"{"role":"system","content":"cwd: C:\\Users\\deploy"}"#,
            r#"{"role":"user","content":"hello"}"#,
        ]
        .join("\n");
        fs::write(project_dir.join("sess-lookup-1.jsonl"), &jsonl_content).unwrap();

        let result = lookup_session_cwd("sess-lookup-1", &tmp);
        assert_eq!(result, Some("C:\\Users\\deploy".to_string()));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_lookup_session_cwd_finds_cwd_from_params() {
        let tmp = std::env::temp_dir().join("ironjaw-claude-test-lookup-params");
        let _ = fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("C--Users-deploy-Projects");
        fs::create_dir_all(&project_dir).unwrap();

        let jsonl_content = [
            r#"{"role":"system","params":{"cwd":"C:\\Users\\deploy\\Projects"}}"#,
            r#"{"role":"user","content":"hello"}"#,
        ]
        .join("\n");
        fs::write(project_dir.join("sess-lookup-params.jsonl"), &jsonl_content).unwrap();

        let result = lookup_session_cwd("sess-lookup-params", &tmp);
        assert_eq!(result, Some("C:\\Users\\deploy\\Projects".to_string()));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_lookup_session_cwd_not_found() {
        let tmp = std::env::temp_dir().join("ironjaw-claude-test-lookup-notfound");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let result = lookup_session_cwd("nonexistent-session", &tmp);
        assert!(result.is_none());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_decode_project_dir_empty() {
        assert!(decode_project_dir("").is_none());
    }

    #[test]
    fn test_decode_project_dir_windows_valid_path() {
        // This test only makes sense on Windows where C:\Users exists
        if cfg!(windows) {
            let result = decode_project_dir("C--Users");
            // C:\Users should exist on any Windows machine
            assert_eq!(result, Some("C:\\Users".to_string()));
        }
    }

    #[test]
    fn test_decode_project_dir_windows_nonexistent() {
        if cfg!(windows) {
            // A path that won't exist
            let result = decode_project_dir("Z--nonexistent-fake-path-xyz");
            assert!(result.is_none());
        }
    }

    #[test]
    fn test_machine_field_defaults_to_workstation() {
        let tmp = std::env::temp_dir().join("ironjaw-claude-test-machine");
        let _ = fs::remove_dir_all(&tmp);
        let project_dir = tmp.join("proj");
        fs::create_dir_all(&project_dir).unwrap();

        let content = r#"{"role":"user","content":"test"}"#;
        fs::write(project_dir.join("sess-m1.jsonl"), content).unwrap();

        let sessions = SessionScanner::scan(&tmp);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].machine, "workstation");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_pid_alive_current_process() {
        // Current process PID should always be alive
        let pid = std::process::id();
        assert!(is_pid_alive(pid));
    }

    #[test]
    fn test_is_pid_alive_nonexistent() {
        // PID 999999999 should not exist
        assert!(!is_pid_alive(999_999_999));
    }

    #[test]
    fn test_is_session_active_no_sessions_dir() {
        // With a nonexistent session, should return false
        assert!(!is_session_active_in_terminal(
            "nonexistent-session-xyz-000"
        ));
    }
}
