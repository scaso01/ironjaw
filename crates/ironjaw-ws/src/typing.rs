// Typing indicator tracking — per-session, per-user typing state with expiry.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Default typing timeout (5 seconds).
const DEFAULT_TYPING_TIMEOUT_SECS: u64 = 5;

/// Tracks per-user typing state within sessions.
#[derive(Debug)]
pub struct TypingIndicator {
    /// session_id -> (user_id -> last_typing_time)
    state: HashMap<String, HashMap<String, Instant>>,
}

/// Events emitted when typing state changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TypingEvent {
    Started { session_id: String, user_id: String },
    Stopped { session_id: String, user_id: String },
}

impl TypingIndicator {
    /// Create a new typing indicator tracker.
    pub fn new() -> Self {
        Self {
            state: HashMap::new(),
        }
    }

    /// Record that a user has started (or is continuing) typing in a session.
    pub fn start_typing(&mut self, session_id: &str, user_id: &str) {
        self.state
            .entry(session_id.to_string())
            .or_default()
            .insert(user_id.to_string(), Instant::now());
    }

    /// Record that a user has stopped typing in a session.
    pub fn stop_typing(&mut self, session_id: &str, user_id: &str) {
        if let Some(users) = self.state.get_mut(session_id) {
            users.remove(user_id);
            if users.is_empty() {
                self.state.remove(session_id);
            }
        }
    }

    /// Get the list of users currently typing in a session.
    ///
    /// Does NOT filter by expiry — call `cleanup_expired` first for accurate results.
    pub fn get_typing_users(&self, session_id: &str) -> Vec<String> {
        self.state
            .get(session_id)
            .map(|users| users.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Remove all typing entries older than the given timeout.
    ///
    /// Returns the list of events for entries that were cleaned up.
    pub fn cleanup_expired(&mut self, timeout: Duration) -> Vec<TypingEvent> {
        let now = Instant::now();
        let mut events = Vec::new();

        self.state.retain(|session_id, users| {
            users.retain(|user_id, last_typing| {
                let expired = now.duration_since(*last_typing) > timeout;
                if expired {
                    events.push(TypingEvent::Stopped {
                        session_id: session_id.clone(),
                        user_id: user_id.clone(),
                    });
                }
                !expired
            });
            !users.is_empty()
        });

        events
    }

    /// Remove all typing state for a session (call on session close).
    pub fn cleanup_session(&mut self, session_id: &str) {
        self.state.remove(session_id);
    }

    /// Returns the default typing timeout duration.
    pub fn default_timeout() -> Duration {
        Duration::from_secs(DEFAULT_TYPING_TIMEOUT_SECS)
    }

    /// Returns the number of sessions with active typing indicators.
    pub fn active_sessions(&self) -> usize {
        self.state.len()
    }

    /// Check if a specific user is typing in a session.
    pub fn is_typing(&self, session_id: &str, user_id: &str) -> bool {
        self.state
            .get(session_id)
            .and_then(|users| users.get(user_id))
            .is_some()
    }
}

impl Default for TypingIndicator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_start_typing() {
        let mut ti = TypingIndicator::new();
        ti.start_typing("s1", "alice");
        assert!(ti.is_typing("s1", "alice"));
        assert!(!ti.is_typing("s1", "bob"));
    }

    #[test]
    fn test_stop_typing() {
        let mut ti = TypingIndicator::new();
        ti.start_typing("s1", "alice");
        ti.stop_typing("s1", "alice");
        assert!(!ti.is_typing("s1", "alice"));
        assert_eq!(ti.active_sessions(), 0);
    }

    #[test]
    fn test_stop_typing_nonexistent() {
        let mut ti = TypingIndicator::new();
        // Should not panic
        ti.stop_typing("s1", "ghost");
        assert_eq!(ti.active_sessions(), 0);
    }

    #[test]
    fn test_get_typing_users_empty() {
        let ti = TypingIndicator::new();
        let users = ti.get_typing_users("s1");
        assert!(users.is_empty());
    }

    #[test]
    fn test_get_typing_users_multiple() {
        let mut ti = TypingIndicator::new();
        ti.start_typing("s1", "alice");
        ti.start_typing("s1", "bob");
        ti.start_typing("s1", "carol");

        let mut users = ti.get_typing_users("s1");
        users.sort();
        assert_eq!(users, vec!["alice", "bob", "carol"]);
    }

    #[test]
    fn test_get_typing_users_filters_stopped() {
        let mut ti = TypingIndicator::new();
        ti.start_typing("s1", "alice");
        ti.start_typing("s1", "bob");
        ti.stop_typing("s1", "alice");

        let users = ti.get_typing_users("s1");
        assert_eq!(users, vec!["bob"]);
    }

    #[test]
    fn test_multiple_sessions_independent() {
        let mut ti = TypingIndicator::new();
        ti.start_typing("s1", "alice");
        ti.start_typing("s2", "bob");

        assert!(ti.is_typing("s1", "alice"));
        assert!(!ti.is_typing("s1", "bob"));
        assert!(ti.is_typing("s2", "bob"));
        assert!(!ti.is_typing("s2", "alice"));
        assert_eq!(ti.active_sessions(), 2);
    }

    #[test]
    fn test_cleanup_expired_removes_old_entries() {
        let mut ti = TypingIndicator::new();
        ti.start_typing("s1", "alice");

        // Manually set the typing time to the past
        if let Some(users) = ti.state.get_mut("s1") {
            if let Some(instant) = users.get_mut("alice") {
                *instant = Instant::now() - Duration::from_secs(10);
            }
        }

        let events = ti.cleanup_expired(Duration::from_secs(5));
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            TypingEvent::Stopped {
                session_id: "s1".to_string(),
                user_id: "alice".to_string(),
            }
        );
        assert!(!ti.is_typing("s1", "alice"));
        assert_eq!(ti.active_sessions(), 0);
    }

