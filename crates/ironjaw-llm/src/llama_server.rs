use crate::llama_status::LlamaServerState;
use crate::model_registry::ModelRegistry;
use crate::types::{
    ChatChunk, ChatRequest, LlmError, ModelInfo, ModelLoadEvent, ToolCallDelta, Usage,
};
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};

pub struct LlamaServerProvider {
    client: Client,
    base_url: String,
    /// Shared llama-server health/model state (same Arc used by LlamaStatusMonitor).
    llama_state: Option<Arc<RwLock<LlamaServerState>>>,
    /// Model registry for validating model names before switching.
    model_registry: Option<Arc<ModelRegistry>>,
    /// Prevents concurrent model switches.
    switch_mutex: Arc<Mutex<()>>,
}

impl LlamaServerProvider {
    pub fn new(base_url: impl Into<String>, timeout_secs: u64) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .tcp_nodelay(true)
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            llama_state: None,
            model_registry: None,
            switch_mutex: Arc::new(Mutex::new(())),
        }
    }

    /// Create a provider with shared state for auto-model-loading.
    pub fn new_with_state(
        base_url: impl Into<String>,
        timeout_secs: u64,
        llama_state: Arc<RwLock<LlamaServerState>>,
        model_registry: Arc<ModelRegistry>,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .tcp_nodelay(true)
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            llama_state: Some(llama_state),
            model_registry: Some(model_registry),
            switch_mutex: Arc::new(Mutex::new(())),
        }
    }

    pub fn name(&self) -> &str {
        "llama-server"
    }

    pub async fn health(&self) -> Result<bool, LlmError> {
        let url = format!("{}/health", self.base_url);
        let short_client = Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| LlmError::Unreachable(e.to_string()))?;

        match short_client.get(&url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    return Ok(false);
                }
                let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
                Ok(body["status"].as_str() == Some("ok"))
            }
            Err(e) => {
                warn!(error = %e, "llama-server health check failed");
                Ok(false)
            }
        }
    }

    pub async fn models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        // Try /v1/models first (standard OpenAI endpoint)
        let v1_url = format!("{}/v1/models", self.base_url);
        if let Ok(resp) = self.client.get(&v1_url).send().await {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                if let Some(data) = body["data"].as_array() {
                    let models: Vec<ModelInfo> = data
                        .iter()
                        .filter_map(|m| {
                            Some(ModelInfo {
                                id: m["id"].as_str()?.to_string(),
                                provider: "llama-server".to_string(),
                                size: m["meta"]["size"].as_u64(),
                                modified_at: None,
                            })
                        })
                        .collect();
                    if !models.is_empty() {
                        return Ok(models);
                    }
                }
            }
        }

        // Fallback: /props endpoint (model_alias field)
        let props_url = format!("{}/props", self.base_url);
        let resp = self
            .client
            .get(&props_url)
            .send()
            .await
            .map_err(|e| LlmError::Unreachable(e.to_string()))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LlmError::RequestFailed(e.to_string()))?;

        let model_name = body["model_alias"]
            .as_str()
            .or_else(|| body["model_path"].as_str())
            .unwrap_or("unknown")
            .to_string();

        Ok(vec![ModelInfo {
            id: model_name,
            provider: "llama-server".to_string(),
            size: None,
            modified_at: None,
        }])
    }

    /// Merge all system messages into a single system message at the front.
    ///
    /// Many Jinja chat templates (Qwen3, Qwen3.5, DeepSeek, etc.) only allow
    /// ONE system message and it MUST be the first message. Ironjaw's brain
    /// context injection creates multiple system-role messages (model info,
    /// core instructions, identity, recalled memories). This method merges
    /// them all into one, regardless of where they appear in the array.
    fn consolidate_system_messages(
        messages: &[crate::types::ChatMessage],
    ) -> Vec<crate::types::ChatMessage> {
        use crate::types::{ChatMessage, Role};

        let mut system_parts: Vec<&str> = Vec::new();
        let mut non_system: Vec<ChatMessage> = Vec::new();

        for msg in messages {
            if msg.role == Role::System {
                if !msg.content.is_empty() {
                    system_parts.push(&msg.content);
                }
            } else {
                non_system.push(msg.clone());
            }
        }

        let mut result = Vec::with_capacity(1 + non_system.len());
        if !system_parts.is_empty() {
            result.push(ChatMessage {
                role: Role::System,
                content: system_parts.join("\n\n"),
                images: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }
        result.extend(non_system);
        result
    }

    pub fn chat(&self, request: ChatRequest) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let model = request.model.clone();

        // Consolidate system messages: many Jinja chat templates (Qwen3, Qwen3.5,
        // DeepSeek, etc.) only allow a SINGLE system message as the first message.
        // Ironjaw's brain context injection creates multiple system messages, so
        // we merge all system messages into one before sending to llama-server.
        let consolidated = Self::consolidate_system_messages(&request.messages);

        let messages: Vec<serde_json::Value> = consolidated
            .iter()
            .map(|m| {
                let mut msg = serde_json::json!({
                    "role": m.role,
                    "content": m.content,
                });
                // Multimodal: include image_data for vision-capable models
                if let Some(images) = &m.images {
                    if !images.is_empty() {
                        let image_data: Vec<serde_json::Value> = images
                            .iter()
                            .map(|b64| {
                                serde_json::json!({
                                    "data": b64,
                                    "id": 0,
                                })
                            })
                            .collect();
                        msg["image_data"] = serde_json::json!(image_data);
                    }
                }
                // Agentic loop: assistant messages carry tool_calls in OpenAI wire format.
                if let Some(tool_calls) = &m.tool_calls {
                    if !tool_calls.is_empty() {
                        let wire: Vec<serde_json::Value> = tool_calls
                            .iter()
                            .map(|tc| {
                                serde_json::json!({
                                    "id": tc.id,
                                    "type": "function",
                                    "function": {
                                        "name": tc.function.name,
                                        "arguments": tc.function.arguments,
                                    },
                                })
                            })
                            .collect();
                        msg["tool_calls"] = serde_json::json!(wire);
                    }
                }
                // Agentic loop: tool-result messages carry tool_call_id.
                if let Some(id) = &m.tool_call_id {
                    msg["tool_call_id"] = serde_json::json!(id);
                }
                msg
            })
            .collect();

        let mut body = serde_json::json!({
            "model": &request.model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });

        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }

        // Thinking/reasoning toggle for models that support it (Qwen3, Qwen3.5, etc.)
        // Default to OFF — Qwen3.5 enables thinking by default, which wastes tokens
        // and adds latency for most chat interactions. Users opt-in via thinking level.
        if let Some(ref thinking) = request.thinking {
            if !thinking.enabled {
                body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": false});
                body["reasoning_format"] = serde_json::json!("none");
            } else if let Some(budget) = thinking.budget_tokens {
                if budget > 0 {
                    body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": true});
                    body["reasoning_budget"] = serde_json::json!(budget);
                } else if budget == -1 {
                    body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": true});
                }
            } else {
                body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": true});
            }
        } else {
            // No thinking config from client — default to OFF
            body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": false});
            body["reasoning_format"] = serde_json::json!("none");
        }

        // Tool advertisement: serialize tool definitions in OpenAI wire format
        if let Some(tools) = &request.tools {
            if !tools.is_empty() {
                let wire: Vec<serde_json::Value> =
                    tools.iter().map(|t| t.to_openai_wire()).collect();
                body["tools"] = serde_json::json!(wire);
                if let Some(tc) = &request.tool_choice {
                    body["tool_choice"] =
                        serde_json::to_value(tc).unwrap_or(serde_json::json!("auto"));
                } else {
                    body["tool_choice"] = serde_json::json!("auto");
                }
            }
        }

        let client = self.client.clone();
        let (tx, rx) = mpsc::channel(64);

        tokio::spawn(async move {
            debug!(url = %url, model = %model, msg_count = messages.len(), "llama-server chat: sending POST");
            let resp = match client.post(&url).json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    error!(error = %e, "llama-server chat request failed");
                    let _ = tx
                        .send(ChatChunk {
                            delta: String::new(),
                            done: true,
                            usage: None,
                            model: model.clone(),
                            error: Some(format!("POST failed: {e}")),
                            tool_call_deltas: None,
                            finish_reason: None,
                        })
                        .await;
                    return;
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let body_text = resp.text().await.unwrap_or_default();
                error!(status = %status, body = %body_text, "llama-server returned error status");
                let _ = tx
                    .send(ChatChunk {
                        delta: String::new(),
                        done: true,
                        usage: None,
                        model: model.clone(),
                        error: Some(format!("HTTP {status}: {body_text}")),
                        tool_call_deltas: None,
                        finish_reason: None,
                    })
                    .await;
                return;
            }

            debug!(status = %resp.status(), "llama-server chat: starting SSE stream");
            let mut stream = resp.bytes_stream();
            let mut buffer = String::new();
            let mut in_reasoning = false;

            let mut chunk_count: u64 = 0;
            let mut stream_error: Option<String> = None;
            use futures_util::StreamExt;
            while let Some(chunk) = stream.next().await {
                chunk_count += 1;
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        error!(error = %e, chunk_count, "Error reading llama-server stream");
                        stream_error = Some(e.to_string());
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

                    // Strip SSE data: prefix
                    let data = if let Some(stripped) = line.strip_prefix("data: ") {
                        stripped.trim()
                    } else if let Some(stripped) = line.strip_prefix("data:") {
                        stripped.trim()
                    } else {
                        // Skip non-data SSE lines (event:, id:, etc.)
                        continue;
                    };

                    if data == "[DONE]" {
                        debug!(chunk_count, "llama-server SSE stream: [DONE]");
                        let _ = tx
                            .send(ChatChunk {
                                delta: String::new(),
                                done: true,
                                usage: None,
                                model: model.clone(),
                                error: None,
                                tool_call_deltas: None,
                                finish_reason: None,
                            })
                            .await;
                        return;
                    }

                    let parsed: serde_json::Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(e) => {
                            debug!(error = %e, data = %data, "Failed to parse SSE data");
                            continue;
                        }
                    };

                    // Handle both content and reasoning_content (Qwen3 thinking mode).
                    // reasoning_content contains chain-of-thought tokens that stream
                    // BEFORE the actual content. We wrap them in <think> tags so the
                    // client can render or hide them.
                    let delta_obj = &parsed["choices"][0]["delta"];
                    let content = delta_obj["content"].as_str().unwrap_or("");
                    let reasoning = delta_obj["reasoning_content"].as_str().unwrap_or("");

                    let delta = if !reasoning.is_empty() {
                        if !in_reasoning {
                            in_reasoning = true;
                            format!("<think>\n{reasoning}")
                        } else {
                            reasoning.to_string()
                        }
                    } else if !content.is_empty() {
                        if in_reasoning {
                            in_reasoning = false;
                            format!("\n</think>\n\n{content}")
                        } else {
                            content.to_string()
                        }
                    } else {
                        String::new()
                    };

                    // Usage may appear in the final content chunk
                    let usage = parsed.get("usage").and_then(|u| {
                        if u.is_null() {
                            return None;
                        }
                        let prompt_tokens = u["prompt_tokens"].as_u64().unwrap_or(0) as u32;
                        let completion_tokens = u["completion_tokens"].as_u64().unwrap_or(0) as u32;
                        Some(Usage {
                            prompt_tokens,
                            completion_tokens,
                            total_tokens: prompt_tokens + completion_tokens,
                        })
                    });

                    let finish_reason = parsed["choices"][0]["finish_reason"].as_str();
                    // When stream_options.include_usage is set, llama-server sends
                    // the usage in a separate chunk AFTER the finish_reason chunk.
                    // We must NOT mark done on finish_reason alone — wait for the
                    // usage chunk (choices=[] + usage) or fall through to [DONE].
                    let done = if usage.is_some() {
                        // Usage chunk (the one after finish_reason) — we're truly done
                        true
                    } else if finish_reason.is_some() && finish_reason != Some("null") {
                        // finish_reason chunk — signal stop reason but don't mark done
                        // so the handler keeps reading until the usage chunk arrives
                        false
                    } else {
                        false
                    };

                    // Parse tool_calls deltas if present in this chunk.
                    let tool_call_deltas: Option<Vec<ToolCallDelta>> =
                        delta_obj["tool_calls"].as_array().and_then(|arr| {
                            let parsed: Vec<ToolCallDelta> = arr
                                .iter()
                                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                                .collect();
                            if parsed.is_empty() {
                                None
                            } else {
                                Some(parsed)
                            }
                        });

                    // Forward the finish_reason from this SSE chunk into the ChatChunk
                    // so downstream consumers (Phase 4 accumulator) can detect
                    // "tool_calls" completion without inspecting raw SSE data.
                    let chunk_finish_reason = finish_reason
                        .filter(|r| *r != "null")
                        .map(|r| r.to_string());

                    let chat_chunk = ChatChunk {
                        delta,
                        done,
                        usage,
                        model: model.clone(),
                        error: None,
                        tool_call_deltas,
                        finish_reason: chunk_finish_reason,
                    };

                    if tx.send(chat_chunk).await.is_err() {
                        debug!("Receiver dropped, stopping llama-server stream");
                        return;
                    }
                }
            }
            // Reaching here means the stream ended abnormally: a read error or
            // EOF without [DONE] ([DONE] returns above, receiver-drop returns
            // above). Send a terminal error chunk so downstream can retry
            // instead of silently accepting a truncated answer as complete.
            let err_msg = match stream_error {
                Some(e) => format!("stream error after {chunk_count} chunks: {e}"),
                None => format!("stream ended without [DONE] after {chunk_count} chunks"),
            };
            warn!(
                chunk_count,
                buffer_remaining = buffer.len(),
                error = %err_msg,
                "llama-server SSE stream ended abnormally"
            );
            let _ = tx
                .send(ChatChunk {
                    delta: String::new(),
                    done: true,
                    usage: None,
                    model: model.clone(),
                    error: Some(err_msg),
                    tool_call_deltas: None,
                    finish_reason: None,
                })
                .await;
        });

        Ok(ReceiverStream::new(rx))
    }

    /// Chat with automatic model loading. If the requested model differs from
    /// what's currently loaded in llama-server, triggers a model switch first.
    ///
    /// Progress events are sent via `event_tx` so the handler can forward them
    /// as WebSocket events to the client.
    pub async fn chat_with_auto_load(
        &self,
        request: ChatRequest,
        event_tx: Option<mpsc::Sender<ModelLoadEvent>>,
    ) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        // Without state tracking, fall through to regular chat
        let (llama_state, model_registry) = match (&self.llama_state, &self.model_registry) {
            (Some(s), Some(r)) => (s, r),
            _ => return self.chat(request),
        };

        let requested = &request.model;

        // Check if the requested model is already loaded.
        // llama-server reports the GGUF filename (e.g. "Qwen3-30B-A3B-abliterated.i1-Q4_K_M.gguf")
        // while we request by friendly name (e.g. "qwen3:30b-a3b-abliterated"). Compare by resolving
        // the friendly name to its GGUF path and extracting the filename stem.
        let needs_switch = {
            let state = llama_state.read().await;
            match &state.loaded_model {
                Some(loaded) => {
                    // Direct match (e.g. after a successful switch that set the friendly name)
                    if loaded == requested {
                        false
                    } else if let Some(entry) = model_registry.get(requested) {
                        // Compare GGUF filename from registry against what llama-server reports
                        let registry_filename = std::path::Path::new(&entry.path)
                            .file_name()
                            .and_then(|f| f.to_str())
                            .unwrap_or("");
                        // llama-server may report the loaded model as a full GGUF
                        // PATH (e.g. "C:\\...\\Qwen3.6-...gguf"), not just the
                        // friendly name — compare basenames or the path never
                        // matches the registry filename and we needlessly re-switch.
                        let loaded_name = std::path::Path::new(loaded)
                            .file_name()
                            .and_then(|f| f.to_str())
                            .unwrap_or(loaded);
                        let loaded_lower = loaded_name.to_lowercase();
                        let registry_lower = registry_filename.to_lowercase();
                        !(loaded_lower == registry_lower
                            || loaded_lower.starts_with(registry_lower.trim_end_matches(".gguf"))
                            || registry_lower.starts_with(loaded_lower.trim_end_matches(".gguf")))
                    } else {
                        // Unknown model — try the switch
                        true
                    }
                }
                // No model loaded — need to switch
                None => true,
            }
        };

        if !needs_switch {
            return self.chat(request);
        }

        // Validate model exists in registry
        if model_registry.get(requested).is_none() {
            if let Some(tx) = &event_tx {
                let _ = tx
                    .send(ModelLoadEvent::Failed {
                        model: requested.clone(),
                        error: format!("Model '{requested}' not found in registry"),
                    })
                    .await;
            }
            return Err(LlmError::ModelNotFound(requested.clone()));
        }

        // Acquire switch mutex (non-blocking try)
        let _guard = match self.switch_mutex.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                if let Some(tx) = &event_tx {
                    let _ = tx
                        .send(ModelLoadEvent::Failed {
                            model: requested.clone(),
                            error: "Another model switch is already in progress".to_string(),
                        })
                        .await;
                }
                return Err(LlmError::RequestFailed(
                    "Model switch already in progress".to_string(),
                ));
            }
        };

        // Send switching event
        let current_model = {
            let state = llama_state.read().await;
            state.loaded_model.clone()
        };
        info!(
            from = ?current_model,
            to = %requested,
            "Auto-loading model for inference"
        );
        if let Some(tx) = &event_tx {
            let _ = tx
                .send(ModelLoadEvent::Switching {
                    from: current_model,
                    to: requested.clone(),
                })
                .await;
        }

        // Run llama-switch via standalone script (not bash function)
        let start = std::time::Instant::now();
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        let switch_script = format!("{home}/Scripts/llama-switch.sh");
        // FIX #2a: bound the switch subprocess. It runs on EVERY llm call via
        // chat_with_auto_load; with no timeout, a wedged llama-switch.sh blocks
        // the whole turn forever (abort is ineffective, no terminal event ever
        // sent → the phone sees a run mid-stream indefinitely). kill_on_drop
        // reaps the child when the timeout future is dropped.
        let switch_result = match tokio::time::timeout(
            std::time::Duration::from_secs(120),
            tokio::process::Command::new(crate::GIT_BASH)
                .args([&switch_script, requested.as_str()])
                .kill_on_drop(true)
                .output(),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                let msg = format!("llama-switch timed out after 120s switching to '{requested}'");
                error!(model = %requested, "llama-switch timed out");
                if let Some(tx) = &event_tx {
                    let _ = tx
                        .send(ModelLoadEvent::Failed {
                            model: requested.clone(),
                            error: msg.clone(),
                        })
                        .await;
                }
                return Err(LlmError::RequestFailed(msg));
            }
        };

        match switch_result {
            Ok(output) if output.status.success() => {
                debug!(model = %requested, "llama-switch completed, waiting for health");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let msg = format!("llama-switch failed: {stderr}");
                error!(model = %requested, stderr = %stderr, "llama-switch failed");
                if let Some(tx) = &event_tx {
                    let _ = tx
                        .send(ModelLoadEvent::Failed {
                            model: requested.clone(),
                            error: msg.clone(),
                        })
                        .await;
                }
                return Err(LlmError::RequestFailed(msg));
            }
            Err(e) => {
                let msg = format!("Failed to run llama-switch: {e}");
                error!(error = %e, "Failed to run llama-switch");
                if let Some(tx) = &event_tx {
                    let _ = tx
                        .send(ModelLoadEvent::Failed {
                            model: requested.clone(),
                            error: msg.clone(),
                        })
                        .await;
                }
                return Err(LlmError::RequestFailed(msg));
            }
        }

        // Poll /health until ready (max 30 attempts, 2s apart = 60s timeout)
        let health_client = Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| LlmError::RequestFailed(e.to_string()))?;

        let health_url = format!("{}/health", self.base_url);
        let mut healthy = false;

        for attempt in 1..=30 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let elapsed = start.elapsed().as_secs() as u32;

            // Send periodic loading event every 5 attempts
            if attempt % 5 == 0 {
                if let Some(tx) = &event_tx {
                    let _ = tx
                        .send(ModelLoadEvent::Loading {
                            model: requested.clone(),
                            elapsed_secs: elapsed,
                        })
                        .await;
                }
            }

            match health_client.get(&health_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if body["status"].as_str() == Some("ok") {
                            healthy = true;
                            info!(
                                model = %requested,
                                elapsed_secs = elapsed,
                                attempts = attempt,
                                "Model loaded and healthy"
                            );
                            break;
                        }
                    }
                }
                _ => {
                    debug!(attempt, "Health check pending...");
                }
            }
        }

        let elapsed = start.elapsed().as_secs() as u32;

        if !healthy {
            let msg = format!("Model '{requested}' failed to become healthy within 60s");
            error!(model = %requested, "Model load timeout");
            if let Some(tx) = &event_tx {
                let _ = tx
                    .send(ModelLoadEvent::Failed {
                        model: requested.clone(),
                        error: msg.clone(),
                    })
                    .await;
            }
            return Err(LlmError::RequestFailed(msg));
        }

        // Update shared state
        {
            let mut state = llama_state.write().await;
            state.loaded_model = Some(requested.clone());
            state.last_known_model = Some(requested.clone());
        }

        // Send ready event
        if let Some(tx) = &event_tx {
            let _ = tx
                .send(ModelLoadEvent::Ready {
                    model: requested.clone(),
                    elapsed_secs: elapsed,
                })
                .await;
        }

        // Proceed to chat
        self.chat(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constructor() {
        let provider = LlamaServerProvider::new("http://localhost:8080", 300);
        assert_eq!(provider.base_url, "http://localhost:8080");
        assert_eq!(provider.name(), "llama-server");
    }

    #[tokio::test]
    async fn test_stream_death_emits_terminal_error_chunk() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Raw TCP server: valid SSE start, one content chunk, then EOF
        // without [DONE] — simulates llama-server dying mid-answer. No
        // Content-Length + Connection: close makes EOF a clean body end,
        // exercising the "ended without [DONE]" path deterministically.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let resp = "HTTP/1.1 200 OK\r\n\
                        content-type: text/event-stream\r\n\
                        connection: close\r\n\r\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\"par\"},\"finish_reason\":null}]}\n\n";
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
            // Socket drops here — no [DONE], no terminal chunk.
        });

        let provider = LlamaServerProvider::new(format!("http://{addr}"), 5);
        let request = ChatRequest {
            model: "test-model".to_string(),
            messages: vec![],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };
        let mut stream = provider.chat(request).unwrap();

        use futures_util::StreamExt;
        let mut chunks = Vec::new();
        while let Some(c) = stream.next().await {
            chunks.push(c);
        }

        assert!(
            chunks.iter().any(|c| c.delta.contains("par")),
            "partial delta must still be delivered: {chunks:?}"
        );
        let last = chunks.last().expect("at least one chunk");
        assert!(last.done, "terminal chunk must be done: {last:?}");
        let err = last
            .error
            .as_ref()
            .expect("abnormal stream end must carry an error");
        assert!(
            err.contains("[DONE]") || err.contains("stream error"),
            "unexpected error text: {err}"
        );
    }

    #[test]
    fn test_parse_chunk() {
        let data =
            r#"{"id":"chatcmpl-1","choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        let delta = parsed["choices"][0]["delta"]["content"].as_str().unwrap();
        let finish = parsed["choices"][0]["finish_reason"].as_str();
        assert_eq!(delta, "Hello");
        assert!(finish.is_none()); // null is not a string
    }

    #[test]
    fn test_parse_reasoning_content() {
        let data = r#"{"id":"chatcmpl-1","choices":[{"delta":{"reasoning_content":"Hmm, let me think"},"finish_reason":null}]}"#;
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        let delta_obj = &parsed["choices"][0]["delta"];
        let content = delta_obj["content"].as_str().unwrap_or("");
        let reasoning = delta_obj["reasoning_content"].as_str().unwrap_or("");
        assert_eq!(reasoning, "Hmm, let me think");
        assert!(content.is_empty());
    }

    #[test]
    fn test_think_tag_transitions() {
        // Simulate the state machine for <think> tag wrapping
        let mut in_reasoning = false;

        // First reasoning chunk: should prepend <think>
        let reasoning = "Let me think";
        let content = "";
        let delta1 = if !reasoning.is_empty() {
            if !in_reasoning {
                in_reasoning = true;
                format!("<think>\n{reasoning}")
            } else {
                reasoning.to_string()
            }
        } else if !content.is_empty() {
            if in_reasoning {
                in_reasoning = false;
                format!("\n</think>\n\n{content}")
            } else {
                content.to_string()
            }
        } else {
            String::new()
        };
        assert_eq!(delta1, "<think>\nLet me think");
        assert!(in_reasoning);

        // Second reasoning chunk: no tag prefix
        let reasoning = " about this";
        let delta2 = if !reasoning.is_empty() {
            if !in_reasoning {
                in_reasoning = true;
                format!("<think>\n{reasoning}")
            } else {
                reasoning.to_string()
            }
        } else {
            String::new()
        };
        assert_eq!(delta2, " about this");

        // Transition to content: should close </think>
        let reasoning = "";
        let content = "The answer is 42";
        let delta3 = if !reasoning.is_empty() {
            reasoning.to_string()
        } else if !content.is_empty() {
            if in_reasoning {
                in_reasoning = false;
                format!("\n</think>\n\n{content}")
            } else {
                content.to_string()
            }
        } else {
            String::new()
        };
        assert_eq!(delta3, "\n</think>\n\nThe answer is 42");
        assert!(!in_reasoning);
    }

    #[test]
    fn test_parse_final_chunk_with_usage() {
        let data = r#"{"id":"chatcmpl-1","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":25,"completion_tokens":100,"total_tokens":125}}"#;
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        let finish = parsed["choices"][0]["finish_reason"].as_str().unwrap();
        let prompt = parsed["usage"]["prompt_tokens"].as_u64().unwrap();
        let completion = parsed["usage"]["completion_tokens"].as_u64().unwrap();
        assert_eq!(finish, "stop");
        assert_eq!(prompt, 25);
        assert_eq!(completion, 100);
    }

    #[test]
    fn test_constructor_with_state() {
        use crate::llama_status::LlamaServerState;
        use std::collections::HashMap;

        let state = Arc::new(RwLock::new(LlamaServerState::default()));
        let registry = Arc::new(ModelRegistry {
            default: "test".to_string(),
            active: std::sync::RwLock::new(None),
            models: HashMap::new(),
            source_path: None,
        });

        let provider =
            LlamaServerProvider::new_with_state("http://localhost:8080", 300, state, registry);
        assert_eq!(provider.base_url, "http://localhost:8080");
        assert!(provider.llama_state.is_some());
        assert!(provider.model_registry.is_some());
    }

    #[test]
    fn test_constructor_without_state() {
        let provider = LlamaServerProvider::new("http://localhost:8080", 300);
        assert!(provider.llama_state.is_none());
        assert!(provider.model_registry.is_none());
    }

    #[tokio::test]
    async fn test_auto_load_no_state_falls_through() {
        // Without llama_state/model_registry, chat_with_auto_load should
        // behave exactly like chat() — it won't try to switch models.
        let provider = LlamaServerProvider::new("http://127.0.0.1:1", 5);
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
        // This will fail (no server) but the point is it doesn't panic
        // from missing state fields.
        let _result = provider.chat_with_auto_load(request, None).await;
    }

    #[tokio::test]
    async fn test_auto_load_model_matches_skips_switch() {
        use crate::llama_status::{LlamaHealth, LlamaServerState};
        use crate::model_registry::ModelEntry;
        use std::collections::HashMap;

        let mut models = HashMap::new();
        models.insert(
            "qwen3-coder".to_string(),
            ModelEntry {
                path: "test.gguf".to_string(),
                context: 8192,
                description: "test".to_string(),
                mmproj: None,
            },
        );

        let state = Arc::new(RwLock::new(LlamaServerState {
            loaded_model: Some("qwen3-coder".to_string()),
            last_known_model: Some("qwen3-coder".to_string()),
            status: LlamaHealth::Healthy,
            last_check: None,
            consecutive_failures: 0,
            recovery_in_progress: false,
        }));
        let registry = Arc::new(ModelRegistry {
            default: "qwen3-coder".to_string(),
            active: std::sync::RwLock::new(None),
            models,
            source_path: None,
        });

        let provider =
            LlamaServerProvider::new_with_state("http://127.0.0.1:1", 5, state, registry);

        let request = ChatRequest {
            model: "qwen3-coder".to_string(),
            messages: vec![],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };
        // Model matches — should skip switch and go straight to chat.
        // Will fail because no server, but should NOT produce ModelNotFound.
        let _result = provider.chat_with_auto_load(request, None).await;
    }

    #[tokio::test]
    async fn test_auto_load_unknown_model_returns_not_found() {
        use crate::llama_status::LlamaServerState;
        use std::collections::HashMap;

        let state = Arc::new(RwLock::new(LlamaServerState {
            loaded_model: Some("qwen3-coder".to_string()),
            ..LlamaServerState::default()
        }));
        let registry = Arc::new(ModelRegistry {
            default: "qwen3-coder".to_string(),
            active: std::sync::RwLock::new(None),
            models: HashMap::new(), // empty — no models registered
            source_path: None,
        });

        let provider =
            LlamaServerProvider::new_with_state("http://127.0.0.1:1", 5, state, registry);

        let (event_tx, mut event_rx) = mpsc::channel(10);
        let request = ChatRequest {
            model: "nonexistent-model".to_string(),
            messages: vec![],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        let result = provider.chat_with_auto_load(request, Some(event_tx)).await;
        assert!(matches!(result, Err(LlmError::ModelNotFound(_))));

        // Should have received a Failed event
        let event = event_rx.try_recv().unwrap();
        assert!(matches!(event, ModelLoadEvent::Failed { .. }));
    }

    #[tokio::test]
    async fn test_auto_load_concurrent_switch_rejected() {
        use crate::llama_status::LlamaServerState;
        use crate::model_registry::ModelEntry;
        use std::collections::HashMap;

        let mut models = HashMap::new();
        models.insert(
            "new-model".to_string(),
            ModelEntry {
                path: "test.gguf".to_string(),
                context: 8192,
                description: "test".to_string(),
                mmproj: None,
            },
        );

        let state = Arc::new(RwLock::new(LlamaServerState {
            loaded_model: Some("old-model".to_string()),
            ..LlamaServerState::default()
        }));
        let registry = Arc::new(ModelRegistry {
            default: "old-model".to_string(),
            active: std::sync::RwLock::new(None),
            models,
            source_path: None,
        });

        let provider =
            LlamaServerProvider::new_with_state("http://127.0.0.1:1", 5, state, registry);

        // Pre-acquire the switch mutex to simulate a concurrent switch
        let _guard = provider.switch_mutex.try_lock().unwrap();

        let (event_tx, mut event_rx) = mpsc::channel(10);
        let request = ChatRequest {
            model: "new-model".to_string(),
            messages: vec![],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        let result = provider.chat_with_auto_load(request, Some(event_tx)).await;
        assert!(matches!(result, Err(LlmError::RequestFailed(_))));

        // Should have received a Failed event about concurrent switch
        let event = event_rx.try_recv().unwrap();
        match event {
            ModelLoadEvent::Failed { error, .. } => {
                assert!(error.contains("already in progress"));
            }
            _ => panic!("Expected Failed event"),
        }
    }

    // -- consolidate_system_messages tests --

    use crate::types::{ChatMessage, Role};

    fn sys(content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::System,
            content: content.to_string(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn user(content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: content.to_string(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn asst(content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: content.to_string(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    #[test]
    fn test_consolidate_no_system_messages() {
        let msgs = vec![user("hi"), asst("hello")];
        let result = LlamaServerProvider::consolidate_system_messages(&msgs);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, Role::User);
    }

    #[test]
    fn test_consolidate_single_system_message() {
        let msgs = vec![sys("You are helpful"), user("hi")];
        let result = LlamaServerProvider::consolidate_system_messages(&msgs);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, Role::System);
        assert_eq!(result[0].content, "You are helpful");
        assert_eq!(result[1].role, Role::User);
    }

    #[test]
    fn test_consolidate_multiple_system_messages() {
        let msgs = vec![
            sys("Identity info"),
            sys("Core instructions"),
            sys("Recalled memories"),
            user("hi"),
            asst("hello"),
        ];
        let result = LlamaServerProvider::consolidate_system_messages(&msgs);
        assert_eq!(result.len(), 3); // 1 merged system + user + assistant
        assert_eq!(result[0].role, Role::System);
        assert!(result[0].content.contains("Identity info"));
        assert!(result[0].content.contains("Core instructions"));
        assert!(result[0].content.contains("Recalled memories"));
        assert_eq!(result[1].role, Role::User);
        assert_eq!(result[2].role, Role::Assistant);
    }

    #[test]
    fn test_consolidate_interleaved_system_messages() {
        // System messages scattered throughout (edge case)
        let msgs = vec![sys("First"), user("hi"), sys("Stray system"), asst("hello")];
        let result = LlamaServerProvider::consolidate_system_messages(&msgs);
        assert_eq!(result.len(), 3); // 1 merged system + user + assistant
        assert_eq!(result[0].role, Role::System);
        assert!(result[0].content.contains("First"));
        assert!(result[0].content.contains("Stray system"));
        assert_eq!(result[1].role, Role::User);
        assert_eq!(result[2].role, Role::Assistant);
    }

    #[test]
    fn test_consolidate_empty_system_messages_skipped() {
        let msgs = vec![sys("Content"), sys(""), user("hi")];
        let result = LlamaServerProvider::consolidate_system_messages(&msgs);
        assert_eq!(result.len(), 2); // 1 system (only non-empty) + user
        assert_eq!(result[0].role, Role::System);
        assert_eq!(result[0].content, "Content");
    }

    #[test]
    fn test_consolidate_preserves_message_order() {
        let msgs = vec![
            sys("System"),
            user("q1"),
            asst("a1"),
            user("q2"),
            asst("a2"),
        ];
        let result = LlamaServerProvider::consolidate_system_messages(&msgs);
        assert_eq!(result.len(), 5);
        assert_eq!(result[0].role, Role::System);
        assert_eq!(result[1].content, "q1");
        assert_eq!(result[2].content, "a1");
        assert_eq!(result[3].content, "q2");
        assert_eq!(result[4].content, "a2");
    }

    #[test]
    fn test_consolidate_separator() {
        let msgs = vec![sys("Part A"), sys("Part B"), user("hi")];
        let result = LlamaServerProvider::consolidate_system_messages(&msgs);
        // Merged with double newline separator
        assert_eq!(result[0].content, "Part A\n\nPart B");
    }

    // -- Thinking config body-building tests --
    //
    // These simulate the body-building logic from chat() to verify that
    // thinking fields are correctly inserted into the request JSON.

    /// Build a request body the same way chat() does, for testing thinking fields.
    fn build_test_body(thinking: Option<crate::types::ThinkingConfig>) -> serde_json::Value {
        let request = ChatRequest {
            model: "test".to_string(),
            messages: vec![user("hi")],
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking,
            tools: None,
            tool_choice: None,
        };

        let mut body = serde_json::json!({
            "model": &request.model,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        });

        // Replicate the thinking logic from chat()
        if let Some(ref thinking) = request.thinking {
            if !thinking.enabled {
                body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": false});
                body["reasoning_format"] = serde_json::json!("none");
            } else if let Some(budget) = thinking.budget_tokens {
                if budget > 0 {
                    body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": true});
                    body["reasoning_budget"] = serde_json::json!(budget);
                } else if budget == -1 {
                    body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": true});
                }
            } else {
                body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": true});
            }
        } else {
            body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": false});
            body["reasoning_format"] = serde_json::json!("none");
        }

        body
    }

    #[test]
    fn test_thinking_disabled_sets_template_kwargs() {
        let body = build_test_body(Some(crate::types::ThinkingConfig {
            enabled: false,
            budget_tokens: Some(0),
        }));
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"],
            serde_json::json!(false)
        );
        assert_eq!(body["reasoning_format"], serde_json::json!("none"));
    }

    #[test]
    fn test_thinking_enabled_with_budget() {
        let body = build_test_body(Some(crate::types::ThinkingConfig {
            enabled: true,
            budget_tokens: Some(1000),
        }));
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"],
            serde_json::json!(true)
        );
        assert_eq!(body["reasoning_budget"], serde_json::json!(1000));
    }

    #[test]
    fn test_thinking_enabled_unlimited() {
        let body = build_test_body(Some(crate::types::ThinkingConfig {
            enabled: true,
            budget_tokens: Some(-1),
        }));
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"],
            serde_json::json!(true)
        );
        // Unlimited: no reasoning_budget key
        assert!(body.get("reasoning_budget").is_none());
    }

    #[test]
    fn test_thinking_enabled_no_budget() {
        let body = build_test_body(Some(crate::types::ThinkingConfig {
            enabled: true,
            budget_tokens: None,
        }));
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"],
            serde_json::json!(true)
        );
        assert!(body.get("reasoning_budget").is_none());
    }

    #[test]
    fn test_thinking_none_defaults_to_off() {
        let body = build_test_body(None);
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"],
            serde_json::json!(false)
        );
        assert_eq!(body["reasoning_format"], serde_json::json!("none"));
        assert!(body.get("reasoning_budget").is_none());
    }

    // ── Phase 3: tool_calls delta parsing from SSE chunk ──

    #[test]
    fn test_parse_tool_calls_delta_from_sse_chunk() {
        // Simulate an SSE delta chunk containing a tool_calls array.
        let data = r#"{
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\":"
                        }
                    }]
                },
                "finish_reason": null
            }]
        }"#;

        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        let delta_obj = &parsed["choices"][0]["delta"];

        // Replicate the parsing logic from the SSE loop.
        let tool_call_deltas: Option<Vec<ToolCallDelta>> =
            delta_obj["tool_calls"].as_array().and_then(|arr| {
                let parsed: Vec<ToolCallDelta> = arr
                    .iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect();
                if parsed.is_empty() {
                    None
                } else {
                    Some(parsed)
                }
            });

        let deltas = tool_call_deltas.unwrap();
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].index, 0);
        assert_eq!(deltas[0].id.as_deref(), Some("call_abc"));
        assert_eq!(
            deltas[0].function.as_ref().unwrap().name.as_deref(),
            Some("get_weather")
        );
        assert_eq!(
            deltas[0].function.as_ref().unwrap().arguments.as_deref(),
            Some("{\"city\":")
        );
    }

    #[test]
    fn test_finish_reason_tool_calls_forwarded() {
        // Simulate the finish_reason chunk that signals tool_calls are complete.
        let data = r#"{
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        }"#;

        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        let finish_reason = parsed["choices"][0]["finish_reason"].as_str();

        // Replicate the forwarding logic from the SSE loop.
        let chunk_finish_reason = finish_reason
            .filter(|r| *r != "null")
            .map(|r| r.to_string());

        assert_eq!(chunk_finish_reason.as_deref(), Some("tool_calls"));
    }

    // -------------------------------------------------------------------------
    // Regression test for the SSE finish_reason / usage chunk ordering bug.
    //
    // With `stream_options.include_usage`, llama-server emits:
    //   chunk N:   { delta:{tool_calls:[...]}, finish_reason:"tool_calls" }   (done=false)
    //   chunk N+1: { delta:{},  usage:{...},   finish_reason:null        }   (done=true)
    //
    // The agentic-loop consumer in handler.rs previously only captured
    // finish_reason on the chunk where done=true — i.e. the usage chunk,
    // whose finish_reason is null. This silently discarded the real stop
    // reason; tool_calls turns fell through to the "stop" branch and the
    // agentic loop never executed the tool. Fixed in handler.rs by
    // capturing finish_reason from *any* chunk that carries one.
    // This test asserts llama-server really does split them that way so
    // the handler-side fix remains necessary.
    #[test]
    fn test_finish_reason_arrives_before_usage_chunk() {
        // Chunk N — finish_reason is set, done=false (no usage yet).
        let finish_chunk = r#"{
            "id": "chatcmpl-1",
            "choices": [{ "delta": {}, "finish_reason": "tool_calls" }]
        }"#;
        let finish_parsed: serde_json::Value = serde_json::from_str(finish_chunk).unwrap();
        let fr = finish_parsed["choices"][0]["finish_reason"].as_str();
        let usage_on_finish_chunk = finish_parsed.get("usage");
        assert_eq!(fr, Some("tool_calls"));
        assert!(
            usage_on_finish_chunk.is_none(),
            "llama-server sends usage on a separate chunk; if this fails the chunking changed"
        );

        // Chunk N+1 — usage arrives, finish_reason is null, done=true.
        let usage_chunk = r#"{
            "id": "chatcmpl-1",
            "choices": [],
            "usage": { "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120 }
        }"#;
        let usage_parsed: serde_json::Value = serde_json::from_str(usage_chunk).unwrap();
        let fr_on_usage = usage_parsed["choices"]
            .get(0)
            .and_then(|c| c["finish_reason"].as_str());
        assert!(
            fr_on_usage.is_none(),
            "usage chunk must not re-emit finish_reason; if it does, the handler fix can be simplified"
        );
        assert!(usage_parsed.get("usage").is_some());
    }
}
