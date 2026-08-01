use crate::complexity::{ComplexityLevel, ComplexityScorer};
use crate::provider::Provider;
use crate::routing_mode::{
    PlanOutput, RoutingMode, SkillRoutingConfig, EXECUTOR_SYSTEM_PREFIX, PLANNER_SYSTEM_PROMPT,
};
use crate::types::{
    ChatChunk, ChatMessage, ChatRequest, LlmError, ModelInfo, ModelLoadEvent, Role,
};
use crate::verification::{self, VerificationConfig, VerificationResult};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tracing::{info, warn};

/// Model overrides for complexity-based routing.
///
/// Kept in the LLM crate (not imported from ironjaw-core) to avoid
/// circular dependencies, since ironjaw-core depends on ironjaw-llm.
#[derive(Debug, Clone, Default)]
pub struct RoutingOverrides {
    pub simple_model: Option<String>,
    pub medium_model: Option<String>,
    pub complex_model: Option<String>,
}

pub struct LlmRouter {
    providers: Vec<Provider>,
    /// G3: interior-mutable so a verified `model.switch` (or the
    /// llama-models.json watcher) can retarget the default without a restart.
    default_model: std::sync::RwLock<Option<String>>,
    scorer: ComplexityScorer,
    routing: Option<RoutingOverrides>,
    /// Maps provider name (e.g. "llama-server", "anthropic") to index in providers vec.
    /// Used for prefix-based routing: "anthropic/claude-sonnet-4-6" routes to the anthropic provider.
    provider_index: HashMap<String, usize>,
    /// Per-skill routing configuration for architect+editor split.
    skill_routing: SkillRoutingConfig,
    /// Configuration for the independent verification stage.
    verification: VerificationConfig,
}

impl LlmRouter {
    pub fn new(
        providers: Vec<Provider>,
        default_model: Option<String>,
        routing: Option<RoutingOverrides>,
    ) -> Self {
        let provider_index: HashMap<String, usize> = providers
            .iter()
            .enumerate()
            .map(|(i, p)| (p.name().to_string(), i))
            .collect();
        info!(
            provider_count = providers.len(),
            default_model = ?default_model,
            routing_enabled = routing.is_some(),
            provider_names = ?provider_index.keys().collect::<Vec<_>>(),
            "LLM router ready"
        );
        Self {
            providers,
            default_model: std::sync::RwLock::new(default_model),
            scorer: ComplexityScorer::new(),
            routing,
            provider_index,
            skill_routing: SkillRoutingConfig::default(),
            verification: VerificationConfig::default(),
        }
    }

    /// Create a router with skill routing and verification configuration.
    pub fn with_config(
        providers: Vec<Provider>,
        default_model: Option<String>,
        routing: Option<RoutingOverrides>,
        skill_routing: SkillRoutingConfig,
        verification: VerificationConfig,
    ) -> Self {
        let mut router = Self::new(providers, default_model, routing);
        router.skill_routing = skill_routing;
        router.verification = verification;
        router
    }

    /// Create an empty router (for tests where no LLM is needed)
    pub fn empty() -> Self {
        Self {
            providers: Vec::new(),
            default_model: std::sync::RwLock::new(None),
            scorer: ComplexityScorer::new(),
            routing: None,
            provider_index: HashMap::new(),
            skill_routing: SkillRoutingConfig::default(),
            verification: VerificationConfig::default(),
        }
    }

    /// Create a router with a mock provider that returns a fixed response.
    /// For integration tests that need end-to-end chat.send -> persist -> chat.history.
    pub fn mock(response: &str) -> Self {
        use crate::provider::MockProvider;
        let providers = vec![Provider::Mock(MockProvider::new(response))];
        let provider_index: HashMap<String, usize> = providers
            .iter()
            .enumerate()
            .map(|(i, p)| (p.name().to_string(), i))
            .collect();
        Self {
            providers,
            default_model: std::sync::RwLock::new(Some("mock-model".to_string())),
            scorer: ComplexityScorer::new(),
            routing: None,
            provider_index,
            skill_routing: SkillRoutingConfig::default(),
            verification: VerificationConfig::default(),
        }
    }

