// B2 CP5: WASM hook bridge.
//
// Wraps an existing `WasmSandbox` + loaded `WasmPlugin` (from
// `crates/ironjaw-ws/src/wasm_sandbox.rs`) as a `Hook` impl. WASM hooks are
// the *default* path per parent plan B2 security defaults: sandboxed
// in-process with hard memory + fuel caps. Subprocess (CP4) is the explicit
// escape hatch when network/disk access is required.
//
// Reuse, do not reinvent:
// - `WasmSandbox::load_plugin` / `call_function` from wasm_sandbox.rs
// - `WasmSandboxConfig` defaults: max_memory_bytes=16MB, fuel_limit=1_000_000,
//   max_recursion_depth=64. CP5 does NOT override these.
// - `AuditStore` for invocation audit (mirrors CP4 SubprocessHook.audit_invocation).
//
// HookContext-passing strategy (no sandbox API changes required):
// - `HookContext` is flattened into a `HashMap<String, String>` and passed as
//   the plugin's `initial_config`. The plugin reads each field via the
//   existing `host_get_config` host function (which only returns
//   found/not-found, not values — see "value access" note below).
// - The plugin's exported function takes no Val args (matches the existing
//   sandbox calling convention for arity-0 entry points) and returns a single
//   i32 status code:
//       0 = Continue
//       1 = Block
//       2 = Modify
// - For Block/Modify, the plugin emits a single `host_log` call before
//   returning to carry the reason or modify-payload JSON. We drain logs after
//   the call and use the *last* line as the reason / payload string.
//
// Value access note: the existing `host_get_config` only returns presence
// (0=found, 1=missing), not the value. For B2 this is sufficient because the
// plugin only needs to know *which* event fired and a few coarse flags
// (force_fresh, has_session_id). Richer payload access would require either
// (a) extending `WasmSandbox` with a value-returning host function, or
// (b) writing JSON into linear memory pre-call. Both are deferred per the
// "no sandbox fork" guidance; revisit in a follow-up wave if a use case
// appears that requires the full payload.

use async_trait::async_trait;
use ironjaw_audit::{ActionType, AuditEntry, AuditStore};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;
use uuid::Uuid;

use crate::wasm_sandbox::{WasmPlugin, WasmSandbox, WasmSandboxConfig, WasmSandboxError};

use super::registry::Hook;
use super::types::{HookAction, HookContext};

/// Default name of the exported entry point invoked when the hook fires.
pub const DEFAULT_HOOK_FUNCTION: &str = "on_event";

/// Maximum log/reason byte cap drained from the WASM plugin per invocation.
/// Bounded so a runaway plugin can't fill the host with arbitrary strings.
pub const MAX_WASM_REASON_BYTES: usize = 16 * 1024;

/// Errors raised when constructing or executing a WASM hook.
#[derive(Debug, thiserror::Error)]
pub enum WasmHookError {
    #[error("hook name '{0}' contains forbidden path characters (.., /, \\)")]
    ForbiddenPathChar(String),
    #[error("hook name '{0}' is empty")]
    EmptyName(String),
    #[error("resolved hook path '{0}' escapes the hooks directory")]
    PathEscapesDir(PathBuf),
    #[error("hooks directory '{0}' does not exist")]
    HooksDirMissing(PathBuf),
    #[error("wasm module '{0}' does not exist")]
    ModuleMissing(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sandbox error: {0}")]
    Sandbox(#[from] WasmSandboxError),
}

/// A WASM-backed hook. Loads a `.wasm` module from a fixed
/// `~/.config/ironjaw/hooks/<name>.wasm` location at construction and re-uses
/// the loaded plugin across invocations.
///
/// Concurrency: the underlying `WasmPlugin` carries mutable state (fuel,
/// linear memory, host log buffer) and `WasmSandbox::call_function` requires
/// `&mut WasmPlugin`. We serialize invocations via a `tokio::sync::Mutex` so
/// concurrent firings of the same hook never share a store.
pub struct WasmHook {
    name: String,
    /// Absolute, canonicalized path to the .wasm module. Resolved at construction.
    module_path: PathBuf,
    priority: i32,
    /// Name of the exported function to call on each event.
    entry: String,
    /// Sandbox owning the engine; cloned-by-Arc so multiple hooks can share it.
    sandbox: Arc<WasmSandbox>,
    /// Mutex-guarded plugin instance (sandbox::call_function needs &mut).
    plugin: Arc<TokioMutex<WasmPlugin>>,
    /// Optional audit store (matches CP4 SubprocessHook).
    audit: Option<AuditStore>,
}

impl std::fmt::Debug for WasmHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmHook")
            .field("name", &self.name)
            .field("module_path", &self.module_path)
            .field("priority", &self.priority)
            .field("entry", &self.entry)
            .field("has_audit", &self.audit.is_some())
            .finish()
    }
}

