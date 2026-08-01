//! Claude Code subprocess runner.
//!
//! Spawns `claude -p` processes and streams their output as typed [`CcStreamEvent`]s
//! via a [`tokio::sync::broadcast`] channel.

use crate::types::{CcConfig, CcStreamEvent, CcUsage, ClaudeError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, error, warn};

/// Manages a single `claude -p` subprocess, broadcasting parsed stream events.
pub struct ClaudeRunner {
    /// Broadcast sender for parsed stream events.
    tx: broadcast::Sender<CcStreamEvent>,
    /// Whether the subprocess is still running.
    running: Arc<AtomicBool>,
    /// Handle to the child process for kill access.
    child: Arc<Mutex<Option<tokio::process::Child>>>,
    /// Session ID extracted from a Result event (available after completion).
    session_id: Arc<Mutex<Option<String>>>,
    /// PID of the subprocess (captured at spawn time).
    pid: Option<u32>,
}

impl ClaudeRunner {
    /// Spawn a `claude -p` subprocess.
    ///
    /// If `session_id` is `Some`, uses `--resume`. Writes `message` to stdin,
    /// closes stdin, then reads stdout line-by-line, parsing stream-json events.
    pub async fn spawn(
        config: &CcConfig,
        session_id: Option<&str>,
        message: &str,
        cwd: &str,
        effort: Option<&str>,
    ) -> Result<Self, ClaudeError> {
        let mut cmd = build_command(config, session_id, effort);
        cmd.current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;
        let pid = child.id();

        // Write user message to stdin, then drop to close it.
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(message.as_bytes()).await?;
            stdin.shutdown().await?;
            // stdin is dropped here, closing the pipe
        }

        let (tx, _rx) = broadcast::channel::<CcStreamEvent>(256);
        let running = Arc::new(AtomicBool::new(true));
        let extracted_session_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let child_handle: Arc<Mutex<Option<tokio::process::Child>>> = Arc::new(Mutex::new(None));

        // Take stdout/stderr before moving child into the Arc.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ClaudeError::Io(std::io::Error::other("failed to capture stdout")))?;
        let stderr_handle = child.stderr.take();

        // Store child for kill access.
        {
            let mut guard = child_handle.lock().await;
            *guard = Some(child);
        }

