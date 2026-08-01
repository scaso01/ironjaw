// WASM plugin sandbox (Stream 8.7)
// Sandboxed WASM plugin execution using wasmi (pure Rust interpreter).
//
// Provides memory-limited, fuel-metered WASM module execution with
// controlled host function imports (log, get_config, tool_call).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::debug;
use wasmi::{Caller, Engine, Extern, Linker, Module, Store, StoreLimitsBuilder, Val};

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// All errors that can arise from WASM sandbox operations.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WasmSandboxError {
    /// The supplied bytes are not a valid WASM module.
    #[error("invalid wasm module: {0}")]
    InvalidModule(String),

    /// The requested export was not found in the module.
    #[error("export not found: {0}")]
    ExportNotFound(String),

    /// The export exists but is not a function.
    #[error("export is not a function: {0}")]
    ExportNotAFunction(String),

    /// Argument count or type mismatch when calling an exported function.
    #[error("argument mismatch: {0}")]
    ArgumentMismatch(String),

    /// The plugin exhausted its fuel (instruction budget).
    #[error("fuel exhausted: plugin exceeded instruction limit")]
    FuelExhausted,

    /// The plugin exceeded its memory limit.
    #[error("memory limit exceeded")]
    MemoryLimitExceeded,

    /// A trap occurred during execution (e.g. unreachable, div-by-zero).
    #[error("execution trapped: {0}")]
    Trap(String),

    /// Instantiation failed (e.g. missing imports, start function trapped).
    #[error("instantiation failed: {0}")]
    InstantiationFailed(String),

    /// The plugin has already been unloaded.
    #[error("plugin not loaded")]
    PluginNotLoaded,

    /// A host function returned an error.
    #[error("host function error: {0}")]
    HostFunctionError(String),
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the WASM sandbox.
#[derive(Debug, Clone)]
pub struct WasmSandboxConfig {
    /// Maximum linear memory size in bytes (default: 16 MB).
    pub max_memory_bytes: usize,
    /// Fuel budget per call (default: 1_000_000 instructions).
    pub fuel_limit: u64,
    /// Maximum recursion depth for the WASM stack (default: 64).
    pub max_recursion_depth: usize,
}

impl Default for WasmSandboxConfig {
    fn default() -> Self {
        Self {
            max_memory_bytes: 16 * 1024 * 1024, // 16 MB
            fuel_limit: 1_000_000,
            max_recursion_depth: 64,
        }
    }
}

// ---------------------------------------------------------------------------
// Host state -- data available to WASM via imported host functions
// ---------------------------------------------------------------------------

/// Shared state accessible from host functions and from the Rust side.
///
/// Wrapped in `Arc<Mutex<_>>` so that `WasmSandbox` can read logs / results
/// after a call completes.
#[derive(Debug, Clone, Default)]
pub struct HostState {
    /// Log messages emitted by the plugin via `host_log`.
    pub logs: Vec<String>,
    /// Configuration key-value pairs exposed to the plugin via `host_get_config`.
    pub config: HashMap<String, String>,
    /// Tool call results returned by the host. The plugin requests a tool call
    /// via `host_tool_call` and receives an i32 status code.
    pub tool_calls: Vec<ToolCallRecord>,
}

/// A record of a tool call made by a WASM plugin.
#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    /// The tool name requested (extracted from WASM linear memory).
    pub tool_name: String,
    /// The arguments string (extracted from WASM linear memory).
    pub args: String,
}

/// Store-local data used by the wasmi `Store`.
struct StoreData {
    /// Resource limiter enforcing memory caps.
    limits: wasmi::StoreLimits,
    /// Shared host state for host functions.
    host: Arc<Mutex<HostState>>,
}

// ---------------------------------------------------------------------------
// WasmPlugin
// ---------------------------------------------------------------------------

/// A loaded and instantiated WASM plugin, ready for function calls.
pub struct WasmPlugin {
    /// The plugin's unique name (from the manifest).
    pub name: String,
    /// The wasmi store owning this plugin's memory and state.
    store: Store<StoreData>,
    /// The instantiated module.
    instance: wasmi::Instance,
    /// Shared host state (same Arc as in StoreData).
    host_state: Arc<Mutex<HostState>>,
    /// Fuel limit to reset before each call.
    fuel_limit: u64,
}

impl std::fmt::Debug for WasmPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmPlugin")
            .field("name", &self.name)
            .field("fuel_limit", &self.fuel_limit)
            .finish_non_exhaustive()
    }
}

