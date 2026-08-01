use crate::types::{ChatChunk, ChatRequest, LlmError, ModelInfo, Role, Usage};
use reqwest::Client;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, warn};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Beta header value for OAuth tokens (required for Claude Max subscriptions).
const OAUTH_BETA: &str =
    "claude-code-20250219,oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14,interleaved-thinking-2025-05-14";
/// Beta header value for API keys.
const APIKEY_BETA: &str = "fine-grained-tool-streaming-2025-05-14,interleaved-thinking-2025-05-14";
/// Mandatory system prompt prefix for OAuth mode.
const OAUTH_SYSTEM_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Known Claude models returned by `models()`.
const KNOWN_MODELS: &[&str] = &[
    "claude-sonnet-4-6",
    "claude-sonnet-4-5-20250620",
    "claude-haiku-4-5-20250620",
    "claude-opus-4-6",
];

/// How the Anthropic provider obtains its API key at runtime.
#[derive(Debug, Clone)]
pub enum ApiKeySource {
    /// Read OAuth token from Claude Code credentials file on every request.
    OAuthFile {
        credentials_path: std::path::PathBuf,
    },
    /// Static API key or pre-resolved token.
    Static { key: String },
}

pub struct AnthropicProvider {
    client: Client,
    base_url: String,
    key_source: ApiKeySource,
    provider_name: String,
}

impl AnthropicProvider {
    pub fn new(
        api_key: impl Into<String>,
        provider_name: impl Into<String>,
        timeout_secs: u64,
        base_url: Option<String>,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .tcp_nodelay(true)
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            base_url: base_url
                .unwrap_or_else(|| ANTHROPIC_API_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            key_source: ApiKeySource::Static {
                key: api_key.into(),
            },
            provider_name: provider_name.into(),
        }
    }

