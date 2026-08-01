use crate::scanner::calculate_context_pct;
use crate::types::{CcUsage, ClaudeError, HistoryEntry, HistoryEntryType, SessionMetadata};
use chrono::{DateTime, Utc};
use std::path::Path;
use tracing::warn;

/// Reads JSONL Claude Code session files into structured conversation history.
pub struct HistoryReader;

impl HistoryReader {
    /// Read paginated history entries from a JSONL session file.
    ///
    /// When `reverse` is true, returns the last `limit` entries (offset from the end),
    /// which is useful for showing recent conversation history first.
    ///
    /// Returns `(entries, total_count_after_filtering)`.
    pub fn read(
        path: &Path,
        offset: usize,
        limit: usize,
        reverse: bool,
    ) -> Result<(Vec<HistoryEntry>, usize), ClaudeError> {
        let content = std::fs::read_to_string(path)?;
        let entries = parse_all_entries(&content);
        let filtered: Vec<HistoryEntry> = entries.into_iter().filter(|e| !is_noise(e)).collect();
        let total = filtered.len();
        let page = if reverse {
            let effective_limit = if limit == 0 { total } else { limit };
            let end = total.saturating_sub(offset);
            let start = end.saturating_sub(effective_limit);
            filtered[start..end].to_vec()
        } else {
            filtered
                .into_iter()
                .skip(offset)
                .take(if limit == 0 { usize::MAX } else { limit })
                .collect()
        };
        Ok((page, total))
    }

    /// Read history entries after `known_count`, returning only new entries.
    ///
    /// If `known_count` >= total filtered entries, returns an empty vec.
    /// Returns `(new_entries, total_count)`.
    pub fn read_since(
        path: &Path,
        known_count: usize,
    ) -> Result<(Vec<HistoryEntry>, usize), ClaudeError> {
        let content = std::fs::read_to_string(path)?;
        let entries = parse_all_entries(&content);
        let filtered: Vec<HistoryEntry> = entries.into_iter().filter(|e| !is_noise(e)).collect();
        let total = filtered.len();
        let new_entries = if known_count >= total {
            Vec::new()
        } else {
            filtered.into_iter().skip(known_count).collect()
        };
        Ok((new_entries, total))
    }

    /// Extract session metadata from a JSONL file.
    pub fn metadata(path: &Path) -> Result<SessionMetadata, ClaudeError> {
        let content = std::fs::read_to_string(path)?;

        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let entries = parse_all_entries(&content);
        let filtered: Vec<&HistoryEntry> = entries.iter().filter(|e| !is_noise(e)).collect();

        // Model: first assistant message with a model field
        let model = entries.iter().find_map(|e| {
            if e.entry_type == HistoryEntryType::Assistant {
                e.model.clone()
            } else {
                None
            }
        });

        // CWD: first system message containing "cwd:"
        let cwd = entries
            .iter()
            .filter(|e| e.entry_type == HistoryEntryType::System)
            .find_map(|e| extract_cwd(&e.content));

        // Compaction count
        let compaction_count = entries
            .iter()
            .filter(|e| e.entry_type == HistoryEntryType::CompactBoundary)
            .count() as u32;

        // Context percentage from last assistant usage
        let context_pct = entries
            .iter()
            .rev()
            .find_map(|e| {
                if e.entry_type == HistoryEntryType::Assistant {
                    e.usage.as_ref()
                } else {
                    None
                }
            })
            .and_then(|usage| calculate_context_pct(Some(usage.input_tokens), None, None));

        // Title: check sibling summary file, then first user message
        let first_user_msg = entries
            .iter()
            .find(|e| e.entry_type == HistoryEntryType::User)
            .map(|e| e.content.clone());
        let title = resolve_title(path, &session_id, &first_user_msg);

        let message_count = filtered.len();

        Ok(SessionMetadata {
            session_id,
            title,
            model,
            context_pct,
            compaction_count,
            cwd,
            message_count,
        })
    }
}