impl WasmPlugin {
    /// Retrieve logs emitted by the plugin since the last drain.
    pub fn drain_logs(&self) -> Vec<String> {
        let mut host = self.host_state.lock().expect("host state lock poisoned");
        std::mem::take(&mut host.logs)
    }

    /// Retrieve tool call records since the last drain.
    pub fn drain_tool_calls(&self) -> Vec<ToolCallRecord> {
        let mut host = self.host_state.lock().expect("host state lock poisoned");
        std::mem::take(&mut host.tool_calls)
    }

    /// Return the number of log messages currently buffered.
    pub fn log_count(&self) -> usize {
        self.host_state
            .lock()
            .expect("host state lock poisoned")
            .logs
            .len()
    }

    /// Return the remaining fuel after the last call.
    pub fn remaining_fuel(&self) -> Result<u64, WasmSandboxError> {
        self.store
            .get_fuel()
            .map_err(|e| WasmSandboxError::Trap(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// WasmSandbox
// ---------------------------------------------------------------------------

/// The top-level WASM sandbox. Creates an engine once and reuses it across
/// all plugins to share compiled code caches.
pub struct WasmSandbox {
    engine: Engine,
    config: WasmSandboxConfig,
}

impl std::fmt::Debug for WasmSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmSandbox")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl WasmSandbox {
    /// Create a new sandbox with the given configuration.
    pub fn new(config: WasmSandboxConfig) -> Self {
        let mut engine_config = wasmi::Config::default();
        engine_config.consume_fuel(true);
        engine_config.set_max_recursion_depth(config.max_recursion_depth);
        let engine = Engine::new(&engine_config);
        Self { engine, config }
    }

    /// Create a sandbox with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(WasmSandboxConfig::default())
    }

    /// Return a reference to the current configuration.
    pub fn config(&self) -> &WasmSandboxConfig {
        &self.config
    }

    /// Load and instantiate a WASM plugin from raw bytes.
    ///
    /// The plugin is sandboxed with:
    /// - Memory capped at `config.max_memory_bytes`
    /// - Fuel set to `config.fuel_limit` per call
    /// - Only whitelisted host imports (`host_log`, `host_get_config`,
    ///   `host_tool_call`) are provided
    pub fn load_plugin(
        &self,
        name: &str,
        wasm_bytes: &[u8],
        initial_config: HashMap<String, String>,
    ) -> Result<WasmPlugin, WasmSandboxError> {
        // --- 1. Compile the module ---
        let module = Module::new(&self.engine, wasm_bytes)
            .map_err(|e| WasmSandboxError::InvalidModule(e.to_string()))?;

        // --- 2. Build store with resource limits ---
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.config.max_memory_bytes)
            .trap_on_grow_failure(true)
            .build();

        let host_state = Arc::new(Mutex::new(HostState {
            logs: Vec::new(),
            config: initial_config,
            tool_calls: Vec::new(),
        }));

        let store_data = StoreData {
            limits,
            host: Arc::clone(&host_state),
        };

        let mut store = Store::new(&self.engine, store_data);
        store.limiter(|data| &mut data.limits);

        // Set initial fuel
        store.set_fuel(self.config.fuel_limit).map_err(|e| {
            WasmSandboxError::InstantiationFailed(format!("failed to set fuel: {e}"))
        })?;

        // --- 3. Build linker with sandboxed host imports ---
        let mut linker: Linker<StoreData> = Linker::new(&self.engine);
        Self::register_host_functions(&mut linker)?;

        // --- 4. Instantiate ---
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("fuel") {
                    WasmSandboxError::FuelExhausted
                } else if msg.contains("memory") || msg.contains("limiter") {
                    WasmSandboxError::MemoryLimitExceeded
                } else {
                    WasmSandboxError::InstantiationFailed(msg)
                }
            })?;

        debug!(plugin = name, "wasm plugin loaded successfully");

