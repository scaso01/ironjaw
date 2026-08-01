use crate::provider::Provider;
use crate::types::{ChatChunk, ChatRequest, LlmError, ModelInfo};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn};

/// Classifies an LlmError for retry/failover decisions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Rate limited (429) -- exponential backoff then retry
    RateLimit,
    /// Auth failure (401/403) -- skip provider
    Auth,
    /// Server error (5xx) -- retry with backoff
    Server,
    /// Network error -- failover to next provider
    Network,
    /// Other -- don't retry
    Other,
}

impl ErrorClass {
    pub fn classify(error: &LlmError) -> Self {
        match error {
            LlmError::RateLimited { .. } => ErrorClass::RateLimit,
            LlmError::AuthFailed(_) => ErrorClass::Auth,
            LlmError::ServerError(_) => ErrorClass::Server,
            LlmError::Unreachable(_) => ErrorClass::Network,
            LlmError::AllProvidersFailed => ErrorClass::Network,
            _ => ErrorClass::Other,
        }
    }

    pub fn should_retry(&self) -> bool {
        matches!(self, ErrorClass::RateLimit | ErrorClass::Server)
    }

    pub fn should_failover(&self) -> bool {
        matches!(self, ErrorClass::Network | ErrorClass::Auth)
    }
}

/// Wraps a provider with retry logic
pub struct RetryProvider {
    inner: Provider,
    max_retries: u32,
    base_delay_ms: u64,
}

impl RetryProvider {
    pub fn new(inner: Provider, max_retries: u32, base_delay_ms: u64) -> Self {
        Self {
            inner,
            max_retries,
            base_delay_ms,
        }
    }

    pub fn name(&self) -> &str {
        self.inner.name()
    }

    pub async fn health(&self) -> Result<bool, LlmError> {
        self.inner.health().await
    }

    pub async fn models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        self.inner.models().await
    }

    pub fn chat(&self, request: ChatRequest) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        // Retry logic is best applied at the router level since chat() returns a stream.
        // For single-shot calls, we retry at the initial connection level.
        self.inner.chat(request)
    }

    /// Execute a chat with retry logic (for non-streaming or initial connection)
    pub async fn chat_with_retry(
        &self,
        request: ChatRequest,
    ) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        let mut last_error = None;
        for attempt in 0..=self.max_retries {
            match self.inner.chat(request.clone()) {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    let class = ErrorClass::classify(&e);
                    if !class.should_retry() || attempt == self.max_retries {
                        return Err(e);
                    }
                    let delay = self.base_delay_ms * 2u64.pow(attempt);
                    warn!(
                        provider = self.inner.name(),
                        attempt = attempt + 1,
                        max = self.max_retries,
                        delay_ms = delay,
                        error = %e,
                        "Retrying after error"
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    last_error = Some(e);
                }
            }
        }
        Err(last_error.unwrap_or(LlmError::AllProvidersFailed))
    }
}

/// Wraps multiple providers with failover logic
pub struct FailoverProvider {
    providers: Vec<Provider>,
    consecutive_failures: Vec<AtomicU64>,
    /// After this many consecutive failures, skip the provider temporarily
    failure_threshold: u64,
}

impl FailoverProvider {
    pub fn new(providers: Vec<Provider>, failure_threshold: u64) -> Self {
        let len = providers.len();
        Self {
            providers,
            consecutive_failures: (0..len).map(|_| AtomicU64::new(0)).collect(),
            failure_threshold,
        }
    }

    /// Try each provider in order, skipping failed ones
    pub async fn chat(&self, request: ChatRequest) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        if self.providers.is_empty() {
            return Err(LlmError::NoProviders);
        }

        for (i, provider) in self.providers.iter().enumerate() {
            let failures = self.consecutive_failures[i].load(Ordering::Relaxed);
            if failures >= self.failure_threshold {
                warn!(
                    provider = provider.name(),
                    failures,
                    threshold = self.failure_threshold,
                    "Provider temporarily skipped due to consecutive failures"
                );
                continue;
            }

            match provider.health().await {
                Ok(true) => match provider.chat(request.clone()) {
                    Ok(stream) => {
                        self.consecutive_failures[i].store(0, Ordering::Relaxed);
                        info!(provider = provider.name(), "Failover: using provider");
                        return Ok(stream);
                    }
                    Err(e) => {
                        self.consecutive_failures[i].fetch_add(1, Ordering::Relaxed);
                        let class = ErrorClass::classify(&e);
                        if class.should_failover() {
                            warn!(
                                provider = provider.name(),
                                error = %e,
                                "Provider failed, trying next"
                            );
                            continue;
                        }
                        return Err(e);
                    }
                },
                _ => {
                    self.consecutive_failures[i].fetch_add(1, Ordering::Relaxed);
                    warn!(
                        provider = provider.name(),
                        "Provider unhealthy in failover chain"
                    );
                    continue;
                }
            }
        }

