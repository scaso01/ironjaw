use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// Tool-result role — used in agentic loop to return tool outputs to the LLM.
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Optional base64-encoded images for multimodal requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    /// Tool calls emitted by an assistant turn (agentic loop — serialised to wire).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Links a `Role::Tool` result message back to the originating call ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

impl Default for ChatMessage {
    fn default() -> Self {
        Self {
            role: Role::User,
            content: String::new(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

impl ChatMessage {
    /// Convenience constructor: user message with no optional fields.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// Convenience constructor: assistant message, optionally carrying tool calls.
    pub fn assistant(content: impl Into<String>, tool_calls: Option<Vec<ToolCall>>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            images: None,
            tool_calls,
            tool_call_id: None,
        }
    }

    /// Convenience constructor: tool-result message.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            images: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// Configuration for model thinking/reasoning mode.
///
/// Controls whether the LLM uses chain-of-thought reasoning and how much
/// token budget to allocate. Mapped from `thinkingLevel` in chat.send params.
#[derive(Debug, Clone, Default)]
pub struct ThinkingConfig {
    pub enabled: bool,
    /// Token budget for reasoning. -1 = unlimited, 0 = disabled, N = budget.
    pub budget_tokens: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
    /// Thinking/reasoning toggle. Not serialized — consumed by providers.
    #[serde(skip)]
    pub thinking: Option<ThinkingConfig>,
    /// Tools advertised to the LLM (Phase 1: field exists, not yet wired).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    /// Tool choice hint (Phase 1: field exists, not yet wired).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChunk {
    pub delta: String,
    pub done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub error: Option<String>,
    /// Streaming tool call deltas (Phase 1: field exists, not yet populated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_deltas: Option<Vec<ToolCallDelta>>,
    /// Finish reason from the LLM (Phase 1: field exists, not yet populated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<String>,
}

/// Progress events emitted during model auto-loading (llama-server only).
/// Sent via an mpsc channel so the handler can forward them as WebSocket events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum ModelLoadEvent {
    /// Model switch initiated
    #[serde(rename = "switching")]
    Switching { from: Option<String>, to: String },
    /// Model is loading (periodic progress update)
    #[serde(rename = "loading")]
    Loading { model: String, elapsed_secs: u32 },
    /// Model loaded and ready for inference
    #[serde(rename = "ready")]
    Ready { model: String, elapsed_secs: u32 },
    /// Model load failed
    #[serde(rename = "failed")]
    Failed { model: String, error: String },
}

// ---------------------------------------------------------------------------
// Tool types (Phase 1 — agentic foundation)
// ---------------------------------------------------------------------------

/// OpenAI-compat tool definition advertised to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input.
    pub parameters: serde_json::Value,
    /// Permission tier (Auto, Notify, Confirm, MultiConfirm). Serialized as string.
    /// NOTE: use a String here (not PermissionTier enum) to avoid a circular dep with
    /// ironjaw-security. Phase N will add a From<PermissionTier> conversion.
    pub tier: String,
}

/// OpenAI-compat tool_choice hint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ToolChoice {
    /// "auto" | "none" | "required"
    Mode(String),
    /// {"type":"function","function":{"name":"..."}}
    Named {
        #[serde(rename = "type")]
        type_: String,
        function: NamedFunction,
    },
}

impl ToolChoice {
    /// Convenience constructor: `"auto"` — let the LLM decide whether to call a tool.
    pub fn auto() -> Self {
        ToolChoice::Mode("auto".to_string())
    }

    /// Convenience constructor: `"none"` — the LLM must not call any tool.
    pub fn none() -> Self {
        ToolChoice::Mode("none".to_string())
    }

