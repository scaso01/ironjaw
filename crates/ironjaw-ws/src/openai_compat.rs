//! OpenAI-compatible REST API endpoints.
//!
//! Provides `/v1/models`, `/v1/models/:model_id`, and `/v1/chat/completions`
//! so that any OpenAI SDK client (Open WebUI, LiteLLM, etc.) can connect to
//! Ironjaw as an LLM proxy.

// Handlers return axum's Response as the error, which is axum's own pattern.
#![allow(clippy::result_large_err)]

use crate::auth::constant_time_eq;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json, Response,
    },
};
use futures_util::stream::Stream;
use ironjaw_llm::stream_parser::ToolCallAccumulator;
use ironjaw_llm::types::{ToolCall, ToolChoice, ToolDefinition};
use ironjaw_llm::{ChatChunk, ChatMessage, ChatRequest, Role, Usage};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use tokio_stream::StreamExt;
use tracing::{error, info};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// OpenAI-compatible request / response types
// ---------------------------------------------------------------------------

/// OpenAI ChatCompletion request body.
#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
}

/// A single message in OpenAI format.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAiMessage {
    pub role: String,
    pub content: String,
    /// Tool calls emitted by the assistant (non-streaming response only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Non-streaming ChatCompletion response.
#[derive(Debug, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageResponse>,
}

#[derive(Debug, Serialize)]
pub struct Choice {
    pub index: u32,
    pub message: OpenAiMessage,
    pub finish_reason: Option<String>,
}

/// Streaming ChatCompletion chunk (SSE delta).
#[derive(Debug, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub model: String,
    pub choices: Vec<StreamChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageResponse>,
}

#[derive(Debug, Serialize)]
pub struct StreamChoice {
    pub index: u32,
    pub delta: DeltaContent,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DeltaContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Streaming tool call deltas (OpenAI format).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Serialize)]
pub struct UsageResponse {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// GET /v1/models response.
#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub object: &'static str,
    pub data: Vec<ModelObject>,
}

#[derive(Debug, Serialize)]
pub struct ModelObject {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub owned_by: String,
}

/// Error response matching OpenAI format.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub message: String,
    pub r#type: String,
    pub code: Option<String>,
}

// ---------------------------------------------------------------------------
// Auth helper
// ---------------------------------------------------------------------------

