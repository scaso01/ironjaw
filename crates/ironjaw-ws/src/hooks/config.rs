// B2 CP6: hook discovery + catalog.
//
// Scans the configured hooks directory for `<name>.wasm` modules and
// `<name>` executables, pairs them with sidecar `<name>.toml` manifests,
// and registers each into the `HookRegistry`. Maintains a parallel
// `HookCatalog` (the metadata-only listing) so `hooks.list` can answer
// without reaching into the registry.
//
// Security and gating live HERE, not inside the hook impls:
// - Subprocess hooks are skipped silently if `[hooks.subprocess].enabled`
//   is false OR the manifest's `event` is not in `enabled_events`. The
//   subprocess hook impl itself is always-execute by design (see CP4 docs).
// - Path traversal is rejected by `SubprocessHook::new` / `WasmHook::new`
//   themselves (CP4/CP5), so this module relies on those constructors and
//   logs+skips on failure rather than aborting startup.
// - Malformed manifest TOML, missing `event` field, or unknown event names
//   produce a warning and skip — never panic.

use ironjaw_audit::AuditStore;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::wasm_sandbox::WasmSandbox;

use super::registry::HookRegistry;
use super::subprocess::{SubprocessHook, HOOK_TIMEOUT_CEILING_SECS};
use super::types::HookEventKind;
use super::wasm_bridge::WasmHook;

/// Sidecar manifest stored as `<hooks_dir>/<name>.toml`. Pairs with either
/// `<name>.wasm` or `<name>` (executable) in the same directory.
#[derive(Debug, Clone, Deserialize)]
pub struct HookManifest {
    /// Event name as a string matching `HookEventKind` Display
    /// (e.g. "PreCompact", "SessionStart"). Parsed via FromStr.
    pub event: String,
    /// Optional priority. Lower runs first; ties preserve registration order.
    /// Default 0.
    #[serde(default)]
    pub priority: i32,
    /// Per-hook timeout override in seconds. Subprocess hooks only; ignored
    /// for WASM (which use fuel limits via WasmSandboxConfig). Clamped to
    /// `HOOK_TIMEOUT_CEILING_SECS` regardless of config ceiling.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Optional WASM exported entry-point function name. Default: `on_event`.
    /// Ignored for subprocess hooks.
    #[serde(default)]
    pub entry: Option<String>,
}

/// Hook backend kind, exposed via `hooks.list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookKind {
    Wasm,
    Subprocess,
}

impl HookKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookKind::Wasm => "wasm",
            HookKind::Subprocess => "subprocess",
        }
    }
}