    /// Create a router with a mock provider that replays a scripted chunk sequence.
    /// The same sequence is replayed on every `.chat()` call (not stateful between calls).
    /// Useful for testing streaming paths like tool-call deltas.
    pub fn mock_scripted(chunks: Vec<crate::types::ChatChunk>) -> Self {
        use crate::provider::MockProvider;
        let providers = vec![Provider::Mock(MockProvider::with_scripted_chunks(chunks))];
        let provider_index: HashMap<String, usize> = providers
            .iter()
            .enumerate()
            .map(|(i, p)| (p.name().to_string(), i))
            .collect();
        Self {
            providers,
            default_model: std::sync::RwLock::new(Some("mock-model".to_string())),
            scorer: ComplexityScorer::new(),
            routing: None,
            provider_index,
            skill_routing: SkillRoutingConfig::default(),
            verification: VerificationConfig::default(),
        }
    }

    /// Like [`Self::mock_scripted`], but with PER-CALL scripts: call N replays
    /// `scripts[N]`, and once one script remains it repeats for all further
    /// calls. Test-only: lets the agentic loop see a different reply on retry.
    #[doc(hidden)]
    pub fn mock_scripted_sequence(scripts: Vec<Vec<crate::types::ChatChunk>>) -> Self {
        use crate::provider::MockProvider;
        let providers = vec![Provider::Mock(MockProvider::with_scripted_sequence(
            scripts,
        ))];
        let provider_index: HashMap<String, usize> = providers
            .iter()
            .enumerate()
            .map(|(i, p)| (p.name().to_string(), i))
            .collect();
        Self {
            providers,
            default_model: std::sync::RwLock::new(Some("mock-model".to_string())),
            scorer: ComplexityScorer::new(),
            routing: None,
            provider_index,
            skill_routing: SkillRoutingConfig::default(),
            verification: VerificationConfig::default(),
        }
    }

    /// Like [`Self::mock_scripted`], but records every `ChatRequest` sent to
    /// the mock provider into `recorder`. Test-only: lets a test drive the
    /// agentic loop to completion and then inspect exactly what was sent on
    /// each iteration (e.g. asserting an injected budget note).
    pub fn mock_scripted_recording(
        chunks: Vec<crate::types::ChatChunk>,
        recorder: std::sync::Arc<std::sync::Mutex<Vec<ChatRequest>>>,
    ) -> Self {
        use crate::provider::MockProvider;
        let providers = vec![Provider::Mock(
            MockProvider::with_scripted_chunks_recording(chunks, recorder),
        )];
        let provider_index: HashMap<String, usize> = providers
            .iter()
            .enumerate()
            .map(|(i, p)| (p.name().to_string(), i))
            .collect();
        Self {
            providers,
            default_model: std::sync::RwLock::new(Some("mock-model".to_string())),
            scorer: ComplexityScorer::new(),
            routing: None,
            provider_index,
            skill_routing: SkillRoutingConfig::default(),
            verification: VerificationConfig::default(),
        }
    }

    /// Like [`Self::mock_scripted`], but waits `delay` before sending each
    /// chunk. Used by mid-stream-reconnect tests that need a deterministic
    /// window to drop and reopen a WebSocket between two scripted chunks.
    pub fn mock_scripted_delayed(
        chunks: Vec<crate::types::ChatChunk>,
        delay: std::time::Duration,
    ) -> Self {
        use crate::provider::MockProvider;
        let providers = vec![Provider::Mock(MockProvider::with_scripted_chunks_delayed(
            chunks, delay,
        ))];
        let provider_index: HashMap<String, usize> = providers
            .iter()
            .enumerate()
            .map(|(i, p)| (p.name().to_string(), i))
            .collect();
        Self {
            providers,
            default_model: std::sync::RwLock::new(Some("mock-model".to_string())),
            scorer: ComplexityScorer::new(),
            routing: None,
            provider_index,
            skill_routing: SkillRoutingConfig::default(),
            verification: VerificationConfig::default(),
        }
    }

    pub fn has_providers(&self) -> bool {
        !self.providers.is_empty()
    }

    /// Check if a specific provider is registered by its configured name.
    /// Uses the provider's own `name()` method, so custom names like
    /// "openrouter" (type=openai) are matched correctly.
    pub fn has_provider(&self, name: &str) -> bool {
        self.providers.iter().any(|p| p.name() == name)
    }

    pub fn default_model(&self) -> Option<String> {
        self.default_model.read().unwrap().clone()
    }

    /// G3: retarget the router's default model at runtime. Called by the
    /// `model.switch` handler after a verified switch and by the
    /// llama-models.json watcher on external edits.
    pub fn set_default_model(&self, model: Option<String>) {
        *self.default_model.write().unwrap() = model;
    }