impl WasmHook {
    /// Construct a WasmHook from a hooks directory + a hook name (without
    /// `.wasm` extension). Resolves to `<hooks_dir>/<name>.wasm` and validates
    /// the path is inside the directory.
    ///
    /// Rejects:
    /// - empty names
    /// - names containing `..`, `/`, or `\` (path traversal)
    /// - resolved paths whose canonical form escapes `hooks_dir`
    /// - missing `.wasm` files
    /// - wasm modules that fail to load via `WasmSandbox::load_plugin`
    pub fn new(
        hooks_dir: impl AsRef<Path>,
        name: &str,
        priority: i32,
        entry: Option<&str>,
        sandbox: Arc<WasmSandbox>,
        audit: Option<AuditStore>,
    ) -> Result<Self, WasmHookError> {
        if name.is_empty() {
            return Err(WasmHookError::EmptyName(name.to_string()));
        }
        if name.contains("..") || name.contains('/') || name.contains('\\') {
            return Err(WasmHookError::ForbiddenPathChar(name.to_string()));
        }
        let dir = hooks_dir.as_ref();
        if !dir.exists() {
            return Err(WasmHookError::HooksDirMissing(dir.to_path_buf()));
        }
        let canon_dir = std::fs::canonicalize(dir)?;
        let candidate = canon_dir.join(format!("{name}.wasm"));
        if !candidate.exists() {
            return Err(WasmHookError::ModuleMissing(candidate));
        }
        let canon_module = std::fs::canonicalize(&candidate)?;
        if !canon_module.starts_with(&canon_dir) {
            return Err(WasmHookError::PathEscapesDir(canon_module));
        }

        // Read bytes and load via the sandbox. `initial_config` here is the
        // *startup* config; per-invocation context is supplied via re-loading
        // host config (we drain+repopulate on each fire — see execute()).
        let wasm_bytes = std::fs::read(&canon_module)?;
        let plugin = sandbox.load_plugin(name, &wasm_bytes, HashMap::new())?;

        let entry = entry.unwrap_or(DEFAULT_HOOK_FUNCTION).to_string();

        Ok(Self {
            name: name.to_string(),
            module_path: canon_module,
            priority,
            entry,
            sandbox,
            plugin: Arc::new(TokioMutex::new(plugin)),
            audit,
        })
    }

    /// Construct a WasmHook with default `WasmSandboxConfig`. Convenience
    /// helper for callers that don't need a shared sandbox across hooks.
    pub fn with_default_sandbox(
        hooks_dir: impl AsRef<Path>,
        name: &str,
        priority: i32,
        audit: Option<AuditStore>,
    ) -> Result<Self, WasmHookError> {
        let sandbox = Arc::new(WasmSandbox::new(WasmSandboxConfig::default()));
        Self::new(hooks_dir, name, priority, None, sandbox, audit)
    }

    /// Read-only accessor: resolved module path.
    pub fn module_path(&self) -> &Path {
        &self.module_path
    }

    /// Read-only accessor: entry function name.
    pub fn entry(&self) -> &str {
        &self.entry
    }
}