    /// Convenience constructor: `"required"` — the LLM must call at least one tool.
    pub fn required() -> Self {
        ToolChoice::Mode("required".to_string())
    }
}

impl ToolDefinition {
    /// Serialize this definition into the OpenAI wire format expected by
    /// `/v1/chat/completions`: `{"type":"function","function":{...}}`.
    pub fn to_openai_wire(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NamedFunction {
    pub name: String,
}

/// Completed tool call returned by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_function_type")]
    pub type_: String,
    pub function: ToolCallFunction,
}

fn default_function_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallFunction {
    pub name: String,
    /// JSON string (may be fragmented during streaming).
    pub arguments: String,
}

/// Streaming delta of a tool_call (by index).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub function: Option<ToolCallFunctionDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallFunctionDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("provider unreachable: {0}")]
    Unreachable(String),
    #[error("model not found: {0}")]
    ModelNotFound(String),
    #[error("request failed: {0}")]
    RequestFailed(String),
    #[error("stream error: {0}")]
    StreamError(String),
    #[error("no providers configured")]
    NoProviders,
    #[error("all providers failed")]
    AllProvidersFailed,
    #[error("rate limited: retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    #[error("server error: {0}")]
    ServerError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_serialization() {
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"assistant\""
        );
    }

    #[test]
    fn test_chat_message_roundtrip() {
        let msg = ChatMessage {
            role: Role::User,
            content: "Hello".to_string(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, Role::User);
        assert_eq!(parsed.content, "Hello");
    }

    #[test]
    fn test_model_info_skip_serializing_if() {
        let info = ModelInfo {
            id: "qwen3-coder".to_string(),
            provider: "llama-server".to_string(),
            size: None,
            modified_at: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("size"));
        assert!(!json.contains("modified_at"));
    }

    #[test]
    fn test_usage_serialization() {
        let usage = Usage {
            prompt_tokens: 10,
            completion_tokens: 20,
            total_tokens: 30,
        };
        let json = serde_json::to_string(&usage).unwrap();
        let parsed: Usage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.prompt_tokens, 10);
        assert_eq!(parsed.completion_tokens, 20);
        assert_eq!(parsed.total_tokens, 30);
    }

