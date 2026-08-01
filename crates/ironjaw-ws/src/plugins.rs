// Plugin system (Stream 13 / P3 Extension Points)
// Plugin manifest and registry system with WASM sandbox integration.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::wasm_sandbox::{WasmPlugin, WasmSandbox, WasmSandboxConfig, WasmSandboxError};

/// Declares a plugin's identity, required permissions, and configuration.
///
/// The `entry_point` field is reserved for future WASM module paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Unique plugin name used as the registry key.
    pub name: String,
    /// Semantic version string (e.g. "1.0.0").
    pub version: String,
    /// Optional author attribution.
    pub author: Option<String>,
    /// Human-readable description of the plugin's purpose.
    pub description: String,
    /// Capabilities this plugin requires to operate.
    pub permissions: Vec<PluginPermission>,
    /// Future: path to the WASM entry point file.
    pub entry_point: String,
    /// Arbitrary key-value configuration consumed by the plugin at runtime.
    pub config: HashMap<String, serde_json::Value>,
}

/// Capability a plugin may request. Checked via [`PluginRegistry::has_permission`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PluginPermission {
    ReadFiles,
    WriteFiles,
    Network,
    Shell,
    Brain,
    Tools,
    /// Permission to execute code inside a WASM sandbox.
    WasmExecution,
}

/// Lifecycle state of an installed plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PluginState {
    Installed,
    Active,
    Disabled,
    Error(String),
}

/// An installed plugin combining its manifest with runtime metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    pub state: PluginState,
    pub installed_at: DateTime<Utc>,
    pub last_active: Option<DateTime<Utc>>,
}

/// Errors returned by [`PluginRegistry`] operations.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PluginError {
    #[error("plugin not found: {0}")]
    NotFound(String),
    #[error("plugin already installed: {0}")]
    AlreadyInstalled(String),
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("plugin in error state: {0}")]
    InErrorState(String),
    #[error("wasm sandbox error: {0}")]
    WasmError(#[from] WasmSandboxError),
    #[error("plugin missing wasm permission: {0}")]
    MissingWasmPermission(String),
}

/// Registry of installed plugins. Manages install, activate, deactivate, and permission checks.
///
/// Plugins that declare the [`PluginPermission::WasmExecution`] permission can be
/// loaded into the embedded WASM sandbox via [`PluginRegistry::load_wasm`].
pub struct PluginRegistry {
    plugins: HashMap<String, InstalledPlugin>,
    /// Shared WASM sandbox engine (created lazily on first `load_wasm` call).
    wasm_sandbox: Option<WasmSandbox>,
    /// Live WASM plugin instances, keyed by plugin name.
    /// These are kept separately from `InstalledPlugin` because `WasmPlugin`
    /// is not serializable and not cloneable.
    wasm_plugins: HashMap<String, WasmPlugin>,
}

impl PluginRegistry {
    /// Create an empty plugin registry.
    pub fn new() -> Self {
        Self {
            plugins: HashMap::new(),
            wasm_sandbox: None,
            wasm_plugins: HashMap::new(),
        }
    }

    /// Validate a manifest before installation.
    ///
    /// Checks that `name`, `version`, and `entry_point` are non-empty.
    pub fn validate_manifest(manifest: &PluginManifest) -> Result<(), PluginError> {
        if manifest.name.trim().is_empty() {
            return Err(PluginError::InvalidManifest(
                "name cannot be empty".to_string(),
            ));
        }
        if manifest.version.trim().is_empty() {
            return Err(PluginError::InvalidManifest(
                "version cannot be empty".to_string(),
            ));
        }
        if manifest.entry_point.trim().is_empty() {
            return Err(PluginError::InvalidManifest(
                "entry_point cannot be empty".to_string(),
            ));
        }
        Ok(())
    }

    /// Install a plugin from its manifest.
    ///
    /// The manifest is validated first. The plugin starts in [`PluginState::Installed`].
    pub fn install(&mut self, manifest: PluginManifest) -> Result<(), PluginError> {
        Self::validate_manifest(&manifest)?;

        use std::collections::hash_map::Entry;
        match self.plugins.entry(manifest.name.clone()) {
            Entry::Occupied(e) => Err(PluginError::AlreadyInstalled(e.key().clone())),
            Entry::Vacant(e) => {
                e.insert(InstalledPlugin {
                    manifest,
                    state: PluginState::Installed,
                    installed_at: Utc::now(),
                    last_active: None,
                });
                Ok(())
            }
        }
    }

