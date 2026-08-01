//! Context Health Monitoring (I16)
//!
//! Tracks 4 failure modes and 5 quality metrics per session, surfacing
//! health data via `context.health` and real-time alerts via
//! `context.health.subscribe`.
//!
//! Failure modes:
//! - **Burst**: tool output exceeds token budget.
//! - **Poisoning**: unverified claims present in context.
//! - **Conflict**: contradictory memories detected.
//! - **Noise**: duplicate or redundant information.
//!
//! Quality metrics:
//! 1. Re-ask rate (user had to repeat a question)
//! 2. Thread recall accuracy (correct context retrieved)
//! 3. Conflict detection frequency
//! 4. Token cost per decision
//! 5. Tool error rate

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

// ---------------------------------------------------------------------------
// Failure Modes
// ---------------------------------------------------------------------------

/// The four tracked context failure modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureMode {
    /// Tool output exceeded the configured token budget.
    Burst,
    /// Unverified or hallucinated claims present in context.
    Poisoning,
    /// Contradictory memories or facts detected.
    Conflict,
    /// Duplicate or redundant information in context.
    Noise,
}

/// A single recorded failure event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureEvent {
    pub mode: FailureMode,
    pub description: String,
    pub severity: f64,
    pub timestamp: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Quality Metrics
// ---------------------------------------------------------------------------

/// The 5 per-session quality metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityMetrics {
    /// Fraction of messages that were re-asks of a previous question.
    pub re_ask_rate: f64,
    /// Fraction of recalls that returned contextually correct results.
    pub thread_recall_accuracy: f64,
    /// Number of conflicts detected in the current window.
    pub conflict_detection_count: u64,
    /// Average tokens spent per decision/action.
    pub token_cost_per_decision: f64,
    /// Fraction of tool invocations that returned errors.
    pub tool_error_rate: f64,
}

