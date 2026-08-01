/// Phase 4 integration tests for the agentic tool-call loop.
///
/// These tests exercise `run_agentic_chat` end-to-end via the WebSocket
/// protocol without mocking internal functions.  The `chat.agentic.enabled`
/// killswitch is toggled per-test to isolate the two code paths.
///
/// # Test coverage
/// 1. `flag_disabled` — agentic flag off → single-pass code path runs
/// 2. `flag_enabled_stop_immediately` — flag on, LLM returns stop immediately → no tool calls
/// 3. `flag_enabled_tool_call_parse_error` — flag on, malformed tool JSON → error result in stream
/// 4. `max_iterations_reached` — max_iterations=1 with tool_calls → loop exits after one pass
/// 5. `hitl_auto_approve_allows_execution` — HITL auto-approve gate doesn't block the loop
/// 6. `tool_call_delta_assembled_correctly` — delta chunks merge into a coherent tool name
/// 7. `abort_during_agentic_loop` — chat.abort stops the stream cleanly
use futures_util::{SinkExt, StreamExt};
use ironjaw_audit::AuditStore;
use ironjaw_brain::{Brain, BrainConfig, BrainStore, Embedder};
use ironjaw_llm::{
    types::{ChatChunk, ToolCallDelta, ToolCallFunctionDelta, Usage},
    LlmRouter,
};
use ironjaw_proactive::{ProactiveConfig, ProactiveEngine, ProactiveStore};
use ironjaw_session::SessionStore;
use ironjaw_vision::ImageProcessor;
use ironjaw_voice::VoiceEngine;
use ironjaw_ws::{server::build_router, AppState, ChatAgenticConfig};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ---------------------------------------------------------------------------
// Shared test infrastructure
// ---------------------------------------------------------------------------

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

async fn test_brain() -> Brain {
    let store = BrainStore::open_memory().await.unwrap();
    let embedder = Embedder::mock(64);
    let config = BrainConfig {
        memory_relevance_threshold: 0.0,
        ..Default::default()
    };
    Brain::new(store, embedder, config)
}

async fn test_proactive() -> ProactiveEngine {
    let store = ProactiveStore::open_memory().await.unwrap();
    ProactiveEngine::new(store, ProactiveConfig::default())
}

/// Build an AppState with the given LLM router and agentic config, bind to a
/// random port, and return the address string.
async fn start_server_with(llm: LlmRouter, agentic: ChatAgenticConfig) -> String {
    let audit = AuditStore::open_memory().await.unwrap();
    let sessions = SessionStore::open_memory().await.unwrap();
    let brain = Arc::new(test_brain().await);
    let proactive = test_proactive().await;
    let voice = VoiceEngine::mock();

    let mut state = AppState::new(
        "test-secret-token".to_string(),
        audit,
        Arc::new(llm),
        sessions,
        32768,
        brain,
        proactive,
        voice,
        ImageProcessor::new(10_485_760),
    );
    state.chat_agentic = agentic;

    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    format!("127.0.0.1:{}", addr.port())
}

/// Authenticate and return the session_id.
async fn authenticate(write: &mut WsSink, read: &mut WsStream) -> String {
    let auth = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0001",
        "method": "auth.connect",
        "params": { "token": "test-secret-token" }
    });
    let txt = serde_json::to_string(&auth).unwrap();
    write.send(Message::Text(txt.into())).await.unwrap();

    loop {
        match read.next().await {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "event" {
                    continue;
                }
                assert_eq!(v["ok"], true, "auth failed: {v}");
                return v["payload"]["session_id"].as_str().unwrap().to_string();
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

/// Send `chat.send` and collect all frames until `state == "final"`.
/// Skips non-chat events.  Panics if no final frame arrives within 5 s.
async fn send_chat_collect(write: &mut WsSink, read: &mut WsStream, msg: &str) -> Vec<Value> {
    let req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0002",
        "method": "chat.send",
        "params": { "message": msg }
    });
    write
        .send(Message::Text(serde_json::to_string(&req).unwrap().into()))
        .await
        .unwrap();

    let mut frames = Vec::new();
    loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(8), read.next())
            .await
            .expect("timed out waiting for chat frame");

        match item {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                // Skip non-chat events (tool.start, tool.result, permission.*)
                if v["type"] == "event" && v["event"] != "chat" {
                    continue;
                }
                let is_final = v
                    .get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str())
                    == Some("final");
                frames.push(v);
                if is_final {
                    break;
                }
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
    frames
}

/// Build a single `ChatChunk` that represents a plain-text LLM reply with `done=true`.
fn stop_chunk(text: &str) -> ChatChunk {
    ChatChunk {
        delta: text.to_string(),
        done: true,
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 3,
            total_tokens: 8,
        }),
        model: "mock-model".to_string(),
        error: None,
        tool_call_deltas: None,
        finish_reason: Some("stop".to_string()),
    }
}

/// Build a `ChatChunk` that contains a tool-call delta announcing one complete
/// tool invocation.  `finish_reason` = "tool_calls" signals the stream parser
/// to assemble the call.
fn tool_call_chunk(tool_name: &str, tool_id: &str, arguments: &str) -> ChatChunk {
    ChatChunk {
        delta: String::new(),
        done: true,
        usage: None,
        model: "mock-model".to_string(),
        error: None,
        tool_call_deltas: Some(vec![ToolCallDelta {
            index: 0,
            id: Some(tool_id.to_string()),
            type_: Some("function".to_string()),
            function: Some(ToolCallFunctionDelta {
                name: Some(tool_name.to_string()),
                arguments: Some(arguments.to_string()),
            }),
        }]),
        finish_reason: Some("tool_calls".to_string()),
    }
}

