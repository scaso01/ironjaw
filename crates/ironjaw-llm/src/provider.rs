use crate::anthropic::AnthropicProvider;
use crate::llama_server::LlamaServerProvider;
use crate::openai::OpenAiProvider;
use crate::types::{ChatChunk, ChatRequest, LlmError, ModelInfo, ModelLoadEvent, Usage};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

pub enum Provider {
    LlamaServer(LlamaServerProvider),
    OpenAi(OpenAiProvider),
    Anthropic(AnthropicProvider),
    Mock(MockProvider),
}

/// A fake LLM provider that returns a fixed response. For testing only.
pub struct MockProvider {
    response: String,
    scripted_chunks: Option<Vec<ChatChunk>>,
    /// Test-only: per-call scripts. Call N pops scripts[N]; once only one
    /// script remains it is replayed for every further call. Lets a test give
    /// the agentic loop a DIFFERENT reply on retry (stream error → success).
    scripted_sequence: Option<Arc<Mutex<std::collections::VecDeque<Vec<ChatChunk>>>>>,
    /// Test-only: when set, every `ChatRequest` passed to `chat()` is appended
    /// here so tests can inspect exactly what was sent on each iteration
    /// (e.g. asserting an injected system note appears on the last request).
    recorded_requests: Option<Arc<Mutex<Vec<ChatRequest>>>>,
    /// Test-only: when set, pause this long before sending each scripted
    /// chunk. Gives a test a deterministic window to drop and reconnect a
    /// WebSocket mid-stream instead of racing an instantaneous replay.
    chunk_delay: Option<std::time::Duration>,
}

impl MockProvider {
    pub fn new(response: &str) -> Self {
        Self {
            response: response.to_string(),
            scripted_chunks: None,
            scripted_sequence: None,
            recorded_requests: None,
            chunk_delay: None,
        }
    }

    /// Create a MockProvider that replays a specific sequence of `ChatChunk`s.
    /// Useful for testing tool_call_deltas or multi-chunk streaming.
    pub fn with_scripted_chunks(chunks: Vec<ChatChunk>) -> Self {
        Self {
            response: String::new(),
            scripted_chunks: Some(chunks),
            scripted_sequence: None,
            recorded_requests: None,
            chunk_delay: None,
        }
    }

    /// Like [`Self::with_scripted_chunks`], but also records every incoming
    /// `ChatRequest` into `recorder` so a test can inspect the exact messages
    /// sent on each loop iteration after the run completes.
    pub fn with_scripted_chunks_recording(
        chunks: Vec<ChatChunk>,
        recorder: Arc<Mutex<Vec<ChatRequest>>>,
    ) -> Self {
        Self {
            response: String::new(),
            scripted_chunks: Some(chunks),
            scripted_sequence: None,
            recorded_requests: Some(recorder),
            chunk_delay: None,
        }
    }

    /// Like [`Self::with_scripted_chunks`], but waits `delay` before sending
    /// each chunk. Used by mid-stream-reconnect tests that need time to drop
    /// and reopen a WebSocket between two scripted chunks.
    pub fn with_scripted_chunks_delayed(
        chunks: Vec<ChatChunk>,
        delay: std::time::Duration,
    ) -> Self {
        Self {
            response: String::new(),
            scripted_chunks: Some(chunks),
            scripted_sequence: None,
            recorded_requests: None,
            chunk_delay: Some(delay),
        }
    }

    /// Per-call scripts: the first `.chat()` call replays `scripts[0]`, the
    /// second `scripts[1]`, and so on; once a single script remains it is
    /// replayed for all further calls. Lets a test script a retry that
    /// behaves differently from the first attempt.
    pub fn with_scripted_sequence(scripts: Vec<Vec<ChatChunk>>) -> Self {
        Self {
            response: String::new(),
            scripted_chunks: None,
            scripted_sequence: Some(Arc::new(Mutex::new(scripts.into_iter().collect()))),
            recorded_requests: None,
            chunk_delay: None,
        }
    }

