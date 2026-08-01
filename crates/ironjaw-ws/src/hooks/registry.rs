// Hook registry + async Hook trait (B2 CP2).
//
// The CP1 sync registry is replaced with an async one mirroring the
// `SlashCommand` / `SlashCommandRegistry` shape from
// `crates/ironjaw-ws/src/commands.rs`:
// - `pub trait Hook` is `async-trait`-decorated so subprocess and WASM
//   implementations can `.await` (tokio::process, fuel-metered wasmi calls).
// - Storage is `Arc<DashMap<HookEventKind, Arc<RwLock<Vec<RegisteredHook>>>>>`
//   to match the lock-free DashMap pattern already used for slash commands.
// - A sync `register_fn` compatibility shim wraps `Fn(&HookContext)
//   -> HookAction` closures in a stateless `FnHook` adapter implementing the
//   trait, so the original Stream 8.2 tests (now async) keep their logic
//   intact.

use async_trait::async_trait;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::types::{HookAction, HookCallback, HookContext, HookEvent, HookEventKind};

/// Async hook trait. All production hook backends (subprocess, WASM) implement
/// this directly; sync closures are adapted via `register_fn` and `FnHook`.
#[async_trait]
pub trait Hook: Send + Sync {
    /// Human-readable hook name (used by listing and unregistration).
    fn name(&self) -> &str;
    /// Priority. Lower runs first; ties preserve registration order.
    fn priority(&self) -> i32;
    /// Execute the hook. May `.await`. Returns the resulting `HookAction`.
    async fn execute(&self, ctx: &HookContext) -> HookAction;
}

/// Sync-closure adapter so existing sync test callbacks still work.
struct FnHook {
    name: String,
    priority: i32,
    callback: HookCallback,
}

#[async_trait]
impl Hook for FnHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn priority(&self) -> i32 {
        self.priority
    }
    async fn execute(&self, ctx: &HookContext) -> HookAction {
        (self.callback)(ctx)
    }
}

/// A registered hook entry with its metadata mirrored at registry level so
/// listing and ordering don't require dispatching into the hook itself.
pub(crate) struct RegisteredHook {
    pub name: String,
    pub priority: i32,
    pub hook: Arc<dyn Hook>,
}

impl std::fmt::Debug for RegisteredHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredHook")
            .field("name", &self.name)
            .field("priority", &self.priority)
            .finish()
    }
}

/// Registry of lifecycle hooks. Indexed by `HookEventKind`. Hooks fire in
/// priority order on `fire`, returning the first non-`Continue` action.
#[derive(Clone)]
pub struct HookRegistry {
    hooks: Arc<DashMap<HookEventKind, Arc<RwLock<Vec<RegisteredHook>>>>>,
}

