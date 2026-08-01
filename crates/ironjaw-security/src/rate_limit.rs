use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Per-method rate limit configuration.
#[derive(Debug, Clone)]
pub struct RateLimit {
    pub max_requests: u32,
    pub window_secs: u64,
}

/// Error returned when rate limit is exceeded.
#[derive(Debug, thiserror::Error)]
#[error("Rate limit exceeded for method '{method}': {max_requests} requests per {window_secs}s")]
pub struct RateLimitError {
    pub method: String,
    pub max_requests: u32,
    pub window_secs: u64,
}

/// Sliding window rate limiter. Tracks per-(connection, method) request timestamps.
pub struct RateLimiter {
    limits: HashMap<String, RateLimit>,
    default_limit: RateLimit,
    state: Mutex<HashMap<(String, String), VecDeque<Instant>>>,
}

impl RateLimiter {
    /// Create a new rate limiter with per-method limits and a default fallback.
    pub fn new(limits: HashMap<String, RateLimit>) -> Self {
        Self {
            limits,
            default_limit: RateLimit {
                max_requests: 120,
                window_secs: 60,
            },
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Create a rate limiter with sensible defaults for Ironjaw methods.
    pub fn with_defaults() -> Self {
        let mut limits = HashMap::new();
        limits.insert(
            "chat.send".to_string(),
            RateLimit {
                max_requests: 30,
                window_secs: 60,
            },
        );
        limits.insert(
            "session.create".to_string(),
            RateLimit {
                max_requests: 10,
                window_secs: 60,
            },
        );
        limits.insert(
            "tools.execute".to_string(),
            RateLimit {
                max_requests: 60,
                window_secs: 60,
            },
        );
        Self::new(limits)
    }

    /// Check if a request is within the rate limit. Registers the request if allowed.
    pub fn check(&self, conn_id: &str, method: &str) -> Result<(), RateLimitError> {
        let limit = self.limits.get(method).unwrap_or(&self.default_limit);
        let window = Duration::from_secs(limit.window_secs);
        let now = Instant::now();

        let mut state = self.state.lock().unwrap_or_else(|poisoned| {
            tracing::error!("Lock poisoned, recovering");
            poisoned.into_inner()
        });
        let key = (conn_id.to_string(), method.to_string());
        let timestamps = state.entry(key).or_default();

        // Prune expired entries
        while let Some(front) = timestamps.front() {
            if now.duration_since(*front) > window {
                timestamps.pop_front();
            } else {
                break;
            }
        }

        if timestamps.len() >= limit.max_requests as usize {
            return Err(RateLimitError {
                method: method.to_string(),
                max_requests: limit.max_requests,
                window_secs: limit.window_secs,
            });
        }

        timestamps.push_back(now);
        Ok(())
    }

    /// Remove all state for a connection (call on disconnect).
    pub fn cleanup_connection(&self, conn_id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| {
            tracing::error!("Lock poisoned, recovering");
            poisoned.into_inner()
        });
        state.retain(|(cid, _), _| cid != conn_id);
    }
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter")
            .field("limits", &self.limits)
            .finish()
    }
}

/// Per-agent rate limit state.
#[derive(Debug, Default)]
struct RateLimitState {
    timestamps: VecDeque<Instant>,
}

/// Rate limiter with per-agent-id tracking.
///
/// Wraps a `RateLimit` config and applies it independently per agent. Each agent
/// has its own sliding window, so one agent hitting its limit does not affect others.
#[derive(Debug)]
pub struct AgentRateLimiter {
    limit: RateLimit,
    state: HashMap<String, RateLimitState>,
}

impl AgentRateLimiter {
    /// Create a new agent rate limiter with the given limit.
    pub fn new(limit: RateLimit) -> Self {
        Self {
            limit,
            state: HashMap::new(),
        }
    }

    /// Create an agent rate limiter with sensible defaults (60 requests / 60s).
    pub fn with_defaults() -> Self {
        Self::new(RateLimit {
            max_requests: 60,
            window_secs: 60,
        })
    }