    /// Create a provider using OAuth credentials file for dynamic token resolution.
    pub fn new_oauth(
        credentials_path: std::path::PathBuf,
        provider_name: impl Into<String>,
        timeout_secs: u64,
        base_url: Option<String>,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            base_url: base_url
                .unwrap_or_else(|| ANTHROPIC_API_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            key_source: ApiKeySource::OAuthFile { credentials_path },
            provider_name: provider_name.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.provider_name
    }

    /// Resolve the API key dynamically based on the source.
    fn resolve_key(&self) -> Result<String, LlmError> {
        match &self.key_source {
            ApiKeySource::OAuthFile { credentials_path } => {
                let data = std::fs::read_to_string(credentials_path).map_err(|e| {
                    LlmError::AuthFailed(format!(
                        "Failed to read OAuth credentials at {}: {}",
                        credentials_path.display(),
                        e
                    ))
                })?;
                let json: serde_json::Value = serde_json::from_str(&data).map_err(|e| {
                    LlmError::AuthFailed(format!("Failed to parse OAuth credentials: {e}"))
                })?;
                json.get("claudeAiOauth")
                    .and_then(|o| o.get("accessToken"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| {
                        LlmError::AuthFailed(
                            "OAuth credentials missing claudeAiOauth.accessToken".to_string(),
                        )
                    })
            }
            ApiKeySource::Static { key } => Ok(key.clone()),
        }
    }

    /// Returns true when using an OAuth token (Claude Max / Claude Code).
    fn is_oauth(&self) -> bool {
        match &self.key_source {
            ApiKeySource::OAuthFile { .. } => true,
            ApiKeySource::Static { key } => key.contains("sk-ant-oat"),
        }
    }

    /// Apply auth + identity headers based on key format.
    /// OAuth tokens (sk-ant-oat*) need Bearer auth + beta headers + identity.
    /// API keys (sk-ant-api03-*) use x-api-key + minimal beta headers.
    fn apply_auth(&self, req: reqwest::RequestBuilder, key: &str) -> reqwest::RequestBuilder {
        if self.is_oauth() {
            req.header("authorization", format!("Bearer {key}"))
                .header("anthropic-beta", OAUTH_BETA)
                .header("anthropic-dangerous-direct-browser-access", "true")
                .header("user-agent", "claude-cli/2.1.62")
                .header("x-app", "cli")
        } else {
            req.header("x-api-key", key)
                .header("anthropic-beta", APIKEY_BETA)
        }
    }

    pub async fn health(&self) -> Result<bool, LlmError> {
        let api_key = self.resolve_key()?;
        // Anthropic has no lightweight health endpoint, so we send a minimal
        // request to /v1/messages and check that we get a valid HTTP response
        // (even a 400 means the service is reachable and authenticated).
        let url = format!("{}/v1/messages", self.base_url);
        let short_client = Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| LlmError::Unreachable(e.to_string()))?;

        let mut body = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}],
        });
        if self.is_oauth() {
            body["system"] = serde_json::json!([{
                "type": "text",
                "text": OAUTH_SYSTEM_PREFIX,
            }]);
        }

        let req = short_client
            .post(&url)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body);
        match self.apply_auth(req, &api_key).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                // 200 = success, 401 = bad key (still reachable), 429 = rate limited
                // Consider anything except network errors as "reachable"
                Ok(status != 403 && status < 500)
            }
            Err(e) => {
                warn!(error = %e, provider = %self.provider_name, "Anthropic health check failed");
                Ok(false)
            }
        }
    }

    pub async fn models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        // Anthropic doesn't expose a public models list endpoint the same way
        // as OpenAI. Return well-known Claude models.
        Ok(KNOWN_MODELS
            .iter()
            .map(|id| ModelInfo {
                id: id.to_string(),
                provider: self.provider_name.clone(),
                size: None,
                modified_at: None,
            })
            .collect())
    }

    pub fn chat(&self, request: ChatRequest) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        let api_key = self.resolve_key()?;
        let url = format!("{}/v1/messages", self.base_url);
        let model = request.model.clone();

        // Build the Anthropic Messages API request body.
        // System messages must be extracted and sent as the top-level "system" param.
        let (system_text, messages) = build_anthropic_request(&request);

        let max_tokens = request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

        let mut body = serde_json::json!({
            "model": &request.model,
            "max_tokens": max_tokens,
            "messages": messages,
            "stream": true,
        });

        // OAuth mode requires a mandatory system prompt prefix as the first
        // content block.  The API rejects a single concatenated string — the
        // prefix MUST be a separate block in an array.
        if self.is_oauth() {
            let mut blocks = vec![serde_json::json!({
                "type": "text",
                "text": OAUTH_SYSTEM_PREFIX,
            })];
            if !system_text.is_empty() {
                blocks.push(serde_json::json!({
                    "type": "text",
                    "text": system_text,
                }));
            }
            body["system"] = serde_json::json!(blocks);
        } else if !system_text.is_empty() {
            body["system"] = serde_json::json!(system_text);
        }

        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }

        // Extended thinking support for Claude models.
        // When enabled, sets the "thinking" parameter with a budget_tokens value.
        // When disabled (or None), Claude defaults to no thinking.
        if let Some(ref thinking) = request.thinking {
            if thinking.enabled {
                let budget = thinking.budget_tokens.unwrap_or(4096);
                let effective_budget = if budget == -1 { 16384 } else { budget };
                body["thinking"] = serde_json::json!({
                    "type": "enabled",
                    "budget_tokens": effective_budget,
                });
            }
            // "off" = just don't include the thinking field; Claude defaults to no thinking
        }

        debug!(body = %body, "Anthropic chat request body");

        let req = self.apply_auth(
            self.client
                .post(&url)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .json(&body),
            &api_key,
        );

        let (tx, rx) = mpsc::channel(64);

        tokio::spawn(async move {
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    error!(error = %e, "Anthropic chat request failed");
                    let _ = tx
                        .send(ChatChunk {
                            delta: String::new(),
                            done: true,
                            usage: None,
                            model: model.clone(),
                            error: Some(format!("Anthropic connection error: {e}")),
                            tool_call_deltas: None,
                            finish_reason: None,
                        })
                        .await;
                    return;
                }
            };

            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let body_text = resp.text().await.unwrap_or_default();
                error!(status, body = %body_text, "Anthropic returned error status");
                let _ = tx
                    .send(ChatChunk {
                        delta: String::new(),
                        done: true,
                        usage: None,
                        model: model.clone(),
                        error: Some(format!("Anthropic API error (HTTP {status}): {body_text}")),
                        tool_call_deltas: None,
                        finish_reason: None,
                    })
                    .await;
                return;
            }

            // Parse the Anthropic SSE stream.
            //
            // Anthropic SSE events look like:
            //   event: message_start
            //   data: {"type":"message_start","message":{"usage":{"input_tokens":25,...}}}
            //
            //   event: content_block_delta
            //   data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}
            //
            //   event: message_delta
            //   data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}
            //
            //   event: message_stop
            //   data: {"type":"message_stop"}

            let mut stream = resp.bytes_stream();
            let mut buffer = String::new();
            let mut input_tokens: u32 = 0;
            let mut output_tokens: u32 = 0;
            let mut current_event = String::new();

            use futures_util::StreamExt;
            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        error!(error = %e, "Error reading Anthropic stream");
                        break;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&bytes));

                while let Some(newline_pos) = buffer.find('\n') {
                    let line = buffer[..newline_pos].trim().to_string();
                    buffer = buffer[newline_pos + 1..].to_string();

                    if line.is_empty() {
                        continue;
                    }

                    // Track the event type
                    if let Some(event_type) = line.strip_prefix("event: ") {
                        current_event = event_type.trim().to_string();
                        continue;
                    } else if line.starts_with("event:") {
                        if let Some(event_type) = line.strip_prefix("event:") {
                            current_event = event_type.trim().to_string();
                        }
                        continue;
                    }

                    // Strip SSE data: prefix
                    let data = if let Some(stripped) = line.strip_prefix("data: ") {
                        stripped.trim()
                    } else if let Some(stripped) = line.strip_prefix("data:") {
                        stripped.trim()
                    } else {
                        continue;
                    };

                    let parsed: serde_json::Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(e) => {
                            debug!(error = %e, data = %data, "Failed to parse Anthropic SSE data");
                            continue;
                        }
                    };

                    let event_type = parsed["type"]
                        .as_str()
                        .unwrap_or(&current_event)
                        .to_string();

                    match event_type.as_str() {
                        "message_start" => {
                            debug!("Anthropic SSE: message_start");
                            // Extract input token count from message_start
                            if let Some(usage) = parsed.get("message").and_then(|m| m.get("usage"))
                            {
                                input_tokens = usage["input_tokens"].as_u64().unwrap_or(0) as u32;
                            }
                        }
                        "content_block_delta" => {
                            let delta_type = parsed["delta"]["type"].as_str().unwrap_or("");
                            let text = if delta_type == "text_delta" {
                                parsed["delta"]["text"].as_str().unwrap_or("").to_string()
                            } else if delta_type == "thinking_delta" {
                                // Extended thinking support
                                parsed["delta"]["thinking"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string()
                            } else {
                                debug!(delta_type, "Anthropic SSE: unknown delta type");
                                String::new()
                            };

                            if !text.is_empty() {
                                debug!(len = text.len(), "Anthropic SSE: content_block_delta text");
                                let chat_chunk = ChatChunk {
                                    delta: text,
                                    done: false,
                                    usage: None,
                                    model: model.clone(),
                                    error: None,
                                    tool_call_deltas: None,
                                    finish_reason: None,
                                };
                                if tx.send(chat_chunk).await.is_err() {
                                    debug!("Receiver dropped, stopping Anthropic stream");
                                    return;
                                }
                            }
                        }
                        "message_delta" => {
                            debug!(output_tokens, "Anthropic SSE: message_delta");
                            // Extract output tokens and stop reason
                            if let Some(usage) = parsed.get("usage") {
                                output_tokens = usage["output_tokens"].as_u64().unwrap_or(0) as u32;
                            }
                        }
                        "message_stop" => {
                            debug!(input_tokens, output_tokens, "Anthropic SSE: message_stop");
                            let usage = Some(Usage {
                                prompt_tokens: input_tokens,
                                completion_tokens: output_tokens,
                                total_tokens: input_tokens + output_tokens,
                            });
                            let _ = tx
                                .send(ChatChunk {
                                    delta: String::new(),
                                    done: true,
                                    usage,
                                    model: model.clone(),
                                    error: None,
                                    tool_call_deltas: None,
                                    finish_reason: None,
                                })
                                .await;
                            return;
                        }
                        "error" => {
                            let error_msg = parsed["error"]["message"]
                                .as_str()
                                .unwrap_or("unknown error");
                            error!(error = %error_msg, "Anthropic stream error event");
                            let _ = tx
                                .send(ChatChunk {
                                    delta: String::new(),
                                    done: true,
                                    usage: None,
                                    model: model.clone(),
                                    error: Some(format!("Anthropic stream error: {error_msg}")),
                                    tool_call_deltas: None,
                                    finish_reason: None,
                                })
                                .await;
                            return;
                        }
                        _ => {
                            // ping, content_block_start, content_block_stop, etc.
                            debug!(event = %event_type, "Ignoring Anthropic SSE event");
                        }
                    }
                }
            }

            // If we exit the loop without having sent `done: true` (no
            // `message_stop` received — e.g. connection dropped or stream
            // error), send a final chunk so the handler knows the stream
            // ended and can flush its buffers.
            debug!("Anthropic stream ended without message_stop, sending done");
            let _ = tx
                .send(ChatChunk {
                    delta: String::new(),
                    done: true,
                    usage: Some(Usage {
                        prompt_tokens: input_tokens,
                        completion_tokens: output_tokens,
                        total_tokens: input_tokens + output_tokens,
                    }),
                    model: model.clone(),
                    error: None,
                    tool_call_deltas: None,
                    finish_reason: None,
                })
                .await;
        });

        Ok(ReceiverStream::new(rx))
    }
}

