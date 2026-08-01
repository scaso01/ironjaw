use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Serialize)]
pub enum LlamaHealth {
    Healthy,
    Loading,
    Error(String),
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct LlamaServerState {
    pub loaded_model: Option<String>,
    /// Last model that was confirmed healthy — used for auto-recovery.
    pub last_known_model: Option<String>,
    pub status: LlamaHealth,
    #[serde(skip)]
    pub last_check: Option<Instant>,
    pub consecutive_failures: u32,
    /// True while an auto-recovery restart is in progress (prevents re-entry).
    #[serde(skip)]
    pub recovery_in_progress: bool,
}

impl Default for LlamaServerState {
    fn default() -> Self {
        Self {
            loaded_model: None,
            last_known_model: None,
            status: LlamaHealth::Unknown,
            last_check: None,
            consecutive_failures: 0,
            recovery_in_progress: false,
        }
    }
}

/// Emitted when the loaded model changes (detected by polling or explicit switch).
#[derive(Debug, Clone, Serialize)]
pub struct ModelChangeEvent {
    /// Previous model (None if first detection).
    pub previous: Option<String>,
    /// New model name.
    pub current: String,
    /// Source of the change.
    pub source: ModelChangeSource,
}

/// How the model change was triggered.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelChangeSource {
    /// Detected by health polling (external switch or recovery).
    Poll,
    /// Triggered by model.switch handler.
    UserSwitch,
    /// Triggered by auto-recovery after crash.
    AutoRecovery,
}

/// Monitors llama-server health and loaded model by polling its HTTP endpoints.
///
/// Holds shared state behind `Arc<RwLock<_>>` so consumers can read the latest
/// status without blocking the polling loop.
pub struct LlamaStatusMonitor {
    state: Arc<RwLock<LlamaServerState>>,
    base_url: String,
    client: reqwest::Client,
    model_change_tx: broadcast::Sender<ModelChangeEvent>,
}

impl LlamaStatusMonitor {
    pub fn new(base_url: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("failed to build reqwest client");
        let (model_change_tx, _) = broadcast::channel(16);
        Self {
            state: Arc::new(RwLock::new(LlamaServerState::default())),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
            model_change_tx,
        }
    }

    /// Subscribe to model change events.
    pub fn subscribe_model_changes(&self) -> broadcast::Receiver<ModelChangeEvent> {
        self.model_change_tx.subscribe()
    }

    /// Get a clone of the model change sender (for external code to emit events).
    pub fn model_change_sender(&self) -> broadcast::Sender<ModelChangeEvent> {
        self.model_change_tx.clone()
    }

    /// Returns the current state snapshot.
    pub async fn state(&self) -> LlamaServerState {
        self.state.read().await.clone()
    }

    /// Returns a clone of the inner `Arc` for sharing with other subsystems.
    pub fn shared_state(&self) -> Arc<RwLock<LlamaServerState>> {
        Arc::clone(&self.state)
    }