    /// Check if an agent's request is within its rate limit.
    ///
    /// Registers the request if allowed.
    pub fn check_agent_rate(&mut self, agent_id: &str) -> Result<(), RateLimitError> {
        let window = Duration::from_secs(self.limit.window_secs);
        let now = Instant::now();

        let state = self.state.entry(agent_id.to_string()).or_default();

        // Prune expired entries
        while let Some(front) = state.timestamps.front() {
            if now.duration_since(*front) > window {
                state.timestamps.pop_front();
            } else {
                break;
            }
        }

        if state.timestamps.len() >= self.limit.max_requests as usize {
            return Err(RateLimitError {
                method: format!("agent:{agent_id}"),
                max_requests: self.limit.max_requests,
                window_secs: self.limit.window_secs,
            });
        }

        state.timestamps.push_back(now);
        Ok(())
    }

    /// Remove all state for an agent.
    pub fn cleanup_agent(&mut self, agent_id: &str) {
        self.state.remove(agent_id);
    }

    /// Remove agents with no recent activity (all timestamps expired).
    pub fn cleanup_stale(&mut self) {
        let window = Duration::from_secs(self.limit.window_secs);
        let now = Instant::now();

        self.state.retain(|_, state| {
            // Prune expired entries first
            while let Some(front) = state.timestamps.front() {
                if now.duration_since(*front) > window {
                    state.timestamps.pop_front();
                } else {
                    break;
                }
            }
            !state.timestamps.is_empty()
        });
    }

    /// Returns the number of tracked agents.
    pub fn tracked_agents(&self) -> usize {
        self.state.len()
    }
}

/// Tracks 429 response counts per agent and computes exponential backoff multipliers.
///
/// After consecutive 429 responses, the backoff factor doubles: 1.0, 2.0, 4.0, 8.0 (capped).
/// A successful response resets the counter.
#[derive(Debug)]
pub struct DeprioritizationTracker {
    /// agent_id -> consecutive 429 count
    consecutive_429s: HashMap<String, u32>,
    /// Maximum backoff factor (2^max_exponent). Default caps at 8.0.
    max_exponent: u32,
}

impl DeprioritizationTracker {
    /// Create a new deprioritization tracker.
    pub fn new() -> Self {
        Self {
            consecutive_429s: HashMap::new(),
            max_exponent: 3, // 2^3 = 8.0 max
        }
    }

    /// Create a tracker with a custom max exponent.
    pub fn with_max_exponent(max_exponent: u32) -> Self {
        Self {
            consecutive_429s: HashMap::new(),
            max_exponent,
        }
    }

    /// Record a 429 response for an agent, incrementing the consecutive count.
    pub fn record_429(&mut self, agent_id: &str) {
        let count = self
            .consecutive_429s
            .entry(agent_id.to_string())
            .or_insert(0);
        *count = count.saturating_add(1);
    }

    /// Record a successful response for an agent, resetting the backoff.
    pub fn record_success(&mut self, agent_id: &str) {
        self.consecutive_429s.remove(agent_id);
    }

    /// Get the backoff factor for an agent.
    ///
    /// Returns 1.0 (no backoff) if no 429s recorded, otherwise 2^min(count, max_exponent).
    pub fn backoff_factor(&self, agent_id: &str) -> f64 {
        match self.consecutive_429s.get(agent_id) {
            None | Some(0) => 1.0,
            Some(&count) => {
                let exponent = count.min(self.max_exponent);
                2f64.powi(exponent as i32)
            }
        }
    }

    /// Returns the number of agents with active backoff.
    pub fn agents_with_backoff(&self) -> usize {
        self.consecutive_429s.values().filter(|&&c| c > 0).count()
    }

    /// Remove tracking state for an agent.
    pub fn cleanup_agent(&mut self, agent_id: &str) {
        self.consecutive_429s.remove(agent_id);
    }
}

impl Default for DeprioritizationTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_under_limit_passes() {
        let limiter = RateLimiter::new(HashMap::from([(
            "ping".to_string(),
            RateLimit {
                max_requests: 3,
                window_secs: 60,
            },
        )]));