/// Parse all JSONL lines into `HistoryEntry` values, expanding content arrays.
fn parse_all_entries(content: &str) -> Vec<HistoryEntry> {
    let mut entries = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                warn!("skipping malformed JSONL line {idx}: {e}");
                continue;
            }
        };
        let entrypoint = parsed
            .get("entrypoint")
            .and_then(|e| e.as_str())
            .map(|s| s.to_string());
        let mut line_entries = parse_line(&parsed, idx);
        if let Some(ref ep) = entrypoint {
            for entry in &mut line_entries {
                entry.entrypoint = Some(ep.clone());
            }
        }
        entries.append(&mut line_entries);
    }
    entries
}

/// Parse a single JSONL value into one or more `HistoryEntry` values.
fn parse_line(val: &serde_json::Value, line_idx: usize) -> Vec<HistoryEntry> {
    let uuid = val
        .get("uuid")
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("line-{line_idx}"));

    let timestamp = val
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok());

    // Determine the type/role from the top-level fields.
    // Two formats: wrapper format ({type, message}) and flat format ({role, content}).
    let line_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let subtype = val.get("subtype").and_then(|s| s.as_str()).unwrap_or("");

    // Check noise at the raw line level
    if is_noise_type(line_type, subtype) {
        return vec![make_noise_entry(&uuid, timestamp)];
    }

    // Resolve the message object (wrapper vs flat)
    let msg = if val.get("message").is_some() {
        val.get("message").unwrap()
    } else {
        val
    };

    let role = msg
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or(line_type);

    let model = msg
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());

    let usage = msg.get("usage").and_then(parse_usage);

    let content_val = msg.get("content");

    // Classify entry type
    let entry_type = classify(role, line_type, subtype, content_val);

    match content_val {
        Some(serde_json::Value::Array(blocks)) => {
            expand_content_array(blocks, &uuid, role, &model, &usage, timestamp, &entry_type)
        }
        Some(serde_json::Value::String(text)) => {
            // Check for system-reminder noise
            if text.contains("<system-reminder>") {
                return vec![make_noise_entry(&uuid, timestamp)];
            }
            vec![HistoryEntry {
                uuid,
                role: role.to_string(),
                content: text.clone(),
                model,
                usage,
                timestamp,
                entry_type,
                entrypoint: None,
            }]
        }
        _ => {
            // No content or unexpected type — only emit if role is present
            if role.is_empty() {
                vec![make_noise_entry(&uuid, timestamp)]
            } else {
                vec![HistoryEntry {
                    uuid,
                    role: role.to_string(),
                    content: String::new(),
                    model,
                    usage,
                    timestamp,
                    entry_type,
                    entrypoint: None,
                }]
            }
        }
    }
}

/// Expand a content array into multiple entries (text concatenated, tool_use/tool_result split).
fn expand_content_array(
    blocks: &[serde_json::Value],
    uuid: &str,
    role: &str,
    model: &Option<String>,
    usage: &Option<CcUsage>,
    timestamp: Option<DateTime<Utc>>,
    parent_type: &HistoryEntryType,
) -> Vec<HistoryEntry> {
    let mut entries = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();

    for (i, block) in blocks.iter().enumerate() {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match block_type {
            "text" => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    if t.contains("<system-reminder>") {
                        continue;
                    }
                    text_parts.push(t.to_string());
                }
            }
            "tool_use" => {
                // Flush any accumulated text first
                if !text_parts.is_empty() {
                    entries.push(HistoryEntry {
                        uuid: uuid.to_string(),
                        role: role.to_string(),
                        content: text_parts.join("\n"),
                        model: model.clone(),
                        usage: usage.clone(),
                        timestamp,
                        entry_type: parent_type.clone(),
                        entrypoint: None,
                    });
                    text_parts.clear();
                }
                let name = block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown");
                let input = block
                    .get("input")
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                entries.push(HistoryEntry {
                    uuid: format!("{uuid}-tool-{i}"),
                    role: role.to_string(),
                    content: format!("[tool_use: {name}] {input}"),
                    model: model.clone(),
                    usage: None,
                    timestamp,
                    entry_type: HistoryEntryType::ToolUse,
                    entrypoint: None,
                });
            }
            "tool_result" => {
                if !text_parts.is_empty() {
                    entries.push(HistoryEntry {
                        uuid: uuid.to_string(),
                        role: role.to_string(),
                        content: text_parts.join("\n"),
                        model: model.clone(),
                        usage: usage.clone(),
                        timestamp,
                        entry_type: parent_type.clone(),
                        entrypoint: None,
                    });
                    text_parts.clear();
                }
                let result_content = block.get("content").and_then(|c| c.as_str()).unwrap_or("");
                entries.push(HistoryEntry {
                    uuid: format!("{uuid}-result-{i}"),
                    role: role.to_string(),
                    content: result_content.to_string(),
                    model: model.clone(),
                    usage: None,
                    timestamp,
                    entry_type: HistoryEntryType::ToolResult,
                    entrypoint: None,
                });
            }
            _ => {
                // Unknown block type — treat text-like
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
        }
    }

    // Flush remaining text
    if !text_parts.is_empty() {
        entries.push(HistoryEntry {
            uuid: uuid.to_string(),
            role: role.to_string(),
            content: text_parts.join("\n"),
            model: model.clone(),
            usage: usage.clone(),
            timestamp,
            entry_type: parent_type.clone(),
            entrypoint: None,
        });
    }

    // If nothing was produced (e.g., empty array), still emit one entry
    if entries.is_empty() {
        entries.push(HistoryEntry {
            uuid: uuid.to_string(),
            role: role.to_string(),
            content: String::new(),
            model: model.clone(),
            usage: usage.clone(),
            timestamp,
            entry_type: parent_type.clone(),
            entrypoint: None,
        });
    }

    entries
}