    /// Remove a plugin by name.
    pub fn uninstall(&mut self, name: &str) -> Result<(), PluginError> {
        if self.plugins.remove(name).is_none() {
            return Err(PluginError::NotFound(name.to_string()));
        }
        Ok(())
    }

    /// Activate an installed or disabled plugin.
    ///
    /// Fails if the plugin is in an error state.
    pub fn activate(&mut self, name: &str) -> Result<(), PluginError> {
        let plugin = self
            .plugins
            .get_mut(name)
            .ok_or_else(|| PluginError::NotFound(name.to_string()))?;

        if let PluginState::Error(ref msg) = plugin.state {
            return Err(PluginError::InErrorState(msg.clone()));
        }

        plugin.state = PluginState::Active;
        plugin.last_active = Some(Utc::now());
        Ok(())
    }

    /// Deactivate a plugin, moving it to [`PluginState::Disabled`].
    pub fn deactivate(&mut self, name: &str) -> Result<(), PluginError> {
        let plugin = self
            .plugins
            .get_mut(name)
            .ok_or_else(|| PluginError::NotFound(name.to_string()))?;

        plugin.state = PluginState::Disabled;
        Ok(())
    }

    /// Look up an installed plugin by name.
    pub fn get(&self, name: &str) -> Option<&InstalledPlugin> {
        self.plugins.get(name)
    }

    /// List all installed plugins, sorted by name.
    pub fn list(&self) -> Vec<&InstalledPlugin> {
        let mut plugins: Vec<&InstalledPlugin> = self.plugins.values().collect();
        plugins.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
        plugins
    }

    /// List only active plugins, sorted by name.
    pub fn list_active(&self) -> Vec<&InstalledPlugin> {
        let mut active: Vec<&InstalledPlugin> = self
            .plugins
            .values()
            .filter(|p| p.state == PluginState::Active)
            .collect();
        active.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
        active
    }

    /// Check whether a plugin has a specific permission.
    ///
    /// Returns `false` if the plugin is not installed.
    pub fn has_permission(&self, name: &str, perm: &PluginPermission) -> bool {
        self.plugins
            .get(name)
            .is_some_and(|p| p.manifest.permissions.contains(perm))
    }

    /// Number of installed plugins.
    pub fn count(&self) -> usize {
        self.plugins.len()
    }

    // -- WASM sandbox integration -----------------------------------------

    /// Configure the WASM sandbox engine. Must be called before [`load_wasm`]
    /// if you want non-default settings (memory limit, fuel budget, etc.).
    ///
    /// Replaces any previously configured sandbox. Existing loaded WASM plugins
    /// are **not** affected (they retain the engine that created them).
    pub fn configure_wasm(&mut self, config: WasmSandboxConfig) {
        self.wasm_sandbox = Some(WasmSandbox::new(config));
    }

    /// Return a reference to the WASM sandbox, if one has been configured.
    pub fn wasm_sandbox(&self) -> Option<&WasmSandbox> {
        self.wasm_sandbox.as_ref()
    }

    /// Load an installed plugin's WASM module into the sandbox.
    ///
    /// The plugin must:
    /// 1. Be installed in the registry
    /// 2. Have the [`PluginPermission::WasmExecution`] permission
    ///
    /// `wasm_bytes` is the raw content of the WASM (or WAT) file referenced
    /// by the plugin's `entry_point` field. The caller is responsible for
    /// reading the file from disk.
    ///
    /// The plugin's `config` map is converted from `serde_json::Value` to
    /// `String` values and passed to the WASM sandbox as host config.
    pub fn load_wasm(&mut self, name: &str, wasm_bytes: &[u8]) -> Result<(), PluginError> {
        // Verify the plugin exists
        let plugin = self
            .plugins
            .get(name)
            .ok_or_else(|| PluginError::NotFound(name.to_string()))?;

        // Verify WasmExecution permission
        if !plugin
            .manifest
            .permissions
            .contains(&PluginPermission::WasmExecution)
        {
            return Err(PluginError::MissingWasmPermission(name.to_string()));
        }

        // Lazily create the sandbox with defaults if not yet configured
        if self.wasm_sandbox.is_none() {
            self.wasm_sandbox = Some(WasmSandbox::with_defaults());
        }

        // Convert serde_json::Value config to HashMap<String, String>
        let config: HashMap<String, String> = plugin
            .manifest
            .config
            .iter()
            .map(|(k, v)| {
                let val = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                (k.clone(), val)
            })
            .collect();

        let sandbox = self.wasm_sandbox.as_ref().unwrap();
        let wasm_plugin = sandbox.load_plugin(name, wasm_bytes, config)?;

        self.wasm_plugins.insert(name.to_string(), wasm_plugin);
        Ok(())
    }