        assert!(limiter.check("conn1", "ping").is_ok());
        assert!(limiter.check("conn1", "ping").is_ok());
        assert!(limiter.check("conn1", "ping").is_ok());
    }

    #[test]
    fn test_over_limit_blocked() {
        let limiter = RateLimiter::new(HashMap::from([(
            "ping".to_string(),
            RateLimit {
                max_requests: 2,
                window_secs: 60,
            },
        )]));

        assert!(limiter.check("conn1", "ping").is_ok());
        assert!(limiter.check("conn1", "ping").is_ok());
        let err = limiter.check("conn1", "ping").unwrap_err();
        assert_eq!(err.method, "ping");
        assert_eq!(err.max_requests, 2);
    }

    #[test]
    fn test_different_connections_independent() {
        let limiter = RateLimiter::new(HashMap::from([(
            "ping".to_string(),
            RateLimit {
                max_requests: 1,
                window_secs: 60,
            },
        )]));

        assert!(limiter.check("conn1", "ping").is_ok());
        assert!(limiter.check("conn2", "ping").is_ok());
        assert!(limiter.check("conn1", "ping").is_err());
    }

    #[test]
    fn test_default_limit_for_unknown_method() {
        let limiter = RateLimiter::new(HashMap::new());
        // Default is 120/60s
        for _ in 0..120 {
            assert!(limiter.check("conn1", "unknown.method").is_ok());
        }
        assert!(limiter.check("conn1", "unknown.method").is_err());
    }

    #[test]
    fn test_cleanup_connection() {
        let limiter = RateLimiter::new(HashMap::from([(
            "ping".to_string(),
            RateLimit {
                max_requests: 1,
                window_secs: 60,
            },
        )]));

        assert!(limiter.check("conn1", "ping").is_ok());
        assert!(limiter.check("conn1", "ping").is_err());

        limiter.cleanup_connection("conn1");
        assert!(limiter.check("conn1", "ping").is_ok());
    }

    // --- AgentRateLimiter tests ---

    #[test]
    fn test_agent_under_limit() {
        let mut limiter = AgentRateLimiter::new(RateLimit {
            max_requests: 3,
            window_secs: 60,
        });
        assert!(limiter.check_agent_rate("agent-1").is_ok());
        assert!(limiter.check_agent_rate("agent-1").is_ok());
        assert!(limiter.check_agent_rate("agent-1").is_ok());
    }

    #[test]
    fn test_agent_over_limit() {
        let mut limiter = AgentRateLimiter::new(RateLimit {
            max_requests: 2,
            window_secs: 60,
        });
        assert!(limiter.check_agent_rate("agent-1").is_ok());
        assert!(limiter.check_agent_rate("agent-1").is_ok());
        let err = limiter.check_agent_rate("agent-1").unwrap_err();
        assert!(err.method.contains("agent-1"));
    }

    #[test]
    fn test_agents_independent() {
        let mut limiter = AgentRateLimiter::new(RateLimit {
            max_requests: 1,
            window_secs: 60,
        });
        assert!(limiter.check_agent_rate("agent-1").is_ok());
        assert!(limiter.check_agent_rate("agent-2").is_ok());
        // agent-1 is at limit
        assert!(limiter.check_agent_rate("agent-1").is_err());
        // agent-2 is also at limit
        assert!(limiter.check_agent_rate("agent-2").is_err());
    }

    #[test]
    fn test_agent_cleanup() {
        let mut limiter = AgentRateLimiter::new(RateLimit {
            max_requests: 1,
            window_secs: 60,
        });
        assert!(limiter.check_agent_rate("agent-1").is_ok());
        assert!(limiter.check_agent_rate("agent-1").is_err());

        limiter.cleanup_agent("agent-1");
        assert!(limiter.check_agent_rate("agent-1").is_ok());
    }

    #[test]
    fn test_agent_tracked_count() {
        let mut limiter = AgentRateLimiter::with_defaults();
        assert_eq!(limiter.tracked_agents(), 0);

        limiter.check_agent_rate("a1").unwrap();
        assert_eq!(limiter.tracked_agents(), 1);

        limiter.check_agent_rate("a2").unwrap();
        assert_eq!(limiter.tracked_agents(), 2);

        limiter.cleanup_agent("a1");
        assert_eq!(limiter.tracked_agents(), 1);
    }

    #[test]
    fn test_agent_cleanup_stale() {
        let mut limiter = AgentRateLimiter::new(RateLimit {
            max_requests: 100,
            window_secs: 1, // 1-second window
        });
        limiter.check_agent_rate("a1").unwrap();
        assert_eq!(limiter.tracked_agents(), 1);

        // Manually expire the timestamp
        if let Some(state) = limiter.state.get_mut("a1") {
            if let Some(ts) = state.timestamps.front_mut() {
                *ts = Instant::now() - Duration::from_secs(5);
            }
        }

        limiter.cleanup_stale();
        assert_eq!(limiter.tracked_agents(), 0);
    }

    // --- DeprioritizationTracker tests ---

    #[test]
    fn test_backoff_no_429s() {
        let tracker = DeprioritizationTracker::new();
        assert_eq!(tracker.backoff_factor("agent-1"), 1.0);
    }

    #[test]
    fn test_backoff_escalation() {
        let mut tracker = DeprioritizationTracker::new();

        tracker.record_429("a1");
        assert_eq!(tracker.backoff_factor("a1"), 2.0);

        tracker.record_429("a1");
        assert_eq!(tracker.backoff_factor("a1"), 4.0);

        tracker.record_429("a1");
        assert_eq!(tracker.backoff_factor("a1"), 8.0);
    }

    #[test]
    fn test_backoff_caps_at_max() {
        let mut tracker = DeprioritizationTracker::new(); // max_exponent=3 => 8.0

        for _ in 0..10 {
            tracker.record_429("a1");
        }
        assert_eq!(tracker.backoff_factor("a1"), 8.0);
    }

    #[test]
    fn test_backoff_custom_max() {
        let mut tracker = DeprioritizationTracker::with_max_exponent(5); // 2^5 = 32.0

        for _ in 0..5 {
            tracker.record_429("a1");
        }
        assert_eq!(tracker.backoff_factor("a1"), 32.0);

        // Should still cap
        tracker.record_429("a1");
        assert_eq!(tracker.backoff_factor("a1"), 32.0);
    }

    #[test]
    fn test_backoff_reset_after_success() {
        let mut tracker = DeprioritizationTracker::new();

        tracker.record_429("a1");
        tracker.record_429("a1");
        assert_eq!(tracker.backoff_factor("a1"), 4.0);

        tracker.record_success("a1");
        assert_eq!(tracker.backoff_factor("a1"), 1.0);
    }

    #[test]
    fn test_backoff_agents_independent() {
        let mut tracker = DeprioritizationTracker::new();

        tracker.record_429("a1");
        tracker.record_429("a1");
        tracker.record_429("a2");

        assert_eq!(tracker.backoff_factor("a1"), 4.0);
        assert_eq!(tracker.backoff_factor("a2"), 2.0);
        assert_eq!(tracker.backoff_factor("a3"), 1.0);
    }

    #[test]
    fn test_backoff_agents_count() {
        let mut tracker = DeprioritizationTracker::new();
        assert_eq!(tracker.agents_with_backoff(), 0);

        tracker.record_429("a1");
        assert_eq!(tracker.agents_with_backoff(), 1);

        tracker.record_429("a2");
        assert_eq!(tracker.agents_with_backoff(), 2);

        tracker.record_success("a1");
        assert_eq!(tracker.agents_with_backoff(), 1);
    }

    #[test]
    fn test_backoff_cleanup_agent() {
        let mut tracker = DeprioritizationTracker::new();
        tracker.record_429("a1");
        tracker.record_429("a1");

        tracker.cleanup_agent("a1");
        assert_eq!(tracker.backoff_factor("a1"), 1.0);
        assert_eq!(tracker.agents_with_backoff(), 0);
    }
}
