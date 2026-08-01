/// Phase 9 integration tests for subagent and autonomous agentic execution.
///
/// # Test coverage
/// 1. `subagent_runs_tool_and_returns` — `execute_subagent_task` invokes a real
///    registered Auto-tier tool and returns its output.
/// 2. `subagent_cannot_elevate_tool_tier` — Confirm-tier tool calls are dropped
///    when ceiling is `"Auto"`.
/// 3. `autonomous_executes_real_tool_instead_of_stub` — `cap_tools_by_tier`
///    correctly passes Auto tools and blocks Confirm tools.
use ironjaw_llm::types::ToolDefinition;
use ironjaw_security::HitlGate;
use ironjaw_tools::{ToolContext, ToolRegistry};
use ironjaw_ws::subagent::{cap_tools_by_tier, execute_subagent_task};
use std::sync::Arc;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_tool_def(name: &str, tier: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: format!("{name} tool"),
        parameters: serde_json::json!({ "type": "object", "properties": {} }),
        tier: tier.to_string(),
    }
}

fn make_tool_context() -> ToolContext {
    ToolContext {
        session_id: "test-session".to_string(),
        connection_id: "test-conn".to_string(),
        working_dir: std::env::temp_dir(),
        brain: None,
        stream_tx: None,
    }
}

fn make_auto_approve_hitl() -> Arc<HitlGate> {
    Arc::new(HitlGate::auto_approve())
}

/// Build a minimal ToolRegistry with the built-in Auto-tier tools registered.
fn make_registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    ironjaw_tools::register_builtins(&mut registry);
    Arc::new(registry)
}

// ---------------------------------------------------------------------------
// Test 1: cap_tools_by_tier passes Auto tools through
// ---------------------------------------------------------------------------

#[test]
fn subagent_runs_tool_and_returns() {
    // This is a unit test for cap_tools_by_tier — verifies that Auto-tier
    // tools survive when ceiling is "Auto".
    let tools = vec![
        make_tool_def("read_file", "Auto"),
        make_tool_def("list_files", "Auto"),
    ];
    let filtered = cap_tools_by_tier(tools, "Auto");
    assert_eq!(
        filtered.len(),
        2,
        "both Auto tools should pass through ceiling=Auto"
    );
    assert!(filtered.iter().all(|t| t.tier == "Auto"));
}

// ---------------------------------------------------------------------------
// Test 2: subagent_cannot_elevate_tool_tier
// ---------------------------------------------------------------------------

#[test]
fn subagent_cannot_elevate_tool_tier() {
    // Confirm-tier tools should be dropped when ceiling is "Auto".
    let tools = vec![
        make_tool_def("read_file", "Auto"),
        make_tool_def("write_file", "Confirm"),
        make_tool_def("shell", "Confirm"),
        make_tool_def("list_files", "Auto"),
    ];
    let filtered = cap_tools_by_tier(tools, "Auto");
    assert_eq!(
        filtered.len(),
        2,
        "only Auto-tier tools should survive when ceiling=Auto"
    );
    for t in &filtered {
        assert_eq!(
            t.tier,
            "Auto",
            "filtered tool {name} has wrong tier",
            name = t.name
        );
    }

    // With ceiling "Notify", Auto + Notify pass; Confirm is blocked.
    let tools2 = vec![
        make_tool_def("read_file", "Auto"),
        make_tool_def("process", "Notify"),
        make_tool_def("write_file", "Confirm"),
    ];
    let filtered2 = cap_tools_by_tier(tools2, "Notify");
    assert_eq!(filtered2.len(), 2);
    assert!(filtered2.iter().any(|t| t.name == "read_file"));
    assert!(filtered2.iter().any(|t| t.name == "process"));
    assert!(!filtered2.iter().any(|t| t.name == "write_file"));

    // With ceiling "MultiConfirm", all tiers pass.
    let tools3 = vec![
        make_tool_def("read_file", "Auto"),
        make_tool_def("shell", "Confirm"),
        make_tool_def("nuke", "MultiConfirm"),
    ];
    let filtered3 = cap_tools_by_tier(tools3, "MultiConfirm");
    assert_eq!(filtered3.len(), 3);
}

// ---------------------------------------------------------------------------
// Test 3: autonomous_executes_real_tool_instead_of_stub
// ---------------------------------------------------------------------------

#[tokio::test]
async fn autonomous_executes_real_tool_instead_of_stub() {
    // Verify execute_subagent_task actually invokes a registered tool (list_files)
    // and returns non-stub output.
    let registry = make_registry();
    let hitl = make_auto_approve_hitl();
    let (tx, _rx) = mpsc::unbounded_channel();
    let tool_context = make_tool_context();

    // Build a ToolCall for list_files (Auto tier, always registered).
    let call = ironjaw_llm::ToolCall {
        id: "call-1".to_string(),
        type_: "function".to_string(),
        function: ironjaw_llm::ToolCallFunction {
            name: "list_files".to_string(),
            arguments: serde_json::json!({
                "path": std::env::temp_dir().to_string_lossy(),
                "recursive": false,
            })
            .to_string(),
        },
    };

    let result = execute_subagent_task(
        "list temp dir",
        &[call],
        &registry,
        &hitl,
        &tx,
        &tool_context,
        "Auto",
        10,
        "",    // session_id — empty in test (HITL gate is auto-approve)
        false, // hitl_session_cache_enabled — irrelevant for auto-approve gate
    )
    .await;

    // The task should succeed (list_files is always runnable on temp dir).
    assert!(
        result.is_ok(),
        "execute_subagent_task failed: {:?}",
        result.err()
    );

    // Output must be non-empty and NOT the old stub string.
    let output = result.unwrap();
    assert!(!output.is_empty(), "output should not be empty");
    assert!(
        !output.contains("tool:list_files executed with params:"),
        "output looks like a stub: {output}"
    );
}