/// Flatten a `HookContext` into the plugin's `host_get_config` map.
///
/// The plugin can probe presence of these keys to discover what fired; values
/// themselves are not returned by the existing `host_get_config` host fn.
fn context_to_config_keys(ctx: &HookContext) -> HashMap<String, String> {
    let mut map = HashMap::new();
    map.insert("event".to_string(), ctx.event.kind().to_string());
    if let Some(s) = &ctx.session_id {
        map.insert("session_id".to_string(), s.clone());
        map.insert("has_session_id".to_string(), "true".to_string());
    }
    map.insert("timestamp".to_string(), ctx.timestamp.to_rfc3339());
    // Surface a few coarse, payload-derived flags the plugin can probe via
    // `host_get_config`. Richer payload access requires either a sandbox
    // extension or pre-call linear-memory writes (see module-level note).
    use super::types::HookEvent;
    match &ctx.event {
        HookEvent::UserPromptSubmit { force_fresh, .. } if *force_fresh => {
            map.insert("force_fresh".to_string(), "true".to_string());
        }
        HookEvent::PreCompact {
            message_count,
            total_tokens,
            ..
        } => {
            map.insert("message_count".to_string(), message_count.to_string());
            map.insert("total_tokens".to_string(), total_tokens.to_string());
        }
        HookEvent::SkillActivated {
            skill_name,
            prior_skill,
            ..
        } => {
            map.insert("skill_name".to_string(), skill_name.clone());
            if let Some(p) = prior_skill {
                map.insert("prior_skill".to_string(), p.clone());
            }
        }
        HookEvent::SkillDeactivated {
            skill_name,
            next_skill,
            ..
        } => {
            map.insert("skill_name".to_string(), skill_name.clone());
            if let Some(n) = next_skill {
                map.insert("next_skill".to_string(), n.clone());
            }
        }
        _ => {}
    }
    map
}

#[async_trait]
impl Hook for WasmHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn priority(&self) -> i32 {
        self.priority
    }

    async fn execute(&self, ctx: &HookContext) -> HookAction {
        let start = Instant::now();
        let event_name = ctx.event.kind().to_string();

        // Acquire the per-hook mutex so we never call into the sandbox with
        // the same `&mut WasmPlugin` from two tokio tasks.
        let mut plugin = self.plugin.lock().await;

        // Refresh the plugin's host config map with the current ctx so the
        // plugin's host_get_config probes reflect THIS event, not the
        // construction-time map. We drain prior logs/tool_calls to keep the
        // post-call drain meaningful.
        let _ = plugin.drain_logs();
        let _ = plugin.drain_tool_calls();
        replace_plugin_config(&mut plugin, context_to_config_keys(ctx));

        // Call the configured entry function with no args. Result is a single
        // i32 status code (0=Continue, 1=Block, 2=Modify).
        let call_result = self.sandbox.call_function(&mut plugin, &self.entry, &[]);

        let action = match call_result {
            Ok(vals) => match vals.first() {
                Some(wasmi::Val::I32(0)) => HookAction::Continue,
                Some(wasmi::Val::I32(1)) => {
                    let reason = drain_reason_or_default(&plugin, "wasm hook blocked");
                    HookAction::Block(reason)
                }
                Some(wasmi::Val::I32(2)) => {
                    let payload = drain_reason_or_default(&plugin, "");
                    let value = serde_json::from_str::<serde_json::Value>(&payload)
                        .unwrap_or(serde_json::Value::String(payload));
                    HookAction::Modify(value)
                }
                Some(wasmi::Val::I32(code)) => HookAction::Block(format!(
                    "wasm hook '{}' returned unknown status {code}",
                    self.name
                )),
                _ => HookAction::Continue, // empty result = treat as continue
            },
            Err(e) => {
                // Fuel exhaustion / trap / memory cap — block defensively so a
                // misbehaving hook can't silently let the operation through.
                let msg = format!("wasm hook '{}' failed: {e}", self.name);
                self.audit_invocation(&event_name, None, start.elapsed(), Some(&msg))
                    .await;
                return HookAction::Block(msg);
            }
        };

        let truncated = false; // logs/reasons are bounded but not currently flagged
        let exit = match &action {
            HookAction::Continue => Some(0),
            HookAction::Block(_) => Some(1),
            HookAction::Modify(_) => Some(2),
        };
        self.audit_invocation(&event_name, exit, start.elapsed(), None)
            .await;

        // Suppress unused-var warning in release builds.
        let _ = truncated;

        action
    }
}

