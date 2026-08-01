use crate::types::{ChatChunk, ChatRequest, LlmError, ModelInfo, Usage};
use reqwest::Client;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, warn};

pub struct OpenAiProvider {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    provider_name: String,
    extra_headers: HashMap<String, String>,
}

impl OpenAiProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        provider_name: impl Into<String>,
        timeout_secs: u64,
        extra_headers: HashMap<String, String>,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .tcp_nodelay(true)
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            provider_name: provider_name.into(),
            extra_headers,
        }
    }

    pub fn name(&self) -> &str {
        &self.provider_name
    }

    /// Inject extra headers (e.g. OpenRouter's HTTP-Referer, X-Title) into a
    /// request builder. Works regardless of which `Client` created the builder.
    fn apply_headers(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (name, value) in &self.extra_headers {
            req = req.header(name, value);
        }
        req
    }

    pub async fn health(&self) -> Result<bool, LlmError> {
        // No API key configured → provider is not usable
        if self.api_key.is_none() {
            debug!(provider = %self.provider_name, "No API key configured, marking unhealthy");
            return Ok(false);
        }

        let url = format!("{}/v1/models", self.base_url);
        let short_client = Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| LlmError::Unreachable(e.to_string()))?;

        let mut req = short_client.get(&url);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        req = self.apply_headers(req);

        match req.send().await {
            Ok(resp) => Ok(resp.status().is_success()),
            Err(e) => {
                warn!(error = %e, provider = %self.provider_name, "OpenAI-compat health check failed");
                Ok(false)
            }
        }
    }

    pub async fn models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        let url = format!("{}/v1/models", self.base_url);
        let mut req = self.client.get(&url);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        req = self.apply_headers(req);

        let resp = req
            .send()
            .await
            .map_err(|e| LlmError::Unreachable(e.to_string()))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LlmError::RequestFailed(e.to_string()))?;

        let models = body["data"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|m| {
                let id = m["id"].as_str()?.to_string();
                Some(ModelInfo {
                    id,
                    provider: self.provider_name.clone(),
                    size: None,
                    modified_at: m["created"].as_u64().map(|ts| ts.to_string()),
                })
            })
            .collect();

        Ok(models)
    }

    pub fn chat(&self, request: ChatRequest) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let model = request.model.clone();

        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|m| {
                // OpenAI multimodal format: content is array of parts
                if let Some(images) = &m.images {
                    if !images.is_empty() {
                        let mut content_parts = vec![serde_json::json!({
                            "type": "text",
                            "text": m.content,
                        })];
                        for b64 in images {
                            content_parts.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:image/jpeg;base64,{b64}"),
                                },
                            }));
                        }
                        return serde_json::json!({
                            "role": m.role,
                            "content": content_parts,
                        });
                    }
                }
                serde_json::json!({
                    "role": m.role,
                    "content": m.content,
                })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": &request.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });

        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
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

        let mut req = self.client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        // Apply extra headers inline (before the spawn, since self isn't moved)
        for (name, value) in &self.extra_headers {
            req = req.header(name, value);
        }

        let (tx, rx) = mpsc::channel(64);

        tokio::spawn(async move {
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    error!(error = %e, "OpenAI-compat chat request failed");
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
            };

            if !resp.status().is_success() {
                error!(status = %resp.status(), "OpenAI-compat returned error status");
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

            let mut stream = resp.bytes_stream();
            let mut buffer = String::new();

            use futures_util::StreamExt;
            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        error!(error = %e, "Error reading OpenAI-compat stream");
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

                    let delta = parsed["choices"][0]["delta"]["content"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();

                    // Usage comes in a separate chunk (with stream_options.include_usage)
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
                    let done = finish_reason.is_some() && finish_reason != Some("null");

                    let chat_chunk = ChatChunk {
                        delta,
                        done,
                        usage,
                        model: model.clone(),
                        error: None,
                        tool_call_deltas: None,
                        finish_reason: None,
                    };

                    if tx.send(chat_chunk).await.is_err() {
                        debug!("Receiver dropped, stopping OpenAI-compat stream");
                        return;
                    }
                }
            }
        });

        Ok(ReceiverStream::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constructor() {
        let provider = OpenAiProvider::new(
            "http://localhost:8080",
            Some("sk-test".into()),
            "openai",
            300,
            HashMap::new(),
        );
        assert_eq!(provider.base_url, "http://localhost:8080");
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn test_constructor_with_extra_headers() {
        let mut headers = HashMap::new();
        headers.insert(
            "HTTP-Referer".to_string(),
            "https://ironjaw.local".to_string(),
        );
        headers.insert("X-Title".to_string(), "Ironjaw".to_string());
        let provider = OpenAiProvider::new(
            "https://openrouter.ai/api/v1",
            Some("sk-or-test".into()),
            "openrouter",
            120,
            headers,
        );
        assert_eq!(provider.name(), "openrouter");
        assert_eq!(provider.extra_headers.len(), 2);
    }

    #[test]
    fn test_parse_sse_chunk() {
        let data =
            r#"{"id":"chatcmpl-1","choices":[{"delta":{"content":"Hi"},"finish_reason":null}]}"#;
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        let delta = parsed["choices"][0]["delta"]["content"].as_str().unwrap();
        let finish = parsed["choices"][0]["finish_reason"].as_str();
        assert_eq!(delta, "Hi");
        assert!(finish.is_none()); // null is not a string
    }

    #[test]
    fn test_parse_final_chunk_with_usage() {
        let data = r#"{"id":"chatcmpl-1","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":15,"completion_tokens":50,"total_tokens":65}}"#;
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        let finish = parsed["choices"][0]["finish_reason"].as_str().unwrap();
        let prompt = parsed["usage"]["prompt_tokens"].as_u64().unwrap();
        let completion = parsed["usage"]["completion_tokens"].as_u64().unwrap();
        assert_eq!(finish, "stop");
        assert_eq!(prompt, 15);
        assert_eq!(completion, 50);
    }
}