impl Default for QualityMetrics {
    fn default() -> Self {
        Self {
            re_ask_rate: 0.0,
            thread_recall_accuracy: 1.0,
            conflict_detection_count: 0,
            token_cost_per_decision: 0.0,
            tool_error_rate: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Thresholds
// ---------------------------------------------------------------------------

/// Configurable alert thresholds for quality metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthThresholds {
    /// Alert if re-ask rate exceeds this value (0.0 – 1.0).
    pub max_re_ask_rate: f64,
    /// Alert if recall accuracy drops below this value.
    pub min_recall_accuracy: f64,
    /// Alert if conflicts exceed this count in the window.
    pub max_conflicts: u64,
    /// Alert if token cost per decision exceeds this.
    pub max_token_cost: f64,
    /// Alert if tool error rate exceeds this value.
    pub max_tool_error_rate: f64,
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self {
            max_re_ask_rate: 0.3,
            min_recall_accuracy: 0.7,
            max_conflicts: 5,
            max_token_cost: 10000.0,
            max_tool_error_rate: 0.2,
        }
    }
}

// ---------------------------------------------------------------------------
// Alert
// ---------------------------------------------------------------------------

/// An alert emitted when a metric exceeds its threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthAlert {
    pub metric: String,
    pub current_value: f64,
    pub threshold: f64,
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Per-Session State
// ---------------------------------------------------------------------------

/// Internal counters used to compute quality metrics.
#[derive(Debug, Clone, Default)]
struct SessionCounters {
    total_messages: u64,
    re_asks: u64,
    total_recalls: u64,
    accurate_recalls: u64,
    total_decisions: u64,
    total_tokens: u64,
    total_tool_calls: u64,
    tool_errors: u64,
    failures: Vec<FailureEvent>,
}

impl SessionCounters {
    fn compute_metrics(&self) -> QualityMetrics {
        QualityMetrics {
            re_ask_rate: if self.total_messages > 0 {
                self.re_asks as f64 / self.total_messages as f64
            } else {
                0.0
            },
            thread_recall_accuracy: if self.total_recalls > 0 {
                self.accurate_recalls as f64 / self.total_recalls as f64
            } else {
                1.0
            },
            conflict_detection_count: self
                .failures
                .iter()
                .filter(|f| f.mode == FailureMode::Conflict)
                .count() as u64,
            token_cost_per_decision: if self.total_decisions > 0 {
                self.total_tokens as f64 / self.total_decisions as f64
            } else {
                0.0
            },
            tool_error_rate: if self.total_tool_calls > 0 {
                self.tool_errors as f64 / self.total_tool_calls as f64
            } else {
                0.0
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Health Snapshot (returned by RPC)
// ---------------------------------------------------------------------------

/// Full health snapshot returned by `context.health`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub session_id: String,
    pub metrics: QualityMetrics,
    pub recent_failures: Vec<FailureEvent>,
    pub alerts: Vec<HealthAlert>,
    pub timestamp: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// ContextHealthMonitor
// ---------------------------------------------------------------------------

/// Central monitor that tracks context health across sessions.
pub struct ContextHealthMonitor {
    sessions: RwLock<HashMap<String, SessionCounters>>,
    thresholds: RwLock<HealthThresholds>,
    alert_tx: broadcast::Sender<HealthAlert>,
}

impl ContextHealthMonitor {
    /// Create a new monitor with default thresholds.
    pub fn new() -> Self {
        let (alert_tx, _) = broadcast::channel(256);
        Self {
            sessions: RwLock::new(HashMap::new()),
            thresholds: RwLock::new(HealthThresholds::default()),
            alert_tx,
        }
    }

    /// Create a new monitor with custom thresholds.
    pub fn with_thresholds(thresholds: HealthThresholds) -> Self {
        let (alert_tx, _) = broadcast::channel(256);
        Self {
            sessions: RwLock::new(HashMap::new()),
            thresholds: RwLock::new(thresholds),
            alert_tx,
        }
    }

    /// Subscribe to real-time health alerts.
    pub fn subscribe(&self) -> broadcast::Receiver<HealthAlert> {
        self.alert_tx.subscribe()
    }

    /// Update thresholds at runtime.
    pub async fn set_thresholds(&self, thresholds: HealthThresholds) {
        *self.thresholds.write().await = thresholds;
    }

    /// Record a user message (for re-ask tracking).
    pub async fn record_message(&self, session_id: &str, is_re_ask: bool) {
        let mut sessions = self.sessions.write().await;
        let c = sessions.entry(session_id.to_string()).or_default();
        c.total_messages += 1;
        if is_re_ask {
            c.re_asks += 1;
        }
    }

    /// Record a memory recall attempt.
    pub async fn record_recall(&self, session_id: &str, accurate: bool) {
        let mut sessions = self.sessions.write().await;
        let c = sessions.entry(session_id.to_string()).or_default();
        c.total_recalls += 1;
        if accurate {
            c.accurate_recalls += 1;
        }
    }

    /// Record a decision with its token cost.
    pub async fn record_decision(&self, session_id: &str, tokens: u64) {
        let mut sessions = self.sessions.write().await;
        let c = sessions.entry(session_id.to_string()).or_default();
        c.total_decisions += 1;
        c.total_tokens += tokens;
    }

    /// Record a tool execution result.
    pub async fn record_tool_call(&self, session_id: &str, errored: bool) {
        let mut sessions = self.sessions.write().await;
        let c = sessions.entry(session_id.to_string()).or_default();
        c.total_tool_calls += 1;
        if errored {
            c.tool_errors += 1;
        }
    }

    /// Record a context failure event.
    pub async fn record_failure(&self, session_id: &str, event: FailureEvent) {
        let mut sessions = self.sessions.write().await;
        let c = sessions.entry(session_id.to_string()).or_default();
        c.failures.push(event);
    }

    /// Get a full health snapshot for a session, checking thresholds.
    pub async fn snapshot(&self, session_id: &str) -> HealthSnapshot {
        let sessions = self.sessions.read().await;
        let thresholds = self.thresholds.read().await;

        let counters = sessions.get(session_id);
        let metrics = counters.map(|c| c.compute_metrics()).unwrap_or_default();
        let recent_failures = counters
            .map(|c| {
                // Last 20 failures
                let start = c.failures.len().saturating_sub(20);
                c.failures[start..].to_vec()
            })
            .unwrap_or_default();

        let alerts = Self::check_thresholds(&metrics, &thresholds);

        // Broadcast any alerts
        for alert in &alerts {
            let _ = self.alert_tx.send(alert.clone());
        }

        HealthSnapshot {
            session_id: session_id.to_string(),
            metrics,
            recent_failures,
            alerts,
            timestamp: Utc::now(),
        }
    }

    /// Reset counters for a session.
    pub async fn reset_session(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        sessions.remove(session_id);
    }

    /// List all tracked session IDs.
    pub async fn tracked_sessions(&self) -> Vec<String> {
        self.sessions.read().await.keys().cloned().collect()
    }

    fn check_thresholds(metrics: &QualityMetrics, t: &HealthThresholds) -> Vec<HealthAlert> {
        let mut alerts = Vec::new();
        let now = Utc::now();

        if metrics.re_ask_rate > t.max_re_ask_rate {
            alerts.push(HealthAlert {
                metric: "re_ask_rate".to_string(),
                current_value: metrics.re_ask_rate,
                threshold: t.max_re_ask_rate,
                message: format!(
                    "Re-ask rate {:.1}% exceeds threshold {:.1}%",
                    metrics.re_ask_rate * 100.0,
                    t.max_re_ask_rate * 100.0
                ),
                timestamp: now,
            });
        }

        if metrics.thread_recall_accuracy < t.min_recall_accuracy {
            alerts.push(HealthAlert {
                metric: "thread_recall_accuracy".to_string(),
                current_value: metrics.thread_recall_accuracy,
                threshold: t.min_recall_accuracy,
                message: format!(
                    "Recall accuracy {:.1}% below threshold {:.1}%",
                    metrics.thread_recall_accuracy * 100.0,
                    t.min_recall_accuracy * 100.0
                ),
                timestamp: now,
            });
        }

        if metrics.conflict_detection_count > t.max_conflicts {
            alerts.push(HealthAlert {
                metric: "conflict_detection_count".to_string(),
                current_value: metrics.conflict_detection_count as f64,
                threshold: t.max_conflicts as f64,
                message: format!(
                    "Conflict count {} exceeds threshold {}",
                    metrics.conflict_detection_count, t.max_conflicts
                ),
                timestamp: now,
            });
        }

        if metrics.token_cost_per_decision > t.max_token_cost {
            alerts.push(HealthAlert {
                metric: "token_cost_per_decision".to_string(),
                current_value: metrics.token_cost_per_decision,
                threshold: t.max_token_cost,
                message: format!(
                    "Token cost per decision {:.0} exceeds threshold {:.0}",
                    metrics.token_cost_per_decision, t.max_token_cost
                ),
                timestamp: now,
            });
        }

        if metrics.tool_error_rate > t.max_tool_error_rate {
            alerts.push(HealthAlert {
                metric: "tool_error_rate".to_string(),
                current_value: metrics.tool_error_rate,
                threshold: t.max_tool_error_rate,
                message: format!(
                    "Tool error rate {:.1}% exceeds threshold {:.1}%",
                    metrics.tool_error_rate * 100.0,
                    t.max_tool_error_rate * 100.0
                ),
                timestamp: now,
            });
        }

        alerts
    }
}

impl Default for ContextHealthMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrap in Arc for shared access from AppState.
pub type SharedContextHealth = Arc<ContextHealthMonitor>;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_metrics_are_healthy() {
        let monitor = ContextHealthMonitor::new();
        let snap = monitor.snapshot("sess-1").await;
        assert!(snap.alerts.is_empty());
        assert_eq!(snap.metrics.re_ask_rate, 0.0);
        assert_eq!(snap.metrics.thread_recall_accuracy, 1.0);
        assert_eq!(snap.metrics.tool_error_rate, 0.0);
    }

    #[tokio::test]
    async fn re_ask_rate_computed_correctly() {
        let monitor = ContextHealthMonitor::new();
        monitor.record_message("s1", false).await;
        monitor.record_message("s1", true).await;
        monitor.record_message("s1", false).await;
        monitor.record_message("s1", true).await;

        let snap = monitor.snapshot("s1").await;
        assert!((snap.metrics.re_ask_rate - 0.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn recall_accuracy_computed() {
        let monitor = ContextHealthMonitor::new();
        monitor.record_recall("s1", true).await;
        monitor.record_recall("s1", true).await;
        monitor.record_recall("s1", false).await;

        let snap = monitor.snapshot("s1").await;
        assert!((snap.metrics.thread_recall_accuracy - 2.0 / 3.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn tool_error_rate_computed() {
        let monitor = ContextHealthMonitor::new();
        monitor.record_tool_call("s1", false).await;
        monitor.record_tool_call("s1", true).await;
        monitor.record_tool_call("s1", false).await;
        monitor.record_tool_call("s1", false).await;

        let snap = monitor.snapshot("s1").await;
        assert!((snap.metrics.tool_error_rate - 0.25).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn token_cost_per_decision() {
        let monitor = ContextHealthMonitor::new();
        monitor.record_decision("s1", 500).await;
        monitor.record_decision("s1", 1500).await;

        let snap = monitor.snapshot("s1").await;
        assert!((snap.metrics.token_cost_per_decision - 1000.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn failure_events_recorded() {
        let monitor = ContextHealthMonitor::new();
        monitor
            .record_failure(
                "s1",
                FailureEvent {
                    mode: FailureMode::Burst,
                    description: "tool output too large".to_string(),
                    severity: 0.8,
                    timestamp: Utc::now(),
                },
            )
            .await;
        monitor
            .record_failure(
                "s1",
                FailureEvent {
                    mode: FailureMode::Conflict,
                    description: "contradictory facts".to_string(),
                    severity: 0.6,
                    timestamp: Utc::now(),
                },
            )
            .await;

        let snap = monitor.snapshot("s1").await;
        assert_eq!(snap.recent_failures.len(), 2);
        assert_eq!(snap.metrics.conflict_detection_count, 1);
    }

    #[tokio::test]
    async fn alerts_fire_when_thresholds_exceeded() {
        let thresholds = HealthThresholds {
            max_re_ask_rate: 0.1,
            ..Default::default()
        };
        let monitor = ContextHealthMonitor::with_thresholds(thresholds);

        // 50% re-ask rate
        monitor.record_message("s1", true).await;
        monitor.record_message("s1", false).await;

        let snap = monitor.snapshot("s1").await;
        assert!(!snap.alerts.is_empty());
        assert_eq!(snap.alerts[0].metric, "re_ask_rate");
    }

    #[tokio::test]
    async fn subscribe_receives_alerts() {
        let thresholds = HealthThresholds {
            max_tool_error_rate: 0.0,
            ..Default::default()
        };
        let monitor = ContextHealthMonitor::with_thresholds(thresholds);
        let mut rx = monitor.subscribe();

        monitor.record_tool_call("s1", true).await;
        let _snap = monitor.snapshot("s1").await;

        let alert = rx.try_recv().unwrap();
        assert_eq!(alert.metric, "tool_error_rate");
    }

    #[tokio::test]
    async fn reset_session_clears_counters() {
        let monitor = ContextHealthMonitor::new();
        monitor.record_message("s1", true).await;
        monitor.reset_session("s1").await;

        let snap = monitor.snapshot("s1").await;
        assert_eq!(snap.metrics.re_ask_rate, 0.0);
    }

    #[tokio::test]
    async fn tracked_sessions_lists_active() {
        let monitor = ContextHealthMonitor::new();
        monitor.record_message("s1", false).await;
        monitor.record_message("s2", false).await;

        let sessions = monitor.tracked_sessions().await;
        assert_eq!(sessions.len(), 2);
    }

    #[tokio::test]
    async fn custom_thresholds_applied() {
        let thresholds = HealthThresholds {
            max_re_ask_rate: 1.0,
            min_recall_accuracy: 0.0,
            max_conflicts: 100,
            max_token_cost: 999999.0,
            max_tool_error_rate: 1.0,
        };
        let monitor = ContextHealthMonitor::with_thresholds(thresholds);

        // Even with 100% re-ask rate, no alert due to max threshold of 1.0
        monitor.record_message("s1", true).await;
        // Also record a tool error — still no alert due to max_tool_error_rate=1.0
        monitor.record_tool_call("s1", true).await;
        let snap = monitor.snapshot("s1").await;
        assert!(snap.alerts.is_empty());
    }

    #[tokio::test]
    async fn multiple_sessions_independent() {
        let monitor = ContextHealthMonitor::new();
        monitor.record_message("s1", true).await;
        monitor.record_message("s1", true).await;
        monitor.record_message("s2", false).await;

        let s1 = monitor.snapshot("s1").await;
        let s2 = monitor.snapshot("s2").await;
        assert!((s1.metrics.re_ask_rate - 1.0).abs() < f64::EPSILON);
        assert!((s2.metrics.re_ask_rate - 0.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn recall_accuracy_alert_fires() {
        let thresholds = HealthThresholds {
            min_recall_accuracy: 0.8,
            ..Default::default()
        };
        let monitor = ContextHealthMonitor::with_thresholds(thresholds);

        // 0% accuracy
        monitor.record_recall("s1", false).await;
        monitor.record_recall("s1", false).await;

        let snap = monitor.snapshot("s1").await;
        assert!(snap
            .alerts
            .iter()
            .any(|a| a.metric == "thread_recall_accuracy"));
    }

    #[tokio::test]
    async fn conflict_count_alert() {
        let thresholds = HealthThresholds {
            max_conflicts: 1,
            ..Default::default()
        };
        let monitor = ContextHealthMonitor::with_thresholds(thresholds);

        for _ in 0..3 {
            monitor
                .record_failure(
                    "s1",
                    FailureEvent {
                        mode: FailureMode::Conflict,
                        description: "conflict".to_string(),
                        severity: 0.5,
                        timestamp: Utc::now(),
                    },
                )
                .await;
        }

        let snap = monitor.snapshot("s1").await;
        assert!(snap
            .alerts
            .iter()
            .any(|a| a.metric == "conflict_detection_count"));
    }

    #[tokio::test]
    async fn set_thresholds_at_runtime() {
        let monitor = ContextHealthMonitor::new();
        monitor.record_tool_call("s1", true).await;

        // Initially default threshold (0.2), 100% error rate -> alert
        let snap = monitor.snapshot("s1").await;
        assert!(!snap.alerts.is_empty());

        // Raise threshold to 1.0 -> no alert
        monitor
            .set_thresholds(HealthThresholds {
                max_tool_error_rate: 1.0,
                ..Default::default()
            })
            .await;
        let snap = monitor.snapshot("s1").await;
        assert!(snap.alerts.iter().all(|a| a.metric != "tool_error_rate"));
    }

    #[tokio::test]
    async fn recent_failures_limited_to_20() {
        let monitor = ContextHealthMonitor::new();
        for i in 0..30 {
            monitor
                .record_failure(
                    "s1",
                    FailureEvent {
                        mode: FailureMode::Noise,
                        description: format!("noise-{i}"),
                        severity: 0.1,
                        timestamp: Utc::now(),
                    },
                )
                .await;
        }

        let snap = monitor.snapshot("s1").await;
        assert_eq!(snap.recent_failures.len(), 20);
        // Should contain the latest ones
        assert_eq!(snap.recent_failures[0].description, "noise-10");
        assert_eq!(snap.recent_failures[19].description, "noise-29");
    }
}
