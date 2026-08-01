use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;
use std::time::Duration;
use tokio::sync::oneshot;
use uuid::Uuid;

/// Maximum number of session-scoped approvals retained in the LRU cache.
/// Oldest (least-recently-used) entries are evicted when this cap is reached.
const MAX_SESSION_APPROVALS: usize = 10_000;

/// Human-In-The-Loop approval gate.
///
/// For Tier 2/3 operations, the server sends a permission request to the client
/// and blocks until the client responds with approval/denial (or timeout).
pub struct HitlGate {
    pending: Mutex<HashMap<Uuid, oneshot::Sender<bool>>>,
    timeout: Duration,
    auto_approve: bool,
    /// Session-scoped approvals: (session_id, tool_name) pairs that have been
    /// granted session-level permission, bypassing the HITL gate for the rest
    /// of the session.  Bounded LRU cache (cap = MAX_SESSION_APPROVALS) so
    /// long-running servers don't leak memory across thousands of sessions.
    session_approvals: Mutex<LruCache<(String, String), ()>>,
    /// Maps `request_id → tool_name` so that when a `cmd.res` arrives with
    /// `scope = "session"`, the handler can look up which tool to record.
    pending_tools: Mutex<HashMap<Uuid, String>>,
}

impl HitlGate {
    pub fn new(timeout_secs: u64) -> Self {
        let cap = NonZeroUsize::new(MAX_SESSION_APPROVALS).expect("cap is non-zero");
        Self {
            pending: Mutex::new(HashMap::new()),
            timeout: Duration::from_secs(timeout_secs),
            auto_approve: false,
            session_approvals: Mutex::new(LruCache::new(cap)),
            pending_tools: Mutex::new(HashMap::new()),
        }
    }

    /// Create a gate that auto-approves all requests (for testing).
    pub fn auto_approve() -> Self {
        let cap = NonZeroUsize::new(MAX_SESSION_APPROVALS).expect("cap is non-zero");
        Self {
            pending: Mutex::new(HashMap::new()),
            timeout: Duration::from_secs(0),
            auto_approve: true,
            session_approvals: Mutex::new(LruCache::new(cap)),
            pending_tools: Mutex::new(HashMap::new()),
        }
    }

    /// Whether this gate auto-approves all requests.
    pub fn is_auto_approve(&self) -> bool {
        self.auto_approve
    }

    /// Create an approval request. Returns (request_id, receiver).
    /// The caller sends the request_id to the client, then awaits the receiver.
    /// The receiver resolves to `true` (approved) or `false` (denied/timeout).
    pub fn request_approval(&self) -> (Uuid, oneshot::Receiver<bool>) {
        let request_id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();

        let mut pending = self.pending.lock().unwrap_or_else(|poisoned| {
            tracing::error!("Lock poisoned, recovering");
            poisoned.into_inner()
        });
        pending.insert(request_id, tx);

        (request_id, rx)
    }