    /// Call an exported function on a loaded WASM plugin.
    ///
    /// The plugin must have been previously loaded via [`load_wasm`].
    /// Fuel is automatically reset before each call.
    pub fn call_wasm(
        &mut self,
        plugin_name: &str,
        function_name: &str,
        args: &[wasmi::Val],
    ) -> Result<Vec<wasmi::Val>, PluginError> {
        let sandbox = self
            .wasm_sandbox
            .as_ref()
            .ok_or(PluginError::WasmError(WasmSandboxError::PluginNotLoaded))?;

        let wasm_plugin = self
            .wasm_plugins
            .get_mut(plugin_name)
            .ok_or(PluginError::WasmError(WasmSandboxError::PluginNotLoaded))?;

        let results = sandbox.call_function(wasm_plugin, function_name, args)?;
        Ok(results)
    }

    /// Remove a loaded WASM plugin instance (does not uninstall the plugin).
    pub fn unload_wasm(&mut self, name: &str) -> bool {
        self.wasm_plugins.remove(name).is_some()
    }

    /// Return a reference to a loaded WASM plugin instance.
    pub fn get_wasm(&self, name: &str) -> Option<&WasmPlugin> {
        self.wasm_plugins.get(name)
    }

    /// Return a mutable reference to a loaded WASM plugin instance.
    pub fn get_wasm_mut(&mut self, name: &str) -> Option<&mut WasmPlugin> {
        self.wasm_plugins.get_mut(name)
    }