/// One entry in the `hooks.list` response. Mirrors what was discovered
/// at startup (or last `hooks.reload`); independent of the registry's
/// internal Hook trait objects.
#[derive(Debug, Clone, Serialize)]
pub struct HookCatalogEntry {
    pub name: String,
    /// Stringified `HookEventKind`.
    pub event: String,
    pub kind: HookKind,
    /// True if the hook was actually registered (false if skipped due to
    /// gating, e.g. subprocess disabled or event not in allowlist).
    pub enabled: bool,
    /// If `enabled = false`, why we skipped registration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

/// Shared, mutable catalog of discovered hooks. Cloned-by-Arc into AppState
/// so handler.rs can answer `hooks.list` and `hooks.reload` can rebuild it.
pub type HookCatalog = Arc<RwLock<Vec<HookCatalogEntry>>>;

/// Construct an empty catalog. Used at AppState init when hooks are disabled
/// (so the field is always present) and as the seed for discovery.
pub fn empty_catalog() -> HookCatalog {
    Arc::new(RwLock::new(Vec::new()))
}

/// Errors raised by the discovery pipeline. Most failure modes are warned
/// and skipped, not propagated; we only return Err for unrecoverable issues
/// like a missing hooks directory.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("hooks directory '{0}' does not exist")]
    DirMissing(PathBuf),
    #[error("io error reading hooks dir: {0}")]
    Io(#[from] std::io::Error),
}

/// Scan `dir` for hook artifacts and register each into `registry` (subject
/// to subprocess-gating in `subprocess_enabled`/`subprocess_enabled_events`).
/// The returned catalog reflects EVERY discovered manifest, including those
/// that were skipped (with `enabled=false` + `skip_reason`).
///
/// Sandbox is reused across all WASM hooks (one engine, many plugins).
/// Audit, when `Some`, is threaded into both subprocess and WASM hooks.
pub async fn discover_and_register(
    registry: &HookRegistry,
    sandbox: Arc<WasmSandbox>,
    audit: Option<AuditStore>,
    dir: &Path,
    subprocess_enabled: bool,
    subprocess_enabled_events: &[String],
    subprocess_default_timeout_secs: u64,
) -> Result<Vec<HookCatalogEntry>, DiscoveryError> {
    if !dir.exists() {
        return Err(DiscoveryError::DirMissing(dir.to_path_buf()));
    }

    let mut entries: Vec<HookCatalogEntry> = Vec::new();
    let mut read_dir = tokio::fs::read_dir(dir).await?;

    while let Some(entry) = read_dir.next_entry().await? {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            // No extension: treat as potential subprocess executable; require
            // a sidecar <name>.toml to confirm intent.
            if let Some(catalog_entry) = try_register_subprocess(
                registry,
                audit.as_ref(),
                dir,
                &stem,
                subprocess_enabled,
                subprocess_enabled_events,
                subprocess_default_timeout_secs,
            )
            .await
            {
                entries.push(catalog_entry);
            }
            continue;
        };

        match ext {
            "wasm" => {
                if let Some(catalog_entry) =
                    try_register_wasm(registry, &sandbox, audit.as_ref(), dir, &stem).await
                {
                    entries.push(catalog_entry);
                }
            }
            "toml" => {
                // Manifests are read alongside their counterpart; standalone
                // `.toml` without a matching .wasm or executable is a no-op.
                debug!(name = %stem, "manifest seen; will pair with binary");
            }
            // Treat common subprocess extensions as such.
            "sh" | "py" | "exe" | "bat" | "cmd" | "ps1" => {
                if let Some(catalog_entry) = try_register_subprocess(
                    registry,
                    audit.as_ref(),
                    dir,
                    &stem,
                    subprocess_enabled,
                    subprocess_enabled_events,
                    subprocess_default_timeout_secs,
                )
                .await
                {
                    entries.push(catalog_entry);
                }
            }
            other => {
                debug!(file = %path.display(), ext = other, "ignoring unknown hook artifact");
            }
        }
    }

    info!(
        dir = %dir.display(),
        total = entries.len(),
        registered = entries.iter().filter(|e| e.enabled).count(),
        "Hook discovery complete"
    );
    Ok(entries)
}

/// Read and parse `<hooks_dir>/<name>.toml`. Returns None (with a warning)
/// if missing or unparseable; callers treat that as "skip this hook".
async fn load_manifest(hooks_dir: &Path, name: &str) -> Option<HookManifest> {
    let manifest_path = hooks_dir.join(format!("{name}.toml"));
    if !manifest_path.exists() {
        debug!(name = %name, "no sidecar manifest; skipping");
        return None;
    }
    let raw = match tokio::fs::read_to_string(&manifest_path).await {
        Ok(s) => s,
        Err(e) => {
            warn!(name = %name, error = %e, "failed to read hook manifest");
            return None;
        }
    };
    match toml::from_str::<HookManifest>(&raw) {
        Ok(m) => Some(m),
        Err(e) => {
            warn!(name = %name, error = %e, "malformed hook manifest");
            None
        }
    }
}