/// Extract system messages from the ChatRequest and build the Anthropic
/// messages array. Anthropic requires system messages as a separate top-level
/// parameter, not in the messages array.
///
/// Also handles multimodal content (images) using Anthropic's format.
/// Build the Anthropic messages request, enforcing strict user/assistant alternation.
///
/// The Anthropic API rejects consecutive messages with the same role. When
/// the session history has adjacent same-role messages (e.g. multiple user
/// messages from retries), they are merged with a newline separator.
pub fn build_anthropic_request(request: &ChatRequest) -> (String, Vec<serde_json::Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<serde_json::Value> = Vec::new();
    let mut last_role: Option<&str> = None;

    for msg in &request.messages {
        if msg.role == Role::System {
            system_parts.push(msg.content.clone());
            continue;
        }

        let role = role_to_str(&msg.role);

        // Anthropic multimodal format: content is array of blocks
        if let Some(images) = &msg.images {
            if !images.is_empty() {
                let mut content_parts = vec![serde_json::json!({
                    "type": "text",
                    "text": msg.content,
                })];
                for b64 in images {
                    content_parts.push(serde_json::json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/jpeg",
                            "data": b64,
                        },
                    }));
                }
                // For multimodal, always push a new message (merging would be complex)
                if last_role == Some(role) {
                    // Insert a placeholder to maintain alternation
                    messages.push(serde_json::json!({
                        "role": if role == "user" { "assistant" } else { "user" },
                        "content": "...",
                    }));
                }
                messages.push(serde_json::json!({
                    "role": role,
                    "content": content_parts,
                }));
                last_role = Some(role);
                continue;
            }
        }

        // Merge consecutive same-role text messages
        if last_role == Some(role) {
            if let Some(last) = messages.last_mut() {
                if let Some(prev_content) = last.get("content").and_then(|c| c.as_str()) {
                    last["content"] =
                        serde_json::json!(format!("{}\n{}", prev_content, msg.content));
                    continue;
                }
            }
        }

        messages.push(serde_json::json!({
            "role": role,
            "content": msg.content,
        }));
        last_role = Some(role);
    }

    let system_text = system_parts.join("\n\n");
    (system_text, messages)
}