    /// Resolve a pending approval request. Called when client sends cmd.res.
    /// Returns `true` if the request was found and resolved, `false` if not found.
    pub fn resolve(&self, request_id: Uuid, approved: bool) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(|poisoned| {
            tracing::error!("Lock poisoned, recovering");
            poisoned.into_inner()
        });
        if let Some(sender) = pending.remove(&request_id) {
            let _ = sender.send(approved);
            true
        } else {
            false
        }
    }

    /// Get the configured timeout duration.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Wait for approval with timeout. Returns `true` if approved, `false` if denied or timed out.
    pub async fn wait_for_approval(&self, rx: oneshot::Receiver<bool>) -> bool {
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(approved)) => approved,
            Ok(Err(_)) => false, // sender dropped
            Err(_) => false,     // timeout
        }
    }

    /// Cancel a pending request (e.g., on disconnect). Cleans up the sender.
    pub fn cancel(&self, request_id: &Uuid) {
        let mut pending = self.pending.lock().unwrap_or_else(|poisoned| {
            tracing::error!("Lock poisoned, recovering");
            poisoned.into_inner()
        });
        pending.remove(request_id);
        // Also clean up the tool name mapping if present.
        let mut tools = self.pending_tools.lock().unwrap_or_else(|poisoned| {
            tracing::error!("pending_tools lock poisoned, recovering");
            poisoned.into_inner()
        });
        tools.remove(request_id);
    }

    /// Associate a `tool_name` with a `request_id` so that when the client
    /// responds with `scope = "session"` the handler can record the approval.
    /// Call this immediately after `request_approval` for agentic tool calls.
    pub fn register_tool_for_request(&self, request_id: Uuid, tool_name: impl Into<String>) {
        let mut tools = self.pending_tools.lock().unwrap_or_else(|poisoned| {
            tracing::error!("pending_tools lock poisoned, recovering");
            poisoned.into_inner()
        });
        tools.insert(request_id, tool_name.into());
    }

    /// Remove and return the tool name associated with `request_id`, if any.
    /// Used by the `cmd.res` handler to look up the tool when `scope = "session"`.
    pub fn take_tool_for_request(&self, request_id: &Uuid) -> Option<String> {
        let mut tools = self.pending_tools.lock().unwrap_or_else(|poisoned| {
            tracing::error!("pending_tools lock poisoned, recovering");
            poisoned.into_inner()
        });
        tools.remove(request_id)
    }

    /// Record a session-scoped approval. After this call, `has_session_approval`
    /// will return `true` for the same `(session_id, tool_name)` pair for the
    /// lifetime of this `HitlGate` instance.
    pub fn record_session_approval(&self, session_id: &str, tool_name: &str) {
        let mut approvals = self.session_approvals.lock().unwrap_or_else(|poisoned| {
            tracing::error!("session_approvals lock poisoned, recovering");
            poisoned.into_inner()
        });
        approvals.put((session_id.to_owned(), tool_name.to_owned()), ());
    }

    /// Check whether a session-scoped approval exists for `(session_id, tool_name)`.
    /// Returns `true` if the user previously granted session-level permission.
    /// Also promotes the entry to most-recently-used so active sessions survive
    /// longer under eviction pressure.
    pub fn has_session_approval(&self, session_id: &str, tool_name: &str) -> bool {
        let mut approvals = self.session_approvals.lock().unwrap_or_else(|poisoned| {
            tracing::error!("session_approvals lock poisoned, recovering");
            poisoned.into_inner()
        });
        approvals
            .get(&(session_id.to_owned(), tool_name.to_owned()))
            .is_some()
    }

    /// Invalidate all session-scoped approvals for `session_id`.
    /// Call this when a session is deleted so its approvals are eagerly reclaimed
    /// rather than waiting for LRU eviction.
    pub fn invalidate_session(&self, session_id: &str) {
        let mut approvals = self.session_approvals.lock().unwrap_or_else(|poisoned| {
            tracing::error!("session_approvals lock poisoned, recovering");
            poisoned.into_inner()
        });
        // LruCache has no bulk-remove by predicate; collect keys then demote them.
        let keys_to_remove: Vec<(String, String)> = approvals
            .iter()
            .filter(|((sid, _), _)| sid == session_id)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys_to_remove {
            approvals.pop(&key);
        }
    }

    /// Number of pending requests (for diagnostics).
    pub fn pending_count(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| {
                tracing::error!("Lock poisoned, recovering");
                poisoned.into_inner()
            })
            .len()
    }
}