async fn try_register_wasm(
    registry: &HookRegistry,
    sandbox: &Arc<WasmSandbox>,
    audit: Option<&AuditStore>,
    hooks_dir: &Path,
    name: &str,
) -> Option<HookCatalogEntry> {
    let manifest = load_manifest(hooks_dir, name).await?;
    let kind = match HookEventKind::from_str(&manifest.event) {
        Ok(k) => k,
        Err(_) => {
            warn!(name = %name, event = %manifest.event, "unknown hook event");
            return Some(HookCatalogEntry {
                name: name.to_string(),
                event: manifest.event,
                kind: HookKind::Wasm,
                enabled: false,
                skip_reason: Some("unknown event".to_string()),
            });
        }
    };

    let entry_fn = manifest
        .entry
        .clone()
        .unwrap_or_else(|| super::wasm_bridge::DEFAULT_HOOK_FUNCTION.to_string());

    let hook = match WasmHook::new(
        hooks_dir,
        name,
        manifest.priority,
        Some(entry_fn.as_str()),
        Arc::clone(sandbox),
        audit.cloned(),
    ) {
        Ok(h) => h,
        Err(e) => {
            warn!(name = %name, error = %e, "failed to construct WASM hook");
            return Some(HookCatalogEntry {
                name: name.to_string(),
                event: kind.to_string(),
                kind: HookKind::Wasm,
                enabled: false,
                skip_reason: Some(format!("construct: {e}")),
            });
        }
    };

    registry.register_hook(kind, Arc::new(hook)).await;
    info!(name = %name, event = %kind, "WASM hook registered");
    Some(HookCatalogEntry {
        name: name.to_string(),
        event: kind.to_string(),
        kind: HookKind::Wasm,
        enabled: true,
        skip_reason: None,
    })
}

