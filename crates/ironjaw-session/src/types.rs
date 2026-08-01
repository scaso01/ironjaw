use ironjaw_llm::types::ToolCall;
use serde::{Deserialize, Serialize};

/// Controls HITL behavior for a session.
/// - `Default`: existing per-tool tier logic applies
/// - `Plan`: all tools require approval regardless of tier
/// - `AutoAccept`: all tools bypass HITL regardless of tier
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    #[default]
    Default,
    Plan,
    AutoAccept,
}

/// A persistent session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub context_tokens: i64,
    pub compaction_count: i64,
    /// Cumulative LLM usage across all completed turns (persisted per turn,
    /// unlike context_tokens which is live occupancy of the last call).
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    /// Populated by queries that join message count
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_count: Option<i64>,
    /// Whether the session is pinned to the top
    #[serde(default)]
    pub pinned: bool,
    /// Whether the session is archived
    #[serde(default)]
    pub archived: bool,
    /// HITL override mode for this session
    #[serde(default)]
    pub mode: SessionMode,
    /// C1: Per-session working directory override. When `Some`, tool-context
    /// resolution prefers this over the `[tools]` config default. Stored as a
    /// raw string in SQLite and parsed to `PathBuf` at the call site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

/// Message role enum — maps to/from the string stored in the DB `role` column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
}

impl MessageRole {
    /// Parse from the raw role string stored in the database.
    pub fn parse_role(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "tool" => Some(Self::Tool),
            "system" => Some(Self::System),
            _ => None,
        }
    }

    /// Serialize back to the raw role string stored in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::System => "system",
        }
    }
}

/// A stored conversation message
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMessage {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<i64>,
    pub complete: bool,
    pub created_at: String,
    /// Tool calls produced by an assistant turn (serialized as JSON in the DB).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Tool call ID for a tool-role message (links result back to assistant's call).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Input for creating a new message
pub struct NewMessage {
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub model: Option<String>,
    pub tokens: Option<i64>,
    pub complete: bool,
    /// Tool calls produced by an assistant turn (serialized as JSON when persisted).
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Tool call ID for a tool-role message.
    pub tool_call_id: Option<String>,
}

impl NewMessage {
    pub fn new(
        session_id: impl Into<String>,
        role: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            role: role.into(),
            content: content.into(),
            model: None,
            tokens: None,
            complete: true,
            tool_calls: None,
            tool_call_id: None,
        }
    }
}