    /// Number of currently loaded WASM plugin instances.
    pub fn wasm_count(&self) -> usize {
        self.wasm_plugins.len()
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a valid manifest with the given name and permissions.
    fn make_manifest(name: &str, perms: Vec<PluginPermission>) -> PluginManifest {
        PluginManifest {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            author: Some("test-author".to_string()),
            description: format!("{name} plugin"),
            permissions: perms,
            entry_point: format!("{name}.wasm"),
            config: HashMap::new(),
        }
    }

    /// Helper: build a minimal valid manifest.
    fn simple_manifest(name: &str) -> PluginManifest {
        make_manifest(name, vec![PluginPermission::ReadFiles])
    }

    // -- validate_manifest -----------------------------------------------

    #[test]
    fn validate_manifest_accepts_valid() {
        let m = simple_manifest("good");
        assert!(PluginRegistry::validate_manifest(&m).is_ok());
    }

    #[test]
    fn validate_manifest_rejects_empty_name() {
        let mut m = simple_manifest("x");
        m.name = String::new();
        let err = PluginRegistry::validate_manifest(&m).unwrap_err();
        assert_eq!(
            err,
            PluginError::InvalidManifest("name cannot be empty".to_string())
        );
    }

    #[test]
    fn validate_manifest_rejects_whitespace_name() {
        let mut m = simple_manifest("x");
        m.name = "   ".to_string();
        assert!(PluginRegistry::validate_manifest(&m).is_err());
    }

    #[test]
    fn validate_manifest_rejects_empty_version() {
        let mut m = simple_manifest("x");
        m.version = String::new();
        let err = PluginRegistry::validate_manifest(&m).unwrap_err();
        assert_eq!(
            err,
            PluginError::InvalidManifest("version cannot be empty".to_string())
        );
    }

    #[test]
    fn validate_manifest_rejects_empty_entry_point() {
        let mut m = simple_manifest("x");
        m.entry_point = String::new();
        let err = PluginRegistry::validate_manifest(&m).unwrap_err();
        assert_eq!(
            err,
            PluginError::InvalidManifest("entry_point cannot be empty".to_string())
        );
    }

    // -- install ---------------------------------------------------------

    #[test]
    fn install_and_get() {
        let mut reg = PluginRegistry::new();
        reg.install(simple_manifest("search")).unwrap();

        let p = reg.get("search").unwrap();
        assert_eq!(p.manifest.name, "search");
        assert_eq!(p.state, PluginState::Installed);
        assert!(p.last_active.is_none());
    }

    #[test]
    fn install_duplicate_fails() {
        let mut reg = PluginRegistry::new();
        reg.install(simple_manifest("search")).unwrap();
        let err = reg.install(simple_manifest("search")).unwrap_err();
        assert_eq!(err, PluginError::AlreadyInstalled("search".to_string()));
    }

    #[test]
    fn install_invalid_manifest_fails() {
        let mut reg = PluginRegistry::new();
        let mut m = simple_manifest("x");
        m.name = String::new();
        assert!(reg.install(m).is_err());
    }

    // -- uninstall -------------------------------------------------------

    #[test]
    fn uninstall_removes_plugin() {
        let mut reg = PluginRegistry::new();
        reg.install(simple_manifest("x")).unwrap();
        assert_eq!(reg.count(), 1);

        reg.uninstall("x").unwrap();
        assert_eq!(reg.count(), 0);
        assert!(reg.get("x").is_none());
    }

    #[test]
    fn uninstall_nonexistent_fails() {
        let mut reg = PluginRegistry::new();
        let err = reg.uninstall("ghost").unwrap_err();
        assert_eq!(err, PluginError::NotFound("ghost".to_string()));
    }

    // -- activate / deactivate -------------------------------------------

    #[test]
    fn activate_installed_plugin() {
        let mut reg = PluginRegistry::new();
        reg.install(simple_manifest("x")).unwrap();

        reg.activate("x").unwrap();
        let p = reg.get("x").unwrap();
        assert_eq!(p.state, PluginState::Active);
        assert!(p.last_active.is_some());
    }

    #[test]
    fn activate_disabled_plugin() {
        let mut reg = PluginRegistry::new();
        reg.install(simple_manifest("x")).unwrap();
        reg.deactivate("x").unwrap();

        reg.activate("x").unwrap();
        assert_eq!(reg.get("x").unwrap().state, PluginState::Active);
    }

    #[test]
    fn activate_nonexistent_fails() {
        let mut reg = PluginRegistry::new();
        let err = reg.activate("ghost").unwrap_err();
        assert_eq!(err, PluginError::NotFound("ghost".to_string()));
    }

    #[test]
    fn activate_error_state_fails() {
        let mut reg = PluginRegistry::new();
        reg.install(simple_manifest("x")).unwrap();
        // Force error state via internal mutation.
        reg.plugins.get_mut("x").unwrap().state = PluginState::Error("crashed".to_string());

        let err = reg.activate("x").unwrap_err();
        assert_eq!(err, PluginError::InErrorState("crashed".to_string()));
    }

    #[test]
    fn deactivate_active_plugin() {
        let mut reg = PluginRegistry::new();
        reg.install(simple_manifest("x")).unwrap();
        reg.activate("x").unwrap();

        reg.deactivate("x").unwrap();
        assert_eq!(reg.get("x").unwrap().state, PluginState::Disabled);
    }

    #[test]
    fn deactivate_nonexistent_fails() {
        let mut reg = PluginRegistry::new();
        let err = reg.deactivate("ghost").unwrap_err();
        assert_eq!(err, PluginError::NotFound("ghost".to_string()));
    }

    // -- list / list_active ----------------------------------------------

    #[test]
    fn list_sorted_by_name() {
        let mut reg = PluginRegistry::new();
        reg.install(simple_manifest("zeta")).unwrap();
        reg.install(simple_manifest("alpha")).unwrap();
        reg.install(simple_manifest("mid")).unwrap();

        let list = reg.list();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].manifest.name, "alpha");
        assert_eq!(list[1].manifest.name, "mid");
        assert_eq!(list[2].manifest.name, "zeta");
    }

    #[test]
    fn list_active_filters_correctly() {
        let mut reg = PluginRegistry::new();
        reg.install(simple_manifest("active1")).unwrap();
        reg.install(simple_manifest("inactive")).unwrap();
        reg.install(simple_manifest("active2")).unwrap();
        reg.activate("active1").unwrap();
        reg.activate("active2").unwrap();

        let active = reg.list_active();
        assert_eq!(active.len(), 2);
        assert_eq!(active[0].manifest.name, "active1");
        assert_eq!(active[1].manifest.name, "active2");
    }

    // -- has_permission --------------------------------------------------

    #[test]
    fn has_permission_true_when_granted() {
        let mut reg = PluginRegistry::new();
        let m = make_manifest(
            "net-plugin",
            vec![PluginPermission::Network, PluginPermission::ReadFiles],
        );
        reg.install(m).unwrap();

        assert!(reg.has_permission("net-plugin", &PluginPermission::Network));
        assert!(reg.has_permission("net-plugin", &PluginPermission::ReadFiles));
    }

    #[test]
    fn has_permission_false_when_not_granted() {
        let mut reg = PluginRegistry::new();
        reg.install(make_manifest(
            "read-only",
            vec![PluginPermission::ReadFiles],
        ))
        .unwrap();

        assert!(!reg.has_permission("read-only", &PluginPermission::Shell));
        assert!(!reg.has_permission("read-only", &PluginPermission::WriteFiles));
    }

    #[test]
    fn has_permission_false_for_missing_plugin() {
        let reg = PluginRegistry::new();
        assert!(!reg.has_permission("ghost", &PluginPermission::Network));
    }

    // -- count / empty / default -----------------------------------------

    #[test]
    fn empty_registry() {
        let reg = PluginRegistry::new();
        assert_eq!(reg.count(), 0);
        assert!(reg.list().is_empty());
        assert!(reg.list_active().is_empty());
        assert!(reg.get("anything").is_none());
    }

    #[test]
    fn default_trait() {
        let reg = PluginRegistry::default();
        assert_eq!(reg.count(), 0);
    }

    #[test]
    fn count_tracks_installs_and_uninstalls() {
        let mut reg = PluginRegistry::new();
        reg.install(simple_manifest("a")).unwrap();
        reg.install(simple_manifest("b")).unwrap();
        assert_eq!(reg.count(), 2);

        reg.uninstall("a").unwrap();
        assert_eq!(reg.count(), 1);
    }

    // -- error display ---------------------------------------------------

    #[test]
    fn error_display_messages() {
        assert_eq!(
            PluginError::NotFound("x".into()).to_string(),
            "plugin not found: x"
        );
        assert_eq!(
            PluginError::AlreadyInstalled("x".into()).to_string(),
            "plugin already installed: x"
        );
        assert_eq!(
            PluginError::InvalidManifest("bad".into()).to_string(),
            "invalid manifest: bad"
        );
        assert_eq!(
            PluginError::InErrorState("crash".into()).to_string(),
            "plugin in error state: crash"
        );
    }

    // -- config / author -------------------------------------------------

    #[test]
    fn manifest_with_config() {
        let mut config = HashMap::new();
        config.insert(
            "max_retries".to_string(),
            serde_json::Value::Number(3.into()),
        );
        let m = PluginManifest {
            name: "configured".to_string(),
            version: "2.0.0".to_string(),
            author: None,
            description: "plugin with config".to_string(),
            permissions: vec![],
            entry_point: "configured.wasm".to_string(),
            config,
        };
        let mut reg = PluginRegistry::new();
        reg.install(m).unwrap();

        let p = reg.get("configured").unwrap();
        assert_eq!(p.manifest.config["max_retries"], 3);
        assert!(p.manifest.author.is_none());
    }

    #[test]
    fn manifest_preserves_all_permissions() {
        let perms = vec![
            PluginPermission::ReadFiles,
            PluginPermission::WriteFiles,
            PluginPermission::Network,
            PluginPermission::Shell,
            PluginPermission::Brain,
            PluginPermission::Tools,
            PluginPermission::WasmExecution,
        ];
        let m = make_manifest("all-perms", perms.clone());
        let mut reg = PluginRegistry::new();
        reg.install(m).unwrap();

        for perm in &perms {
            assert!(reg.has_permission("all-perms", perm));
        }
    }

    // -- WASM integration tests -------------------------------------------

    /// Helper: WAT module with an `add(i32,i32)->i32` export.
    fn wasm_add_bytes() -> Vec<u8> {
        r#"(module
            (func (export "add") (param i32 i32) (result i32)
                local.get 0
                local.get 1
                i32.add))"#
            .as_bytes()
            .to_vec()
    }

    /// Helper: build a manifest with WasmExecution permission.
    fn wasm_manifest(name: &str) -> PluginManifest {
        make_manifest(name, vec![PluginPermission::WasmExecution])
    }

    #[test]
    fn load_wasm_and_call() {
        let mut reg = PluginRegistry::new();
        reg.install(wasm_manifest("adder")).unwrap();
        reg.load_wasm("adder", &wasm_add_bytes()).unwrap();

        let results = reg
            .call_wasm("adder", "add", &[wasmi::Val::I32(3), wasmi::Val::I32(4)])
            .unwrap();
        assert_eq!(results.len(), 1);
        match results[0] {
            wasmi::Val::I32(v) => assert_eq!(v, 7),
            ref other => panic!("expected I32, got {other:?}"),
        }
    }

    #[test]
    fn load_wasm_missing_permission() {
        let mut reg = PluginRegistry::new();
        // Plugin without WasmExecution permission
        reg.install(simple_manifest("no-wasm")).unwrap();
        let err = reg.load_wasm("no-wasm", &wasm_add_bytes()).unwrap_err();
        assert!(matches!(err, PluginError::MissingWasmPermission(_)));
    }

    #[test]
    fn load_wasm_plugin_not_found() {
        let mut reg = PluginRegistry::new();
        let err = reg.load_wasm("ghost", &wasm_add_bytes()).unwrap_err();
        assert!(matches!(err, PluginError::NotFound(_)));
    }

    #[test]
    fn load_wasm_invalid_bytes() {
        let mut reg = PluginRegistry::new();
        reg.install(wasm_manifest("bad")).unwrap();
        let err = reg.load_wasm("bad", b"not wasm").unwrap_err();
        assert!(matches!(err, PluginError::WasmError(_)));
    }

    #[test]
    fn unload_wasm_plugin() {
        let mut reg = PluginRegistry::new();
        reg.install(wasm_manifest("x")).unwrap();
        reg.load_wasm("x", &wasm_add_bytes()).unwrap();
        assert_eq!(reg.wasm_count(), 1);

        assert!(reg.unload_wasm("x"));
        assert_eq!(reg.wasm_count(), 0);
        assert!(reg.get_wasm("x").is_none());
    }

    #[test]
    fn unload_nonexistent_wasm() {
        let mut reg = PluginRegistry::new();
        assert!(!reg.unload_wasm("ghost"));
    }

    #[test]
    fn get_wasm_returns_reference() {
        let mut reg = PluginRegistry::new();
        reg.install(wasm_manifest("ref-test")).unwrap();
        reg.load_wasm("ref-test", &wasm_add_bytes()).unwrap();

        let wasm = reg.get_wasm("ref-test").unwrap();
        assert_eq!(wasm.name, "ref-test");
    }

    #[test]
    fn call_wasm_not_loaded() {
        let mut reg = PluginRegistry::new();
        let err = reg
            .call_wasm("ghost", "add", &[wasmi::Val::I32(1)])
            .unwrap_err();
        assert!(matches!(err, PluginError::WasmError(_)));
    }

    #[test]
    fn configure_wasm_sandbox() {
        let mut reg = PluginRegistry::new();
        assert!(reg.wasm_sandbox().is_none());

        reg.configure_wasm(WasmSandboxConfig {
            fuel_limit: 500,
            ..WasmSandboxConfig::default()
        });
        assert!(reg.wasm_sandbox().is_some());
        assert_eq!(reg.wasm_sandbox().unwrap().config().fuel_limit, 500);
    }

    #[test]
    fn wasm_lazy_sandbox_creation() {
        let mut reg = PluginRegistry::new();
        assert!(reg.wasm_sandbox().is_none());

        // load_wasm should lazily create the sandbox
        reg.install(wasm_manifest("lazy")).unwrap();
        reg.load_wasm("lazy", &wasm_add_bytes()).unwrap();

        assert!(reg.wasm_sandbox().is_some());
    }

    #[test]
    fn wasm_config_passed_to_plugin() {
        let mut reg = PluginRegistry::new();
        let mut manifest = wasm_manifest("configured");
        manifest.config.insert(
            "api_key".to_string(),
            serde_json::Value::String("secret123".to_string()),
        );
        manifest
            .config
            .insert("retries".to_string(), serde_json::Value::Number(3.into()));
        reg.install(manifest).unwrap();

        // WAT module that uses host_get_config to check for "api_key"
        let config_wasm = r#"(module
            (import "env" "host_get_config" (func $get_config (param i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "api_key")
            (func (export "check") (result i32)
                i32.const 0
                i32.const 7
                call $get_config))"#
            .as_bytes();

        reg.load_wasm("configured", config_wasm).unwrap();
        let results = reg.call_wasm("configured", "check", &[]).unwrap();
        match results[0] {
            wasmi::Val::I32(v) => assert_eq!(v, 0), // 0 = found
            ref other => panic!("expected I32, got {other:?}"),
        }
    }

    #[test]
    fn wasm_error_display() {
        let err = PluginError::WasmError(WasmSandboxError::FuelExhausted);
        assert!(err.to_string().contains("fuel"));

        let err = PluginError::MissingWasmPermission("test".into());
        assert!(err.to_string().contains("test"));
    }
}
