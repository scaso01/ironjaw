use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Subprocess stream events (parsed from `claude --output-format stream-json`)
// ---------------------------------------------------------------------------

/// Events parsed from the `claude -p --output-format stream-json` stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum CcStreamEvent {
    /// Incremental text token from the assistant.
    #[serde(rename = "delta")]
    Delta { text: String, seq: u32 },
    /// A tool invocation started by Claude Code.
    #[serde(rename = "tool_use")]
    ToolUse { name: String, input: String },
    /// Result of a tool invocation.
    #[serde(rename = "tool_result")]
    ToolResult { name: String, output: String },
    /// Final result of the subprocess run.
    #[serde(rename = "final")]
    Result {
        session_id: String,
        result: String,
        usage: Option<CcUsage>,
        cost_usd: f64,
    },
    /// Error from the subprocess.
    #[serde(rename = "error")]
    Error { message: String },
}

/// Token usage from a Claude Code subprocess result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CcUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

// ---------------------------------------------------------------------------
// JSONL history entries
// ---------------------------------------------------------------------------

/// A parsed entry from a Claude Code session JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub uuid: String,
    pub role: String,
    pub content: String,
    pub model: Option<String>,
    pub usage: Option<CcUsage>,
    pub timestamp: Option<DateTime<Utc>>,
    pub entry_type: HistoryEntryType,
    /// Source of this entry: "cli" (terminal), "sdk-cli" (Dr. CLAW phone), etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
}

/// Classification of a JSONL history entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryEntryType {
    User,
    Assistant,
    System,
    ToolUse,
    ToolResult,
    CompactBoundary,
    /// Filtered from display (queue-operation, progress, snapshots, etc.)
    Noise,
}

// ---------------------------------------------------------------------------
// Active subprocess tracking
// ---------------------------------------------------------------------------

/// Metadata about a currently-running Claude Code subprocess.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveSession {
    pub session_id: String,
    pub cwd: String,
    pub started_at: DateTime<Utc>,
    pub pid: u32,
}

// ---------------------------------------------------------------------------
// Unified session list entry
// ---------------------------------------------------------------------------

/// Unified entry for session listings (merges historical + active).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListEntry {
    pub session_id: String,
    pub title: Option<String>,
    pub project: Option<String>,
    pub cwd: Option<String>,
    /// "running" or "historical"
    pub status: String,
    pub context_pct: Option<f32>,
    pub compaction_count: u32,
    pub model: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub machine: String,
}

// ---------------------------------------------------------------------------
// Runner config (from [claude] TOML section)
// ---------------------------------------------------------------------------

/// Configuration for the Claude Code subprocess runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CcConfig {
    /// Whether CC session management is enabled.
    #[serde(default = "default_cc_enabled")]
    pub enabled: bool,
    /// Path to the claude CLI executable (e.g. claude.cmd on Windows).
    #[serde(default = "default_claude_cmd")]
    pub claude_cmd: String,
    /// Default working directory for new sessions.
    #[serde(default = "default_cwd")]
    pub default_cwd: String,
    /// Path to ~/.claude/projects/ for session scanning.
    #[serde(default = "default_sessions_dir")]
    pub sessions_dir: String,
    /// Global subprocess timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Idle timeout (no stdout) in milliseconds.
    #[serde(default = "default_idle_timeout_ms")]
    pub idle_timeout_ms: u64,
    /// Maximum concurrent CC subprocesses.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
}

fn default_cc_enabled() -> bool {
    true
}
fn default_claude_cmd() -> String {
    if cfg!(windows) {
        r"C:\Users\deploy\AppData\Roaming\npm\claude.cmd".to_string()
    } else {
        "claude".to_string()
    }
}
fn default_cwd() -> String {
    if cfg!(windows) {
        r"C:\Users\deploy\Projects".to_string()
    } else {
        "/home/deploy/Projects".to_string()
    }
}
fn default_sessions_dir() -> String {
    if cfg!(windows) {
        let home = std::env::var("USERPROFILE").unwrap_or_else(|_| r"C:\Users\deploy".to_string());
        format!(r"{home}\.claude\projects")
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/deploy".to_string());
        format!("{home}/.claude/projects")
    }
}
fn default_timeout_ms() -> u64 {
    120_000
}
fn default_idle_timeout_ms() -> u64 {
    120_000
}
fn default_max_concurrent() -> u32 {
    3
}