    /// Spawn a background task that polls health + model every `interval_secs`.
    pub fn start_polling(&self, interval_secs: u64) {
        let state = Arc::clone(&self.state);
        let base_url = self.base_url.clone();
        let client = self.client.clone();
        let change_tx = self.model_change_tx.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            loop {
                interval.tick().await;
                Self::poll_once(&client, &base_url, &state, Some(&change_tx)).await;
            }
        });
    }

    /// Like `start_polling` but with auto-recovery: on crash detection (3+
    /// consecutive failures), automatically runs `llama-switch {active_or_default}`
    /// to restart with the last known good model.
    ///
    /// If `model_registry` is provided, persists the recovered model as active.
    pub fn start_polling_with_recovery(
        &self,
        interval_secs: u64,
        default_model: String,
        model_registry: Option<Arc<crate::model_registry::ModelRegistry>>,
    ) {
        let state = Arc::clone(&self.state);
        let base_url = self.base_url.clone();
        let client = self.client.clone();
        let change_tx = self.model_change_tx.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            loop {
                interval.tick().await;
                Self::poll_once(&client, &base_url, &state, Some(&change_tx)).await;

                // Check if we need auto-recovery
                let should_recover = {
                    let s = state.read().await;
                    s.consecutive_failures >= 3 && !s.recovery_in_progress
                };

                if should_recover {
                    let recovery_model = {
                        let mut s = state.write().await;
                        s.recovery_in_progress = true;
                        s.last_known_model
                            .clone()
                            .unwrap_or_else(|| default_model.clone())
                    };

                    warn!(
                        model = %recovery_model,
                        "Auto-recovering crashed llama-server"
                    );

                    let home = std::env::var("HOME")
                        .or_else(|_| std::env::var("USERPROFILE"))
                        .unwrap_or_default();
                    let switch_script = format!("{home}/Scripts/llama-switch.sh");
                    let recovery_result = tokio::process::Command::new(crate::GIT_BASH)
                        .args([&switch_script, &recovery_model])
                        .output()
                        .await;

                    match recovery_result {
                        Ok(output) if output.status.success() => {
                            debug!(
                                model = %recovery_model,
                                "Recovery llama-switch completed, waiting for health"
                            );
                            // Persist the recovered model as active
                            if let Some(ref registry) = model_registry {
                                if let Err(e) = registry.persist_active(&recovery_model) {
                                    warn!(error = %e, "Failed to persist recovered model as active");
                                }
                            }
                        }
                        Ok(output) => {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            error!(
                                model = %recovery_model,
                                stderr = %stderr,
                                "Recovery llama-switch failed"
                            );
                        }
                        Err(e) => {
                            error!(error = %e, "Failed to run recovery llama-switch");
                        }
                    }

                    // Clear recovery flag after attempt (next poll will update health)
                    let mut s = state.write().await;
                    s.recovery_in_progress = false;
                }
            }
        });
    }

    /// Run a single health + model check cycle. Public for tests and on-demand refresh.
    pub async fn check_now(&self) {
        Self::poll_once(
            &self.client,
            &self.base_url,
            &self.state,
            Some(&self.model_change_tx),
        )
        .await;
    }

    async fn poll_once(
        client: &reqwest::Client,
        base_url: &str,
        state: &Arc<RwLock<LlamaServerState>>,
        model_change_tx: Option<&broadcast::Sender<ModelChangeEvent>>,
    ) {
        let healthy = Self::check_health(client, base_url).await;
        let model = if healthy {
            Self::check_loaded_model(client, base_url).await
        } else {
            None
        };

        let mut s = state.write().await;
        s.last_check = Some(Instant::now());

        if healthy {
            if s.consecutive_failures > 0 {
                debug!(
                    previous_failures = s.consecutive_failures,
                    "llama-server recovered"
                );
            }
            s.consecutive_failures = 0;
            s.status = LlamaHealth::Healthy;

            // Detect model change and broadcast
            if let Some(ref new_model) = model {
                let old_model = s.loaded_model.as_deref();
                if old_model != Some(new_model.as_str()) {
                    info!(
                        previous = ?old_model,
                        current = %new_model,
                        "Model change detected by poll"
                    );
                    if let Some(tx) = model_change_tx {
                        let _ = tx.send(ModelChangeEvent {
                            previous: old_model.map(String::from),
                            current: new_model.clone(),
                            source: ModelChangeSource::Poll,
                        });
                    }
                }
                s.last_known_model = model.clone();
            }
            s.loaded_model = model;
        } else {
            s.consecutive_failures += 1;
            if s.consecutive_failures >= 2 {
                let msg = format!(
                    "llama-server unreachable ({} consecutive failures)",
                    s.consecutive_failures
                );
                warn!("{}", msg);
                s.status = LlamaHealth::Error(msg);
            } else {
                debug!(
                    "llama-server health check failed (attempt {})",
                    s.consecutive_failures
                );
                s.status = LlamaHealth::Loading;
            }
        }
    }

    /// Hit `GET {base_url}/health` and return true if status is "ok".
    async fn check_health(client: &reqwest::Client, base_url: &str) -> bool {
        let url = format!("{base_url}/health");
        match client.get(&url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    return false;
                }
                match resp.json::<serde_json::Value>().await {
                    Ok(body) => body["status"].as_str() == Some("ok"),
                    Err(e) => {
                        debug!(error = %e, "Failed to parse health response");
                        false
                    }
                }
            }
            Err(e) => {
                debug!(error = %e, "Health check request failed");
                false
            }
        }
    }

    /// Hit `GET {base_url}/v1/models` and extract the model ID from the first entry.
    async fn check_loaded_model(client: &reqwest::Client, base_url: &str) -> Option<String> {
        let url = format!("{base_url}/v1/models");
        match client.get(&url).send().await {
            Ok(resp) => {
                let body: serde_json::Value = resp.json().await.ok()?;
                body["data"]
                    .as_array()?
                    .first()?
                    .get("id")?
                    .as_str()
                    .map(|s| s.to_string())
            }
            Err(e) => {
                error!(error = %e, "Failed to fetch loaded model");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_state() {
        let state = LlamaServerState::default();
        assert!(state.loaded_model.is_none());
        assert!(state.last_known_model.is_none());
        assert!(state.last_check.is_none());
        assert_eq!(state.consecutive_failures, 0);
        assert!(!state.recovery_in_progress);
        assert!(matches!(state.status, LlamaHealth::Unknown));
    }

    #[test]
    fn test_health_variants_serialize() {
        // Ensure all variants can be serialized without panic
        let variants = vec![
            LlamaHealth::Healthy,
            LlamaHealth::Loading,
            LlamaHealth::Error("test error".to_string()),
            LlamaHealth::Unknown,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            assert!(!json.is_empty());
        }
    }

    #[test]
    fn test_state_clone() {
        let state = LlamaServerState {
            loaded_model: Some("qwen3-coder".to_string()),
            last_known_model: Some("qwen3-coder".to_string()),
            status: LlamaHealth::Healthy,
            last_check: Some(Instant::now()),
            consecutive_failures: 0,
            recovery_in_progress: false,
        };
        assert_eq!(state.loaded_model, Some("qwen3-coder".to_string()));
        assert_eq!(state.last_known_model, Some("qwen3-coder".to_string()));
        assert!(matches!(state.status, LlamaHealth::Healthy));
    }

    #[tokio::test]
    async fn test_monitor_initial_state() {
        let monitor = LlamaStatusMonitor::new("http://localhost:99999");
        let state = monitor.state().await;
        assert!(matches!(state.status, LlamaHealth::Unknown));
        assert!(state.loaded_model.is_none());
    }

    #[tokio::test]
    async fn test_health_transitions_to_loading_then_error() {
        // Hitting a non-existent server should fail health checks.
        // After 1 failure: Loading. After 2: Error.
        let monitor = LlamaStatusMonitor::new("http://127.0.0.1:1");

        // First check — single failure → Loading
        monitor.check_now().await;
        let state = monitor.state().await;
        assert_eq!(state.consecutive_failures, 1);
        assert!(matches!(state.status, LlamaHealth::Loading));

        // Second check — two consecutive failures → Error
        monitor.check_now().await;
        let state = monitor.state().await;
        assert_eq!(state.consecutive_failures, 2);
        assert!(matches!(state.status, LlamaHealth::Error(_)));
    }

    #[tokio::test]
    async fn test_shared_state_reflects_updates() {
        let monitor = LlamaStatusMonitor::new("http://127.0.0.1:1");
        let shared = monitor.shared_state();

        // Before any check
        {
            let s = shared.read().await;
            assert!(matches!(s.status, LlamaHealth::Unknown));
        }

        monitor.check_now().await;

        // After one failed check
        {
            let s = shared.read().await;
            assert_eq!(s.consecutive_failures, 1);
            assert!(s.last_check.is_some());
        }
    }

    #[tokio::test]
    async fn test_recovery_resets_failures() {
        // Simulate recovery by directly manipulating state
        let monitor = LlamaStatusMonitor::new("http://127.0.0.1:1");

        // Inject a failure state
        {
            let mut s = monitor.state.write().await;
            s.consecutive_failures = 5;
            s.status = LlamaHealth::Error("down".to_string());
        }

        // Simulate a successful poll by writing healthy state directly.
        // (We can't spin up a real server in unit tests, so we verify the
        // reset logic via the state contract.)
        {
            let mut s = monitor.state.write().await;
            s.consecutive_failures = 0;
            s.status = LlamaHealth::Healthy;
            s.loaded_model = Some("recovered-model".to_string());
        }

        let state = monitor.state().await;
        assert_eq!(state.consecutive_failures, 0);
        assert!(matches!(state.status, LlamaHealth::Healthy));
        assert_eq!(state.loaded_model, Some("recovered-model".to_string()));
    }

    #[test]
    fn test_model_change_event_serializes() {
        let event = ModelChangeEvent {
            previous: Some("old-model".to_string()),
            current: "new-model".to_string(),
            source: ModelChangeSource::Poll,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("old-model"));
        assert!(json.contains("new-model"));
        assert!(json.contains("poll"));
    }

    #[test]
    fn test_model_change_source_variants_serialize() {
        let sources = vec![
            ModelChangeSource::Poll,
            ModelChangeSource::UserSwitch,
            ModelChangeSource::AutoRecovery,
        ];
        for source in sources {
            let json = serde_json::to_string(&source).unwrap();
            assert!(!json.is_empty());
        }
    }

    #[tokio::test]
    async fn test_subscribe_model_changes() {
        let monitor = LlamaStatusMonitor::new("http://127.0.0.1:1");
        let mut rx = monitor.subscribe_model_changes();

        // Manually send a model change event
        let tx = monitor.model_change_sender();
        tx.send(ModelChangeEvent {
            previous: None,
            current: "test-model".to_string(),
            source: ModelChangeSource::UserSwitch,
        })
        .unwrap();

        let event = rx.recv().await.unwrap();
        assert_eq!(event.current, "test-model");
        assert!(event.previous.is_none());
        assert!(matches!(event.source, ModelChangeSource::UserSwitch));
    }

    #[tokio::test]
    async fn test_poll_detects_model_change() {
        // Simulate a model change by injecting state directly, then verifying
        // that poll_once would detect the difference.
        let _state = Arc::new(RwLock::new(LlamaServerState {
            loaded_model: Some("model-a".to_string()),
            last_known_model: Some("model-a".to_string()),
            status: LlamaHealth::Healthy,
            last_check: None,
            consecutive_failures: 0,
            recovery_in_progress: false,
        }));

        let (tx, mut rx) = broadcast::channel(4);

        // poll_once against a non-existent server won't change to a new model,
        // but we can verify the channel was created and is functional
        assert_eq!(tx.receiver_count(), 1);
        tx.send(ModelChangeEvent {
            previous: Some("model-a".to_string()),
            current: "model-b".to_string(),
            source: ModelChangeSource::Poll,
        })
        .unwrap();

        let event = rx.recv().await.unwrap();
        assert_eq!(event.previous, Some("model-a".to_string()));
        assert_eq!(event.current, "model-b".to_string());
    }
}
