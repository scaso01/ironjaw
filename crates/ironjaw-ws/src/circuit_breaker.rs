use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Circuit breaker state
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CircuitState {
    /// Normal operation — requests flow through
    Closed,
    /// Too many failures — requests are rejected
    Open,
    /// Cooldown elapsed — next request is a probe
    HalfOpen,
}

/// Per-target circuit breaker
#[derive(Debug)]
struct CircuitBreaker {
    state: CircuitState,
    failures: u64,
    threshold: u64,
    cooldown_secs: u64,
    last_failure: Option<std::time::Instant>,
    last_success: Option<std::time::Instant>,
}

impl CircuitBreaker {
    fn new(threshold: u64, cooldown_secs: u64) -> Self {
        Self {
            state: CircuitState::Closed,
            failures: 0,
            threshold,
            cooldown_secs,
            last_failure: None,
            last_success: None,
        }
    }

    fn check(&mut self) -> Result<(), CircuitState> {
        match self.state {
            CircuitState::Closed => Ok(()),
            CircuitState::Open => {
                // Check if cooldown has elapsed
                if let Some(last) = self.last_failure {
                    if last.elapsed().as_secs() >= self.cooldown_secs {
                        self.state = CircuitState::HalfOpen;
                        return Ok(());
                    }
                }
                Err(CircuitState::Open)
            }
            CircuitState::HalfOpen => Ok(()), // Allow probe request
        }
    }

    fn record_success(&mut self) {
        self.failures = 0;
        self.state = CircuitState::Closed;
        self.last_success = Some(std::time::Instant::now());
    }

    fn record_failure(&mut self) {
        self.failures += 1;
        self.last_failure = Some(std::time::Instant::now());
        if self.failures >= self.threshold {
            self.state = CircuitState::Open;
        }
    }
}

/// Registry of circuit breakers, keyed by target name (e.g. tool name, provider)
#[derive(Clone)]
pub struct CircuitBreakerRegistry {
    breakers: Arc<RwLock<HashMap<String, CircuitBreaker>>>,
    default_threshold: u64,
    default_cooldown_secs: u64,
}

impl CircuitBreakerRegistry {
    pub fn new(default_threshold: u64, default_cooldown_secs: u64) -> Self {
        Self {
            breakers: Arc::new(RwLock::new(HashMap::new())),
            default_threshold,
            default_cooldown_secs,
        }
    }

    /// Check if requests to a target are allowed.
    pub async fn check(&self, target: &str) -> Result<(), String> {
        let mut breakers = self.breakers.write().await;
        let breaker = breakers.entry(target.to_string()).or_insert_with(|| {
            CircuitBreaker::new(self.default_threshold, self.default_cooldown_secs)
        });
        breaker.check().map_err(|_| {
            format!(
                "Circuit breaker open for '{}' ({} failures, cooldown {}s)",
                target, breaker.failures, breaker.cooldown_secs
            )
        })
    }

    /// Record a successful operation.
    pub async fn record_success(&self, target: &str) {
        let mut breakers = self.breakers.write().await;
        if let Some(breaker) = breakers.get_mut(target) {
            breaker.record_success();
        }
    }

    /// Record a failed operation.
    pub async fn record_failure(&self, target: &str) {
        let mut breakers = self.breakers.write().await;
        let breaker = breakers.entry(target.to_string()).or_insert_with(|| {
            CircuitBreaker::new(self.default_threshold, self.default_cooldown_secs)
        });
        breaker.record_failure();
    }

    /// Get the current state of a target's circuit breaker.
    pub async fn state(&self, target: &str) -> CircuitState {
        let breakers = self.breakers.read().await;
        breakers
            .get(target)
            .map(|b| b.state)
            .unwrap_or(CircuitState::Closed)
    }

    /// Get a snapshot of all breaker states.
    pub async fn snapshot(&self) -> HashMap<String, (CircuitState, u64)> {
        let breakers = self.breakers.read().await;
        breakers
            .iter()
            .map(|(k, v)| (k.clone(), (v.state, v.failures)))
            .collect()
    }
}

