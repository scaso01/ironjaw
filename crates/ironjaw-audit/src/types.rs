use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Summary of a single tool call within an agentic loop iteration, recorded in the audit log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditToolCall {
    /// Tool name that was invoked.
    pub tool_name: String,
    /// The tool-call ID returned by the LLM (links back to the assistant message).
    pub tool_call_id: String,
    /// Whether the tool executed successfully.
    pub ok: bool,
    /// Duration of the tool call in milliseconds.
    pub duration_ms: u64,
    /// Brief error string if `ok == false`.
    pub error: Option<String>,
}

/// An entry to be written to the audit log
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub action_type: ActionType,
    pub method: Option<String>,
    pub tool_name: Option<String>,
    pub params: Option<serde_json::Value>,
    pub result: Option<serde_json::Value>,
    pub ok: bool,
    pub error: Option<String>,
    pub llm_model: Option<String>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub cost_usd: Option<f64>,
    pub latency_ms: Option<i64>,
    pub session_id: Option<String>,
    pub device_id: Option<String>,
    pub request_id: Uuid,
    /// Agentic loop: which iteration produced this LLM call (0 = first pass).
    /// `None` for non-agentic or when the feature flag is disabled.
    pub iteration: Option<usize>,
    /// Agentic loop: tool calls executed during this iteration, if any.
    pub tool_calls: Option<Vec<AuditToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionType {
    Auth,
    Request,
    ToolCall,
    LlmCall,
    Event,
    Error,
}

impl std::fmt::Display for ActionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionType::Auth => write!(f, "auth"),
            ActionType::Request => write!(f, "request"),
            ActionType::ToolCall => write!(f, "tool_call"),
            ActionType::LlmCall => write!(f, "llm_call"),
            ActionType::Event => write!(f, "event"),
            ActionType::Error => write!(f, "error"),
        }
    }
}

/// A stored audit record (includes DB-generated fields)
#[derive(Debug, Clone)]
pub struct AuditRecord {
    pub id: i64,
    pub timestamp: String,
    pub action_type: String,
    pub method: Option<String>,
    pub tool_name: Option<String>,
    pub params: Option<String>,
    pub result: Option<String>,
    pub ok: bool,
    pub error: Option<String>,
    pub llm_model: Option<String>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub cost_usd: Option<f64>,
    pub latency_ms: Option<i64>,
    pub session_id: Option<String>,
    pub device_id: Option<String>,
    pub request_id: String,
    pub entry_hash: Option<String>,
    pub prev_hash: Option<String>,
}

/// Result of verifying the Merkle hash chain
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainVerification {
    /// Whether the entire chain is valid
    pub valid: bool,
    /// Number of entries checked
    pub entries_checked: usize,
    /// ID of the first entry where the chain broke (if any)
    pub first_broken_at: Option<i64>,
}