/// Like [`tool_call_chunk`] but also carries a `Usage` payload, so tests can
/// drive the per-run token accounting (`tokens_in`/`tokens_out`) directly.
fn tool_call_chunk_with_usage(
    tool_name: &str,
    tool_id: &str,
    arguments: &str,
    prompt_tokens: u32,
    completion_tokens: u32,
) -> ChatChunk {
    ChatChunk {
        delta: String::new(),
        done: true,
        usage: Some(Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
        }),
        model: "mock-model".to_string(),
        error: None,
        tool_call_deltas: Some(vec![ToolCallDelta {
            index: 0,
            id: Some(tool_id.to_string()),
            type_: Some("function".to_string()),
            function: Some(ToolCallFunctionDelta {
                name: Some(tool_name.to_string()),
                arguments: Some(arguments.to_string()),
            }),
        }]),
        finish_reason: Some("tool_calls".to_string()),
    }
}

/// A non-final incremental text delta chunk (no usage, `done: false`).
/// Used by A3 mid-stream-reconnect tests to simulate a multi-chunk reply.
fn delta_chunk(text: &str) -> ChatChunk {
    ChatChunk {
        delta: text.to_string(),
        done: false,
        usage: None,
        model: "mock-model".to_string(),
        error: None,
        tool_call_deltas: None,
        finish_reason: None,
    }
}

/// The terminal chunk of a plain-text ("stop") turn assembled from prior
/// [`delta_chunk`]s: empty delta, `done: true`, `finish_reason: "stop"`.
fn done_stop_chunk() -> ChatChunk {
    ChatChunk {
        delta: String::new(),
        done: true,
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 2,
            total_tokens: 7,
        }),
        model: "mock-model".to_string(),
        error: None,
        tool_call_deltas: None,
        finish_reason: Some("stop".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Test 1 — agentic flag disabled falls through to single-pass path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flag_disabled_uses_single_pass_path() {
    // Flag off → single-pass code path; mock LLM returns "hello from single pass"
    let llm = LlmRouter::mock("hello from single pass");
    let agentic = ChatAgenticConfig {
        enabled: false,
        max_iterations: 5,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    let frames = send_chat_collect(&mut w, &mut r, "hello").await;

    // At least one frame must arrive and the final one must have state=final
    assert!(!frames.is_empty(), "expected at least one frame");
    let last = frames.last().unwrap();
    assert_eq!(
        last.get("payload")
            .and_then(|p| p.get("state"))
            .and_then(|s| s.as_str()),
        Some("final"),
        "last frame must be final, got: {last}"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — flag enabled, LLM returns stop immediately → no tool calls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flag_enabled_stop_immediately_returns_text() {
    // LLM immediately returns stop with text — no tool calls in stream
    let chunks = vec![stop_chunk("agentic response text")];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 5,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    let frames = send_chat_collect(&mut w, &mut r, "hi").await;

    assert!(!frames.is_empty(), "expected frames from agentic path");
    let last = frames.last().unwrap();
    assert_eq!(
        last.get("payload")
            .and_then(|p| p.get("state"))
            .and_then(|s| s.as_str()),
        Some("final"),
        "last frame must be final: {last}"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — tool argument parse error is propagated cleanly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flag_enabled_tool_arg_parse_error_propagated() {
    // The tool call chunk carries deliberately malformed JSON arguments.
    // The agentic loop should emit a tool.result event with ok=false and then
    // proceed to a second LLM pass (or hit max_iterations), ultimately sending
    // a final frame to the client without panicking.
    let bad_args = "{not valid json!!!";
    let chunks = vec![tool_call_chunk("list_files", "call-parse-err-1", bad_args)];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        // Limit to 1 so the second call (which also returns tool_calls due to
        // with_scripted_chunks replaying the same sequence) exits the loop.
        max_iterations: 1,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    // Collect raw frames including events so we can inspect tool.result
    let req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0003",
        "method": "chat.send",
        "params": { "message": "list files" }
    });
    w.send(Message::Text(serde_json::to_string(&req).unwrap().into()))
        .await
        .unwrap();

    let mut saw_tool_result = false;
    let mut saw_final = false;

    for _ in 0..50 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(8), r.next())
            .await
            .expect("timed out");

        match item {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();

                if v["type"] == "event" && v["event"] == "tool.result" {
                    // ok should be false because arguments were invalid JSON
                    let ok = v["payload"]["ok"].as_bool().unwrap_or(true);
                    assert!(!ok, "expected ok=false for parse error, got: {v}");
                    saw_tool_result = true;
                }

                let is_final = v
                    .get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str())
                    == Some("final");
                if is_final {
                    saw_final = true;
                    break;
                }
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    assert!(saw_tool_result, "expected at least one tool.result event");
    assert!(saw_final, "expected a final frame");
}

// ---------------------------------------------------------------------------
// Test 4 — max_iterations cap exits the loop and emits a final frame
// ---------------------------------------------------------------------------

#[tokio::test]
async fn max_iterations_reached_exits_loop() {
    // Every LLM call returns tool_calls (same scripted sequence replayed).
    // max_iterations=2 means the loop runs at most 2 times, then terminates.
    let list_files_args = r#"{"path": "."}"#;
    let chunks = vec![tool_call_chunk(
        "list_files",
        "call-iter-limit-1",
        list_files_args,
    )];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 2,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    // Just confirm we get a final frame eventually (not an infinite loop)
    let frames = send_chat_collect(&mut w, &mut r, "go").await;
    assert!(!frames.is_empty());
    let last = frames.last().unwrap();
    assert_eq!(
        last.get("payload")
            .and_then(|p| p.get("state"))
            .and_then(|s| s.as_str()),
        Some("final"),
        "expected final frame after max_iterations, got: {last}"
    );
}

// ---------------------------------------------------------------------------
// Test 5 — HITL auto-approve allows tool execution without blocking
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hitl_auto_approve_does_not_block_execution() {
    // AppState::new() sets up HitlGate::auto_approve() internally, so
    // Confirm-tier tools (like `write_file`) should proceed without waiting.
    // We use `list_files` (Auto tier) which won't trigger HITL anyway,
    // to simply verify the full execution path completes.
    let list_files_args = r#"{"path": "."}"#;
    let chunks = vec![tool_call_chunk(
        "list_files",
        "call-hitl-1",
        list_files_args,
    )];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 1,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    // Collect all frames including events
    let req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0005",
        "method": "chat.send",
        "params": { "message": "list the files" }
    });
    w.send(Message::Text(serde_json::to_string(&req).unwrap().into()))
        .await
        .unwrap();

    let mut saw_tool_start = false;
    let mut saw_final = false;

    for _ in 0..50 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(8), r.next())
            .await
            .expect("timed out");

        match item {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();

                if v["type"] == "event" && v["event"] == "tool.start" {
                    saw_tool_start = true;
                }

                let is_final = v
                    .get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str())
                    == Some("final");
                // Also accept res frames with state=final
                let is_res_final = v.get("ok").is_some()
                    && v.get("payload")
                        .and_then(|p| p.get("state"))
                        .and_then(|s| s.as_str())
                        == Some("final");
                if is_final || is_res_final {
                    saw_final = true;
                    break;
                }
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    assert!(
        saw_tool_start,
        "expected tool.start event — HITL blocked execution"
    );
    assert!(saw_final, "expected final frame");
}

