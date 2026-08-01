//! Parallel and sequential tool execution infrastructure.
//!
//! Provides `ParallelExecutor` which can run multiple tool calls concurrently
//! (for Auto-tier tools) or sequentially (for Confirm-tier or mixed tiers).

use crate::registry::ToolRegistry;
use crate::types::ToolContext;
use ironjaw_security::PermissionTier;
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// A pending tool call to execute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool name (must match a registered tool)
    pub name: String,
    /// Parameters to pass to the tool
    pub params: serde_json::Value,
    /// Unique identifier for this call (correlates request to response)
    pub call_id: String,
}

/// Result of a single tool call execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    /// Correlating call ID from the request
    pub call_id: String,
    /// The tool result (Ok = JSON value, Err = error message)
    pub result: Result<serde_json::Value, String>,
    /// Execution duration in milliseconds
    pub duration_ms: u64,
}

/// Executes tool calls in parallel or sequentially depending on permission tier.
pub struct ParallelExecutor;

impl ParallelExecutor {
    /// Execute multiple tool calls in parallel (for Auto-tier tools only).
    ///
    /// Returns results in the same order as input calls.
    pub async fn execute_parallel(
        registry: &ToolRegistry,
        calls: Vec<ToolCall>,
        ctx: &ToolContext,
    ) -> Vec<ToolCallResult> {
        let futures: Vec<_> = calls
            .into_iter()
            .map(|call| {
                let ctx = ctx.clone();
                async move { Self::execute_one(registry, call, &ctx).await }
            })
            .collect();

        futures_util::future::join_all(futures).await
    }

