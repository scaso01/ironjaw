//! Active session tracker for Claude Code subprocesses.
//!
//! [`ActiveSessionTracker`] is a registry of running CC subprocesses with
//! concurrency limits enforced by a tokio [`Semaphore`].

use crate::runner::ClaudeRunner;
use crate::types::{ActiveSession, CcConfig, CcStreamEvent, ClaudeError};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, OwnedSemaphorePermit, RwLock, Semaphore};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Internal record for a tracked runner and its concurrency permit.
struct TrackedRunner {
    runner: ClaudeRunner,
    cwd: String,
    started_at: chrono::DateTime<Utc>,
    _permit: OwnedSemaphorePermit,
}

/// Registry of running Claude Code subprocesses with concurrency limits.
///
/// The tracker enforces a maximum number of concurrent sessions via a
/// [`Semaphore`]. Each spawned runner holds an owned permit; when the
/// runner is removed (abort, completion, shutdown) the permit is
/// automatically released.
pub struct ActiveSessionTracker {
    sessions: Arc<RwLock<HashMap<String, TrackedRunner>>>,
    semaphore: Arc<Semaphore>,
    max_concurrent: u32,
}

impl ActiveSessionTracker {
    /// Create a new tracker allowing up to `max_concurrent` sessions.
    pub fn new(max_concurrent: u32) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            semaphore: Arc::new(Semaphore::new(max_concurrent as usize)),
            max_concurrent,
        }
    }

    /// Spawn a new CC subprocess and return `(tracking_key, event_receiver)`.
    ///
    /// Returns [`ClaudeError::BridgeError`] if the concurrency limit is reached.
    pub async fn spawn(
        &self,
        config: &CcConfig,
        session_id: Option<&str>,
        message: &str,
        cwd: &str,
        effort: Option<&str>,
    ) -> Result<(String, broadcast::Receiver<CcStreamEvent>), ClaudeError> {
        // Acquire semaphore permit (fail fast).
        let permit = Arc::clone(&self.semaphore)
            .try_acquire_owned()
            .map_err(|_| {
                ClaudeError::BridgeError(
                    "MAX_CONCURRENT: maximum concurrent sessions reached".into(),
                )
            })?;

        // Generate tracking key.
        let tracking_key = session_id
            .map(|s| s.to_string())
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        info!(key = %tracking_key, "spawning CC subprocess");

        // Spawn the runner.
        let runner = ClaudeRunner::spawn(config, session_id, message, cwd, effort).await?;

        // Subscribe before storing so we don't miss early events.
        let rx = runner.subscribe();

        let tracked = TrackedRunner {
            runner,
            cwd: cwd.to_string(),
            started_at: Utc::now(),
            _permit: permit,
        };

        // Store in the map.
        {
            let mut map = self.sessions.write().await;
            map.insert(tracking_key.clone(), tracked);
        }

        // Spawn background cleanup task.
        let sessions = Arc::clone(&self.sessions);
        let key = tracking_key.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let should_remove = {
                    let map = sessions.read().await;
                    match map.get(&key) {
                        Some(t) => !t.runner.is_running(),
                        None => break, // already removed
                    }
                };
                if should_remove {
                    let mut map = sessions.write().await;
                    if let Some(t) = map.get(&key) {
                        if !t.runner.is_running() {
                            map.remove(&key);
                            debug!(key = %key, "cleanup: removed finished session");
                        }
                    }
                    break;
                }
            }
        });

        Ok((tracking_key, rx))
    }

    /// Attach to an already-running session, returning an event receiver.
    pub async fn attach(
        &self,
        session_id: &str,
    ) -> Result<broadcast::Receiver<CcStreamEvent>, ClaudeError> {
        let map = self.sessions.read().await;
        match map.get(session_id) {
            Some(tracked) if tracked.runner.is_running() => {
                debug!(session_id = %session_id, "attaching to running session");
                Ok(tracked.runner.subscribe())
            }
            Some(_) => {
                warn!(session_id = %session_id, "session found but not running");
                Err(ClaudeError::SessionNotFound(session_id.to_string()))
            }
            None => Err(ClaudeError::SessionNotFound(session_id.to_string())),
        }
    }

    /// Abort a running session by killing its subprocess.
    pub async fn abort(&self, session_id: &str) -> Result<(), ClaudeError> {
        let mut map = self.sessions.write().await;
        match map.remove(session_id) {
            Some(tracked) => {
                info!(session_id = %session_id, "aborting session");
                tracked.runner.abort().await?;
                // Permit is dropped here, releasing the semaphore slot.
                Ok(())
            }
            None => Err(ClaudeError::SessionNotFound(session_id.to_string())),
        }
    }

    /// List all active (running) sessions.
    pub async fn list_active(&self) -> Vec<ActiveSession> {
        let map = self.sessions.read().await;
        map.iter()
            .filter(|(_, t)| t.runner.is_running())
            .map(|(key, t)| ActiveSession {
                session_id: key.clone(),
                cwd: t.cwd.clone(),
                started_at: t.started_at,
                pid: t.runner.pid().unwrap_or(0),
            })
            .collect()
    }

    /// Shutdown: abort all running subprocesses and clear the registry.
    pub async fn shutdown(&self) {
        let mut map = self.sessions.write().await;
        for (key, tracked) in map.drain() {
            info!(session_id = %key, "shutdown: aborting session");
            if let Err(e) = tracked.runner.abort().await {
                warn!(session_id = %key, error = %e, "shutdown: failed to abort");
            }
            // Permit dropped here.
        }
    }

    /// Return the configured maximum concurrent session count.
    pub fn max_concurrent(&self) -> u32 {
        self.max_concurrent
    }

    /// Return the number of currently tracked sessions.
    pub async fn active_count(&self) -> usize {
        let map = self.sessions.read().await;
        map.len()
    }

    /// Return the number of available semaphore permits.
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Re-key a session: remove the entry under `old` and re-insert under `new`.
    ///
    /// Used when a new session's tracking key (random UUID) is replaced by the
    /// real CC session UUID extracted from the `result` event. If `old` is not
    /// found, this is a no-op.
    pub async fn update_key(&self, old: &str, new: &str) {
        let mut map = self.sessions.write().await;
        if let Some(tracked) = map.remove(old) {
            debug!(old = %old, new = %new, "re-keying session");
            map.insert(new.to_string(), tracked);
        }
    }

    /// Get the session ID extracted from the CC subprocess result event.
    ///
    /// Returns `None` if the session is not found or the runner has not yet
    /// received a result event.
    pub async fn get_session_id(&self, key: &str) -> Option<String> {
        let map = self.sessions.read().await;
        map.get(key).and_then(|t| t.runner.session_id())
    }

    /// Get the working directory for a tracked session.
    pub async fn get_cwd(&self, key: &str) -> Option<String> {
        let map = self.sessions.read().await;
        map.get(key).map(|t| t.cwd.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> CcConfig {
        CcConfig {
            enabled: true,
            claude_cmd: "claude".to_string(),
            default_cwd: "/tmp".to_string(),
            sessions_dir: "/tmp/.claude/projects".to_string(),
            timeout_ms: 60_000,
            idle_timeout_ms: 30_000,
            max_concurrent: 3,
        }
    }

    // -------------------------------------------------------------------
    // Construction tests
    // -------------------------------------------------------------------

    #[test]
    fn test_new_creates_tracker_with_capacity() {
        let tracker = ActiveSessionTracker::new(5);
        assert_eq!(tracker.max_concurrent(), 5);
        assert_eq!(tracker.available_permits(), 5);
    }

    #[test]
    fn test_new_with_zero_capacity() {
        let tracker = ActiveSessionTracker::new(0);
        assert_eq!(tracker.max_concurrent(), 0);
        assert_eq!(tracker.available_permits(), 0);
    }

    #[test]
    fn test_new_with_one_capacity() {
        let tracker = ActiveSessionTracker::new(1);
        assert_eq!(tracker.max_concurrent(), 1);
        assert_eq!(tracker.available_permits(), 1);
    }

    #[test]
    fn test_new_with_large_capacity() {
        let tracker = ActiveSessionTracker::new(100);
        assert_eq!(tracker.max_concurrent(), 100);
        assert_eq!(tracker.available_permits(), 100);
    }

    #[tokio::test]
    async fn test_list_active_empty_on_fresh_tracker() {
        let tracker = ActiveSessionTracker::new(3);
        let active = tracker.list_active().await;
        assert!(active.is_empty());
    }

    #[tokio::test]
    async fn test_active_count_zero_on_fresh_tracker() {
        let tracker = ActiveSessionTracker::new(3);
        assert_eq!(tracker.active_count().await, 0);
    }

    // -------------------------------------------------------------------
    // Error path tests
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_attach_nonexistent_returns_session_not_found() {
        let tracker = ActiveSessionTracker::new(3);
        let result = tracker.attach("nonexistent-id").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ClaudeError::SessionNotFound(id) => assert_eq!(id, "nonexistent-id"),
            other => panic!("expected SessionNotFound, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_abort_nonexistent_returns_session_not_found() {
        let tracker = ActiveSessionTracker::new(3);
        let result = tracker.abort("nonexistent-id").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ClaudeError::SessionNotFound(id) => assert_eq!(id, "nonexistent-id"),
            other => panic!("expected SessionNotFound, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_spawn_with_zero_capacity_returns_max_concurrent() {
        let tracker = ActiveSessionTracker::new(0);
        let config = make_config();
        let result = tracker.spawn(&config, None, "test", "/tmp", None).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ClaudeError::BridgeError(msg) => {
                assert!(
                    msg.starts_with("MAX_CONCURRENT"),
                    "error should start with MAX_CONCURRENT, got: {msg}"
                );
            }
            other => panic!("expected BridgeError, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_attach_empty_string_session_id_returns_not_found() {
        let tracker = ActiveSessionTracker::new(3);
        let result = tracker.attach("").await;
        assert!(matches!(result, Err(ClaudeError::SessionNotFound(_))));
    }

    #[tokio::test]
    async fn test_abort_empty_string_session_id_returns_not_found() {
        let tracker = ActiveSessionTracker::new(3);
        let result = tracker.abort("").await;
        assert!(matches!(result, Err(ClaudeError::SessionNotFound(_))));
    }

    // -------------------------------------------------------------------
    // Semaphore / capacity tests
    // -------------------------------------------------------------------

    #[test]
    fn test_semaphore_capacity_matches_max_concurrent() {
        let tracker = ActiveSessionTracker::new(7);
        assert_eq!(tracker.available_permits(), 7);
    }

    #[test]
    fn test_available_permits_initial_equals_max() {
        for cap in [1, 2, 5, 10, 50] {
            let tracker = ActiveSessionTracker::new(cap);
            assert_eq!(tracker.available_permits(), cap as usize);
        }
    }

    // -------------------------------------------------------------------
    // Shutdown tests
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_shutdown_on_empty_tracker_is_noop() {
        let tracker = ActiveSessionTracker::new(3);
        tracker.shutdown().await;
        assert_eq!(tracker.active_count().await, 0);
        assert_eq!(tracker.available_permits(), 3);
    }

    #[tokio::test]
    async fn test_double_shutdown_is_safe() {
        let tracker = ActiveSessionTracker::new(3);
        tracker.shutdown().await;
        tracker.shutdown().await;
        assert_eq!(tracker.active_count().await, 0);
    }

    #[tokio::test]
    async fn test_abort_after_shutdown_returns_not_found() {
        let tracker = ActiveSessionTracker::new(3);
        // No sessions to populate since ClaudeRunner::spawn needs a real
        // binary, but we can verify shutdown + abort on empty is correct.
        tracker.shutdown().await;
        let result = tracker.abort("any-id").await;
        assert!(matches!(result, Err(ClaudeError::SessionNotFound(_))));
    }

    // -------------------------------------------------------------------
    // TrackedRunner struct tests
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_tracked_runner_fields() {
        // Verify TrackedRunner can be constructed with the right fields.
        // We test this indirectly by verifying the semaphore permit behavior.
        let sem = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&sem).try_acquire_owned().unwrap();
        assert_eq!(sem.available_permits(), 0);

        // Drop the permit manually
        drop(permit);
        assert_eq!(sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn test_owned_permit_released_on_drop() {
        let sem = Arc::new(Semaphore::new(2));
        let p1 = Arc::clone(&sem).try_acquire_owned().unwrap();
        let p2 = Arc::clone(&sem).try_acquire_owned().unwrap();
        assert_eq!(sem.available_permits(), 0);

        // Third acquire should fail
        assert!(Arc::clone(&sem).try_acquire_owned().is_err());

        drop(p1);
        assert_eq!(sem.available_permits(), 1);

        drop(p2);
        assert_eq!(sem.available_permits(), 2);
    }

    // -------------------------------------------------------------------
    // Config helper test
    // -------------------------------------------------------------------

    // -------------------------------------------------------------------
    // update_key / get_session_id / get_cwd tests
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_update_key_on_empty_tracker_is_noop() {
        let tracker = ActiveSessionTracker::new(3);
        // Should not panic or error on missing key.
        tracker.update_key("missing", "new-key").await;
        assert_eq!(tracker.active_count().await, 0);
    }

    #[tokio::test]
    async fn test_get_session_id_returns_none_on_empty_tracker() {
        let tracker = ActiveSessionTracker::new(3);
        assert!(tracker.get_session_id("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_get_cwd_returns_none_on_empty_tracker() {
        let tracker = ActiveSessionTracker::new(3);
        assert!(tracker.get_cwd("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_update_key_preserves_entry_count() {
        // We can't easily create a real runner, but we can verify that
        // update_key on a missing key doesn't increase the count.
        let tracker = ActiveSessionTracker::new(3);
        tracker.update_key("a", "b").await;
        assert_eq!(tracker.active_count().await, 0);
    }

    // -------------------------------------------------------------------
    // Config helper test
    // -------------------------------------------------------------------

    #[test]
    fn test_make_config_is_valid() {
        let config = make_config();
        assert!(config.enabled);
        assert_eq!(config.max_concurrent, 3);
        assert_eq!(config.timeout_ms, 60_000);
        assert_eq!(config.idle_timeout_ms, 30_000);
    }
}