async fn try_register_subprocess(
    registry: &HookRegistry,
    audit: Option<&AuditStore>,
    hooks_dir: &Path,
    name: &str,
    subprocess_enabled: bool,
    subprocess_enabled_events: &[String],
    default_timeout_secs: u64,
) -> Option<HookCatalogEntry> {
    let manifest = load_manifest(hooks_dir, name).await?;
    let kind = match HookEventKind::from_str(&manifest.event) {
        Ok(k) => k,
        Err(_) => {
            warn!(name = %name, event = %manifest.event, "unknown hook event");
            return Some(HookCatalogEntry {
                name: name.to_string(),
                event: manifest.event,
                kind: HookKind::Subprocess,
                enabled: false,
                skip_reason: Some("unknown event".to_string()),
            });
        }
    };

    if !subprocess_enabled {
        debug!(name = %name, "subprocess hooks globally disabled; cataloging as skipped");
        return Some(HookCatalogEntry {
            name: name.to_string(),
            event: kind.to_string(),
            kind: HookKind::Subprocess,
            enabled: false,
            skip_reason: Some("subprocess disabled".to_string()),
        });
    }
    let event_name = kind.to_string();
    if !subprocess_enabled_events.iter().any(|e| e == &event_name) {
        debug!(name = %name, event = %event_name, "event not in subprocess allowlist; skipping");
        return Some(HookCatalogEntry {
            name: name.to_string(),
            event: event_name,
            kind: HookKind::Subprocess,
            enabled: false,
            skip_reason: Some("event not allowlisted".to_string()),
        });
    }

    let timeout_secs = manifest
        .timeout_secs
        .unwrap_or(default_timeout_secs)
        .min(HOOK_TIMEOUT_CEILING_SECS);

    let hook = match SubprocessHook::new(
        hooks_dir,
        name,
        manifest.priority,
        timeout_secs,
        audit.cloned(),
    ) {
        Ok(h) => h,
        Err(e) => {
            warn!(name = %name, error = %e, "failed to construct subprocess hook");
            return Some(HookCatalogEntry {
                name: name.to_string(),
                event: event_name,
                kind: HookKind::Subprocess,
                enabled: false,
                skip_reason: Some(format!("construct: {e}")),
            });
        }
    };

    registry.register_hook(kind, Arc::new(hook)).await;
    info!(name = %name, event = %event_name, "subprocess hook registered");
    Some(HookCatalogEntry {
        name: name.to_string(),
        event: event_name,
        kind: HookKind::Subprocess,
        enabled: true,
        skip_reason: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn discovery_errors_when_dir_missing() {
        let registry = HookRegistry::new();
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let bogus = PathBuf::from("/this/does/not/exist/ironjaw-hooks-test");
        let res = discover_and_register(&registry, sandbox, None, &bogus, false, &[], 5).await;
        assert!(matches!(res, Err(DiscoveryError::DirMissing(_))));
    }

    #[tokio::test]
    async fn empty_dir_yields_empty_catalog() {
        let dir = TempDir::new().unwrap();
        let registry = HookRegistry::new();
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let entries =
            discover_and_register(&registry, sandbox, None, dir.path(), false, &[], 5).await;
        assert!(entries.is_ok());
        assert!(entries.unwrap().is_empty());
    }

    #[tokio::test]
    async fn standalone_manifest_without_binary_is_noop() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(
            dir.path().join("orphan.toml"),
            "event = \"SessionStart\"\npriority = 0\n",
        )
        .await
        .unwrap();
        let registry = HookRegistry::new();
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let entries =
            discover_and_register(&registry, sandbox, None, dir.path(), false, &[], 5).await;
        let entries = entries.unwrap();
        // The .toml itself is treated as a manifest, but with no binary
        // counterpart we never call a register_* helper, so the catalog
        // remains empty.
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn malformed_manifest_skips_with_warning() {
        let dir = TempDir::new().unwrap();
        // No matching binary, but we test the manifest parser path here.
        tokio::fs::write(dir.path().join("badmanifest.toml"), "garbage = [")
            .await
            .unwrap();
        let m = load_manifest(dir.path(), "badmanifest").await;
        assert!(m.is_none(), "malformed manifest must yield None");
    }

    #[tokio::test]
    async fn missing_manifest_skips_silently() {
        let dir = TempDir::new().unwrap();
        let m = load_manifest(dir.path(), "nope").await;
        assert!(m.is_none());
    }

    #[test]
    fn hook_kind_serializes_lowercase() {
        let s = serde_json::to_string(&HookKind::Wasm).unwrap();
        assert_eq!(s, "\"wasm\"");
        let s = serde_json::to_string(&HookKind::Subprocess).unwrap();
        assert_eq!(s, "\"subprocess\"");
    }

    #[test]
    fn hook_kind_as_str_matches_serde() {
        assert_eq!(HookKind::Wasm.as_str(), "wasm");
        assert_eq!(HookKind::Subprocess.as_str(), "subprocess");
    }

    #[tokio::test]
    async fn subprocess_disabled_globally_marks_skip_reason() {
        let dir = TempDir::new().unwrap();
        // Create a fake .sh + sidecar .toml.
        tokio::fs::write(dir.path().join("greeter.sh"), "#!/bin/sh\necho ok\n")
            .await
            .unwrap();
        tokio::fs::write(
            dir.path().join("greeter.toml"),
            "event = \"SessionStart\"\npriority = 0\n",
        )
        .await
        .unwrap();
        let registry = HookRegistry::new();
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let entries = discover_and_register(&registry, sandbox, None, dir.path(), false, &[], 5)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert!(!e.enabled);
        assert_eq!(e.kind, HookKind::Subprocess);
        assert_eq!(e.event, "SessionStart");
        assert_eq!(e.skip_reason.as_deref(), Some("subprocess disabled"));
    }

    #[tokio::test]
    async fn subprocess_event_not_allowlisted_marks_skip_reason() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("greeter.sh"), "#!/bin/sh\necho ok\n")
            .await
            .unwrap();
        tokio::fs::write(
            dir.path().join("greeter.toml"),
            "event = \"SessionStart\"\npriority = 0\n",
        )
        .await
        .unwrap();
        let registry = HookRegistry::new();
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let entries = discover_and_register(
            &registry,
            sandbox,
            None,
            dir.path(),
            true, // globally enabled
            &["PreCompact".to_string()],
            5,
        )
        .await
        .unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert!(!e.enabled);
        assert_eq!(e.skip_reason.as_deref(), Some("event not allowlisted"));
    }

    #[tokio::test]
    async fn unknown_event_in_manifest_is_skipped() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("badwasm.wasm"), &[0u8; 4])
            .await
            .unwrap();
        tokio::fs::write(
            dir.path().join("badwasm.toml"),
            "event = \"NotARealEvent\"\npriority = 0\n",
        )
        .await
        .unwrap();
        let registry = HookRegistry::new();
        let sandbox = Arc::new(WasmSandbox::with_defaults());
        let entries = discover_and_register(&registry, sandbox, None, dir.path(), false, &[], 5)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].enabled);
        assert_eq!(entries[0].skip_reason.as_deref(), Some("unknown event"));
    }
}
