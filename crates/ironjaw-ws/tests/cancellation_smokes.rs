/// C2 Sub-B: CancellationToken integration smokes.
///
/// Three focused checks that exercise the cancellation contract added in
/// `feat(C2 Sub-B): propagate CancellationToken through all Tool::execute sites`:
///
/// 1. `smoke_normal_completion` — token is never cancelled; a pure-compute
///    stub returns `ToolResult::Text`.
/// 2. `smoke_pre_cancelled` — token is cancelled BEFORE `execute` is called;
///    a cooperative stub detects the cancellation immediately and returns
///    `ToolResult::Cancelled`.
/// 3. `smoke_mid_execution_cancel` — token is cancelled DURING execution;
///    a sleeping stub races `tokio::select!` against the token and returns
///    `ToolResult::Cancelled` before the full sleep completes.
///
/// These tests exercise `ToolRegistry::execute` directly — no WebSocket
/// server is needed, keeping them fast and deterministic.
use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolRegistry, ToolResult};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

fn test_ctx() -> ToolContext {
    ToolContext::new("smoke-session", "smoke-conn", PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// Stub tools
// ---------------------------------------------------------------------------

/// Pure-compute stub that always returns a `Text` result and ignores the token.
struct PureComputeStub;

#[async_trait::async_trait]
impl Tool for PureComputeStub {
    fn name(&self) -> &str {
        "pure_compute_stub"
    }
    fn description(&self) -> &str {
        "Always returns Text without doing async I/O"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        ToolResult::text("pure-compute result")
    }
}

/// Long-running stub that uses `tokio::select!` to race a 10-second sleep
/// against the cancellation token.  Represents a network-bound or I/O-heavy
/// tool that honours cooperative cancellation.
struct LongRunningStub;

#[async_trait::async_trait]
impl Tool for LongRunningStub {
    fn name(&self) -> &str {
        "long_running_stub"
    }
    fn description(&self) -> &str {
        "Sleeps for 10 s unless cancelled"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                ToolResult::text("long-running result (completed)")
            }
            _ = cancel.cancelled() => {
                ToolResult::Cancelled { reason: "cancelled by token".to_string() }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Smoke 1: Normal completion — token never cancelled
// ---------------------------------------------------------------------------

#[tokio::test]
async fn smoke_normal_completion() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(PureComputeStub));

    let cancel = CancellationToken::new();
    let result = registry
        .execute(
            "pure_compute_stub",
            serde_json::json!({}),
            &test_ctx(),
            cancel,
        )
        .await;

    match result {
        ToolResult::Text { content } => {
            assert_eq!(content, "pure-compute result");
        }
        other => panic!("expected ToolResult::Text, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Smoke 2: Pre-cancelled — token fired before execute returns
// ---------------------------------------------------------------------------

#[tokio::test]
async fn smoke_pre_cancelled() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(LongRunningStub));

    let cancel = CancellationToken::new();
    // Cancel the token BEFORE calling execute.
    cancel.cancel();

    let result = registry
        .execute(
            "long_running_stub",
            serde_json::json!({}),
            &test_ctx(),
            cancel,
        )
        .await;

    match result {
        ToolResult::Cancelled { reason } => {
            assert_eq!(reason, "cancelled by token");
        }
        other => panic!("expected ToolResult::Cancelled, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Smoke 3: Mid-execution cancel — token fired while execute is running
// ---------------------------------------------------------------------------

#[tokio::test]
async fn smoke_mid_execution_cancel() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(LongRunningStub));

    let cancel = CancellationToken::new();
    // Clone the token so we can cancel it from a background task while
    // execute is running its 10-second sleep.
    let cancel_clone = cancel.clone();

    // Fire the token after 50 ms — well before the 10-second sleep would
    // complete, but after execute has started.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel_clone.cancel();
    });

    let result = registry
        .execute(
            "long_running_stub",
            serde_json::json!({}),
            &test_ctx(),
            cancel,
        )
        .await;

    match result {
        ToolResult::Cancelled { reason } => {
            assert_eq!(reason, "cancelled by token");
        }
        other => panic!("expected ToolResult::Cancelled, got: {other:?}"),
    }
}