/// Classify the entry type from role/type/subtype fields.
fn classify(
    role: &str,
    line_type: &str,
    subtype: &str,
    content_val: Option<&serde_json::Value>,
) -> HistoryEntryType {
    // Compact boundary detection
    if role == "system" || line_type == "system" {
        if subtype == "compact_boundary" {
            return HistoryEntryType::CompactBoundary;
        }
        if let Some(serde_json::Value::String(text)) = content_val {
            if text.to_lowercase().contains("compact") {
                return HistoryEntryType::CompactBoundary;
            }
        }
        return HistoryEntryType::System;
    }
    if role == "user" || line_type == "user" {
        return HistoryEntryType::User;
    }
    if role == "assistant" || line_type == "assistant" {
        return HistoryEntryType::Assistant;
    }
    HistoryEntryType::Noise
}

/// Check if an entry is noise and should be filtered from display.
fn is_noise(entry: &HistoryEntry) -> bool {
    entry.entry_type == HistoryEntryType::Noise
        || (entry.content.trim().is_empty()
            && !matches!(
                entry.entry_type,
                HistoryEntryType::ToolUse | HistoryEntryType::ToolResult
            ))
}

/// Check if a raw line type/subtype is noise.
fn is_noise_type(line_type: &str, subtype: &str) -> bool {
    matches!(
        line_type,
        "queue-operation" | "file-history-snapshot" | "progress"
    ) || matches!(subtype, "queue-operation")
}

/// Create a noise entry placeholder.
fn make_noise_entry(uuid: &str, timestamp: Option<DateTime<Utc>>) -> HistoryEntry {
    HistoryEntry {
        uuid: uuid.to_string(),
        role: String::new(),
        content: String::new(),
        model: None,
        usage: None,
        timestamp,
        entry_type: HistoryEntryType::Noise,
        entrypoint: None,
    }
}

/// Parse a usage JSON value into `CcUsage`.
fn parse_usage(val: &serde_json::Value) -> Option<CcUsage> {
    let input_tokens = val.get("input_tokens").and_then(|t| t.as_u64())?;
    let output_tokens = val
        .get("output_tokens")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    Some(CcUsage {
        input_tokens,
        output_tokens,
    })
}

