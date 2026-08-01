use ironjaw_brain::Brain;
use ironjaw_security::PermissionTier;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A hint from a tool about a live-state fact it just produced.
///
/// When a tool execution succeeds, the agentic loop inspects `memory_hints()`
/// and writes a `ToolVerified` memory to the brain for each hint.  The memory
/// is tagged with `entity` and `key` so it can be found and superseded by the
/// next invocation of the same tool.
///
/// `volatility` controls the `valid_to` field: the memory is considered fresh
/// for `now + volatility`.  After that the live-state freshness filter in
/// Phase 10 will drop it from auto-inject.
#[derive(Debug, Clone)]
pub struct MemoryHint {
    /// The entity this fact describes (e.g. `"workstation"`, `"docker-host"`, `"system"`).
    pub entity: String,
    /// The attribute key (e.g. `"services"`, `"containers"`, `"processes"`).
    pub key: String,
    /// How long this fact stays fresh before the filter should drop it.
    pub volatility: Duration,
    /// Human-readable content to store in the memory.  When `None` the agentic
    /// loop uses a compact serialisation of the tool output.
    pub content: Option<String>,
}

/// Async tool trait — implemented by all tools (built-in and future plugins).
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Unique tool name (e.g., "shell", "read_file")
    fn name(&self) -> &str;
    /// Human-readable description
    fn description(&self) -> &str;
    /// JSON Schema for the tool's parameters
    fn schema(&self) -> serde_json::Value;
    /// Permission tier required to execute this tool
    fn tier(&self) -> PermissionTier;
    /// Execute the tool with the given parameters and context.
    ///
    /// `cancel` is a [`CancellationToken`] the caller can fire to request
    /// cooperative cancellation.  Long-running tools should poll
    /// `cancel.cancelled()` inside `tokio::select!` and return
    /// [`ToolResult::cancelled`] when the token fires.  Pure-compute tools
    /// may ignore the token.
    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult;

    /// Declare live-state facts that this tool produces on a successful run.
    ///
    /// The default implementation returns an empty list — tools that track
    /// rapidly-changing infrastructure state should override this to opt in to
    /// tool-memory writeback (Phase 11).
    ///
    /// `input` is the raw params passed to `execute`; `output` is the
    /// serialised `ToolResult`.  Implementations may use either to populate
    /// `MemoryHint::content`.
    fn memory_hints(
        &self,
        _input: &serde_json::Value,
        _output: &serde_json::Value,
    ) -> Vec<MemoryHint> {
        vec![]
    }

    /// Convert this tool to an OpenAI-compat `ToolDefinition` for LLM advertisement.
    /// Default implementation builds the definition from the other trait methods.
    fn to_definition(&self) -> ironjaw_llm::types::ToolDefinition {
        ironjaw_llm::types::ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.schema(),
            tier: format!("{:?}", self.tier()),
        }
    }
}

/// Per-execution context passed to every tool invocation.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub connection_id: String,
    pub working_dir: PathBuf,
    pub brain: Option<Arc<Brain>>,
    pub stream_tx: Option<mpsc::UnboundedSender<String>>,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            connection_id: String::new(),
            working_dir: PathBuf::new(),
            brain: None,
            stream_tx: None,
        }
    }
}

impl ToolContext {
    pub fn new(
        session_id: impl Into<String>,
        connection_id: impl Into<String>,
        working_dir: PathBuf,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            connection_id: connection_id.into(),
            working_dir,
            brain: None,
            stream_tx: None,
        }
    }

    pub fn with_brain(mut self, brain: Arc<Brain>) -> Self {
        self.brain = Some(brain);
        self
    }

    pub fn with_stream_tx(mut self, tx: mpsc::UnboundedSender<String>) -> Self {
        self.stream_tx = Some(tx);
        self
    }
}

/// Result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResult {
    Text {
        content: String,
    },
    Json {
        content: serde_json::Value,
    },
    Error {
        code: String,
        message: String,
    },
    /// The tool was cooperatively cancelled before it could produce a result.
    Cancelled {
        reason: String,
    },
}

impl ToolResult {
    pub fn text(content: impl Into<String>) -> Self {
        ToolResult::Text {
            content: content.into(),
        }
    }

    pub fn json(content: serde_json::Value) -> Self {
        ToolResult::Json { content }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        ToolResult::Error {
            code: code.into(),
            message: message.into(),
        }
    }

    /// Construct a cancellation result with the given human-readable reason.
    pub fn cancelled(reason: impl Into<String>) -> Self {
        ToolResult::Cancelled {
            reason: reason.into(),
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, ToolResult::Error { .. })
    }

    /// Returns `true` when the tool was cancelled before producing a result.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, ToolResult::Cancelled { .. })
    }
}

/// Tool metadata returned by tools.list
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
    pub tier: u8,
}

/// Lightweight tool summary (name + description only, no schema).
/// Used for LLM context efficiency — full schemas loaded on-demand via get_schema().
#[derive(Debug, Clone, Serialize)]
pub struct ToolSummary {
    pub name: String,
    pub description: String,
    pub tier: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_result_text() {
        let r = ToolResult::text("hello");
        assert!(!r.is_error());
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn test_tool_result_json() {
        let r = ToolResult::json(serde_json::json!({"key": "value"}));
        assert!(!r.is_error());
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["type"], "json");
        assert_eq!(json["content"]["key"], "value");
    }

    #[test]
    fn test_tool_result_error() {
        let r = ToolResult::error("NOT_FOUND", "File not found");
        assert!(r.is_error());
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["code"], "NOT_FOUND");
    }

    #[test]
    fn test_tool_result_roundtrip() {
        let r = ToolResult::text("roundtrip test");
        let json_str = serde_json::to_string(&r).unwrap();
        let parsed: ToolResult = serde_json::from_str(&json_str).unwrap();
        assert!(!parsed.is_error());
    }
}