    /// Execute tool calls sequentially (for Confirm-tier or mixed tiers).
    ///
    /// Returns results in the same order as input calls.
    pub async fn execute_sequential(
        registry: &ToolRegistry,
        calls: Vec<ToolCall>,
        ctx: &ToolContext,
    ) -> Vec<ToolCallResult> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            results.push(Self::execute_one(registry, call, ctx).await);
        }
        results
    }

    /// Smart execution: runs Auto-tier calls in parallel, others sequentially.
    ///
    /// Separates calls into Auto-tier (parallel batch) and non-Auto (sequential).
    /// Auto-tier calls run first in parallel, then non-Auto calls run sequentially.
    /// Results are returned in the original input order.
    pub async fn execute_smart(
        registry: &ToolRegistry,
        calls: Vec<ToolCall>,
        ctx: &ToolContext,
        tier_lookup: impl Fn(&str) -> PermissionTier,
    ) -> Vec<ToolCallResult> {
        // Tag each call with its original index and tier
        let indexed: Vec<(usize, ToolCall, bool)> = calls
            .into_iter()
            .enumerate()
            .map(|(i, call)| {
                let is_auto = tier_lookup(&call.name) == PermissionTier::Auto;
                (i, call, is_auto)
            })
            .collect();

        let total = indexed.len();
        let mut results: Vec<Option<ToolCallResult>> = vec![None; total];

        // Collect auto-tier calls for parallel execution
        let (auto_calls, sequential_calls): (Vec<_>, Vec<_>) =
            indexed.into_iter().partition(|(_, _, is_auto)| *is_auto);

        // Run auto-tier calls in parallel
        if !auto_calls.is_empty() {
            let auto_indices: Vec<usize> = auto_calls.iter().map(|(i, _, _)| *i).collect();
            let calls_only: Vec<ToolCall> =
                auto_calls.into_iter().map(|(_, call, _)| call).collect();

            debug!(
                count = calls_only.len(),
                "Executing auto-tier calls in parallel"
            );
            let auto_results = Self::execute_parallel(registry, calls_only, ctx).await;

            for (idx, result) in auto_indices.into_iter().zip(auto_results) {
                results[idx] = Some(result);
            }
        }

        // Run non-auto calls sequentially
        for (idx, call, _) in sequential_calls {
            debug!(tool = %call.name, "Executing non-auto call sequentially");
            let result = Self::execute_one(registry, call, ctx).await;
            results[idx] = Some(result);
        }

        // Unwrap all (all slots must be filled)
        results.into_iter().map(|r| r.unwrap()).collect()
    }

    /// Execute a single tool call, measuring duration.
    async fn execute_one(
        registry: &ToolRegistry,
        call: ToolCall,
        ctx: &ToolContext,
    ) -> ToolCallResult {
        let start = Instant::now();

        let tool_result = registry
            .execute(&call.name, call.params, ctx, CancellationToken::new())
            .await;

        let duration_ms = start.elapsed().as_millis() as u64;

        let result = match tool_result {
            crate::types::ToolResult::Text { content } => Ok(serde_json::Value::String(content)),
            crate::types::ToolResult::Json { content } => Ok(content),
            crate::types::ToolResult::Error { code, message } => Err(format!("{code}: {message}")),
            crate::types::ToolResult::Cancelled { reason } => Err(format!("CANCELLED: {reason}")),
        };

        ToolCallResult {
            call_id: call.call_id,
            result,
            duration_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Tool, ToolResult};
    use std::path::PathBuf;

    /// Mock tool that returns its name as the result.
    struct EchoTool {
        tool_name: String,
        delay_ms: u64,
    }

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            &self.tool_name
        }
        fn description(&self) -> &str {
            "Echo tool for testing"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn tier(&self) -> PermissionTier {
            PermissionTier::Auto
        }
        async fn execute(
            &self,
            params: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            ToolResult::json(serde_json::json!({
                "tool": self.tool_name,
                "echo": params,
            }))
        }
    }

    struct FailTool;

    #[async_trait::async_trait]
    impl Tool for FailTool {
        fn name(&self) -> &str {
            "fail_tool"
        }
        fn description(&self) -> &str {
            "Always fails"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn tier(&self) -> PermissionTier {
            PermissionTier::Confirm
        }
        async fn execute(
            &self,
            _params: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            ToolResult::error("FAIL", "intentional failure")
        }
    }

    fn test_ctx() -> ToolContext {
        ToolContext::new("test-session", "test-conn", PathBuf::from("/tmp"))
    }

    fn make_registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(EchoTool {
            tool_name: "tool_a".to_string(),
            delay_ms: 0,
        }));
        reg.register(Box::new(EchoTool {
            tool_name: "tool_b".to_string(),
            delay_ms: 0,
        }));
        reg.register(Box::new(EchoTool {
            tool_name: "tool_c".to_string(),
            delay_ms: 0,
        }));
        reg.register(Box::new(FailTool));
        reg
    }

    fn make_call(name: &str, id: &str) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            params: serde_json::json!({"input": id}),
            call_id: id.to_string(),
        }
    }

    #[tokio::test]
    async fn test_execute_parallel_basic() {
        let reg = make_registry();
        let ctx = test_ctx();
        let calls = vec![
            make_call("tool_a", "1"),
            make_call("tool_b", "2"),
            make_call("tool_c", "3"),
        ];

        let results = ParallelExecutor::execute_parallel(&reg, calls, &ctx).await;
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].call_id, "1");
        assert_eq!(results[1].call_id, "2");
        assert_eq!(results[2].call_id, "3");
        assert!(results[0].result.is_ok());
        assert!(results[1].result.is_ok());
        assert!(results[2].result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_sequential_basic() {
        let reg = make_registry();
        let ctx = test_ctx();
        let calls = vec![make_call("tool_a", "s1"), make_call("tool_b", "s2")];

        let results = ParallelExecutor::execute_sequential(&reg, calls, &ctx).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].call_id, "s1");
        assert_eq!(results[1].call_id, "s2");
    }

    #[tokio::test]
    async fn test_execute_parallel_with_failure() {
        let reg = make_registry();
        let ctx = test_ctx();
        let calls = vec![make_call("tool_a", "ok"), make_call("fail_tool", "bad")];

        let results = ParallelExecutor::execute_parallel(&reg, calls, &ctx).await;
        assert!(results[0].result.is_ok());
        assert!(results[1].result.is_err());
    }

    #[tokio::test]
    async fn test_execute_parallel_unknown_tool() {
        let reg = make_registry();
        let ctx = test_ctx();
        let calls = vec![make_call("nonexistent", "missing")];

        let results = ParallelExecutor::execute_parallel(&reg, calls, &ctx).await;
        assert!(results[0].result.is_err());
        let err = results[0].result.as_ref().unwrap_err();
        assert!(err.contains("TOOL_NOT_FOUND"));
    }

    #[tokio::test]
    async fn test_execute_smart_separates_tiers() {
        let reg = make_registry();
        let ctx = test_ctx();

        // tool_a and tool_b are Auto, fail_tool is Confirm
        let calls = vec![
            make_call("tool_a", "auto1"),
            make_call("fail_tool", "confirm1"),
            make_call("tool_b", "auto2"),
        ];

        let results = ParallelExecutor::execute_smart(&reg, calls, &ctx, |name| {
            if name == "fail_tool" {
                PermissionTier::Confirm
            } else {
                PermissionTier::Auto
            }
        })
        .await;

        // Results should be in original order
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].call_id, "auto1");
        assert!(results[0].result.is_ok());
        assert_eq!(results[1].call_id, "confirm1");
        assert!(results[1].result.is_err()); // fail_tool always fails
        assert_eq!(results[2].call_id, "auto2");
        assert!(results[2].result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_smart_all_auto() {
        let reg = make_registry();
        let ctx = test_ctx();
        let calls = vec![make_call("tool_a", "a"), make_call("tool_b", "b")];

        let results =
            ParallelExecutor::execute_smart(&reg, calls, &ctx, |_| PermissionTier::Auto).await;

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.result.is_ok()));
    }

    #[tokio::test]
    async fn test_execute_empty() {
        let reg = make_registry();
        let ctx = test_ctx();
        let results = ParallelExecutor::execute_parallel(&reg, vec![], &ctx).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_duration_tracked() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(EchoTool {
            tool_name: "slow".to_string(),
            delay_ms: 50,
        }));

        let ctx = test_ctx();
        let calls = vec![make_call("slow", "timed")];
        let results = ParallelExecutor::execute_sequential(&reg, calls, &ctx).await;

        assert!(results[0].duration_ms >= 40); // allow some slack
    }

    #[tokio::test]
    async fn test_result_preserves_call_id() {
        let reg = make_registry();
        let ctx = test_ctx();
        let calls = vec![make_call("tool_a", "unique-id-abc-123")];

        let results = ParallelExecutor::execute_parallel(&reg, calls, &ctx).await;
        assert_eq!(results[0].call_id, "unique-id-abc-123");
    }
}