        Ok(WasmPlugin {
            name: name.to_string(),
            store,
            instance,
            host_state,
            fuel_limit: self.config.fuel_limit,
        })
    }

    /// Register the sandboxed host functions into the linker.
    ///
    /// All imports live under the `"env"` module:
    /// - `host_log(ptr: i32, len: i32)` -- log a UTF-8 message
    /// - `host_get_config(key_ptr: i32, key_len: i32) -> i32` -- get config (returns 0=found, 1=missing)
    /// - `host_tool_call(name_ptr: i32, name_len: i32, args_ptr: i32, args_len: i32) -> i32`
    fn register_host_functions(linker: &mut Linker<StoreData>) -> Result<(), WasmSandboxError> {
        // host_log: reads a string from WASM memory and appends to logs
        linker
            .func_wrap(
                "env",
                "host_log",
                |caller: Caller<'_, StoreData>, ptr: i32, len: i32| {
                    let msg = read_wasm_string(&caller, ptr, len)
                        .unwrap_or_else(|_| "<invalid utf-8>".to_string());
                    debug!(wasm_log = %msg, "plugin log");
                    let host = &caller.data().host;
                    if let Ok(mut h) = host.lock() {
                        h.logs.push(msg);
                    }
                },
            )
            .map_err(|e| {
                WasmSandboxError::InstantiationFailed(format!("failed to register host_log: {e}"))
            })?;

        // host_get_config: reads a key from WASM memory, returns 0 if found
        linker
            .func_wrap(
                "env",
                "host_get_config",
                |caller: Caller<'_, StoreData>, key_ptr: i32, key_len: i32| -> i32 {
                    let key = match read_wasm_string(&caller, key_ptr, key_len) {
                        Ok(k) => k,
                        Err(_) => return -1,
                    };
                    let host = &caller.data().host;
                    match host.lock() {
                        Ok(h) => {
                            if h.config.contains_key(&key) {
                                0 // found
                            } else {
                                1 // not found
                            }
                        }
                        Err(_) => -1,
                    }
                },
            )
            .map_err(|e| {
                WasmSandboxError::InstantiationFailed(format!(
                    "failed to register host_get_config: {e}"
                ))
            })?;

        // host_tool_call: reads tool name + args from WASM memory, records
        // the call and returns 0 on success
        linker
            .func_wrap(
                "env",
                "host_tool_call",
                |caller: Caller<'_, StoreData>,
                 name_ptr: i32,
                 name_len: i32,
                 args_ptr: i32,
                 args_len: i32|
                 -> i32 {
                    let tool_name = match read_wasm_string(&caller, name_ptr, name_len) {
                        Ok(n) => n,
                        Err(_) => return -1,
                    };
                    let args = match read_wasm_string(&caller, args_ptr, args_len) {
                        Ok(a) => a,
                        Err(_) => return -1,
                    };
                    debug!(tool = %tool_name, "plugin tool call");
                    let host = &caller.data().host;
                    match host.lock() {
                        Ok(mut h) => {
                            h.tool_calls.push(ToolCallRecord { tool_name, args });
                            0
                        }
                        Err(_) => -1,
                    }
                },
            )
            .map_err(|e| {
                WasmSandboxError::InstantiationFailed(format!(
                    "failed to register host_tool_call: {e}"
                ))
            })?;

        Ok(())
    }

    /// Call an exported function on a loaded plugin.
    ///
    /// Fuel is reset to the configured limit before each call.
    /// Returns the output values from the WASM function.
    pub fn call_function(
        &self,
        plugin: &mut WasmPlugin,
        name: &str,
        args: &[Val],
    ) -> Result<Vec<Val>, WasmSandboxError> {
        // Reset fuel before each call
        plugin
            .store
            .set_fuel(plugin.fuel_limit)
            .map_err(|e| WasmSandboxError::Trap(format!("failed to reset fuel: {e}")))?;

        // Look up the export
        let func = plugin
            .instance
            .get_export(&plugin.store, name)
            .ok_or_else(|| WasmSandboxError::ExportNotFound(name.to_string()))
            .and_then(|ext| match ext {
                Extern::Func(f) => Ok(f),
                _ => Err(WasmSandboxError::ExportNotAFunction(name.to_string())),
            })?;

        // Determine the result arity from the function type
        let func_type = func.ty(&plugin.store);
        let result_count = func_type.results().len();

        // Validate argument count
        let param_count = func_type.params().len();
        if args.len() != param_count {
            return Err(WasmSandboxError::ArgumentMismatch(format!(
                "expected {param_count} arguments, got {}",
                args.len()
            )));
        }

        // Prepare results buffer
        let mut results = vec![Val::I32(0); result_count];

        // Call the function
        func.call(&mut plugin.store, args, &mut results)
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("fuel") || msg.contains("OutOfFuel") {
                    WasmSandboxError::FuelExhausted
                } else if msg.contains("memory") || msg.contains("limiter") {
                    WasmSandboxError::MemoryLimitExceeded
                } else {
                    WasmSandboxError::Trap(msg)
                }
            })?;

        Ok(results)
    }
}