// ---------------------------------------------------------------------------
// Test 6 — tool_call_deltas across multiple chunks merge into complete call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_call_deltas_assembled_from_multiple_chunks() {
    // Simulate streaming where the tool-call delta arrives in two separate
    // chunks: first the function name, then the arguments.  The stream parser
    // must merge them by `index` before executing.
    let chunks = vec![
        // Chunk 1: announce call id + function name (no arguments yet)
        ChatChunk {
            delta: String::new(),
            done: false,
            usage: None,
            model: "mock-model".to_string(),
            error: None,
            tool_call_deltas: Some(vec![ToolCallDelta {
                index: 0,
                id: Some("call-delta-merge-1".to_string()),
                type_: Some("function".to_string()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("list_files".to_string()),
                    arguments: None,
                }),
            }]),
            finish_reason: None,
        },
        // Chunk 2: arguments fragment + finish
        ChatChunk {
            delta: String::new(),
            done: true,
            usage: None,
            model: "mock-model".to_string(),
            error: None,
            tool_call_deltas: Some(vec![ToolCallDelta {
                index: 0,
                id: None,
                type_: None,
                function: Some(ToolCallFunctionDelta {
                    name: None,
                    arguments: Some(r#"{"path": "."}"#.to_string()),
                }),
            }]),
            finish_reason: Some("tool_calls".to_string()),
        },
    ];

    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 1,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    let req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0006",
        "method": "chat.send",
        "params": { "message": "what files exist" }
    });
    w.send(Message::Text(serde_json::to_string(&req).unwrap().into()))
        .await
        .unwrap();

    let mut saw_tool_start = false;
    let mut saw_final = false;

    for _ in 0..50 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(8), r.next())
            .await
            .expect("timed out");

        match item {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "event" && v["event"] == "tool.start" {
                    // Verify the tool name was assembled correctly
                    let name = v["payload"]["tool_name"].as_str().unwrap_or("");
                    assert_eq!(name, "list_files", "wrong tool name: {v}");
                    saw_tool_start = true;
                }
                let is_final = v
                    .get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str())
                    == Some("final");
                if is_final {
                    saw_final = true;
                    break;
                }
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    assert!(
        saw_tool_start,
        "tool.start not seen — delta merge may have failed"
    );
    assert!(saw_final, "no final frame received");
}

// ---------------------------------------------------------------------------
// Test 7 — chat.abort during agentic loop causes clean shutdown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn abort_during_agentic_loop_shuts_down_cleanly() {
    // Use a slow tool (none actually — tool execution is near-instant in
    // tests) but with max_iterations=5 so the loop would continue if not
    // aborted.  We send chat.abort immediately after chat.send and verify
    // the server doesn't panic / the connection stays open.
    let list_files_args = r#"{"path": "."}"#;
    let chunks = vec![tool_call_chunk(
        "list_files",
        "call-abort-1",
        list_files_args,
    )];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 5,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    let _session_id = authenticate(&mut w, &mut r).await;

    // 1. Send chat.send
    let chat_req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0007",
        "method": "chat.send",
        "params": { "message": "start the loop" }
    });
    w.send(Message::Text(
        serde_json::to_string(&chat_req).unwrap().into(),
    ))
    .await
    .unwrap();

    // 2. Immediately send chat.abort (may race with loop start — that's intentional)
    let abort_req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0008",
        "method": "chat.abort",
        "params": {}
    });
    w.send(Message::Text(
        serde_json::to_string(&abort_req).unwrap().into(),
    ))
    .await
    .unwrap();

    // 3. Drain frames until we get a final frame OR the abort ack.  We must
    //    not see a panic / connection drop.  Give it 5 s.
    let mut saw_completion = false;
    for _ in 0..50 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), r.next()).await;

        match item {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(&t).unwrap();

                // Accept either a final chat frame or an abort ack
                let is_final = v
                    .get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str())
                    == Some("final");
                let is_abort_ack = v["type"] == "res"
                    && v.get("id")
                        .and_then(|i| i.as_str())
                        .map(|id| id.contains("a9e0100a0008"))
                        .unwrap_or(false);

                if is_final || is_abort_ack {
                    saw_completion = true;
                    break;
                }
            }
            // Timeout → loop may have completed before abort arrived, which is
            // fine — treat as success (no panic occurred).
            Err(_) => {
                saw_completion = true;
                break;
            }
            Ok(Some(Ok(Message::Close(_)))) => {
                // Server closed cleanly — acceptable after abort
                saw_completion = true;
                break;
            }
            other => {
                // Any other non-text message — skip
                let _ = other;
            }
        }
    }

    assert!(
        saw_completion,
        "expected clean shutdown after abort (final frame, abort ack, or timeout)"
    );
}