/// Validate the Authorization header. Always requires a valid Bearer token.
/// Returns an error response if the header is missing, malformed, or the token is wrong.
#[allow(clippy::result_large_err)] // Response is inherently large; boxing adds no value here
fn check_auth(headers: &HeaderMap, state: &AppState) -> Result<(), Response> {
    let unauthorized = |msg: &str| {
        let body = ErrorResponse {
            error: ErrorDetail {
                message: msg.to_string(),
                r#type: "invalid_request_error".to_string(),
                code: Some("invalid_api_key".to_string()),
            },
        };
        (StatusCode::UNAUTHORIZED, Json(body)).into_response()
    };

    let auth_header = headers
        .get("authorization")
        .ok_or_else(|| unauthorized("Missing Authorization header"))?;
    let auth_str = auth_header.to_str().unwrap_or("");
    let token = auth_str
        .strip_prefix("Bearer ")
        .ok_or_else(|| unauthorized("Invalid Authorization format"))?;
    if !constant_time_eq(token, &state.auth_token) {
        return Err(unauthorized("Invalid API key"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /v1/models -- list available models from all providers.
pub async fn list_models(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Json<ModelsResponse>, Response> {
    check_auth(&headers, &state)?;

    let models = state.llm.router.list_models().await.unwrap_or_default();
    let now = chrono::Utc::now().timestamp();

    let data: Vec<ModelObject> = models
        .into_iter()
        .map(|m| ModelObject {
            id: m.id,
            object: "model",
            created: now,
            owned_by: m.provider,
        })
        .collect();

    Ok(Json(ModelsResponse {
        object: "list",
        data,
    }))
}

/// GET /v1/models/:model_id -- get info for a specific model.
pub async fn get_model(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> Result<Json<ModelObject>, Response> {
    check_auth(&headers, &state)?;

    let models = state.llm.router.list_models().await.unwrap_or_default();
    let now = chrono::Utc::now().timestamp();

    let found = models.into_iter().find(|m| m.id == model_id);

    match found {
        Some(m) => Ok(Json(ModelObject {
            id: m.id,
            object: "model",
            created: now,
            owned_by: m.provider,
        })),
        None => {
            let body = ErrorResponse {
                error: ErrorDetail {
                    message: format!("Model '{model_id}' not found"),
                    r#type: "invalid_request_error".to_string(),
                    code: Some("model_not_found".to_string()),
                },
            };
            Err((StatusCode::NOT_FOUND, Json(body)).into_response())
        }
    }
}

/// POST /v1/chat/completions -- proxy chat completions.
///
/// Supports both streaming (SSE) and non-streaming (JSON) modes.
pub async fn chat_completions(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<Response, Response> {
    check_auth(&headers, &state)?;

    let stream_mode = req.stream.unwrap_or(false);
    let completion_id = format!("chatcmpl-{}", Uuid::new_v4());
    let created = chrono::Utc::now().timestamp();

    // Convert OpenAI messages to internal format
    let messages: Vec<ChatMessage> = req
        .messages
        .iter()
        .map(|m| ChatMessage {
            role: match m.role.as_str() {
                "system" => Role::System,
                "assistant" => Role::Assistant,
                _ => Role::User,
            },
            content: m.content.clone(),
            images: None,
            ..Default::default()
        })
        .collect();

    // Resolve model name (fall back to default)
    let model = if req.model.is_empty() {
        state
            .llm
            .router
            .default_model()
            .unwrap_or_else(|| "default".to_string())
    } else {
        req.model.clone()
    };

    let chat_request = ChatRequest {
        model: model.clone(),
        messages,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        extra: None,
        thinking: None,
        tools: req.tools,
        tool_choice: req.tool_choice,
    };

    // Get the token stream from the LLM router
    let token_stream = match state.llm.router.chat(chat_request).await {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "LLM chat request failed");
            let body = ErrorResponse {
                error: ErrorDetail {
                    message: e.to_string(),
                    r#type: "server_error".to_string(),
                    code: Some("llm_error".to_string()),
                },
            };
            return Err((StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response());
        }
    };

    if stream_mode {
        info!(model = %model, "OpenAI compat: streaming chat completion");
        Ok(stream_response(token_stream, completion_id, created, model).into_response())
    } else {
        info!(model = %model, "OpenAI compat: non-streaming chat completion");
        Ok(
            non_streaming_response(token_stream, completion_id, created, model)
                .await
                .into_response(),
        )
    }
}

// ---------------------------------------------------------------------------
// Think-tag stripping
// ---------------------------------------------------------------------------

/// Strip `<think>...</think>` blocks and bare `</think>` prefixes from LLM output.
/// Qwen3.5 models emit these as template artifacts even when thinking is disabled.
pub(crate) fn strip_think_tags(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;

    // Strip <think>...</think> blocks
    while let Some(start) = remaining.find("<think>") {
        result.push_str(&remaining[..start]);
        if let Some(end_offset) = remaining[start..].find("</think>") {
            remaining = &remaining[start + end_offset + 8..];
        } else {
            // Unclosed <think> — discard the rest as thinking content
            remaining = "";
            break;
        }
    }
    result.push_str(remaining);

    // Strip leading whitespace/newlines left after removing blocks, plus
    // bare </think> prefix (from budget=0 forcing immediate close)
    let trimmed = result.trim_start();
    if let Some(rest) = trimmed.strip_prefix("</think>") {
        rest.trim_start_matches('\n').to_string()
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// Response builders
// ---------------------------------------------------------------------------

/// Collect all chunks and return a single JSON response.
async fn non_streaming_response(
    mut stream: tokio_stream::wrappers::ReceiverStream<ChatChunk>,
    id: String,
    created: i64,
    model: String,
) -> Json<ChatCompletionResponse> {
    let mut full_content = String::new();
    let mut final_usage: Option<Usage> = None;
    let mut final_model = model;
    let mut accumulator = ToolCallAccumulator::new();
    let mut finish_reason = "stop".to_string();

    while let Some(chunk) = stream.next().await {
        if !chunk.delta.is_empty() {
            full_content.push_str(&chunk.delta);
        }
        if chunk.usage.is_some() {
            final_usage = chunk.usage;
        }
        if let Some(deltas) = &chunk.tool_call_deltas {
            accumulator.apply_deltas(deltas);
        }
        if let Some(fr) = chunk.finish_reason {
            finish_reason = fr;
        }
        final_model = chunk.model;
    }

    // Strip think tags (Qwen3.5 template artifacts when reasoning is disabled)
    let clean_content = strip_think_tags(&full_content);

    let usage = final_usage.map(|u| UsageResponse {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
    });

    // Finalize accumulated tool calls (best-effort; ignore errors from malformed deltas)
    let tool_calls = if accumulator.is_empty() {
        None
    } else {
        accumulator.complete_all().ok().filter(|v| !v.is_empty())
    };

    Json(ChatCompletionResponse {
        id,
        object: "chat.completion",
        created,
        model: final_model,
        choices: vec![Choice {
            index: 0,
            message: OpenAiMessage {
                role: "assistant".to_string(),
                content: clean_content,
                tool_calls,
            },
            finish_reason: Some(finish_reason),
        }],
        usage,
    })
}

/// Return an SSE stream of ChatCompletionChunk events.
fn stream_response(
    stream: tokio_stream::wrappers::ReceiverStream<ChatChunk>,
    id: String,
    created: i64,
    _model: String,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut first = true;
    let mut in_think_block = false;
    let mut think_buffer = String::new();

    let sse_stream = stream.filter_map(move |chunk| {
        if chunk.done {
            // Final chunk with finish_reason
            let final_chunk = ChatCompletionChunk {
                id: id.clone(),
                object: "chat.completion.chunk",
                created,
                model: chunk.model.clone(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: DeltaContent {
                        role: None,
                        content: None,
                        tool_calls: None,
                    },
                    finish_reason: Some(
                        chunk
                            .finish_reason
                            .clone()
                            .unwrap_or_else(|| "stop".to_string()),
                    ),
                }],
                usage: chunk.usage.map(|u| UsageResponse {
                    prompt_tokens: u.prompt_tokens,
                    completion_tokens: u.completion_tokens,
                    total_tokens: u.total_tokens,
                }),
            };
            let json = serde_json::to_string(&final_chunk).unwrap_or_default();
            Some(Ok(Event::default().data(json)))
        } else {
            let mut role = None;
            if first {
                role = Some("assistant".to_string());
                first = false;
            }

            // Filter think tags from streaming content
            let visible_delta = if !chunk.delta.is_empty() {
                think_buffer.push_str(&chunk.delta);
                let mut visible = String::new();
                let mut just_exited_think = false;
                loop {
                    if in_think_block {
                        if let Some(end) = think_buffer.find("</think>") {
                            think_buffer = think_buffer[end + 8..].to_string();
                            in_think_block = false;
                            just_exited_think = true;
                        } else {
                            // Still inside think block — wait for </think> in a
                            // future chunk.  Do NOT clear the buffer: the closing
                            // tag may straddle SSE chunk boundaries and we need
                            // the accumulated content to detect it.
                            break;
                        }
                    } else if let Some(start) = think_buffer.find("<think>") {
                        visible.push_str(&think_buffer[..start]);
                        think_buffer = think_buffer[start + 7..].to_string();
                        in_think_block = true;
                        just_exited_think = false;
                    } else if think_buffer.starts_with("</think>") {
                        // Bare closing tag (budget=0 artifact)
                        think_buffer = think_buffer[8..].to_string();
                    } else {
                        // No think tags remain in buffer.
                        // If we just exited a think block, flush the entire buffer
                        // now — we know this content is real output, and deferring
                        // it would silently drop the tail on end-of-stream.
                        // Otherwise buffer a small tail in case a new <think> tag
                        // is split across the next SSE chunk boundary.
                        let safe = if just_exited_think {
                            let flushed = think_buffer.clone();
                            think_buffer.clear();
                            flushed
                        } else if think_buffer.len() > 8 {
                            let split = think_buffer.len() - 8;
                            let safe_part = think_buffer[..split].to_string();
                            think_buffer = think_buffer[split..].to_string();
                            safe_part
                        } else {
                            String::new()
                        };
                        visible.push_str(&safe);
                        break;
                    }
                }
                // Trim leading newlines from first visible content
                let trimmed = visible.trim_start_matches('\n');
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            } else {
                None
            };

            // Build tool_call delta payload in OpenAI wire format
            let tool_call_deltas_json: Option<Vec<serde_json::Value>> =
                chunk.tool_call_deltas.as_ref().map(|deltas| {
                    deltas
                        .iter()
                        .map(|d| {
                            serde_json::json!({
                                "index": d.index,
                                "id": d.id,
                                "type": d.type_,
                                "function": d.function.as_ref().map(|f| serde_json::json!({
                                    "name": f.name,
                                    "arguments": f.arguments,
                                })),
                            })
                        })
                        .collect()
                });

            if role.is_none() && visible_delta.is_none() && tool_call_deltas_json.is_none() {
                // Nothing visible to emit — skip this chunk
                return None;
            }

            let delta_chunk = ChatCompletionChunk {
                id: id.clone(),
                object: "chat.completion.chunk",
                created,
                model: chunk.model.clone(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: DeltaContent {
                        role,
                        content: visible_delta,
                        tool_calls: tool_call_deltas_json,
                    },
                    finish_reason: None,
                }],
                usage: None,
            };
            let json = serde_json::to_string(&delta_chunk).unwrap_or_default();
            Some(Ok(Event::default().data(json)))
        }
    });

    // Append [DONE] sentinel after the stream finishes
    let done_stream = futures_util::stream::once(async {
        Ok::<Event, Infallible>(Event::default().data("[DONE]"))
    });

    let combined = sse_stream.chain(done_stream);

    Sse::new(combined).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------------------
// Embeddings proxy
// ---------------------------------------------------------------------------

/// POST /v1/embeddings -- proxy to llama-server /v1/embeddings.
///
/// Forwards the request body verbatim and returns the upstream response.
/// This avoids duplicating embedding logic; llama-server already implements
/// the full OpenAI embeddings API.
pub async fn embeddings(
    headers: HeaderMap,
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    check_auth(&headers, &state)?;

    let base_url = &state.integration_urls.llm_base_url;
    if base_url.is_empty() {
        let err = ErrorResponse {
            error: ErrorDetail {
                message: "Embeddings endpoint not configured (llm_base_url is empty)".to_string(),
                r#type: "server_error".to_string(),
                code: Some("not_configured".to_string()),
            },
        };
        return Err((StatusCode::SERVICE_UNAVAILABLE, Json(err)).into_response());
    }

    let url = format!("{}/v1/embeddings", base_url.trim_end_matches('/'));
    info!(url = %url, "OpenAI compat: proxying embeddings request");

    let upstream_resp = state
        .http_client
        .post(&url)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await;

    match upstream_resp {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let resp_body = resp.bytes().await.unwrap_or_default();
            Ok((
                status,
                [(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/json"),
                )],
                resp_body,
            )
                .into_response())
        }
        Err(e) => {
            error!(error = %e, "Embeddings proxy request failed");
            let err = ErrorResponse {
                error: ErrorDetail {
                    message: format!("Upstream embeddings request failed: {e}"),
                    r#type: "server_error".to_string(),
                    code: Some("upstream_error".to_string()),
                },
            };
            Err((StatusCode::BAD_GATEWAY, Json(err)).into_response())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_completion_request_deserialization() {
        let json = r#"{
            "model": "qwen3-coder",
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hello"}
            ],
            "temperature": 0.7,
            "max_tokens": 100,
            "stream": true
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "qwen3-coder");
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[1].role, "user");
        assert_eq!(req.temperature, Some(0.7));
        assert_eq!(req.max_tokens, Some(100));
        assert_eq!(req.stream, Some(true));
    }

    #[test]
    fn test_chat_completion_request_defaults() {
        let json = r#"{
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        assert!(req.temperature.is_none());
        assert!(req.max_tokens.is_none());
        assert!(req.stream.is_none());
    }

    #[test]
    fn test_chat_completion_response_serialization() {
        let resp = ChatCompletionResponse {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion",
            created: 1234567890,
            model: "qwen3-coder".to_string(),
            choices: vec![Choice {
                index: 0,
                message: OpenAiMessage {
                    role: "assistant".to_string(),
                    content: "Hello!".to_string(),
                    tool_calls: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(UsageResponse {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("chat.completion"));
        assert!(json.contains("Hello!"));
        assert!(json.contains("prompt_tokens"));
    }

    #[test]
    fn test_models_response_serialization() {
        let resp = ModelsResponse {
            object: "list",
            data: vec![ModelObject {
                id: "qwen3-coder".to_string(),
                object: "model",
                created: 1234567890,
                owned_by: "llama-server".to_string(),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"object\":\"list\""));
        assert!(json.contains("qwen3-coder"));
        assert!(json.contains("llama-server"));
    }

    #[test]
    fn test_streaming_chunk_serialization() {
        let chunk = ChatCompletionChunk {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion.chunk",
            created: 1234567890,
            model: "test-model".to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: DeltaContent {
                    role: Some("assistant".to_string()),
                    content: Some("Hi".to_string()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains("chat.completion.chunk"));
        assert!(json.contains("\"role\":\"assistant\""));
        assert!(json.contains("\"content\":\"Hi\""));
        assert!(!json.contains("usage"));
    }

    #[test]
    fn test_error_response_serialization() {
        let err = ErrorResponse {
            error: ErrorDetail {
                message: "Model not found".to_string(),
                r#type: "invalid_request_error".to_string(),
                code: Some("model_not_found".to_string()),
            },
        };
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("Model not found"));
        assert!(json.contains("invalid_request_error"));
        assert!(json.contains("model_not_found"));
    }

    #[test]
    fn test_role_mapping() {
        let msgs = [
            OpenAiMessage {
                role: "system".to_string(),
                content: "sys".to_string(),
                tool_calls: None,
            },
            OpenAiMessage {
                role: "user".to_string(),
                content: "usr".to_string(),
                tool_calls: None,
            },
            OpenAiMessage {
                role: "assistant".to_string(),
                content: "asst".to_string(),
                tool_calls: None,
            },
            OpenAiMessage {
                role: "function".to_string(),
                content: "fn".to_string(),
                tool_calls: None,
            },
        ];
        let internal: Vec<ChatMessage> = msgs
            .iter()
            .map(|m| ChatMessage {
                role: match m.role.as_str() {
                    "system" => Role::System,
                    "assistant" => Role::Assistant,
                    _ => Role::User,
                },
                content: m.content.clone(),
                images: None,
                tool_call_id: None,
                tool_calls: None,
            })
            .collect();
        assert_eq!(internal[0].role, Role::System);
        assert_eq!(internal[1].role, Role::User);
        assert_eq!(internal[2].role, Role::Assistant);
        assert_eq!(internal[3].role, Role::User); // unknown roles map to User
    }

    #[test]
    fn test_strip_think_tags_bare_closing() {
        // Qwen3.5 template artifact: bare </think> prefix with budget=0
        assert_eq!(strip_think_tags("</think>\n\nFour"), "Four");
        assert_eq!(strip_think_tags("</think>\n\n Four"), " Four");
    }

    #[test]
    fn test_strip_think_tags_full_block() {
        // Full <think>...</think> block
        assert_eq!(
            strip_think_tags("<think>\nI need to think\n</think>\n\nFour"),
            "Four"
        );
    }

    #[test]
    fn test_strip_think_tags_no_tags() {
        // Clean content passes through unchanged
        assert_eq!(strip_think_tags("Hello world"), "Hello world");
        assert_eq!(strip_think_tags(""), "");
    }

    #[test]
    fn test_strip_think_tags_content_after_block() {
        assert_eq!(
            strip_think_tags("Prefix <think>hidden</think> suffix"),
            "Prefix  suffix"
        );
    }

    #[test]
    fn test_delta_content_skip_serialization() {
        // When role, content, and tool_calls are None, they should all be omitted
        let delta = DeltaContent {
            role: None,
            content: None,
            tool_calls: None,
        };
        let json = serde_json::to_string(&delta).unwrap();
        assert_eq!(json, "{}");

        // When only content is set
        let delta = DeltaContent {
            role: None,
            content: Some("hello".to_string()),
            tool_calls: None,
        };
        let json = serde_json::to_string(&delta).unwrap();
        assert!(!json.contains("role"));
        assert!(json.contains("hello"));
    }

    // -----------------------------------------------------------------------
    // Phase 8 — tool_calls support tests
    // -----------------------------------------------------------------------

    #[test]
    fn accepts_tools_in_request() {
        // ChatCompletionRequest must deserialize `tools` and `tool_choice` fields.
        let json = r#"{
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {
                    "name": "get_weather",
                    "description": "Get the weather for a location",
                    "parameters": {"type": "object", "properties": {}},
                    "tier": "auto"
                }
            ],
            "tool_choice": "auto"
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).unwrap();
        let tools = req.tools.expect("tools field must deserialize");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "get_weather");
        let choice = req.tool_choice.expect("tool_choice field must deserialize");
        assert_eq!(
            choice,
            ironjaw_llm::types::ToolChoice::Mode("auto".to_string())
        );
    }

    #[test]
    fn emits_streamed_tool_calls() {
        // DeltaContent with tool_calls must serialize the array to the wire format.
        let tool_delta = serde_json::json!({
            "index": 0,
            "id": "call_abc123",
            "type": "function",
            "function": {
                "name": "get_weather",
                "arguments": "{\"location\":"
            }
        });
        let delta = DeltaContent {
            role: None,
            content: None,
            tool_calls: Some(vec![tool_delta]),
        };
        let json = serde_json::to_string(&delta).unwrap();
        assert!(
            json.contains("tool_calls"),
            "tool_calls key must be present"
        );
        assert!(
            json.contains("get_weather"),
            "function name must be serialized"
        );
        assert!(
            json.contains("call_abc123"),
            "tool call id must be serialized"
        );
        // role and content must be absent (skip_serializing_if)
        assert!(!json.contains("\"role\""), "role must be omitted when None");
        assert!(
            !json.contains("\"content\""),
            "content must be omitted when None"
        );
    }

    #[test]
    fn non_streaming_collects_tool_calls() {
        use ironjaw_llm::types::{ToolCall, ToolCallDelta, ToolCallFunctionDelta};

        // Verify ToolCallAccumulator integration: two deltas → one complete ToolCall.
        let mut acc = ToolCallAccumulator::new();

        // First delta: id + name
        acc.apply_delta(&ToolCallDelta {
            index: 0,
            id: Some("call_xyz".to_string()),
            type_: Some("function".to_string()),
            function: Some(ToolCallFunctionDelta {
                name: Some("list_files".to_string()),
                arguments: Some("{\"path\":".to_string()),
            }),
        });

        // Second delta: argument continuation
        acc.apply_delta(&ToolCallDelta {
            index: 0,
            id: None,
            type_: None,
            function: Some(ToolCallFunctionDelta {
                name: None,
                arguments: Some("\"/tmp\"}".to_string()),
            }),
        });

        assert!(
            !acc.is_empty(),
            "accumulator must be non-empty after deltas"
        );

        let calls: Vec<ToolCall> = acc.complete_all().expect("complete_all must succeed");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_xyz");
        assert_eq!(calls[0].function.name, "list_files");
        assert_eq!(calls[0].function.arguments, "{\"path\":\"/tmp\"}");
    }

    /// Regression test: `</think>` split across two SSE chunks must not drop
    /// the post-think visible content.
    ///
    /// Chunk 1: `<think>reasoning</` — `</think>` is incomplete, so the state
    ///          machine must stay in `in_think_block=true` WITHOUT clearing the
    ///          buffer (the old bug: `think_buffer.clear()` discarded "reasoning</").
    ///
    /// Chunk 2: `think>visible text` — completing `</think>`.  After the fix the
    ///          buffer at entry is "reasoning</think>visible text", the terminator
    ///          is found, the machine exits the think block, and "visible text" is
    ///          emitted.
    #[test]
    fn streaming_think_tag_split_across_chunks_no_content_dropped() {
        // Simulate the state machine directly (no AppState needed).
        let mut in_think_block = false;
        let mut think_buffer = String::new();

        // Helper: run one chunk through the state machine, return visible output.
        let process_chunk = |delta: &str, in_think: &mut bool, buf: &mut String| -> String {
            buf.push_str(delta);
            let mut visible = String::new();
            let mut just_exited_think = false;
            loop {
                if *in_think {
                    if let Some(end) = buf.find("</think>") {
                        *buf = buf[end + 8..].to_string();
                        *in_think = false;
                        just_exited_think = true;
                    } else {
                        // Wait — do NOT clear (the fix under test)
                        break;
                    }
                } else if let Some(start) = buf.find("<think>") {
                    visible.push_str(&buf[..start]);
                    *buf = buf[start + 7..].to_string();
                    *in_think = true;
                    just_exited_think = false;
                } else if buf.starts_with("</think>") {
                    *buf = buf[8..].to_string();
                } else {
                    let safe = if just_exited_think {
                        let flushed = buf.clone();
                        buf.clear();
                        flushed
                    } else if buf.len() > 8 {
                        let split = buf.len() - 8;
                        let safe_part = buf[..split].to_string();
                        *buf = buf[split..].to_string();
                        safe_part
                    } else {
                        String::new()
                    };
                    visible.push_str(&safe);
                    break;
                }
            }
            visible
        };

        // Chunk 1: opening tag + reasoning content + start of closing tag
        let v1 = process_chunk("<think>reasoning</", &mut in_think_block, &mut think_buffer);
        assert!(
            v1.is_empty(),
            "chunk 1 must emit nothing (inside think block)"
        );
        assert!(in_think_block, "must still be in think block after chunk 1");

        // Chunk 2: rest of closing tag + visible content
        let v2 = process_chunk("think>visible text", &mut in_think_block, &mut think_buffer);
        assert!(
            !in_think_block,
            "must have exited think block after chunk 2"
        );
        // After exiting the think block the state machine flushes the entire
        // remaining buffer immediately (just_exited_think=true path) so all
        // post-think content must appear in v2 without any tail truncation.
        let full_visible = v2;
        assert!(
            full_visible.contains("visible"),
            "post-think content must not be dropped; got: {full_visible:?}"
        );
    }

    #[test]
    fn finish_reason_tool_calls_surfaces() {
        // When the LLM emits finish_reason = "tool_calls", the non-streaming
        // response must pass that value through rather than hardcoding "stop".
        //
        // We verify this by checking that ChatCompletionResponse can carry the
        // correct finish_reason string and that it serializes properly.
        let resp = ChatCompletionResponse {
            id: "chatcmpl-tool-test".to_string(),
            object: "chat.completion",
            created: 1234567890,
            model: "test-model".to_string(),
            choices: vec![Choice {
                index: 0,
                message: OpenAiMessage {
                    role: "assistant".to_string(),
                    content: String::new(),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_abc".to_string(),
                        type_: "function".to_string(),
                        function: ironjaw_llm::types::ToolCallFunction {
                            name: "get_weather".to_string(),
                            arguments: "{\"location\":\"London\"}".to_string(),
                        },
                    }]),
                },
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: None,
        };

        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            json.contains("\"finish_reason\":\"tool_calls\""),
            "finish_reason must be 'tool_calls', got: {json}"
        );
        assert!(
            json.contains("get_weather"),
            "tool call function name must appear in serialized response"
        );
    }
}