impl Default for CcConfig {
    fn default() -> Self {
        Self {
            enabled: default_cc_enabled(),
            claude_cmd: default_claude_cmd(),
            default_cwd: default_cwd(),
            sessions_dir: default_sessions_dir(),
            timeout_ms: default_timeout_ms(),
            idle_timeout_ms: default_idle_timeout_ms(),
            max_concurrent: default_max_concurrent(),
        }
    }
}

// ---------------------------------------------------------------------------
// Session metadata (from HistoryReader)
// ---------------------------------------------------------------------------

/// Metadata extracted from a JSONL session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub session_id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub context_pct: Option<f32>,
    pub compaction_count: u32,
    pub cwd: Option<String>,
    pub message_count: usize,
}

// ---------------------------------------------------------------------------
// Legacy types (kept for backward compatibility)
// ---------------------------------------------------------------------------

/// A frame sent/received over the cc-bridge WS protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CcBridgeFrame {
    #[serde(rename = "req")]
    Request {
        id: String,
        method: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    #[serde(rename = "res")]
    Response {
        id: String,
        #[serde(default)]
        ok: bool,
        #[serde(default)]
        data: serde_json::Value,
        #[serde(default)]
        error: Option<String>,
    },
    #[serde(rename = "event")]
    Event {
        event: String,
        #[serde(default, rename = "sessionId")]
        session_id: Option<String>,
        #[serde(default)]
        data: serde_json::Value,
    },
}

/// Events emitted by the bridge for subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeEvent {
    pub event: String,
    pub session_id: Option<String>,
    pub data: serde_json::Value,
    pub timestamp: DateTime<Utc>,
}

/// A historical session found by scanning ~/.claude/projects/.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoricalSession {
    pub session_id: String,
    pub project: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub context_pct: Option<f32>,
    pub compaction_count: u32,
    pub last_activity: Option<DateTime<Utc>>,
    pub first_message: Option<String>,
    /// Machine that scanned this session (e.g. "workstation", "docker-host").
    #[serde(default = "default_machine")]
    pub machine: String,
}

fn default_machine() -> String {
    "workstation".to_string()
}

// ---------------------------------------------------------------------------
// Phone exchange marker (written to ~/.claude/phone-pending/)
// ---------------------------------------------------------------------------

/// A single exchange marker written when the phone sends a message via cc.chat.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneExchangeMarker {
    pub timestamp: DateTime<Utc>,
    pub session_id: String,
    pub user_message: String,
    pub assistant_summary: String,
    pub source: String,
}