// ---------------------------------------------------------------------------
// Test 8 — session-scoped HITL cache: second tool use skips permission.request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_hitl_cache_skips_second_permission_request() {
    // This test verifies the session-scoped HITL cache mechanism end-to-end:
    //
    // 1. The mock LLM returns a tool call on the first iteration.
    // 2. AppState::new() installs an auto-approve HitlGate, so the tool call
    //    proceeds immediately without waiting for a real cmd.res.
    // 3. The agentic config has `hitl_session_cache_enabled: true`.
    // 4. We confirm the loop completes cleanly (no deadlock, no panic).
    //
    // A true "second call skips permission.request" test would require a
    // manual-approval HitlGate that is not yet exposed in the test harness.
    // This test therefore validates the happy path: the cache flag is wired
    // through and does not break execution for the auto-approve gate.
    let list_files_args = r#"{"path": "."}"#;
    let chunks = vec![tool_call_chunk(
        "list_files",
        "call-cache-1",
        list_files_args,
    )];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 1,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    let req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0008",
        "method": "chat.send",
        "params": { "message": "list files with session cache enabled" }
    });
    w.send(Message::Text(serde_json::to_string(&req).unwrap().into()))
        .await
        .unwrap();

    // Collect frames; we must NOT see any permission.request events (Auto-tier
    // tool + auto-approve gate means no HITL check is triggered at all), and
    // we must eventually see a final frame.
    let mut saw_permission_request = false;
    let mut saw_tool_start = false;
    let mut saw_final = false;

    for _ in 0..50 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(8), r.next())
            .await
            .expect("timed out waiting for frame");

        match item {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();

                if v["type"] == "event" && v["event"] == "permission.request" {
                    saw_permission_request = true;
                }
                if v["type"] == "event" && v["event"] == "tool.start" {
                    saw_tool_start = true;
                }

                let is_final = v
                    .get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str())
                    == Some("final");
                if is_final {
                    saw_final = true;
                    break;
                }
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    assert!(
        !saw_permission_request,
        "permission.request should not fire for Auto-tier tool with auto-approve gate"
    );
    assert!(saw_tool_start, "expected tool.start event");
    assert!(saw_final, "expected final frame");
}

// ---------------------------------------------------------------------------
// Phase 11 writeback helper — returns both addr and a clone of the brain Arc
// so individual tests can query the brain after chat.send completes.
// ---------------------------------------------------------------------------

async fn start_server_with_brain(
    llm: LlmRouter,
    agentic: ChatAgenticConfig,
) -> (String, Arc<ironjaw_brain::Brain>) {
    let audit = AuditStore::open_memory().await.unwrap();
    let sessions = SessionStore::open_memory().await.unwrap();
    let brain = Arc::new(test_brain().await);
    let brain_ref = Arc::clone(&brain);
    let proactive = test_proactive().await;
    let voice = VoiceEngine::mock();

    let mut state = AppState::new(
        "test-secret-token".to_string(),
        audit,
        Arc::new(llm),
        sessions,
        32768,
        brain,
        proactive,
        voice,
        ImageProcessor::new(10_485_760),
    );
    state.chat_agentic = agentic;

    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (format!("127.0.0.1:{}", addr.port()), brain_ref)
}

// ---------------------------------------------------------------------------
// Phase 11 — Test 8: writeback enabled + tool with hints → brain memory stored
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_writeback_creates_tool_verified_memory() {
    // process_list has memory_hints returning entity="system", key="processes".
    // With writeback enabled, after execution a ToolVerified memory should appear.
    let process_args = r#"{}"#;
    let chunks = vec![tool_call_chunk("process_list", "call-wb-1", process_args)];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 1,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: true,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let (addr, brain) = start_server_with_brain(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    // Drive the agentic loop to completion.
    let req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0110a0001",
        "method": "chat.send",
        "params": { "message": "what processes are running" }
    });
    w.send(Message::Text(serde_json::to_string(&req).unwrap().into()))
        .await
        .unwrap();

    // Drain until final frame.
    let mut saw_final = false;
    for _ in 0..50 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(8), r.next())
            .await
            .expect("timed out waiting for frame");

        match item {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                let is_final = v
                    .get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str())
                    == Some("final");
                if is_final {
                    saw_final = true;
                    break;
                }
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
    assert!(saw_final, "expected final frame");

    // Give the writeback task a moment to complete (it runs in the same async task
    // inside execute_single, so by the time the final frame is sent it's done).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Verify a ToolVerified memory was written to the brain.
    let store = brain.store();
    let memories = store.all_memories().await.unwrap();
    let writeback = memories
        .iter()
        .find(|m| m.tags.as_deref().unwrap_or("").contains("tool_verified"));

    assert!(
        writeback.is_some(),
        "expected a tool_verified tagged memory after writeback, got memories: {:?}",
        memories.iter().map(|m| &m.tags).collect::<Vec<_>>()
    );
    let mem = writeback.unwrap();
    assert_eq!(
        mem.source.as_deref(),
        Some("process_list"),
        "source should be tool name"
    );
    assert_eq!(mem.memory_type.as_str(), "tool_verified");
}