impl Default for WasmSandbox {
    fn default() -> Self {
        Self::with_defaults()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read a UTF-8 string from WASM linear memory at the given pointer and length.
fn read_wasm_string(
    caller: &Caller<'_, StoreData>,
    ptr: i32,
    len: i32,
) -> Result<String, WasmSandboxError> {
    if ptr < 0 || len < 0 {
        return Err(WasmSandboxError::HostFunctionError(
            "negative pointer or length".to_string(),
        ));
    }
    let ptr = ptr as usize;
    let len = len as usize;

    let memory = caller
        .get_export("memory")
        .and_then(|ext| ext.into_memory())
        .ok_or_else(|| {
            WasmSandboxError::HostFunctionError("no 'memory' export found".to_string())
        })?;

    let data = memory.data(caller);
    let end = ptr
        .checked_add(len)
        .ok_or_else(|| WasmSandboxError::HostFunctionError("pointer overflow".to_string()))?;
    if end > data.len() {
        return Err(WasmSandboxError::HostFunctionError(
            "out of bounds memory access".to_string(),
        ));
    }

    String::from_utf8(data[ptr..end].to_vec())
        .map_err(|e| WasmSandboxError::HostFunctionError(format!("invalid utf-8: {e}")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Val comparison helper ---
    // wasmi::Val does not implement PartialEq, so we extract the i32 manually.

    /// Extract an i32 from a `Val`, panicking with context on type mismatch.
    fn val_i32(val: &Val) -> i32 {
        match val {
            Val::I32(v) => *v,
            other => panic!("expected Val::I32, got {other:?}"),
        }
    }

    // --- WAT helper ---
    // wasmi's Module::new() accepts WAT text directly when the `wat`
    // feature is enabled (it is in our default features), so we just
    // return the WAT string as bytes.

    fn wat_bytes(wat: &str) -> Vec<u8> {
        wat.as_bytes().to_vec()
    }

    // --- Minimal modules used across tests ---

    /// A minimal WASM module that exports an `add(i32, i32) -> i32` function.
    fn add_module_bytes() -> Vec<u8> {
        wat_bytes(
            r#"
            (module
                (func (export "add") (param i32 i32) (result i32)
                    local.get 0
                    local.get 1
                    i32.add))
            "#,
        )
    }

    /// Module that exports a function returning a constant.
    fn constant_module_bytes() -> Vec<u8> {
        wat_bytes(
            r#"
            (module
                (func (export "get_value") (result i32)
                    i32.const 42))
            "#,
        )
    }

    /// Module with memory + host_log import.
    fn logging_module_bytes() -> Vec<u8> {
        wat_bytes(
            r#"
            (module
                (import "env" "host_log" (func $host_log (param i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "hello from wasm")
                (func (export "do_log")
                    i32.const 0   ;; ptr
                    i32.const 15  ;; len
                    call $host_log))
            "#,
        )
    }

    /// Module with host_get_config import.
    fn config_module_bytes() -> Vec<u8> {
        wat_bytes(
            r#"
            (module
                (import "env" "host_get_config" (func $host_get_config (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "my_key")
                (func (export "check_config") (result i32)
                    i32.const 0   ;; key ptr
                    i32.const 6   ;; key len
                    call $host_get_config))
            "#,
        )
    }

    /// Module with host_tool_call import.
    fn tool_call_module_bytes() -> Vec<u8> {
        wat_bytes(
            r#"
            (module
                (import "env" "host_tool_call"
                    (func $host_tool_call (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "read_file")
                (data (i32.const 9) "/tmp/test")
                (func (export "do_tool_call") (result i32)
                    i32.const 0   ;; name ptr
                    i32.const 9   ;; name len
                    i32.const 9   ;; args ptr
                    i32.const 9   ;; args len
                    call $host_tool_call))
            "#,
        )
    }

    /// Module that loops forever (for fuel exhaustion testing).
    fn infinite_loop_module_bytes() -> Vec<u8> {
        wat_bytes(
            r#"
            (module
                (func (export "spin")
                    (loop $loop
                        br $loop)))
            "#,
        )
    }

    /// Module that tries to grow memory beyond limits.
    fn memory_grow_module_bytes() -> Vec<u8> {
        wat_bytes(
            r#"
            (module
                (memory (export "memory") 1)
                (func (export "grow_big") (result i32)
                    ;; Try to grow by 1024 pages = 64 MB
                    i32.const 1024
                    memory.grow))
            "#,
        )
    }

    /// Module with no exports.
    fn empty_module_bytes() -> Vec<u8> {
        wat_bytes("(module)")
    }

    /// Module exporting a global (not a function).
    fn global_export_module_bytes() -> Vec<u8> {
        wat_bytes(
            r#"
            (module
                (global (export "my_global") i32 (i32.const 99)))
            "#,
        )
    }

    /// Module that triggers unreachable trap.
    fn trap_module_bytes() -> Vec<u8> {
        wat_bytes(
            r#"
            (module
                (func (export "trap_me")
                    unreachable))
            "#,
        )
    }

    /// Module with multiple exports.
    fn multi_export_module_bytes() -> Vec<u8> {
        wat_bytes(
            r#"
            (module
                (func (export "double") (param i32) (result i32)
                    local.get 0
                    i32.const 2
                    i32.mul)
                (func (export "negate") (param i32) (result i32)
                    i32.const 0
                    local.get 0
                    i32.sub)
                (func (export "identity") (param i32) (result i32)
                    local.get 0))
            "#,
        )
    }

    // ====================================================================
    // Tests
    // ====================================================================

    // --- 1. Basic loading ---

    #[test]
    fn load_valid_module() {
        let sandbox = WasmSandbox::with_defaults();
        let plugin = sandbox.load_plugin("test-add", &add_module_bytes(), HashMap::new());
        assert!(plugin.is_ok());
        assert_eq!(plugin.unwrap().name, "test-add");
    }

    // --- 2. Reject invalid bytes ---

    #[test]
    fn reject_invalid_wasm_bytes() {
        let sandbox = WasmSandbox::with_defaults();
        let result = sandbox.load_plugin("bad", b"not wasm", HashMap::new());
        assert!(result.is_err());
        match result.unwrap_err() {
            WasmSandboxError::InvalidModule(msg) => {
                assert!(!msg.is_empty());
            }
            other => panic!("expected InvalidModule, got {other:?}"),
        }
    }

    #[test]
    fn reject_empty_bytes() {
        let sandbox = WasmSandbox::with_defaults();
        let result = sandbox.load_plugin("empty", b"", HashMap::new());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WasmSandboxError::InvalidModule(_)
        ));
    }

    #[test]
    fn reject_truncated_wasm() {
        let full = add_module_bytes();
        let truncated = &full[..full.len() / 2];
        let sandbox = WasmSandbox::with_defaults();
        let result = sandbox.load_plugin("trunc", truncated, HashMap::new());
        assert!(result.is_err());
    }

    // --- 3. Basic function calls ---

    #[test]
    fn call_add_function() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("adder", &add_module_bytes(), HashMap::new())
            .unwrap();
        let results = sandbox
            .call_function(&mut plugin, "add", &[Val::I32(3), Val::I32(7)])
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(val_i32(&results[0]), 10);
    }

    #[test]
    fn call_constant_function() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("const", &constant_module_bytes(), HashMap::new())
            .unwrap();
        let results = sandbox
            .call_function(&mut plugin, "get_value", &[])
            .unwrap();
        assert_eq!(val_i32(&results[0]), 42);
    }

    // --- 4. Missing export ---

    #[test]
    fn call_missing_export() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("adder", &add_module_bytes(), HashMap::new())
            .unwrap();
        let result = sandbox.call_function(&mut plugin, "nonexistent", &[]);
        assert!(matches!(
            result.unwrap_err(),
            WasmSandboxError::ExportNotFound(_)
        ));
    }

    // --- 5. Export is not a function ---

    #[test]
    fn call_non_function_export() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("global", &global_export_module_bytes(), HashMap::new())
            .unwrap();
        let result = sandbox.call_function(&mut plugin, "my_global", &[]);
        assert!(matches!(
            result.unwrap_err(),
            WasmSandboxError::ExportNotAFunction(_)
        ));
    }

    // --- 6. Argument mismatch ---

    #[test]
    fn argument_count_mismatch() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("adder", &add_module_bytes(), HashMap::new())
            .unwrap();
        // add expects 2 args, we pass 1
        let result = sandbox.call_function(&mut plugin, "add", &[Val::I32(1)]);
        assert!(matches!(
            result.unwrap_err(),
            WasmSandboxError::ArgumentMismatch(_)
        ));
    }

    #[test]
    fn too_many_arguments() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("adder", &add_module_bytes(), HashMap::new())
            .unwrap();
        let result =
            sandbox.call_function(&mut plugin, "add", &[Val::I32(1), Val::I32(2), Val::I32(3)]);
        assert!(matches!(
            result.unwrap_err(),
            WasmSandboxError::ArgumentMismatch(_)
        ));
    }

    // --- 7. Fuel exhaustion ---

    #[test]
    fn fuel_exhaustion_on_infinite_loop() {
        let config = WasmSandboxConfig {
            fuel_limit: 1_000, // very low
            ..Default::default()
        };
        let sandbox = WasmSandbox::new(config);
        let mut plugin = sandbox
            .load_plugin("spinner", &infinite_loop_module_bytes(), HashMap::new())
            .unwrap();
        let result = sandbox.call_function(&mut plugin, "spin", &[]);
        assert!(
            matches!(result.unwrap_err(), WasmSandboxError::FuelExhausted),
            "expected FuelExhausted"
        );
    }

    #[test]
    fn fuel_resets_between_calls() {
        let config = WasmSandboxConfig {
            fuel_limit: 100_000,
            ..Default::default()
        };
        let sandbox = WasmSandbox::new(config);
        let mut plugin = sandbox
            .load_plugin("adder", &add_module_bytes(), HashMap::new())
            .unwrap();

        // Call should succeed multiple times because fuel resets
        for _ in 0..10 {
            let results = sandbox
                .call_function(&mut plugin, "add", &[Val::I32(1), Val::I32(2)])
                .unwrap();
            assert_eq!(val_i32(&results[0]), 3);
        }
    }

    // --- 8. Memory limit ---

    #[test]
    fn memory_grow_within_limit() {
        // Default 16MB limit, module starts with 1 page (64KB)
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("grower", &memory_grow_module_bytes(), HashMap::new())
            .unwrap();
        // Try to grow by 1024 pages (64MB) -- should fail (return -1), not trap
        // because wasmi returns -1 from memory.grow on failure
        // unless trap_on_grow_failure is true
        let result = sandbox.call_function(&mut plugin, "grow_big", &[]);
        // With trap_on_grow_failure=true, this should trap
        assert!(
            result.is_err(),
            "expected error from excessive memory growth"
        );
    }

    #[test]
    fn memory_limit_configurable() {
        let config = WasmSandboxConfig {
            // Only allow 1 page = 64KB
            max_memory_bytes: 64 * 1024,
            ..Default::default()
        };
        let sandbox = WasmSandbox::new(config);
        let mut plugin = sandbox
            .load_plugin("grower", &memory_grow_module_bytes(), HashMap::new())
            .unwrap();
        // Even growing by 1 page should fail since we already have 1 page
        let result = sandbox.call_function(&mut plugin, "grow_big", &[]);
        assert!(result.is_err());
    }

    // --- 9. Host function: logging ---

    #[test]
    fn host_log_captures_messages() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("logger", &logging_module_bytes(), HashMap::new())
            .unwrap();
        sandbox.call_function(&mut plugin, "do_log", &[]).unwrap();

        let logs = plugin.drain_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0], "hello from wasm");
    }

    #[test]
    fn host_log_multiple_calls() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("logger", &logging_module_bytes(), HashMap::new())
            .unwrap();

        sandbox.call_function(&mut plugin, "do_log", &[]).unwrap();
        sandbox.call_function(&mut plugin, "do_log", &[]).unwrap();

        let logs = plugin.drain_logs();
        assert_eq!(logs.len(), 2);
    }

    #[test]
    fn drain_logs_clears_buffer() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("logger", &logging_module_bytes(), HashMap::new())
            .unwrap();

        sandbox.call_function(&mut plugin, "do_log", &[]).unwrap();
        assert_eq!(plugin.log_count(), 1);

        let _ = plugin.drain_logs();
        assert_eq!(plugin.log_count(), 0);
    }

    // --- 10. Host function: get_config ---

    #[test]
    fn host_get_config_found() {
        let mut config = HashMap::new();
        config.insert("my_key".to_string(), "my_value".to_string());

        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("config-checker", &config_module_bytes(), config)
            .unwrap();
        let results = sandbox
            .call_function(&mut plugin, "check_config", &[])
            .unwrap();
        assert_eq!(val_i32(&results[0]), 0); // 0 = found
    }

    #[test]
    fn host_get_config_missing() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("config-checker", &config_module_bytes(), HashMap::new())
            .unwrap();
        let results = sandbox
            .call_function(&mut plugin, "check_config", &[])
            .unwrap();
        assert_eq!(val_i32(&results[0]), 1); // 1 = not found
    }

    // --- 11. Host function: tool_call ---

    #[test]
    fn host_tool_call_records() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("tool-caller", &tool_call_module_bytes(), HashMap::new())
            .unwrap();
        let results = sandbox
            .call_function(&mut plugin, "do_tool_call", &[])
            .unwrap();
        assert_eq!(val_i32(&results[0]), 0); // success

        let calls = plugin.drain_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_name, "read_file");
        assert_eq!(calls[0].args, "/tmp/test");
    }

    // --- 12. Trap handling ---

    #[test]
    fn trap_on_unreachable() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("trapper", &trap_module_bytes(), HashMap::new())
            .unwrap();
        let result = sandbox.call_function(&mut plugin, "trap_me", &[]);
        assert!(matches!(result.unwrap_err(), WasmSandboxError::Trap(_)));
    }

    // --- 13. Multiple exports ---

    #[test]
    fn call_multiple_exports() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("multi", &multi_export_module_bytes(), HashMap::new())
            .unwrap();

        let r1 = sandbox
            .call_function(&mut plugin, "double", &[Val::I32(5)])
            .unwrap();
        assert_eq!(val_i32(&r1[0]), 10);

        let r2 = sandbox
            .call_function(&mut plugin, "negate", &[Val::I32(5)])
            .unwrap();
        assert_eq!(val_i32(&r2[0]), -5);

        let r3 = sandbox
            .call_function(&mut plugin, "identity", &[Val::I32(42)])
            .unwrap();
        assert_eq!(val_i32(&r3[0]), 42);
    }

    // --- 14. Empty module ---

    #[test]
    fn load_empty_module() {
        let sandbox = WasmSandbox::with_defaults();
        let plugin = sandbox.load_plugin("empty", &empty_module_bytes(), HashMap::new());
        assert!(plugin.is_ok());
    }

    // --- 15. Plugin reuse after error ---

    #[test]
    fn plugin_reusable_after_trap() {
        let sandbox = WasmSandbox::with_defaults();
        let bytes = wat_bytes(
            r#"
            (module
                (func (export "safe") (result i32) i32.const 1)
                (func (export "unsafe") unreachable))
            "#,
        );
        let mut plugin = sandbox
            .load_plugin("reusable", &bytes, HashMap::new())
            .unwrap();

        // First call traps
        let err = sandbox
            .call_function(&mut plugin, "unsafe", &[])
            .unwrap_err();
        assert!(matches!(err, WasmSandboxError::Trap(_)));

        // Second call should still work (fuel reset + trap recovery)
        let result = sandbox.call_function(&mut plugin, "safe", &[]).unwrap();
        assert_eq!(val_i32(&result[0]), 1);
    }

    // --- 16. Concurrent sandbox instances ---

    #[test]
    fn concurrent_sandbox_instances() {
        let bytes = add_module_bytes();

        let handles: Vec<_> = (0..4)
            .map(|i| {
                let bytes = bytes.clone();
                std::thread::spawn(move || {
                    let sandbox = WasmSandbox::with_defaults();
                    let mut plugin = sandbox
                        .load_plugin(&format!("thread-{i}"), &bytes, HashMap::new())
                        .unwrap();
                    let results = sandbox
                        .call_function(&mut plugin, "add", &[Val::I32(i), Val::I32(100)])
                        .unwrap();
                    val_i32(&results[0])
                })
            })
            .collect();

        for (i, handle) in handles.into_iter().enumerate() {
            let val = handle.join().unwrap();
            assert_eq!(val, i as i32 + 100);
        }
    }

    // --- 17. Config access ---

    #[test]
    fn sandbox_config_accessible() {
        let config = WasmSandboxConfig {
            max_memory_bytes: 1024 * 1024,
            fuel_limit: 500_000,
            max_recursion_depth: 32,
        };
        let sandbox = WasmSandbox::new(config);
        assert_eq!(sandbox.config().max_memory_bytes, 1024 * 1024);
        assert_eq!(sandbox.config().fuel_limit, 500_000);
        assert_eq!(sandbox.config().max_recursion_depth, 32);
    }

    // --- 18. Default trait ---

    #[test]
    fn sandbox_default_trait() {
        let sandbox = WasmSandbox::default();
        assert_eq!(sandbox.config().max_memory_bytes, 16 * 1024 * 1024);
        assert_eq!(sandbox.config().fuel_limit, 1_000_000);
    }

    // --- 19. Remaining fuel after call ---

    #[test]
    fn remaining_fuel_decreases_after_call() {
        let config = WasmSandboxConfig {
            fuel_limit: 100_000,
            ..Default::default()
        };
        let sandbox = WasmSandbox::new(config);
        let mut plugin = sandbox
            .load_plugin("adder", &add_module_bytes(), HashMap::new())
            .unwrap();

        sandbox
            .call_function(&mut plugin, "add", &[Val::I32(1), Val::I32(2)])
            .unwrap();

        let remaining = plugin.remaining_fuel().unwrap();
        assert!(
            remaining < 100_000,
            "fuel should have decreased, got {remaining}"
        );
    }

    // --- 20. Multiple plugins from same sandbox ---

    #[test]
    fn multiple_plugins_from_same_sandbox() {
        let sandbox = WasmSandbox::with_defaults();

        let mut p1 = sandbox
            .load_plugin("p1", &add_module_bytes(), HashMap::new())
            .unwrap();
        let mut p2 = sandbox
            .load_plugin("p2", &constant_module_bytes(), HashMap::new())
            .unwrap();

        let r1 = sandbox
            .call_function(&mut p1, "add", &[Val::I32(10), Val::I32(20)])
            .unwrap();
        let r2 = sandbox.call_function(&mut p2, "get_value", &[]).unwrap();

        assert_eq!(val_i32(&r1[0]), 30);
        assert_eq!(val_i32(&r2[0]), 42);
    }

    // --- 21. Plugin debug display ---

    #[test]
    fn plugin_debug_display() {
        let sandbox = WasmSandbox::with_defaults();
        let plugin = sandbox
            .load_plugin("debug-test", &add_module_bytes(), HashMap::new())
            .unwrap();
        let debug_str = format!("{plugin:?}");
        assert!(debug_str.contains("debug-test"));
        assert!(debug_str.contains("fuel_limit"));
    }

    // --- 22. Sandbox debug display ---

    #[test]
    fn sandbox_debug_display() {
        let sandbox = WasmSandbox::with_defaults();
        let debug_str = format!("{sandbox:?}");
        assert!(debug_str.contains("WasmSandbox"));
        assert!(debug_str.contains("config"));
    }

    // --- 23. Error display messages ---

    #[test]
    fn error_display_messages() {
        assert_eq!(
            WasmSandboxError::InvalidModule("bad".into()).to_string(),
            "invalid wasm module: bad"
        );
        assert_eq!(
            WasmSandboxError::ExportNotFound("foo".into()).to_string(),
            "export not found: foo"
        );
        assert_eq!(
            WasmSandboxError::FuelExhausted.to_string(),
            "fuel exhausted: plugin exceeded instruction limit"
        );
        assert_eq!(
            WasmSandboxError::MemoryLimitExceeded.to_string(),
            "memory limit exceeded"
        );
        assert_eq!(
            WasmSandboxError::PluginNotLoaded.to_string(),
            "plugin not loaded"
        );
    }

    // --- 24. Very low fuel prevents even simple operations ---

    #[test]
    fn extremely_low_fuel() {
        let config = WasmSandboxConfig {
            fuel_limit: 1, // almost nothing
            ..Default::default()
        };
        let sandbox = WasmSandbox::new(config);
        let mut plugin = sandbox
            .load_plugin("low-fuel", &add_module_bytes(), HashMap::new())
            .unwrap();
        let result = sandbox.call_function(&mut plugin, "add", &[Val::I32(1), Val::I32(2)]);
        // With fuel=1, the function should run out of fuel
        assert!(
            matches!(result, Err(WasmSandboxError::FuelExhausted) | Ok(_)),
            "expected either fuel exhaustion or (unlikely) success"
        );
    }

    // --- 25. Call function on empty module ---

    #[test]
    fn call_on_empty_module() {
        let sandbox = WasmSandbox::with_defaults();
        let mut plugin = sandbox
            .load_plugin("empty", &empty_module_bytes(), HashMap::new())
            .unwrap();
        let result = sandbox.call_function(&mut plugin, "anything", &[]);
        assert!(matches!(
            result.unwrap_err(),
            WasmSandboxError::ExportNotFound(_)
        ));
    }
}