        Err(LlmError::AllProvidersFailed)
    }

    /// Reset failure count for a provider (e.g., after recovery)
    pub fn reset_failures(&self, index: usize) {
        if index < self.consecutive_failures.len() {
            self.consecutive_failures[index].store(0, Ordering::Relaxed);
        }
    }

    /// Reset all failure counts
    pub fn reset_all_failures(&self) {
        for counter in &self.consecutive_failures {
            counter.store(0, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::types::{ChatMessage, Role};
    use std::sync::atomic::Ordering;

    fn simple_request() -> ChatRequest {
        ChatRequest {
            model: "test".to_string(),
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
        }
    }

    #[test]
    fn test_error_classification() {
        assert_eq!(
            ErrorClass::classify(&LlmError::RateLimited {
                retry_after_secs: 5
            }),
            ErrorClass::RateLimit
        );
        assert_eq!(
            ErrorClass::classify(&LlmError::AuthFailed("bad key".to_string())),
            ErrorClass::Auth
        );
        assert_eq!(
            ErrorClass::classify(&LlmError::ServerError("500".to_string())),
            ErrorClass::Server
        );
        assert_eq!(
            ErrorClass::classify(&LlmError::Unreachable("timeout".to_string())),
            ErrorClass::Network
        );
        assert_eq!(
            ErrorClass::classify(&LlmError::ModelNotFound("x".to_string())),
            ErrorClass::Other
        );
    }

    #[test]
    fn test_retry_decisions() {
        assert!(ErrorClass::RateLimit.should_retry());
        assert!(ErrorClass::Server.should_retry());
        assert!(!ErrorClass::Auth.should_retry());
        assert!(!ErrorClass::Network.should_retry());
        assert!(!ErrorClass::Other.should_retry());
    }

    #[test]
    fn test_failover_decisions() {
        assert!(ErrorClass::Network.should_failover());
        assert!(ErrorClass::Auth.should_failover());
        assert!(!ErrorClass::RateLimit.should_failover());
        assert!(!ErrorClass::Server.should_failover());
        assert!(!ErrorClass::Other.should_failover());
    }

    #[test]
    fn test_retry_provider_delegates_name() {
        let mock = Provider::Mock(MockProvider::new("ok"));
        let retry = RetryProvider::new(mock, 3, 100);
        assert_eq!(retry.name(), "mock");
    }

    #[tokio::test]
    async fn test_retry_provider_success() {
        let mock = Provider::Mock(MockProvider::new("response"));
        let retry = RetryProvider::new(mock, 3, 100);
        let result = retry.chat(simple_request());
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_failover_with_single_provider() {
        let providers = vec![Provider::Mock(MockProvider::new("response"))];
        let failover = FailoverProvider::new(providers, 3);
        let result = failover.chat(simple_request()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_failover_empty_providers() {
        let failover = FailoverProvider::new(vec![], 3);
        let result = failover.chat(simple_request()).await;
        assert!(matches!(result, Err(LlmError::NoProviders)));
    }

    #[tokio::test]
    async fn test_failover_reset_failures() {
        let providers = vec![Provider::Mock(MockProvider::new("ok"))];
        let failover = FailoverProvider::new(providers, 3);
        failover.consecutive_failures[0].store(5, Ordering::Relaxed);
        failover.reset_failures(0);
        assert_eq!(failover.consecutive_failures[0].load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_failover_reset_all() {
        let providers = vec![
            Provider::Mock(MockProvider::new("ok")),
            Provider::Mock(MockProvider::new("ok2")),
        ];
        let failover = FailoverProvider::new(providers, 3);
        failover.consecutive_failures[0].store(5, Ordering::Relaxed);
        failover.consecutive_failures[1].store(3, Ordering::Relaxed);
        failover.reset_all_failures();
        assert_eq!(failover.consecutive_failures[0].load(Ordering::Relaxed), 0);
        assert_eq!(failover.consecutive_failures[1].load(Ordering::Relaxed), 0);
    }
}