    #[test]
    fn test_cleanup_expired_keeps_recent() {
        let mut ti = TypingIndicator::new();
        ti.start_typing("s1", "alice");

        let events = ti.cleanup_expired(Duration::from_secs(5));
        assert!(events.is_empty());
        assert!(ti.is_typing("s1", "alice"));
    }

    #[test]
    fn test_cleanup_expired_mixed() {
        let mut ti = TypingIndicator::new();
        ti.start_typing("s1", "alice");
        ti.start_typing("s1", "bob");

        // Make alice's entry old
        if let Some(users) = ti.state.get_mut("s1") {
            if let Some(instant) = users.get_mut("alice") {
                *instant = Instant::now() - Duration::from_secs(10);
            }
        }

        let events = ti.cleanup_expired(Duration::from_secs(5));
        assert_eq!(events.len(), 1);
        assert!(!ti.is_typing("s1", "alice"));
        assert!(ti.is_typing("s1", "bob"));
    }

    #[test]
    fn test_typing_event_serialization() {
        let event = TypingEvent::Started {
            session_id: "s1".to_string(),
            user_id: "alice".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "started");
        assert_eq!(json["session_id"], "s1");
        assert_eq!(json["user_id"], "alice");

        let event = TypingEvent::Stopped {
            session_id: "s1".to_string(),
            user_id: "bob".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "stopped");
        assert_eq!(json["user_id"], "bob");
    }

    #[test]
    fn test_typing_event_deserialization() {
        let json = serde_json::json!({
            "type": "started",
            "session_id": "s1",
            "user_id": "alice"
        });
        let event: TypingEvent = serde_json::from_value(json).unwrap();
        assert_eq!(
            event,
            TypingEvent::Started {
                session_id: "s1".to_string(),
                user_id: "alice".to_string(),
            }
        );
    }

    #[test]
    fn test_cleanup_session() {
        let mut ti = TypingIndicator::new();
        ti.start_typing("s1", "alice");
        ti.start_typing("s1", "bob");
        ti.start_typing("s2", "carol");

        ti.cleanup_session("s1");
        assert_eq!(ti.active_sessions(), 1);
        assert!(!ti.is_typing("s1", "alice"));
        assert!(!ti.is_typing("s1", "bob"));
        assert!(ti.is_typing("s2", "carol"));
    }

    #[test]
    fn test_default_timeout() {
        let timeout = TypingIndicator::default_timeout();
        assert_eq!(timeout, Duration::from_secs(5));
    }

    #[test]
    fn test_start_typing_updates_timestamp() {
        let mut ti = TypingIndicator::new();
        ti.start_typing("s1", "alice");

        let first = *ti.state.get("s1").unwrap().get("alice").unwrap();

        // Re-start typing (simulates continued typing)
        std::thread::sleep(Duration::from_millis(10));
        ti.start_typing("s1", "alice");

        let second = *ti.state.get("s1").unwrap().get("alice").unwrap();
        assert!(second > first);
    }
}