impl std::fmt::Debug for HitlGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HitlGate")
            .field("timeout", &self.timeout)
            .field("auto_approve", &self.auto_approve)
            .field("pending_count", &self.pending_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_approve() {
        let gate = HitlGate::new(5);
        let (request_id, rx) = gate.request_approval();
        assert_eq!(gate.pending_count(), 1);

        assert!(gate.resolve(request_id, true));
        assert_eq!(gate.pending_count(), 0);

        let result = rx.await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_deny() {
        let gate = HitlGate::new(5);
        let (request_id, rx) = gate.request_approval();

        assert!(gate.resolve(request_id, false));

        let result = rx.await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_timeout_auto_denies() {
        let gate = HitlGate::new(0); // 0 second timeout = immediate timeout
        let (_request_id, rx) = gate.request_approval();

        let result = gate.wait_for_approval(rx).await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_resolve_unknown_id() {
        let gate = HitlGate::new(5);
        let unknown_id = Uuid::new_v4();
        assert!(!gate.resolve(unknown_id, true));
    }

    #[tokio::test]
    async fn test_cancel() {
        let gate = HitlGate::new(5);
        let (request_id, _rx) = gate.request_approval();
        assert_eq!(gate.pending_count(), 1);

        gate.cancel(&request_id);
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn test_session_approval_record_and_check() {
        let gate = HitlGate::new(5);
        assert!(!gate.has_session_approval("sess-1", "write_file"));

        gate.record_session_approval("sess-1", "write_file");
        assert!(gate.has_session_approval("sess-1", "write_file"));
    }

    #[test]
    fn test_session_approval_different_session_is_independent() {
        let gate = HitlGate::new(5);
        gate.record_session_approval("sess-1", "write_file");

        // Different session — must NOT inherit approval.
        assert!(!gate.has_session_approval("sess-2", "write_file"));
    }

    #[test]
    fn test_session_approval_different_tool_is_independent() {
        let gate = HitlGate::new(5);
        gate.record_session_approval("sess-1", "write_file");

        // Same session, different tool — must NOT be granted.
        assert!(!gate.has_session_approval("sess-1", "shell"));
    }

    #[test]
    fn test_session_approval_multiple_tools() {
        let gate = HitlGate::new(5);
        gate.record_session_approval("sess-1", "write_file");
        gate.record_session_approval("sess-1", "shell");

        assert!(gate.has_session_approval("sess-1", "write_file"));
        assert!(gate.has_session_approval("sess-1", "shell"));
        assert!(!gate.has_session_approval("sess-1", "patch"));
    }

    #[test]
    fn test_session_approval_idempotent() {
        let gate = HitlGate::new(5);
        gate.record_session_approval("sess-1", "write_file");
        gate.record_session_approval("sess-1", "write_file"); // second call is a no-op
        assert!(gate.has_session_approval("sess-1", "write_file"));
    }

    /// Cap+1 entries must evict the oldest (first inserted) entry.
    #[test]
    fn lru_evicts_oldest_on_cap() {
        // Use a tiny cap so we don't need to insert 10_000 entries.
        let cap = NonZeroUsize::new(3).unwrap();
        let mut cache: LruCache<(String, String), ()> = LruCache::new(cap);

        cache.put(("sess-a".into(), "tool-1".into()), ());
        cache.put(("sess-b".into(), "tool-2".into()), ());
        cache.put(("sess-c".into(), "tool-3".into()), ());

        // Adding a 4th entry evicts the LRU entry (sess-a/tool-1).
        cache.put(("sess-d".into(), "tool-4".into()), ());

        assert!(
            !cache.contains(&("sess-a".to_owned(), "tool-1".to_owned())),
            "sess-a/tool-1 should have been evicted"
        );
        assert!(cache.contains(&("sess-b".to_owned(), "tool-2".to_owned())));
        assert!(cache.contains(&("sess-c".to_owned(), "tool-3".to_owned())));
        assert!(cache.contains(&("sess-d".to_owned(), "tool-4".to_owned())));
    }

    /// Touching an old entry promotes it; it should survive subsequent evictions.
    #[test]
    fn lru_recency_moves_entry_to_front() {
        let cap = NonZeroUsize::new(3).unwrap();
        let mut cache: LruCache<(String, String), ()> = LruCache::new(cap);

        cache.put(("sess-a".into(), "tool-1".into()), ());
        cache.put(("sess-b".into(), "tool-2".into()), ());
        cache.put(("sess-c".into(), "tool-3".into()), ());

        // Promote sess-a/tool-1 to MRU.
        let _ = cache.get(&("sess-a".to_owned(), "tool-1".to_owned()));

        // Next insert should evict sess-b/tool-2 (now the LRU), not sess-a.
        cache.put(("sess-d".into(), "tool-4".into()), ());

        assert!(
            cache.contains(&("sess-a".to_owned(), "tool-1".to_owned())),
            "sess-a/tool-1 should have survived after promotion"
        );
        assert!(
            !cache.contains(&("sess-b".to_owned(), "tool-2".to_owned())),
            "sess-b/tool-2 should have been evicted"
        );
    }
}