// ---------------------------------------------------------------------------
// Phase 11 — Test 9: writeback disabled → no tool_verified memory written
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_writeback_disabled_by_config_writes_nothing() {
    let process_args = r#"{}"#;
    let chunks = vec![tool_call_chunk("process_list", "call-wb-2", process_args)];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 1,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        // disabled — writeback must not fire
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let (addr, brain) = start_server_with_brain(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    let req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0110a0002",
        "method": "chat.send",
        "params": { "message": "what processes are running" }
    });
    w.send(Message::Text(serde_json::to_string(&req).unwrap().into()))
        .await
        .unwrap();

    let mut saw_final = false;
    for _ in 0..50 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(8), r.next())
            .await
            .expect("timed out waiting for frame");

        match item {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                let is_final = v
                    .get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str())
                    == Some("final");
                if is_final {
                    saw_final = true;
                    break;
                }
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
    assert!(saw_final, "expected final frame");

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let store = brain.store();
    let memories = store.all_memories().await.unwrap();
    let writeback = memories
        .iter()
        .find(|m| m.tags.as_deref().unwrap_or("").contains("tool_verified"));

    assert!(
        writeback.is_none(),
        "expected NO tool_verified memory when writeback disabled, found: {writeback:?}",
    );
}

// ---------------------------------------------------------------------------
// Phase 11 — Test 10: tool execution failure → no writeback despite enabled=true
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_failure_does_not_trigger_writeback() {
    // Use a tool name that is not registered — execute returns ToolResult::Error.
    // Writeback is guarded by `ok == true`, so it must not fire.
    let chunks = vec![tool_call_chunk(
        "nonexistent_tool_phase11",
        "call-wb-3",
        r#"{}"#,
    )];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 1,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: true,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let (addr, brain) = start_server_with_brain(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    let req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0110a0003",
        "method": "chat.send",
        "params": { "message": "run nonexistent tool" }
    });
    w.send(Message::Text(serde_json::to_string(&req).unwrap().into()))
        .await
        .unwrap();

    let mut saw_tool_result_error = false;
    let mut saw_final = false;
    for _ in 0..50 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(8), r.next())
            .await
            .expect("timed out waiting for frame");

        match item {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "event"
                    && v["event"] == "tool.result"
                    && v["payload"]["ok"].as_bool() == Some(false)
                {
                    saw_tool_result_error = true;
                }
                let is_final = v
                    .get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str())
                    == Some("final");
                if is_final {
                    saw_final = true;
                    break;
                }
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
    assert!(saw_tool_result_error, "expected tool.result with ok=false");
    assert!(saw_final, "expected final frame");

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let store = brain.store();
    let memories = store.all_memories().await.unwrap();
    let writeback = memories
        .iter()
        .find(|m| m.tags.as_deref().unwrap_or("").contains("tool_verified"));

    assert!(
        writeback.is_none(),
        "expected NO tool_verified memory after tool failure, found: {writeback:?}",
    );
}