    pub fn name(&self) -> &str {
        "mock"
    }

    pub async fn health(&self) -> Result<bool, LlmError> {
        Ok(true)
    }

    pub async fn models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        Ok(vec![ModelInfo {
            id: "mock-model".to_string(),
            provider: "mock".to_string(),
            size: None,
            modified_at: None,
        }])
    }

    pub fn chat(&self, request: ChatRequest) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        if let Some(recorder) = &self.recorded_requests {
            recorder.lock().unwrap().push(request);
        }
        if let Some(seq) = &self.scripted_sequence {
            let chunks = {
                let mut q = seq.lock().unwrap();
                if q.len() > 1 {
                    q.pop_front().unwrap()
                } else {
                    q.front().cloned().unwrap_or_default()
                }
            };
            let (tx, rx) = tokio::sync::mpsc::channel(chunks.len().max(1));
            tokio::spawn(async move {
                for chunk in chunks {
                    let _ = tx.send(chunk).await;
                }
            });
            return Ok(ReceiverStream::new(rx));
        }
        if let Some(chunks) = self.scripted_chunks.clone() {
            let (tx, rx) = tokio::sync::mpsc::channel(chunks.len().max(1));
            let delay = self.chunk_delay;
            tokio::spawn(async move {
                for chunk in chunks {
                    if let Some(d) = delay {
                        tokio::time::sleep(d).await;
                    }
                    let _ = tx.send(chunk).await;
                }
            });
            return Ok(ReceiverStream::new(rx));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let response = self.response.clone();
        tokio::spawn(async move {
            let _ = tx
                .send(ChatChunk {
                    delta: response,
                    done: true,
                    usage: Some(Usage {
                        prompt_tokens: 10,
                        completion_tokens: 5,
                        total_tokens: 15,
                    }),
                    model: "mock-model".to_string(),
                    error: None,
                    tool_call_deltas: None,
                    finish_reason: None,
                })
                .await;
        });
        Ok(ReceiverStream::new(rx))
    }
}

impl Provider {
    pub fn name(&self) -> &str {
        match self {
            Provider::LlamaServer(p) => p.name(),
            Provider::OpenAi(p) => p.name(),
            Provider::Anthropic(p) => p.name(),
            Provider::Mock(p) => p.name(),
        }
    }

    pub async fn health(&self) -> Result<bool, LlmError> {
        match self {
            Provider::LlamaServer(p) => p.health().await,
            Provider::OpenAi(p) => p.health().await,
            Provider::Anthropic(p) => p.health().await,
            Provider::Mock(p) => p.health().await,
        }
    }

    pub async fn models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        match self {
            Provider::LlamaServer(p) => p.models().await,
            Provider::OpenAi(p) => p.models().await,
            Provider::Anthropic(p) => p.models().await,
            Provider::Mock(p) => p.models().await,
        }
    }

    pub fn chat(&self, request: ChatRequest) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        match self {
            Provider::LlamaServer(p) => p.chat(request),
            Provider::OpenAi(p) => p.chat(request),
            Provider::Anthropic(p) => p.chat(request),
            Provider::Mock(p) => p.chat(request),
        }
    }

    /// Chat with automatic model loading for providers that support it.
    /// For llama-server: checks loaded model, switches if needed, then chats.
    /// For other providers: falls through to regular chat().
    pub async fn chat_with_auto_load(
        &self,
        request: ChatRequest,
        event_tx: Option<mpsc::Sender<ModelLoadEvent>>,
    ) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        match self {
            Provider::LlamaServer(p) => p.chat_with_auto_load(request, event_tx).await,
            Provider::OpenAi(p) => p.chat(request),
            Provider::Anthropic(p) => p.chat(request),
            Provider::Mock(p) => p.chat(request),
        }
    }
}