        // Spawn the reader task.
        {
            let tx = tx.clone();
            let running = running.clone();
            let session_id_slot = extracted_session_id.clone();
            let child_for_kill = child_handle.clone();
            let idle_timeout = std::time::Duration::from_millis(config.idle_timeout_ms);
            let global_timeout = std::time::Duration::from_millis(config.timeout_ms);

            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                let mut seq: u32 = 0;
                let mut has_stream_deltas = false;
                let start = tokio::time::Instant::now();

                loop {
                    let read_line = tokio::select! {
                        line = lines.next_line() => line,
                        () = tokio::time::sleep(idle_timeout) => {
                            warn!(
                                "claude subprocess idle timeout after {}ms",
                                idle_timeout.as_millis()
                            );
                            kill_child(&child_for_kill).await;
                            let _ = tx.send(CcStreamEvent::Error {
                                message: format!(
                                    "idle timeout after {}ms",
                                    idle_timeout.as_millis()
                                ),
                            });
                            break;
                        }
                        () = tokio::time::sleep(
                            global_timeout.saturating_sub(start.elapsed())
                        ) => {
                            warn!(
                                "claude subprocess global timeout after {}ms",
                                global_timeout.as_millis()
                            );
                            kill_child(&child_for_kill).await;
                            let _ = tx.send(CcStreamEvent::Error {
                                message: format!(
                                    "global timeout after {}ms",
                                    global_timeout.as_millis()
                                ),
                            });
                            break;
                        }
                    };

                    match read_line {
                        Ok(Some(line)) => {
                            if let Some(event) = parse_stream_line(&line, &mut seq) {
                                // Track whether we got incremental deltas from stream_event.
                                // If so, skip the duplicate "assistant" fallback event.
                                if matches!(&event, CcStreamEvent::Delta { .. }) {
                                    // Check if this came from stream_event (seq was incremented
                                    // by stream_event handler) vs assistant fallback.
                                    // We use a simple heuristic: if we already have stream deltas,
                                    // and the line contains "assistant", skip it.
                                    if has_stream_deltas && line.contains(r#""type":"assistant""#) {
                                        debug!("skipping duplicate assistant event (already have stream deltas)");
                                        continue;
                                    }
                                    has_stream_deltas = true;
                                }
                                // Capture session_id from Result events.
                                if let CcStreamEvent::Result { ref session_id, .. } = event {
                                    let mut slot = session_id_slot.lock().await;
                                    *slot = Some(session_id.clone());
                                }
                                let _ = tx.send(event);
                            }
                        }
                        Ok(None) => {
                            // EOF — process closed stdout.
                            debug!("claude subprocess stdout closed (EOF)");
                            break;
                        }
                        Err(e) => {
                            error!("error reading claude subprocess stdout: {e}");
                            let _ = tx.send(CcStreamEvent::Error {
                                message: format!("stdout read error: {e}"),
                            });
                            break;
                        }
                    }
                }

                // Process has exited (or been killed). Check exit status + stderr.
                let mut child_guard = child_for_kill.lock().await;
                if let Some(ref mut child) = *child_guard {
                    // Read stderr.
                    let stderr_text = if let Some(stderr) = stderr_handle {
                        let mut buf = String::new();
                        let mut reader = BufReader::new(stderr);
                        let _ =
                            tokio::io::AsyncReadExt::read_to_string(&mut reader, &mut buf).await;
                        buf
                    } else {
                        String::new()
                    };

                    match child.try_wait() {
                        Ok(Some(status)) if !status.success() => {
                            let msg = if stderr_text.is_empty() {
                                format!("subprocess exited with {status}")
                            } else {
                                format!("subprocess exited with {status}: {}", stderr_text.trim())
                            };
                            error!("{msg}");
                            let _ = tx.send(CcStreamEvent::Error { message: msg });
                        }
                        Ok(Some(_)) => {
                            debug!("claude subprocess exited successfully");
                        }
                        Ok(None) => {
                            // Still running — wait for it.
                            match child.wait().await {
                                Ok(status) if !status.success() => {
                                    let msg = if stderr_text.is_empty() {
                                        format!("subprocess exited with {status}")
                                    } else {
                                        format!(
                                            "subprocess exited with {status}: {}",
                                            stderr_text.trim()
                                        )
                                    };
                                    error!("{msg}");
                                    let _ = tx.send(CcStreamEvent::Error { message: msg });
                                }
                                Ok(_) => {
                                    debug!("claude subprocess exited successfully");
                                }
                                Err(e) => {
                                    error!("error waiting for claude subprocess: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            error!("error checking claude subprocess status: {e}");
                        }
                    }
                }
                drop(child_guard);

                running.store(false, Ordering::SeqCst);
            });
        }

        Ok(Self {
            tx,
            running,
            child: child_handle,
            session_id: extracted_session_id,
            pid,
        })
    }

    /// Get a broadcast receiver for stream events.
    pub fn subscribe(&self) -> broadcast::Receiver<CcStreamEvent> {
        self.tx.subscribe()
    }