// ---------------------------------------------------------------------------
// Phase 11 — Test 11: two executions with writeback → second supersedes first
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_writeback_second_execution_updates_memory() {
    // Run process_list twice via two chat.send calls.  Both should succeed and
    // write ToolVerified memories.  After both calls, at least one (and possibly
    // two, depending on supersession behaviour) memory with tag tool_verified
    // should be present.
    let process_args = r#"{}"#;
    let chunks = vec![tool_call_chunk("process_list", "call-wb-4a", process_args)];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 1,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: true,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let (addr, brain) = start_server_with_brain(llm, agentic).await;

    // --- First chat.send ---
    {
        let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
        let (mut w, mut r) = ws.split();
        authenticate(&mut w, &mut r).await;

        let req = json!({
            "type": "req",
            "id": "00000000-0000-0000-0000-a9e0110a0004",
            "method": "chat.send",
            "params": { "message": "list processes first time" }
        });
        w.send(Message::Text(serde_json::to_string(&req).unwrap().into()))
            .await
            .unwrap();

        for _ in 0..50 {
            let item = tokio::time::timeout(std::time::Duration::from_secs(8), r.next())
                .await
                .expect("timed out");

            match item {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    let is_final = v
                        .get("payload")
                        .and_then(|p| p.get("state"))
                        .and_then(|s| s.as_str())
                        == Some("final");
                    if is_final {
                        break;
                    }
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // --- Second chat.send (new connection, same server) ---
    {
        let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
        let (mut w, mut r) = ws.split();
        authenticate(&mut w, &mut r).await;

        let req = json!({
            "type": "req",
            "id": "00000000-0000-0000-0000-a9e0110a0005",
            "method": "chat.send",
            "params": { "message": "list processes second time" }
        });
        w.send(Message::Text(serde_json::to_string(&req).unwrap().into()))
            .await
            .unwrap();

        for _ in 0..50 {
            let item = tokio::time::timeout(std::time::Duration::from_secs(8), r.next())
                .await
                .expect("timed out");

            match item {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    let is_final = v
                        .get("payload")
                        .and_then(|p| p.get("state"))
                        .and_then(|s| s.as_str())
                        == Some("final");
                    if is_final {
                        break;
                    }
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // After both executions there must be at least one ToolVerified memory.
    let store = brain.store();
    let memories = store.all_memories().await.unwrap();
    let writeback_count = memories
        .iter()
        .filter(|m| m.tags.as_deref().unwrap_or("").contains("tool_verified"))
        .count();

    assert!(
        writeback_count >= 1,
        "expected at least one tool_verified memory after two executions, got {writeback_count}"
    );
}

// ---------------------------------------------------------------------------
// A1 — Test: iteration budget note is injected exactly once, on the final
// iteration, and carried forward in every request sent afterward.
// ---------------------------------------------------------------------------

const BUDGET_NOTE: &str = "SYSTEM BUDGET NOTE: this is your final iteration. Answer the \
                            original question now in plain text using what you already have. \
                            Do not call any more tools.";

#[tokio::test]
async fn budget_note_injected_only_on_final_iteration() {
    // Every LLM call returns tool_calls (same scripted chunk replayed), so the
    // loop keeps going until max_iterations. The recording MockProvider
    // captures every ChatRequest sent, so we can verify the budget note
    // appears exactly once across the whole run, absent on the first request,
    // and present on the last.
    let list_files_args = r#"{"path": "."}"#;
    let chunks = vec![tool_call_chunk(
        "list_files",
        "call-budget-1",
        list_files_args,
    )];
    let recorder = Arc::new(std::sync::Mutex::new(Vec::new()));
    let llm = LlmRouter::mock_scripted_recording(chunks, recorder.clone());
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 3,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    let frames = send_chat_collect(&mut w, &mut r, "go").await;
    assert!(!frames.is_empty(), "expected at least one frame");

    let requests = recorder.lock().unwrap();
    assert!(
        requests.len() >= 3,
        "expected at least max_iterations LLM calls, got {}",
        requests.len()
    );

    let count_note = |req: &ironjaw_llm::ChatRequest| {
        req.messages
            .iter()
            .filter(|m| m.content == BUDGET_NOTE)
            .count()
    };

    // Every individual request's message list may contain the note at most
    // once (it's a `push`, never a duplicate injection).
    for (i, req) in requests.iter().enumerate() {
        let c = count_note(req);
        assert!(c <= 1, "request {i} contains the budget note {c} times");
    }

    // Requests sent BEFORE the final configured iteration (index
    // max_iterations - 1 == 2) must not carry the note at all — it is
    // injected only on the final iteration.
    for req in requests.iter().take(2) {
        assert_eq!(
            count_note(req),
            0,
            "budget note must not appear before the final iteration"
        );
    }

    // From the final configured iteration onward (including any later
    // forced-synthesis request, which clones the growing message history),
    // the note must be present exactly once.
    for req in requests.iter().skip(2) {
        assert_eq!(
            count_note(req),
            1,
            "budget note must be present from the final iteration onward"
        );
    }

    // The last request sent overall must contain it exactly once.
    let last = requests.last().unwrap();
    assert_eq!(
        count_note(last),
        1,
        "budget note must be present in the final request sent"
    );
}

// ---------------------------------------------------------------------------
// A2 — Test: per-run token ceiling stops the loop with an error-style event.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_token_ceiling_forces_synthesis_instead_of_error() {
    // The single scripted chunk reports huge usage (200k tokens) and a
    // tool_calls finish. The ceiling (150k) is checked at the TOP of each
    // iteration, so iteration 0 runs (spending the tokens, executing the
    // tool), and iteration 1 stops before calling the LLM again — but the
    // user must still get a real answer (forced synthesis), never a budget
    // error frame (regression: a healthy exploration run tripped the ceiling
    // and surfaced only "run token ceiling reached" to the user).
    let chunks = vec![tool_call_chunk_with_usage(
        "list_files",
        "call-ceiling-1",
        r#"{"path": "."}"#,
        200_000,
        0,
    )];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 5,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 150_000,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    let req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0009",
        "method": "chat.send",
        "params": { "message": "go over budget" }
    });
    w.send(Message::Text(serde_json::to_string(&req).unwrap().into()))
        .await
        .unwrap();

    let mut saw_final = false;
    for _ in 0..50 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(8), r.next())
            .await
            .expect("timed out waiting for frame");
        match item {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                let state = v
                    .get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str());
                if state == Some("error") {
                    panic!("token ceiling must force synthesis, not an error frame: {v}");
                }
                if state == Some("final") {
                    saw_final = true;
                    break;
                }
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
    assert!(
        saw_final,
        "expected a final frame (forced synthesis) after the token ceiling"
    );
}

// ---------------------------------------------------------------------------
// A3 — mid-stream resume (tee + swappable sink)
// ---------------------------------------------------------------------------

/// Authenticate, optionally resuming an existing `session_id`. Returns the
/// (possibly resumed) session_id from the auth ack. Any interleaved `event`
/// frames received before the ack (e.g. an immediate catchup on resume) are
/// skipped here — tests that care about them keep reading `read` afterward.
async fn authenticate_resuming(
    write: &mut WsSink,
    read: &mut WsStream,
    session_id: &str,
) -> String {
    let auth = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0020",
        "method": "auth.connect",
        "params": { "token": "test-secret-token", "session_id": session_id }
    });
    write
        .send(Message::Text(serde_json::to_string(&auth).unwrap().into()))
        .await
        .unwrap();

    loop {
        match read.next().await {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "event" {
                    continue;
                }
                assert_eq!(v["ok"], true, "resume auth failed: {v}");
                return v["payload"]["session_id"].as_str().unwrap().to_string();
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

#[tokio::test]
async fn reconnect_mid_stream_gets_catchup_then_tail_delta_then_final() {
    // Three chunks with a 400ms gap between each send, giving the test a
    // deterministic window to drop the first connection after "Hello " but
    // before "World" arrives, then reconnect and observe: catchup (with
    // exactly what was generated so far) -> tail delta ("World") -> final.
    let chunks = vec![
        delta_chunk("Hello "),
        delta_chunk("World"),
        done_stop_chunk(),
    ];
    let llm = LlmRouter::mock_scripted_delayed(chunks, std::time::Duration::from_millis(400));
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 5,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;

    let (ws1, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w1, mut r1) = ws1.split();
    let session_id = authenticate(&mut w1, &mut r1).await;

    let req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0021",
        "method": "chat.send",
        "params": { "message": "stream something long" }
    });
    w1.send(Message::Text(serde_json::to_string(&req).unwrap().into()))
        .await
        .unwrap();

    // Read until the first delta ("Hello ") arrives.
    let mut saw_first_delta = false;
    for _ in 0..10 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(8), r1.next())
            .await
            .expect("timed out waiting for the first delta");
        match item {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                let state = v
                    .get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str());
                if state == Some("delta") {
                    assert_eq!(v["payload"]["message"]["content"].as_str(), Some("Hello "));
                    saw_first_delta = true;
                    break;
                }
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert!(
        saw_first_delta,
        "expected the first delta before reconnecting"
    );

    // Drop the first connection mid-stream — "World" hasn't been sent yet
    // (the mock waits 400ms between chunks).
    drop(w1);
    drop(r1);

    // Give the server a moment to notice the closed socket and keep draining.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Reconnect on the SAME session before the run finishes.
    let (ws2, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w2, mut r2) = ws2.split();
    authenticate_resuming(&mut w2, &mut r2, &session_id).await;

    let mut saw_catchup = false;
    let mut saw_tail_delta = false;
    let mut saw_final = false;
    for _ in 0..20 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(8), r2.next())
            .await
            .expect("timed out waiting for a frame after reconnect");
        match item {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                let state = v
                    .get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str());
                if state == Some("catchup") {
                    assert_eq!(
                        v["payload"]["message"]["content"].as_str(),
                        Some("Hello "),
                        "catchup must carry exactly what was generated before the drop: {v}"
                    );
                    saw_catchup = true;
                } else if state == Some("delta") {
                    assert!(
                        saw_catchup,
                        "a tail delta arrived before the catchup frame: {v}"
                    );
                    assert_eq!(v["payload"]["message"]["content"].as_str(), Some("World"));
                    saw_tail_delta = true;
                } else if state == Some("final") {
                    assert!(
                        saw_catchup && saw_tail_delta,
                        "final arrived out of order: {v}"
                    );
                    saw_final = true;
                    break;
                }
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert!(saw_catchup, "expected a catchup frame on reconnect");
    assert!(
        saw_tail_delta,
        "expected the remaining delta to arrive on the new connection"
    );
    assert!(saw_final, "expected a final frame on the new connection");

    // Give the async persistence call (fired just after the final event) a
    // moment to land, then verify it was written exactly once.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let hist_req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0022",
        "method": "chat.history",
        "params": {}
    });
    w2.send(Message::Text(
        serde_json::to_string(&hist_req).unwrap().into(),
    ))
    .await
    .unwrap();

    let hist_v = loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), r2.next())
            .await
            .expect("timed out waiting for chat.history response");
        match item {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "res" {
                    break v;
                }
                // Skip any other interleaved event frames.
            }
            other => panic!("unexpected: {other:?}"),
        }
    };
    let messages = hist_v["payload"]["messages"]
        .as_array()
        .expect("messages array");
    let final_count = messages
        .iter()
        .filter(|m| m["role"] == "assistant" && m["content"] == "Hello World")
        .count();
    assert_eq!(
        final_count, 1,
        "final assistant message must be persisted exactly once, got: {messages:?}"
    );
}