    #[test]
    fn test_model_load_event_switching_serialization() {
        let event = ModelLoadEvent::Switching {
            from: Some("qwen3-coder".to_string()),
            to: "deepseek-r1:32b".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"state\":\"switching\""));
        assert!(json.contains("\"from\":\"qwen3-coder\""));
        assert!(json.contains("\"to\":\"deepseek-r1:32b\""));
    }

    #[test]
    fn test_model_load_event_loading_serialization() {
        let event = ModelLoadEvent::Loading {
            model: "deepseek-r1:32b".to_string(),
            elapsed_secs: 15,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"state\":\"loading\""));
        assert!(json.contains("\"elapsed_secs\":15"));
    }

    #[test]
    fn test_model_load_event_ready_serialization() {
        let event = ModelLoadEvent::Ready {
            model: "deepseek-r1:32b".to_string(),
            elapsed_secs: 30,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"state\":\"ready\""));
        assert!(json.contains("\"elapsed_secs\":30"));
    }

    #[test]
    fn test_model_load_event_failed_serialization() {
        let event = ModelLoadEvent::Failed {
            model: "bad-model".to_string(),
            error: "Model not found".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"state\":\"failed\""));
        assert!(json.contains("\"error\":\"Model not found\""));
    }

    #[test]
    fn test_model_load_event_switching_from_none() {
        let event = ModelLoadEvent::Switching {
            from: None,
            to: "qwen3-coder".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"from\":null"));
    }

    #[test]
    fn test_chat_chunk_error_serialization() {
        let chunk = ChatChunk {
            delta: String::new(),
            done: true,
            usage: None,
            model: "test".to_string(),
            error: Some("Anthropic API error (HTTP 401)".to_string()),
            tool_call_deltas: None,
            finish_reason: None,
        };
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains("\"error\":\"Anthropic API error (HTTP 401)\""));
    }

    #[test]
    fn test_chat_chunk_no_error_omitted() {
        let chunk = ChatChunk {
            delta: "hello".to_string(),
            done: false,
            usage: None,
            model: "test".to_string(),
            error: None,
            tool_call_deltas: None,
            finish_reason: None,
        };
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(!json.contains("error"));
    }

    #[test]
    fn test_thinking_config_default() {
        let config = ThinkingConfig::default();
        assert!(!config.enabled);
        assert!(config.budget_tokens.is_none());
    }

    #[test]
    fn test_thinking_config_enabled_with_budget() {
        let config = ThinkingConfig {
            enabled: true,
            budget_tokens: Some(1000),
        };
        assert!(config.enabled);
        assert_eq!(config.budget_tokens, Some(1000));
    }

    #[test]
    fn test_thinking_config_unlimited() {
        let config = ThinkingConfig {
            enabled: true,
            budget_tokens: Some(-1),
        };
        assert!(config.enabled);
        assert_eq!(config.budget_tokens, Some(-1));
    }

    #[test]
    fn test_chat_request_thinking_not_serialized() {
        let request = ChatRequest {
            model: "test".to_string(),
            messages: vec![],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking: Some(ThinkingConfig {
                enabled: true,
                budget_tokens: Some(1000),
            }),
            tools: None,
            tool_choice: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        // thinking field has #[serde(skip)] so it must not appear in JSON
        assert!(!json.contains("thinking"));
        assert!(!json.contains("budget_tokens"));
    }

    #[test]
    fn test_chat_request_thinking_level_mapping() {
        // Test the mapping logic that will be used in handler.rs
        let levels = vec![
            ("off", false, Some(0)),
            ("low", true, Some(200)),
            ("medium", true, Some(1000)),
            ("high", true, Some(-1)),
        ];
        for (level, expected_enabled, expected_budget) in levels {
            let config = match level {
                "off" => ThinkingConfig {
                    enabled: false,
                    budget_tokens: Some(0),
                },
                "low" => ThinkingConfig {
                    enabled: true,
                    budget_tokens: Some(200),
                },
                "medium" => ThinkingConfig {
                    enabled: true,
                    budget_tokens: Some(1000),
                },
                "high" => ThinkingConfig {
                    enabled: true,
                    budget_tokens: Some(-1),
                },
                _ => ThinkingConfig::default(),
            };
            assert_eq!(config.enabled, expected_enabled, "level={level}");
            assert_eq!(config.budget_tokens, expected_budget, "level={level}");
        }
    }

    // -------------------------------------------------------------------------
    // Phase 1 tool type round-trip tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_tool_definition_roundtrip() {
        let def = ToolDefinition {
            name: "shell".to_string(),
            description: "Run a shell command".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"}
                },
                "required": ["command"]
            }),
            tier: "Confirm".to_string(),
        };
        let json = serde_json::to_string(&def).unwrap();
        let parsed: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, def);
        assert_eq!(parsed.name, "shell");
        assert_eq!(parsed.tier, "Confirm");
    }

    #[test]
    fn test_tool_choice_mode_auto_roundtrip() {
        let choice = ToolChoice::Mode("auto".to_string());
        let json = serde_json::to_string(&choice).unwrap();
        assert_eq!(json, "\"auto\"");
        let parsed: ToolChoice = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ToolChoice::Mode("auto".to_string()));
    }

    #[test]
    fn test_tool_choice_mode_none_roundtrip() {
        let choice = ToolChoice::Mode("none".to_string());
        let json = serde_json::to_string(&choice).unwrap();
        assert_eq!(json, "\"none\"");
        let parsed: ToolChoice = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ToolChoice::Mode("none".to_string()));
    }

    #[test]
    fn test_tool_choice_named_roundtrip() {
        let choice = ToolChoice::Named {
            type_: "function".to_string(),
            function: NamedFunction {
                name: "shell".to_string(),
            },
        };
        let json = serde_json::to_string(&choice).unwrap();
        assert!(json.contains("\"type\":\"function\""));
        assert!(json.contains("\"name\":\"shell\""));
        let parsed: ToolChoice = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, choice);
    }

    #[test]
    fn test_tool_call_roundtrip() {
        let call = ToolCall {
            id: "call_abc123".to_string(),
            type_: "function".to_string(),
            function: ToolCallFunction {
                name: "read_file".to_string(),
                arguments: "{\"path\":\"/etc/hosts\"}".to_string(),
            },
        };
        let json = serde_json::to_string(&call).unwrap();
        // The field is serialized as "type" not "type_"
        assert!(json.contains("\"type\":\"function\""));
        assert!(json.contains("\"id\":\"call_abc123\""));
        let parsed: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, call);
    }

    #[test]
    fn test_tool_call_default_type_on_deserialize() {
        // When "type" is absent from JSON, default_function_type() must fill it
        let json = r#"{"id":"call_xyz","function":{"name":"shell","arguments":"{}"}}"#;
        let parsed: ToolCall = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.type_, "function");
        assert_eq!(parsed.id, "call_xyz");
    }

    #[test]
    fn test_tool_call_delta_roundtrip() {
        let delta = ToolCallDelta {
            index: 0,
            id: Some("call_abc".to_string()),
            type_: Some("function".to_string()),
            function: Some(ToolCallFunctionDelta {
                name: Some("shell".to_string()),
                arguments: Some("{\"cmd\":".to_string()),
            }),
        };
        let json = serde_json::to_string(&delta).unwrap();
        let parsed: ToolCallDelta = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, delta);
        assert_eq!(parsed.index, 0);
    }

    #[test]
    fn test_tool_call_delta_partial_roundtrip() {
        // Mid-stream delta: only arguments fragment, no id/name
        let delta = ToolCallDelta {
            index: 0,
            id: None,
            type_: None,
            function: Some(ToolCallFunctionDelta {
                name: None,
                arguments: Some("\"ls\"}".to_string()),
            }),
        };
        let json = serde_json::to_string(&delta).unwrap();
        let parsed: ToolCallDelta = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, delta);
        assert!(parsed.id.is_none());
    }

    #[test]
    fn test_chat_request_with_tools_roundtrip() {
        let request = ChatRequest {
            model: "qwen3-coder".to_string(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: "list files".to_string(),
                images: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            temperature: Some(0.7),
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: Some(vec![ToolDefinition {
                name: "list_files".to_string(),
                description: "List directory contents".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                tier: "Auto".to_string(),
            }]),
            tool_choice: Some(ToolChoice::Mode("auto".to_string())),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"tools\""));
        assert!(json.contains("\"tool_choice\""));
        assert!(json.contains("list_files"));
        let parsed: ChatRequest = serde_json::from_str(&json).unwrap();
        assert!(parsed.tools.is_some());
        assert_eq!(parsed.tools.as_ref().unwrap().len(), 1);
        assert_eq!(parsed.tools.as_ref().unwrap()[0].name, "list_files");
    }

    #[test]
    fn test_chat_request_without_tools_omits_fields() {
        let request = ChatRequest {
            model: "test".to_string(),
            messages: vec![],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("tools"));
        assert!(!json.contains("tool_choice"));
    }

    #[test]
    fn test_chat_chunk_with_tool_call_deltas_roundtrip() {
        let chunk = ChatChunk {
            delta: String::new(),
            done: false,
            usage: None,
            model: "qwen3-coder".to_string(),
            error: None,
            tool_call_deltas: Some(vec![ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                type_: Some("function".to_string()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("shell".to_string()),
                    arguments: None,
                }),
            }]),
            finish_reason: Some("tool_calls".to_string()),
        };
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains("tool_call_deltas"));
        assert!(json.contains("finish_reason"));
        let parsed: ChatChunk = serde_json::from_str(&json).unwrap();
        assert!(parsed.tool_call_deltas.is_some());
        assert_eq!(parsed.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn test_chat_chunk_without_tool_deltas_omits_fields() {
        let chunk = ChatChunk {
            delta: "hello".to_string(),
            done: false,
            usage: None,
            model: "test".to_string(),
            error: None,
            tool_call_deltas: None,
            finish_reason: None,
        };
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(!json.contains("tool_call_deltas"));
        assert!(!json.contains("finish_reason"));
    }
}