/// Extract CWD from system message content.
fn extract_cwd(content: &str) -> Option<String> {
    for line in content.lines() {
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

/// Resolve session title from summary file or first user message.
fn resolve_title(
    jsonl_path: &Path,
    session_id: &str,
    first_user_msg: &Option<String>,
) -> Option<String> {
    if let Some(parent) = jsonl_path.parent() {
        let summary_path = parent
            .join("session-memory")
            .join(format!("{session_id}-summary.md"));
        if let Ok(content) = std::fs::read_to_string(&summary_path) {
            if let Some(line) = content.lines().nth(3) {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    first_user_msg.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use std::fs;

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ironjaw-history-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_jsonl(dir: &Path, filename: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = dir.join(filename);
        fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    // 1. Parse user message (wrapper format)
    #[test]
    fn test_parse_user_message_wrapper_format() {
        let line = r#"{"type":"user","message":{"role":"user","content":"Build a feature"},"uuid":"abc-123","timestamp":"2026-03-15T10:00:00Z"}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry_type, HistoryEntryType::User);
        assert_eq!(entries[0].content, "Build a feature");
        assert_eq!(entries[0].uuid, "abc-123");
        assert!(entries[0].timestamp.is_some());
    }

    // 2. Parse user message (flat format)
    #[test]
    fn test_parse_user_message_flat_format() {
        let line = r#"{"role":"user","content":"Hello world","uuid":"u1"}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry_type, HistoryEntryType::User);
        assert_eq!(entries[0].content, "Hello world");
        assert_eq!(entries[0].uuid, "u1");
    }

    // 3. Parse assistant message with string content
    #[test]
    fn test_parse_assistant_string_content() {
        let line = r#"{"role":"assistant","content":"Sure!","model":"claude-3-opus","usage":{"input_tokens":500,"output_tokens":200}}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry_type, HistoryEntryType::Assistant);
        assert_eq!(entries[0].content, "Sure!");
        assert_eq!(entries[0].model, Some("claude-3-opus".to_string()));
        let usage = entries[0].usage.as_ref().unwrap();
        assert_eq!(usage.input_tokens, 500);
        assert_eq!(usage.output_tokens, 200);
    }

    // 4. Parse assistant message with array content (text + tool_use blocks)
    #[test]
    fn test_parse_assistant_array_content() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Sure!"},{"type":"tool_use","name":"Bash","input":{"command":"ls"}}],"model":"claude-opus-4-6","usage":{"input_tokens":1000,"output_tokens":500}},"uuid":"abc-456"}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].entry_type, HistoryEntryType::Assistant);
        assert_eq!(entries[0].content, "Sure!");
        assert_eq!(entries[1].entry_type, HistoryEntryType::ToolUse);
        assert!(entries[1].content.contains("Bash"));
    }

    // 5. Parse system message (regular)
    #[test]
    fn test_parse_system_message() {
        let line = r#"{"role":"system","content":"cwd: /home/user/project"}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry_type, HistoryEntryType::System);
    }

    // 6. Parse system compact_boundary (subtype)
    #[test]
    fn test_parse_compact_boundary_subtype() {
        let line =
            r#"{"role":"system","subtype":"compact_boundary","content":"compacted context"}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry_type, HistoryEntryType::CompactBoundary);
    }

    // 7. Parse compact_boundary from content keyword
    #[test]
    fn test_parse_compact_boundary_keyword() {
        let line = r#"{"role":"system","content":"Context compacted at 75%"}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry_type, HistoryEntryType::CompactBoundary);
    }

    // 8. Filter noise: queue-operation
    #[test]
    fn test_filter_noise_queue_operation() {
        let line = r#"{"type":"queue-operation","data":{}}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries[0].entry_type, HistoryEntryType::Noise);
    }

    // 9. Filter noise: file-history-snapshot
    #[test]
    fn test_filter_noise_file_history_snapshot() {
        let line = r#"{"type":"file-history-snapshot","data":{}}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries[0].entry_type, HistoryEntryType::Noise);
    }

    // 10. Filter noise: progress
    #[test]
    fn test_filter_noise_progress() {
        let line = r#"{"type":"progress","data":{"percent":50}}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries[0].entry_type, HistoryEntryType::Noise);
    }

    // 11. Filter noise: system-reminder content
    #[test]
    fn test_filter_noise_system_reminder() {
        let line =
            r#"{"role":"assistant","content":"Here is <system-reminder>hidden</system-reminder>"}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries[0].entry_type, HistoryEntryType::Noise);
    }

    // 12. Content flattening from array with multiple text blocks
    #[test]
    fn test_content_flattening_multiple_text() {
        let line = r#"{"role":"assistant","content":[{"type":"text","text":"Hello"},{"type":"text","text":"World"}]}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "Hello\nWorld");
    }

    // 13. Pagination: offset=0, limit=3 on 5 entries
    #[test]
    fn test_pagination_basic() {
        let dir = tmp_dir("pagination-basic");
        let lines: Vec<&str> = (0..5)
            .map(|_| r#"{"role":"user","content":"msg"}"#)
            .collect();
        let path = write_jsonl(&dir, "sess.jsonl", &lines);

        let (entries, total) = HistoryReader::read(&path, 0, 3, false).unwrap();
        assert_eq!(total, 5);
        assert_eq!(entries.len(), 3);

        let _ = fs::remove_dir_all(&dir);
    }

    // 14. Pagination: offset beyond total
    #[test]
    fn test_pagination_offset_beyond_total() {
        let dir = tmp_dir("pagination-beyond");
        let lines = vec![r#"{"role":"user","content":"msg"}"#; 3];
        let path = write_jsonl(&dir, "sess.jsonl", &lines);

        let (entries, total) = HistoryReader::read(&path, 100, 10, false).unwrap();
        assert_eq!(total, 3);
        assert!(entries.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    // 15. Pagination: limit=0 returns all
    #[test]
    fn test_pagination_limit_zero() {
        let dir = tmp_dir("pagination-zero");
        let lines = vec![r#"{"role":"user","content":"msg"}"#; 5];
        let path = write_jsonl(&dir, "sess.jsonl", &lines);

        let (entries, total) = HistoryReader::read(&path, 0, 0, false).unwrap();
        assert_eq!(total, 5);
        assert_eq!(entries.len(), 5);

        let _ = fs::remove_dir_all(&dir);
    }

    // 16. Metadata extraction
    #[test]
    fn test_metadata_extraction() {
        let dir = tmp_dir("metadata");
        let lines = &[
            r#"{"role":"system","content":"cwd: /home/user/project"}"#,
            r#"{"role":"user","content":"Hello world"}"#,
            r#"{"role":"assistant","content":"Hi!","model":"claude-opus-4-6","usage":{"input_tokens":1000,"output_tokens":500}}"#,
            r#"{"role":"system","subtype":"compact_boundary","content":"compacted"}"#,
        ];
        let path = write_jsonl(&dir, "test-session.jsonl", lines);

        let meta = HistoryReader::metadata(&path).unwrap();
        assert_eq!(meta.session_id, "test-session");
        assert_eq!(meta.model, Some("claude-opus-4-6".to_string()));
        assert_eq!(meta.cwd, Some("/home/user/project".to_string()));
        assert_eq!(meta.compaction_count, 1);
        assert!(meta.context_pct.is_some());
        assert_eq!(meta.message_count, 4); // all non-noise
                                           // Title should fall back to first user message
        assert_eq!(meta.title, Some("Hello world".to_string()));

        let _ = fs::remove_dir_all(&dir);
    }

    // 17. Empty file
    #[test]
    fn test_empty_file() {
        let dir = tmp_dir("empty");
        let path = write_jsonl(&dir, "empty.jsonl", &[]);

        let (entries, total) = HistoryReader::read(&path, 0, 10, false).unwrap();
        assert_eq!(total, 0);
        assert!(entries.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    // 18. File with only noise entries
    #[test]
    fn test_file_only_noise() {
        let dir = tmp_dir("only-noise");
        let lines = &[
            r#"{"type":"queue-operation","data":{}}"#,
            r#"{"type":"progress","data":{"percent":50}}"#,
            r#"{"type":"file-history-snapshot","data":{}}"#,
        ];
        let path = write_jsonl(&dir, "noise.jsonl", lines);

        let (entries, total) = HistoryReader::read(&path, 0, 10, false).unwrap();
        assert_eq!(total, 0);
        assert!(entries.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    // 19. Malformed JSON lines skipped
    #[test]
    fn test_malformed_lines_skipped() {
        let dir = tmp_dir("malformed");
        let lines = &[
            "this is not json",
            r#"{"role":"user","content":"valid"}"#,
            "{broken",
            r#"{"role":"user","content":"also valid"}"#,
        ];
        let path = write_jsonl(&dir, "mixed.jsonl", lines);

        let (entries, total) = HistoryReader::read(&path, 0, 100, false).unwrap();
        assert_eq!(total, 2);
        assert_eq!(entries[0].content, "valid");
        assert_eq!(entries[1].content, "also valid");

        let _ = fs::remove_dir_all(&dir);
    }

    // 20. UUID extraction
    #[test]
    fn test_uuid_extraction() {
        let line = r#"{"role":"user","content":"test","uuid":"my-uuid-123"}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 5);
        assert_eq!(entries[0].uuid, "my-uuid-123");
    }

    // 21. UUID fallback to line index
    #[test]
    fn test_uuid_fallback() {
        let line = r#"{"role":"user","content":"test"}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 7);
        assert_eq!(entries[0].uuid, "line-7");
    }

    // 22. Timestamp parsing
    #[test]
    fn test_timestamp_parsing() {
        let line = r#"{"role":"user","content":"test","timestamp":"2026-03-15T10:30:00Z"}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        let ts = entries[0].timestamp.unwrap();
        assert_eq!(ts.year(), 2026);
        assert_eq!(ts.month(), 3);
        assert_eq!(ts.day(), 15);
    }

    // 23. Timestamp missing is None
    #[test]
    fn test_timestamp_missing() {
        let line = r#"{"role":"user","content":"test"}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert!(entries[0].timestamp.is_none());
    }

    // 24. Context percentage calculation in metadata
    #[test]
    fn test_context_pct_in_metadata() {
        let dir = tmp_dir("ctx-pct");
        let lines = &[
            r#"{"role":"assistant","content":"a","model":"m1","usage":{"input_tokens":100000,"output_tokens":500}}"#,
        ];
        let path = write_jsonl(&dir, "pct.jsonl", lines);

        let meta = HistoryReader::metadata(&path).unwrap();
        // 100000 / 200000 * 100 = 50%
        let pct = meta.context_pct.unwrap();
        assert!((pct - 50.0).abs() < 0.01);

        let _ = fs::remove_dir_all(&dir);
    }

    // 25. Tool result content block
    #[test]
    fn test_tool_result_block() {
        let line = r#"{"role":"assistant","content":[{"type":"text","text":"Running..."},{"type":"tool_result","content":"success"}]}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].entry_type, HistoryEntryType::Assistant);
        assert_eq!(entries[0].content, "Running...");
        assert_eq!(entries[1].entry_type, HistoryEntryType::ToolResult);
        assert_eq!(entries[1].content, "success");
    }

    // 26. Noise filtering in read()
    #[test]
    fn test_read_filters_noise() {
        let dir = tmp_dir("read-noise");
        let lines = &[
            r#"{"role":"user","content":"hello"}"#,
            r#"{"type":"queue-operation","data":{}}"#,
            r#"{"role":"assistant","content":"hi"}"#,
        ];
        let path = write_jsonl(&dir, "sess.jsonl", lines);

        let (entries, total) = HistoryReader::read(&path, 0, 100, false).unwrap();
        assert_eq!(total, 2);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].entry_type, HistoryEntryType::User);
        assert_eq!(entries[1].entry_type, HistoryEntryType::Assistant);

        let _ = fs::remove_dir_all(&dir);
    }

    // 27. Metadata with title from summary file
    #[test]
    fn test_metadata_title_from_summary() {
        let dir = tmp_dir("meta-title");
        let session_dir = dir.join("session-memory");
        fs::create_dir_all(&session_dir).unwrap();
        // Summary file: line 4 (0-indexed line 3) has the title
        let summary = "# Summary\n\n## Title\nImplement the bridge client\n\nDetails...";
        fs::write(session_dir.join("my-session-summary.md"), summary).unwrap();

        let lines = &[r#"{"role":"user","content":"first message"}"#];
        let path = write_jsonl(&dir, "my-session.jsonl", lines);

        let meta = HistoryReader::metadata(&path).unwrap();
        assert_eq!(meta.title, Some("Implement the bridge client".to_string()));

        let _ = fs::remove_dir_all(&dir);
    }

    // 28. File not found error
    #[test]
    fn test_file_not_found() {
        let result = HistoryReader::read(Path::new("/nonexistent/file.jsonl"), 0, 10, false);
        assert!(result.is_err());
    }

    // 29. Subtype queue-operation noise
    #[test]
    fn test_subtype_queue_operation_noise() {
        let line = r#"{"type":"something","subtype":"queue-operation","data":{}}"#;
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        let entries = parse_line(&val, 0);
        assert_eq!(entries[0].entry_type, HistoryEntryType::Noise);
    }

    // 30. Metadata with no assistant messages (no model, no context_pct)
    #[test]
    fn test_metadata_no_assistant() {
        let dir = tmp_dir("meta-no-asst");
        let lines = &[
            r#"{"role":"system","content":"init"}"#,
            r#"{"role":"user","content":"hello"}"#,
        ];
        let path = write_jsonl(&dir, "no-asst.jsonl", lines);

        let meta = HistoryReader::metadata(&path).unwrap();
        assert!(meta.model.is_none());
        assert!(meta.context_pct.is_none());
        assert_eq!(meta.compaction_count, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    // 31. CWD extraction from extract_cwd helper
    #[test]
    fn test_extract_cwd_helper() {
        assert_eq!(
            extract_cwd("cwd: /home/user/project"),
            Some("/home/user/project".to_string())
        );
        assert_eq!(
            extract_cwd("Working directory: C:\\Users\\test"),
            Some("C:\\Users\\test".to_string())
        );
        assert!(extract_cwd("no cwd here").is_none());
    }

    // 32. Parse usage helper
    #[test]
    fn test_parse_usage() {
        let val: serde_json::Value = serde_json::json!({"input_tokens": 42, "output_tokens": 7});
        let usage = parse_usage(&val).unwrap();
        assert_eq!(usage.input_tokens, 42);
        assert_eq!(usage.output_tokens, 7);
    }

    // 33. Parse usage missing output_tokens defaults to 0
    #[test]
    fn test_parse_usage_missing_output() {
        let val: serde_json::Value = serde_json::json!({"input_tokens": 100});
        let usage = parse_usage(&val).unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 0);
    }

    // 34. Parse usage missing input_tokens returns None
    #[test]
    fn test_parse_usage_missing_input() {
        let val: serde_json::Value = serde_json::json!({"output_tokens": 100});
        assert!(parse_usage(&val).is_none());
    }

    // 35. Classify unknown role/type as Noise
    #[test]
    fn test_classify_unknown() {
        assert_eq!(classify("", "", "", None), HistoryEntryType::Noise);
    }

    // 36. read_since returns only new entries
    #[test]
    fn test_read_since_basic() {
        let dir = tmp_dir("read-since-basic");
        let lines = vec![r#"{"role":"user","content":"msg"}"#; 5];
        let path = write_jsonl(&dir, "sess.jsonl", &lines);

        let (new_entries, total) = HistoryReader::read_since(&path, 3).unwrap();
        assert_eq!(total, 5);
        assert_eq!(new_entries.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    // 37. read_since when known_count >= total returns empty
    #[test]
    fn test_read_since_no_new() {
        let dir = tmp_dir("read-since-no-new");
        let lines = vec![r#"{"role":"user","content":"msg"}"#; 3];
        let path = write_jsonl(&dir, "sess.jsonl", &lines);

        let (new_entries, total) = HistoryReader::read_since(&path, 5).unwrap();
        assert_eq!(total, 3);
        assert!(new_entries.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    // 38. read_since with known_count=0 returns all
    #[test]
    fn test_read_since_all() {
        let dir = tmp_dir("read-since-all");
        let lines = vec![r#"{"role":"user","content":"msg"}"#; 4];
        let path = write_jsonl(&dir, "sess.jsonl", &lines);

        let (new_entries, total) = HistoryReader::read_since(&path, 0).unwrap();
        assert_eq!(total, 4);
        assert_eq!(new_entries.len(), 4);

        let _ = fs::remove_dir_all(&dir);
    }

    // 39. read_since filters noise
    #[test]
    fn test_read_since_filters_noise() {
        let dir = tmp_dir("read-since-noise");
        let lines = &[
            r#"{"role":"user","content":"first"}"#,
            r#"{"type":"queue-operation","data":{}}"#,
            r#"{"role":"user","content":"second"}"#,
            r#"{"role":"user","content":"third"}"#,
        ];
        let path = write_jsonl(&dir, "sess.jsonl", lines);

        // known_count=1 means skip the first non-noise entry
        let (new_entries, total) = HistoryReader::read_since(&path, 1).unwrap();
        assert_eq!(total, 3); // 3 non-noise
        assert_eq!(new_entries.len(), 2);
        assert_eq!(new_entries[0].content, "second");
        assert_eq!(new_entries[1].content, "third");

        let _ = fs::remove_dir_all(&dir);
    }
}