    /// Return true if `model` will be served by a local llama-server provider.
    ///
    /// Mirrors the routing in [`Self::chat`]: an explicit `provider/model`
    /// prefix routes to that provider, so we're local only if that provider is
    /// the `LlamaServer` variant. A bare name (e.g. `qwen3.6:35b-a3b`) — or a
    /// slash-name whose prefix matches no provider — falls through to the local
    /// fallback, so it is local iff a llama-server provider is registered.
    ///
    /// This is the correct gate for local-only behaviors (base prompt +
    /// curated tool set): the phone resolves its model via `default_model()` to
    /// a BARE name, which a `starts_with("llama-server/")` check misses.
    pub fn is_llama_server_model(&self, model: &str) -> bool {
        if let Some((idx, _)) = self.route_by_prefix(model) {
            return matches!(self.providers.get(idx), Some(Provider::LlamaServer(_)));
        }
        self.providers
            .iter()
            .any(|p| matches!(p, Provider::LlamaServer(_)))
    }

    /// Resolve the model to use based on complexity routing.
    ///
    /// If routing overrides are configured, scores the request complexity
    /// and selects the corresponding model. Falls back to the request's
    /// model field, then to the default model.
    fn resolve_model(&self, request: &ChatRequest) -> String {
        if let Some(routing) = &self.routing {
            let level = self.scorer.score(request);
            let routed = match level {
                ComplexityLevel::Simple => routing.simple_model.as_deref(),
                ComplexityLevel::Medium => routing.medium_model.as_deref(),
                ComplexityLevel::Complex => routing.complex_model.as_deref(),
            };
            if let Some(model) = routed {
                info!(?level, model, "Complexity routing selected model");
                return model.to_string();
            }
        }
        // Fall back to request model or default
        if !request.model.is_empty() {
            request.model.clone()
        } else {
            self.default_model
                .read()
                .unwrap()
                .clone()
                .unwrap_or_default()
        }
    }

    /// Try to route a model name with a provider prefix (e.g. "anthropic/claude-sonnet-4-6").
    /// Returns (provider index, bare model name) if prefix matches a registered provider.
    fn route_by_prefix(&self, model: &str) -> Option<(usize, String)> {
        let (prefix, bare) = model.split_once('/')?;
        let &idx = self.provider_index.get(prefix)?;
        Some((idx, bare.to_string()))
    }