/// Simple internal metrics using atomics
#[derive(Debug)]
pub struct Metrics {
    pub connections_total: AtomicU64,
    pub requests_total: AtomicU64,
    pub llm_calls_total: AtomicU64,
    pub tool_calls_total: AtomicU64,
    pub errors_total: AtomicU64,
    pub voice_sessions_total: AtomicU64,
    pub active_connections: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            connections_total: AtomicU64::new(0),
            requests_total: AtomicU64::new(0),
            llm_calls_total: AtomicU64::new(0),
            tool_calls_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            voice_sessions_total: AtomicU64::new(0),
            active_connections: AtomicU64::new(0),
        }
    }

    pub fn inc_connections(&self) {
        self.connections_total.fetch_add(1, Ordering::Relaxed);
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_connections(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn inc_requests(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_llm_calls(&self) {
        self.llm_calls_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_tool_calls(&self) {
        self.tool_calls_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_errors(&self) {
        self.errors_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_voice_sessions(&self) {
        self.voice_sessions_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Get a JSON-serializable snapshot of all metrics.
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "connections_total": self.connections_total.load(Ordering::Relaxed),
            "active_connections": self.active_connections.load(Ordering::Relaxed),
            "requests_total": self.requests_total.load(Ordering::Relaxed),
            "llm_calls_total": self.llm_calls_total.load(Ordering::Relaxed),
            "tool_calls_total": self.tool_calls_total.load(Ordering::Relaxed),
            "errors_total": self.errors_total.load(Ordering::Relaxed),
            "voice_sessions_total": self.voice_sessions_total.load(Ordering::Relaxed),
        })
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_circuit_breaker_closed_by_default() {
        let registry = CircuitBreakerRegistry::new(3, 30);
        assert_eq!(registry.state("test").await, CircuitState::Closed);
        assert!(registry.check("test").await.is_ok());
    }

    #[tokio::test]
    async fn test_circuit_breaker_opens_after_threshold() {
        let registry = CircuitBreakerRegistry::new(3, 30);
        registry.record_failure("test").await;
        registry.record_failure("test").await;
        assert!(registry.check("test").await.is_ok()); // 2 failures, threshold is 3
        registry.record_failure("test").await;
        assert_eq!(registry.state("test").await, CircuitState::Open);
        assert!(registry.check("test").await.is_err());
    }

    #[tokio::test]
    async fn test_circuit_breaker_success_resets() {
        let registry = CircuitBreakerRegistry::new(3, 30);
        registry.record_failure("test").await;
        registry.record_failure("test").await;
        registry.record_success("test").await;
        assert_eq!(registry.state("test").await, CircuitState::Closed);
        // Should need 3 more failures to open again
        registry.record_failure("test").await;
        registry.record_failure("test").await;
        assert!(registry.check("test").await.is_ok());
    }

    #[tokio::test]
    async fn test_circuit_breaker_half_open_after_cooldown() {
        let registry = CircuitBreakerRegistry::new(1, 0); // 0s cooldown for test
        registry.record_failure("test").await;
        assert_eq!(registry.state("test").await, CircuitState::Open);
        // With 0s cooldown, next check should transition to half-open
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(registry.check("test").await.is_ok());
        assert_eq!(registry.state("test").await, CircuitState::HalfOpen);
    }

    #[tokio::test]
    async fn test_circuit_breaker_independent_targets() {
        let registry = CircuitBreakerRegistry::new(2, 30);
        registry.record_failure("a").await;
        registry.record_failure("a").await;
        assert!(registry.check("a").await.is_err());
        assert!(registry.check("b").await.is_ok()); // 'b' unaffected
    }

    #[tokio::test]
    async fn test_circuit_breaker_snapshot() {
        let registry = CircuitBreakerRegistry::new(3, 30);
        registry.record_failure("a").await;
        registry.record_failure("b").await;
        registry.record_failure("b").await;
        let snap = registry.snapshot().await;
        assert_eq!(snap.len(), 2);
        assert_eq!(snap["a"].1, 1); // 1 failure
        assert_eq!(snap["b"].1, 2); // 2 failures
    }

    #[test]
    fn test_metrics_increment() {
        let m = Metrics::new();
        m.inc_connections();
        m.inc_connections();
        m.inc_requests();
        m.inc_llm_calls();
        m.inc_tool_calls();
        m.inc_errors();
        m.inc_voice_sessions();
        m.dec_connections();

        let snap = m.snapshot();
        assert_eq!(snap["connections_total"], 2);
        assert_eq!(snap["active_connections"], 1);
        assert_eq!(snap["requests_total"], 1);
        assert_eq!(snap["llm_calls_total"], 1);
        assert_eq!(snap["tool_calls_total"], 1);
        assert_eq!(snap["errors_total"], 1);
        assert_eq!(snap["voice_sessions_total"], 1);
    }

    #[test]
    fn test_metrics_default() {
        let m = Metrics::default();
        let snap = m.snapshot();
        assert_eq!(snap["connections_total"], 0);
        assert_eq!(snap["requests_total"], 0);
    }
}