fn role_to_str(role: &Role) -> &'static str {
    match role {
        Role::System => "user", // Should not happen -- filtered above
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user", // Tool results injected as user messages for Anthropic compat
    }
}

/// Check if a model name should be routed to the Anthropic provider.
pub fn is_anthropic_model(model: &str) -> bool {
    model.starts_with("claude-") || model.starts_with("anthropic/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatMessage, Role};

    // -----------------------------------------------------------------------
    // Constructor tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_constructor_default_url() {
        let provider = AnthropicProvider::new("sk-ant-test", "anthropic", 300, None);
        assert_eq!(provider.base_url, "https://api.anthropic.com");
        assert_eq!(provider.name(), "anthropic");
        match &provider.key_source {
            ApiKeySource::Static { key } => assert_eq!(key, "sk-ant-test"),
            _ => panic!("Expected Static key source"),
        }
    }

    #[test]
    fn test_constructor_custom_url() {
        let provider = AnthropicProvider::new(
            "sk-ant-test",
            "anthropic-custom",
            300,
            Some("https://custom.api.example.com/".to_string()),
        );
        assert_eq!(provider.base_url, "https://custom.api.example.com");
        assert_eq!(provider.name(), "anthropic-custom");
    }

    // -----------------------------------------------------------------------
    // Request serialization tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_system_message_extraction() {
        let request = ChatRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![
                ChatMessage {
                    role: Role::System,
                    content: "You are a helpful assistant.".to_string(),
                    images: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                ChatMessage {
                    role: Role::User,
                    content: "Hello".to_string(),
                    images: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        let (system, messages) = build_anthropic_request(&request);
        assert_eq!(system, "You are a helpful assistant.");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Hello");
    }

    #[test]
    fn test_multiple_system_messages_concatenated() {
        let request = ChatRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![
                ChatMessage {
                    role: Role::System,
                    content: "Rule 1: Be helpful.".to_string(),
                    images: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                ChatMessage {
                    role: Role::System,
                    content: "Rule 2: Be concise.".to_string(),
                    images: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                ChatMessage {
                    role: Role::User,
                    content: "Hi".to_string(),
                    images: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        let (system, messages) = build_anthropic_request(&request);
        assert_eq!(system, "Rule 1: Be helpful.\n\nRule 2: Be concise.");
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_no_system_message() {
        let request = ChatRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: "Hello".to_string(),
                images: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        let (system, messages) = build_anthropic_request(&request);
        assert!(system.is_empty());
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_multimodal_message_format() {
        let request = ChatRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: "What is this?".to_string(),
                images: Some(vec!["base64encodeddata".to_string()]),
                tool_calls: None,
                tool_call_id: None,
            }],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        let (_, messages) = build_anthropic_request(&request);
        assert_eq!(messages.len(), 1);
        let content = messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "What is this?");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/jpeg");
        assert_eq!(content[1]["source"]["data"], "base64encodeddata");
    }

    #[test]
    fn test_conversation_message_order() {
        let request = ChatRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![
                ChatMessage {
                    role: Role::System,
                    content: "Be helpful.".to_string(),
                    images: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                ChatMessage {
                    role: Role::User,
                    content: "Hello".to_string(),
                    images: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                ChatMessage {
                    role: Role::Assistant,
                    content: "Hi there!".to_string(),
                    images: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                ChatMessage {
                    role: Role::User,
                    content: "How are you?".to_string(),
                    images: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        let (system, messages) = build_anthropic_request(&request);
        assert_eq!(system, "Be helpful.");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Hello");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "Hi there!");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"], "How are you?");
    }

    // -----------------------------------------------------------------------
    // Response parsing tests (SSE events)
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_message_start() {
        let data = r#"{"type":"message_start","message":{"id":"msg_123","type":"message","role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":25,"output_tokens":0}}}"#;
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(parsed["type"], "message_start");
        let input = parsed["message"]["usage"]["input_tokens"].as_u64().unwrap();
        assert_eq!(input, 25);
    }

    #[test]
    fn test_parse_content_block_delta() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello, how can"}}"#;
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(parsed["type"], "content_block_delta");
        assert_eq!(parsed["delta"]["type"], "text_delta");
        assert_eq!(parsed["delta"]["text"], "Hello, how can");
    }

    #[test]
    fn test_parse_message_delta_with_usage() {
        let data = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":42}}"#;
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(parsed["type"], "message_delta");
        let output = parsed["usage"]["output_tokens"].as_u64().unwrap();
        assert_eq!(output, 42);
        assert_eq!(parsed["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn test_parse_message_stop() {
        let data = r#"{"type":"message_stop"}"#;
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(parsed["type"], "message_stop");
    }

    #[test]
    fn test_parse_error_event() {
        let data = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(parsed["type"], "error");
        assert_eq!(parsed["error"]["message"], "Overloaded");
    }

    // -----------------------------------------------------------------------
    // Model prefix routing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_anthropic_model_claude_prefix() {
        assert!(is_anthropic_model("claude-sonnet-4-6"));
        assert!(is_anthropic_model("claude-opus-4-6"));
        assert!(is_anthropic_model("claude-haiku-4-5-20250620"));
        assert!(is_anthropic_model("claude-3-opus-20240229"));
    }

    #[test]
    fn test_is_anthropic_model_anthropic_prefix() {
        assert!(is_anthropic_model("anthropic/claude-sonnet-4-6"));
        assert!(is_anthropic_model("anthropic/claude-haiku-4-5-20250620"));
    }

    #[test]
    fn test_is_not_anthropic_model() {
        assert!(!is_anthropic_model("qwen3-coder"));
        assert!(!is_anthropic_model("gpt-4"));
        assert!(!is_anthropic_model("llama-3"));
        assert!(!is_anthropic_model(""));
    }

    // -----------------------------------------------------------------------
    // Usage tracking test
    // -----------------------------------------------------------------------

    #[test]
    fn test_usage_accumulation_simulation() {
        // Simulate how the streaming loop accumulates usage from
        // message_start (input_tokens) and message_delta (output_tokens)
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;

        // message_start event
        let start_data = r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"output_tokens":0}}}"#;
        let start: serde_json::Value = serde_json::from_str(start_data).unwrap();
        if let Some(usage) = start.get("message").and_then(|m| m.get("usage")) {
            input_tokens = usage["input_tokens"].as_u64().unwrap_or(0) as u32;
        }

        // message_delta event
        let delta_data = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":55}}"#;
        let delta: serde_json::Value = serde_json::from_str(delta_data).unwrap();
        if let Some(usage) = delta.get("usage") {
            output_tokens = usage["output_tokens"].as_u64().unwrap_or(0) as u32;
        }

        let final_usage = Usage {
            prompt_tokens: input_tokens,
            completion_tokens: output_tokens,
            total_tokens: input_tokens + output_tokens,
        };

        assert_eq!(final_usage.prompt_tokens, 100);
        assert_eq!(final_usage.completion_tokens, 55);
        assert_eq!(final_usage.total_tokens, 155);
    }

    // -----------------------------------------------------------------------
    // Models list test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_models_returns_known_models() {
        let provider = AnthropicProvider::new("sk-ant-test", "anthropic", 300, None);
        let models = provider.models().await.unwrap();
        assert!(models.len() >= 4);
        assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
        assert!(models.iter().all(|m| m.provider == "anthropic"));
    }

    // -----------------------------------------------------------------------
    // Role conversion test
    // -----------------------------------------------------------------------

    #[test]
    fn test_role_to_str() {
        assert_eq!(role_to_str(&Role::User), "user");
        assert_eq!(role_to_str(&Role::Assistant), "assistant");
        assert_eq!(role_to_str(&Role::System), "user"); // fallback, should not be used
    }

    // -----------------------------------------------------------------------
    // Empty images field treated as no images
    // -----------------------------------------------------------------------

    #[test]
    fn test_empty_images_vec_not_multimodal() {
        let request = ChatRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: "Hello".to_string(),
                images: Some(vec![]),
                tool_calls: None,
                tool_call_id: None,
            }],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        let (_, messages) = build_anthropic_request(&request);
        // Should be plain string content, not array
        assert!(messages[0]["content"].is_string());
        assert_eq!(messages[0]["content"], "Hello");
    }

    #[test]
    fn test_resolve_key_static() {
        let provider = AnthropicProvider::new("sk-ant-test-key", "anthropic", 300, None);
        let key = provider.resolve_key().unwrap();
        assert_eq!(key, "sk-ant-test-key");
    }

    #[test]
    fn test_resolve_key_oauth_missing_file() {
        let provider = AnthropicProvider::new_oauth(
            std::path::PathBuf::from("/nonexistent/path/.credentials.json"),
            "anthropic",
            300,
            None,
        );
        let result = provider.resolve_key();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to read OAuth credentials"));
    }

    #[test]
    fn test_is_oauth_file_source() {
        let provider = AnthropicProvider::new_oauth(
            std::path::PathBuf::from("/tmp/creds.json"),
            "anthropic",
            300,
            None,
        );
        assert!(provider.is_oauth());
    }

    #[test]
    fn test_is_oauth_static_api_key() {
        let provider = AnthropicProvider::new("sk-ant-api03-test", "anthropic", 300, None);
        assert!(!provider.is_oauth());
    }

    // -----------------------------------------------------------------------
    // Thinking config tests
    // -----------------------------------------------------------------------

    /// Build an Anthropic request body the same way chat() does, for testing thinking.
    fn build_anthropic_test_body(
        thinking: Option<crate::types::ThinkingConfig>,
    ) -> serde_json::Value {
        let request = ChatRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: "Hello".to_string(),
                images: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking,
            tools: None,
            tool_choice: None,
        };

        let max_tokens = request.max_tokens.unwrap_or(8192);
        let (system_text, messages) = build_anthropic_request(&request);

        let mut body = serde_json::json!({
            "model": &request.model,
            "max_tokens": max_tokens,
            "messages": messages,
            "stream": true,
        });

        if !system_text.is_empty() {
            body["system"] = serde_json::json!(system_text);
        }

        // Replicate thinking logic from chat()
        if let Some(ref thinking) = request.thinking {
            if thinking.enabled {
                let budget = thinking.budget_tokens.unwrap_or(4096);
                let effective_budget = if budget == -1 { 16384 } else { budget };
                body["thinking"] = serde_json::json!({
                    "type": "enabled",
                    "budget_tokens": effective_budget,
                });
            }
        }

        body
    }

    #[test]
    fn test_anthropic_thinking_enabled_with_budget() {
        let body = build_anthropic_test_body(Some(crate::types::ThinkingConfig {
            enabled: true,
            budget_tokens: Some(4096),
        }));
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
    }

    #[test]
    fn test_anthropic_thinking_enabled_unlimited() {
        let body = build_anthropic_test_body(Some(crate::types::ThinkingConfig {
            enabled: true,
            budget_tokens: Some(-1),
        }));
        assert_eq!(body["thinking"]["type"], "enabled");
        // -1 maps to 16384 for Anthropic
        assert_eq!(body["thinking"]["budget_tokens"], 16384);
    }

    #[test]
    fn test_anthropic_thinking_enabled_default_budget() {
        let body = build_anthropic_test_body(Some(crate::types::ThinkingConfig {
            enabled: true,
            budget_tokens: None,
        }));
        assert_eq!(body["thinking"]["type"], "enabled");
        // Default budget is 4096
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
    }

    #[test]
    fn test_anthropic_thinking_disabled() {
        let body = build_anthropic_test_body(Some(crate::types::ThinkingConfig {
            enabled: false,
            budget_tokens: Some(0),
        }));
        // When disabled, thinking field should not be present
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn test_anthropic_thinking_none() {
        let body = build_anthropic_test_body(None);
        // When None, thinking field should not be present
        assert!(body.get("thinking").is_none());
    }
}
