// Hook system (Stream 8.2 + B2 expansion).
//
// Layout:
// - `types.rs`     -> HookEvent (+ kind), HookAction, HookContext, HookCallback
// - `registry.rs`  -> async `Hook` trait + `HookRegistry` (DashMap-backed)
//
// CP1 added the four B2 event variants. CP2 introduced the async `Hook`
// trait + `HookRegistry` with a sync compatibility shim so existing sync
// closures remain registerable. CP3-CP6 will add subprocess/WASM backends,
// emit sites, and config wiring.

pub mod config;
mod registry;
pub mod subprocess;
mod types;
pub mod wasm_bridge;

#[cfg(test)]
mod emit_tests;

pub use config::{
    discover_and_register, empty_catalog, DiscoveryError, HookCatalog, HookCatalogEntry, HookKind,
    HookManifest,
};
pub use registry::{Hook, HookRegistry};
pub use subprocess::{SubprocessHook, SubprocessHookError};
pub use types::{HookAction, HookCallback, HookContext, HookEvent, HookEventKind};
pub use wasm_bridge::{WasmHook, WasmHookError};