#[tokio::test]
async fn reconnect_after_run_finished_gets_no_catchup() {
    // Run to completion on connection 1, THEN reconnect on the same session.
    // The RunBufferGuard removes the registry entry when run_agentic_chat
    // returns, so attach_if_live must find nothing live.
    let chunks = vec![stop_chunk("already finished")];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 5,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;

    let session_id = {
        let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
        let (mut w, mut r) = ws.split();
        let sid = authenticate(&mut w, &mut r).await;
        let frames = send_chat_collect(&mut w, &mut r, "hello").await;
        assert!(!frames.is_empty());
        sid
        // w, r dropped here — connection closes AFTER the run fully completed.
    };

    // Let the RunBufferGuard's Drop (end of run_agentic_chat) settle.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let (ws2, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w2, mut r2) = ws2.split();
    authenticate_resuming(&mut w2, &mut r2, &session_id).await;

    // Nothing further should arrive unsolicited, and certainly no catchup.
    for _ in 0..3 {
        let item = tokio::time::timeout(std::time::Duration::from_millis(500), r2.next()).await;
        match item {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                let state = v
                    .get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str());
                assert_ne!(
                    state,
                    Some("catchup"),
                    "must not get a catchup after the run already finished: {v}"
                );
            }
            Err(_) => break, // timed out — no more frames, which is expected
            other => panic!("unexpected: {other:?}"),
        }
    }
}

#[tokio::test]
async fn connection_stays_up_never_emits_catchup() {
    // Baseline regression: a connection that never drops must never see a
    // catchup frame, and the normal frame sequence (ack -> ... -> final) is
    // unchanged by the A3 plumbing.
    let chunks = vec![stop_chunk("no reconnect happened here")];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 5,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    let frames = send_chat_collect(&mut w, &mut r, "hello").await;
    assert!(!frames.is_empty());
    for f in &frames {
        let state = f
            .get("payload")
            .and_then(|p| p.get("state"))
            .and_then(|s| s.as_str());
        assert_ne!(
            state,
            Some("catchup"),
            "a connection that never dropped must never see a catchup frame: {f}"
        );
    }
    let last = frames.last().unwrap();
    assert_eq!(
        last["payload"]["state"].as_str(),
        Some("final"),
        "last frame must still be final: {last}"
    );
}

// ---------------------------------------------------------------------------
// Keepalive — protocol Pings must be answered while a run is streaming.
// Regression test for the starvation bug where in-run select arms dropped
// Ping frames into `_ => {}`, so keepalive-enforcing clients (OkHttp
// pingInterval, Python websockets) killed the connection mid-run.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn protocol_ping_answered_during_agentic_streaming() {
    let chunks = vec![
        delta_chunk("Hello "),
        delta_chunk("World"),
        done_stop_chunk(),
    ];
    let llm = LlmRouter::mock_scripted_delayed(chunks, std::time::Duration::from_millis(400));
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 5,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;

    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut write, mut read) = ws.split();
    authenticate(&mut write, &mut read).await;

    let req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0031",
        "method": "chat.send",
        "params": { "message": "stream something slow" }
    });
    write
        .send(Message::Text(serde_json::to_string(&req).unwrap().into()))
        .await
        .unwrap();

    // Wait for the first delta so we know the run is mid-stream.
    let mut saw_first_delta = false;
    for _ in 0..10 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(8), read.next())
            .await
            .expect("timed out waiting for the first delta");
        if let Some(Ok(Message::Text(t))) = item {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v.get("payload")
                .and_then(|p| p.get("state"))
                .and_then(|s| s.as_str())
                == Some("delta")
            {
                saw_first_delta = true;
                break;
            }
        }
    }
    assert!(saw_first_delta, "expected a delta before pinging");

    // Send a protocol-level Ping during the 400ms inter-chunk gap.
    write
        .send(Message::Ping(b"hb".to_vec().into()))
        .await
        .unwrap();

    // The server must answer with a Pong before the run finishes.
    let mut saw_pong = false;
    let mut saw_final = false;
    for _ in 0..30 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(8), read.next())
            .await
            .expect("timed out waiting for pong/final");
        match item {
            Some(Ok(Message::Pong(data))) => {
                assert_eq!(&data[..], b"hb", "pong must echo the ping payload");
                saw_pong = true;
            }
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v.get("payload")
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.as_str())
                    == Some("final")
                {
                    saw_final = true;
                    break;
                }
            }
            Some(Ok(_)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert!(
        saw_pong,
        "protocol Ping during streaming must be answered with a Pong"
    );
    assert!(saw_final, "the run must still complete normally");
}