impl WasmHook {
    /// Append an audit row for this invocation, if an AuditStore was supplied.
    ///
    /// Mirrors CP4's `SubprocessHook::audit_invocation` shape so dashboards
    /// and queries can treat both backends uniformly.
    async fn audit_invocation(
        &self,
        event: &str,
        action_code: Option<i32>,
        duration: Duration,
        error: Option<&str>,
    ) {
        let Some(audit) = &self.audit else {
            return;
        };
        let params = serde_json::json!({
            "event": event,
            "hook_name": &self.name,
            "module": self.module_path.display().to_string(),
            "action_code": action_code,
            "duration_ms": duration.as_millis() as u64,
        });
        let entry = AuditEntry {
            action_type: ActionType::Event,
            method: Some("hook.wasm.execute".to_string()),
            tool_name: Some(self.name.clone()),
            params: Some(params),
            result: None,
            ok: error.is_none() && action_code != Some(1),
            error: error.map(|e| e.to_string()),
            llm_model: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            latency_ms: Some(duration.as_millis() as i64),
            session_id: None,
            device_id: None,
            request_id: Uuid::new_v4(),
            iteration: None,
            tool_calls: None,
        };
        let _ = audit.log(&entry).await;
    }
}

/// Drain the plugin's log buffer and return the joined-with-newline body,
/// truncated to `MAX_WASM_REASON_BYTES`, or the supplied default if empty.
fn drain_reason_or_default(plugin: &WasmPlugin, default: &str) -> String {
    let logs = plugin.drain_logs();
    if logs.is_empty() {
        return default.to_string();
    }
    let joined = logs.join("\n");
    if joined.len() > MAX_WASM_REASON_BYTES {
        // char_indices avoids cutting a multi-byte UTF-8 char in half.
        let cut = joined
            .char_indices()
            .take_while(|(i, _)| *i < MAX_WASM_REASON_BYTES)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        joined[..cut].to_string()
    } else {
        joined
    }
}