impl std::fmt::Debug for HookRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookRegistry")
            .field("event_kinds", &self.hooks.len())
            .finish()
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HookRegistry {
    pub fn new() -> Self {
        Self {
            hooks: Arc::new(DashMap::new()),
        }
    }

    /// Register a fully-async `Hook` implementation. Preferred for production
    /// subprocess / WASM backends.
    pub async fn register_hook(&self, kind: HookEventKind, hook: Arc<dyn Hook>) {
        let entry = RegisteredHook {
            name: hook.name().to_string(),
            priority: hook.priority(),
            hook,
        };
        let bucket = self
            .hooks
            .entry(kind)
            .or_insert_with(|| Arc::new(RwLock::new(Vec::new())))
            .clone();
        let mut guard = bucket.write().await;
        guard.push(entry);
        guard.sort_by_key(|h| h.priority);
    }

    /// Sync compatibility shim: register a closure callback. Adapts
    /// the sync closure to the `Hook` trait via `FnHook`.
    pub async fn register_fn(
        &self,
        kind: HookEventKind,
        name: &str,
        priority: i32,
        callback: HookCallback,
    ) {
        let hook = Arc::new(FnHook {
            name: name.to_string(),
            priority,
            callback,
        });
        self.register_hook(kind, hook).await;
    }

    /// Convenience: register a sync closure for a `HookEvent` (kind extracted).
    pub async fn register(
        &self,
        event: HookEvent,
        name: &str,
        priority: i32,
        callback: HookCallback,
    ) {
        self.register_fn(event.kind(), name, priority, callback)
            .await;
    }

    /// Unregister a hook by event kind and name. Returns true if removed.
    pub async fn unregister_kind(&self, kind: HookEventKind, name: &str) -> bool {
        let bucket = self.hooks.get(&kind).map(|b| b.value().clone());
        if let Some(bucket) = bucket {
            let mut guard = bucket.write().await;
            let before = guard.len();
            guard.retain(|h| h.name != name);
            return guard.len() < before;
        }
        false
    }

    /// Convenience: unregister by `HookEvent`.
    pub async fn unregister(&self, event: HookEvent, name: &str) -> bool {
        self.unregister_kind(event.kind(), name).await
    }

    /// Fire all hooks registered for the context's event in priority order.
    /// Returns the first non-`Continue` action; `Continue` if none/empty.
    pub async fn fire(&self, ctx: &HookContext) -> HookAction {
        let bucket = self.hooks.get(&ctx.event.kind()).map(|b| b.value().clone());
        if let Some(bucket) = bucket {
            // Snapshot the hooks under read lock so we don't hold it across
            // `.await` boundaries inside individual hook executions (which
            // could deadlock if a hook tried to register/unregister).
            let snapshot: Vec<Arc<dyn Hook>> = {
                let guard = bucket.read().await;
                guard.iter().map(|e| e.hook.clone()).collect()
            };
            for hook in snapshot {
                let action = hook.execute(ctx).await;
                if !action.is_continue() {
                    return action;
                }
            }
        }
        HookAction::Continue
    }

    /// List the names of all hooks registered for a specific event kind.
    pub async fn list_hooks_by_kind(&self, kind: &HookEventKind) -> Vec<String> {
        let bucket = self.hooks.get(kind).map(|b| b.value().clone());
        match bucket {
            Some(bucket) => {
                let guard = bucket.read().await;
                guard.iter().map(|e| e.name.clone()).collect()
            }
            None => Vec::new(),
        }
    }

    /// Convenience: list by `HookEvent`.
    pub async fn list_hooks(&self, event: &HookEvent) -> Vec<String> {
        self.list_hooks_by_kind(&event.kind()).await
    }

    /// Remove all hooks across all events.
    pub async fn clear(&self) {
        // Snapshot the buckets out of the DashMap before awaiting to avoid
        // holding DashMap iter guards across await points.
        let buckets: Vec<Arc<RwLock<Vec<RegisteredHook>>>> = self
            .hooks
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        for bucket in buckets {
            bucket.write().await.clear();
        }
    }

    /// Total number of registered hooks across all events.
    pub async fn total_hooks(&self) -> usize {
        let buckets: Vec<Arc<RwLock<Vec<RegisteredHook>>>> = self
            .hooks
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        let mut total = 0;
        for bucket in buckets {
            total += bucket.read().await.len();
        }
        total
    }

    /// Whether any hooks are registered for the given event kind.
    pub async fn has_hooks_by_kind(&self, kind: &HookEventKind) -> bool {
        // Clone the Arc<RwLock<...>> out before crossing the await boundary
        // so the DashMap `Ref` guard isn't held across await (would not be
        // Send-safe and triggers borrow-lifetime errors).
        let bucket = self.hooks.get(kind).map(|b| b.value().clone());
        match bucket {
            Some(bucket) => !bucket.read().await.is_empty(),
            None => false,
        }
    }

    /// Convenience: existence check by `HookEvent`.
    pub async fn has_hooks(&self, event: &HookEvent) -> bool {
        self.has_hooks_by_kind(&event.kind()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx(event: HookEvent) -> HookContext {
        HookContext {
            event,
            session_id: Some("test-session".to_string()),
            data: serde_json::json!({"key": "value"}),
            timestamp: chrono::Utc::now(),
        }
    }

    fn continue_hook() -> HookCallback {
        Arc::new(|_ctx| HookAction::Continue)
    }

    fn block_hook(reason: &str) -> HookCallback {
        let reason = reason.to_string();
        Arc::new(move |_ctx| HookAction::Block(reason.clone()))
    }

    fn modify_hook(value: serde_json::Value) -> HookCallback {
        Arc::new(move |_ctx| HookAction::Modify(value.clone()))
    }

    #[tokio::test]
    async fn test_register_and_list() {
        let reg = HookRegistry::new();
        reg.register(HookEvent::PreChat, "logger", 10, continue_hook())
            .await;
        reg.register(HookEvent::PreChat, "validator", 5, continue_hook())
            .await;
        let names = reg.list_hooks(&HookEvent::PreChat).await;
        assert_eq!(names, vec!["validator".to_string(), "logger".to_string()]);
    }

    #[tokio::test]
    async fn test_register_multiple_events() {
        let reg = HookRegistry::new();
        reg.register(HookEvent::PreChat, "pre_hook", 0, continue_hook())
            .await;
        reg.register(HookEvent::PostChat, "post_hook", 0, continue_hook())
            .await;
        assert_eq!(
            reg.list_hooks(&HookEvent::PreChat).await,
            vec!["pre_hook".to_string()]
        );
        assert_eq!(
            reg.list_hooks(&HookEvent::PostChat).await,
            vec!["post_hook".to_string()]
        );
        assert_eq!(reg.total_hooks().await, 2);
    }

    #[tokio::test]
    async fn test_unregister_by_name() {
        let reg = HookRegistry::new();
        reg.register(HookEvent::PreChat, "keep", 0, continue_hook())
            .await;
        reg.register(HookEvent::PreChat, "remove_me", 0, continue_hook())
            .await;
        assert!(reg.unregister(HookEvent::PreChat, "remove_me").await);
        assert_eq!(
            reg.list_hooks(&HookEvent::PreChat).await,
            vec!["keep".to_string()]
        );
    }

    #[tokio::test]
    async fn test_unregister_nonexistent() {
        let reg = HookRegistry::new();
        assert!(!reg.unregister(HookEvent::PreChat, "ghost").await);
    }

    #[tokio::test]
    async fn test_unregister_wrong_event() {
        let reg = HookRegistry::new();
        reg.register(HookEvent::PreChat, "hook", 0, continue_hook())
            .await;
        assert!(!reg.unregister(HookEvent::PostChat, "hook").await);
        assert_eq!(
            reg.list_hooks(&HookEvent::PreChat).await,
            vec!["hook".to_string()]
        );
    }

    #[tokio::test]
    async fn test_fire_no_hooks() {
        let reg = HookRegistry::new();
        let ctx = test_ctx(HookEvent::PreChat);
        assert!(reg.fire(&ctx).await.is_continue());
    }

    #[tokio::test]
    async fn test_fire_all_continue() {
        let reg = HookRegistry::new();
        reg.register(HookEvent::PreChat, "a", 0, continue_hook())
            .await;
        reg.register(HookEvent::PreChat, "b", 1, continue_hook())
            .await;
        let ctx = test_ctx(HookEvent::PreChat);
        assert!(reg.fire(&ctx).await.is_continue());
    }

    #[tokio::test]
    async fn test_fire_first_block_wins() {
        let reg = HookRegistry::new();
        reg.register(HookEvent::PreChat, "pass", 0, continue_hook())
            .await;
        reg.register(HookEvent::PreChat, "blocker", 1, block_hook("not allowed"))
            .await;
        reg.register(HookEvent::PreChat, "never_reached", 2, continue_hook())
            .await;
        let ctx = test_ctx(HookEvent::PreChat);
        let action = reg.fire(&ctx).await;
        assert!(action.is_block());
        if let HookAction::Block(reason) = action {
            assert_eq!(reason, "not allowed");
        }
    }

    #[tokio::test]
    async fn test_fire_priority_order() {
        let reg = HookRegistry::new();
        reg.register(
            HookEvent::PreToolExec,
            "blocker",
            1,
            block_hook("blocked first"),
        )
        .await;
        reg.register(
            HookEvent::PreToolExec,
            "modifier",
            10,
            modify_hook(serde_json::json!("modified")),
        )
        .await;
        let ctx = test_ctx(HookEvent::PreToolExec);
        assert!(reg.fire(&ctx).await.is_block());
    }

    #[tokio::test]
    async fn test_fire_modify_action() {
        let reg = HookRegistry::new();
        let modified = serde_json::json!({"injected": true});
        reg.register(
            HookEvent::MemoryStore,
            "injector",
            0,
            modify_hook(modified.clone()),
        )
        .await;
        let ctx = test_ctx(HookEvent::MemoryStore);
        let action = reg.fire(&ctx).await;
        assert!(action.is_modify());
        if let HookAction::Modify(value) = action {
            assert_eq!(value, modified);
        }
    }

    #[tokio::test]
    async fn test_fire_only_matching_event() {
        let reg = HookRegistry::new();
        reg.register(
            HookEvent::PostChat,
            "post_blocker",
            0,
            block_hook("blocked"),
        )
        .await;
        let ctx = test_ctx(HookEvent::PreChat);
        assert!(reg.fire(&ctx).await.is_continue());
    }

    #[tokio::test]
    async fn test_clear() {
        let reg = HookRegistry::new();
        reg.register(HookEvent::PreChat, "a", 0, continue_hook())
            .await;
        reg.register(HookEvent::PostChat, "b", 0, continue_hook())
            .await;
        reg.register(HookEvent::SessionStart, "c", 0, continue_hook())
            .await;
        assert_eq!(reg.total_hooks().await, 3);
        reg.clear().await;
        assert_eq!(reg.total_hooks().await, 0);
        assert!(reg.list_hooks(&HookEvent::PreChat).await.is_empty());
    }

    #[tokio::test]
    async fn test_has_hooks() {
        let reg = HookRegistry::new();
        assert!(!reg.has_hooks(&HookEvent::PreChat).await);
        reg.register(HookEvent::PreChat, "hook", 0, continue_hook())
            .await;
        assert!(reg.has_hooks(&HookEvent::PreChat).await);
        assert!(!reg.has_hooks(&HookEvent::PostChat).await);
    }

    #[tokio::test]
    async fn test_default_trait() {
        let reg = HookRegistry::default();
        assert_eq!(reg.total_hooks().await, 0);
    }

    #[tokio::test]
    async fn test_stable_sort_preserves_insertion_order() {
        let reg = HookRegistry::new();
        reg.register(HookEvent::PreChat, "first", 0, continue_hook())
            .await;
        reg.register(HookEvent::PreChat, "second", 0, continue_hook())
            .await;
        reg.register(HookEvent::PreChat, "third", 0, continue_hook())
            .await;
        assert_eq!(
            reg.list_hooks(&HookEvent::PreChat).await,
            vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_hook_callback_receives_context() {
        let reg = HookRegistry::new();
        let cb: HookCallback = Arc::new(|ctx| {
            if ctx.data.get("block").and_then(|v| v.as_bool()) == Some(true) {
                HookAction::Block("data said block".to_string())
            } else {
                HookAction::Continue
            }
        });
        reg.register(HookEvent::PreToolExec, "conditional", 0, cb)
            .await;

        let ctx_pass =
            HookContext::new(HookEvent::PreToolExec, serde_json::json!({"block": false}));
        assert!(reg.fire(&ctx_pass).await.is_continue());

        let ctx_block =
            HookContext::new(HookEvent::PreToolExec, serde_json::json!({"block": true}));
        assert!(reg.fire(&ctx_block).await.is_block());
    }

    #[tokio::test]
    async fn test_negative_priority() {
        let reg = HookRegistry::new();
        reg.register(
            HookEvent::PreChat,
            "normal",
            0,
            block_hook("normal blocked"),
        )
        .await;
        reg.register(
            HookEvent::PreChat,
            "urgent",
            -10,
            block_hook("urgent blocked"),
        )
        .await;
        let ctx = test_ctx(HookEvent::PreChat);
        let action = reg.fire(&ctx).await;
        if let HookAction::Block(reason) = action {
            assert_eq!(reason, "urgent blocked");
        } else {
            panic!("Expected Block action");
        }
    }

    #[tokio::test]
    async fn test_register_payload_variant_by_kind() {
        let reg = HookRegistry::new();
        reg.register_fn(HookEventKind::PreCompact, "snapshot", 0, continue_hook())
            .await;
        let ctx = HookContext::new(
            HookEvent::PreCompact {
                session_id: "abc".into(),
                message_count: 42,
                total_tokens: 1000,
            },
            serde_json::json!(null),
        );
        assert!(reg.fire(&ctx).await.is_continue());
        assert!(reg.has_hooks_by_kind(&HookEventKind::PreCompact).await);
    }

    #[tokio::test]
    async fn test_async_hook_trait_impl() {
        // A pure-async Hook impl (no closure shim).
        struct AsyncBlocker {
            n: String,
            p: i32,
        }
        #[async_trait]
        impl Hook for AsyncBlocker {
            fn name(&self) -> &str {
                &self.n
            }
            fn priority(&self) -> i32 {
                self.p
            }
            async fn execute(&self, _ctx: &HookContext) -> HookAction {
                // Yield once to prove .await works.
                tokio::task::yield_now().await;
                HookAction::Block("async blocked".into())
            }
        }
        let reg = HookRegistry::new();
        reg.register_hook(
            HookEventKind::SessionStart,
            Arc::new(AsyncBlocker {
                n: "blocker".into(),
                p: 0,
            }),
        )
        .await;
        let ctx =
            HookContext::with_session(HookEvent::SessionStart, "sess", serde_json::json!(null));
        let action = reg.fire(&ctx).await;
        assert!(action.is_block());
        if let HookAction::Block(reason) = action {
            assert_eq!(reason, "async blocked");
        }
    }
}
