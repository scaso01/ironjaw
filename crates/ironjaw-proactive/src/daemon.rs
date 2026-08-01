//! Autonomous daemon wrapper — persistent background agent that acts without
//! user input. Heartbeat-driven: periodically asks "anything worth doing right now?"

use chrono::Utc;

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub enabled: bool,
    pub heartbeat_secs: u64,
    pub max_proactive_budget_secs: u64,
    pub log_dir: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            heartbeat_secs: 300,           // 5 minutes
            max_proactive_budget_secs: 15, // 15 seconds per proactive action
            log_dir: "data/proactive".to_string(),
        }
    }
}

impl DaemonConfig {
    pub fn daily_log_path(&self) -> String {
        let date = Utc::now().format("%Y-%m-%d");
        format!("{}/proactive-{}.log", self.log_dir, date)
    }
}

pub struct ProactiveDaemon {
    config: DaemonConfig,
    last_heartbeat: Option<chrono::DateTime<Utc>>,
}

impl ProactiveDaemon {
    pub fn new(config: DaemonConfig) -> Self {
        Self {
            config,
            last_heartbeat: None,
        }
    }

    pub fn heartbeat_prompt(&self) -> String {
        format!(
            "You are a proactive daemon. Check if there is anything worth doing right now.\n\
             Budget: {} second maximum for any action.\n\
             Rules:\n\
             - Only act on time-sensitive items (expiring schedules, stale monitors)\n\
             - Never modify code without explicit prior approval\n\
             - Log all actions to the daily log\n\
             - If nothing needs attention, respond with IDLE\n\n\
             What needs attention?",
            self.config.max_proactive_budget_secs,
        )
    }

    pub fn should_heartbeat(&self) -> bool {
        if !self.config.enabled {
            return false;
        }
        match self.last_heartbeat {
            None => true,
            Some(last) => {
                let elapsed = Utc::now().signed_duration_since(last).num_seconds();
                elapsed >= self.config.heartbeat_secs as i64
            }
        }
    }

    pub fn mark_heartbeat(&mut self) {
        self.last_heartbeat = Some(Utc::now());
    }

    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_config_defaults() {
        let config = DaemonConfig::default();
        assert_eq!(config.heartbeat_secs, 300); // 5 minutes
        assert_eq!(config.max_proactive_budget_secs, 15);
        assert!(config.enabled);
    }

    #[test]
    fn test_daemon_heartbeat_generates_action_check() {
        let daemon = ProactiveDaemon::new(DaemonConfig::default());
        let prompt = daemon.heartbeat_prompt();
        assert!(prompt.contains("anything worth doing"));
        assert!(prompt.contains("15 second"));
    }

    #[test]
    fn test_daemon_daily_log_path() {
        let config = DaemonConfig::default();
        let path = config.daily_log_path();
        assert!(path.contains("proactive"));
        assert!(path.ends_with(".log"));
    }

    #[test]
    fn test_daemon_should_heartbeat_initially() {
        let daemon = ProactiveDaemon::new(DaemonConfig::default());
        assert!(daemon.should_heartbeat());
    }

    #[test]
    fn test_daemon_should_not_heartbeat_after_mark() {
        let mut daemon = ProactiveDaemon::new(DaemonConfig::default());
        daemon.mark_heartbeat();
        // Just marked — not enough time passed
        assert!(!daemon.should_heartbeat());
    }

    #[test]
    fn test_daemon_disabled_no_heartbeat() {
        let config = DaemonConfig {
            enabled: false,
            ..Default::default()
        };
        let daemon = ProactiveDaemon::new(config);
        assert!(!daemon.should_heartbeat());
    }
}