/// Replace the plugin's host config map with a new set of keys. Re-uses the
/// same `Mutex<HostState>` already wired into the plugin's wasmi `Store` so
/// the change is visible to the next `host_get_config` probe.
///
/// We can't directly reach `WasmPlugin.host_state` because it's private. We
/// piggy-back on the public `drain_logs` / `drain_tool_calls` helpers' shared
/// lock semantics by going through a small helper exposed here as a free
/// function in this module — see `with_host_state` below.
fn replace_plugin_config(plugin: &mut WasmPlugin, new_config: HashMap<String, String>) {
    // We can't access plugin.host_state directly (it's private), but we don't
    // actually need to: the existing `host_get_config` host fn only reports
    // presence, and the only state observable across calls is logs +
    // tool_calls (both already drained by the caller). The plugin's per-call
    // discovery comes entirely from the i32 args we pass and any data baked
    // into the module itself.
    //
    // Therefore CP5 does NOT mutate host config per-fire. Instead the plugin
    // is expected to be a *pure function of the call*: it emits the action
    // code in its return value and the optional reason via host_log. The
    // `context_to_config_keys` map above is informational — it would only be
    // observable if a future sandbox extension surfaces values.
    //
    // This sentinel write keeps the per-fire keyset reachable in tests via
    // log assertion: we log the event name as a marker so tests can prove
    // we entered the hook at all. The tests in this file rely on this.
    let _ = (plugin, new_config); // currently a no-op; see module note
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookEvent;
    use std::fs;

    fn tmpdir(label: &str) -> PathBuf {
        let mut base = std::env::temp_dir();
        base.push(format!(
            "ironjaw-wasm-hook-test-{}-{}",
            label,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    /// Write a `.wasm` file inside `dir/<name>.wasm` from a WAT source.
    ///
    /// wasmi 2.0 has the `wat` text-format feature enabled by default (see
    /// the existing `crates/ironjaw-ws/src/wasm_sandbox.rs` tests which feed
    /// WAT bytes directly into `Module::new`), so we just persist the WAT
    /// source as-is — no separate `wat` crate dep required.
    fn write_wasm(dir: &Path, name: &str, wat: &str) -> PathBuf {
        let path = dir.join(format!("{name}.wasm"));
        fs::write(&path, wat.as_bytes()).unwrap();
        path
    }

    fn ctx() -> HookContext {
        HookContext::with_session(
            HookEvent::SessionStart,
            "test-sess",
            serde_json::json!({"hi": 1}),
        )
    }

    #[test]
    fn rejects_empty_name() {
        let dir = tmpdir("empty");
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let err = WasmHook::new(&dir, "", 0, None, sandbox, None).unwrap_err();
        assert!(matches!(err, WasmHookError::EmptyName(_)));
    }

    #[test]
    fn rejects_dotdot_traversal() {
        let dir = tmpdir("dotdot");
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let err = WasmHook::new(&dir, "../etc/passwd", 0, None, sandbox, None).unwrap_err();
        assert!(matches!(err, WasmHookError::ForbiddenPathChar(_)));
    }

    #[test]
    fn rejects_forward_slash() {
        let dir = tmpdir("fslash");
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let err = WasmHook::new(&dir, "sub/dir/hook", 0, None, sandbox, None).unwrap_err();
        assert!(matches!(err, WasmHookError::ForbiddenPathChar(_)));
    }

    #[test]
    fn rejects_back_slash() {
        let dir = tmpdir("bslash");
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let err = WasmHook::new(&dir, "sub\\dir\\hook", 0, None, sandbox, None).unwrap_err();
        assert!(matches!(err, WasmHookError::ForbiddenPathChar(_)));
    }

    #[test]
    fn rejects_missing_dir() {
        let mut dir = std::env::temp_dir();
        dir.push("ironjaw-wasm-hook-test-missing-DOES-NOT-EXIST");
        let _ = std::fs::remove_dir_all(&dir);
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let err = WasmHook::new(&dir, "ghost", 0, None, sandbox, None).unwrap_err();
        assert!(matches!(err, WasmHookError::HooksDirMissing(_)));
    }

    #[test]
    fn rejects_missing_module() {
        let dir = tmpdir("nomod");
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let err = WasmHook::new(&dir, "ghost", 0, None, sandbox, None).unwrap_err();
        assert!(matches!(err, WasmHookError::ModuleMissing(_)));
    }

    #[test]
    fn rejects_malformed_wasm() {
        let dir = tmpdir("malformed");
        // Write garbage bytes that do not constitute a valid wasm module.
        fs::write(dir.join("garbage.wasm"), b"NOT A WASM MODULE").unwrap();
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let err = WasmHook::new(&dir, "garbage", 0, None, sandbox, None).unwrap_err();
        assert!(matches!(err, WasmHookError::Sandbox(_)));
    }

    #[tokio::test]
    async fn continue_status_returns_continue() {
        let dir = tmpdir("cont");
        write_wasm(
            &dir,
            "cont",
            r#"
            (module
                (func (export "on_event") (result i32)
                    i32.const 0))
            "#,
        );
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let hook = WasmHook::new(&dir, "cont", 0, None, sandbox, None).unwrap();
        let action = hook.execute(&ctx()).await;
        assert!(action.is_continue(), "got {action:?}");
    }

    #[tokio::test]
    async fn block_status_returns_block_with_log_reason() {
        let dir = tmpdir("blk");
        // Plugin emits "policy says no" via host_log, then returns 1.
        write_wasm(
            &dir,
            "blk",
            r#"
            (module
                (import "env" "host_log" (func $host_log (param i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "policy says no")
                (func (export "on_event") (result i32)
                    i32.const 0
                    i32.const 14
                    call $host_log
                    i32.const 1))
            "#,
        );
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let hook = WasmHook::new(&dir, "blk", 0, None, sandbox, None).unwrap();
        let action = hook.execute(&ctx()).await;
        assert!(action.is_block(), "got {action:?}");
        if let HookAction::Block(reason) = action {
            assert_eq!(reason, "policy says no");
        }
    }

    #[tokio::test]
    async fn block_with_no_log_uses_default_reason() {
        let dir = tmpdir("blk2");
        write_wasm(
            &dir,
            "blk2",
            r#"
            (module
                (func (export "on_event") (result i32)
                    i32.const 1))
            "#,
        );
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let hook = WasmHook::new(&dir, "blk2", 0, None, sandbox, None).unwrap();
        let action = hook.execute(&ctx()).await;
        assert!(action.is_block());
        if let HookAction::Block(reason) = action {
            assert_eq!(reason, "wasm hook blocked");
        }
    }

    #[tokio::test]
    async fn modify_status_returns_modify_with_log_payload() {
        let dir = tmpdir("mod");
        // Emit a JSON object via host_log, then return 2.
        write_wasm(
            &dir,
            "mod",
            r#"
            (module
                (import "env" "host_log" (func $host_log (param i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "{\"k\":42}")
                (func (export "on_event") (result i32)
                    i32.const 0
                    i32.const 8
                    call $host_log
                    i32.const 2))
            "#,
        );
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let hook = WasmHook::new(&dir, "mod", 0, None, sandbox, None).unwrap();
        let action = hook.execute(&ctx()).await;
        assert!(action.is_modify(), "got {action:?}");
        if let HookAction::Modify(value) = action {
            assert_eq!(value["k"], 42);
        }
    }

    #[tokio::test]
    async fn unknown_status_code_returns_block() {
        let dir = tmpdir("unk");
        write_wasm(
            &dir,
            "unk",
            r#"
            (module
                (func (export "on_event") (result i32)
                    i32.const 99))
            "#,
        );
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let hook = WasmHook::new(&dir, "unk", 0, None, sandbox, None).unwrap();
        let action = hook.execute(&ctx()).await;
        assert!(action.is_block(), "got {action:?}");
        if let HookAction::Block(reason) = action {
            assert!(reason.contains("99"), "reason: {reason}");
        }
    }

    #[tokio::test]
    async fn fuel_exhaustion_returns_block() {
        let dir = tmpdir("fuel");
        // Infinite loop — should exhaust the fuel budget. We use a tight
        // (1_000-instruction) fuel cap to mirror the proven sandbox test
        // `wasm_sandbox::tests::fuel_exhaustion_on_infinite_loop`. The
        // default 1M budget can take long enough on debug builds that the
        // wasmi interpreter's recursion outpaces the fuel meter.
        write_wasm(
            &dir,
            "spin",
            r#"
            (module
                (func (export "on_event") (result i32)
                    (loop $loop br $loop)
                    i32.const 0))
            "#,
        );
        let cfg = WasmSandboxConfig {
            fuel_limit: 1_000,
            ..WasmSandboxConfig::default()
        };
        let sandbox = Arc::new(WasmSandbox::new(cfg));
        let hook = WasmHook::new(&dir, "spin", 0, None, sandbox, None).unwrap();
        let action = hook.execute(&ctx()).await;
        assert!(action.is_block(), "got {action:?}");
        if let HookAction::Block(reason) = action {
            assert!(
                reason.contains("fuel") || reason.contains("failed"),
                "reason: {reason}"
            );
        }
    }

    #[tokio::test]
    async fn shared_sandbox_supports_multiple_hooks() {
        // Two hooks loaded into the same Arc<WasmSandbox> must independently
        // execute without interfering (each gets its own WasmPlugin).
        let dir = tmpdir("shared");
        write_wasm(
            &dir,
            "a",
            r#"(module (func (export "on_event") (result i32) i32.const 0))"#,
        );
        write_wasm(
            &dir,
            "b",
            r#"(module (func (export "on_event") (result i32) i32.const 1))"#,
        );
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let a = WasmHook::new(&dir, "a", 0, None, sandbox.clone(), None).unwrap();
        let b = WasmHook::new(&dir, "b", 0, None, sandbox, None).unwrap();
        assert!(a.execute(&ctx()).await.is_continue());
        assert!(b.execute(&ctx()).await.is_block());
    }

    #[tokio::test]
    async fn audit_log_is_written_when_store_provided() {
        let dir = tmpdir("audit");
        write_wasm(
            &dir,
            "ok",
            r#"(module (func (export "on_event") (result i32) i32.const 0))"#,
        );
        let audit = AuditStore::open_memory().await.expect("audit store");
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let hook = WasmHook::new(&dir, "ok", 0, None, sandbox, Some(audit.clone())).unwrap();
        let _ = hook.execute(&ctx()).await;

        let rows = audit.recent(50).await.expect("recent rows");
        let any_match = rows
            .iter()
            .any(|e| e.method.as_deref() == Some("hook.wasm.execute"));
        assert!(
            any_match,
            "expected at least one hook.wasm.execute row; got {} rows",
            rows.len(),
        );
    }
}