/// Error types for the claude crate.
#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    #[error("bridge not connected")]
    NotConnected,
    #[error("bridge authentication failed: {0}")]
    AuthFailed(String),
    #[error("request timed out after {0}s")]
    Timeout(u64),
    #[error("cc-bridge error: {0}")]
    BridgeError(String),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("websocket error: {0}")]
    WebSocket(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_frame_serialization() {
        let frame = CcBridgeFrame::Request {
            id: "abc-123".to_string(),
            method: "session.list".to_string(),
            params: serde_json::json!({"limit": 10}),
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""type":"req""#));
        assert!(json.contains(r#""method":"session.list""#));
        assert!(json.contains(r#""id":"abc-123""#));
    }

    #[test]
    fn test_request_frame_deserialization() {
        let json = r#"{"type":"req","id":"id1","method":"ping","params":{}}"#;
        let frame: CcBridgeFrame = serde_json::from_str(json).unwrap();
        match frame {
            CcBridgeFrame::Request { id, method, params } => {
                assert_eq!(id, "id1");
                assert_eq!(method, "ping");
                assert!(params.is_object());
            }
            _ => panic!("expected Request frame"),
        }
    }

    #[test]
    fn test_response_frame_ok() {
        let json = r#"{"type":"res","id":"r1","ok":true,"data":{"count":5}}"#;
        let frame: CcBridgeFrame = serde_json::from_str(json).unwrap();
        match frame {
            CcBridgeFrame::Response {
                id,
                ok,
                data,
                error,
            } => {
                assert_eq!(id, "r1");
                assert!(ok);
                assert_eq!(data["count"], 5);
                assert!(error.is_none());
            }
            _ => panic!("expected Response frame"),
        }
    }

    #[test]
    fn test_response_frame_error() {
        let json = r#"{"type":"res","id":"r2","ok":false,"error":"not found"}"#;
        let frame: CcBridgeFrame = serde_json::from_str(json).unwrap();
        match frame {
            CcBridgeFrame::Response { ok, error, .. } => {
                assert!(!ok);
                assert_eq!(error, Some("not found".to_string()));
            }
            _ => panic!("expected Response frame"),
        }
    }

    #[test]
    fn test_event_frame() {
        let json =
            r#"{"type":"event","event":"session.init","sessionId":"s1","data":{"status":"ok"}}"#;
        let frame: CcBridgeFrame = serde_json::from_str(json).unwrap();
        match frame {
            CcBridgeFrame::Event {
                event,
                session_id,
                data,
            } => {
                assert_eq!(event, "session.init");
                assert_eq!(session_id, Some("s1".to_string()));
                assert_eq!(data["status"], "ok");
            }
            _ => panic!("expected Event frame"),
        }
    }

    #[test]
    fn test_event_frame_no_session_id() {
        let json = r#"{"type":"event","event":"auth.challenge","data":{"challenge":"abc"}}"#;
        let frame: CcBridgeFrame = serde_json::from_str(json).unwrap();
        match frame {
            CcBridgeFrame::Event { session_id, .. } => {
                assert!(session_id.is_none());
            }
            _ => panic!("expected Event frame"),
        }
    }

    #[test]
    fn test_request_default_params() {
        let json = r#"{"type":"req","id":"id1","method":"ping"}"#;
        let frame: CcBridgeFrame = serde_json::from_str(json).unwrap();
        match frame {
            CcBridgeFrame::Request { params, .. } => {
                assert!(params.is_null());
            }
            _ => panic!("expected Request frame"),
        }
    }

    #[test]
    fn test_response_defaults() {
        let json = r#"{"type":"res","id":"r1"}"#;
        let frame: CcBridgeFrame = serde_json::from_str(json).unwrap();
        match frame {
            CcBridgeFrame::Response {
                ok, data, error, ..
            } => {
                assert!(!ok);
                assert!(data.is_null());
                assert!(error.is_none());
            }
            _ => panic!("expected Response frame"),
        }
    }

    #[test]
    fn test_bridge_event_roundtrip() {
        let event = BridgeEvent {
            event: "session.init".to_string(),
            session_id: Some("sess-1".to_string()),
            data: serde_json::json!({"model": "claude-3-opus"}),
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: BridgeEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.event, "session.init");
        assert_eq!(deserialized.session_id, Some("sess-1".to_string()));
    }

    #[test]
    fn test_historical_session_roundtrip() {
        let session = HistoricalSession {
            session_id: "abc-123".to_string(),
            project: "ironjaw".to_string(),
            cwd: Some("/home/user/projects/ironjaw".to_string()),
            title: Some("Implement bridge client".to_string()),
            model: Some("claude-3-opus".to_string()),
            context_pct: Some(45.2),
            compaction_count: 3,
            last_activity: Some(Utc::now()),
            first_message: Some("Build the bridge".to_string()),
            machine: "workstation".to_string(),
        };
        let json = serde_json::to_string(&session).unwrap();
        let deserialized: HistoricalSession = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.session_id, "abc-123");
        assert_eq!(deserialized.compaction_count, 3);
        assert_eq!(deserialized.context_pct, Some(45.2));
    }

    #[test]
    fn test_claude_error_display() {
        let err = ClaudeError::NotConnected;
        assert_eq!(err.to_string(), "bridge not connected");

        let err = ClaudeError::Timeout(30);
        assert_eq!(err.to_string(), "request timed out after 30s");

        let err = ClaudeError::AuthFailed("bad token".to_string());
        assert_eq!(err.to_string(), "bridge authentication failed: bad token");

        let err = ClaudeError::SessionNotFound("xyz".to_string());
        assert_eq!(err.to_string(), "session not found: xyz");
    }

    #[test]
    fn test_claude_error_from_serde() {
        let result: Result<CcBridgeFrame, _> = serde_json::from_str("invalid json");
        let serde_err = result.unwrap_err();
        let claude_err: ClaudeError = serde_err.into();
        assert!(matches!(claude_err, ClaudeError::Serialization(_)));
    }

    #[test]
    fn test_claude_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let claude_err: ClaudeError = io_err.into();
        assert!(matches!(claude_err, ClaudeError::Io(_)));
        assert!(claude_err.to_string().contains("file not found"));
    }

    // -- New types tests --

    #[test]
    fn test_cc_stream_event_delta_roundtrip() {
        let event = CcStreamEvent::Delta {
            text: "Hello".to_string(),
            seq: 1,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""state":"delta""#));
        assert!(json.contains(r#""text":"Hello""#));
        let deserialized: CcStreamEvent = serde_json::from_str(&json).unwrap();
        match deserialized {
            CcStreamEvent::Delta { text, seq } => {
                assert_eq!(text, "Hello");
                assert_eq!(seq, 1);
            }
            _ => panic!("expected Delta"),
        }
    }

    #[test]
    fn test_cc_stream_event_tool_use() {
        let event = CcStreamEvent::ToolUse {
            name: "Bash".to_string(),
            input: "cargo test".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""state":"tool_use""#));
        let deserialized: CcStreamEvent = serde_json::from_str(&json).unwrap();
        match deserialized {
            CcStreamEvent::ToolUse { name, input } => {
                assert_eq!(name, "Bash");
                assert_eq!(input, "cargo test");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn test_cc_stream_event_result() {
        let event = CcStreamEvent::Result {
            session_id: "abc-123".to_string(),
            result: "Done".to_string(),
            usage: Some(CcUsage {
                input_tokens: 1000,
                output_tokens: 500,
            }),
            cost_usd: 0.05,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""state":"final""#));
        let deserialized: CcStreamEvent = serde_json::from_str(&json).unwrap();
        match deserialized {
            CcStreamEvent::Result {
                session_id,
                cost_usd,
                usage,
                ..
            } => {
                assert_eq!(session_id, "abc-123");
                assert!((cost_usd - 0.05).abs() < f64::EPSILON);
                assert_eq!(usage.unwrap().input_tokens, 1000);
            }
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn test_cc_stream_event_error() {
        let event = CcStreamEvent::Error {
            message: "process crashed".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""state":"error""#));
    }

    #[test]
    fn test_cc_usage_roundtrip() {
        let usage = CcUsage {
            input_tokens: 42000,
            output_tokens: 8000,
        };
        let json = serde_json::to_string(&usage).unwrap();
        let d: CcUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(d.input_tokens, 42000);
        assert_eq!(d.output_tokens, 8000);
    }

    #[test]
    fn test_history_entry_roundtrip() {
        let entry = HistoryEntry {
            uuid: "u1".to_string(),
            role: "user".to_string(),
            content: "Build a feature".to_string(),
            model: None,
            usage: None,
            timestamp: Some(Utc::now()),
            entry_type: HistoryEntryType::User,
            entrypoint: Some("cli".to_string()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let d: HistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(d.entrypoint, Some("cli".to_string()));
        assert_eq!(d.uuid, "u1");
        assert_eq!(d.role, "user");
        assert_eq!(d.entry_type, HistoryEntryType::User);
    }

    #[test]
    fn test_history_entry_type_equality() {
        assert_eq!(HistoryEntryType::User, HistoryEntryType::User);
        assert_ne!(HistoryEntryType::User, HistoryEntryType::Assistant);
        assert_eq!(HistoryEntryType::Noise, HistoryEntryType::Noise);
    }

    #[test]
    fn test_active_session_roundtrip() {
        let session = ActiveSession {
            session_id: "s1".to_string(),
            cwd: "/home/user".to_string(),
            started_at: Utc::now(),
            pid: 12345,
        };
        let json = serde_json::to_string(&session).unwrap();
        let d: ActiveSession = serde_json::from_str(&json).unwrap();
        assert_eq!(d.session_id, "s1");
        assert_eq!(d.pid, 12345);
    }

    #[test]
    fn test_session_list_entry_roundtrip() {
        let entry = SessionListEntry {
            session_id: "s1".to_string(),
            title: Some("My session".to_string()),
            project: Some("ironjaw".to_string()),
            cwd: Some("/projects/ironjaw".to_string()),
            status: "running".to_string(),
            context_pct: Some(45.2),
            compaction_count: 3,
            model: Some("claude-opus-4-6".to_string()),
            updated_at: Some(Utc::now()),
            machine: "workstation".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let d: SessionListEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(d.session_id, "s1");
        assert_eq!(d.status, "running");
        assert_eq!(d.machine, "workstation");
        assert_eq!(d.compaction_count, 3);
    }

    #[test]
    fn test_cc_config_default() {
        let config = CcConfig::default();
        assert!(config.enabled);
        assert_eq!(config.timeout_ms, 120_000);
        assert_eq!(config.idle_timeout_ms, 120_000);
        assert_eq!(config.max_concurrent, 3);
        assert!(!config.claude_cmd.is_empty());
    }

    #[test]
    fn test_cc_config_deserialization() {
        let toml = r#"
            enabled = true
            claude_cmd = "/usr/bin/claude"
            default_cwd = "/tmp"
            sessions_dir = "/home/user/.claude/projects"
            timeout_ms = 60000
            idle_timeout_ms = 30000
            max_concurrent = 5
        "#;
        let config: CcConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.claude_cmd, "/usr/bin/claude");
        assert_eq!(config.timeout_ms, 60000);
        assert_eq!(config.max_concurrent, 5);
    }

    #[test]
    fn test_cc_config_partial_deserialization() {
        let toml = r#"
            enabled = false
        "#;
        let config: CcConfig = toml::from_str(toml).unwrap();
        assert!(!config.enabled);
        // Defaults filled in
        assert_eq!(config.timeout_ms, 120_000);
        assert_eq!(config.max_concurrent, 3);
    }

    #[test]
    fn test_historical_session_serializes_camel_case() {
        let session = HistoricalSession {
            session_id: "test-123".to_string(),
            project: "myproject".to_string(),
            cwd: Some("/home/user/projects".to_string()),
            title: Some("Test session".to_string()),
            model: Some("claude-3-opus".to_string()),
            context_pct: Some(42.5),
            compaction_count: 2,
            last_activity: Some(
                chrono::NaiveDate::from_ymd_opt(2026, 3, 24)
                    .unwrap()
                    .and_hms_opt(12, 0, 0)
                    .unwrap()
                    .and_utc(),
            ),
            first_message: Some("Hello world".to_string()),
            machine: "workstation".to_string(),
        };

        let value = serde_json::to_value(&session).unwrap();
        let obj = value.as_object().unwrap();

        // Verify camelCase keys are present
        assert!(obj.contains_key("sessionId"), "expected sessionId key");
        assert!(obj.contains_key("contextPct"), "expected contextPct key");
        assert!(
            obj.contains_key("compactionCount"),
            "expected compactionCount key"
        );
        assert!(
            obj.contains_key("lastActivity"),
            "expected lastActivity key"
        );
        assert!(
            obj.contains_key("firstMessage"),
            "expected firstMessage key"
        );

        // Verify snake_case keys are NOT present
        assert!(
            !obj.contains_key("session_id"),
            "should not have session_id"
        );
        assert!(
            !obj.contains_key("context_pct"),
            "should not have context_pct"
        );
        assert!(
            !obj.contains_key("compaction_count"),
            "should not have compaction_count"
        );
        assert!(
            !obj.contains_key("last_activity"),
            "should not have last_activity"
        );
        assert!(
            !obj.contains_key("first_message"),
            "should not have first_message"
        );

        // Verify single-word fields are present (same in both cases)
        assert!(obj.contains_key("project"), "expected project key");
        assert!(obj.contains_key("cwd"), "expected cwd key");
        assert!(obj.contains_key("title"), "expected title key");
        assert!(obj.contains_key("model"), "expected model key");
        assert!(obj.contains_key("machine"), "expected machine key");

        // Verify values
        assert_eq!(obj["sessionId"], "test-123");
        assert_eq!(obj["project"], "myproject");
        assert_eq!(obj["compactionCount"], 2);
    }

    #[test]
    fn test_historical_session_deserializes_camel_case() {
        let json = r#"{
            "sessionId": "deser-456",
            "project": "ironjaw",
            "cwd": "/home/user/ironjaw",
            "title": "Deserialization test",
            "model": "claude-3-sonnet",
            "contextPct": 75.5,
            "compactionCount": 5,
            "lastActivity": "2026-03-24T10:30:00Z",
            "firstMessage": "Build the thing",
            "machine": "docker-host"
        }"#;

        let session: HistoricalSession = serde_json::from_str(json).unwrap();
        assert_eq!(session.session_id, "deser-456");
        assert_eq!(session.project, "ironjaw");
        assert_eq!(session.cwd, Some("/home/user/ironjaw".to_string()));
        assert_eq!(session.title, Some("Deserialization test".to_string()));
        assert_eq!(session.model, Some("claude-3-sonnet".to_string()));
        assert_eq!(session.context_pct, Some(75.5));
        assert_eq!(session.compaction_count, 5);
        assert!(session.last_activity.is_some());
        assert_eq!(session.first_message, Some("Build the thing".to_string()));
        assert_eq!(session.machine, "docker-host");
    }

    #[test]
    fn test_historical_session_round_trip_equality() {
        let original = HistoricalSession {
            session_id: "rt-789".to_string(),
            project: "test-project".to_string(),
            cwd: Some("/tmp/test".to_string()),
            title: Some("Round trip".to_string()),
            model: Some("claude-3-opus".to_string()),
            context_pct: Some(33.3),
            compaction_count: 1,
            last_activity: Some(
                chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
                    .unwrap()
                    .and_hms_opt(8, 30, 0)
                    .unwrap()
                    .and_utc(),
            ),
            first_message: Some("Start".to_string()),
            machine: "workstation".to_string(),
        };

        let json = serde_json::to_string(&original).unwrap();
        let deserialized: HistoricalSession = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_historical_session_default_machine() {
        // JSON without a "machine" field should default to "workstation"
        let json = r#"{
            "sessionId": "no-machine",
            "project": "test",
            "cwd": null,
            "title": null,
            "model": null,
            "contextPct": null,
            "compactionCount": 0,
            "lastActivity": null,
            "firstMessage": null
        }"#;

        let session: HistoricalSession = serde_json::from_str(json).unwrap();
        assert_eq!(session.machine, "workstation");
        assert_eq!(session.session_id, "no-machine");
    }

    #[test]
    fn test_historical_session_optional_fields_null() {
        let session = HistoricalSession {
            session_id: "null-test".to_string(),
            project: "test".to_string(),
            cwd: None,
            title: None,
            model: None,
            context_pct: None,
            compaction_count: 0,
            last_activity: None,
            first_message: None,
            machine: "workstation".to_string(),
        };

        let value = serde_json::to_value(&session).unwrap();
        let obj = value.as_object().unwrap();

        // Optional fields should be present as null, not omitted
        assert!(obj.contains_key("cwd"), "cwd should be present");
        assert!(obj["cwd"].is_null(), "cwd should be null");

        assert!(obj.contains_key("title"), "title should be present");
        assert!(obj["title"].is_null(), "title should be null");

        assert!(obj.contains_key("model"), "model should be present");
        assert!(obj["model"].is_null(), "model should be null");

        assert!(
            obj.contains_key("contextPct"),
            "contextPct should be present"
        );
        assert!(obj["contextPct"].is_null(), "contextPct should be null");

        assert!(
            obj.contains_key("lastActivity"),
            "lastActivity should be present"
        );
        assert!(obj["lastActivity"].is_null(), "lastActivity should be null");

        assert!(
            obj.contains_key("firstMessage"),
            "firstMessage should be present"
        );
        assert!(obj["firstMessage"].is_null(), "firstMessage should be null");

        // Non-optional fields should have values
        assert_eq!(obj["sessionId"], "null-test");
        assert_eq!(obj["project"], "test");
        assert_eq!(obj["compactionCount"], 0);
        assert_eq!(obj["machine"], "workstation");
    }

    #[test]
    fn test_session_metadata_roundtrip() {
        let meta = SessionMetadata {
            session_id: "s1".to_string(),
            title: Some("Test".to_string()),
            model: Some("opus".to_string()),
            context_pct: Some(50.0),
            compaction_count: 2,
            cwd: Some("/tmp".to_string()),
            message_count: 42,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let d: SessionMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(d.message_count, 42);
    }
}