    /// Abort (kill) the subprocess.
    pub async fn abort(&self) -> Result<(), ClaudeError> {
        kill_child(&self.child).await;
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Check if the subprocess is still running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Get the PID of the subprocess.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Get the session ID extracted from the Result event (available after
    /// completion).
    pub fn session_id(&self) -> Option<String> {
        // Use try_lock to avoid blocking — returns None if locked.
        match self.session_id.try_lock() {
            Ok(guard) => guard.clone(),
            Err(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Command construction
// ---------------------------------------------------------------------------

/// Build the `tokio::process::Command` for a claude subprocess.
fn build_command(config: &CcConfig, session_id: Option<&str>, effort: Option<&str>) -> Command {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", &config.claude_cmd]);
        c
    } else {
        Command::new(&config.claude_cmd)
    };

    cmd.arg("-p");
    cmd.arg("--verbose");
    cmd.args(["--output-format", "stream-json"]);
    cmd.arg("--include-partial-messages");
    cmd.arg("--dangerously-skip-permissions");

    if let Some(sid) = session_id {
        cmd.args(["--resume", sid]);
    }

    // Map thinking level to Claude Code --effort flag
    if let Some(level) = effort {
        let effort_level = match level {
            "low" | "medium" | "high" | "max" => level,
            _ => "medium",
        };
        cmd.args(["--effort", effort_level]);
    }

    cmd
}

// ---------------------------------------------------------------------------
// Stream-json line parser
// ---------------------------------------------------------------------------

/// Parse a single line of stream-json stdout into a [`CcStreamEvent`].
///
/// Returns `None` for lines that cannot be parsed (logged as warnings).
fn parse_stream_line(line: &str, seq: &mut u32) -> Option<CcStreamEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            warn!("skipping unparseable stream line: {e}");
            debug!("raw line: {trimmed}");
            return None;
        }
    };

    let obj = value.as_object()?;
    let event_type = obj.get("type")?.as_str()?;

    match event_type {
        // --include-partial-messages wraps API events in stream_event
        "stream_event" => {
            let inner = obj.get("event")?.as_object()?;
            let inner_type = inner.get("type")?.as_str()?;
            match inner_type {
                "content_block_delta" => {
                    let delta = inner.get("delta")?;
                    let delta_type = delta.get("type")?.as_str()?;
                    if delta_type == "text_delta" {
                        let text = delta.get("text")?.as_str()?.to_string();
                        *seq += 1;
                        Some(CcStreamEvent::Delta { text, seq: *seq })
                    } else {
                        debug!("ignoring content_block_delta type={delta_type}");
                        None
                    }
                }
                "content_block_start" => {
                    let content_block = inner.get("content_block")?;
                    let block_type = content_block.get("type")?.as_str()?;
                    if block_type == "tool_use" {
                        let name = content_block
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let input = content_block
                            .get("input")
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        Some(CcStreamEvent::ToolUse { name, input })
                    } else {
                        None
                    }
                }
                _ => {
                    debug!("ignoring stream_event inner type={inner_type}");
                    None
                }
            }
        }
        // Legacy: raw API events (without stream_event wrapper)
        "content_block_delta" => {
            let delta = obj.get("delta")?;
            let delta_type = delta.get("type")?.as_str()?;
            if delta_type == "text_delta" {
                let text = delta.get("text")?.as_str()?.to_string();
                *seq += 1;
                Some(CcStreamEvent::Delta { text, seq: *seq })
            } else {
                None
            }
        }
        "content_block_start" => {
            let content_block = obj.get("content_block")?;
            let block_type = content_block.get("type")?.as_str()?;
            if block_type == "tool_use" {
                let name = content_block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let input = content_block
                    .get("input")
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                Some(CcStreamEvent::ToolUse { name, input })
            } else {
                None
            }
        }
        "content_block_stop" => {
            let content_block = obj.get("content_block");
            let name = content_block
                .and_then(|cb| cb.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("unknown")
                .to_string();
            let output = content_block
                .and_then(|cb| cb.get("output"))
                .map(|v| {
                    if let Some(s) = v.as_str() {
                        s.to_string()
                    } else {
                        v.to_string()
                    }
                })
                .unwrap_or_default();
            Some(CcStreamEvent::ToolResult { name, output })
        }
        "result" => {
            // Check for error results first (e.g. failed --resume).
            let is_error = obj
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if is_error {
                let msg = obj
                    .get("errors")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|e| e.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .or_else(|| obj.get("error").and_then(|e| e.as_str()).map(String::from))
                    .unwrap_or_else(|| "unknown error".to_string());
                return Some(CcStreamEvent::Error { message: msg });
            }
            let session_id = obj
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let result = obj
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Claude Code uses total_cost_usd, raw API uses cost_usd
            let cost_usd = obj
                .get("total_cost_usd")
                .or(obj.get("cost_usd"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let usage = obj.get("usage").and_then(|u| {
                let input_tokens = u.get("input_tokens")?.as_u64()?;
                let output_tokens = u.get("output_tokens")?.as_u64()?;
                Some(CcUsage {
                    input_tokens,
                    output_tokens,
                })
            });
            Some(CcStreamEvent::Result {
                session_id,
                result,
                usage,
                cost_usd,
            })
        }
        // Claude Code stream-json: full assistant message (fallback)
        "assistant" => {
            let message = obj.get("message")?;
            let content = message.get("content")?;
            if let Some(arr) = content.as_array() {
                for block in arr {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            *seq += 1;
                            return Some(CcStreamEvent::Delta {
                                text: text.to_string(),
                                seq: *seq,
                            });
                        }
                    }
                }
            }
            None
        }
        "error" => {
            let message = obj
                .get("error")
                .and_then(|e| {
                    e.as_str()
                        .map(String::from)
                        .or_else(|| e.get("message").and_then(|m| m.as_str()).map(String::from))
                })
                .unwrap_or_else(|| "unknown error".to_string());
            Some(CcStreamEvent::Error { message })
        }
        // system, rate_limit_event, etc. — skip silently
        "system" | "rate_limit_event" => None,
        other => {
            debug!("ignoring unknown stream event type: {other}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Kill the child process if it exists.
async fn kill_child(child: &Arc<Mutex<Option<tokio::process::Child>>>) {
    let mut guard = child.lock().await;
    if let Some(ref mut c) = *guard {
        if let Err(e) = c.kill().await {
            warn!("failed to kill claude subprocess: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_stream_line tests --

    #[test]
    fn test_parse_delta() {
        let mut seq = 0u32;
        let line = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Delta { text, seq: s } => {
                assert_eq!(text, "Hello");
                assert_eq!(s, 1);
            }
            _ => panic!("expected Delta, got {event:?}"),
        }
        assert_eq!(seq, 1);
    }

    #[test]
    fn test_parse_delta_increments_seq() {
        let mut seq = 5u32;
        let line = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"world"}}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Delta { seq: s, .. } => assert_eq!(s, 6),
            _ => panic!("expected Delta"),
        }
        assert_eq!(seq, 6);
    }

    #[test]
    fn test_parse_delta_non_text_type_ignored() {
        let mut seq = 0u32;
        let line = r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{}"}}"#;
        assert!(parse_stream_line(line, &mut seq).is_none());
        assert_eq!(seq, 0);
    }

    #[test]
    fn test_parse_tool_use() {
        let mut seq = 0u32;
        let line = r#"{"type":"content_block_start","content_block":{"type":"tool_use","name":"Bash","input":"cargo test"}}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::ToolUse { name, input } => {
                assert_eq!(name, "Bash");
                assert_eq!(input, r#""cargo test""#);
            }
            _ => panic!("expected ToolUse, got {event:?}"),
        }
    }

    #[test]
    fn test_parse_tool_use_missing_name() {
        let mut seq = 0u32;
        let line = r#"{"type":"content_block_start","content_block":{"type":"tool_use"}}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::ToolUse { name, .. } => {
                assert_eq!(name, "unknown");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn test_parse_tool_result() {
        let mut seq = 0u32;
        let line = r#"{"type":"content_block_stop","content_block":{"name":"Bash","output":"ok"}}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::ToolResult { name, output } => {
                assert_eq!(name, "Bash");
                assert_eq!(output, "ok");
            }
            _ => panic!("expected ToolResult, got {event:?}"),
        }
    }

    #[test]
    fn test_parse_tool_result_no_content_block() {
        let mut seq = 0u32;
        let line = r#"{"type":"content_block_stop"}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::ToolResult { name, output } => {
                assert_eq!(name, "unknown");
                assert_eq!(output, "");
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn test_parse_result() {
        let mut seq = 0u32;
        let line = r#"{"type":"result","session_id":"abc-123","result":"Done","usage":{"input_tokens":1000,"output_tokens":500},"cost_usd":0.05}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Result {
                session_id,
                result,
                usage,
                cost_usd,
            } => {
                assert_eq!(session_id, "abc-123");
                assert_eq!(result, "Done");
                assert!((cost_usd - 0.05).abs() < f64::EPSILON);
                let u = usage.unwrap();
                assert_eq!(u.input_tokens, 1000);
                assert_eq!(u.output_tokens, 500);
            }
            _ => panic!("expected Result, got {event:?}"),
        }
    }

    #[test]
    fn test_parse_result_no_usage() {
        let mut seq = 0u32;
        let line = r#"{"type":"result","session_id":"s1","result":"ok","cost_usd":0.0}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Result { usage, .. } => {
                assert!(usage.is_none());
            }
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn test_parse_result_missing_fields() {
        let mut seq = 0u32;
        let line = r#"{"type":"result"}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Result {
                session_id,
                result,
                usage,
                cost_usd,
            } => {
                assert_eq!(session_id, "");
                assert_eq!(result, "");
                assert!(usage.is_none());
                assert!((cost_usd - 0.0).abs() < f64::EPSILON);
            }
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn test_parse_error() {
        let mut seq = 0u32;
        let line = r#"{"type":"error","error":"something went wrong"}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Error { message } => {
                assert_eq!(message, "something went wrong");
            }
            _ => panic!("expected Error, got {event:?}"),
        }
    }

    #[test]
    fn test_parse_error_nested_message() {
        let mut seq = 0u32;
        let line = r#"{"type":"error","error":{"message":"nested error"}}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Error { message } => {
                assert_eq!(message, "nested error");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_parse_error_no_error_field() {
        let mut seq = 0u32;
        let line = r#"{"type":"error"}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Error { message } => {
                assert_eq!(message, "unknown error");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_parse_unknown_type_returns_none() {
        let mut seq = 0u32;
        let line = r#"{"type":"message_start","message":{"id":"msg_01"}}"#;
        assert!(parse_stream_line(line, &mut seq).is_none());
    }

    #[test]
    fn test_parse_invalid_json_returns_none() {
        let mut seq = 0u32;
        assert!(parse_stream_line("not json at all", &mut seq).is_none());
    }

    #[test]
    fn test_parse_empty_line_returns_none() {
        let mut seq = 0u32;
        assert!(parse_stream_line("", &mut seq).is_none());
        assert!(parse_stream_line("   ", &mut seq).is_none());
    }

    #[test]
    fn test_parse_json_without_type_returns_none() {
        let mut seq = 0u32;
        assert!(parse_stream_line(r#"{"foo":"bar"}"#, &mut seq).is_none());
    }

    #[test]
    fn test_parse_non_object_json_returns_none() {
        let mut seq = 0u32;
        assert!(parse_stream_line(r#"[1, 2, 3]"#, &mut seq).is_none());
        assert!(parse_stream_line(r#""hello""#, &mut seq).is_none());
        assert!(parse_stream_line("42", &mut seq).is_none());
    }

    // -- stream_event wrapper tests (--include-partial-messages) --

    #[test]
    fn test_parse_stream_event_text_delta() {
        let mut seq = 0u32;
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}},"session_id":"abc"}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Delta { text, seq: s } => {
                assert_eq!(text, "Hello");
                assert_eq!(s, 1);
            }
            _ => panic!("expected Delta, got {event:?}"),
        }
    }

    #[test]
    fn test_parse_stream_event_tool_use() {
        let mut seq = 0u32;
        let line = r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"read_file","input":{}}}}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::ToolUse { name, .. } => {
                assert_eq!(name, "read_file");
            }
            _ => panic!("expected ToolUse, got {event:?}"),
        }
    }

    #[test]
    fn test_parse_stream_event_message_stop_ignored() {
        let mut seq = 0u32;
        let line = r#"{"type":"stream_event","event":{"type":"message_stop"}}"#;
        assert!(parse_stream_line(line, &mut seq).is_none());
    }

    #[test]
    fn test_parse_assistant_event() {
        let mut seq = 0u32;
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"world"}]}}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Delta { text, .. } => {
                assert_eq!(text, "world");
            }
            _ => panic!("expected Delta from assistant, got {event:?}"),
        }
    }

    #[test]
    fn test_parse_system_event_ignored() {
        let mut seq = 0u32;
        let line = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        assert!(parse_stream_line(line, &mut seq).is_none());
    }

    #[test]
    fn test_parse_result_total_cost_usd() {
        let mut seq = 0u32;
        let line = r#"{"type":"result","session_id":"s1","result":"done","total_cost_usd":0.15,"usage":{"input_tokens":100,"output_tokens":50}}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Result { cost_usd, .. } => {
                assert!((cost_usd - 0.15).abs() < f64::EPSILON);
            }
            _ => panic!("expected Result"),
        }
    }

    // -- build_command tests --

    #[test]
    fn test_build_command_new_session() {
        let config = CcConfig {
            claude_cmd: "claude".to_string(),
            ..CcConfig::default()
        };
        let cmd = build_command(&config, None, None);
        let as_std = cmd.as_std();
        let args: Vec<&std::ffi::OsStr> = as_std.get_args().collect();

        if cfg!(windows) {
            assert_eq!(as_std.get_program(), "cmd");
            assert!(args.contains(&std::ffi::OsStr::new("/C")));
            assert!(args.contains(&std::ffi::OsStr::new("claude")));
        } else {
            assert_eq!(as_std.get_program(), "claude");
        }
        assert!(args.contains(&std::ffi::OsStr::new("-p")));
        assert!(args.contains(&std::ffi::OsStr::new("--verbose")));
        assert!(args.contains(&std::ffi::OsStr::new("--output-format")));
        assert!(args.contains(&std::ffi::OsStr::new("stream-json")));
        assert!(args.contains(&std::ffi::OsStr::new("--include-partial-messages")));
        assert!(args.contains(&std::ffi::OsStr::new("--dangerously-skip-permissions")));
        assert!(!args.contains(&std::ffi::OsStr::new("--resume")));
    }

    #[test]
    fn test_build_command_resume_session() {
        let config = CcConfig {
            claude_cmd: "claude".to_string(),
            ..CcConfig::default()
        };
        let cmd = build_command(&config, Some("sess-42"), None);
        let as_std = cmd.as_std();
        let args: Vec<&std::ffi::OsStr> = as_std.get_args().collect();

        assert!(args.contains(&std::ffi::OsStr::new("--resume")));
        assert!(args.contains(&std::ffi::OsStr::new("sess-42")));
    }

    #[test]
    fn test_build_command_custom_claude_cmd() {
        let config = CcConfig {
            claude_cmd: "/usr/local/bin/claude-dev".to_string(),
            ..CcConfig::default()
        };
        let cmd = build_command(&config, None, None);
        let as_std = cmd.as_std();

        if cfg!(windows) {
            let args: Vec<&std::ffi::OsStr> = as_std.get_args().collect();
            assert!(args.contains(&std::ffi::OsStr::new("/usr/local/bin/claude-dev")));
        } else {
            assert_eq!(
                as_std.get_program(),
                std::ffi::OsStr::new("/usr/local/bin/claude-dev")
            );
        }
    }

    #[test]
    fn test_build_command_with_effort() {
        let config = CcConfig {
            claude_cmd: "claude".to_string(),
            ..CcConfig::default()
        };
        let cmd = build_command(&config, None, Some("high"));
        let as_std = cmd.as_std();
        let args: Vec<&std::ffi::OsStr> = as_std.get_args().collect();

        assert!(args.contains(&std::ffi::OsStr::new("--effort")));
        assert!(args.contains(&std::ffi::OsStr::new("high")));
    }

    // -- broadcast channel tests --

    #[tokio::test]
    async fn test_broadcast_channel_delivers_events() {
        let (tx, mut rx) = broadcast::channel::<CcStreamEvent>(256);
        tx.send(CcStreamEvent::Delta {
            text: "hi".to_string(),
            seq: 1,
        })
        .unwrap();
        tx.send(CcStreamEvent::Error {
            message: "done".to_string(),
        })
        .unwrap();

        let e1 = rx.recv().await.unwrap();
        match e1 {
            CcStreamEvent::Delta { text, seq } => {
                assert_eq!(text, "hi");
                assert_eq!(seq, 1);
            }
            _ => panic!("expected Delta"),
        }

        let e2 = rx.recv().await.unwrap();
        match e2 {
            CcStreamEvent::Error { message } => assert_eq!(message, "done"),
            _ => panic!("expected Error"),
        }
    }

    #[tokio::test]
    async fn test_broadcast_multiple_subscribers() {
        let (tx, mut rx1) = broadcast::channel::<CcStreamEvent>(256);
        let mut rx2 = tx.subscribe();

        tx.send(CcStreamEvent::Delta {
            text: "msg".to_string(),
            seq: 1,
        })
        .unwrap();

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();

        match (e1, e2) {
            (
                CcStreamEvent::Delta {
                    text: t1, seq: s1, ..
                },
                CcStreamEvent::Delta {
                    text: t2, seq: s2, ..
                },
            ) => {
                assert_eq!(t1, "msg");
                assert_eq!(t2, "msg");
                assert_eq!(s1, 1);
                assert_eq!(s2, 1);
            }
            _ => panic!("expected Delta from both subscribers"),
        }
    }

    // -- ClaudeRunner field access tests --

    #[test]
    fn test_running_flag() {
        let flag = Arc::new(AtomicBool::new(true));
        assert!(flag.load(Ordering::SeqCst));
        flag.store(false, Ordering::SeqCst);
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_session_id_slot() {
        let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        assert!(slot.lock().await.is_none());

        {
            let mut guard = slot.lock().await;
            *guard = Some("abc-123".to_string());
        }

        assert_eq!(*slot.lock().await, Some("abc-123".to_string()));
    }

    // -- Sequence of parsed events test --

    #[test]
    fn test_parse_full_stream_sequence() {
        let mut seq = 0u32;

        let lines = [
            r#"{"type":"message_start","message":{"id":"msg_01"}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":" world"}}"#,
            r#"{"type":"content_block_start","content_block":{"type":"tool_use","name":"Read"}}"#,
            r#"{"type":"content_block_stop","content_block":{"name":"Read","output":"file contents"}}"#,
            r#"{"type":"result","session_id":"s42","result":"All done","usage":{"input_tokens":100,"output_tokens":50},"cost_usd":0.01}"#,
        ];

        let events: Vec<CcStreamEvent> = lines
            .iter()
            .filter_map(|line| parse_stream_line(line, &mut seq))
            .collect();

        assert_eq!(events.len(), 5); // message_start is skipped
        assert!(matches!(
            &events[0],
            CcStreamEvent::Delta { text, seq: 1 } if text == "Hello"
        ));
        assert!(matches!(
            &events[1],
            CcStreamEvent::Delta { text, seq: 2 } if text == " world"
        ));
        assert!(matches!(
            &events[2],
            CcStreamEvent::ToolUse { name, .. } if name == "Read"
        ));
        assert!(matches!(
            &events[3],
            CcStreamEvent::ToolResult { name, output }
                if name == "Read" && output == "file contents"
        ));
        assert!(matches!(
            &events[4],
            CcStreamEvent::Result { session_id, .. } if session_id == "s42"
        ));
        assert_eq!(seq, 2); // only deltas increment seq
    }

    // -- Whitespace / edge-case tolerance --

    #[test]
    fn test_parse_result_error() {
        let mut seq = 0u32;
        let line = r#"{"type":"result","is_error":true,"errors":["No conversation found with id abc-123"],"session_id":"","result":""}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Error { message } => {
                assert_eq!(message, "No conversation found with id abc-123");
            }
            _ => panic!("expected Error for is_error result, got {event:?}"),
        }
    }

    #[test]
    fn test_parse_result_error_multiple_errors() {
        let mut seq = 0u32;
        let line = r#"{"type":"result","is_error":true,"errors":["error one","error two"]}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Error { message } => {
                assert_eq!(message, "error one; error two");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_parse_result_error_no_errors_field() {
        let mut seq = 0u32;
        let line = r#"{"type":"result","is_error":true,"error":"fallback error"}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Error { message } => {
                assert_eq!(message, "fallback error");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_parse_result_error_no_message_at_all() {
        let mut seq = 0u32;
        let line = r#"{"type":"result","is_error":true}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Error { message } => {
                assert_eq!(message, "unknown error");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_parse_result_is_error_false_still_parses_as_result() {
        let mut seq = 0u32;
        let line =
            r#"{"type":"result","is_error":false,"session_id":"s1","result":"ok","cost_usd":0.0}"#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Result { session_id, .. } => {
                assert_eq!(session_id, "s1");
            }
            _ => panic!("expected Result when is_error=false, got {event:?}"),
        }
    }

    #[test]
    fn test_parse_line_with_leading_whitespace() {
        let mut seq = 0u32;
        let line =
            r#"  {"type":"content_block_delta","delta":{"type":"text_delta","text":"trimmed"}}  "#;
        let event = parse_stream_line(line, &mut seq).unwrap();
        match event {
            CcStreamEvent::Delta { text, .. } => {
                assert_eq!(text, "trimmed")
            }
            _ => panic!("expected Delta"),
        }
    }
}