    /// Aggregate models from all providers
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        let mut all_models = Vec::new();
        for provider in &self.providers {
            match provider.models().await {
                Ok(models) => all_models.extend(models),
                Err(e) => {
                    warn!(
                        provider = provider.name(),
                        error = %e,
                        "Failed to list models from provider"
                    );
                }
            }
        }
        Ok(all_models)
    }

    /// Try providers in order, health-check each, return stream from first healthy one.
    ///
    /// Routing order:
    /// 1. If model has a provider prefix (e.g. "anthropic/claude-sonnet-4-6"), route directly
    ///    to that provider. If the provider is unhealthy, fail immediately — do NOT fall through.
    /// 2. Otherwise, apply complexity-based routing and try providers in order.
    pub async fn chat(
        &self,
        mut request: ChatRequest,
    ) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        if self.providers.is_empty() {
            return Err(LlmError::NoProviders);
        }

        // 1. Prefix-based routing: "provider/model" → direct dispatch
        if let Some((idx, bare_model)) = self.route_by_prefix(&request.model) {
            let provider = &self.providers[idx];
            info!(
                provider = provider.name(),
                model = %bare_model,
                "Prefix routing to provider"
            );
            request.model = bare_model;
            match provider.health().await {
                Ok(true) => return provider.chat(request),
                Ok(false) => {
                    return Err(LlmError::Unreachable(format!(
                        "provider '{}' is unhealthy",
                        provider.name()
                    )));
                }
                Err(e) => {
                    return Err(LlmError::Unreachable(format!(
                        "provider '{}' health check failed: {}",
                        provider.name(),
                        e
                    )));
                }
            }
        }

        // 2. Apply complexity-based model routing
        request.model = self.resolve_model(&request);

        // 3. Fallback: first healthy provider
        for provider in &self.providers {
            match provider.health().await {
                Ok(true) => {
                    info!(provider = provider.name(), "Using provider for chat");
                    return provider.chat(request);
                }
                Ok(false) => {
                    warn!(provider = provider.name(), "Provider unhealthy, skipping");
                }
                Err(e) => {
                    warn!(
                        provider = provider.name(),
                        error = %e,
                        "Provider health check error, skipping"
                    );
                }
            }
        }

        Err(LlmError::AllProvidersFailed)
    }

    /// Like `chat()` but with model auto-loading support.
    ///
    /// For llama-server providers, if the requested model differs from what's
    /// currently loaded, triggers a model switch before inference. Progress
    /// events are sent via `event_tx` for the handler to forward to clients.
    pub async fn chat_with_events(
        &self,
        mut request: ChatRequest,
        event_tx: Option<mpsc::Sender<ModelLoadEvent>>,
    ) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        if self.providers.is_empty() {
            return Err(LlmError::NoProviders);
        }

        // 1. Prefix-based routing: "provider/model" → direct dispatch
        if let Some((idx, bare_model)) = self.route_by_prefix(&request.model) {
            let provider = &self.providers[idx];
            info!(
                provider = provider.name(),
                model = %bare_model,
                "Prefix routing to provider (with events)"
            );
            request.model = bare_model;
            match provider.health().await {
                Ok(true) => return provider.chat_with_auto_load(request, event_tx).await,
                Ok(false) => {
                    return Err(LlmError::Unreachable(format!(
                        "provider '{}' is unhealthy",
                        provider.name()
                    )));
                }
                Err(e) => {
                    return Err(LlmError::Unreachable(format!(
                        "provider '{}' health check failed: {}",
                        provider.name(),
                        e
                    )));
                }
            }
        }

        // 2. Apply complexity-based model routing
        request.model = self.resolve_model(&request);

        // 3. Fallback: first healthy provider (with auto-load)
        for provider in &self.providers {
            match provider.health().await {
                Ok(true) => {
                    info!(
                        provider = provider.name(),
                        "Using provider for chat (with events)"
                    );
                    return provider.chat_with_auto_load(request, event_tx).await;
                }
                Ok(false) => {
                    warn!(provider = provider.name(), "Provider unhealthy, skipping");
                }
                Err(e) => {
                    warn!(
                        provider = provider.name(),
                        error = %e,
                        "Provider health check error, skipping"
                    );
                }
            }
        }

        Err(LlmError::AllProvidersFailed)
    }

    /// Get the routing mode for a skill, consulting the skill routing config.
    pub fn routing_mode_for(&self, skill_name: Option<&str>) -> RoutingMode {
        match skill_name {
            Some(name) => self.skill_routing.routing_mode_for(name),
            None => RoutingMode::Complexity,
        }
    }

    /// Get a reference to the skill routing config.
    pub fn skill_routing(&self) -> &SkillRoutingConfig {
        &self.skill_routing
    }

    /// Get a reference to the verification config.
    pub fn verification_config(&self) -> &VerificationConfig {
        &self.verification
    }

    /// Execute the planner phase: send the conversation to the planner model
    /// and collect the full plan as a non-streaming response.
    ///
    /// Returns the plan text and the model that produced it.
    pub async fn run_planner(
        &self,
        messages: &[ChatMessage],
        planner_model: &str,
    ) -> Result<PlanOutput, LlmError> {
        let mut planner_messages = vec![ChatMessage {
            role: Role::System,
            content: PLANNER_SYSTEM_PROMPT.to_string(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        planner_messages.extend_from_slice(messages);

        let request = ChatRequest {
            model: planner_model.to_string(),
            messages: planner_messages,
            temperature: Some(0.3), // Low temperature for deterministic planning
            max_tokens: Some(1024),
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        let mut stream = self.chat(request).await?;
        let mut plan_text = String::new();
        while let Some(chunk) = stream.next().await {
            plan_text.push_str(&chunk.delta);
        }

        info!(
            planner_model,
            plan_length = plan_text.len(),
            "Planner phase complete"
        );

        Ok(PlanOutput {
            plan: plan_text,
            planner_model: planner_model.to_string(),
        })
    }

    /// Execute the executor phase: send the plan + conversation context to the
    /// executor model and return a streaming response.
    pub async fn run_executor(
        &self,
        messages: &[ChatMessage],
        plan: &PlanOutput,
        executor_model: &str,
        event_tx: Option<mpsc::Sender<ModelLoadEvent>>,
    ) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        let system_content = format!("{}{}", EXECUTOR_SYSTEM_PREFIX, plan.plan);
        let mut executor_messages = vec![ChatMessage {
            role: Role::System,
            content: system_content,
            images: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        executor_messages.extend_from_slice(messages);

        let request = ChatRequest {
            model: executor_model.to_string(),
            messages: executor_messages,
            temperature: None, // Use default for natural output
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        info!(
            executor_model,
            plan_model = %plan.planner_model,
            "Executor phase starting"
        );

        self.chat_with_events(request, event_tx).await
    }

    /// Run the full phased pipeline: planner -> executor.
    ///
    /// If the planner model is unavailable, falls back to complexity routing
    /// (single-phase, as if PhaseBased was not configured).
    pub async fn chat_phased(
        &self,
        messages: &[ChatMessage],
        planner_model: &str,
        executor_model: &str,
        event_tx: Option<mpsc::Sender<ModelLoadEvent>>,
    ) -> Result<ReceiverStream<ChatChunk>, LlmError> {
        // Phase 1: Plan
        let plan = match self.run_planner(messages, planner_model).await {
            Ok(plan) => plan,
            Err(e) => {
                warn!(
                    error = %e,
                    planner_model,
                    "Planner phase failed, falling back to complexity routing"
                );
                // Fallback: build a regular request with the executor model
                let request = ChatRequest {
                    model: executor_model.to_string(),
                    messages: messages.to_vec(),
                    temperature: None,
                    max_tokens: None,
                    extra: None,
                    thinking: None,
                    tools: None,
                    tool_choice: None,
                };
                return self.chat_with_events(request, event_tx).await;
            }
        };

        // Phase 2: Execute
        self.run_executor(messages, &plan, executor_model, event_tx)
            .await
    }

    /// Run independent verification on a generated response.
    ///
    /// The verifier sees ONLY the original request + generated response + constraints.
    /// It does NOT see the planning/reasoning from the planner phase.
    pub async fn verify_response(
        &self,
        original_request: &str,
        generated_response: &str,
        constraints: Option<&str>,
        verifier_model: Option<&str>,
    ) -> Result<VerificationResult, LlmError> {
        let model = verifier_model
            .map(str::to_string)
            .or_else(|| self.verification.verifier_model.clone())
            .or_else(|| self.default_model())
            .unwrap_or_else(|| "default".to_string());

        let prompt = verification::build_verification_prompt(
            original_request,
            generated_response,
            constraints,
        );

        let request = ChatRequest {
            model: model.clone(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: prompt,
                images: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            temperature: Some(0.1), // Very low temperature for consistent verification
            max_tokens: Some(512),
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        let mut stream = self.chat(request).await?;
        let mut output = String::new();
        while let Some(chunk) = stream.next().await {
            output.push_str(&chunk.delta);
        }

        let result = verification::parse_verification_output(&output, &model);

        info!(
            verifier_model = %model,
            passed = result.passed,
            discrepancy_count = result.discrepancies.len(),
            "Verification complete"
        );

        Ok(result)
    }

    /// Check if verification is enabled for the given skill.
    pub fn should_verify(&self, skill_name: Option<&str>) -> bool {
        self.verification.is_enabled_for(skill_name)
    }

    /// Get the maximum retry count for verification failures.
    pub fn verification_max_retries(&self) -> u32 {
        self.verification.max_retries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_router() {
        let router = LlmRouter::empty();
        assert!(!router.has_providers());
        assert!(router.default_model().is_none());
    }

    #[tokio::test]
    async fn test_no_providers_error() {
        let router = LlmRouter::empty();
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
        let result = router.chat(request).await;
        assert!(matches!(result, Err(LlmError::NoProviders)));
    }

    #[test]
    fn test_default_model() {
        let router = LlmRouter::new(vec![], Some("qwen3-coder".to_string()), None);
        assert_eq!(router.default_model().as_deref(), Some("qwen3-coder"));
    }

    #[tokio::test]
    async fn test_set_default_model_is_used_by_next_chat() {
        // G3 regression proof: after set_default_model, a request with an
        // empty model field must resolve to the NEW default — no restart.
        let recorder = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let chunks = vec![ChatChunk {
            delta: "ok".to_string(),
            done: true,
            usage: None,
            model: "mock-model".to_string(),
            error: None,
            tool_call_deltas: None,
            finish_reason: Some("stop".to_string()),
        }];
        let router = LlmRouter::mock_scripted_recording(chunks, recorder.clone());

        router.set_default_model(Some("new-active".to_string()));
        assert_eq!(router.default_model().as_deref(), Some("new-active"));

        let mut stream = router.chat(simple_request("")).await.unwrap();
        while stream.next().await.is_some() {}

        let recorded = recorder.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].model, "new-active",
            "chat with empty model must resolve to the runtime-set default"
        );
    }

    fn simple_request(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages: vec![crate::types::ChatMessage {
                role: crate::types::Role::User,
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

    fn complex_request(model: &str) -> ChatRequest {
        let messages: Vec<_> = (0..20)
            .map(|i| crate::types::ChatMessage {
                role: if i % 2 == 0 {
                    crate::types::Role::User
                } else {
                    crate::types::Role::Assistant
                },
                content: "a]question or answer text".to_string(),
                images: None,
                tool_calls: None,
                tool_call_id: None,
            })
            .collect();
        ChatRequest {
            model: model.to_string(),
            messages,
            temperature: None,
            max_tokens: None,
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        }
    }

    #[test]
    fn test_resolve_model_no_routing() {
        let router = LlmRouter::new(vec![], Some("default-model".to_string()), None);
        let req = simple_request("explicit-model");
        assert_eq!(router.resolve_model(&req), "explicit-model");
    }

    #[test]
    fn test_resolve_model_falls_back_to_default() {
        let router = LlmRouter::new(vec![], Some("default-model".to_string()), None);
        let req = simple_request("");
        assert_eq!(router.resolve_model(&req), "default-model");
    }

    #[test]
    fn test_resolve_model_routing_simple() {
        let overrides = RoutingOverrides {
            simple_model: Some("qwen3:8b".to_string()),
            medium_model: Some("qwen3:30b".to_string()),
            complex_model: Some("qwen3-coder".to_string()),
        };
        let router = LlmRouter::new(vec![], Some("default".to_string()), Some(overrides));
        let req = simple_request("whatever");
        assert_eq!(router.resolve_model(&req), "qwen3:8b");
    }

    #[test]
    fn test_resolve_model_routing_complex() {
        let overrides = RoutingOverrides {
            simple_model: Some("qwen3:8b".to_string()),
            medium_model: Some("qwen3:30b".to_string()),
            complex_model: Some("qwen3-coder".to_string()),
        };
        let router = LlmRouter::new(vec![], Some("default".to_string()), Some(overrides));
        let req = complex_request("whatever");
        assert_eq!(router.resolve_model(&req), "qwen3-coder");
    }

    #[test]
    fn test_resolve_model_partial_overrides_fall_through() {
        // Only simple_model set; medium and complex should fall back to request model
        let overrides = RoutingOverrides {
            simple_model: Some("qwen3:8b".to_string()),
            medium_model: None,
            complex_model: None,
        };
        let router = LlmRouter::new(vec![], Some("default".to_string()), Some(overrides));
        let req = complex_request("explicit-model");
        // Complex level, but no complex_model override => falls back to request model
        assert_eq!(router.resolve_model(&req), "explicit-model");
    }

    #[test]
    fn test_resolve_model_empty_request_model_uses_default() {
        let overrides = RoutingOverrides {
            simple_model: None,
            medium_model: None,
            complex_model: None,
        };
        let router = LlmRouter::new(vec![], Some("fallback".to_string()), Some(overrides));
        let req = simple_request("");
        // No override for simple, empty request model => use default
        assert_eq!(router.resolve_model(&req), "fallback");
    }

    // -- Prefix-based routing tests --

    fn mock_provider(name: &str) -> Provider {
        use crate::provider::MockProvider;
        match name {
            "mock" => Provider::Mock(MockProvider::new("mock-response")),
            _ => Provider::Mock(MockProvider::new(&format!("{name}-response"))),
        }
    }

    #[test]
    fn test_provider_index_built_on_new() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, None, None);
        assert!(router.provider_index.contains_key("mock"));
        assert_eq!(router.provider_index["mock"], 0);
    }

    #[test]
    fn test_route_by_prefix_matches() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, None, None);
        let result = router.route_by_prefix("mock/some-model");
        assert!(result.is_some());
        let (idx, bare) = result.unwrap();
        assert_eq!(idx, 0);
        assert_eq!(bare, "some-model");
    }

    #[test]
    fn test_route_by_prefix_unknown_returns_none() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, None, None);
        assert!(router.route_by_prefix("unknown/model").is_none());
    }

    #[test]
    fn test_route_by_prefix_no_slash_returns_none() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, None, None);
        assert!(router.route_by_prefix("plain-model-name").is_none());
    }

    #[test]
    fn test_route_by_prefix_strips_prefix() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, None, None);
        let (_, bare) = router.route_by_prefix("mock/deep/model").unwrap();
        // Only strips the first prefix segment
        assert_eq!(bare, "deep/model");
    }

    // -- is_llama_server_model tests (FIX #0) --

    fn llama_provider() -> Provider {
        Provider::LlamaServer(crate::llama_server::LlamaServerProvider::new(
            "http://localhost:8080",
            300,
        ))
    }

    fn openrouter_provider() -> Provider {
        Provider::OpenAi(crate::openai::OpenAiProvider::new(
            "https://openrouter.ai/api/v1",
            Some("test-key".to_string()),
            "openrouter",
            120,
            std::collections::HashMap::new(),
        ))
    }

    #[test]
    fn test_is_llama_server_bare_name_is_local() {
        // The phone's real path: default_model() → bare "qwen3.6:35b-a3b".
        let router = LlmRouter::new(vec![llama_provider()], None, None);
        assert!(router.is_llama_server_model("qwen3.6:35b-a3b"));
    }

    #[test]
    fn test_is_llama_server_prefixed_llama_is_local() {
        let router = LlmRouter::new(vec![llama_provider()], None, None);
        assert!(router.is_llama_server_model("llama-server/qwen3.6:35b-a3b"));
    }

    #[test]
    fn test_is_llama_server_cloud_prefix_is_not_local() {
        let router = LlmRouter::new(vec![llama_provider(), openrouter_provider()], None, None);
        assert!(!router.is_llama_server_model("openrouter/google/gemini-2.5-pro"));
    }

    #[test]
    fn test_is_llama_server_no_llama_provider_is_not_local() {
        // Cloud-only deployment: nothing is local.
        let router = LlmRouter::new(vec![openrouter_provider()], None, None);
        assert!(!router.is_llama_server_model("qwen3.6:35b-a3b"));
        assert!(!router.is_llama_server_model("openrouter/x"));
    }

    #[tokio::test]
    async fn test_chat_prefix_routes_to_correct_provider() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, None, None);
        let request = simple_request("mock/test-model");
        let result = router.chat(request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_chat_no_prefix_uses_fallback() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, Some("default".to_string()), None);
        let request = simple_request("some-model");
        // No prefix match → falls through to first healthy provider
        let result = router.chat(request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_chat_unknown_prefix_falls_through() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, None, None);
        // "unknown/model" — prefix doesn't match any provider → fallback behavior
        let request = simple_request("unknown/model");
        let result = router.chat(request).await;
        assert!(result.is_ok()); // Falls through to first healthy (mock)
    }

    // -- chat_with_events tests --

    #[tokio::test]
    async fn test_chat_with_events_no_providers() {
        let router = LlmRouter::empty();
        let request = simple_request("test");
        let result = router.chat_with_events(request, None).await;
        assert!(matches!(result, Err(LlmError::NoProviders)));
    }

    #[tokio::test]
    async fn test_chat_with_events_prefix_routing() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, None, None);
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        let request = simple_request("mock/test-model");
        let result = router.chat_with_events(request, Some(tx)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_chat_with_events_fallback() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, Some("default".to_string()), None);
        let request = simple_request("no-prefix");
        let result = router.chat_with_events(request, None).await;
        assert!(result.is_ok());
    }

    // -- Routing mode tests --

    #[test]
    fn test_routing_mode_for_no_skill() {
        let router = LlmRouter::empty();
        let mode = router.routing_mode_for(None);
        assert!(matches!(mode, RoutingMode::Complexity));
    }

    #[test]
    fn test_routing_mode_for_unconfigured_skill() {
        let router = LlmRouter::empty();
        let mode = router.routing_mode_for(Some("coding"));
        assert!(matches!(mode, RoutingMode::Complexity));
    }

    #[test]
    fn test_routing_mode_for_configured_skill() {
        let skill_routing = SkillRoutingConfig {
            phase_based_skills: vec!["coding".to_string()],
            default_planner: Some("small-model".to_string()),
            default_executor: Some("big-model".to_string()),
        };
        let router = LlmRouter::with_config(
            vec![],
            None,
            None,
            skill_routing,
            VerificationConfig::default(),
        );
        let mode = router.routing_mode_for(Some("coding"));
        match mode {
            RoutingMode::PhaseBased { planner, executor } => {
                assert_eq!(planner, "small-model");
                assert_eq!(executor, "big-model");
            }
            _ => panic!("Expected PhaseBased for coding skill"),
        }
    }

    #[test]
    fn test_with_config_preserves_settings() {
        let skill_routing = SkillRoutingConfig {
            phase_based_skills: vec!["analysis".to_string()],
            default_planner: Some("planner".to_string()),
            default_executor: Some("executor".to_string()),
        };
        let verification = VerificationConfig {
            enabled: true,
            verifier_model: Some("verifier".to_string()),
            enabled_skills: vec!["analysis".to_string()],
            max_retries: 2,
        };
        let router = LlmRouter::with_config(
            vec![],
            Some("default".to_string()),
            None,
            skill_routing,
            verification,
        );
        assert!(router.skill_routing().is_phase_based("analysis"));
        assert!(router.should_verify(Some("analysis")));
        assert_eq!(router.verification_max_retries(), 2);
    }

    // -- Planner tests --

    #[tokio::test]
    async fn test_run_planner_with_mock() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, Some("mock-model".to_string()), None);
        let messages = vec![ChatMessage {
            role: Role::User,
            content: "Explain Rust ownership".to_string(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        let result = router.run_planner(&messages, "mock-model").await;
        assert!(result.is_ok());
        let plan = result.unwrap();
        // MockProvider returns "mock-response" for any mock provider created via mock_provider()
        assert!(!plan.plan.is_empty());
        assert_eq!(plan.planner_model, "mock-model");
    }

    #[tokio::test]
    async fn test_run_planner_no_providers_fails() {
        let router = LlmRouter::empty();
        let messages = vec![ChatMessage {
            role: Role::User,
            content: "test".to_string(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        let result = router.run_planner(&messages, "model").await;
        assert!(result.is_err());
    }

    // -- Executor tests --

    #[tokio::test]
    async fn test_run_executor_with_mock() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, Some("mock-model".to_string()), None);
        let messages = vec![ChatMessage {
            role: Role::User,
            content: "Explain Rust ownership".to_string(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        let plan = PlanOutput {
            plan: "1. Explain ownership rules\n2. Give examples".to_string(),
            planner_model: "mock-model".to_string(),
        };
        let result = router
            .run_executor(&messages, &plan, "mock-model", None)
            .await;
        assert!(result.is_ok());
    }

    // -- Phased pipeline tests --

    #[tokio::test]
    async fn test_chat_phased_with_mock() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, Some("mock-model".to_string()), None);
        let messages = vec![ChatMessage {
            role: Role::User,
            content: "Explain Rust ownership".to_string(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        let result = router
            .chat_phased(&messages, "mock-model", "mock-model", None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_chat_phased_planner_failure_falls_back() {
        // Empty providers will cause planner to fail, but since executor also needs
        // a provider, this will ultimately fail. The point is it attempts fallback.
        let router = LlmRouter::empty();
        let messages = vec![ChatMessage {
            role: Role::User,
            content: "test".to_string(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        let result = router
            .chat_phased(&messages, "planner", "executor", None)
            .await;
        // Both planner and fallback fail with no providers
        assert!(result.is_err());
    }

    // -- Verification tests --

    #[tokio::test]
    async fn test_verify_response_with_mock() {
        let providers = vec![mock_provider("mock")];
        let router = LlmRouter::new(providers, Some("mock-model".to_string()), None);
        let result = router
            .verify_response("What is 2+2?", "4", None, None)
            .await;
        assert!(result.is_ok());
        let verification = result.unwrap();
        // MockProvider returns "mock-response" which doesn't contain "FAIL",
        // so parse_verification_output defaults to passed
        assert!(verification.passed);
    }

    #[test]
    fn test_should_verify_disabled() {
        let router = LlmRouter::empty();
        assert!(!router.should_verify(Some("coding")));
        assert!(!router.should_verify(None));
    }

    #[test]
    fn test_should_verify_enabled() {
        let verification = VerificationConfig {
            enabled: true,
            verifier_model: None,
            enabled_skills: vec!["coding".to_string()],
            max_retries: 1,
        };
        let router = LlmRouter::with_config(
            vec![],
            None,
            None,
            SkillRoutingConfig::default(),
            verification,
        );
        assert!(router.should_verify(Some("coding")));
        assert!(!router.should_verify(Some("chat")));
        assert!(!router.should_verify(None));
    }

    #[test]
    fn test_verification_max_retries() {
        let verification = VerificationConfig {
            enabled: true,
            verifier_model: None,
            enabled_skills: vec![],
            max_retries: 3,
        };
        let router = LlmRouter::with_config(
            vec![],
            None,
            None,
            SkillRoutingConfig::default(),
            verification,
        );
        assert_eq!(router.verification_max_retries(), 3);
    }

    #[test]
    fn test_skill_routing_accessor() {
        let config = SkillRoutingConfig {
            phase_based_skills: vec!["test".to_string()],
            default_planner: None,
            default_executor: None,
        };
        let router =
            LlmRouter::with_config(vec![], None, None, config, VerificationConfig::default());
        assert!(router.skill_routing().is_phase_based("test"));
        assert!(!router.skill_routing().is_phase_based("other"));
    }
}