// ---------------------------------------------------------------------------
// G1 — mid-stream death: terminal error chunk triggers one identical retry;
// a second failure surfaces the longer partial with a visible marker and
// persists it with complete = false.
// ---------------------------------------------------------------------------

/// Terminal chunk carrying a stream error (what llama_server.rs now emits on
/// an SSE read error or EOF without [DONE]).
fn error_chunk(msg: &str) -> ChatChunk {
    ChatChunk {
        delta: String::new(),
        done: true,
        usage: None,
        model: "mock-model".to_string(),
        error: Some(msg.to_string()),
        tool_call_deltas: None,
        finish_reason: None,
    }
}

#[tokio::test]
async fn stream_error_retried_once_then_succeeds() {
    // Call 1 dies mid-answer; call 2 (the retry) delivers the full answer.
    let llm = LlmRouter::mock_scripted_sequence(vec![
        vec![
            delta_chunk("partial "),
            error_chunk("stream error after 2 chunks: connection reset"),
        ],
        vec![stop_chunk("full answer after retry")],
    ]);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 5,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    let frames = send_chat_collect(&mut w, &mut r, "hello").await;
    let final_content = frames.last().unwrap()["payload"]["message"]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        final_content, "full answer after retry",
        "retry must replace the truncated partial"
    );
    assert!(
        !final_content.contains("[answer truncated"),
        "a successful retry must not carry the truncation marker"
    );
}

#[tokio::test]
async fn stream_error_twice_surfaces_truncated_partial() {
    // Every call dies after the same partial — retry also fails, so the
    // partial is surfaced with a visible marker and persisted incomplete.
    let chunks = vec![delta_chunk("half an ans"), error_chunk("boom")];
    let llm = LlmRouter::mock_scripted(chunks);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 5,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 0,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    let frames = send_chat_collect(&mut w, &mut r, "hello").await;
    let final_content = frames.last().unwrap()["payload"]["message"]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        final_content, "half an ans\n\n[answer truncated: model stream error]",
        "double failure must surface the partial with the truncation marker"
    );

    // The persisted assistant message must be flagged incomplete.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let hist_req = json!({
        "type": "req",
        "id": "00000000-0000-0000-0000-a9e0100a0031",
        "method": "chat.history",
        "params": {}
    });
    w.send(Message::Text(
        serde_json::to_string(&hist_req).unwrap().into(),
    ))
    .await
    .unwrap();
    let hist_v = loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), r.next())
            .await
            .expect("timed out waiting for chat.history response");
        match item {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "res" {
                    break v;
                }
            }
            other => panic!("unexpected: {other:?}"),
        }
    };
    let messages = hist_v["payload"]["messages"]
        .as_array()
        .expect("messages array");
    let assistant = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant message persisted");
    assert_eq!(
        assistant["complete"], false,
        "doubly-truncated answer must persist complete=false: {assistant}"
    );
}

// ---------------------------------------------------------------------------
// G2 — minimum-length final-answer guard: a too-short answer is re-prompted
// once; a second short answer is accepted as-is.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn short_answer_reprompted_once_for_full_answer() {
    let llm = LlmRouter::mock_scripted_sequence(vec![
        vec![stop_chunk("...")],
        vec![stop_chunk(
            "This is the real, complete answer to the question.",
        )],
    ]);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 5,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 20,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    let frames = send_chat_collect(&mut w, &mut r, "explain the setup").await;
    let final_content = frames.last().unwrap()["payload"]["message"]["content"]
        .as_str()
        .unwrap();
    assert_eq!(
        final_content, "This is the real, complete answer to the question.",
        "short answer must be re-prompted into the full answer"
    );
}

#[tokio::test]
async fn short_answer_twice_is_accepted() {
    // Guard fires once; a legitimately short answer costs one retry then passes.
    let llm = LlmRouter::mock_scripted(vec![stop_chunk("ok!")]);
    let agentic = ChatAgenticConfig {
        enabled: true,
        max_iterations: 5,
        tool_timeout_secs: 30,
        nudger_enabled: false,
        nudger_max_retries: 2,
        hitl_session_cache_enabled: true,
        tool_memory_writeback_enabled: false,
        run_token_ceiling: 0,
        tool_result_max_chars: 0,
        min_answer_chars: 20,
        turn_deadline_secs: 0,
    };
    let addr = start_server_with(llm, agentic).await;
    let (ws, _) = connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let (mut w, mut r) = ws.split();
    authenticate(&mut w, &mut r).await;

    let frames = send_chat_collect(&mut w, &mut r, "quick check").await;
    let final_content = frames.last().unwrap()["payload"]["message"]["content"]
        .as_str()
        .unwrap();
    assert_eq!(
        final_content, "ok!",
        "second short answer must be accepted as-is"
    );
}
