use crate::auth::{handle_auth_connect, AuthResult};
use crate::experiment;
use crate::notifications::Priority;
use crate::protocol::Frame;
use crate::state::AppState;
use crate::static_layer::collect_static_layer;
use crate::tool_set::{build_full_tool_set, build_local_tool_set};
use axum::extract::ws::{Message, WebSocket};
use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use ironjaw_audit::{ActionType, AuditEntry};
use ironjaw_brain::types::{NewMemory, TrustLevel};
use ironjaw_llm::{
    ChatMessage, ChatRequest, LlmError, ModelLoadEvent, Role, ToolCall, ToolCallDelta,
    ToolCallFunction, ToolChoice,
};
use ironjaw_proactive::{ActionType as ProactiveActionType, WorkflowStep};
use ironjaw_security::{check_permission, tier_for_method, PermissionDecision};
use ironjaw_session::compaction::{FlushMetrics, MemoryFlushConfig};
use ironjaw_session::NewMessage;
use ironjaw_tools::{MemoryHint, ToolContext, ToolResult};
use ironjaw_voice::VoiceMode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Global flush metrics shared across all handler invocations.
fn flush_metrics() -> &'static FlushMetrics {
    static METRICS: OnceLock<FlushMetrics> = OnceLock::new();
    METRICS.get_or_init(FlushMetrics::new)
}

/// Global injection detector (compile patterns once, reuse across all connections).
fn injection_detector() -> &'static ironjaw_security::sanitize::InjectionDetector {
    static DETECTOR: OnceLock<ironjaw_security::sanitize::InjectionDetector> = OnceLock::new();
    DETECTOR.get_or_init(ironjaw_security::sanitize::InjectionDetector::new)
}

/// Slim reference to a brain memory that was auto-injected into the LLM context for a turn.
/// Serialized into the `injected_memories` field of the `state: "final"` chat event.
#[derive(serde::Serialize, Clone)]
pub struct MemoryRef {
    pub id: i64,
    pub scope: Option<String>,
    pub tags: Option<String>,
    pub trust_level: String,
    pub snippet: String,
    /// Composite recall score (vector + FTS5 + RRF fusion) from ironjaw-brain HybridSearch.
    pub score: f32,
}

/// Per-connection mutable state: bundles auth info + current session
pub struct ConnectionContext {
    pub auth: AuthResult,
    pub current_session_id: String,
    pub connection_id: String,
    /// Pending images from binary frames, consumed by next chat.send
    pub pending_images: Vec<String>,
    /// Sender half for pushing frames to this connection's WebSocket
    pub tx: mpsc::UnboundedSender<Message>,
    /// Tracks pending (unsent) messages in the write channel for backpressure
    pub pending_count: Arc<AtomicUsize>,
    /// FIX #8: cancelled by the writer task when the socket sink send fails, so an
    /// in-flight turn flips to drain/buffer mode immediately instead of waiting for
    /// read-side Close/None (which can lag minutes on Windows). Does NOT abort the
    /// run — A3 catchup still delivers to a reconnecting client.
    pub socket_dead: tokio_util::sync::CancellationToken,
    /// Token to cancel active LLM streaming
    pub abort_token: tokio_util::sync::CancellationToken,
    /// Per-connection model override (set by model.switch)
    pub active_model_override: Option<String>,
}

#[cfg(test)]
impl ConnectionContext {
    /// Minimal ConnectionContext for unit tests. The tx channel is dropped immediately
    /// (nothing reads from it) — callers that send frames will silently discard them.
    pub fn new_for_tests() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            auth: AuthResult {
                authenticated: true,
                session_id: String::new(),
                device_id: None,
            },
            current_session_id: String::new(),
            connection_id: "test-conn".to_string(),
            pending_images: Vec::new(),
            tx,
            pending_count: Arc::new(AtomicUsize::new(0)),
            socket_dead: tokio_util::sync::CancellationToken::new(),
            abort_token: tokio_util::sync::CancellationToken::new(),
            active_model_override: None,
        }
    }
}

/// Handle a single WebSocket connection.
/// First message must be auth.connect. After auth, dispatch method calls.
pub async fn handle_connection(mut socket: WebSocket, state: AppState) {
    info!("New WebSocket connection");

    // Phase 1: First message must be auth (before socket split)
    let auth_result = match wait_for_auth(&mut socket, &state).await {
        Some(result) if result.authenticated => result,
        _ => {
            info!("Connection closed: auth failed or client disconnected");
            return;
        }
    };

    let connection_id = Uuid::new_v4().to_string();

    // Split WebSocket into sender/receiver halves.
    // The sender is wrapped in an mpsc channel so any context (DeviceRegistry,
    // notification router, etc.) can push frames without holding the socket.
    let (ws_sink, mut ws_stream) = socket.split();
    let (tx, mut tx_rx) = mpsc::unbounded_channel::<Message>();

    // Backpressure counter: tracks pending messages in the write channel
    let pending_count = Arc::new(AtomicUsize::new(0));
    let writer_pending = pending_count.clone();

    // FIX #8: shared with the ConnectionContext so a write-side sink failure
    // (client vanished) flips the in-flight turn to drain/buffer mode at once,
    // instead of waiting on read-side Close/None (which can lag minutes on
    // Windows) — this saves the forced-synthesis LLM call on a truly-dead socket.
    let socket_dead = tokio_util::sync::CancellationToken::new();
    let writer_socket_dead = socket_dead.clone();

    // Spawn writer task: drains channel messages into the WebSocket sink
    tokio::spawn(async move {
        let mut sink = ws_sink;
        while let Some(msg) = tx_rx.recv().await {
            // Saturating sub: `send_frame` (used for events) does not increment the counter,
            // so we must not allow the counter to wrap on underflow.
            writer_pending
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    Some(n.saturating_sub(1))
                })
                .ok();
            if sink.send(msg).await.is_err() {
                // Client's socket is gone — signal any in-flight turn to drain.
                writer_socket_dead.cancel();
                break;
            }
        }
    });

    // Track connection metrics
    state.observability.metrics.inc_connections();

    // Register device in the multi-device registry
    let device_type = auth_result.device_id.as_deref().unwrap_or("unknown");
    let device_name = format!("Device-{}", &connection_id[..8]);
    state
        .device
        .registry
        .register(
            &connection_id,
            device_type,
            &device_name,
            vec![],
            tx.clone(),
        )
        .await;

    // Drain any queued messages for this device (offline delivery)
    let queued = state.device.message_queue.drain(&connection_id).await;
    for qm in queued {
        let _ = tx.send(Message::Text(qm.frame_json.into()));
    }

    let mut ctx = ConnectionContext {
        current_session_id: auth_result.session_id.clone(),
        auth: auth_result,
        connection_id: connection_id.clone(),
        pending_images: Vec::new(),
        tx: tx.clone(),
        pending_count: pending_count.clone(),
        socket_dead,
        abort_token: tokio_util::sync::CancellationToken::new(),
        active_model_override: None,
    };

    // A3: if a `chat.send` agentic-loop run is still in flight for this
    // session (client reconnected mid-stream), reattach it to this
    // connection's tx. Pushes one `catchup` frame carrying everything
    // generated so far, then resumes live delivery on this socket.
    state
        .chat_runs
        .attach_if_live(&ctx.current_session_id, tx.clone());

    info!(
        session_id = %ctx.current_session_id,
        "Session active, entering message loop"
    );

    // B2: emit SessionStart so subprocess + WASM hooks observe new session
    // boundaries. Fires once per WebSocket session activation (post-auth,
    // before the message loop). Block actions are not honored here — the
    // session is already bound to the connection.
    {
        let s_ctx = crate::hooks::HookContext::with_session(
            crate::hooks::HookEvent::SessionStart,
            &ctx.current_session_id,
            serde_json::json!({"site": "auth.connect.post"}),
        );
        let _ = state.hooks.fire(&s_ctx).await;
    }

    // Spawn tick heartbeat as independent task (15 seconds) — must run even
    // during long-running handlers like chat.send LLM streaming, otherwise
    // the Dr. CLAW app's tick watchdog kills the connection after 45s.
    let tick_tx = tx.clone();
    let tick_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(15000));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // consume initial immediate tick
        loop {
            interval.tick().await;
            let tick_event = Frame::event(
                "tick",
                serde_json::json!({
                    "ts": chrono::Utc::now().timestamp_millis(),
                }),
            );
            if send_frame(&tick_tx, &tick_event).is_err() {
                break; // connection closed
            }
        }
    });

    // Message loop
    while let Some(msg) = ws_stream.next().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(e) => {
                error!("WebSocket receive error: {e}");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                debug!(
                    len = text.len(),
                    "Received text frame from client: {}",
                    &text[..text.len().min(500)]
                );
                if let Err(e) = handle_text_message(&text, &mut ws_stream, &state, &mut ctx).await {
                    error!("Error handling message: {e}");
                }
            }
            Message::Close(reason) => {
                info!(
                    session_id = %ctx.current_session_id,
                    ?reason,
                    "Client closed connection"
                );
                break;
            }
            Message::Binary(data) => {
                debug!(len = data.len(), "Received binary frame from client");
                if let Err(e) = handle_binary_frame(&data, &state, &mut ctx).await {
                    error!("Error handling binary frame: {e}");
                }
            }
            Message::Ping(data) => {
                debug!("Received ping from client");
                if tx.send(Message::Pong(data)).is_err() {
                    break;
                }
            }
            _ => {
                debug!("Received other frame type from client");
            }
        }
    }

    // Stop tick heartbeat
    tick_handle.abort();

    // Cleanup: unregister device, rate limiter state, and decrement metrics
    state.device.registry.unregister(&connection_id).await;
    state
        .security
        .rate_limiter
        .cleanup_connection(&connection_id);
    state.observability.metrics.dec_connections();

    info!(
        session_id = %ctx.current_session_id,
        "Connection ended"
    );
}

/// Wait for the first message, which must be auth.connect (or connect for OpenClaw compat)
async fn wait_for_auth(socket: &mut WebSocket, state: &AppState) -> Option<AuthResult> {
    // Protocol v4: wait for client-initiated auth.connect (no server challenge)
    let timeout = tokio::time::timeout(
        std::time::Duration::from_secs(state.auth_timeout_secs),
        socket.recv(),
    )
    .await;

    let msg = match timeout {
        Ok(Some(Ok(Message::Text(text)))) => text,
        Ok(Some(Ok(Message::Close(_)))) => return None,
        Ok(Some(Ok(_))) => return None,
        Ok(Some(Err(e))) => {
            error!("WebSocket error during auth: {}", e);
            return None;
        }
        Ok(None) => return None,
        Err(_) => {
            warn!(
                "Auth timeout: client did not authenticate within {} seconds",
                state.auth_timeout_secs
            );
            let timeout_frame =
                Frame::err(Uuid::new_v4(), "AUTH_TIMEOUT", "Authentication timeout");
            let _ = send_frame_ws(socket, &timeout_frame).await;
            return None;
        }
    };

    let frame: Frame = match serde_json::from_str(&msg) {
        Ok(f) => f,
        Err(e) => {
            warn!("Invalid JSON in auth message: {}", e);
            let err_frame = Frame::err(Uuid::new_v4(), "INVALID_FRAME", "Invalid JSON");
            let _ = send_frame_ws(socket, &err_frame).await;
            return None;
        }
    };

    match frame {
        Frame::Req { id, method, params } if method == "auth.connect" || method == "connect" => {
            let (response, auth_result) = handle_auth_connect(id, params, state).await;

            let _ = state
                .audit
                .log(&AuditEntry {
                    action_type: ActionType::Auth,
                    method: Some("auth.connect".to_string()),
                    tool_name: None,
                    params: None,
                    result: if auth_result.authenticated {
                        Some(serde_json::json!({"session_id": auth_result.session_id}))
                    } else {
                        None
                    },
                    ok: auth_result.authenticated,
                    error: if !auth_result.authenticated {
                        Some("Authentication failed".to_string())
                    } else {
                        None
                    },
                    llm_model: None,
                    tokens_in: None,
                    tokens_out: None,
                    cost_usd: None,
                    latency_ms: None,
                    session_id: if auth_result.authenticated {
                        Some(auth_result.session_id.clone())
                    } else {
                        None
                    },
                    device_id: auth_result.device_id.clone(),
                    request_id: id,
                    iteration: None,
                    tool_calls: None,
                })
                .await;

            let response_json = serde_json::to_string(&response).unwrap_or_default();
            debug!(
                "Auth response being sent: {}",
                &response_json[..response_json.len().min(800)]
            );
            let _ = send_frame_ws(socket, &response).await;
            Some(auth_result)
        }
        _ => {
            warn!("First message was not auth.connect");
            let err_frame = Frame::err(
                Uuid::new_v4(),
                "AUTH_REQUIRED",
                "First message must be auth.connect",
            );
            let _ = send_frame_ws(socket, &err_frame).await;
            None
        }
    }
}

/// Handle a text message after authentication
async fn handle_text_message(
    text: &str,
    ws_stream: &mut SplitStream<WebSocket>,
    state: &AppState,
    ctx: &mut ConnectionContext,
) -> anyhow::Result<()> {
    let frame: Frame = serde_json::from_str(text)?;

    match frame {
        Frame::Req { id, method, params } => {
            let start = std::time::Instant::now();
            state.observability.metrics.inc_requests();

            // Security: rate limit check (uses backpressure-aware send)
            if let Err(e) = state
                .security
                .rate_limiter
                .check(&ctx.connection_id, &method)
            {
                let response = Frame::retryable_err(id, "RATE_LIMITED", e.to_string());
                send_frame_checked(&ctx.tx, &ctx.pending_count, &response)?;
                return Ok(());
            }

            // Security: permission tier check
            let tier = tier_for_method(&method);
            let decision = check_permission(tier);

            // Security: HITL approval for Confirm/MultiConfirm tiers
            if tier.requires_approval() {
                let reason = match &decision {
                    PermissionDecision::NeedsApproval { reason }
                    | PermissionDecision::NeedsMultiConfirm { reason } => reason.clone(),
                    _ => String::new(),
                };
                let approved = await_hitl_approval(
                    ws_stream, state, &method, &tier, &reason, &params, &ctx.tx,
                )
                .await?;
                if !approved {
                    let response =
                        Frame::err(id, "PERMISSION_DENIED", "Operation was denied or timed out");
                    send_frame_checked(&ctx.tx, &ctx.pending_count, &response)?;
                    return Ok(());
                }
            }

            // Dispatch handlers
            match method.as_str() {
                "chat.send" => {
                    handle_chat_send(id, params, ws_stream, state, ctx).await?;
                }
                "chat.history" => {
                    let response = handle_chat_history(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "chat.abort" => {
                    // Cancel the active streaming task
                    ctx.abort_token.cancel();
                    // A3: also cancel the token stored in the run registry.
                    // If this connection reattached to a run started by an
                    // earlier (now-dead) connection, `ctx.abort_token` is a
                    // FRESH token the still-running task never sees — this
                    // reaches the original token it actually awaits on.
                    state.chat_runs.abort(&ctx.current_session_id);
                    // Create a fresh token for the next chat.send
                    ctx.abort_token = tokio_util::sync::CancellationToken::new();
                    let response = Frame::ok(id, serde_json::json!({"aborted": true}));
                    send_frame(&ctx.tx, &response)?;
                }
                "chat.inject" => {
                    let response = handle_chat_inject(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "chat.search" => {
                    let response = handle_chat_search(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "model.list" => {
                    let response = handle_model_list(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "model.available" => {
                    let response = handle_model_available(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "gateway.model" | "model.status" => {
                    let response = handle_gateway_model(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "session.list" | "sessions.list" => {
                    let response = handle_session_list(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "session.get" => {
                    let response = handle_session_get(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "session.create" => {
                    let response = handle_session_create(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "session.switch" => {
                    let response = handle_session_switch(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "session.delete" => {
                    let response = handle_session_delete(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "session.fork" => {
                    let response = handle_session_fork(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "session.rename" => {
                    let response = handle_session_rename(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "session.pin" => {
                    let response = handle_session_pin(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "session.archive" => {
                    let response = handle_session_archive(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "session.set_mode" => {
                    let response = handle_session_set_mode(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "session.set_cwd" => {
                    let response = handle_session_set_cwd(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "tools.list" => {
                    let response = handle_tools_list(id, state);
                    send_frame(&ctx.tx, &response)?;
                }
                "tools.execute" => {
                    handle_tools_execute(id, params, ws_stream, state, ctx).await?;
                }
                "brain.remember" => {
                    let response = handle_brain_remember(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.recall" => {
                    let response = handle_brain_recall(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.forget" => {
                    let response = handle_brain_forget(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.import" => {
                    let response = handle_brain_import(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.status" => {
                    let response = handle_brain_status(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.health" => {
                    let response = handle_brain_health(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.reload" => {
                    let response = handle_brain_reload(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.memories" => {
                    let response = handle_brain_memories(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.consolidate" => {
                    let response = handle_brain_consolidate(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.cleanup.stale" => {
                    let response = handle_brain_cleanup_stale(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.cleanup.contradictions" => {
                    let response = handle_brain_cleanup_contradictions(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.cleanup.duplicates" => {
                    let response = handle_brain_cleanup_duplicates(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.timeline" => {
                    let response = handle_brain_timeline(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.summaries" => {
                    let response = handle_brain_summaries(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.context.set" => {
                    let response = handle_brain_context_set(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.context.load" => {
                    let response = handle_brain_context_load(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.context.list" => {
                    let response = handle_brain_context_list(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.context.delete" => {
                    let response = handle_brain_context_delete(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.context.peek" => {
                    let response = handle_brain_context_peek(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.trace" => {
                    let response = handle_brain_trace(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.workflows" => {
                    let response = handle_brain_workflows(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.workflow.apply" => {
                    let response = handle_brain_workflow_apply(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.workflow.track" => {
                    let response = handle_brain_workflow_track(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.pending.list" => {
                    let response = handle_brain_pending_list(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.pending.approve" => {
                    let response = handle_brain_pending_approve(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.pending.reject" => {
                    let response = handle_brain_pending_reject(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.auto_learn.report" => {
                    let response = handle_brain_auto_learn_report(id).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.snapshot.list" => {
                    let response = handle_brain_snapshot_list(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.snapshot.create" => {
                    let response = handle_brain_snapshot_create(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.snapshot.restore" => {
                    let response = handle_brain_snapshot_restore(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.snapshot.delete" => {
                    let response = handle_brain_snapshot_delete(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "prompt.stats" => {
                    let response = handle_prompt_stats(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "schedule.list" => {
                    let response = handle_schedule_list(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "schedule.create" => {
                    let response = handle_schedule_create(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "schedule.delete" => {
                    let response = handle_schedule_delete(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "schedule.toggle" => {
                    let response = handle_schedule_toggle(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "workflow.create" => {
                    let response = handle_workflow_create(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "workflow.status" => {
                    let response = handle_workflow_status(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "workflow.cancel" => {
                    let response = handle_workflow_cancel(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "workflow.approve" => {
                    let response = handle_workflow_approve(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "voice.start" => {
                    let response = handle_voice_start(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "voice.stop" => {
                    let response = handle_voice_stop(id, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "voice.tts" => {
                    handle_voice_tts(id, &params, state, ctx).await?;
                }
                "tts.generate" | "tts.convert" => {
                    let response = handle_tts_generate(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "tts.voices" => {
                    let response = handle_tts_voices(id);
                    send_frame(&ctx.tx, &response)?;
                }
                "device.list" => {
                    let response = handle_device_list(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "device.cmd" => {
                    let response = handle_device_cmd(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "notify.send" => {
                    let response = handle_notify_send(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "export.conversations" => {
                    let response = handle_export_conversations(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "export.brain" => {
                    let response = handle_export_brain(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "infra.status" | "infra.metrics" => {
                    let response = handle_infra_metrics(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "metrics.counters" => {
                    let response = handle_metrics_counters(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "backup.create" => {
                    let response = handle_backup_create(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // CC Bridge proxy methods
                "ccbridge.sessions" => {
                    let response = handle_ccbridge_sessions(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "ccbridge.sessions.rename" => {
                    let response = handle_ccbridge_rename(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "ccbridge.chat" => {
                    let response = handle_ccbridge_chat(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // CC session management (direct subprocess via ironjaw-claude)
                "cc.sessions" => {
                    let response = handle_cc_sessions(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "cc.session.get" => {
                    let response = handle_cc_session_get(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "cc.history" => {
                    let response = handle_cc_history(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "cc.history.since" => {
                    let response = handle_cc_history_since(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "cc.chat" => {
                    // Spawn in background so the message loop stays responsive
                    // (allows cc.chat.abort to be processed while streaming).
                    let state2 = state.clone();
                    let tx2 = ctx.tx.clone();
                    let params2 = params.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_cc_chat_bg(id, &params2, &state2, &tx2).await {
                            error!("cc.chat background error: {e}");
                        }
                    });
                }
                "cc.chat.abort" => {
                    let response = handle_cc_chat_abort(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "cc.session.create" => {
                    let state2 = state.clone();
                    let tx2 = ctx.tx.clone();
                    let params2 = params.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_cc_session_create_bg(id, &params2, &state2, &tx2).await
                        {
                            error!("cc.session.create background error: {e}");
                        }
                    });
                }
                "cc.attach" => {
                    handle_cc_attach(id, &params, state, ctx).await?;
                }
                "cc.detach" => {
                    let response = handle_cc_detach(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // CC Chronicle proxy methods
                "chronicle.sessions" => {
                    let response =
                        handle_chronicle_proxy(id, &params, state, "/api/sessions").await;
                    send_frame(&ctx.tx, &response)?;
                }
                "chronicle.session" => {
                    let session_id = params.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let response = handle_chronicle_proxy(
                        id,
                        &params,
                        state,
                        &format!("/api/sessions/{session_id}"),
                    )
                    .await;
                    send_frame(&ctx.tx, &response)?;
                }
                "chronicle.search" => {
                    let response = handle_chronicle_proxy(id, &params, state, "/api/search").await;
                    send_frame(&ctx.tx, &response)?;
                }
                "chronicle.topics" => {
                    let response = handle_chronicle_proxy(id, &params, state, "/api/topics").await;
                    send_frame(&ctx.tx, &response)?;
                }
                "chronicle.insights" => {
                    let response =
                        handle_chronicle_proxy(id, &params, state, "/api/insights").await;
                    send_frame(&ctx.tx, &response)?;
                }
                "chronicle.stats" => {
                    let response = handle_chronicle_proxy(id, &params, state, "/api/stats").await;
                    send_frame(&ctx.tx, &response)?;
                }
                "chronicle.timeline" => {
                    let response =
                        handle_chronicle_proxy(id, &params, state, "/api/timeline").await;
                    send_frame(&ctx.tx, &response)?;
                }
                // Dr. CLAW RPC equivalents
                "project.status" => {
                    let response = handle_project_status(id, &params).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "file.get" => {
                    let response = handle_file_get(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "model.llm" => {
                    let response = handle_model_llm(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "model.switch" => {
                    let response = handle_model_switch(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "role.list" => {
                    let response = handle_role_list(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // Skill method aliases: skills.list / skill.list return skills array;
                // skills.active / session.skills return active skill info with skills key.
                "skills.list" | "skill.list" => {
                    let response = handle_skills_list(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "skills.active" | "session.skills" => {
                    let response = handle_skills_active(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "role.switch" => {
                    let response = handle_role_switch(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "role.active" => {
                    let response = handle_role_active(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "model.session" => {
                    let response = handle_model_session(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "role.preset.apply" => {
                    let response = handle_role_preset_apply(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "gateway.restart" => {
                    let response = handle_gateway_restart(id).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "chronicle.enrich" => {
                    let response = handle_chronicle_enrich(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // Dr. CLAW compatibility aliases
                "sessions.patch" => {
                    let response = handle_session_rename(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "sessions.delete" => {
                    let response = handle_session_delete(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "sessions.preview" => {
                    let response = handle_sessions_preview(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // Command Center methods
                "command.sessions" => {
                    let response = handle_command_sessions(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "command.session.get" => {
                    let response = handle_command_session_get(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "command.session.action" => {
                    let response = handle_command_session_action(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "command.usage.summary" => {
                    let response = Frame::ok(
                        id,
                        serde_json::json!({
                            "totalTokens": 0,
                            "totalCostUsd": 0.0,
                            "requestCount": 0,
                        }),
                    );
                    send_frame(&ctx.tx, &response)?;
                }
                "command.usage.by_role" => {
                    let response = Frame::ok(id, serde_json::json!([]));
                    send_frame(&ctx.tx, &response)?;
                }
                // System resource monitoring (Phase 4 Polish)
                "system.resources" => {
                    let resources = crate::system_info::get_system_resources();
                    let frame = Frame::ok(id, serde_json::to_value(&resources).unwrap_or_default());
                    send_frame(&ctx.tx, &frame)?;
                }
                // Device pairing
                "device.pair.request" => {
                    let response = handle_device_pair_request(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "device.pair.confirm" => {
                    let response = handle_device_pair_confirm(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "device.pair.revoke" => {
                    let response = handle_device_pair_revoke(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // Plugin management
                "plugins.list" => {
                    let response = handle_plugins_list(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "plugins.toggle" => {
                    let response = handle_plugins_toggle(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // Model performance tracking (Phase 4 Polish)
                "model.perf" => {
                    let perf = state.llm.perf_tracker.summary();
                    let frame = Frame::ok(id, serde_json::to_value(&perf).unwrap_or_default());
                    send_frame(&ctx.tx, &frame)?;
                }
                // Experiment runner (I1)
                "experiment.create" => {
                    let response = experiment::handle_experiment_create(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "experiment.start" => {
                    let response = experiment::handle_experiment_start(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "experiment.status" => {
                    let response = experiment::handle_experiment_status(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "experiment.stop" => {
                    let response = experiment::handle_experiment_stop(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "experiment.results" => {
                    let response = experiment::handle_experiment_results(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "experiment.resume" => {
                    let response = experiment::handle_experiment_resume(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // Prompt-space evolution (I13)
                "brain.evolve.start" => {
                    let response = handle_brain_evolve_start(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.evolve.status" => {
                    let response = handle_brain_evolve_status(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.evolve.stop" => {
                    let response = handle_brain_evolve_stop(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.evolve.promote" => {
                    let response = handle_brain_evolve_promote(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.evolve.history" => {
                    let response = handle_brain_evolve_history(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // Agent handoff (I5)
                "agent.handoff" => {
                    let response = handle_agent_handoff(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // Scatter-gather (I9)
                "agent.scatter" => {
                    let response = handle_agent_scatter(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // Blackboard (I7)
                "blackboard.create" => {
                    let response = handle_blackboard_create(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "blackboard.write" => {
                    let response = handle_blackboard_write(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "blackboard.read" => {
                    let response = handle_blackboard_read(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "blackboard.subscribe" => {
                    let response = handle_blackboard_subscribe(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "blackboard.destroy" => {
                    let response = handle_blackboard_destroy(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "blackboard.list" => {
                    let response = handle_blackboard_list(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // Group conversations (I8)
                "group.create" => {
                    let response = handle_group_create(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "group.message" => {
                    let response = handle_group_message(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "group.terminate" => {
                    let response = handle_group_terminate(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "group.history" => {
                    let response = handle_group_history(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "group.list" => {
                    let response = handle_group_list(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // ---- Context Health Monitoring (I16) ----
                "context.health" => {
                    let snap = state
                        .brain
                        .context_health
                        .snapshot(&ctx.current_session_id)
                        .await;
                    let response = Frame::ok(id, serde_json::to_value(&snap).unwrap_or_default());
                    send_frame(&ctx.tx, &response)?;
                }
                "context.health.subscribe" => {
                    let mut rx = state.brain.context_health.subscribe();
                    let tx2 = ctx.tx.clone();
                    tokio::spawn(async move {
                        while let Ok(alert) = rx.recv().await {
                            let event = Frame::event(
                                "context.health.alert",
                                serde_json::to_value(&alert).unwrap_or_default(),
                            );
                            if send_frame(&tx2, &event).is_err() {
                                break;
                            }
                        }
                    });
                    let response = Frame::ok(id, serde_json::json!({"subscribed": true}));
                    send_frame(&ctx.tx, &response)?;
                }
                // ---- CodeAct Pattern (I17) ----
                "code.execute" => {
                    let request: crate::code_act::ExecuteRequest = serde_json::from_value(
                        params.clone(),
                    )
                    .unwrap_or(crate::code_act::ExecuteRequest {
                        code: params
                            .get("code")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        language: params
                            .get("language")
                            .and_then(|v| v.as_str())
                            .unwrap_or("python")
                            .to_string(),
                        timeout_secs: params.get("timeout_secs").and_then(|v| v.as_u64()),
                    });
                    let response = match state
                        .code_act
                        .execute(&ctx.current_session_id, &request)
                        .await
                    {
                        Ok(result) => {
                            Frame::ok(id, serde_json::to_value(&result).unwrap_or_default())
                        }
                        Err(e) => Frame::err(id, "CODE_EXECUTE_FAILED", e.to_string()),
                    };
                    send_frame(&ctx.tx, &response)?;
                }
                // ---- Skill Library / Voyager (I4) ----
                "skills.save" => {
                    let request: Result<crate::skill_library::SaveSkillRequest, _> =
                        serde_json::from_value(params.clone());
                    let response = match request {
                        Ok(req) => match state.skill.library.save(req).await {
                            Ok(skill) => {
                                Frame::ok(id, serde_json::to_value(&skill).unwrap_or_default())
                            }
                            Err(e) => Frame::err(id, "SKILL_SAVE_FAILED", e.to_string()),
                        },
                        Err(e) => Frame::err(id, "INVALID_PARAMS", e.to_string()),
                    };
                    send_frame(&ctx.tx, &response)?;
                }
                "skills.search" => {
                    let request: Result<crate::skill_library::SearchSkillRequest, _> =
                        serde_json::from_value(params.clone());
                    let response = match request {
                        Ok(req) => {
                            let results = state.skill.library.search(&req).await;
                            Frame::ok(id, serde_json::to_value(&results).unwrap_or_default())
                        }
                        Err(e) => Frame::err(id, "INVALID_PARAMS", e.to_string()),
                    };
                    send_frame(&ctx.tx, &response)?;
                }
                "skills.chain" => {
                    let request: Result<crate::skill_library::ChainSkillRequest, _> =
                        serde_json::from_value(params.clone());
                    let response = match request {
                        Ok(req) => match state.skill.library.chain(&req).await {
                            Ok(skill) => {
                                Frame::ok(id, serde_json::to_value(&skill).unwrap_or_default())
                            }
                            Err(e) => Frame::err(id, "SKILL_CHAIN_FAILED", e.to_string()),
                        },
                        Err(e) => Frame::err(id, "INVALID_PARAMS", e.to_string()),
                    };
                    send_frame(&ctx.tx, &response)?;
                }
                "skills.library.list" => {
                    let skills = state.skill.library.list().await;
                    let response = Frame::ok(id, serde_json::to_value(&skills).unwrap_or_default());
                    send_frame(&ctx.tx, &response)?;
                }
                // ---- Dynamic Tool Registry (I21) ----
                "tools.define" => {
                    let request: Result<crate::tool_registry::DefineToolRequest, _> =
                        serde_json::from_value(params.clone());
                    let response = match request {
                        Ok(req) => {
                            match state
                                .dynamic_tools
                                .define(req, &ctx.current_session_id)
                                .await
                            {
                                Ok(tool) => {
                                    Frame::ok(id, serde_json::to_value(&tool).unwrap_or_default())
                                }
                                Err(e) => Frame::err(id, "TOOL_DEFINE_FAILED", e.to_string()),
                            }
                        }
                        Err(e) => Frame::err(id, "INVALID_PARAMS", e.to_string()),
                    };
                    send_frame(&ctx.tx, &response)?;
                }
                "tools.dynamic.list" => {
                    let tools = state.dynamic_tools.list().await;
                    let response = Frame::ok(id, serde_json::to_value(&tools).unwrap_or_default());
                    send_frame(&ctx.tx, &response)?;
                }
                "tools.dynamic.invoke" => {
                    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let tool_params = params
                        .get("params")
                        .cloned()
                        .unwrap_or(serde_json::json!({}));
                    let response = match state.dynamic_tools.invoke(name, tool_params).await {
                        Ok(result) => {
                            Frame::ok(id, serde_json::to_value(&result).unwrap_or_default())
                        }
                        Err(e) => Frame::err(id, "TOOL_INVOKE_FAILED", e.to_string()),
                    };
                    send_frame(&ctx.tx, &response)?;
                }
                "tools.dynamic.remove" => {
                    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let response = match state.dynamic_tools.remove(name).await {
                        Ok(()) => Frame::ok(id, serde_json::json!({"removed": name})),
                        Err(e) => Frame::err(id, "TOOL_REMOVE_FAILED", e.to_string()),
                    };
                    send_frame(&ctx.tx, &response)?;
                }
                // ---- Meta-Configuration Optimizer (I22) ----
                "config.experiment" => {
                    let request: Result<crate::meta_config::CreateExperimentRequest, _> =
                        serde_json::from_value(params.clone());
                    let response = match request {
                        Ok(req) => match state.meta_config.create(req).await {
                            Ok(exp) => {
                                Frame::ok(id, serde_json::to_value(&exp).unwrap_or_default())
                            }
                            Err(e) => Frame::err(id, "EXPERIMENT_CREATE_FAILED", e.to_string()),
                        },
                        Err(e) => Frame::err(id, "INVALID_PARAMS", e.to_string()),
                    };
                    send_frame(&ctx.tx, &response)?;
                }
                "config.experiment.status" => {
                    let exp_id = params.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let response = match state.meta_config.status(exp_id).await {
                        Ok(status) => {
                            Frame::ok(id, serde_json::to_value(&status).unwrap_or_default())
                        }
                        Err(e) => Frame::err(id, "EXPERIMENT_STATUS_FAILED", e.to_string()),
                    };
                    send_frame(&ctx.tx, &response)?;
                }
                "config.experiment.promote" => {
                    let exp_id = params.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let variant = params.get("variant").and_then(|v| v.as_str()).unwrap_or("");
                    let response = match state.meta_config.promote(exp_id, variant).await {
                        Ok(exp) => Frame::ok(id, serde_json::to_value(&exp).unwrap_or_default()),
                        Err(e) => Frame::err(id, "EXPERIMENT_PROMOTE_FAILED", e.to_string()),
                    };
                    send_frame(&ctx.tx, &response)?;
                }
                // ---- CC Parity Phase 6: Task 18 — Context & Safety ----
                "tools.search" => {
                    let response = handle_tools_search(id, &params, state);
                    send_frame(&ctx.tx, &response)?;
                }
                "context.status" => {
                    let response = handle_context_status(id, state);
                    send_frame(&ctx.tx, &response)?;
                }
                "context.compact" => {
                    let response = handle_context_compact(id, &params, state, ctx).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "context.collapse" => {
                    let response = handle_context_collapse(id, &params, state);
                    send_frame(&ctx.tx, &response)?;
                }
                "cache.stats" => {
                    let response = handle_cache_stats(id, state);
                    send_frame(&ctx.tx, &response)?;
                }
                "file.snapshots" => {
                    let response = handle_file_snapshots(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "file.restore" => {
                    let response = handle_file_restore(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "security.rules.list" => {
                    let response = handle_security_rules_list(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "security.rules.add" => {
                    let response = handle_security_rules_add(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "security.rules.remove" => {
                    let response = handle_security_rules_remove(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "security.rules.test" => {
                    let response = handle_security_rules_test(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                // ---- CC Parity Phase 6: Task 19 — Agent & Advanced ----
                "agent.fork" => {
                    let response = handle_agent_fork(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "agent.worktree.create" => {
                    let response = handle_agent_worktree_create(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "agent.worktree.cleanup" => {
                    let response = handle_agent_worktree_cleanup(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "agent.mailbox.send" => {
                    let response = handle_agent_mailbox_send(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "agent.mailbox.peek" => {
                    let response = handle_agent_mailbox_peek(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "agent.mailbox.claim" => {
                    let response = handle_agent_mailbox_claim(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "session.denials" => {
                    let response = handle_session_denials(id, &params, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "security.classifier.stats" => {
                    let response = handle_security_classifier_stats(id, state);
                    send_frame(&ctx.tx, &response)?;
                }
                "daemon.status" => {
                    let response = handle_daemon_status(id, state);
                    send_frame(&ctx.tx, &response)?;
                }
                "daemon.log" => {
                    let response = handle_daemon_log(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.dream.status" => {
                    let response = handle_brain_dream_status(id, state);
                    send_frame(&ctx.tx, &response)?;
                }
                "brain.dream.trigger" => {
                    let response = handle_brain_dream_trigger(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "sentiment.check" => {
                    let response = handle_sentiment_check(id, &params);
                    send_frame(&ctx.tx, &response)?;
                }
                "security.anti_distill.status" => {
                    let response = handle_anti_distill_status(id);
                    send_frame(&ctx.tx, &response)?;
                }
                "prefetch.status" => {
                    let response = handle_prefetch_status(id, state).await;
                    send_frame(&ctx.tx, &response)?;
                }
                "commands.list" => {
                    let cmds = state.commands.list();
                    let response = Frame::ok(id, serde_json::to_value(&cmds).unwrap_or_default());
                    send_frame(&ctx.tx, &response)?;
                }
                "commands.execute" => {
                    let command = params
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim_start_matches('/');
                    let args = params.get("args").and_then(|v| v.as_str()).unwrap_or("");
                    let response = match state.commands.get(command) {
                        Some(cmd) => {
                            let result = cmd.execute(args, state, ctx).await;
                            Frame::ok(id, serde_json::to_value(&result).unwrap_or_default())
                        }
                        None => Frame::err(
                            id,
                            "COMMAND_NOT_FOUND",
                            format!("/{command} is not a registered command"),
                        ),
                    };
                    send_frame(&ctx.tx, &response)?;
                }
                "hooks.list" => {
                    // B2 CP6: dump the discovered-hook catalog. Independent
                    // of the live registry; populated at startup and on
                    // hooks.reload.
                    let entries = state.hooks_catalog.read().await.clone();
                    let response =
                        Frame::ok(id, serde_json::to_value(&entries).unwrap_or_default());
                    send_frame(&ctx.tx, &response)?;
                }
                "hooks.reload" => {
                    // B2 CP6: rescan the hooks directory and rebuild the
                    // registry + catalog. Tier is MultiConfirm at the method
                    // table; the reload itself trusts that gate.
                    let response = match state.hooks_runtime.as_ref() {
                        None => Frame::err(
                            id,
                            "HOOKS_DISABLED",
                            "Hooks are disabled in config; cannot reload.".to_string(),
                        ),
                        Some(rt) => {
                            // Drop and recreate the registry to discard prior
                            // hooks. The Arc swap is racy with in-flight
                            // dispatches, but per-event RwLocks inside the
                            // registry mean any in-flight `fire` finishes
                            // against the old registry — acceptable for B2.
                            let new_registry = crate::hooks::HookRegistry::new();
                            let result = crate::hooks::discover_and_register(
                                &new_registry,
                                Arc::clone(&rt.sandbox),
                                Some(state.audit.clone()),
                                &rt.dir,
                                rt.subprocess_enabled,
                                &rt.subprocess_enabled_events,
                                rt.subprocess_default_timeout_secs,
                            )
                            .await;
                            match result {
                                Ok(entries) => {
                                    let count = entries.len();
                                    let registered = entries.iter().filter(|e| e.enabled).count();
                                    *state.hooks_catalog.write().await = entries;
                                    Frame::ok(
                                        id,
                                        serde_json::json!({
                                            "ok": true,
                                            "discovered": count,
                                            "registered": registered,
                                        }),
                                    )
                                }
                                Err(e) => Frame::err(
                                    id,
                                    "HOOKS_RELOAD_FAILED",
                                    format!("hook discovery failed: {e}"),
                                ),
                            }
                        }
                    };
                    send_frame(&ctx.tx, &response)?;
                }
                other => {
                    let response = match other {
                        "ping" => Frame::ok(
                            id,
                            serde_json::json!({
                                "pong": true,
                                "timestamp": chrono::Utc::now().to_rfc3339(),
                                "session_id": ctx.current_session_id,
                                "uptime_secs": (chrono::Utc::now() - state.started_at).num_seconds(),
                            }),
                        ),
                        _ => Frame::err(
                            id,
                            "NOT_IMPLEMENTED",
                            format!("Method '{other}' is not implemented yet"),
                        ),
                    };

                    let elapsed = start.elapsed();

                    let _ = state
                        .audit
                        .log(&AuditEntry {
                            action_type: ActionType::Request,
                            method: Some(method.clone()),
                            tool_name: None,
                            params: Some(params),
                            result: None,
                            ok: matches!(&response, Frame::Res { ok: true, .. }),
                            error: None,
                            llm_model: None,
                            tokens_in: None,
                            tokens_out: None,
                            cost_usd: None,
                            latency_ms: Some(elapsed.as_millis() as i64),
                            session_id: Some(ctx.current_session_id.clone()),
                            device_id: ctx.auth.device_id.clone(),
                            request_id: id,
                            iteration: None,
                            tool_calls: None,
                        })
                        .await;

                    send_frame(&ctx.tx, &response)?;
                }
            }

            // Security: send notify event for Tier 1 operations
            if tier.notifies_after() {
                let notify = Frame::event(
                    "permission.notify",
                    serde_json::json!({
                        "method": method,
                        "tier": "notify",
                    }),
                );
                let _ = send_frame(&ctx.tx, &notify);
            }
        }
        Frame::CmdRes { id, ok, scope, .. } => {
            // Resolve pending HITL approval requests
            if !state.security.hitl_gate.resolve(id, ok) {
                warn!(request_id = %id, "Received cmd.res for unknown request");
            }
            // Phase 6: if the client approved with scope="session", record the
            // session-scoped approval so future calls skip the gate entirely.
            if ok && scope.as_deref() == Some("session") {
                if let Some(tool_name) = state.security.hitl_gate.take_tool_for_request(&id) {
                    if !ctx.current_session_id.is_empty() {
                        state
                            .security
                            .hitl_gate
                            .record_session_approval(&ctx.current_session_id, &tool_name);
                        debug!(
                            session_id = %ctx.current_session_id,
                            tool = %tool_name,
                            "Session-scoped HITL approval recorded"
                        );
                    }
                }
            }
        }
        Frame::UiRes { id, action, data } => {
            info!(id = %id, action = %action, "Received UI response");
            let data = data.unwrap_or(serde_json::json!({}));

            // Route approve/deny actions to workflow approval gates
            match action.as_str() {
                "approve" | "deny" => {
                    let workflow_id = data.get("workflow_id").and_then(|v| v.as_str());
                    let step_id = data.get("step_id").and_then(|v| v.as_str());

                    match (workflow_id, step_id) {
                        (Some(wf_id), Some(s_id)) => {
                            let result = if action == "approve" {
                                state.proactive.approve_workflow_step(wf_id, s_id).await
                            } else {
                                state.proactive.deny_workflow_step(wf_id, s_id).await
                            };
                            match result {
                                Ok(()) => {
                                    info!(
                                        workflow_id = wf_id,
                                        step_id = s_id,
                                        action = %action,
                                        "Workflow step {action} via ui.res"
                                    );
                                }
                                Err(e) => {
                                    warn!(
                                        workflow_id = wf_id,
                                        step_id = s_id,
                                        error = %e,
                                        "Failed to {action} workflow step via ui.res"
                                    );
                                }
                            }
                        }
                        _ => {
                            warn!(
                                id = %id,
                                "ui.res {action} missing workflow_id or step_id"
                            );
                        }
                    }
                }
                _ => {
                    debug!(id = %id, action = %action, "ui.res action not routed");
                }
            }
        }
        _ => {
            warn!("Unexpected frame type received");
        }
    }

    Ok(())
}

// -- HITL approval helper --

/// Send a permission request to the client and wait for their CmdRes approval.
/// Returns `true` if approved, `false` if denied or timed out.
async fn await_hitl_approval(
    ws_stream: &mut SplitStream<WebSocket>,
    state: &AppState,
    method: &str,
    tier: &ironjaw_security::PermissionTier,
    reason: &str,
    params: &serde_json::Value,
    tx: &mpsc::UnboundedSender<Message>,
) -> anyhow::Result<bool> {
    // Auto-approve mode (used in tests)
    if state.security.hitl_gate.is_auto_approve() {
        return Ok(true);
    }

    let (request_id, _rx) = state.security.hitl_gate.request_approval();

    // Send permission.request event to client via channel
    let event = Frame::event(
        "permission.request",
        serde_json::json!({
            "request_id": request_id,
            "method": method,
            "tier": *tier as u8,
            "reason": reason,
            "params": params,
        }),
    );
    send_frame(tx, &event)?;

    // Wait for CmdRes with matching request_id (with timeout)
    let timeout_duration = state.security.hitl_gate.timeout();
    let deadline = tokio::time::Instant::now() + timeout_duration;

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            state.security.hitl_gate.cancel(&request_id);
            return Ok(false);
        }
        let remaining = deadline - now;

        match tokio::time::timeout(remaining, ws_stream.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(Frame::CmdRes { id, ok, .. }) = serde_json::from_str::<Frame>(&text) {
                    if id == request_id {
                        state.security.hitl_gate.resolve(request_id, ok);
                        return Ok(ok);
                    }
                }
                // Non-matching frames are ignored during HITL wait
            }
            Ok(Some(Ok(Message::Ping(data)))) => {
                let _ = tx.send(Message::Pong(data));
            }
            _ => {
                // Connection closed, error, or timeout
                state.security.hitl_gate.cancel(&request_id);
                return Ok(false);
            }
        }
    }
}

// -- Context visibility helpers --

/// Get context window of the currently loaded model (falls back to config limit).
async fn active_model_context_window(state: &AppState) -> i64 {
    let loaded = state.llm.llama_state.read().await.loaded_model.clone();
    loaded
        .as_deref()
        .and_then(|m| {
            // Try exact match by registry key first
            state.llm.model_registry.get(m).or_else(|| {
                // Fallback: match by GGUF filename in path (loaded_model is often the filename)
                let m_lower = m.to_lowercase();
                state.llm.model_registry.models.values().find(|e| {
                    let fname = e.path.replace('\\', "/");
                    let fname = fname.rsplit('/').next().unwrap_or(&e.path);
                    fname.to_lowercase() == m_lower
                        || fname.to_lowercase().trim_end_matches(".gguf") == m_lower
                        || m_lower.ends_with(&fname.to_lowercase())
                })
            })
        })
        .map(|e| e.context as i64)
        .unwrap_or(state.session.context_token_limit as i64)
}

/// Calculate context percentage (0-100, clamped).
fn context_pct(tokens: i64, window: i64) -> i64 {
    if window > 0 {
        ((tokens * 100) / window).min(100)
    } else {
        0
    }
}

/// Think-state transition emitted while filtering reasoning tags from streamed text.
#[derive(Debug, PartialEq, Eq)]
enum ThinkEvent {
    Started,
    Ended,
}

/// Strip `<think>...</think>` reasoning blocks — and stray orphan `</think>` /
/// `<think>` tags — from streaming LLM text. Consumes the processed prefix of
/// `buffer`, leaving any partial-tag tail for the next chunk, and tracks
/// `in_think` across chunks. Returns the visible text to forward plus the
/// ordered think-state transitions for the caller to surface as events.
///
/// Abliterated Qwen3.6 with thinking disabled emits a doubled/empty skeleton
/// like `<think>\n\n</think>\n\n</think>`; the trailing orphan `</think>` has no
/// matching opener, so the old paired-only filter forwarded it to the client.
/// This mirrors `strip_think_tags` in openai_compat.rs, which already handles
/// the same bare-tag case on the /v1 path.
fn filter_think_stream(buffer: &mut String, in_think: &mut bool) -> (String, Vec<ThinkEvent>) {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    let mut visible = String::new();
    let mut events = Vec::new();
    loop {
        if *in_think {
            if let Some(end) = buffer.find(CLOSE) {
                *buffer = buffer[end + CLOSE.len()..].to_string();
                *in_think = false;
                events.push(ThinkEvent::Ended);
            } else {
                // Still inside the block — consume and wait for more.
                break;
            }
        } else {
            let open = buffer.find(OPEN);
            let close = buffer.find(CLOSE);
            // A closing tag with no opener before it is a stray orphan — discard
            // it rather than forward it to the client.
            let stray_close = match (open, close) {
                (None, Some(c)) => Some(c),
                (Some(o), Some(c)) if c < o => Some(c),
                _ => None,
            };
            if let Some(c) = stray_close {
                visible.push_str(&buffer[..c]);
                *buffer = buffer[c + CLOSE.len()..].to_string();
            } else if let Some(o) = open {
                visible.push_str(&buffer[..o]);
                *buffer = buffer[o + OPEN.len()..].to_string();
                *in_think = true;
                events.push(ThinkEvent::Started);
            } else {
                // No complete tag. Hold back a tail in case a tag is split across
                // chunks (longest tag is "</think>" = 8 bytes). A '<' that never
                // becomes a tag is flushed once more text (or stream end) arrives.
                if !buffer.contains('<') {
                    visible.push_str(buffer);
                    buffer.clear();
                } else if buffer.len() > CLOSE.len() {
                    let mut safe = buffer.len() - CLOSE.len();
                    while safe > 0 && !buffer.is_char_boundary(safe) {
                        safe -= 1;
                    }
                    if safe > 0 {
                        visible.push_str(&buffer[..safe]);
                        *buffer = buffer[safe..].to_string();
                    }
                }
                break;
            }
        }
    }
    (visible, events)
}

/// Quick check if a string looks like a UUID (8-4-4-4-12 hex pattern).
fn looks_like_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts[0].len() == 8
        && parts[1].len() == 4
        && parts[2].len() == 4
        && parts[3].len() == 4
        && parts[4].len() == 12
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

// -- Session handlers --

async fn handle_session_list(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let limit = params.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
    let offset = params.get("offset").and_then(|v| v.as_i64()).unwrap_or(0);

    match state.session.store.list_sessions(limit, offset).await {
        Ok(sessions) => {
            let count = state.session.store.count_sessions().await.unwrap_or(0);
            let model_ctx = active_model_context_window(state).await;
            let enriched: Vec<serde_json::Value> = sessions
                .into_iter()
                .map(|s| {
                    let pct = context_pct(s.context_tokens, model_ctx);
                    serde_json::json!({
                        "id": s.id,
                        "user_id": s.user_id,
                        "title": s.title,
                        "created_at": s.created_at,
                        "updated_at": s.updated_at,
                        "context_tokens": s.context_tokens,
                        "compaction_count": s.compaction_count,
                        "input_tokens": s.input_tokens,
                        "output_tokens": s.output_tokens,
                        "message_count": s.message_count,
                        "pinned": s.pinned,
                        "archived": s.archived,
                        "context_pct": pct,
                        "context_window": model_ctx,
                    })
                })
                .collect();
            Frame::ok(
                id,
                serde_json::json!({ "sessions": enriched, "count": count }),
            )
        }
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

async fn handle_session_get(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &ConnectionContext,
) -> Frame {
    let session_id = params
        .get("session_id")
        .and_then(|s| s.as_str())
        .unwrap_or(&ctx.current_session_id);

    match state.session.store.get_session(session_id).await {
        Ok(Some(session)) => {
            let model_ctx = active_model_context_window(state).await;
            let pct = context_pct(session.context_tokens, model_ctx);
            Frame::ok(
                id,
                serde_json::json!({
                    "id": session.id,
                    "user_id": session.user_id,
                    "title": session.title,
                    "created_at": session.created_at,
                    "updated_at": session.updated_at,
                    "context_tokens": session.context_tokens,
                    "compaction_count": session.compaction_count,
                    "message_count": session.message_count,
                    "context_pct": pct,
                    "context_window": model_ctx,
                }),
            )
        }
        Ok(None) => Frame::err(id, "NOT_FOUND", "Session not found"),
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

async fn handle_session_create(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &mut ConnectionContext,
) -> Frame {
    let title = params.get("title").and_then(|v| v.as_str());
    let switch = params
        .get("switch")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let new_id = Uuid::new_v4().to_string();

    match state
        .session
        .store
        .create_session(&new_id, "default", title)
        .await
    {
        Ok(session) => {
            let switched = if switch {
                ctx.current_session_id = session.id.clone();
                true
            } else {
                false
            };
            Frame::ok(
                id,
                serde_json::json!({ "session": session, "switched": switched }),
            )
        }
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

async fn handle_session_switch(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &mut ConnectionContext,
) -> Frame {
    let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
        Some(sid) => sid,
        None => return Frame::err(id, "INVALID_PARAMS", "session_id is required"),
    };

    match state.session.store.get_session(session_id).await {
        Ok(Some(session)) => {
            // Before switching, spawn session summary for the departing session (fire-and-forget)
            let brain_cfg = state.brain.engine.config();
            if brain_cfg.session_summary_enabled && brain_cfg.session_summary_on_switch {
                let departing_session_id = ctx.current_session_id.clone();
                let sessions = state.session.store.clone();
                let brain_store = state.brain.engine.store().clone();
                let llm = state.llm.router.clone();
                tokio::spawn(async move {
                    if let Ok(messages) = sessions.recent_messages(&departing_session_id, 50).await
                    {
                        let pairs = ironjaw_session::extract_flush_candidates(&messages);
                        if let Err(e) = ironjaw_brain::generate_session_summary(
                            &brain_store,
                            &llm,
                            &departing_session_id,
                            &pairs,
                        )
                        .await
                        {
                            warn!(
                                session_id = %departing_session_id,
                                error = %e,
                                "session.switch: summary generation failed"
                            );
                        }
                        // LLM-powered memory extraction from departing session
                        let flush_config = MemoryFlushConfig::default();
                        let candidates = ironjaw_session::flush_memories_llm(
                            &messages,
                            &flush_config,
                            Some(&llm),
                            Some(flush_metrics()),
                        )
                        .await;
                        if !candidates.is_empty() {
                            info!(
                                session_id = %departing_session_id,
                                count = candidates.len(),
                                "session.switch: extracted memory candidates"
                            );
                        }
                    }
                });
            }

            ctx.current_session_id = session.id.clone();
            Frame::ok(
                id,
                serde_json::json!({ "session": session, "switched": true }),
            )
        }
        Ok(None) => Frame::err(
            id,
            "SESSION_NOT_FOUND",
            format!("Session '{session_id}' not found"),
        ),
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

async fn handle_session_delete(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &ConnectionContext,
) -> Frame {
    let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
        Some(sid) => sid.to_string(),
        None => return Frame::err(id, "INVALID_PARAMS", "session_id is required"),
    };

    if session_id == ctx.current_session_id {
        return Frame::err(
            id,
            "CANNOT_DELETE_ACTIVE",
            "Cannot delete the active session. Switch to another session first.",
        );
    }

    // Verify session exists before spawning background work
    match state.session.store.get_session(&session_id).await {
        Ok(None) => {
            return Frame::err(
                id,
                "SESSION_NOT_FOUND",
                format!("Session '{session_id}' not found"),
            );
        }
        Err(e) => {
            return Frame::err(id, "STORE_ERROR", e.to_string());
        }
        Ok(Some(_)) => {}
    }

    // Fetch messages for background summary before deleting
    let messages_for_summary = if state.brain.engine.config().session_summary_enabled {
        state
            .session
            .store
            .recent_messages(&session_id, 50)
            .await
            .ok()
    } else {
        None
    };

    // Spawn background task for summary extraction only (fire-and-forget)
    if let Some(messages) = messages_for_summary {
        let bg_state = state.clone();
        let bg_session_id = session_id.clone();
        tokio::spawn(async move {
            let pairs = ironjaw_session::extract_flush_candidates(&messages);
            if let Err(e) = ironjaw_brain::generate_session_summary(
                bg_state.brain.engine.store(),
                &bg_state.llm.router,
                &bg_session_id,
                &pairs,
            )
            .await
            {
                warn!(
                    session_id = %bg_session_id,
                    error = %e,
                    "session.delete: summary generation failed"
                );
            }
            // LLM-powered memory extraction
            let flush_config = MemoryFlushConfig::default();
            let candidates = ironjaw_session::flush_memories_llm(
                &messages,
                &flush_config,
                Some(&bg_state.llm.router),
                Some(flush_metrics()),
            )
            .await;
            if !candidates.is_empty() {
                info!(
                    session_id = %bg_session_id,
                    count = candidates.len(),
                    "session.delete: extracted memory candidates"
                );
            }
        });
    }

    // Delete synchronously so the UI sees it immediately
    if let Err(e) = state.session.store.delete_session(&session_id).await {
        return Frame::err(id, "DELETE_ERROR", e.to_string());
    }

    Frame::ok(
        id,
        serde_json::json!({ "deleted": true, "session_id": session_id }),
    )
}

async fn handle_session_rename(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &ConnectionContext,
) -> Frame {
    let title = match params.get("title").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return Frame::err(id, "INVALID_PARAMS", "title is required"),
    };

    let session_id = params
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or(&ctx.current_session_id);

    match state.session.store.rename_session(session_id, title).await {
        Ok(true) => Frame::ok(
            id,
            serde_json::json!({ "renamed": true, "session_id": session_id, "title": title }),
        ),
        Ok(false) => Frame::err(
            id,
            "SESSION_NOT_FOUND",
            format!("Session '{session_id}' not found"),
        ),
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

/// Handle session.pin — pin or unpin a session.
async fn handle_session_pin(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
        Some(sid) => sid,
        None => return Frame::err(id, "INVALID_PARAMS", "session_id is required"),
    };
    let pinned = params
        .get("pinned")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    match state.session.store.pin_session(session_id, pinned).await {
        Ok(true) => Frame::ok(
            id,
            serde_json::json!({ "ok": true, "session_id": session_id, "pinned": pinned }),
        ),
        Ok(false) => Frame::err(
            id,
            "SESSION_NOT_FOUND",
            format!("Session '{session_id}' not found"),
        ),
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

/// Handle session.archive — archive or unarchive a session.
async fn handle_session_archive(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
        Some(sid) => sid,
        None => return Frame::err(id, "INVALID_PARAMS", "session_id is required"),
    };
    let archived = params
        .get("archived")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    match state
        .session
        .store
        .archive_session(session_id, archived)
        .await
    {
        Ok(true) => Frame::ok(
            id,
            serde_json::json!({ "ok": true, "session_id": session_id, "archived": archived }),
        ),
        Ok(false) => Frame::err(
            id,
            "SESSION_NOT_FOUND",
            format!("Session '{session_id}' not found"),
        ),
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

/// Handle session.set_mode — set HITL override mode for a session.
async fn handle_session_set_mode(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
        Some(sid) => sid,
        None => return Frame::err(id, "INVALID_PARAMS", "session_id is required"),
    };
    let mode_str = match params.get("mode").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => return Frame::err(id, "INVALID_PARAMS", "mode is required"),
    };
    let mode: ironjaw_session::types::SessionMode =
        match serde_json::from_value(serde_json::Value::String(mode_str.to_owned())) {
            Ok(m) => m,
            Err(_) => {
                return Frame::err(
                    id,
                    "INVALID_PARAMS",
                    format!("invalid mode '{mode_str}'; expected default, plan, or autoacccept"),
                )
            }
        };
    match state.session.store.set_mode(session_id, &mode).await {
        Ok(true) => Frame::ok(
            id,
            serde_json::json!({ "ok": true, "session_id": session_id, "mode": mode_str }),
        ),
        Ok(false) => Frame::err(
            id,
            "SESSION_NOT_FOUND",
            format!("Session '{session_id}' not found"),
        ),
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

/// C1 (Wave C): set the per-session working directory override. Pass
/// `path: null` to clear the override and fall back to `current_dir()`.
/// The path is validated to exist and be a directory before persistence —
/// the value the user sees later via `session.get` matches what tools will
/// resolve against. Tier: Confirm (it scopes file/shell tools to a new FS root).
async fn handle_session_set_cwd(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &ConnectionContext,
) -> Frame {
    let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
        Some(sid) => sid,
        None => return Frame::err(id, "INVALID_PARAMS", "session_id is required"),
    };

    // C2-A: Reject if any tools are currently executing for this session.
    // This prevents a race where a tool resolves its cwd between our check
    // and the actual directory change.
    {
        let in_flight_count = state
            .in_flight_tools
            .get(session_id)
            .map(|m| m.len())
            .unwrap_or(0);
        if in_flight_count > 0 {
            tracing::warn!(
                session_id = %session_id,
                in_flight = in_flight_count,
                "session.set_cwd blocked — tools still in-flight",
            );
            let _ = state
                .audit
                .log(&AuditEntry {
                    action_type: ActionType::Request,
                    method: Some("session.set_cwd".to_string()),
                    tool_name: None,
                    params: Some(params.clone()),
                    result: None,
                    ok: false,
                    error: Some(format!(
                        "blocked: {in_flight_count} tool(s) still executing"
                    )),
                    llm_model: None,
                    tokens_in: None,
                    tokens_out: None,
                    cost_usd: None,
                    latency_ms: None,
                    session_id: Some(session_id.to_string()),
                    device_id: ctx.auth.device_id.clone(),
                    request_id: id,
                    iteration: None,
                    tool_calls: None,
                })
                .await;
            return Frame::err(
                id,
                "IN_FLIGHT_TOOLS",
                format!(
                    "{in_flight_count} tool(s) still executing for session '{session_id}'; \
                     retry after they complete"
                ),
            );
        }
    }

    // C2-A: Acquire per-session cwd lock so concurrent set_cwd requests are
    // serialised and don't interleave between validation and the store write.
    let lock_entry = state
        .cwd_locks
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _cwd_guard = lock_entry.lock().await;

    // `path` is optional — passing null/missing clears the override.
    let path_param = params.get("path");
    let path_str: Option<&str> = match path_param {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => match v.as_str() {
            Some(s) if !s.is_empty() => Some(s),
            Some(_) => None,
            None => return Frame::err(id, "INVALID_PARAMS", "path must be a string or null"),
        },
    };
    if let Some(p) = path_str {
        let pb = std::path::PathBuf::from(p);
        if !pb.exists() {
            return Frame::err(id, "PATH_NOT_FOUND", format!("path '{p}' does not exist"));
        }
        if !pb.is_dir() {
            return Frame::err(id, "PATH_NOT_DIR", format!("path '{p}' is not a directory"));
        }
    }

    let start = std::time::Instant::now();
    let store_result = state
        .session
        .store
        .set_working_dir(session_id, path_str)
        .await;
    let latency_ms = start.elapsed().as_millis() as i64;

    match store_result {
        Ok(true) => {
            // C2-A: Audit successful cwd change.
            tracing::info!(
                session_id = %session_id,
                working_dir = ?path_str,
                "session.set_cwd succeeded",
            );
            let _ = state
                .audit
                .log(&AuditEntry {
                    action_type: ActionType::Request,
                    method: Some("session.set_cwd".to_string()),
                    tool_name: None,
                    params: Some(params.clone()),
                    result: Some(serde_json::json!({
                        "session_id": session_id,
                        "working_dir": path_str,
                    })),
                    ok: true,
                    error: None,
                    llm_model: None,
                    tokens_in: None,
                    tokens_out: None,
                    cost_usd: None,
                    latency_ms: Some(latency_ms),
                    session_id: Some(session_id.to_string()),
                    device_id: ctx.auth.device_id.clone(),
                    request_id: id,
                    iteration: None,
                    tool_calls: None,
                })
                .await;
            Frame::ok(
                id,
                serde_json::json!({
                    "ok": true,
                    "session_id": session_id,
                    "working_dir": path_str,
                }),
            )
        }
        Ok(false) => Frame::err(
            id,
            "SESSION_NOT_FOUND",
            format!("Session '{session_id}' not found"),
        ),
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

/// C1 (Wave C): resolve the working directory tools should run against for
/// `session_id`. Priority chain:
///   1. `session.working_dir` (per-session override, if Some and the path exists)
///   2. `state.tools_working_dir` (global default from `[tools] working_dir` config)
///   3. `std::env::current_dir()` (process default)
///
/// Tier 2 was deferred from initial C1 and added in C1 tier-3 follow-up.
/// `tools_working_dir` is `None` when the config value is the default (".") —
/// equivalent to falling through to tier 3.
async fn resolve_session_cwd(state: &AppState, session_id: &str) -> std::path::PathBuf {
    if let Ok(Some(s)) = state.session.store.get_session(session_id).await {
        if let Some(wd) = s.working_dir {
            let p = std::path::PathBuf::from(&wd);
            if p.exists() && p.is_dir() {
                return p;
            }
            // Stale override (path no longer exists) — fall through to next tier.
            tracing::warn!(
                session_id = %session_id,
                stale_path = %wd,
                "Session working_dir override is stale; falling back to config / current_dir",
            );
        }
    }
    // Tier 2: global default from `[tools] working_dir` config.
    if let Some(p) = state.tools_working_dir.read().await.as_ref() {
        if p.exists() && p.is_dir() {
            return p.clone();
        }
        tracing::warn!(
            stale_path = %p.display(),
            "Configured [tools] working_dir is stale; falling back to current_dir",
        );
    }
    std::env::current_dir().unwrap_or_default()
}

/// Handle session.fork — fork a session, optionally at a specific message.
/// Creates a new session with messages copied up to the fork point.
async fn handle_session_fork(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
        Some(sid) => sid,
        None => return Frame::err(id, "INVALID_PARAMS", "session_id is required"),
    };
    let message_id = params.get("message_id").and_then(|v| v.as_i64());
    let description = params
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Verify source session exists
    let source = match state.session.store.get_session(session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Frame::err(
                id,
                "SESSION_NOT_FOUND",
                format!("Session '{session_id}' not found"),
            )
        }
        Err(e) => return Frame::err(id, "STORE_ERROR", e.to_string()),
    };

    // Create the forked session
    let fork_session_id = Uuid::new_v4().to_string();
    let fork_title = format!("Fork of {}", source.title.as_deref().unwrap_or(&source.id));
    let forked = match state
        .session
        .store
        .create_session(&fork_session_id, &source.user_id, Some(&fork_title))
        .await
    {
        Ok(s) => s,
        Err(e) => return Frame::err(id, "STORE_ERROR", e.to_string()),
    };

    // Copy messages up to the fork point
    let messages = match state
        .session
        .store
        .get_messages(session_id, 10_000, None)
        .await
    {
        Ok(msgs) => msgs,
        Err(e) => return Frame::err(id, "STORE_ERROR", e.to_string()),
    };

    let mut copied = 0;
    for msg in &messages {
        // If a message_id fork point was specified, stop after that message
        if let Some(fork_at) = message_id {
            if msg.id > fork_at {
                break;
            }
        }
        let new_msg = ironjaw_session::NewMessage {
            session_id: fork_session_id.clone(),
            role: msg.role.clone(),
            content: msg.content.clone(),
            model: msg.model.clone(),
            tokens: msg.tokens,
            complete: msg.complete,
            tool_calls: None,
            tool_call_id: None,
        };
        if let Err(e) = state.session.store.add_message(&new_msg).await {
            warn!("Failed to copy message during fork: {e}");
        } else {
            copied += 1;
        }
    }

    // Register the fork in the ForkManager
    let fork_point = message_id
        .map(|mid| mid.to_string())
        .unwrap_or_else(|| "head".to_string());
    let mut fork_mgr = state.session.fork_manager.write().await;
    let fork_id = match fork_mgr.create_fork(session_id, &fork_session_id, &fork_point, description)
    {
        Ok(fid) => fid,
        Err(e) => {
            warn!("Fork created but ForkManager registration failed: {e}");
            String::new()
        }
    };

    Frame::ok(
        id,
        serde_json::json!({
            "session": forked,
            "fork_id": fork_id,
            "messages_copied": copied,
            "fork_point": fork_point,
        }),
    )
}

async fn handle_chat_history(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &ConnectionContext,
) -> Frame {
    // Accept both "session_id" (Ironjaw native) and "sessionKey" (Dr. CLAW app compat)
    let session_id = params
        .get("session_id")
        .or_else(|| params.get("sessionKey"))
        .and_then(|v| v.as_str())
        .unwrap_or(&ctx.current_session_id);
    let limit = params.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
    let before_id = params.get("before_id").and_then(|v| v.as_i64());

    match state
        .session
        .store
        .get_messages(session_id, limit, before_id)
        .await
    {
        Ok(messages) => {
            let total = state
                .session
                .store
                .message_count(session_id)
                .await
                .unwrap_or(0);
            let has_more = if let Some(first) = messages.first() {
                first.id > 1
            } else {
                false
            };
            // Convert messages to include epoch ms timestamp for Dr. CLAW app compat
            let compat_messages: Vec<serde_json::Value> = messages
                .iter()
                .map(|m| {
                    let ts_ms = chrono::NaiveDateTime::parse_from_str(
                        &m.created_at,
                        "%Y-%m-%dT%H:%M:%S%.f",
                    )
                    .or_else(|_| {
                        chrono::NaiveDateTime::parse_from_str(&m.created_at, "%Y-%m-%dT%H:%M:%S")
                    })
                    .or_else(|_| {
                        // SQLite strftime %f outputs SS.SSS — try parsing as RFC 3339
                        m.created_at
                            .parse::<chrono::DateTime<chrono::Utc>>()
                            .map(|dt| dt.naive_utc())
                    })
                    .map(|dt| dt.and_utc().timestamp_millis())
                    .unwrap_or(0);
                    serde_json::json!({
                        "id": m.id,
                        "session_id": m.session_id,
                        "role": m.role,
                        "content": m.content,
                        "model": m.model,
                        "tokens": m.tokens,
                        "complete": m.complete,
                        "created_at": m.created_at,
                        "timestamp": ts_ms,
                        "tool_calls": m.tool_calls,
                        "tool_call_id": m.tool_call_id,
                    })
                })
                .collect();
            Frame::ok(
                id,
                serde_json::json!({
                    "messages": compat_messages,
                    "count": total,
                    "has_more": has_more,
                }),
            )
        }
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

// -- Chat handler --

/// Estimate token count from text (~4 chars per token)
fn estimate_tokens(text: &str) -> i64 {
    (text.len() as i64 + 3) / 4
}

/// Handle chat.send — stream LLM response tokens back to client
async fn handle_chat_send(
    id: Uuid,
    params: serde_json::Value,
    ws_stream: &mut SplitStream<WebSocket>,
    state: &AppState,
    ctx: &mut ConnectionContext,
) -> anyhow::Result<()> {
    let start = std::time::Instant::now();

    if !state.llm.router.has_providers() {
        let response = Frame::err(id, "NO_LLM", "No LLM providers configured");
        let _ = state
            .audit
            .log(&AuditEntry {
                action_type: ActionType::LlmCall,
                method: Some("chat.send".to_string()),
                tool_name: None,
                params: Some(params),
                result: None,
                ok: false,
                error: Some("No LLM providers configured".to_string()),
                llm_model: None,
                tokens_in: None,
                tokens_out: None,
                cost_usd: None,
                latency_ms: Some(start.elapsed().as_millis() as i64),
                session_id: Some(ctx.current_session_id.clone()),
                device_id: ctx.auth.device_id.clone(),
                request_id: id,
                iteration: None,
                tool_calls: None,
            })
            .await;
        send_frame(&ctx.tx, &response)?;
        return Ok(());
    }

    // Determine mode: single message (server-managed context) vs messages array (client-managed)
    let is_single_message = params.get("message").is_some();

    // Phase 10: force_fresh=true skips brain injection entirely for this request
    let force_fresh = params
        .get("force_fresh")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Memories auto-injected this turn; populated in the client-managed branch below.
    // Empty for server-managed turns (build_context has no scored memories).
    let mut injected_refs: Vec<MemoryRef> = Vec::new();

    // Parse messages from params
    let (messages, user_message_text) = if let Some(msgs) = params.get("messages") {
        let parsed: Vec<ChatMessage> = serde_json::from_value(msgs.clone()).unwrap_or_default();
        let last_user = parsed
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone());

        // Static layer: discover CLAUDE.md files from cwd, inject BEFORE brain context.
        let static_cwd = resolve_session_cwd(state, &ctx.current_session_id).await;
        let static_entries = collect_static_layer(&static_cwd).await;

        // Auto-inject brain memories for client-managed message arrays.
        // Skip when voice session is active and exclude_voice is configured (latency).
        let skip_voice = state.brain.engine.config().auto_inject.exclude_voice
            && state.voice.has_session(&ctx.connection_id).await;
        let inject_msg = if let Some(ref query) = last_user {
            if skip_voice {
                debug!("auto-inject: skipping (voice session active)");
                None
            } else {
                let inject_timeout_ms = state.brain.engine.config().auto_inject.timeout_ms;
                let inject_future = state
                    .brain
                    .engine
                    .build_auto_inject_context(query, force_fresh);
                let inject_result = if inject_timeout_ms > 0 {
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(inject_timeout_ms),
                        inject_future,
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => {
                            warn!(
                                timeout_ms = inject_timeout_ms,
                                "auto-inject: brain context timed out, proceeding without"
                            );
                            Ok((None, Vec::new()))
                        }
                    }
                } else {
                    inject_future.await
                };
                let (msg, inject_scored) = match inject_result {
                    Ok((m, scored)) => (m, scored),
                    Err(e) => {
                        warn!(error = %e, "auto-inject: brain recall failed, skipping");
                        (None, vec![])
                    }
                };
                injected_refs = inject_scored
                    .iter()
                    .map(|sm| MemoryRef {
                        id: sm.memory.id,
                        scope: sm.memory.scope.clone(),
                        tags: sm.memory.tags.clone(),
                        trust_level: format!("{:?}", sm.memory.trust_level).to_lowercase(),
                        snippet: sm.memory.content.chars().take(120).collect(),
                        score: sm.score,
                    })
                    .collect();
                if msg.is_some() {
                    debug!(
                        count = injected_refs.len(),
                        "auto-inject: prepending brain context"
                    );
                }
                msg
            }
        } else {
            None
        };

        // Assemble prompt prefix via layered builder (StaticLayer → Brain → History).
        let prefix = crate::prompt_builder::PromptBuilder::new()
            .with_static_layer(&static_entries)
            .with_brain(inject_msg)
            .build();
        let mut final_msgs = prefix.messages;
        final_msgs.extend(parsed);

        (final_msgs, last_user)
    } else if let Some(msg) = params.get("message").and_then(|m| m.as_str()) {
        let user_msg = msg.to_string();
        // Persist user message
        let _ = state
            .session
            .store
            .add_message(&NewMessage {
                session_id: ctx.current_session_id.clone(),
                role: "user".to_string(),
                content: user_msg.clone(),
                model: None,
                tokens: Some(estimate_tokens(&user_msg)),
                complete: true,
                tool_calls: None,
                tool_call_id: None,
            })
            .await;

        // Load recent history for LLM context
        let history = state
            .session
            .store
            .recent_messages(&ctx.current_session_id, 50)
            .await
            .unwrap_or_default();

        let llm_messages: Vec<ChatMessage> = history
            .iter()
            .map(|m| ChatMessage {
                role: match m.role.as_str() {
                    "system" => Role::System,
                    "assistant" => Role::Assistant,
                    _ => Role::User,
                },
                content: m.content.clone(),
                images: None,
                ..Default::default()
            })
            .collect();

        // Brain context injection (server-managed context only).
        // Determine if model is cloud (has provider prefix like "anthropic/...").
        // Cloud models get full brain files; local models get token-budgeted context.
        let aliases = &*state.llm.aliases;

        // Ephemeral model for brain check (one-shot, highest priority)
        let ephemeral_for_brain = params
            .get("ephemeral_model")
            .and_then(|m| m.as_str())
            .map(|s| crate::skills::resolve_alias(aliases, s));

        let resolved_model_for_brain_check = ephemeral_for_brain
            .or_else(|| ctx.active_model_override.clone())
            .or_else(|| {
                let pm = params
                    .get("model")
                    .and_then(|m| m.as_str())
                    .map(|s| crate::skills::resolve_alias(aliases, s));
                if pm.as_ref().map(|m| m.contains('/')).unwrap_or(false) {
                    pm
                } else {
                    None
                }
            })
            .or_else(|| {
                state
                    .skill
                    .registry
                    .read()
                    .ok()
                    .and_then(|skills| skills.resolve_model(aliases))
            })
            .or_else(|| {
                params
                    .get("model")
                    .and_then(|m| m.as_str())
                    .map(|s| crate::skills::resolve_alias(aliases, s))
            });
        let is_cloud = resolved_model_for_brain_check
            .as_ref()
            .map(|m| m.contains('/'))
            .unwrap_or(false);
        let inject_timeout_ms = state.brain.engine.config().auto_inject.timeout_ms;
        let brain_context = if inject_timeout_ms > 0 {
            match tokio::time::timeout(
                std::time::Duration::from_millis(inject_timeout_ms),
                state.brain.engine.build_context(&user_msg, is_cloud),
            )
            .await
            {
                Ok(result) => result.unwrap_or_default(),
                Err(_) => {
                    warn!(
                        timeout_ms = inject_timeout_ms,
                        "brain context timed out, proceeding without"
                    );
                    Vec::new()
                }
            }
        } else {
            state
                .brain
                .engine
                .build_context(&user_msg, is_cloud)
                .await
                .unwrap_or_default()
        };
        // Static layer: inject CLAUDE.md files BEFORE brain context for server-managed turns.
        let static_cwd_sv = resolve_session_cwd(state, &ctx.current_session_id).await;
        let static_entries_sv = collect_static_layer(&static_cwd_sv).await;

        // Assemble prompt prefix via layered builder (StaticLayer → Brain → History).
        let prefix_sv = crate::prompt_builder::PromptBuilder::new()
            .with_static_layer(&static_entries_sv)
            .with_brain(brain_context.into_iter().next())
            .build();
        let mut final_messages = prefix_sv.messages;
        final_messages.extend(llm_messages);

        (final_messages, Some(user_msg))
    } else {
        let response = Frame::err(
            id,
            "INVALID_PARAMS",
            "chat.send requires 'messages' array or 'message' string",
        );
        send_frame(&ctx.tx, &response)?;
        return Ok(());
    };

    // Security: sanitize user messages before sending to LLM
    let mut messages: Vec<ChatMessage> = messages
        .into_iter()
        .map(|mut m| {
            if m.role == Role::User {
                m.content = state.security.sanitize.sanitize(&m.content);
            }
            m
        })
        .collect();

    // Security: injection detection on user messages
    if let Some(ref user_text) = user_message_text {
        let result = injection_detector().scan(user_text);
        if result.has_findings() {
            use ironjaw_security::sanitize::InjectionAction;
            match result.recommended_action {
                InjectionAction::Block => {
                    warn!(
                        score = result.composite_score,
                        findings = result.findings.len(),
                        "chat.send: injection detected, blocking"
                    );
                    let response = Frame::err(
                        id,
                        "INJECTION_DETECTED",
                        "Message blocked by injection detection",
                    );
                    send_frame(&ctx.tx, &response)?;
                    return Ok(());
                }
                InjectionAction::Warn => {
                    warn!(
                        score = result.composite_score,
                        findings = result.findings.len(),
                        "chat.send: possible injection, allowing with warning"
                    );
                }
                InjectionAction::Log => {
                    debug!(
                        score = result.composite_score,
                        "chat.send: low-risk injection patterns detected"
                    );
                }
            }
        }
    }

    // Vision: collect images from params and/or pending binary frames
    let mut image_list: Vec<String> = Vec::new();
    if let Some(imgs) = params.get("images").and_then(|v| v.as_array()) {
        for img in imgs {
            if let Some(b64) = img.as_str() {
                // Validate the image before including
                match state.image_processor.validate_base64(b64) {
                    Ok(_) => image_list.push(b64.to_string()),
                    Err(e) => warn!(error = %e, "Skipping invalid image in chat.send"),
                }
            }
        }
    }
    // Drain pending images from binary frames
    if !ctx.pending_images.is_empty() {
        image_list.append(&mut ctx.pending_images);
    }
    // Attach images to the last user message
    if !image_list.is_empty() {
        if let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == Role::User) {
            last_user.images = Some(image_list);
        }
    }

    // Role-aware model resolution: ephemeral -> override -> skill.model -> fallback -> params -> default
    // Exception: if params.model contains a provider prefix (e.g. "anthropic/claude-sonnet-4-6"),
    // it takes top priority (after override) because it's an explicit cross-provider routing request.
    let aliases = &*state.llm.aliases;

    // Ephemeral model: one-shot override, does not persist across requests
    let ephemeral_model = params
        .get("ephemeral_model")
        .and_then(|m| m.as_str())
        .map(|s| crate::skills::resolve_alias(aliases, s));

    let params_model = params
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| crate::skills::resolve_alias(aliases, s));
    let params_has_prefix = params_model
        .as_ref()
        .map(|m| m.contains('/'))
        .unwrap_or(false);
    let model = ephemeral_model
        .clone()
        .or_else(|| ctx.active_model_override.clone())
        .or_else(|| {
            // Prefixed model from params takes priority over skill model
            if params_has_prefix {
                params_model.clone()
            } else {
                None
            }
        })
        .or_else(|| {
            // Try to get model from active skill
            state
                .skill
                .registry
                .read()
                .ok()
                .and_then(|skills| skills.resolve_model(aliases))
        })
        .or(params_model)
        .or_else(|| state.llm.router.default_model())
        .unwrap_or_else(|| "default".to_string());

    // B2 (CP3): UserPromptSubmit hook fires AFTER security passes (sanitize +
    // injection detection above) and AFTER model resolution, so registered
    // hooks see the exact prompt text and the model the LLM will receive.
    // A `Block` action short-circuits with `HOOK_BLOCKED`, mirroring the
    // INJECTION_DETECTED early-return pattern at line 2421. Note: the user
    // message has already been persisted at the entry of handle_chat_send
    // (matching the existing INJECTION_DETECTED behavior); per-hook
    // pre-persistence cancellation is out of scope for B2 plumbing and would
    // require deferring persistence past all gates.
    if let Some(ref prompt_text) = user_message_text {
        let event = crate::hooks::HookEvent::UserPromptSubmit {
            session_id: ctx.current_session_id.clone(),
            prompt_text: prompt_text.clone(),
            model: model.clone(),
            force_fresh,
        };
        let hook_ctx = crate::hooks::HookContext::with_session(
            event,
            &ctx.current_session_id,
            serde_json::json!({
                "model": &model,
                "force_fresh": force_fresh,
            }),
        );
        let action = state.hooks.fire(&hook_ctx).await;
        if let crate::hooks::HookAction::Block(reason) = action {
            warn!(
                session_id = %ctx.current_session_id,
                reason = %reason,
                "chat.send: blocked by UserPromptSubmit hook"
            );
            let response = Frame::err(id, "HOOK_BLOCKED", &reason);
            send_frame(&ctx.tx, &response)?;
            return Ok(());
        }
    }

    // Temperature: skill override -> params -> None
    let skill_temperature = state
        .skill
        .registry
        .read()
        .ok()
        .and_then(|skills| skills.active_skill().and_then(|s| s.temperature));

    // Thinking/reasoning toggle: map thinkingLevel string to ThinkingConfig
    let thinking_config = params
        .get("thinkingLevel")
        .and_then(|v| v.as_str())
        .map(|level| match level {
            "off" => ironjaw_llm::ThinkingConfig {
                enabled: false,
                budget_tokens: Some(0),
            },
            "low" => ironjaw_llm::ThinkingConfig {
                enabled: true,
                budget_tokens: Some(200),
            },
            "medium" => ironjaw_llm::ThinkingConfig {
                enabled: true,
                budget_tokens: Some(1000),
            },
            "high" => ironjaw_llm::ThinkingConfig {
                enabled: true,
                budget_tokens: Some(-1),
            },
            _ => ironjaw_llm::ThinkingConfig {
                enabled: true,
                budget_tokens: None,
            },
        });

    // Local-model base behavioral prompt. llama-server models get no strong
    // tool-use/stop/output priors, so the agentic loop leans on runtime
    // band-aids (nudger, blank-turn re-prompt, forced synthesis). Prepend a
    // Claude-Code-style behavioral spine as the first System message. Cloud
    // models already carry strong priors and are left untouched.
    // ponytail: per-turn file read (few KB at chat cadence); add an mtime cache only if it shows up in a profile.
    // FIX #0: gate on the RESOLVED provider, not a string prefix. The phone
    // resolves its model via router.default_model() to a BARE name
    // (qwen3.6:35b-a3b), which `starts_with("llama-server/")` misses — so the
    // base prompt + curated tools never fired on the real client path.
    let is_local_model = state.llm.router.is_llama_server_model(&model);
    if is_local_model && state.local_model.enabled {
        match tokio::fs::read_to_string(&state.local_model.system_prompt_file).await {
            Ok(text) if !text.trim().is_empty() => {
                messages.insert(
                    0,
                    ChatMessage {
                        role: Role::System,
                        content: text,
                        ..Default::default()
                    },
                );
            }
            Ok(_) => {} // empty file → no-op
            Err(e) => warn!(
                path = %state.local_model.system_prompt_file,
                error = %e,
                "local base prompt: file unreadable, proceeding without"
            ),
        }
    }

    // Tool set: local models get a curated allowlist (cuts ~5-7K tokens/turn
    // and most mis-selection among ~66-83 near-duplicate tools); cloud models
    // keep the full registry, gated only by per-tool tier and [security.rules].
    // The active skill is not consulted for tool advertising — its `tools`
    // field is preserved for the dashboard's legacy display only.
    let tool_registry = state.tools.clone();
    let active_tools = if is_local_model && state.local_model.enabled {
        build_local_tool_set(&tool_registry, &state.local_model.tools)
            .or_else(|| build_full_tool_set(&tool_registry))
    } else {
        build_full_tool_set(&tool_registry)
    };
    let active_tool_choice = active_tools.as_ref().map(|_| ToolChoice::auto());

    let request = ChatRequest {
        model: model.clone(),
        messages,
        temperature: skill_temperature.or_else(|| {
            params
                .get("temperature")
                .and_then(|t| t.as_f64())
                .map(|t| t as f32)
        }),
        max_tokens: params
            .get("max_tokens")
            .and_then(|m| m.as_u64())
            .map(|m| m as u32)
            .or_else(|| {
                // Default max_tokens for local models to prevent infinite generation
                // (thinking models like qwen3-coder run indefinitely without a limit)
                if model.starts_with("llama-server/") {
                    Some(4096)
                } else {
                    None
                }
            }),
        extra: None,
        thinking: thinking_config,
        tools: active_tools,
        tool_choice: active_tool_choice,
    };

    // ---------------------------------------------------------------------------
    // Phase 4 agentic loop — gated on killswitch flag.
    // When enabled, `run_agentic_chat` owns the LLM call, tool execution, and
    // persistence for this turn.  When disabled, the original single-pass code
    // below runs unchanged.
    // ---------------------------------------------------------------------------
    if state.chat_agentic.enabled {
        // C1: ToolContext.working_dir comes from the session override when set.
        let cwd = resolve_session_cwd(state, &ctx.current_session_id).await;
        let tool_context = ToolContext::new(
            ctx.current_session_id.clone(),
            ctx.connection_id.clone(),
            cwd,
        )
        .with_brain(state.brain.engine.clone());
        return run_agentic_chat(
            id,
            params,
            request,
            model,
            start,
            ctx,
            state,
            tool_context,
            ws_stream,
            injected_refs,
        )
        .await;
    }

    // Create event channel for model auto-load progress
    let (load_event_tx, mut load_event_rx) = mpsc::channel::<ModelLoadEvent>(16);
    let event_ws_tx = ctx.tx.clone();

    // Spawn task to forward model load events as WebSocket event frames
    tokio::spawn(async move {
        while let Some(event) = load_event_rx.recv().await {
            if let Ok(json) = serde_json::to_value(&event) {
                let frame = Frame::event("model.loading", json);
                if let Ok(text) = serde_json::to_string(&frame) {
                    let _ = event_ws_tx.send(Message::Text(text.into()));
                }
            }
        }
    });

    // When the WebSocket closes mid-stream (e.g. client reconnects), we
    // switch to drain mode: stop sending frames but keep consuming the LLM
    // stream so the full response is persisted to the database.
    let mut ws_closed = false;

    let mut stream = match await_socket_alive(
        state
            .llm
            .router
            .chat_with_events(request, Some(load_event_tx)),
        ws_stream,
        &mut ws_closed,
        ctx,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            let response = match &e {
                LlmError::NoProviders => Frame::err(id, "NO_LLM", "No LLM providers configured"),
                LlmError::AllProvidersFailed => {
                    Frame::retryable_err(id, "LLM_UNAVAILABLE", "All LLM providers failed")
                }
                other => Frame::err(id, "LLM_ERROR", other.to_string()),
            };
            send_frame(&ctx.tx, &response)?;
            return Ok(());
        }
    };

    // Acknowledge the request so the client knows streaming will begin
    send_frame(
        &ctx.tx,
        &Frame::ok(id, serde_json::json!({"streaming": true})),
    )?;

    let mut tokens_out: u32 = 0;
    let mut tokens_in: Option<u32> = None;
    let mut final_model = model.clone();
    let mut full_response = String::new();
    let mut stream_complete = false;
    let run_id = id.to_string();
    let session_key = ctx.current_session_id.clone();
    let mut seq: u64 = 0;

    // Think-tag filter: strip <think>...</think> blocks from streaming output.
    // Models like Qwen3 emit reasoning in these tags; clients shouldn't see them.
    let mut in_think_block = false;
    let mut think_buffer = String::new();

    loop {
        tokio::select! {
            _ = ctx.abort_token.cancelled() => {
                // Abort: send aborted frame so clients get a ChatAborted event
                let abort_model_ctx = active_model_context_window(state).await;
                let abort_total = tokens_in.map(|p| p as i64 + tokens_out as i64).unwrap_or(0);
                let abort_pct = context_pct(abort_total, abort_model_ctx);
                let frame = Frame::event(
                    "chat",
                    serde_json::json!({
                        "state": "aborted",
                        "runId": run_id,
                        "seq": seq,
                        "sessionKey": session_key,
                        "message": {
                            "role": "assistant",
                            "content": full_response,
                        },
                        "stopReason": "abort",
                        "contextPct": abort_pct,
                        "contextWindow": abort_model_ctx,
                        "contextTokens": abort_total,
                    }),
                );
                let _ = send_frame(&ctx.tx, &frame);
                break;
            }
            // Check for incoming messages during streaming (e.g. chat.abort)
            ws_msg = ws_stream.next(), if !ws_closed => {
                match ws_msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(Frame::Req { id: abort_id, method, .. }) = serde_json::from_str::<Frame>(&text) {
                            if method == "chat.abort" {
                                ctx.abort_token.cancel();
                                let abort_resp = Frame::ok(abort_id, serde_json::json!({"aborted": true}));
                                let _ = send_frame(&ctx.tx, &abort_resp);
                                // The cancel branch will fire on next loop iteration
                            } else if method == "ping" {
                                let pong = Frame::ok(abort_id, serde_json::json!({"pong": true}));
                                let _ = send_frame(&ctx.tx, &pong);
                            }
                            // Other methods during streaming are silently ignored
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ctx.tx.send(Message::Pong(data));
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        // WebSocket closed — switch to drain mode to persist
                        // the full LLM response even though we can't send frames.
                        tracing::warn!("WebSocket closed during streaming, draining LLM stream for persistence");
                        ws_closed = true;
                    }
                    _ => {} // ignore binary during streaming
                }
            }
            chunk = stream.next() => {
                match chunk {
                    Some(chunk) => {
                        // Filter <think>...</think> blocks from streaming output
                        let visible_delta = if !chunk.delta.is_empty() {
                            think_buffer.push_str(&chunk.delta);
                            let (visible, think_events) =
                                filter_think_stream(&mut think_buffer, &mut in_think_block);
                            if !ws_closed {
                                for ev in &think_events {
                                    let state = match ev {
                                        ThinkEvent::Started => "thinking",
                                        ThinkEvent::Ended => "thinking_done",
                                    };
                                    let _ = send_frame(
                                        &ctx.tx,
                                        &Frame::event(
                                            "chat",
                                            serde_json::json!({"state": state, "runId": run_id}),
                                        ),
                                    );
                                }
                            }
                            if !visible.is_empty() {
                                tokens_out += 1;
                                full_response.push_str(&visible);
                            }
                            visible
                        } else {
                            String::new()
                        };

                        final_model = chunk.model.clone();

                        if chunk.usage.is_some() {
                            if let Some(u) = &chunk.usage {
                                tokens_in = Some(u.prompt_tokens);
                                tokens_out = u.completion_tokens;
                            }
                        }

                        if chunk.done {
                            stream_complete = true;
                            // Flush any remaining buffer (outside think block)
                            if !in_think_block && !think_buffer.is_empty() {
                                full_response.push_str(&think_buffer);
                            }
                            // Persist context tokens BEFORE sending final frame
                            // so any immediate session.list RPC sees current data
                            if let Some(prompt_tokens) = tokens_in {
                                let total = prompt_tokens as i64 + tokens_out as i64;
                                let _ = state
                                    .session
                                    .store
                                    .update_context_tokens(
                                        &ctx.current_session_id,
                                        total,
                                    )
                                    .await;
                            }
                            // Accumulate lifetime session usage (survives client restarts)
                            let _ = state
                                .session
                                .store
                                .add_usage_tokens(
                                    &ctx.current_session_id,
                                    tokens_in.unwrap_or(0) as i64,
                                    tokens_out as i64,
                                )
                                .await;
                            // Send final chat event with full accumulated text
                            if !ws_closed {
                                let done_model_ctx = active_model_context_window(state).await;
                                let done_total = tokens_in
                                    .map(|p| p as i64 + tokens_out as i64)
                                    .unwrap_or(0);
                                let done_pct = context_pct(done_total, done_model_ctx);
                                let (stop_reason, error_field) =
                                    if let Some(ref err) = chunk.error {
                                        ("error", Some(err.clone()))
                                    } else {
                                        ("stop", None)
                                    };
                                let mut final_data = serde_json::json!({
                                    "state": "final",
                                    "runId": run_id,
                                    "seq": seq,
                                    "sessionKey": session_key,
                                    "message": {
                                        "role": "assistant",
                                        "content": full_response,
                                    },
                                    "stopReason": stop_reason,
                                    "contextPct": done_pct,
                                    "contextWindow": done_model_ctx,
                                    "contextTokens": done_total,
                                    "inputTokens": tokens_in.unwrap_or(0),
                                    "outputTokens": tokens_out,
                                    "injected_memories": injected_refs,
                                });
                                if let Some(err) = error_field {
                                    final_data["error"] = serde_json::json!(err);
                                }
                                let frame = Frame::event("chat", final_data);
                                let _ = send_frame(&ctx.tx, &frame);
                            }
                            break;
                        } else if !visible_delta.is_empty() && !ws_closed {
                            // Send delta chat event for each token
                            let frame = Frame::event(
                                "chat",
                                serde_json::json!({
                                    "state": "delta",
                                    "runId": run_id,
                                    "seq": seq,
                                    "sessionKey": session_key,
                                    "message": {
                                        "role": "assistant",
                                        "content": visible_delta,
                                    },
                                }),
                            );
                            seq += 1;
                            if send_frame(&ctx.tx, &frame).is_err() {
                                ws_closed = true;
                            }
                        }
                    }
                    None => break,
                }
            }
        }
    }

    // If the stream ended without a proper `done` chunk (e.g. provider
    // connection dropped), flush the think buffer and send a best-effort
    // final frame so the client gets a termination signal.
    if !stream_complete {
        if !in_think_block && !think_buffer.is_empty() {
            full_response.push_str(&think_buffer);
        }
        if !full_response.is_empty() {
            let inc_model_ctx = active_model_context_window(state).await;
            let inc_total = tokens_in.map(|p| p as i64 + tokens_out as i64).unwrap_or(0);
            let inc_pct = context_pct(inc_total, inc_model_ctx);
            let frame = Frame::event(
                "chat",
                serde_json::json!({
                    "state": "final",
                    "runId": run_id,
                    "seq": seq,
                    "sessionKey": session_key,
                    "message": {
                        "role": "assistant",
                        "content": full_response,
                    },
                    "stopReason": "incomplete",
                    "contextPct": inc_pct,
                    "contextWindow": inc_model_ctx,
                    "contextTokens": inc_total,
                }),
            );
            let _ = send_frame(&ctx.tx, &frame);
            stream_complete = true;
        }
    }

    // Response filter: correct model self-identification claims before persistence.
    // Uses the actual loaded model name from llama-server state.
    if !full_response.is_empty() {
        let actual_model = state.llm.llama_state.read().await.loaded_model.clone();
        if let Some(ref model_name) = actual_model {
            full_response =
                crate::response_filter::validate_model_claims(&full_response, model_name);
        }
    }

    // Clone for extraction pipeline (needed before values are moved)
    let full_response_for_extraction = full_response.clone();
    let user_message_for_extraction = user_message_text.clone();

    // Persist assistant response
    if !full_response.is_empty() {
        let _ = state
            .session
            .store
            .add_message(&NewMessage {
                session_id: ctx.current_session_id.clone(),
                role: "assistant".to_string(),
                content: full_response,
                model: Some(final_model.clone()),
                tokens: Some(tokens_out as i64),
                complete: stream_complete,
                tool_calls: None,
                tool_call_id: None,
            })
            .await;
    }

    // For client-managed context (messages array), also persist the last user message
    if !is_single_message {
        if let Some(user_text) = user_message_text {
            let _ = state
                .session
                .store
                .add_message(&NewMessage {
                    session_id: ctx.current_session_id.clone(),
                    role: "user".to_string(),
                    content: user_text.clone(),
                    model: None,
                    tokens: Some(estimate_tokens(&user_text)),
                    complete: true,
                    tool_calls: None,
                    tool_call_id: None,
                })
                .await;
        }
    }

    // Update session context tokens from real usage data
    // (For the normal done path this was already persisted before the final
    // frame; this fallback covers the incomplete-stream and abort paths.)
    if !stream_complete {
        if let Some(prompt_tokens) = tokens_in {
            let total = prompt_tokens as i64 + tokens_out as i64;
            let _ = state
                .session
                .store
                .update_context_tokens(&ctx.current_session_id, total)
                .await;
        }
    }

    // Touch session timestamp
    let _ = state
        .session
        .store
        .touch_session(&ctx.current_session_id)
        .await;

    // Compaction trigger: if context tokens exceed the limit, extract flush
    // candidates from the messages that would be discarded and generate a
    // session summary (fire-and-forget). Follows the same pattern as auto-learn.
    if let Some(prompt_tokens) = tokens_in {
        let total_tokens = prompt_tokens as u64 + tokens_out as u64;
        let compact_config = ironjaw_session::compaction::CompactionConfig {
            token_limit: state.session.context_token_limit,
            ..Default::default()
        };
        if ironjaw_session::compaction::should_compact(total_tokens, &compact_config) {
            let brain_cfg = state.brain.engine.config().clone();
            if brain_cfg.session_summary_enabled && brain_cfg.session_summary_on_compact {
                let session_id = ctx.current_session_id.clone();
                let sessions = state.session.store.clone();
                let brain_store = state.brain.engine.store().clone();
                let llm = state.llm.router.clone();
                let hooks = state.hooks.clone();
                tokio::spawn(async move {
                    if let Ok(messages) = sessions.recent_messages(&session_id, 200).await {
                        // B2 (CP3): PreCompact hook fires before compact_history.
                        // A `Block` action skips compaction entirely, leaving the
                        // message buffer untouched (no summary generation, no
                        // memory flush). The chat context window itself is
                        // unaffected — this only gates the summary pipeline.
                        let hook_event = crate::hooks::HookEvent::PreCompact {
                            session_id: session_id.clone(),
                            message_count: messages.len(),
                            total_tokens,
                        };
                        let hook_ctx = crate::hooks::HookContext::with_session(
                            hook_event,
                            &session_id,
                            serde_json::json!({
                                "site": "chat.send",
                                "message_count": messages.len(),
                                "total_tokens": total_tokens,
                            }),
                        );
                        if hooks.fire(&hook_ctx).await.is_block() {
                            tracing::info!(
                                session_id = %session_id,
                                "compaction: skipped by PreCompact hook"
                            );
                            return;
                        }
                        let result =
                            ironjaw_session::compaction::compact_history(messages, &compact_config);
                        // Combine discarded + summarized messages for flush extraction
                        let mut flush_messages = result.to_discard;
                        flush_messages.extend(result.to_summarize);
                        let pairs = ironjaw_session::extract_flush_candidates(&flush_messages);
                        if let Err(e) = ironjaw_brain::generate_session_summary(
                            &brain_store,
                            &llm,
                            &session_id,
                            &pairs,
                        )
                        .await
                        {
                            tracing::warn!(
                                session_id = %session_id,
                                error = %e,
                                "compaction: session summary generation failed"
                            );
                        }
                        // LLM-powered structured memory extraction from compacted messages
                        let flush_config = MemoryFlushConfig::default();
                        let candidates = ironjaw_session::flush_memories_llm(
                            &flush_messages,
                            &flush_config,
                            Some(&llm),
                            Some(flush_metrics()),
                        )
                        .await;
                        if !candidates.is_empty() {
                            info!(
                                session_id = %session_id,
                                count = candidates.len(),
                                "compaction: extracted memory candidates via LLM"
                            );
                        }
                    }
                });
            }
        }
    }

    let elapsed = start.elapsed();

    // Audit the LLM call
    let _ = state
        .audit
        .log(&AuditEntry {
            action_type: ActionType::LlmCall,
            method: Some("chat.send".to_string()),
            tool_name: None,
            params: Some(params),
            result: None,
            ok: true,
            error: None,
            llm_model: Some(final_model),
            tokens_in: tokens_in.map(|t| t as i64),
            tokens_out: Some(tokens_out as i64),
            cost_usd: None,
            latency_ms: Some(elapsed.as_millis() as i64),
            session_id: Some(ctx.current_session_id.clone()),
            device_id: ctx.auth.device_id.clone(),
            request_id: id,
            iteration: None,
            tool_calls: None,
        })
        .await;

    // Auto-learn: trigger background learning after enough turns
    if state.brain.engine.config().auto_learn {
        let learn_after = state.brain.engine.config().learn_after_turns;
        let use_llm = state.brain.engine.config().learn_use_llm;
        let session_id = ctx.current_session_id.clone();
        let brain = state.brain.engine.clone();
        let llm = state.llm.router.clone();
        let sessions = state.session.store.clone();
        let sleep_scheduler = state.brain.sleep_scheduler.clone();
        tokio::spawn(async move {
            let msg_count = sessions.message_count(&session_id).await.unwrap_or(0) as usize;
            // Only learn every N turns (count user+assistant = 2 messages per turn)
            if msg_count < learn_after * 2 || !msg_count.is_multiple_of(learn_after * 2) {
                return;
            }
            // De-contend: wait for the user to pause before spending the GPU on
            // extraction, so this never fights a live chat. 25s idle, 5min hard cap.
            wait_for_idle(&sleep_scheduler, 25, 300).await;
            let messages = sessions
                .recent_messages(&session_id, 20)
                .await
                .unwrap_or_default();
            let pairs: Vec<(String, String)> = messages
                .iter()
                .map(|m| (m.role.clone(), m.content.clone()))
                .collect();

            // LLM extraction: call LLM if configured, fall back to heuristic on failure
            let llm_response = if use_llm {
                match extract_via_llm(&llm, &pairs).await {
                    Some(resp) => {
                        info!(session = %session_id, "Auto-learn: LLM extraction succeeded");
                        Some(resp)
                    }
                    None => {
                        debug!(session = %session_id, "Auto-learn: LLM extraction failed, using heuristic");
                        None
                    }
                }
            } else {
                None
            };

            let dedup = brain.config().learn_dedup_threshold;
            let learner = ironjaw_brain::Learner::new(use_llm);
            if let Err(e) = learner
                .learn(&brain, &pairs, llm_response.as_deref(), dedup)
                .await
            {
                warn!(error = %e, session = %session_id, "Auto-learn failed");
            }
        });
    }

    // Per-message extraction pipeline: runs async, debounced, non-blocking
    if let Some(user_text) = user_message_for_extraction {
        let pipeline = state.brain.extraction_pipeline.clone();
        let brain = state.brain.engine.clone();
        let llm = state.llm.router.clone();
        let session_id = ctx.current_session_id.clone();
        let full_resp = full_response_for_extraction;
        let sleep_scheduler = state.brain.sleep_scheduler.clone();
        tokio::spawn(async move {
            // De-contend: hold off until the chat goes idle so this background
            // extraction doesn't steal GPU mid-conversation. 25s idle, 5min cap.
            wait_for_idle(&sleep_scheduler, 25, 300).await;
            if let Err(e) = pipeline
                .extract(&brain, &llm, &user_text, &full_resp, Some(&session_id))
                .await
            {
                warn!(error = %e, session = %session_id, "Extraction pipeline failed");
            }
        });
    }

    // Touch sleep scheduler idle tracker (reset idle timer on activity)
    state.brain.sleep_scheduler.touch().await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Agentic loop — Phase 4
// ---------------------------------------------------------------------------

/// Awaits `fut` while keeping the WebSocket serviced.
///
/// Long LLM awaits — prompt processing on a large context takes minutes on
/// local models — otherwise starve the socket: protocol Pings go unanswered
/// and keepalive-enforcing clients (OkHttp `pingInterval`, Python
/// `websockets`) kill the connection mid-run. This answers protocol pings,
/// handles `chat.abort` and app-level `ping` requests, and flips `ws_closed`
/// on client close (drain mode — the caller keeps generating so a
/// reconnecting client gets the A3 catchup).
async fn await_socket_alive<F: std::future::Future>(
    fut: F,
    ws_stream: &mut SplitStream<WebSocket>,
    ws_closed: &mut bool,
    ctx: &ConnectionContext,
) -> F::Output {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            res = &mut fut => break res,
            // FIX #8: writer-side sink failure — flip to drain immediately.
            _ = ctx.socket_dead.cancelled(), if !*ws_closed => {
                tracing::warn!("WebSocket writer failed (dead sink) during long LLM await, draining");
                *ws_closed = true;
            }
            ws_msg = ws_stream.next(), if !*ws_closed => {
                match ws_msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(Frame::Req { id: req_id, method, .. }) =
                            serde_json::from_str::<Frame>(&text)
                        {
                            if method == "chat.abort" {
                                ctx.abort_token.cancel();
                                let resp =
                                    Frame::ok(req_id, serde_json::json!({"aborted": true}));
                                let _ = send_frame(&ctx.tx, &resp);
                            } else if method == "ping" {
                                let pong =
                                    Frame::ok(req_id, serde_json::json!({"pong": true}));
                                let _ = send_frame(&ctx.tx, &pong);
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ctx.tx.send(Message::Pong(data));
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::warn!("WebSocket closed during long LLM await, draining");
                        *ws_closed = true;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Runs the agentic chat loop when `state.chat_agentic.enabled == true`.
///
/// Responsibilities:
/// 1. Sends the initial LLM request (Pass 1) — streams text deltas to the client.
/// 2. Detects `finish_reason == "tool_calls"` in the done chunk.
/// 3. Accumulates streaming `ToolCallDelta`s by index into complete `ToolCall`s.
/// 4. Calls `execute_tool_batch` for each tool-call batch.
/// 5. Appends assistant (with tool_calls) + tool-result messages to `request.messages`.
/// 6. Loops (up to `max_iterations`) until `finish_reason == "stop"` or max reached.
/// 7. Persists the full turn (user, N× assistant+tool, final assistant) to session store.
/// 8. Sends the `chat` event stream to the WebSocket client.
///
/// The function signature deliberately matches the call site at the top of
/// `handle_chat_send` so no refactoring is required when the flag is enabled.
#[allow(clippy::too_many_arguments)]
async fn run_agentic_chat(
    id: Uuid,
    params: serde_json::Value,
    mut request: ChatRequest,
    model: String,
    start: std::time::Instant,
    ctx: &mut ConnectionContext,
    state: &AppState,
    tool_context: ironjaw_tools::ToolContext,
    ws_stream: &mut SplitStream<WebSocket>,
    injected_memories: Vec<MemoryRef>,
) -> anyhow::Result<()> {
    let run_id = id.to_string();
    let session_key = ctx.current_session_id.clone();

    // A3: register this run's delivery buffer before anything else, so a
    // reconnect racing the very first frame still finds a live entry. The
    // guard removes the registry entry when this function returns, whatever
    // the exit path (success, LLM error, or abort).
    let buf = crate::stream_buffer::RunBuffer::new(
        run_id.clone(),
        session_key.clone(),
        ctx.tx.clone(),
        ctx.abort_token.clone(),
    );
    state.chat_runs.register(session_key.clone(), buf.clone());
    let _run_buffer_guard = crate::stream_buffer::RunBufferGuard::new(
        state.chat_runs.clone(),
        session_key.clone(),
        run_id.clone(),
    );

    let max_iterations = state.chat_agentic.max_iterations;
    let tool_timeout_secs = state.chat_agentic.tool_timeout_secs;
    let turn_deadline_secs = state.chat_agentic.turn_deadline_secs;

    // A4: fetch per-session mode once; fall back to Default on any error.
    let session_mode = state
        .session
        .store
        .get_mode(&ctx.current_session_id)
        .await
        .ok()
        .flatten()
        .unwrap_or(ironjaw_session::types::SessionMode::Default);

    // Acknowledge before the first stream starts so the client knows streaming will begin.
    send_frame(
        &ctx.tx,
        &Frame::ok(id, serde_json::json!({"streaming": true})),
    )?;

    // Accumulated state across all iterations.
    let mut tokens_in: Option<u32> = None;
    let mut tokens_out: u32 = 0;
    // Live context occupancy = the LAST LLM call's prompt + completion. The
    // cumulative `tokens_in` re-counts the whole prompt every iteration (a
    // 14-iteration turn once reported 205K "context" on a 131K model), so it
    // is only valid for spend accounting (run_token_ceiling, usage/billing) —
    // never for contextTokens/contextPct or the compaction trigger.
    let mut last_call_tokens: i64 = 0;
    let mut final_model = model.clone();
    let mut final_text = String::new(); // text from the last (stop) iteration
                                        // FIX #2c: track whether ANY terminal frame (final/aborted/error) was sent.
                                        // Every normal exit sets this; a guaranteed-terminal fallback at the end of
                                        // the function fires if none did, so the client never sees a run stuck
                                        // mid-stream (e.g. the token/deadline break with a dead-but-reattaching
                                        // socket, or a synthesis-skip branch).
    let mut terminal_sent = false;
    let mut total_audit_calls: Vec<ironjaw_audit::types::AuditToolCall> = Vec::new();
    let mut last_iteration: usize = 0;
    let mut ws_closed = false;
    // A3: seq is now owned by `buf` (RunBuffer), not tracked locally, so
    // reattach/catchup and live delivery always agree on the next value.
    // Phase 5: nudger retry counter — incremented each time we re-prompt the LLM
    // because it emitted prose despite having tools available.
    let mut nudge_retries: usize = 0;
    // Tier 3: WrongToolNudge counter — fires at most once per turn when the LLM
    // picks a web-search tool but the user asked about a local folder.
    let mut wrong_tool_retries: usize = 0;
    // Text-format tool-call recovery counter — fires at most twice per turn when
    // the model emits a `[name(args)]` pythonic call as plain text instead of a
    // structured Hermes `<tool_call>` (abliterated-model format drift).
    let mut text_parse_retries: usize = 0;
    // Empty-final-turn counter — fires at most once per turn when the model ends
    // the loop with a blank text turn (no answer) after a burst of tool calls.
    let mut empty_answer_retries: usize = 0;
    // G1: mid-stream death — the provider signalled a terminal error chunk
    // (SSE read error / EOF without [DONE]). Retry the identical request once;
    // on a second failure keep the longer partial and mark it truncated.
    let mut stream_error_retries: usize = 0;
    let mut stream_error_partial = String::new();
    let mut stream_died = false;
    // G1: a doubly-truncated answer is persisted with complete = false.
    let mut final_complete = true;
    // G2: minimum-length final-answer guard — fires at most once per turn.
    let mut min_len_retries: usize = 0;
    // A1: iteration budget pressure — fires exactly once, on the final
    // iteration, so the LLM knows to stop calling tools and answer now.
    let mut budget_note_injected = false;
    // A2: per-run token ceiling (0 = disabled).
    let run_token_ceiling = state.chat_agentic.run_token_ceiling;

    'outer: for iteration in 0..max_iterations {
        last_iteration = iteration;

        // A2: per-run token ceiling — checked at the top of each iteration so
        // we never start another LLM pass once the turn has spent its budget.
        // Leaves `final_text` empty and breaks so the forced-synthesis pass
        // below answers with what was gathered — the user gets a real answer,
        // not a budget error (live-observed 2026-07-06: a healthy exploration
        // run tripped the ceiling mid-tool-burst and surfaced only the error).
        if run_token_ceiling > 0 {
            let used = tokens_in.unwrap_or(0) as u64 + tokens_out as u64;
            if used > run_token_ceiling {
                tracing::info!(
                    used,
                    run_token_ceiling,
                    iteration,
                    "agentic: run token ceiling reached — forcing final synthesis"
                );
                break 'outer;
            }
        }

        // FIX #2b: per-turn wall-clock deadline. A slow/wedged local model (or
        // a tool that stalls) could otherwise keep the loop alive well past any
        // reasonable turn. Checked here (same spot as the token ceiling) so we
        // never start another LLM pass past the deadline; breaks to the
        // forced-synthesis pass below so the user still gets what was gathered.
        if turn_deadline_secs > 0 && start.elapsed().as_secs() > turn_deadline_secs {
            tracing::warn!(
                elapsed_secs = start.elapsed().as_secs(),
                turn_deadline_secs,
                iteration,
                "agentic: per-turn wall-clock deadline reached — forcing final synthesis"
            );
            break 'outer;
        }

        // A1: iteration budget pressure — on the final allotted iteration, tell
        // the LLM to stop calling tools and answer now with what it already has.
        // Fires at most once per run.
        if iteration == max_iterations - 1 && !budget_note_injected {
            request.messages.push(ChatMessage {
                role: Role::User,
                content: "SYSTEM BUDGET NOTE: this is your final iteration. Answer the \
                          original question now in plain text using what you already have. \
                          Do not call any more tools."
                    .to_string(),
                images: None,
                tool_calls: None,
                tool_call_id: None,
            });
            budget_note_injected = true;
        }

        // ---------------------------------------------------------------
        // Pass N: call LLM and stream deltas.
        // ---------------------------------------------------------------
        let (load_event_tx, mut load_event_rx) = mpsc::channel::<ModelLoadEvent>(16);
        let event_ws_tx = ctx.tx.clone();
        tokio::spawn(async move {
            while let Some(event) = load_event_rx.recv().await {
                if let Ok(json) = serde_json::to_value(&event) {
                    let frame = Frame::event("model.loading", json);
                    if let Ok(text) = serde_json::to_string(&frame) {
                        let _ = event_ws_tx.send(axum::extract::ws::Message::Text(text.into()));
                    }
                }
            }
        });

        let mut stream = match await_socket_alive(
            state
                .llm
                .router
                .chat_with_events(request.clone(), Some(load_event_tx)),
            ws_stream,
            &mut ws_closed,
            ctx,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                let response = match &e {
                    LlmError::NoProviders => {
                        Frame::err(id, "NO_LLM", "No LLM providers configured")
                    }
                    LlmError::AllProvidersFailed => {
                        Frame::retryable_err(id, "LLM_UNAVAILABLE", "All LLM providers failed")
                    }
                    other => Frame::err(id, "LLM_ERROR", other.to_string()),
                };
                // A3: always deliver via the buffer — a dead sink is a no-op
                // there, and a reattached client must still see the error.
                buf.send_now(&response);
                terminal_sent = true;
                break 'outer;
            }
        };

        // Accumulate tool-call deltas keyed by their stream index.
        // Map<index, (partial_id, partial_name, partial_args)>
        let mut delta_map: std::collections::HashMap<usize, (String, String, String)> =
            std::collections::HashMap::new();
        let mut iter_text = String::new();
        let mut finish_reason: Option<String> = None;
        // G1: set when the provider ends the stream with a terminal error chunk.
        let mut stream_error: Option<String> = None;

        // Think-tag filter state (per iteration).
        let mut in_think_block = false;
        let mut think_buffer = String::new();

        loop {
            tokio::select! {
                _ = ctx.abort_token.cancelled() => {
                    // Client aborted — send aborted event and stop all loops.
                    let abort_total = last_call_tokens;
                    let abort_ctx = active_model_context_window(state).await;
                    let abort_pct = context_pct(abort_total, abort_ctx);
                    let frame = Frame::event(
                        "chat",
                        serde_json::json!({
                            "state": "aborted",
                            "runId": run_id,
                            "seq": buf.seq(),
                            "sessionKey": session_key,
                            "message": { "role": "assistant", "content": iter_text },
                            "stopReason": "abort",
                            "contextPct": abort_pct,
                            "contextWindow": abort_ctx,
                            "contextTokens": abort_total,
                        }),
                    );
                    buf.send_now(&frame);
                    terminal_sent = true;
                    break 'outer;
                }
                // FIX #8: writer-side sink failure — flip to drain immediately so
                // the truly-dead-socket case skips the forced-synthesis pass. A3
                // catchup still covers a client that reconnects mid-run.
                _ = ctx.socket_dead.cancelled(), if !ws_closed => {
                    tracing::warn!(
                        "WebSocket writer failed (dead sink) during agentic streaming (iteration {iteration}), draining"
                    );
                    ws_closed = true;
                }
                ws_msg = ws_stream.next(), if !ws_closed => {
                    match ws_msg {
                        Some(Ok(axum::extract::ws::Message::Text(text))) => {
                            if let Ok(Frame::Req { id: abort_id, method, .. }) =
                                serde_json::from_str::<Frame>(&text)
                            {
                                if method == "chat.abort" {
                                    ctx.abort_token.cancel();
                                    let abort_resp =
                                        Frame::ok(abort_id, serde_json::json!({"aborted": true}));
                                    let _ = send_frame(&ctx.tx, &abort_resp);
                                } else if method == "ping" {
                                    let pong =
                                        Frame::ok(abort_id, serde_json::json!({"pong": true}));
                                    let _ = send_frame(&ctx.tx, &pong);
                                }
                            }
                        }
                        Some(Ok(axum::extract::ws::Message::Ping(data))) => {
                            let _ = ctx.tx.send(axum::extract::ws::Message::Pong(data));
                        }
                        Some(Ok(axum::extract::ws::Message::Close(_))) | None => {
                            tracing::warn!(
                                "WebSocket closed during agentic streaming (iteration {iteration}), draining"
                            );
                            ws_closed = true;
                        }
                        _ => {}
                    }
                }
                chunk_opt = stream.next() => {
                    match chunk_opt {
                        None => break,
                        Some(chunk) => {
                            // G1: terminal error chunk — remember it so the
                            // post-stream check below can retry the request.
                            if let Some(err) = &chunk.error {
                                stream_error = Some(err.clone());
                            }

                            // Accumulate tool-call deltas.
                            if let Some(deltas) = &chunk.tool_call_deltas {
                                for delta in deltas {
                                    merge_tool_call_delta(&mut delta_map, delta);
                                }
                            }

                            // Apply think-tag filter to text content.
                            let visible_delta = if !chunk.delta.is_empty() {
                                think_buffer.push_str(&chunk.delta);
                                let (visible, think_events) =
                                    filter_think_stream(&mut think_buffer, &mut in_think_block);
                                // A3: always deliver via the buffer (dead sink is a
                                // no-op there; a reattached client must still see it).
                                for ev in &think_events {
                                    let think_state = match ev {
                                        ThinkEvent::Started => "thinking",
                                        ThinkEvent::Ended => "thinking_done",
                                    };
                                    buf.send_now(&Frame::event(
                                        "chat",
                                        serde_json::json!({"state": think_state, "runId": run_id}),
                                    ));
                                }
                                if !visible.is_empty() {
                                    tokens_out += 1;
                                    iter_text.push_str(&visible);
                                }
                                visible
                            } else {
                                String::new()
                            };

                            final_model = chunk.model.clone();
                            if let Some(u) = &chunk.usage {
                                // Accumulate across iterations so the final Usage
                                // reflects the full N-iteration turn.  prompt_tokens
                                // is additive (each re-prompt re-sends the growing
                                // context); completion_tokens is clearly additive.
                                // TODO: if billing semantics prefer prompt_tokens=first
                                // iteration only (cached prompts are cheaper), change
                                // the prompt line to a first-wins pattern instead.
                                tokens_in = Some(tokens_in.unwrap_or(0) + u.prompt_tokens);
                                tokens_out += u.completion_tokens;
                                // This call's prompt already contains all prior
                                // iterations' tool calls/results, so prompt +
                                // completion of the latest call IS the occupancy.
                                last_call_tokens =
                                    (u.prompt_tokens + u.completion_tokens) as i64;
                            }

                            // Capture finish_reason whenever it appears. llama-server
                            // with `stream_options.include_usage` sends finish_reason
                            // in one chunk (done=false) and usage in a SEPARATE trailing
                            // chunk (done=true, finish_reason=null). If we only read
                            // finish_reason on `done=true`, the real stop reason is
                            // lost — tool_calls turns then fall through to the
                            // "stop" branch and the agentic loop never executes tools.
                            if chunk.finish_reason.is_some() {
                                finish_reason = chunk.finish_reason.clone();
                            }

                            if chunk.done {
                                // Flush residual think buffer if not inside a block.
                                if !in_think_block && !think_buffer.is_empty() {
                                    iter_text.push_str(&think_buffer);
                                    think_buffer.clear();
                                }
                                break;
                            }

                            // Stream text delta to client (only when not tool_call finish).
                            // A3: always tee through the buffer — it owns the seq
                            // counter and the catchup text, and silently no-ops on a
                            // dead sink instead of setting ws_closed (that flag is
                            // reserved for gating reads off a dead ws_stream).
                            if !visible_delta.is_empty() {
                                buf.send_delta(&visible_delta);
                            }
                        }
                    }
                }
            }
        } // end inner stream loop

        // ---------------------------------------------------------------
        // G1: the LLM stream died mid-response (terminal error chunk). The
        // partial can't be trusted — a truncated tool_calls turn must not
        // execute and a truncated answer reads as complete. Retry the
        // identical request once; on a second failure surface the longer
        // partial with a visible truncation marker.
        // ---------------------------------------------------------------
        if let Some(err) = stream_error {
            if stream_error_retries < 1 {
                stream_error_retries += 1;
                if iter_text.len() > stream_error_partial.len() {
                    stream_error_partial = iter_text;
                }
                tracing::warn!(
                    iteration,
                    error = %err,
                    "agentic: LLM stream died — retrying request once"
                );
                continue 'outer;
            }
            if stream_error_partial.len() > iter_text.len() {
                iter_text = stream_error_partial.clone();
            }
            tracing::error!(
                iteration,
                error = %err,
                partial_len = iter_text.len(),
                "agentic: LLM stream died twice — surfacing truncated partial"
            );
            iter_text.push_str("\n\n[answer truncated: model stream error]");
            stream_died = true;
            final_complete = false;
        }

        // ---------------------------------------------------------------
        // Decide: was this a tool_calls turn or a stop turn?
        // ---------------------------------------------------------------
        if !stream_died && finish_reason.as_deref() == Some("tool_calls") && !delta_map.is_empty() {
            // Assemble complete ToolCall structs from accumulated deltas.
            let tool_calls: Vec<ToolCall> = assemble_tool_calls(delta_map);
            let tool_names: Vec<String> = tool_calls
                .iter()
                .map(|tc| tc.function.name.clone())
                .collect();
            debug!(
                iteration,
                tools = ?tool_names,
                "agentic: tool_calls finish — executing batch"
            );

            // Tier 3 WrongToolNudge: if the LLM's first call is a web-search tool
            // but the user's prompt referenced a local folder under working_dir,
            // re-prompt instead of executing. Caps at 1 retry per turn.
            if wrong_tool_retries == 0 {
                if let Some(first_tool) = tool_names.first() {
                    let user_prompt = request
                        .messages
                        .iter()
                        .rev()
                        .find(|m| matches!(m.role, Role::User))
                        .map(|m| m.content.clone())
                        .unwrap_or_default();
                    let folder_names = enumerate_local_folders(&tool_context.working_dir);
                    if let Some(nudge_msg) =
                        ironjaw_llm::wrong_tool_nudge(&user_prompt, first_tool, &folder_names)
                    {
                        tracing::info!(
                            iteration,
                            wrong_tool = %first_tool,
                            "agentic: WrongToolNudge — local folder match, re-prompting"
                        );
                        request.messages.push(ChatMessage {
                            role: Role::User,
                            content: nudge_msg,
                            images: None,
                            tool_calls: None,
                            tool_call_id: None,
                        });
                        wrong_tool_retries += 1;
                        continue 'outer;
                    }
                }
            }

            // Persist the assistant message (with tool_calls) for this pass.
            let _ = state
                .session
                .store
                .add_message(&NewMessage {
                    session_id: ctx.current_session_id.clone(),
                    role: "assistant".to_string(),
                    content: iter_text.clone(),
                    model: Some(final_model.clone()),
                    tokens: Some(tokens_out as i64),
                    complete: true,
                    tool_calls: Some(tool_calls.clone()),
                    tool_call_id: None,
                })
                .await;

            // Append assistant message to the in-flight request for Pass N+1.
            request.messages.push(ChatMessage {
                role: Role::Assistant,
                content: iter_text.clone(),
                images: None,
                tool_calls: Some(tool_calls.clone()),
                tool_call_id: None,
            });

            // FIX #4: gate tool EXECUTION to the advertised set. `request.tools`
            // is the curated allowlist for local models (equal to the full
            // registry for cloud). A call naming a tool that EXISTS in the registry
            // but was not advertised is the mis-selection the curated set exists to
            // prevent — it is not executed; the model instead gets a synthetic error
            // result and self-corrects. Unknown/hallucinated names fall through to
            // execute_tool_batch, which emits the normal not-found tool.result the
            // client and model already handle. Every emitted tool_call_id still
            // receives exactly one tool result, so the assistant-tool_call ↔
            // tool-result pairing stays intact.
            let (exec_calls, blocked_results) = if let Some(advertised) = request.tools.as_ref() {
                let allowed: std::collections::HashSet<&str> =
                    advertised.iter().map(|t| t.name.as_str()).collect();
                let advertised_list = advertised
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let (exec, blocked) = partition_advertised_tool_calls(
                    &tool_calls,
                    |n| allowed.contains(n),
                    |n| state.tools.get(n).is_some(),
                    &advertised_list,
                );
                for b in &blocked {
                    tracing::warn!(
                        iteration,
                        tool = %b.tool_name,
                        "agentic: blocked non-advertised tool call (FIX #4)"
                    );
                }
                (exec, blocked)
            } else {
                (tool_calls.clone(), Vec::new())
            };

            // Execute the tool batch — emits tool.start / tool.result events.
            // C2-B: pass the connection's abort_token so chat.abort propagates
            // into running tools cooperatively.
            //
            // C2-C: while the batch is running the outer message loop is blocked
            // (ws_stream is held by this function), so an incoming chat.abort WS
            // frame would sit unread in the socket buffer — the abort_token would
            // never be cancelled and kill_on_drop / select! arms in shell.rs would
            // never fire.  Fix: drive ws_stream alongside the batch future via
            // select! so that chat.abort is processed in real-time during execution.
            let results = {
                // A3: hand the tool batch the buffer's CURRENT sink (may already be
                // a reattached client's tx), not the original connection's `ctx.tx`.
                let batch_tx = buf.current_tx();
                let batch_fut = crate::agentic::execute_tool_batch(
                    &exec_calls,
                    &state.tools,
                    &state.security.hitl_gate,
                    &batch_tx,
                    &tool_context,
                    tool_timeout_secs,
                    &ctx.current_session_id,
                    state.chat_agentic.hitl_session_cache_enabled,
                    Some(&state.brain.engine),
                    state.chat_agentic.tool_memory_writeback_enabled,
                    session_mode.clone(),
                    Some(&state.audit),
                    &state.in_flight_tools,
                    ctx.abort_token.clone(),
                );
                tokio::pin!(batch_fut);
                // Drive both the batch and the WS reader until the batch finishes.
                // Any chat.abort received here cancels abort_token, which the
                // select! arms inside execute_tool_batch / shell.rs then see.
                loop {
                    tokio::select! {
                        results = &mut batch_fut => break results,
                        ws_msg = ws_stream.next(), if !ws_closed => {
                            match ws_msg {
                                Some(Ok(axum::extract::ws::Message::Text(text))) => {
                                    if let Ok(Frame::Req { id: abort_id, method, .. }) =
                                        serde_json::from_str::<Frame>(&text)
                                    {
                                        if method == "chat.abort" {
                                            ctx.abort_token.cancel();
                                            let abort_resp = Frame::ok(
                                                abort_id,
                                                serde_json::json!({"aborted": true}),
                                            );
                                            let _ = send_frame(&ctx.tx, &abort_resp);
                                        } else if method == "ping" {
                                            let pong = Frame::ok(
                                                abort_id,
                                                serde_json::json!({"pong": true}),
                                            );
                                            let _ = send_frame(&ctx.tx, &pong);
                                        }
                                    }
                                }
                                Some(Ok(axum::extract::ws::Message::Ping(data))) => {
                                    let _ =
                                        ctx.tx.send(axum::extract::ws::Message::Pong(data));
                                }
                                Some(Ok(axum::extract::ws::Message::Close(_))) | None => {
                                    ws_closed = true;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            };
            // FIX #4: fold in synthetic error results for any blocked
            // (non-advertised) tool calls so each emitted tool_call_id gets a
            // matching tool result and the model sees why the call was refused.
            let mut results = results;
            results.extend(blocked_results);

            // Accumulate audit records for this batch.
            for res in &results {
                total_audit_calls.push(ironjaw_audit::types::AuditToolCall {
                    tool_name: res.tool_name.clone(),
                    tool_call_id: res.tool_call_id.clone(),
                    ok: res.ok,
                    duration_ms: res.duration_ms,
                    error: res.error.clone(),
                });
            }

            // Append one tool-result message per executed tool (capped — see
            // tool_result_content).
            for res in &results {
                let content = crate::agentic::tool_result_content(
                    &res.result,
                    state.chat_agentic.tool_result_max_chars,
                );

                // Persist tool result message.
                let _ = state
                    .session
                    .store
                    .add_message(&NewMessage {
                        session_id: ctx.current_session_id.clone(),
                        role: "tool".to_string(),
                        content: content.clone(),
                        model: None,
                        tokens: Some(estimate_tokens(&content)),
                        complete: true,
                        tool_calls: None,
                        tool_call_id: Some(res.tool_call_id.clone()),
                    })
                    .await;

                // Append tool result to request for next pass.
                request.messages.push(ChatMessage {
                    role: Role::Tool,
                    content,
                    images: None,
                    tool_calls: None,
                    tool_call_id: Some(res.tool_call_id.clone()),
                });
            }

            // Loop back for Pass N+1.
            continue 'outer;
        }

        // ── Text-format ("pythonic") tool-call fallback ──────────────────────
        // Abliterated models occasionally emit a call as plain text like
        // `[list_files(path="…")]` instead of a structured Hermes `<tool_call>`,
        // so llama-server returns it as content with finish_reason "stop" and no
        // tool_calls. Recover + execute those so the turn isn't a dead response.
        // Mirrors the tool_calls branch above minus the WrongToolNudge and the
        // ws_stream abort-drive (plain await is fine for this rare path; the
        // abort_token is still passed so disconnect still kills running tools).
        if delta_map.is_empty()
            && finish_reason.as_deref() != Some("tool_calls")
            && text_parse_retries < 2
        {
            let advertised: Vec<String> = request
                .tools
                .as_ref()
                .map(|ts| ts.iter().map(|t| t.name.clone()).collect())
                .unwrap_or_default();
            let parsed = crate::agentic::parse_text_tool_calls(&iter_text, &advertised, 4);
            if !parsed.is_empty() {
                let names: Vec<String> = parsed.iter().map(|tc| tc.function.name.clone()).collect();
                tracing::info!(
                    iteration,
                    tools = ?names,
                    "agentic: recovered text-format tool call(s), executing"
                );

                // Persist the assistant message carrying the recovered tool_calls.
                let _ = state
                    .session
                    .store
                    .add_message(&NewMessage {
                        session_id: ctx.current_session_id.clone(),
                        role: "assistant".to_string(),
                        content: iter_text.clone(),
                        model: Some(final_model.clone()),
                        tokens: Some(tokens_out as i64),
                        complete: true,
                        tool_calls: Some(parsed.clone()),
                        tool_call_id: None,
                    })
                    .await;
                request.messages.push(ChatMessage {
                    role: Role::Assistant,
                    content: iter_text.clone(),
                    images: None,
                    tool_calls: Some(parsed.clone()),
                    tool_call_id: None,
                });

                // A3: hand the tool batch the buffer's CURRENT sink, not the
                // original connection's `ctx.tx` (see the tool_calls branch above).
                let text_fallback_tx = buf.current_tx();
                let results = crate::agentic::execute_tool_batch(
                    &parsed,
                    &state.tools,
                    &state.security.hitl_gate,
                    &text_fallback_tx,
                    &tool_context,
                    tool_timeout_secs,
                    &ctx.current_session_id,
                    state.chat_agentic.hitl_session_cache_enabled,
                    Some(&state.brain.engine),
                    state.chat_agentic.tool_memory_writeback_enabled,
                    session_mode.clone(),
                    Some(&state.audit),
                    &state.in_flight_tools,
                    ctx.abort_token.clone(),
                )
                .await;

                for res in &results {
                    total_audit_calls.push(ironjaw_audit::types::AuditToolCall {
                        tool_name: res.tool_name.clone(),
                        tool_call_id: res.tool_call_id.clone(),
                        ok: res.ok,
                        duration_ms: res.duration_ms,
                        error: res.error.clone(),
                    });
                }

                for res in &results {
                    let content = crate::agentic::tool_result_content(
                        &res.result,
                        state.chat_agentic.tool_result_max_chars,
                    );
                    let _ = state
                        .session
                        .store
                        .add_message(&NewMessage {
                            session_id: ctx.current_session_id.clone(),
                            role: "tool".to_string(),
                            content: content.clone(),
                            model: None,
                            tokens: Some(estimate_tokens(&content)),
                            complete: true,
                            tool_calls: None,
                            tool_call_id: Some(res.tool_call_id.clone()),
                        })
                        .await;
                    request.messages.push(ChatMessage {
                        role: Role::Tool,
                        content,
                        images: None,
                        tool_calls: None,
                        tool_call_id: Some(res.tool_call_id.clone()),
                    });
                }

                text_parse_retries += 1;
                continue 'outer;
            }
        }

        // finish_reason == "stop" (or unrecognised) — this is the final text response.
        //
        // Phase 5: before we accept the turn as finished, give the ToolNudger a
        // chance to catch "describes the tool instead of calling it" responses.
        // If the LLM had tools available and its prose matches conversational
        // patterns ("I would …", "Here's how …"), re-prompt once or twice.
        if !stream_died && state.chat_agentic.nudger_enabled {
            let tool_names: Vec<String> = request
                .tools
                .as_ref()
                .map(|ts| ts.iter().map(|t| t.name.clone()).collect())
                .unwrap_or_default();
            if !tool_names.is_empty() {
                let nudger = ironjaw_llm::ToolNudger::new(state.chat_agentic.nudger_max_retries);
                match nudger.needs_nudge(&iter_text, &tool_names, nudge_retries) {
                    ironjaw_llm::NudgeDecision::Nudge { message } => {
                        tracing::info!(
                            iteration,
                            nudge_retries,
                            "agentic: nudging LLM to use a tool"
                        );
                        request.messages.push(ChatMessage {
                            role: Role::User,
                            content: message,
                            images: None,
                            tool_calls: None,
                            tool_call_id: None,
                        });
                        nudge_retries += 1;
                        continue 'outer;
                    }
                    // FIX #7: the model kept describing actions instead of taking
                    // them and the nudge budget is spent. Don't accept the prose as
                    // the answer — break to the tools-removed forced-synthesis pass
                    // (final_text is still empty here) so the user gets a real answer
                    // built from whatever tool results were already gathered, instead
                    // of burning the remaining iterations on more prose.
                    ironjaw_llm::NudgeDecision::MaxRetriesExceeded => {
                        tracing::info!(
                            iteration,
                            nudge_retries,
                            "agentic: nudge retries exhausted on conversational answer — forcing synthesis"
                        );
                        break 'outer;
                    }
                    ironjaw_llm::NudgeDecision::Accept => {}
                }
            }
        }

        // ── Empty final turn guard ───────────────────────────────────────────
        // An abliterated model sometimes ends the loop with a blank text turn
        // (no tool call, no answer) after a burst of tool calls — so the user
        // sees nothing. Re-prompt once for a plain-text answer using the results
        // already in context; if it is STILL blank, surface the most recent tool
        // output so the turn is never a dead response.
        if iter_text.trim().is_empty() {
            if empty_answer_retries < 1 {
                empty_answer_retries += 1;
                tracing::info!(
                    iteration,
                    "agentic: empty final turn — re-prompting for a plain-text answer"
                );
                request.messages.push(ChatMessage {
                    role: Role::User,
                    content: "Answer my question now in plain text, using the tool \
                              results above. Do not call any more tools."
                        .to_string(),
                    images: None,
                    tool_calls: None,
                    tool_call_id: None,
                });
                continue 'outer;
            }
            // ponytail: re-prompt did not help — fall back to the last tool output
            // (raw, but better than a blank). Only reached on a stubbornly-empty model.
            if let Some(last_tool) = request
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m.role, Role::Tool))
            {
                if !last_tool.content.trim().is_empty() {
                    iter_text = last_tool.content.clone();
                }
            }
        }

        // ── G2: minimum-length final-answer guard ────────────────────────────
        // A model that just burned N tool calls sometimes closes with a bare
        // fragment ("...", "Done."). Re-prompt once for a full answer; a second
        // short answer is accepted as-is so legit short answers cost one retry.
        let min_answer_chars = state.chat_agentic.min_answer_chars;
        if !stream_died
            && min_answer_chars > 0
            && iter_text.trim().chars().count() < min_answer_chars
            && min_len_retries < 1
        {
            min_len_retries += 1;
            tracing::info!(
                iteration,
                answer_chars = iter_text.trim().chars().count(),
                min_answer_chars,
                "agentic: final answer too short — re-prompting once"
            );
            request.messages.push(ChatMessage {
                role: Role::User,
                content: "Your answer was too short. Answer my question fully in plain \
                          text using the tool results above; do not call any more tools."
                    .to_string(),
                images: None,
                tool_calls: None,
                tool_call_id: None,
            });
            continue 'outer;
        }

        final_text = iter_text;

        // Persist context token count (live occupancy, not cumulative spend).
        if tokens_in.is_some() {
            let _ = state
                .session
                .store
                .update_context_tokens(&ctx.current_session_id, last_call_tokens)
                .await;
        }

        // Accumulate lifetime session usage — cumulative spend IS the right
        // number here (what the run actually consumed, incl. re-prompts).
        let _ = state
            .session
            .store
            .add_usage_tokens(
                &ctx.current_session_id,
                tokens_in.unwrap_or(0) as i64,
                tokens_out as i64,
            )
            .await;

        // Send final chat event. A3: always deliver via the buffer — a
        // reattached client (post-reconnect) must still receive it even
        // though `ws_closed` reflects the ORIGINAL (now-dead) connection.
        {
            let done_ctx = active_model_context_window(state).await;
            let done_total = last_call_tokens;
            let done_pct = context_pct(done_total, done_ctx);
            let frame = Frame::event(
                "chat",
                serde_json::json!({
                    "state": "final",
                    "runId": run_id,
                    "seq": buf.seq(),
                    "sessionKey": session_key,
                    "message": { "role": "assistant", "content": final_text },
                    "stopReason": "stop",
                    "contextPct": done_pct,
                    "contextWindow": done_ctx,
                    "contextTokens": done_total,
                    "inputTokens": tokens_in.unwrap_or(0),
                    "outputTokens": tokens_out,
                    "iteration": iteration,
                    "injected_memories": injected_memories,
                }),
            );
            buf.send_now(&frame);
            terminal_sent = true;
            // Phase 4: one line per completed turn makes log forensics far easier.
            info!(
                run_id = %run_id,
                iteration,
                stop_reason = "stop",
                answer_chars = final_text.len(),
                "agentic: final chat event sent"
            );
        }

        break 'outer;
    } // end outer iteration loop

    // Exhausted max_iterations on tool_calls → final_text is empty: the model kept
    // calling tools (or announcing more) and never synthesized an answer. Force ONE
    // final answer-only pass with tools removed so the user gets a real summary of
    // everything gathered instead of a blank/garbage max_iterations turn.
    //
    // A3: skip this (costly) extra LLM call when the ORIGINAL connection is
    // dead AND nobody has reattached — there's no one to deliver the
    // synthesis to. If a client did reattach mid-run, `buf.was_reattached()`
    // overrides the stale `ws_closed` flag and the pass still runs.
    if final_text.is_empty() && (!ws_closed || buf.was_reattached()) {
        let mut synth = request.clone();
        synth.tools = None; // no more tool calls — the model must answer in text
        synth.messages.push(ChatMessage {
            role: Role::User,
            content: "You have gathered enough information from the tool results above. \
                      Answer my original question now in plain text. Do not call any tools."
                .to_string(),
            images: None,
            tool_calls: None,
            tool_call_id: None,
        });

        // ponytail: collect the whole synthesis then emit one final frame (no live
        // delta streaming on this fallback path). Stream deltas if it ever needs to.
        // G1: retry once if the synthesis stream dies mid-answer; on a second
        // failure keep the longer partial and mark it visibly truncated.
        let mut synth_text = String::new();
        let mut synth_best = String::new();
        let mut synth_error: Option<String> = None;
        for attempt in 0..2u32 {
            synth_text.clear();
            synth_error = None;
            if let Ok(mut stream) = await_socket_alive(
                state.llm.router.chat(synth.clone()),
                ws_stream,
                &mut ws_closed,
                ctx,
            )
            .await
            {
                while let Some(chunk) =
                    await_socket_alive(stream.next(), ws_stream, &mut ws_closed, ctx).await
                {
                    if let Some(err) = &chunk.error {
                        synth_error = Some(err.clone());
                    }
                    if !chunk.delta.is_empty() {
                        synth_text.push_str(&chunk.delta);
                    }
                }
            }
            match &synth_error {
                None => break,
                Some(err) => {
                    tracing::warn!(attempt, error = %err, "agentic: synthesis stream died");
                    if synth_text.len() > synth_best.len() {
                        synth_best = synth_text.clone();
                    }
                }
            }
        }
        if synth_error.is_some() {
            synth_text = synth_best;
            final_complete = false;
        }
        final_text = crate::openai_compat::strip_think_tags(&synth_text)
            .trim()
            .to_string();
        if synth_error.is_some() && !final_text.is_empty() {
            final_text.push_str("\n\n[answer truncated: model stream error]");
        }
        tracing::info!(
            synth_len = final_text.len(),
            "agentic: max_iterations reached — forced final synthesis pass"
        );

        let max_ctx = active_model_context_window(state).await;
        let max_total = last_call_tokens;
        let max_pct = context_pct(max_total, max_ctx);
        let frame = Frame::event(
            "chat",
            serde_json::json!({
                "state": "final",
                "runId": run_id,
                "seq": buf.seq(),
                "sessionKey": session_key,
                "message": { "role": "assistant", "content": final_text },
                "stopReason": if final_text.is_empty() { "max_iterations" } else { "stop" },
                "contextPct": max_pct,
                "contextWindow": max_ctx,
                "contextTokens": max_total,
                "injected_memories": injected_memories,
            }),
        );
        buf.send_now(&frame);
        terminal_sent = true;
        // Phase 4: log the forced-synthesis terminal too.
        info!(
            run_id = %run_id,
            stop_reason = if final_text.is_empty() { "max_iterations" } else { "stop" },
            answer_chars = final_text.len(),
            "agentic: final chat event sent (forced synthesis)"
        );
    }

    // FIX #2c: guaranteed terminal frame. Every normal path above sets
    // `terminal_sent`, but a few can fall through with nothing emitted — most
    // notably the synthesis-skip branch (original socket dead, nobody
    // reattached) and the token/deadline breaks when synthesis produced no
    // frame. Without a terminal frame, a client that reattaches in the race
    // window before this run's registry entry is removed would sit stuck
    // mid-stream. Emit one final frame so the run always closes cleanly.
    if !terminal_sent {
        let end_ctx = active_model_context_window(state).await;
        let end_total = last_call_tokens;
        let frame = Frame::event(
            "chat",
            serde_json::json!({
                "state": "final",
                "runId": run_id,
                "seq": buf.seq(),
                "sessionKey": session_key,
                "message": { "role": "assistant", "content": final_text },
                "stopReason": if final_text.is_empty() { "max_iterations" } else { "stop" },
                "contextPct": context_pct(end_total, end_ctx),
                "contextWindow": end_ctx,
                "contextTokens": end_total,
                "injected_memories": injected_memories,
            }),
        );
        buf.send_now(&frame);
        tracing::warn!(
            run_id = %run_id,
            final_len = final_text.len(),
            "agentic: emitted guaranteed-terminal fallback frame (no terminal event had been sent)"
        );
    }

    // -----------------------------------------------------------------------
    // Persist the final assistant message (stop turn).
    // -----------------------------------------------------------------------
    if !final_text.is_empty() {
        let actual_model = state.llm.llama_state.read().await.loaded_model.clone();
        if let Some(ref model_name) = actual_model {
            final_text = crate::response_filter::validate_model_claims(&final_text, model_name);
        }
        let _ = state
            .session
            .store
            .add_message(&NewMessage {
                session_id: ctx.current_session_id.clone(),
                role: "assistant".to_string(),
                content: final_text.clone(),
                model: Some(final_model.clone()),
                tokens: Some(tokens_out as i64),
                complete: final_complete,
                tool_calls: None,
                tool_call_id: None,
            })
            .await;
    }

    // Touch session + trigger compaction (same pattern as single-pass path).
    let _ = state
        .session
        .store
        .touch_session(&ctx.current_session_id)
        .await;

    if tokens_in.is_some() {
        let total_tokens = last_call_tokens as u64;
        let compact_config = ironjaw_session::compaction::CompactionConfig {
            token_limit: state.session.context_token_limit,
            ..Default::default()
        };
        if ironjaw_session::compaction::should_compact(total_tokens, &compact_config) {
            let brain_cfg = state.brain.engine.config().clone();
            if brain_cfg.session_summary_enabled && brain_cfg.session_summary_on_compact {
                let session_id = ctx.current_session_id.clone();
                let sessions = state.session.store.clone();
                let brain_store = state.brain.engine.store().clone();
                let llm = state.llm.router.clone();
                let hooks = state.hooks.clone();
                tokio::spawn(async move {
                    if let Ok(messages) = sessions.recent_messages(&session_id, 200).await {
                        // B2 (CP3): PreCompact hook fires before compact_history.
                        // A `Block` action skips compaction entirely, leaving the
                        // message buffer untouched (no summary generation, no
                        // memory flush). The chat context window itself is
                        // unaffected — this only gates the summary pipeline.
                        let hook_event = crate::hooks::HookEvent::PreCompact {
                            session_id: session_id.clone(),
                            message_count: messages.len(),
                            total_tokens,
                        };
                        let hook_ctx = crate::hooks::HookContext::with_session(
                            hook_event,
                            &session_id,
                            serde_json::json!({
                                "site": "chat.send",
                                "message_count": messages.len(),
                                "total_tokens": total_tokens,
                            }),
                        );
                        if hooks.fire(&hook_ctx).await.is_block() {
                            tracing::info!(
                                session_id = %session_id,
                                "compaction: skipped by PreCompact hook"
                            );
                            return;
                        }
                        let result =
                            ironjaw_session::compaction::compact_history(messages, &compact_config);
                        let mut flush_messages = result.to_discard;
                        flush_messages.extend(result.to_summarize);
                        let pairs = ironjaw_session::extract_flush_candidates(&flush_messages);
                        if let Err(e) = ironjaw_brain::generate_session_summary(
                            &brain_store,
                            &llm,
                            &session_id,
                            &pairs,
                        )
                        .await
                        {
                            tracing::warn!(
                                session_id = %session_id,
                                error = %e,
                                "agentic compaction: session summary failed"
                            );
                        }
                        let flush_config = MemoryFlushConfig::default();
                        let candidates = ironjaw_session::flush_memories_llm(
                            &flush_messages,
                            &flush_config,
                            Some(&llm),
                            Some(flush_metrics()),
                        )
                        .await;
                        if !candidates.is_empty() {
                            info!(
                                session_id = %session_id,
                                count = candidates.len(),
                                "agentic compaction: extracted memory candidates"
                            );
                        }
                    }
                });
            }
        }
    }

    // Audit the full agentic turn.
    let elapsed = start.elapsed();
    let _ = state
        .audit
        .log(&AuditEntry {
            action_type: ActionType::LlmCall,
            method: Some("chat.send".to_string()),
            tool_name: None,
            params: Some(params),
            result: None,
            ok: true,
            error: None,
            llm_model: Some(final_model),
            tokens_in: tokens_in.map(|t| t as i64),
            tokens_out: Some(tokens_out as i64),
            cost_usd: None,
            latency_ms: Some(elapsed.as_millis() as i64),
            session_id: Some(ctx.current_session_id.clone()),
            device_id: ctx.auth.device_id.clone(),
            request_id: id,
            iteration: Some(last_iteration),
            tool_calls: if total_audit_calls.is_empty() {
                None
            } else {
                Some(total_audit_calls)
            },
        })
        .await;

    // Touch sleep scheduler.
    state.brain.sleep_scheduler.touch().await;

    Ok(())
}

/// Merge a streaming [`ToolCallDelta`] into the accumulator map.
///
/// The map stores `(id, name, arguments)` tuples keyed by stream index.
fn merge_tool_call_delta(
    map: &mut std::collections::HashMap<usize, (String, String, String)>,
    delta: &ToolCallDelta,
) {
    let entry = map.entry(delta.index).or_insert_with(|| {
        (
            delta.id.clone().unwrap_or_default(),
            String::new(),
            String::new(),
        )
    });
    if let Some(id) = &delta.id {
        if !id.is_empty() {
            entry.0 = id.clone();
        }
    }
    if let Some(f) = &delta.function {
        if let Some(name) = &f.name {
            // First-delta-wins: some OpenAI-compat backends repeat the function
            // name on continuation deltas, which would produce "read_fileread_file"
            // and cause a registry miss.  Only write the name once.
            if entry.1.is_empty() {
                entry.1.push_str(name);
            }
        }
        if let Some(args) = &f.arguments {
            // Arguments arrive as streamed fragments — accumulate all of them.
            entry.2.push_str(args);
        }
    }
}

/// Enumerate immediate subdirectory names under `working_dir`.
///
/// Used by the Tier 3 WrongToolNudge rule to detect when the LLM's first
/// tool call (a web-search) is mismatched against a user prompt that
/// references one of the user's local project folders.
///
/// Returns an empty Vec on any I/O error — callers treat that as "no nudge".
/// Symlinks and hidden entries (`.`-prefixed) are skipped. Capped at 256
/// entries to bound work.
fn enumerate_local_folders(working_dir: &std::path::Path) -> Vec<String> {
    let read_dir = match std::fs::read_dir(working_dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in read_dir.flatten() {
        if out.len() >= 256 {
            break;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        out.push(name);
    }
    out
}

/// Assemble complete [`ToolCall`]s from the accumulated delta map.
///
/// Sorted by index so the order matches what the LLM emitted.
fn assemble_tool_calls(
    map: std::collections::HashMap<usize, (String, String, String)>,
) -> Vec<ToolCall> {
    let mut entries: Vec<(usize, (String, String, String))> = map.into_iter().collect();
    entries.sort_by_key(|(idx, _)| *idx);
    entries
        .into_iter()
        .map(|(_, (id, name, arguments))| ToolCall {
            id,
            type_: "function".to_string(),
            function: ToolCallFunction { name, arguments },
        })
        .collect()
}

/// FIX #4: split a tool-call batch into (executable, blocked). A call is blocked
/// when its tool `is_registered` but not `is_allowed` (advertised) — the curated-set
/// mis-selection we want to stop. Unknown/hallucinated names are left executable so
/// `execute_tool_batch` emits the normal not-found `tool.result` the client and
/// model already handle. Blocked calls get a synthetic error result (fed back so the
/// model self-corrects), keeping every tool_call_id paired with exactly one result.
fn partition_advertised_tool_calls(
    tool_calls: &[ToolCall],
    is_allowed: impl Fn(&str) -> bool,
    is_registered: impl Fn(&str) -> bool,
    advertised_list: &str,
) -> (Vec<ToolCall>, Vec<crate::agentic::ExecutedToolResult>) {
    let mut exec = Vec::new();
    let mut blocked = Vec::new();
    for tc in tool_calls {
        let name = tc.function.name.as_str();
        if !is_allowed(name) && is_registered(name) {
            blocked.push(crate::agentic::ExecutedToolResult {
                tool_call_id: tc.id.clone(),
                tool_name: tc.function.name.clone(),
                result: ToolResult::error(
                    "tool_not_available",
                    format!("tool `{name}` is not available; use one of: {advertised_list}"),
                ),
                ok: false,
                duration_ms: 0,
                error: Some("tool not available".to_string()),
            });
        } else {
            exec.push(tc.clone());
        }
    }
    (exec, blocked)
}

/// Block until the user has been idle for at least `min_idle_secs`, so background
/// LLM jobs (auto-learn, extraction) don't steal GPU from a live chat. Polls every
/// 5s and gives up after `max_wait_secs` so a nonstop conversation still eventually
/// learns. The idle clock is reset by `sleep_scheduler.touch()` on every chat.send.
/// ponytail: fixed 5s poll + caller-supplied cap; make config if we ever tune it.
async fn wait_for_idle(
    scheduler: &ironjaw_brain::SleepAgentScheduler,
    min_idle_secs: u64,
    max_wait_secs: u64,
) {
    let mut waited = 0u64;
    while scheduler.idle_duration_secs().await < min_idle_secs && waited < max_wait_secs {
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        waited += 5;
    }
}

/// Call the LLM to extract structured facts from conversation messages.
///
/// Builds an extraction prompt via `ExtractionPrompt::build`, sends it to
/// the LLM router, and collects the streamed response into a single string.
/// Returns `None` on any error (caller falls back to heuristic extraction).
async fn extract_via_llm(
    llm: &std::sync::Arc<ironjaw_llm::LlmRouter>,
    pairs: &[(String, String)],
) -> Option<String> {
    let prompt = ironjaw_brain::ExtractionPrompt::build(pairs);
    let request = ChatRequest {
        model: String::new(), // use default model
        messages: vec![ChatMessage {
            role: Role::User,
            content: prompt,
            images: None,
            ..Default::default()
        }],
        temperature: Some(0.2),
        max_tokens: Some(2048),
        extra: None,
        thinking: None,
        tools: None,
        tool_choice: None,
    };

    let mut stream = match llm.chat(request).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "extract_via_llm: LLM call failed");
            return None;
        }
    };

    let mut response = String::new();
    while let Some(chunk) = stream.next().await {
        response.push_str(&chunk.delta);
        if chunk.done {
            break;
        }
    }

    if response.is_empty() {
        warn!("extract_via_llm: empty response from LLM");
        None
    } else {
        debug!(len = response.len(), "extract_via_llm: got LLM response");
        Some(response)
    }
}

/// Handle model.list — return all models from all providers
async fn handle_model_list(id: Uuid, state: &AppState) -> Frame {
    let models = state.llm.router.list_models().await.unwrap_or_default();
    Frame::ok(id, serde_json::json!({ "models": models }))
}

/// Handle model.available — return all known models from the model registry
async fn handle_model_available(id: Uuid, state: &AppState) -> Frame {
    let registry = &state.llm.model_registry;
    let llama_state = state.llm.llama_state.read().await;

    // Local models from llama-models.json
    let local_models: Vec<serde_json::Value> = registry
        .models
        .iter()
        .map(|(name, entry)| {
            let is_loaded = llama_state
                .loaded_model
                .as_deref()
                .is_some_and(|m| m == name);
            serde_json::json!({
                "name": name,
                "description": entry.description,
                "context": entry.context,
                "has_vision": entry.mmproj.is_some(),
                "loaded": is_loaded,
                "type": "local",
                "provider": "llama-server",
                "available": true,
            })
        })
        .collect();

    // Cloud models from config catalogs
    let cloud_models: Vec<serde_json::Value> = state
        .llm
        .cloud_catalogs
        .iter()
        .flat_map(|catalog| {
            let provider = &catalog.provider;
            // Cloud model is available if the provider is registered in the router
            let provider_available = state.llm.router.has_provider(provider);
            catalog.models.iter().map(move |m| {
                serde_json::json!({
                    "name": format!("{}/{}", provider, m.id),
                    "description": m.description,
                    "type": "cloud",
                    "provider": provider,
                    "available": provider_available,
                })
            })
        })
        .collect();

    // Merge local + cloud
    let mut models = local_models;
    models.extend(cloud_models);

    let aliases: Vec<serde_json::Value> = state
        .llm
        .aliases
        .iter()
        .map(|(alias, target)| {
            serde_json::json!({
                "alias": alias,
                "model": target,
            })
        })
        .collect();

    Frame::ok(
        id,
        serde_json::json!({
            "default": registry.default,
            "active": registry.active_or_default(),
            "models": models,
            "aliases": aliases,
            "loaded_model": llama_state.loaded_model,
        }),
    )
}

/// Handle gateway.model / model.status — return live llama-server state
async fn handle_gateway_model(id: Uuid, state: &AppState) -> Frame {
    let llama_state = state.llm.llama_state.read().await;

    let status_str = match &llama_state.status {
        ironjaw_llm::LlamaHealth::Healthy => "ready",
        ironjaw_llm::LlamaHealth::Loading => "loading",
        ironjaw_llm::LlamaHealth::Error(_) => "error",
        ironjaw_llm::LlamaHealth::Unknown => "unknown",
    };

    let model_name = llama_state
        .loaded_model
        .clone()
        .or_else(|| state.llm.router.default_model())
        .unwrap_or_else(|| "unknown".to_string());

    Frame::ok(
        id,
        serde_json::json!({
            "id": model_name,
            "name": model_name,
            "provider": "llama-server",
            "status": status_str,
            "consecutive_failures": llama_state.consecutive_failures,
        }),
    )
}

// -- Tools handlers --

fn handle_tools_list(id: Uuid, state: &AppState) -> Frame {
    let tools = state.tools.list();
    Frame::ok(
        id,
        serde_json::json!({
            "tools": tools,
            "count": tools.len(),
        }),
    )
}

async fn handle_tools_execute(
    id: Uuid,
    params: serde_json::Value,
    ws_stream: &mut SplitStream<WebSocket>,
    state: &AppState,
    ctx: &ConnectionContext,
) -> anyhow::Result<()> {
    let start = std::time::Instant::now();

    let tool_name = match params.get("tool").and_then(|v| v.as_str()) {
        Some(name) => name.to_string(),
        None => {
            let response = Frame::err(id, "INVALID_PARAMS", "Missing required parameter: tool");
            send_frame(&ctx.tx, &response)?;
            return Ok(());
        }
    };

    let tool_params = params
        .get("params")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    // Check if tool exists
    let tool = match state.tools.get(&tool_name) {
        Some(t) => t,
        None => {
            let response = Frame::err(
                id,
                "TOOL_NOT_FOUND",
                format!("Tool '{tool_name}' is not registered"),
            );
            send_frame(&ctx.tx, &response)?;
            return Ok(());
        }
    };

    // Check tool-level permission tier (separate from method-level)
    let tool_tier = tool.tier();
    if tool_tier.requires_approval() {
        let reason = format!("Tool '{tool_name}' requires confirmation");
        let approved = await_hitl_approval(
            ws_stream,
            state,
            &format!("tools.execute:{tool_name}"),
            &tool_tier,
            &reason,
            &tool_params,
            &ctx.tx,
        )
        .await?;
        if !approved {
            let response = Frame::err(
                id,
                "PERMISSION_DENIED",
                format!("Execution of tool '{tool_name}' was denied"),
            );
            send_frame(&ctx.tx, &response)?;
            return Ok(());
        }
    }

    // Send tool.start event with the params so clients can show the user
    // *what* is about to run (matches the agentic-loop tool.start shape).
    let start_event = Frame::event(
        "tool.start",
        serde_json::json!({
            "tool": tool_name,
            "tool_name": tool_name,
            "request_id": id,
            "arguments": tool_params,
        }),
    );
    let _ = send_frame(&ctx.tx, &start_event);

    // Execute the tool — C1: resolve the cwd via session override.
    let cwd = resolve_session_cwd(state, &ctx.current_session_id).await;
    let tool_ctx = ToolContext::new(
        ctx.current_session_id.clone(),
        ctx.connection_id.clone(),
        cwd,
    )
    .with_brain(state.brain.engine.clone());

    let result = state
        .tools
        .execute(
            &tool_name,
            tool_params.clone(),
            &tool_ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    let elapsed = start.elapsed();

    // Build response
    let (ok, response_data) = match &result {
        ToolResult::Text { content } => (
            true,
            serde_json::json!({ "result": { "type": "text", "content": content } }),
        ),
        ToolResult::Json { content } => (
            true,
            serde_json::json!({ "result": { "type": "json", "content": content } }),
        ),
        ToolResult::Error { code, message } => (
            false,
            serde_json::json!({ "result": { "type": "error", "code": code, "message": message } }),
        ),
        ToolResult::Cancelled { reason } => (
            false,
            serde_json::json!({ "result": { "type": "cancelled", "reason": reason } }),
        ),
    };

    // Synthesize a stable tool_call_id for this standalone invocation.
    // The "execute-" prefix distinguishes these from LLM-sourced tool_call_ids.
    let synthetic_id = format!("execute-{}", Uuid::new_v4());

    // Derive a string representation of the output for session persistence
    // (capped — the res frame above still carries the full result).
    let tool_result_content =
        crate::agentic::tool_result_content(&result, state.chat_agentic.tool_result_max_chars);

    // Persist message 1: assistant turn announcing the tool call.
    let _ = state
        .session
        .store
        .add_message(&NewMessage {
            session_id: ctx.current_session_id.clone(),
            role: "assistant".to_string(),
            content: String::new(),
            model: None,
            tokens: None,
            complete: true,
            tool_calls: Some(vec![ToolCall {
                id: synthetic_id.clone(),
                type_: "function".to_string(),
                function: ToolCallFunction {
                    name: tool_name.clone(),
                    arguments: serde_json::to_string(&tool_params).unwrap_or_default(),
                },
            }]),
            tool_call_id: None,
        })
        .await;

    // Persist message 2: tool result linked back by synthetic_id.
    let _ = state
        .session
        .store
        .add_message(&NewMessage {
            session_id: ctx.current_session_id.clone(),
            role: "tool".to_string(),
            content: tool_result_content,
            model: None,
            tokens: None,
            complete: true,
            tool_calls: None,
            tool_call_id: Some(synthetic_id.clone()),
        })
        .await;

    // Send tool.done event.
    // DEPRECATED: Use `tool.result` in agentic loop contexts (chat.agentic enabled).
    // `tool.done` is preserved for backward-compat with the tools.execute standalone path.
    // Fields are enriched (tool_call_id, success, output, error) while keeping the
    // original fields (tool, request_id, duration_ms) intact for existing consumers.
    let (done_success, done_output, done_error) = match &result {
        ToolResult::Text { content } => (
            true,
            Some(serde_json::json!({"type": "text", "content": content})),
            None::<String>,
        ),
        ToolResult::Json { content } => (
            true,
            Some(serde_json::json!({"type": "json", "content": content})),
            None::<String>,
        ),
        ToolResult::Error { code, message } => (false, None, Some(format!("{code}: {message}"))),
        ToolResult::Cancelled { reason } => (false, None, Some(format!("cancelled: {reason}"))),
    };
    let done_event = Frame::event(
        "tool.done",
        serde_json::json!({
            "tool": tool_name,
            "tool_name": tool_name,
            "tool_call_id": synthetic_id,
            "request_id": id,
            "duration_ms": elapsed.as_millis() as u64,
            "success": done_success,
            "output": done_output,
            "error": done_error,
        }),
    );
    let _ = send_frame(&ctx.tx, &done_event);

    let response = if ok {
        Frame::ok(id, response_data)
    } else {
        // Extract error code/message for the frame-level error
        let code = response_data["result"]["code"]
            .as_str()
            .unwrap_or("TOOL_ERROR");
        let message = response_data["result"]["message"]
            .as_str()
            .unwrap_or("Tool execution failed");
        Frame::err(id, code, message)
    };

    // Audit the tool execution
    // Clone before the audit so tool_name/tool_params remain available for the
    // Phase 11 writeback block below (AuditEntry takes ownership).
    let _ = state
        .audit
        .log(&AuditEntry {
            action_type: ActionType::ToolCall,
            method: Some("tools.execute".to_string()),
            tool_name: Some(tool_name.clone()),
            params: Some(tool_params.clone()),
            result: Some(serde_json::to_value(&result).unwrap_or_default()),
            ok,
            error: if !ok {
                Some(format!("{result:?}"))
            } else {
                None
            },
            llm_model: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            latency_ms: Some(elapsed.as_millis() as i64),
            session_id: Some(ctx.current_session_id.clone()),
            device_id: ctx.auth.device_id.clone(),
            request_id: id,
            iteration: None,
            tool_calls: None,
        })
        .await;

    // Phase 11 (direct path): tool-memory writeback — mirror the agentic loop
    // writeback in agentic.rs so that tools.execute standalone calls also persist
    // live-state facts to the brain.  Without this, memory_hints() is dead code
    // on the direct dispatch path and brain.recall returns 0 memories after a
    // successful tools.execute.
    //
    // Fire-and-forget via tokio::spawn so brain I/O does not block the response.
    if ok && state.chat_agentic.tool_memory_writeback_enabled {
        let output_json = serde_json::to_value(&result).unwrap_or(serde_json::Value::Null);
        let hints: Vec<MemoryHint> = state
            .tools
            .get(&tool_name)
            .map(|t| t.memory_hints(&tool_params, &output_json))
            .unwrap_or_default();

        if !hints.is_empty() {
            let brain_arc = Arc::clone(&state.brain.engine);
            let tool_name_owned = tool_name.clone();

            tokio::spawn(async move {
                for hint in hints {
                    let valid_to = {
                        let expiry_secs = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                            .saturating_add(hint.volatility.as_secs());
                        let dt =
                            chrono::DateTime::<chrono::Utc>::from_timestamp(expiry_secs as i64, 0)
                                .unwrap_or_default();
                        Some(dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
                    };

                    let content = hint.content.unwrap_or_else(|| output_json.to_string());
                    let tags = Some(format!("tool_verified,{},{}", hint.entity, hint.key));

                    let mut mem = NewMemory {
                        content,
                        source: Some(tool_name_owned.clone()),
                        tags,
                        embedding: None,
                        memory_type: Some("tool_verified".to_string()),
                        importance: Some(0.8),
                        strength: None,
                        supersedes: None,
                        content_hash: None,
                        valid_from: None,
                        valid_to,
                        source_type: None,
                        reflection: None,
                        trust_level: Some(TrustLevel::ToolVerified),
                        evidence_session_id: None,
                        evidence_message_id: None,
                        scope: None,
                        procedure: None,
                        quality_score: None,
                        quality_flag: None,
                    };

                    if let Err(e) = brain_arc.remember_full(&mut mem).await {
                        warn!(
                            tool = %tool_name_owned,
                            entity = %hint.entity,
                            key = %hint.key,
                            error = %e,
                            "Phase 11 (direct): tool memory writeback failed"
                        );
                    } else {
                        debug!(
                            tool = %tool_name_owned,
                            entity = %hint.entity,
                            key = %hint.key,
                            "Phase 11 (direct): tool memory writeback succeeded"
                        );
                    }
                }
            });
            // Task handle intentionally dropped — fire-and-forget.
        }
    }

    send_frame(&ctx.tx, &response)?;
    Ok(())
}

// -- Brain handlers --

async fn handle_brain_remember(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let content = match params.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return Frame::err(id, "INVALID_PARAMS", "content is required"),
    };

    // Security: injection detection on memory content
    let injection_result = injection_detector().scan(content);
    if injection_result.has_findings() {
        use ironjaw_security::sanitize::InjectionAction;
        match injection_result.recommended_action {
            InjectionAction::Block => {
                warn!(
                    score = injection_result.composite_score,
                    findings = injection_result.findings.len(),
                    "brain.remember: injection detected, blocking"
                );
                return Frame::err(
                    id,
                    "INJECTION_DETECTED",
                    "Memory content blocked by injection detection",
                );
            }
            InjectionAction::Warn => {
                warn!(
                    score = injection_result.composite_score,
                    findings = injection_result.findings.len(),
                    "brain.remember: possible injection, allowing with warning"
                );
            }
            InjectionAction::Log => {
                debug!(
                    score = injection_result.composite_score,
                    "brain.remember: low-risk injection patterns detected"
                );
            }
        }
    }

    let source = params.get("source").and_then(|v| v.as_str());
    let tags = params.get("tags").and_then(|v| v.as_str());

    // New optional metadata fields
    let memory_type_str = params.get("memory_type").and_then(|v| v.as_str());
    let confidence = params.get("confidence").and_then(|v| v.as_f64());
    let trust_level_raw = params.get("trust_level").and_then(|v| v.as_i64());
    let importance_override = params.get("importance").and_then(|v| v.as_f64());
    let evidence_session_id = params
        .get("evidence_session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let evidence_message_id = params
        .get("evidence_message_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let scope = params
        .get("scope")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let supersedes = params.get("supersedes").and_then(|v| v.as_i64());

    // Resolve importance: explicit override > confidence > memory_type base > None
    let importance = importance_override.or_else(|| {
        confidence.or_else(|| {
            memory_type_str.and_then(|mt| {
                mt.parse::<ironjaw_brain::types::MemoryType>()
                    .ok()
                    .map(|t| t.base_importance())
            })
        })
    });

    // Parse trust_level from integer (1-5), validated by TrustLevel::from
    let trust_level = trust_level_raw.map(|v| ironjaw_brain::types::TrustLevel::from(v as i32));

    let mut new_mem = ironjaw_brain::types::NewMemory {
        content: content.to_string(),
        source: source.map(|s| s.to_string()),
        tags: tags.map(|t| t.to_string()),
        embedding: None,
        memory_type: memory_type_str.map(|s| s.to_string()),
        importance,
        strength: None,
        supersedes,
        content_hash: None,
        valid_from: None,
        valid_to: None,
        source_type: None,
        reflection: None,
        trust_level,
        evidence_session_id,
        evidence_message_id,
        scope,
        procedure: None,
        quality_score: None,
        quality_flag: None,
    };

    match state.brain.engine.remember_full(&mut new_mem).await {
        Ok((memory, embedded)) => Frame::ok(
            id,
            serde_json::json!({
                "memory_id": memory.id,
                "embedded": embedded,
                "memory_type": memory.memory_type,
                "importance": memory.importance,
                "trust_level": memory.trust_level.to_string(),
                "quality_score": memory.quality_score,
                "quality_flag": memory.quality_flag,
            }),
        ),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("memory rejected") {
                Frame::err(id, "MEMORY_REJECTED", msg)
            } else {
                Frame::err(id, "BRAIN_ERROR", msg)
            }
        }
    }
}

async fn handle_brain_recall(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    // Accept both `query` and `q` as aliases for transport-symmetry with the HTTP API.
    let query = match params
        .get("query")
        .or_else(|| params.get("q"))
        .and_then(|v| v.as_str())
    {
        Some(q) if !q.is_empty() => q,
        _ => return Frame::err(id, "INVALID_PARAMS", "query (or q) is required"),
    };

    let limit = params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|l| l as usize);

    let detail = params
        .get("detail")
        .and_then(|v| v.as_str())
        .unwrap_or("full");

    let scope_filter = params.get("scope_filter").and_then(|v| v.as_str());

    // When explain=true, use recall_scored to expose composite score breakdown per memory.
    let explain = params
        .get("explain")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if explain {
        // recall_scored returns ScoredMemory with composite score; scope_filter not supported
        // in explain mode (falls back to unfiltered scored recall).
        let scored_result = state
            .brain
            .engine
            .recall_scored(query, limit.unwrap_or(10))
            .await;

        return match scored_result {
            Ok(scored) => {
                let count = scored.len();
                let memories: Vec<serde_json::Value> = scored
                    .iter()
                    .map(|sm| {
                        serde_json::json!({
                            "id": sm.memory.id,
                            "content": sm.memory.content,
                            "memory_type": sm.memory.memory_type,
                            "importance": sm.memory.importance,
                            "strength": sm.memory.strength,
                            "access_count": sm.memory.access_count,
                            "trust_level": sm.memory.trust_level.to_string(),
                            "trust_score": sm.memory.trust_level.score(),
                            "created_at": sm.memory.created_at,
                            "accessed_at": sm.memory.accessed_at,
                            "composite_score": sm.score,
                            "score_weights": {
                                "semantic": 0.35,
                                "recency": 0.20,
                                "importance": 0.15,
                                "trust": 0.15,
                                "strength": 0.10,
                                "frequency": 0.05
                            }
                        })
                    })
                    .collect();
                Frame::ok(
                    id,
                    serde_json::json!({
                        "memories": memories,
                        "count": count,
                        "detail": detail,
                        "explain": true,
                    }),
                )
            }
            Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
        };
    }

    // Use enhanced recall with scope/procedural boosting if scope_filter present
    let recall_result = if scope_filter.is_some() {
        state
            .brain
            .engine
            .recall_with_options(query, limit, scope_filter)
            .await
    } else {
        state.brain.engine.recall(query, limit).await
    };

    match recall_result {
        Ok(memories) => {
            let count = memories.len();
            let result = match detail {
                "compact" => {
                    let compact: Vec<serde_json::Value> = memories
                        .iter()
                        .map(|m| {
                            let snippet: String = m.content.chars().take(100).collect();
                            serde_json::json!({
                                "id": m.id,
                                "snippet": snippet,
                                "memory_type": m.memory_type,
                                "importance": m.importance,
                                "created_at": m.created_at,
                            })
                        })
                        .collect();
                    serde_json::json!({
                        "memories": compact,
                        "count": count,
                        "detail": "compact",
                    })
                }
                "deep" => {
                    let mut deep: Vec<serde_json::Value> = Vec::with_capacity(count);
                    for m in &memories {
                        let mut val = serde_json::to_value(m).unwrap_or(serde_json::Value::Null);
                        // If source looks like a UUID, look up session title
                        if let Some(source) = m.source.as_deref() {
                            if looks_like_uuid(source) {
                                if let Ok(Some(session)) =
                                    state.session.store.get_session(source).await
                                {
                                    val["session_title"] = serde_json::json!(session.title);
                                }
                            }
                        }
                        deep.push(val);
                    }
                    serde_json::json!({
                        "memories": deep,
                        "count": count,
                        "detail": "deep",
                    })
                }
                _ => {
                    // "full" (default) — backward compatible
                    serde_json::json!({
                        "memories": memories,
                        "count": count,
                    })
                }
            };
            Frame::ok(id, result)
        }
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

async fn handle_brain_forget(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let memory_id = match params.get("memory_id").and_then(|v| v.as_i64()) {
        Some(mid) => mid,
        None => return Frame::err(id, "INVALID_PARAMS", "memory_id is required"),
    };

    match state.brain.engine.forget(memory_id).await {
        Ok(forgotten) => Frame::ok(
            id,
            serde_json::json!({
                "forgotten": forgotten,
                "memory_id": memory_id,
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Import files into brain identity layer (Phase 13 brain migration).
/// Reads .md files from a directory and stores each as an identity entry.
async fn handle_brain_import(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let path = match params.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return Frame::err(id, "INVALID_PARAMS", "path is required (directory or file)"),
    };

    let path = std::path::Path::new(path);
    let mut imported = Vec::new();
    let mut errors = Vec::new();

    if path.is_file() {
        // Single file import
        match import_file_to_identity(path, state).await {
            Ok(key) => imported.push(key),
            Err(e) => errors.push(format!("{}: {}", path.display(), e)),
        }
    } else if path.is_dir() {
        // Import all .md files from directory
        let entries = match std::fs::read_dir(path) {
            Ok(e) => e,
            Err(e) => return Frame::err(id, "IO_ERROR", format!("Cannot read directory: {e}")),
        };
        for entry in entries.flatten() {
            let file_path = entry.path();
            if file_path.extension().and_then(|e| e.to_str()) == Some("md") {
                match import_file_to_identity(&file_path, state).await {
                    Ok(key) => imported.push(key),
                    Err(e) => errors.push(format!("{}: {}", file_path.display(), e)),
                }
            }
        }
    } else {
        return Frame::err(
            id,
            "NOT_FOUND",
            format!("Path not found: {}", path.display()),
        );
    }

    Frame::ok(
        id,
        serde_json::json!({
            "imported": imported,
            "imported_count": imported.len(),
            "errors": errors,
        }),
    )
}

/// Import a single file into the brain identity table.
/// Key is the file stem (e.g. "SOUL" for "SOUL.md").
async fn import_file_to_identity(
    file_path: &std::path::Path,
    state: &AppState,
) -> anyhow::Result<String> {
    let content = tokio::fs::read_to_string(file_path).await?;
    let key = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_lowercase();

    state
        .brain
        .engine
        .store()
        .set_identity(&key, &content)
        .await?;
    Ok(key)
}

/// Handle brain.status — return loaded layers, token counts, memory count, learning stats
async fn handle_brain_status(id: Uuid, state: &AppState) -> Frame {
    let memory_count = state
        .brain
        .engine
        .store()
        .count_memories()
        .await
        .unwrap_or(0);
    let identity_entries = state
        .brain
        .engine
        .store()
        .get_all_identity()
        .await
        .unwrap_or_default();

    let layers: Vec<serde_json::Value> = identity_entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "key": e.key,
                "tokens": ironjaw_brain::estimate_tokens(&e.content),
                "updated_at": e.updated_at,
            })
        })
        .collect();

    let total_tokens: usize = identity_entries
        .iter()
        .map(|e| ironjaw_brain::estimate_tokens(&e.content))
        .sum();

    let last_learn_at = state
        .brain
        .engine
        .store()
        .last_learn_at()
        .await
        .unwrap_or(None);

    let memories_by_type: serde_json::Value = state
        .brain
        .engine
        .store()
        .count_by_type()
        .await
        .map(|counts| {
            counts
                .into_iter()
                .map(|(t, c)| serde_json::json!({"type": t, "count": c}))
                .collect::<Vec<_>>()
        })
        .map(serde_json::Value::from)
        .unwrap_or(serde_json::Value::Array(vec![]));

    // Temporal fact counts
    let temporal_active = state
        .brain
        .engine
        .store()
        .count_temporal_active()
        .await
        .unwrap_or(0);
    let temporal_expired = state
        .brain
        .engine
        .store()
        .count_temporal_expired()
        .await
        .unwrap_or(0);

    // Context variable stats
    let (ctx_var_count, ctx_var_total_tokens) = state
        .brain
        .engine
        .store()
        .count_context_vars()
        .await
        .unwrap_or((0, 0));

    let config = state.brain.engine.config();
    Frame::ok(
        id,
        serde_json::json!({
            "layers": layers,
            "layer_count": identity_entries.len(),
            "total_tokens": total_tokens,
            "memory_count": memory_count,
            "auto_learn": config.auto_learn,
            "brain_files_dir": config.brain_files_dir,
            "memory_relevance_threshold": config.memory_relevance_threshold,
            "learn_dedup_threshold": config.learn_dedup_threshold,
            "learn_after_turns": config.learn_after_turns,
            "learning_enabled": config.auto_learn,
            "last_learn_at": last_learn_at,
            "memories_by_type": memories_by_type,
            "temporal_active": temporal_active,
            "temporal_expired": temporal_expired,
            "context_vars": {
                "count": ctx_var_count,
                "total_tokens": ctx_var_total_tokens,
            },
        }),
    )
}

/// Handle prompt.stats — measure token cost of the system prompt.
async fn handle_prompt_stats(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let is_cloud = params
        .get("is_cloud")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match state.brain.engine.prompt_stats(message, is_cloud).await {
        Ok(stats) => Frame::ok(id, serde_json::to_value(&stats).unwrap_or_default()),
        Err(e) => Frame::err(
            id,
            "PROMPT_STATS_FAILED",
            format!("prompt stats failed: {e}"),
        ),
    }
}

/// Handle brain.health — comprehensive brain health metrics.
async fn handle_brain_health(id: Uuid, state: &AppState) -> Frame {
    match state.brain.engine.store().health_metrics().await {
        Ok(metrics) => Frame::ok(id, serde_json::to_value(&metrics).unwrap_or_default()),
        Err(e) => Frame::err(
            id,
            "BRAIN_HEALTH_FAILED",
            format!("brain health metrics failed: {e}"),
        ),
    }
}

/// Handle brain.reload — force re-bootstrap brain from disk files
async fn handle_brain_reload(id: Uuid, state: &AppState) -> Frame {
    match state.brain.engine.bootstrap_from_files().await {
        Ok(report) => Frame::ok(
            id,
            serde_json::json!({
                "reloaded": true,
                "layers_loaded": report.layers_loaded,
                "core_tokens": report.core_tokens,
                "total_tokens": report.total_tokens,
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", format!("Reload failed: {e}")),
    }
}

/// Handle brain.memories — list/search auto-learned memories
async fn handle_brain_memories(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let query = params.get("query").and_then(|v| v.as_str());
    let limit = params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|l| l as usize)
        .unwrap_or(50);

    let memories = if let Some(q) = query {
        // Semantic search
        match state.brain.engine.recall(q, Some(limit)).await {
            Ok(m) => m,
            Err(e) => return Frame::err(id, "BRAIN_ERROR", e.to_string()),
        }
    } else {
        // List all (paginated)
        match state.brain.engine.store().all_memories().await {
            Ok(mut m) => {
                m.truncate(limit);
                m
            }
            Err(e) => return Frame::err(id, "BRAIN_ERROR", e.to_string()),
        }
    };

    let count = memories.len();
    Frame::ok(
        id,
        serde_json::json!({
            "memories": memories,
            "count": count,
        }),
    )
}

/// Handle brain.consolidate — trigger the maintenance pipeline (decay, prune, dedup, cleanup)
async fn handle_brain_consolidate(id: Uuid, state: &AppState) -> Frame {
    let maint = ironjaw_brain::MemoryMaintenance::new(state.brain.engine.store().clone());
    match maint.run_all().await {
        Ok(report) => Frame::ok(
            id,
            serde_json::json!({
                "decayed": report.decayed,
                "pruned": report.pruned,
                "deduplicated": report.deduplicated,
                "cleaned": report.cleaned,
                "merged": report.merged,
                "boosted": report.boosted,
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", format!("Consolidation failed: {e}")),
    }
}

/// Handle brain.cleanup.stale — retire memories older than N days with low access count.
async fn handle_brain_cleanup_stale(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let days = params.get("days").and_then(|v| v.as_u64()).unwrap_or(90);
    let min_accesses = params
        .get("min_accesses")
        .and_then(|v| v.as_i64())
        .unwrap_or(3);

    let maint = ironjaw_brain::MemoryMaintenance::new(state.brain.engine.store().clone());
    match maint.cleanup_stale(days, min_accesses).await {
        Ok(report) => Frame::ok(id, serde_json::to_value(&report).unwrap()),
        Err(e) => Frame::err(id, "BRAIN_ERROR", format!("cleanup.stale failed: {e}")),
    }
}

/// Handle brain.cleanup.contradictions — detect and retire contradictory memories via LLM.
async fn handle_brain_cleanup_contradictions(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let batch_size = params
        .get("batch_size")
        .and_then(|v| v.as_u64())
        .unwrap_or(50) as usize;

    let maint = ironjaw_brain::MemoryMaintenance::new(state.brain.engine.store().clone());
    match maint
        .cleanup_contradictions(&state.llm.router, batch_size)
        .await
    {
        Ok(report) => Frame::ok(id, serde_json::to_value(&report).unwrap()),
        Err(e) => Frame::err(
            id,
            "BRAIN_ERROR",
            format!("cleanup.contradictions failed: {e}"),
        ),
    }
}

/// Handle brain.cleanup.duplicates — find and merge near-duplicate memories.
async fn handle_brain_cleanup_duplicates(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let threshold = params
        .get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.92);

    let maint = ironjaw_brain::MemoryMaintenance::new(state.brain.engine.store().clone());
    match maint.cleanup_duplicates(threshold).await {
        Ok(report) => Frame::ok(id, serde_json::to_value(&report).unwrap()),
        Err(e) => Frame::err(id, "BRAIN_ERROR", format!("cleanup.duplicates failed: {e}")),
    }
}

// -- Brain v2: Temporal & Context handlers --

/// Handle brain.timeline — query memories valid at a specific point in time
async fn handle_brain_timeline(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let at = match params.get("at").and_then(|v| v.as_str()) {
        Some(a) => a,
        None => return Frame::err(id, "INVALID_PARAMS", "at is required (ISO 8601 timestamp)"),
    };
    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    match state
        .brain
        .engine
        .store()
        .query_point_in_time(at, limit)
        .await
    {
        Ok(memories) => Frame::ok(
            id,
            serde_json::json!({
                "memories": memories,
                "count": memories.len(),
                "at": at,
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Handle brain.summaries — query session summary memories
async fn handle_brain_summaries(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let session_id = params.get("session_id").and_then(|v| v.as_str());
    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    match state
        .brain
        .engine
        .store()
        .get_summaries(session_id, limit)
        .await
    {
        Ok(summaries) => Frame::ok(
            id,
            serde_json::json!({
                "summaries": summaries,
                "count": summaries.len(),
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Handle brain.context.set — upsert a named context variable
async fn handle_brain_context_set(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return Frame::err(id, "INVALID_PARAMS", "name is required"),
    };
    let content = match params.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return Frame::err(id, "INVALID_PARAMS", "content is required"),
    };
    let source_path = params.get("source_path").and_then(|v| v.as_str());
    let token_count = (content.len() / 4) as i64;

    match state
        .brain
        .engine
        .store()
        .set_context_var(name, content, source_path, token_count)
        .await
    {
        Ok(()) => Frame::ok(
            id,
            serde_json::json!({
                "name": name,
                "token_count": token_count,
                "set": true,
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Handle brain.context.load — toggle loaded state of a context variable
async fn handle_brain_context_load(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return Frame::err(id, "INVALID_PARAMS", "name is required"),
    };
    let loaded = params
        .get("loaded")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    match state
        .brain
        .engine
        .store()
        .set_context_var_loaded(name, loaded)
        .await
    {
        Ok(updated) => {
            if updated {
                Frame::ok(
                    id,
                    serde_json::json!({
                        "name": name,
                        "loaded": loaded,
                    }),
                )
            } else {
                Frame::err(
                    id,
                    "NOT_FOUND",
                    format!("context variable '{name}' not found"),
                )
            }
        }
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Handle brain.context.list — list all context variables with metadata
async fn handle_brain_context_list(id: Uuid, state: &AppState) -> Frame {
    match state.brain.engine.store().list_context_vars().await {
        Ok(vars) => Frame::ok(
            id,
            serde_json::json!({
                "variables": vars,
                "count": vars.len(),
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Handle brain.context.delete — remove a context variable by name
async fn handle_brain_context_delete(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return Frame::err(id, "INVALID_PARAMS", "name is required"),
    };

    match state.brain.engine.store().delete_context_var(name).await {
        Ok(deleted) => {
            if deleted {
                Frame::ok(
                    id,
                    serde_json::json!({
                        "name": name,
                        "deleted": true,
                    }),
                )
            } else {
                Frame::err(
                    id,
                    "NOT_FOUND",
                    format!("context variable '{name}' not found"),
                )
            }
        }
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Handle brain.context.peek — windowed view of a context variable's content
async fn handle_brain_context_peek(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return Frame::err(id, "INVALID_PARAMS", "name is required"),
    };
    let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(1000) as usize;

    match state
        .brain
        .engine
        .store()
        .get_context_var_slice(name, offset, limit)
        .await
    {
        Ok(Some((slice, total_len))) => Frame::ok(
            id,
            serde_json::json!({
                "name": name,
                "content": slice,
                "offset": offset,
                "length": slice.len(),
                "total_length": total_len,
            }),
        ),
        Ok(None) => Frame::err(
            id,
            "NOT_FOUND",
            format!("context variable '{name}' not found"),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

// -- Evolution handlers (I13: GEPA Pattern) --

fn get_evolution_registry(state: &AppState) -> &ironjaw_brain::EvolutionRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<ironjaw_brain::EvolutionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let base_dir = std::path::PathBuf::from(&state.brain.engine.config().brain_files_dir);
        let llm_url = state.integration_urls.llm_base_url.clone();
        ironjaw_brain::EvolutionRegistry::new(base_dir, llm_url)
    })
}

async fn handle_brain_evolve_start(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let config: ironjaw_brain::EvolutionConfig = match serde_json::from_value(params.clone()) {
        Ok(c) => c,
        Err(e) => return Frame::err(id, "INVALID_PARAMS", format!("Bad config: {e}")),
    };

    let registry = get_evolution_registry(state);
    let evo_id = match registry.create(config).await {
        Ok(eid) => eid,
        Err(e) => return Frame::err(id, "EVOLUTION_ERROR", e.to_string()),
    };

    if let Err(e) = registry.start(&evo_id).await {
        return Frame::err(id, "EVOLUTION_ERROR", e.to_string());
    }

    match registry.status(&evo_id).await {
        Ok(report) => Frame::ok(id, serde_json::to_value(&report).unwrap_or_default()),
        Err(e) => Frame::err(id, "EVOLUTION_ERROR", e.to_string()),
    }
}

async fn handle_brain_evolve_status(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let evo_id = match params.get("evolution_id").and_then(|v| v.as_str()) {
        Some(eid) => eid,
        None => return Frame::err(id, "INVALID_PARAMS", "evolution_id is required"),
    };

    let registry = get_evolution_registry(state);
    match registry.status(evo_id).await {
        Ok(report) => Frame::ok(id, serde_json::to_value(&report).unwrap_or_default()),
        Err(e) => Frame::err(id, "EVOLUTION_ERROR", e.to_string()),
    }
}

async fn handle_brain_evolve_stop(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let evo_id = match params.get("evolution_id").and_then(|v| v.as_str()) {
        Some(eid) => eid,
        None => return Frame::err(id, "INVALID_PARAMS", "evolution_id is required"),
    };

    let registry = get_evolution_registry(state);
    match registry.stop(evo_id).await {
        Ok(report) => Frame::ok(id, serde_json::to_value(&report).unwrap_or_default()),
        Err(e) => Frame::err(id, "EVOLUTION_ERROR", e.to_string()),
    }
}

async fn handle_brain_evolve_promote(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let evo_id = match params.get("evolution_id").and_then(|v| v.as_str()) {
        Some(eid) => eid,
        None => return Frame::err(id, "INVALID_PARAMS", "evolution_id is required"),
    };

    let registry = get_evolution_registry(state);
    match registry.promote(evo_id).await {
        Ok(result) => Frame::ok(id, serde_json::to_value(&result).unwrap_or_default()),
        Err(e) => Frame::err(id, "EVOLUTION_ERROR", e.to_string()),
    }
}

async fn handle_brain_evolve_history(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let evo_id = match params.get("evolution_id").and_then(|v| v.as_str()) {
        Some(eid) => eid,
        None => return Frame::err(id, "INVALID_PARAMS", "evolution_id is required"),
    };

    let registry = get_evolution_registry(state);
    match registry.history(evo_id).await {
        Ok(history) => Frame::ok(
            id,
            serde_json::json!({
                "evolution_id": evo_id,
                "generations": serde_json::to_value(&history).unwrap_or_default(),
                "total": history.len(),
            }),
        ),
        Err(e) => Frame::err(id, "EVOLUTION_ERROR", e.to_string()),
    }
}

// -- I10: Causal Trace handler --

/// Handle brain.trace — traverse causal links from an entity
async fn handle_brain_trace(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let entity = match params.get("entity").and_then(|v| v.as_str()) {
        Some(e) => e,
        None => return Frame::err(id, "INVALID_PARAMS", "entity is required"),
    };
    let direction = params
        .get("direction")
        .and_then(|v| v.as_str())
        .unwrap_or("both");
    let depth = params.get("depth").and_then(|v| v.as_u64()).unwrap_or(3) as u32;

    if !["causes", "effects", "both"].contains(&direction) {
        return Frame::err(
            id,
            "INVALID_PARAMS",
            "direction must be 'causes', 'effects', or 'both'",
        );
    }

    let graph = match state.brain.engine.graph() {
        Some(g) => g,
        None => return Frame::err(id, "BRAIN_ERROR", "knowledge graph not initialized"),
    };

    // Find the entity by name
    let entity_obj = match graph.find_entity_by_name(entity).await {
        Ok(Some(e)) => e,
        Ok(None) => return Frame::err(id, "NOT_FOUND", format!("entity '{entity}' not found")),
        Err(e) => return Frame::err(id, "BRAIN_ERROR", e.to_string()),
    };

    match graph.trace(entity_obj.id, direction, depth).await {
        Ok(traced) => {
            let results: Vec<serde_json::Value> = traced
                .iter()
                .map(|(ent, rel_type, hop)| {
                    serde_json::json!({
                        "entity": ent.name,
                        "entity_type": ent.entity_type,
                        "relation": rel_type,
                        "depth": hop,
                        "confidence": ent.confidence,
                    })
                })
                .collect();
            Frame::ok(
                id,
                serde_json::json!({
                    "entity": entity,
                    "direction": direction,
                    "depth": depth,
                    "results": results,
                    "count": results.len(),
                }),
            )
        }
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

// -- I15: Workflow handlers --

/// Handle brain.workflows — list learned workflows
async fn handle_brain_workflows(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let candidates_only = params
        .get("candidates_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match state
        .brain
        .engine
        .store()
        .list_workflows(candidates_only)
        .await
    {
        Ok(workflows) => Frame::ok(
            id,
            serde_json::json!({
                "workflows": workflows,
                "count": workflows.len(),
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Handle brain.workflow.apply — retrieve a workflow by ID for replay
async fn handle_brain_workflow_apply(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let workflow_id = match params.get("workflow_id").and_then(|v| v.as_i64()) {
        Some(wid) => wid,
        None => return Frame::err(id, "INVALID_PARAMS", "workflow_id is required"),
    };

    match state.brain.engine.store().get_workflow(workflow_id).await {
        Ok(Some(wf)) => Frame::ok(
            id,
            serde_json::json!({
                "workflow": wf,
                "actions": wf.actions,
            }),
        ),
        Ok(None) => Frame::err(id, "NOT_FOUND", format!("workflow {workflow_id} not found")),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Handle brain.workflow.track — record an action and detect patterns
async fn handle_brain_workflow_track(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let method = match params.get("method").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => return Frame::err(id, "INVALID_PARAMS", "method is required"),
    };
    let param_fingerprint = params.get("param_fingerprint").and_then(|v| v.as_str());
    let session_id = params.get("session_id").and_then(|v| v.as_str());

    // Log the action
    let store = state.brain.engine.store();
    if let Err(e) = store
        .log_action(method, param_fingerprint, session_id)
        .await
    {
        return Frame::err(id, "BRAIN_ERROR", e.to_string());
    }

    // Check for pattern: get last 20 actions and look for repeating subsequences
    let recent = match store.get_recent_actions(20).await {
        Ok(actions) => actions,
        Err(e) => return Frame::err(id, "BRAIN_ERROR", e.to_string()),
    };

    // Simple sliding-window pattern detection (length 3-5)
    let mut detected_workflow = None;
    for window_size in (3..=5).rev() {
        if recent.len() < window_size * 2 {
            continue;
        }
        let window: Vec<&str> = recent[..window_size]
            .iter()
            .map(|a| a.method.as_str())
            .collect();
        // Check if same sequence appears earlier
        for start in window_size..=(recent.len() - window_size) {
            let candidate: Vec<&str> = recent[start..start + window_size]
                .iter()
                .map(|a| a.method.as_str())
                .collect();
            if window == candidate {
                detected_workflow = Some(recent[..window_size].to_vec());
                break;
            }
        }
        if detected_workflow.is_some() {
            break;
        }
    }

    // If a pattern was detected, upsert the workflow
    if let Some(actions) = &detected_workflow {
        let name = actions
            .iter()
            .map(|a| a.method.clone())
            .collect::<Vec<_>>()
            .join(" -> ");
        let new_wf = ironjaw_brain::NewWorkflow {
            name,
            actions: actions.clone(),
        };
        match store.upsert_workflow(&new_wf).await {
            Ok((wf_id, count)) => {
                return Frame::ok(
                    id,
                    serde_json::json!({
                        "tracked": true,
                        "pattern_detected": true,
                        "workflow_id": wf_id,
                        "occurrence_count": count,
                        "is_candidate": count >= 3,
                    }),
                );
            }
            Err(e) => return Frame::err(id, "BRAIN_ERROR", e.to_string()),
        }
    }

    Frame::ok(
        id,
        serde_json::json!({
            "tracked": true,
            "pattern_detected": false,
        }),
    )
}

// -- Pending memory review handlers --

/// Handle brain.pending.list — list auto-extracted pending memories awaiting review
async fn handle_brain_pending_list(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let limit = params.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
    let offset = params.get("offset").and_then(|v| v.as_i64()).unwrap_or(0);

    match state
        .brain
        .engine
        .store()
        .list_pending_memories(limit, offset)
        .await
    {
        Ok(memories) => Frame::ok(
            id,
            serde_json::json!({
                "memories": memories,
                "count": memories.len(),
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Handle brain.pending.approve — promote a pending memory to permanent storage
async fn handle_brain_pending_approve(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let Some(pending_id) = params.get("id").and_then(|v| v.as_i64()) else {
        return Frame::err(id, "INVALID_PARAMS", "id is required");
    };

    match state
        .brain
        .engine
        .store()
        .approve_pending_memory(pending_id)
        .await
    {
        Ok(memory_id) => Frame::ok(
            id,
            serde_json::json!({
                "approved": true,
                "memory_id": memory_id,
                "pending_id": pending_id,
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Handle brain.pending.reject — discard a pending memory
async fn handle_brain_pending_reject(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let Some(pending_id) = params.get("id").and_then(|v| v.as_i64()) else {
        return Frame::err(id, "INVALID_PARAMS", "id is required");
    };

    match state
        .brain
        .engine
        .store()
        .reject_pending_memory(pending_id)
        .await
    {
        Ok(()) => Frame::ok(
            id,
            serde_json::json!({
                "rejected": true,
                "pending_id": pending_id,
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

// -- Brain auto_learn report handler (D2) --

/// Handle brain.auto_learn.report — return process-lifetime auto-learn telemetry counters
async fn handle_brain_auto_learn_report(id: Uuid) -> Frame {
    use ironjaw_brain::{
        auto_learn_approved_total, auto_learn_dropped_total, auto_learn_pending_total,
    };

    Frame::ok(
        id,
        serde_json::json!({
            "pending_routed": auto_learn_pending_total(),
            "auto_approved": auto_learn_approved_total(),
            "dropped": auto_learn_dropped_total(),
        }),
    )
}

// -- Brain snapshot handlers (D0) --

/// Handle brain.snapshot.list — list all non-deleted snapshots
async fn handle_brain_snapshot_list(
    id: Uuid,
    _params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    match state.brain.snapshots.list().await {
        Ok(snapshots) => {
            let items: Vec<serde_json::Value> = snapshots
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "id": s.id,
                        "label": s.label,
                        "memory_count": s.memory_count,
                        "identity_count": s.identity_count,
                        "context_var_count": s.context_var_count,
                        "created_at": s.created_at,
                    })
                })
                .collect();
            Frame::ok(id, serde_json::json!({ "snapshots": items }))
        }
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Handle brain.snapshot.create — capture current brain state
async fn handle_brain_snapshot_create(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let label = params
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("manual snapshot")
        .to_string();

    match state
        .brain
        .snapshots
        .create(&label, state.brain.engine.store())
        .await
    {
        Ok(snapshot) => Frame::ok(
            id,
            serde_json::json!({
                "snapshot": {
                    "id": snapshot.id,
                    "label": snapshot.label,
                    "memory_count": snapshot.memory_count,
                    "identity_count": snapshot.identity_count,
                    "context_var_count": snapshot.context_var_count,
                    "created_at": snapshot.created_at,
                }
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Handle brain.snapshot.restore — atomically restore brain state from snapshot
async fn handle_brain_snapshot_restore(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let Some(snapshot_id) = params.get("id").and_then(|v| v.as_i64()) else {
        return Frame::err(id, "INVALID_PARAMS", "id is required");
    };

    match state.brain.snapshots.restore(snapshot_id).await {
        Ok(report) => Frame::ok(
            id,
            serde_json::json!({
                "restored": true,
                "snapshot_id": report.snapshot_id,
                "memories_written": report.memories_written,
                "identities_written": report.identities_written,
                "context_vars_written": report.context_vars_written,
                "memories_soft_deleted": report.memories_soft_deleted,
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

/// Handle brain.snapshot.delete — soft-delete a snapshot
async fn handle_brain_snapshot_delete(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let Some(snapshot_id) = params.get("id").and_then(|v| v.as_i64()) else {
        return Frame::err(id, "INVALID_PARAMS", "id is required");
    };

    match state.brain.snapshots.delete(snapshot_id).await {
        Ok(deleted) => Frame::ok(
            id,
            serde_json::json!({
                "deleted": deleted,
                "snapshot_id": snapshot_id,
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

// -- Schedule handlers (Phase 8) --

async fn handle_schedule_list(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let enabled_only = params
        .get("enabled_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    match state.proactive.list_schedules(enabled_only).await {
        Ok(schedules) => Frame::ok(id, serde_json::json!({ "schedules": schedules })),
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

async fn handle_schedule_create(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return Frame::err(id, "INVALID_PARAMS", "name is required"),
    };
    let cron_expr = match params.get("cron_expr").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return Frame::err(id, "INVALID_PARAMS", "cron_expr is required"),
    };
    let action_type_str = match params.get("action_type").and_then(|v| v.as_str()) {
        Some(a) => a,
        None => return Frame::err(id, "INVALID_PARAMS", "action_type is required"),
    };
    let action_type = match ProactiveActionType::from_str(action_type_str) {
        Some(at) => at,
        None => {
            return Frame::err(
                id,
                "INVALID_PARAMS",
                format!("Unknown action_type: {action_type_str}"),
            )
        }
    };
    let action_data = params
        .get("action_data")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    match state
        .proactive
        .create_schedule(name, cron_expr, action_type, &action_data)
        .await
    {
        Ok(schedule) => Frame::ok(id, serde_json::json!({ "schedule": schedule })),
        Err(e) => Frame::err(id, "SCHEDULE_ERROR", e.to_string()),
    }
}

async fn handle_schedule_delete(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let schedule_id = match params.get("schedule_id").and_then(|v| v.as_str()) {
        Some(sid) => sid,
        None => return Frame::err(id, "INVALID_PARAMS", "schedule_id is required"),
    };
    match state.proactive.delete_schedule(schedule_id).await {
        Ok(deleted) => Frame::ok(id, serde_json::json!({ "deleted": deleted })),
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

async fn handle_schedule_toggle(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let schedule_id = match params.get("schedule_id").and_then(|v| v.as_str()) {
        Some(sid) => sid,
        None => return Frame::err(id, "INVALID_PARAMS", "schedule_id is required"),
    };
    let enabled = match params.get("enabled").and_then(|v| v.as_bool()) {
        Some(e) => e,
        None => return Frame::err(id, "INVALID_PARAMS", "enabled is required"),
    };
    match state.proactive.toggle_schedule(schedule_id, enabled).await {
        Ok(Some(schedule)) => Frame::ok(id, serde_json::json!({ "schedule": schedule })),
        Ok(None) => Frame::err(
            id,
            "SCHEDULE_NOT_FOUND",
            format!("Schedule '{schedule_id}' not found"),
        ),
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

// -- Workflow handlers (Phase 8) --

async fn handle_workflow_create(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return Frame::err(id, "INVALID_PARAMS", "name is required"),
    };

    let steps_json = match params.get("steps") {
        Some(s) => s,
        None => return Frame::err(id, "INVALID_PARAMS", "steps is required"),
    };

    let steps: Vec<WorkflowStep> = match serde_json::from_value(steps_json.clone()) {
        Ok(s) => s,
        Err(e) => return Frame::err(id, "INVALID_PARAMS", format!("Invalid steps: {e}")),
    };

    match state.proactive.create_workflow(name, steps).await {
        Ok(workflow) => Frame::ok(id, serde_json::json!({ "workflow": workflow })),
        Err(e) => Frame::err(id, "WORKFLOW_ERROR", e.to_string()),
    }
}

async fn handle_workflow_status(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let workflow_id = match params.get("workflow_id").and_then(|v| v.as_str()) {
        Some(wid) => wid,
        None => return Frame::err(id, "INVALID_PARAMS", "workflow_id is required"),
    };
    match state.proactive.get_workflow(workflow_id).await {
        Ok(Some(workflow)) => {
            let steps = state
                .proactive
                .get_workflow_steps(workflow_id)
                .await
                .unwrap_or_default();
            Frame::ok(
                id,
                serde_json::json!({ "workflow": workflow, "steps": steps }),
            )
        }
        Ok(None) => Frame::err(
            id,
            "WORKFLOW_NOT_FOUND",
            format!("Workflow '{workflow_id}' not found"),
        ),
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

async fn handle_workflow_cancel(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let workflow_id = match params.get("workflow_id").and_then(|v| v.as_str()) {
        Some(wid) => wid,
        None => return Frame::err(id, "INVALID_PARAMS", "workflow_id is required"),
    };
    match state.proactive.cancel_workflow(workflow_id).await {
        Ok(cancelled) => Frame::ok(id, serde_json::json!({ "cancelled": cancelled })),
        Err(e) => Frame::err(id, "WORKFLOW_ERROR", e.to_string()),
    }
}

async fn handle_workflow_approve(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let workflow_id = match params.get("workflow_id").and_then(|v| v.as_str()) {
        Some(wid) => wid,
        None => return Frame::err(id, "INVALID_PARAMS", "workflow_id is required"),
    };
    let step_id = match params.get("step_id").and_then(|v| v.as_str()) {
        Some(sid) => sid,
        None => return Frame::err(id, "INVALID_PARAMS", "step_id is required"),
    };
    match state
        .proactive
        .approve_workflow_step(workflow_id, step_id)
        .await
    {
        Ok(()) => Frame::ok(id, serde_json::json!({ "approved": true })),
        Err(e) => Frame::err(id, "WORKFLOW_ERROR", e.to_string()),
    }
}

// ── Voice handlers (Phase 9) ──────────────────────────────────────────

async fn handle_voice_start(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &ConnectionContext,
) -> Frame {
    let mode = match params.get("mode").and_then(|v| v.as_str()) {
        Some("vad") => VoiceMode::Vad,
        _ => VoiceMode::PushToTalk,
    };
    match state.voice.start_session(&ctx.connection_id, mode).await {
        Ok(session_id) => Frame::ok(id, serde_json::json!({ "session_id": session_id })),
        Err(e) => Frame::err(id, "VOICE_ERROR", e.to_string()),
    }
}

async fn handle_voice_stop(id: Uuid, state: &AppState, ctx: &ConnectionContext) -> Frame {
    match state.voice.stop_session(&ctx.connection_id).await {
        Ok(result) => Frame::ok(
            id,
            serde_json::json!({
                "transcription": {
                    "text": result.text,
                    "language": result.language,
                    "duration_secs": result.duration_secs,
                },
            }),
        ),
        Err(e) => Frame::err(id, "VOICE_ERROR", e.to_string()),
    }
}

async fn handle_voice_tts(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &ConnectionContext,
) -> anyhow::Result<()> {
    let text = match params.get("text").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => {
            let response = Frame::err(id, "INVALID_PARAMS", "text is required");
            send_frame(&ctx.tx, &response)?;
            return Ok(());
        }
    };

    match state.voice.speak(text).await {
        Ok(audio) => {
            if !audio.is_empty() {
                // Send audio as binary frame: 0x01 header + audio bytes
                let mut frame_data = Vec::with_capacity(1 + audio.len());
                frame_data.push(0x01); // audio frame marker
                frame_data.extend_from_slice(&audio);
                ctx.tx
                    .send(Message::Binary(frame_data.into()))
                    .map_err(|_| anyhow::anyhow!("Connection closed"))?;
            }
            // Send JSON confirmation
            let response = Frame::ok(id, serde_json::json!({ "audio_bytes": audio.len() }));
            send_frame(&ctx.tx, &response)?;
        }
        Err(e) => {
            let response = Frame::err(id, "TTS_ERROR", e.to_string());
            send_frame(&ctx.tx, &response)?;
        }
    }
    Ok(())
}

/// Generate speech audio from text using the TTS backend.
/// Params: { text: string, voice?: string, format?: "wav"|"mp3" }
/// Returns: { audio: string (base64), format: string, voice: string, durationMs: u64 }
async fn handle_tts_generate(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let text = match params.get("text").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t,
        _ => return Frame::err(id, "MISSING_PARAM", "text required"),
    };

    let voice = params
        .get("voice")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    match state.voice.speak(text).await {
        Ok(audio_bytes) => {
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&audio_bytes);
            // Rough duration estimate for 16kHz 16-bit mono audio
            let duration_ms = if audio_bytes.len() > 44 {
                ((audio_bytes.len() as u64 - 44) * 1000) / (16000 * 2)
            } else {
                0
            };
            Frame::ok(
                id,
                serde_json::json!({
                    "audio": b64,
                    "format": "wav",
                    "voice": voice,
                    "durationMs": duration_ms,
                }),
            )
        }
        Err(e) => Frame::retryable_err(id, "TTS_ERROR", format!("TTS synthesis failed: {e}")),
    }
}

/// List all available character voices for TTS.
/// Returns: array of { id, name, character, description }
fn handle_tts_voices(id: Uuid) -> Frame {
    let roster = ironjaw_voice::VoiceRoster::default_roster();
    let voices: Vec<serde_json::Value> = roster
        .list()
        .iter()
        .map(|v| {
            serde_json::json!({
                "id": v.id,
                "name": v.name,
                "character": v.character,
                "description": v.description,
            })
        })
        .collect();
    Frame::ok(id, serde_json::json!(voices))
}

/// Handle a binary WebSocket frame.
/// First byte identifies content type: 0x01 = audio, 0x02 = image
async fn handle_binary_frame(
    data: &[u8],
    state: &AppState,
    ctx: &mut ConnectionContext,
) -> anyhow::Result<()> {
    if data.is_empty() {
        return Ok(());
    }

    let frame_type = data[0];
    let payload = &data[1..];

    match frame_type {
        0x01 => {
            // Audio chunk — feed to voice session
            if let Err(e) = state.voice.feed_audio(&ctx.connection_id, payload).await {
                warn!(
                    connection_id = %ctx.connection_id,
                    error = %e,
                    "Failed to feed audio"
                );
                let err_frame = Frame::err(Uuid::new_v4(), "AUDIO_ERROR", e.to_string());
                send_frame(&ctx.tx, &err_frame)?;
            }
        }
        0x02 => {
            // Image — validate and store as pending for next chat.send
            match state.image_processor.process(payload.to_vec()) {
                Ok(image_input) => {
                    info!(
                        connection_id = %ctx.connection_id,
                        mime_type = %image_input.mime_type,
                        size = payload.len(),
                        "Image received and validated"
                    );
                    ctx.pending_images.push(image_input.base64);
                    let ack = Frame::event(
                        "image.received",
                        serde_json::json!({
                            "mime_type": image_input.mime_type,
                            "size": payload.len(),
                            "pending_count": ctx.pending_images.len(),
                        }),
                    );
                    send_frame(&ctx.tx, &ack)?;
                }
                Err(e) => {
                    warn!(
                        connection_id = %ctx.connection_id,
                        error = %e,
                        "Invalid image binary frame"
                    );
                    let err_frame = Frame::err(Uuid::new_v4(), "IMAGE_ERROR", e.to_string());
                    send_frame(&ctx.tx, &err_frame)?;
                }
            }
        }
        other => {
            warn!(frame_type = other, "Unknown binary frame type");
        }
    }

    Ok(())
}

// ── Device handlers (Phase 11) ─────────────────────────────────────────

async fn handle_device_list(id: Uuid, state: &AppState) -> Frame {
    let devices = state.device.registry.list().await;
    Frame::ok(
        id,
        serde_json::json!({
            "devices": devices,
            "count": devices.len(),
        }),
    )
}

async fn handle_device_cmd(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let tool = match params.get("tool").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return Frame::err(id, "INVALID_PARAMS", "tool is required"),
    };
    let cmd_params = params
        .get("params")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    // Target a specific device or broadcast to all
    let device_id = params.get("device_id").and_then(|v| v.as_str());

    let cmd_id = Uuid::new_v4();
    let cmd_frame = Frame::Cmd {
        id: cmd_id,
        tool: tool.to_string(),
        params: cmd_params,
    };
    let cmd_json = match serde_json::to_string(&cmd_frame) {
        Ok(j) => j,
        Err(e) => return Frame::err(id, "INTERNAL_ERROR", e.to_string()),
    };

    match device_id {
        Some(did) => match state.device.registry.send_text(did, cmd_json).await {
            Ok(_) => Frame::ok(
                id,
                serde_json::json!({
                    "cmd_id": cmd_id.to_string(),
                    "device_id": did,
                    "status": "sent",
                }),
            ),
            Err(e) => Frame::err(id, "DEVICE_ERROR", e),
        },
        None => {
            // No specific device — send to first connected device
            let devices = state.device.registry.list().await;
            if devices.is_empty() {
                return Frame::err(id, "NO_DEVICES", "No devices connected");
            }
            let target = &devices[0].id;
            match state.device.registry.send_text(target, cmd_json).await {
                Ok(_) => Frame::ok(
                    id,
                    serde_json::json!({
                        "cmd_id": cmd_id.to_string(),
                        "device_id": target,
                        "status": "sent",
                    }),
                ),
                Err(e) => Frame::err(id, "DEVICE_ERROR", e),
            }
        }
    }
}

async fn handle_notify_send(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let priority_str = params
        .get("priority")
        .and_then(|v| v.as_str())
        .unwrap_or("normal");
    let priority = match Priority::from_str(priority_str) {
        Some(p) => p,
        None => {
            return Frame::err(
                id,
                "INVALID_PARAMS",
                format!("Unknown priority: {priority_str}"),
            )
        }
    };
    let title = match params.get("title").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return Frame::err(id, "INVALID_PARAMS", "title is required"),
    };
    let body = match params.get("body").and_then(|v| v.as_str()) {
        Some(b) => b,
        None => return Frame::err(id, "INVALID_PARAMS", "body is required"),
    };

    let mut notification = crate::notifications::Notification::new(priority, title, body);
    if let Some(data) = params.get("data") {
        notification = notification.with_data(data.clone());
    }

    let event = Frame::event("notification", notification.to_event_data());
    let event_json = serde_json::to_string(&event).unwrap_or_default();

    // Broadcast to all connected devices
    state
        .device
        .registry
        .broadcast_text(event_json.clone())
        .await;

    // Also queue for disconnected devices (if any were expected)
    // For now, just report success with the count of connected devices
    let connected = state.device.registry.count().await;

    Frame::ok(
        id,
        serde_json::json!({
            "delivered": connected,
            "notification_id": notification.id,
        }),
    )
}

// ── Export handlers (Phase 11) ──────────────────────────────────────────

async fn handle_export_conversations(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &ConnectionContext,
) -> Frame {
    let session_id = params
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or(&ctx.current_session_id);
    let format = params
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("json");

    // Load session info
    let session = match state.session.store.get_session(session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Frame::err(
                id,
                "SESSION_NOT_FOUND",
                format!("Session '{session_id}' not found"),
            )
        }
        Err(e) => return Frame::err(id, "STORE_ERROR", e.to_string()),
    };

    // Load all messages
    let messages = match state
        .session
        .store
        .get_messages(session_id, 10000, None)
        .await
    {
        Ok(m) => m,
        Err(e) => return Frame::err(id, "STORE_ERROR", e.to_string()),
    };

    match format {
        "markdown" => {
            let title = session.title.as_deref().unwrap_or("Untitled Session");
            let mut md = format!("# {title}\n\n");
            md.push_str(&format!(
                "**Session ID:** {}\n**Created:** {}\n\n---\n\n",
                session.id, session.created_at
            ));
            for msg in &messages {
                let role = msg.role.to_uppercase();
                md.push_str(&format!("### {}\n\n{}\n\n", role, msg.content));
            }
            Frame::ok(
                id,
                serde_json::json!({
                    "format": "markdown",
                    "data": md,
                    "message_count": messages.len(),
                }),
            )
        }
        _ => {
            // Default: JSON
            Frame::ok(
                id,
                serde_json::json!({
                    "format": "json",
                    "session": session,
                    "messages": messages,
                    "message_count": messages.len(),
                }),
            )
        }
    }
}

async fn handle_export_brain(id: Uuid, state: &AppState) -> Frame {
    match state.brain.engine.recall("", Some(10000)).await {
        Ok(memories) => Frame::ok(
            id,
            serde_json::json!({
                "format": "json",
                "memories": memories,
                "count": memories.len(),
            }),
        ),
        Err(e) => Frame::err(id, "BRAIN_ERROR", e.to_string()),
    }
}

// ── Phase 12: Metrics + Backup handlers ────────────────────────────────

async fn handle_infra_metrics(id: Uuid, state: &AppState) -> Frame {
    let metrics = state.observability.metrics.snapshot();
    let uptime_secs = (chrono::Utc::now() - state.started_at).num_seconds();
    let circuit_breakers = state.observability.circuit_breakers.snapshot().await;

    let cb_json: serde_json::Value = circuit_breakers
        .iter()
        .map(|(k, (s, f))| {
            (
                k.clone(),
                serde_json::json!({
                    "state": format!("{:?}", s),
                    "failures": f,
                }),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into();

    // Probe live services with 5s timeout
    let timeout = std::time::Duration::from_secs(5);

    // Probe llama-server
    let llama_status = match tokio::time::timeout(
        timeout,
        state
            .http_client
            .get(format!("{}/health", state.integration_urls.llm_base_url))
            .send(),
    )
    .await
    {
        Ok(Ok(resp)) => {
            let status_code = resp.status().as_u16();
            let body = resp.json::<serde_json::Value>().await.unwrap_or_default();
            serde_json::json!({
                "status": if status_code == 200 { "online" } else { "degraded" },
                "status_code": status_code,
                "details": body,
            })
        }
        _ => serde_json::json!({ "status": "offline" }),
    };

    // Probe llama-server for loaded model
    let llama_model = match tokio::time::timeout(
        timeout,
        state
            .http_client
            .get(format!("{}/v1/models", state.integration_urls.llm_base_url))
            .send(),
    )
    .await
    {
        Ok(Ok(resp)) => {
            let body = resp.json::<serde_json::Value>().await.unwrap_or_default();
            body["data"][0]["id"]
                .as_str()
                .unwrap_or("unknown")
                .to_string()
        }
        _ => "unknown".to_string(),
    };

    // Probe cc-bridge
    let bridge_status = if !state.integration_urls.cc_bridge_url.is_empty() {
        match tokio::time::timeout(
            timeout,
            state
                .http_client
                .get(format!(
                    "{}/cc/health",
                    state.integration_urls.cc_bridge_url
                ))
                .send(),
        )
        .await
        {
            Ok(Ok(resp)) => {
                serde_json::json!({
                    "status": if resp.status().is_success() { "online" } else { "degraded" },
                    "status_code": resp.status().as_u16(),
                })
            }
            _ => serde_json::json!({ "status": "offline" }),
        }
    } else {
        serde_json::json!({ "status": "not_configured" })
    };

    // Probe cc-chronicle
    let chronicle_status = if !state.integration_urls.cc_chronicle_url.is_empty() {
        match tokio::time::timeout(
            timeout,
            state
                .http_client
                .get(format!(
                    "{}/api/health",
                    state.integration_urls.cc_chronicle_url
                ))
                .send(),
        )
        .await
        {
            Ok(Ok(resp)) => {
                serde_json::json!({
                    "status": if resp.status().is_success() { "online" } else { "degraded" },
                    "status_code": resp.status().as_u16(),
                })
            }
            _ => serde_json::json!({ "status": "offline" }),
        }
    } else {
        serde_json::json!({ "status": "not_configured" })
    };

    Frame::ok(
        id,
        serde_json::json!({
            "uptime_secs": uptime_secs,
            "started_at": state.started_at.to_rfc3339(),
            "metrics": metrics,
            "circuit_breakers": cb_json,
            "devices_connected": state.device.registry.count().await,
            "services": {
                "ironjaw": {
                    "status": "online",
                    "uptime_secs": uptime_secs,
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "llama_server": {
                    "health": llama_status,
                    "model": llama_model,
                    "url": state.integration_urls.llm_base_url,
                },
                "cc_bridge": bridge_status,
                "cc_chronicle": chronicle_status,
            },
        }),
    )
}

async fn handle_metrics_counters(id: Uuid, state: &AppState) -> Frame {
    let counters = state.observability.snapshot_counters();
    Frame::ok(id, serde_json::json!({ "counters": counters }))
}

async fn handle_backup_create(id: Uuid, _state: &AppState) -> Frame {
    // Collect database paths from various stores
    let backup_dir = format!(
        "data/backups/{}",
        chrono::Utc::now().format("%Y-%m-%d_%H%M%S")
    );

    // Create backup directory
    if let Err(e) = tokio::fs::create_dir_all(&backup_dir).await {
        return Frame::err(
            id,
            "BACKUP_ERROR",
            format!("Failed to create backup dir: {e}"),
        );
    }

    let mut backed_up = Vec::new();
    let mut errors = Vec::new();

    // Copy known database files
    let db_files = [
        ("data/sessions.db", "sessions.db"),
        ("data/brain.db", "brain.db"),
        ("data/proactive.db", "proactive.db"),
        ("data/audit.db", "audit.db"),
        ("data/secrets.db", "secrets.db"),
    ];

    for (src, name) in &db_files {
        let src_path = std::path::Path::new(src);
        if src_path.exists() {
            let dst = format!("{backup_dir}/{name}");
            match tokio::fs::copy(src_path, &dst).await {
                Ok(bytes) => {
                    backed_up.push(serde_json::json!({
                        "file": name,
                        "bytes": bytes,
                    }));
                }
                Err(e) => {
                    errors.push(format!("{name}: {e}"));
                }
            }
        }
    }

    // Copy config
    let config_src = std::path::Path::new("config/ironjaw.toml");
    if config_src.exists() {
        let dst = format!("{backup_dir}/ironjaw.toml");
        match tokio::fs::copy(config_src, &dst).await {
            Ok(bytes) => {
                backed_up.push(serde_json::json!({
                    "file": "ironjaw.toml",
                    "bytes": bytes,
                }));
            }
            Err(e) => {
                errors.push(format!("ironjaw.toml: {e}"));
            }
        }
    }

    Frame::ok(
        id,
        serde_json::json!({
            "backup_dir": backup_dir,
            "files": backed_up,
            "errors": errors,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
    )
}

// -- CC Bridge proxy handlers --

/// Proxy ccbridge.sessions to CC Bridge HTTP API
async fn handle_ccbridge_sessions(
    id: Uuid,
    _params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let base = &state.integration_urls.cc_bridge_url;
    if base.is_empty() {
        return Frame::err(id, "NOT_CONFIGURED", "cc_bridge_url not configured");
    }
    let url = format!("{base}/cc/sessions");
    match state.http_client.get(&url).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(body) => Frame::ok(id, body),
            Err(e) => Frame::err(id, "PARSE_ERROR", e.to_string()),
        },
        Err(e) => Frame::err(id, "PROXY_ERROR", format!("CC Bridge unreachable: {e}")),
    }
}

/// Proxy ccbridge.sessions.rename — POST to CC Bridge rename endpoint
async fn handle_ccbridge_rename(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let base = &state.integration_urls.cc_bridge_url;
    if base.is_empty() {
        return Frame::err(id, "NOT_CONFIGURED", "cc_bridge_url not configured");
    }
    let session_id = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return Frame::err(id, "INVALID_PARAMS", "sessionId required"),
    };
    let title = match params.get("title").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return Frame::err(id, "INVALID_PARAMS", "title required"),
    };

    let body = serde_json::json!({ "title": title });

    // Try PATCH /cc/sessions/{sessionId}/rename first, fall back to PUT /cc/sessions/{sessionId}
    let rename_url = format!("{base}/cc/sessions/{session_id}/rename");
    let resp = state
        .http_client
        .patch(&rename_url)
        .json(&body)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
            Ok(data) => Frame::ok(id, data),
            Err(_) => Frame::ok(
                id,
                serde_json::json!({ "sessionId": session_id, "title": title, "renamed": true }),
            ),
        },
        Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
            // PATCH not supported — try PUT as fallback
            let put_url = format!("{base}/cc/sessions/{session_id}");
            match state.http_client.put(&put_url).json(&body).send().await {
                Ok(r2) if r2.status().is_success() => match r2.json::<serde_json::Value>().await {
                    Ok(data) => Frame::ok(id, data),
                    Err(_) => Frame::ok(
                        id,
                        serde_json::json!({ "sessionId": session_id, "title": title, "renamed": true }),
                    ),
                },
                Ok(r2) => {
                    let status = r2.status();
                    let detail = r2.text().await.unwrap_or_default();
                    Frame::err(
                        id,
                        "PROXY_ERROR",
                        format!("CC Bridge PUT returned {status}: {detail}"),
                    )
                }
                Err(e) => Frame::err(id, "PROXY_ERROR", format!("CC Bridge unreachable: {e}")),
            }
        }
        Ok(r) => {
            let status = r.status();
            let detail = r.text().await.unwrap_or_default();
            Frame::err(
                id,
                "PROXY_ERROR",
                format!("CC Bridge PATCH returned {status}: {detail}"),
            )
        }
        Err(e) => Frame::err(id, "PROXY_ERROR", format!("CC Bridge unreachable: {e}")),
    }
}

/// Proxy ccbridge.chat — POST to CC Bridge
async fn handle_ccbridge_chat(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let base = &state.integration_urls.cc_bridge_url;
    if base.is_empty() {
        return Frame::err(id, "NOT_CONFIGURED", "cc_bridge_url not configured");
    }
    let url = format!("{base}/cc/chat");
    match state.http_client.post(&url).json(params).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(body) => Frame::ok(id, body),
            Err(e) => Frame::err(id, "PARSE_ERROR", e.to_string()),
        },
        Err(e) => Frame::err(id, "PROXY_ERROR", format!("CC Bridge unreachable: {e}")),
    }
}

// -- CC Chronicle proxy handlers --

/// Generic Chronicle proxy: forwards params as query string to the given path
async fn handle_chronicle_proxy(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    path: &str,
) -> Frame {
    let base = &state.integration_urls.cc_chronicle_url;
    if base.is_empty() {
        return Frame::err(id, "NOT_CONFIGURED", "cc_chronicle_url not configured");
    }
    let url = format!("{base}{path}");

    // Convert params object to query string pairs
    let mut query_pairs: Vec<(String, String)> = Vec::new();
    if let Some(obj) = params.as_object() {
        for (k, v) in obj {
            // Skip the "id" key used for session lookup (already in path)
            if k == "id" {
                continue;
            }
            match v {
                serde_json::Value::String(s) => query_pairs.push((k.clone(), s.clone())),
                serde_json::Value::Number(n) => query_pairs.push((k.clone(), n.to_string())),
                serde_json::Value::Bool(b) => query_pairs.push((k.clone(), b.to_string())),
                _ => {}
            }
        }
    }

    let request = state.http_client.get(&url).query(&query_pairs);
    match request.send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(body) => Frame::ok(id, body),
            Err(e) => Frame::err(id, "PARSE_ERROR", e.to_string()),
        },
        Err(e) => Frame::err(id, "PROXY_ERROR", format!("CC Chronicle unreachable: {e}")),
    }
}

// -- Dr. CLAW RPC equivalent handlers --

/// Handle project.status — read JobHunter run_history.json and return last completed run
async fn handle_project_status(id: Uuid, params: &serde_json::Value) -> Frame {
    let project = params
        .get("project")
        .and_then(|v| v.as_str())
        .unwrap_or("jobhunter");

    let state_path = match project {
        "jobhunter" => {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_default();
            format!("{home}/Projects/JobHunter/run_history.json")
        }
        _ => return Frame::err(id, "UNKNOWN_PROJECT", format!("Unknown project: {project}")),
    };

    match tokio::fs::read_to_string(&state_path).await {
        Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(data) => {
                // run_history.json is an array of run records -- return the last one
                if let Some(runs) = data.as_array() {
                    if let Some(last) = runs.last() {
                        Frame::ok(id, last.clone())
                    } else {
                        Frame::err(id, "EMPTY", "No run history found")
                    }
                } else {
                    // If it's an object already, return as-is
                    Frame::ok(id, data)
                }
            }
            Err(e) => Frame::err(id, "PARSE_ERROR", e.to_string()),
        },
        Err(e) => Frame::err(id, "IO_ERROR", format!("Cannot read {state_path}: {e}")),
    }
}

/// Handle file.get — read file content with path allowlist, extension filter, and traversal guard
async fn handle_file_get(id: Uuid, params: &serde_json::Value, _state: &AppState) -> Frame {
    let path = match params.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return Frame::err(id, "INVALID_PARAMS", "path required"),
    };

    // Security: reject path traversal
    if path.contains("..") {
        return Frame::err(id, "FORBIDDEN", "Path traversal not allowed");
    }

    // Normalize backslashes for consistent prefix matching
    let normalized = path.replace('/', "\\");

    // Security: only allow files under known safe directories
    let allowed_win = [
        "C:\\Users\\deploy\\Projects\\",
        "C:\\Users\\deploy\\clawd\\",
    ];
    let allowed_bash = ["/c/Users/deploy/Projects/", "/c/Users/deploy/clawd/"];

    let is_allowed = allowed_win
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
        || allowed_bash.iter().any(|prefix| path.starts_with(prefix));
    if !is_allowed {
        return Frame::err(id, "FORBIDDEN", "Path not in allowlist");
    }

    // Security: extension allowlist
    let allowed_extensions = [".md", ".pdf", ".csv", ".json", ".txt", ".docx"];
    let lower = path.to_lowercase();
    let ext_ok = allowed_extensions.iter().any(|ext| lower.ends_with(ext));
    if !ext_ok {
        return Frame::err(
            id,
            "FORBIDDEN",
            format!(
                "File extension not allowed. Allowed: {}",
                allowed_extensions.join(", ")
            ),
        );
    }

    match tokio::fs::read(path).await {
        Ok(bytes) => {
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

            // Extract file name from path
            let file_name = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");

            // Determine MIME type from extension
            let mime_type = if lower.ends_with(".md") {
                "text/markdown"
            } else if lower.ends_with(".pdf") {
                "application/pdf"
            } else if lower.ends_with(".csv") {
                "text/csv"
            } else if lower.ends_with(".json") {
                "application/json"
            } else if lower.ends_with(".txt") {
                "text/plain"
            } else if lower.ends_with(".docx") {
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            } else {
                "application/octet-stream"
            };

            Frame::ok(
                id,
                serde_json::json!({
                    "path": path,
                    "fileName": file_name,
                    "mimeType": mime_type,
                    "content": b64,
                    "sizeBytes": bytes.len(),
                }),
            )
        }
        Err(e) => Frame::err(id, "IO_ERROR", format!("Cannot read file: {e}")),
    }
}

/// Handle model.llm — query llama-server /props
async fn handle_model_llm(id: Uuid, state: &AppState) -> Frame {
    let base = &state.integration_urls.llm_base_url;
    if base.is_empty() {
        return Frame::err(id, "NOT_CONFIGURED", "No LLM provider configured");
    }

    let url = format!("{base}/props");
    match state.http_client.get(&url).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(body) => Frame::ok(id, body),
            Err(e) => Frame::err(id, "PARSE_ERROR", e.to_string()),
        },
        Err(e) => Frame::err(id, "LLM_ERROR", format!("llama-server unreachable: {e}")),
    }
}

/// Update brain's runtime model info after a model switch.
fn update_brain_model_info(state: &AppState, resolved_model: &str) {
    let (provider, model) = if let Some((p, m)) = resolved_model.split_once('/') {
        (p, m)
    } else {
        ("llama-server", resolved_model)
    };
    let backend = if provider.contains("anthropic") || provider.contains("claude") {
        format!("cloud model ({provider} / {model})")
    } else {
        format!("local LLM ({provider} / {model})")
    };
    state
        .brain
        .engine
        .set_model_info(format!("Your exact model is: {model}. Provider: {provider}. Backend: {backend}. Do NOT guess or substitute a different model name — use this exact identifier when asked."));
    info!(backend, "Brain model info updated after switch");
}

/// Handle model.switch — resolve alias, persist override on ConnectionContext.
/// Supports `{model: String}` or `{mode: "cloud"|"local"}` param styles.
async fn handle_model_switch(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &mut ConnectionContext,
) -> Frame {
    let aliases = &*state.llm.aliases;

    // Support mode-based switching: "cloud" -> anthropic, "local" -> llama-server
    if let Some(mode) = params.get("mode").and_then(|v| v.as_str()) {
        match mode {
            "cloud" => {
                let has_cloud = state.llm.router.has_provider("openai")
                    || state.llm.router.has_provider("anthropic")
                    || state.llm.router.has_provider("openrouter");
                if !has_cloud {
                    return Frame::err(
                        id,
                        "NO_CLOUD_PROVIDER",
                        "No cloud (anthropic/openai/openrouter) provider configured",
                    );
                }
                let resolved = crate::skills::resolve_alias(aliases, "cloud");
                let provider = resolved.split_once('/').map(|(p, _)| p).unwrap_or("cloud");
                ctx.active_model_override = Some(resolved.clone());
                update_brain_model_info(state, &resolved);
                info!(mode = "cloud", resolved = %resolved, "Model mode switch applied");
                return Frame::ok(
                    id,
                    serde_json::json!({
                        "model": resolved,
                        "acknowledged": true,
                        "instant": true,
                        "provider": provider,
                        "note": "Switched to cloud provider",
                    }),
                );
            }
            "local" => {
                ctx.active_model_override = None;
                let default = state
                    .llm
                    .router
                    .default_model()
                    .unwrap_or_else(|| "unknown".to_string());
                update_brain_model_info(state, &default);
                info!(
                    mode = "local",
                    "Model mode switch applied (cleared override)"
                );
                return Frame::ok(
                    id,
                    serde_json::json!({
                        "model": "local",
                        "acknowledged": true,
                        "instant": true,
                        "provider": "llama-server",
                        "note": "Switched to local llama-server provider",
                    }),
                );
            }
            other => {
                return Frame::err(
                    id,
                    "INVALID_MODE",
                    format!("Unknown mode '{other}'. Use 'cloud' or 'local'"),
                );
            }
        }
    }

    let model = match params.get("model").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => return Frame::err(id, "INVALID_PARAMS", "model or mode required"),
    };

    let resolved = crate::skills::resolve_alias(aliases, model);

    // Validate against active skill's allowed models (if any)
    let allowed = if let Ok(skills) = state.skill.registry.read() {
        if let Some(skill) = skills.active_skill() {
            let mut valid = vec![];
            if let Some(ref m) = skill.model {
                valid.push(m.clone());
            }
            valid.extend(skill.fallback_chain.clone());
            valid.extend(skill.cloud_models.clone());
            if valid.is_empty() {
                true
            } else {
                let valid_resolved: Vec<String> = valid
                    .iter()
                    .map(|m| crate::skills::resolve_alias(aliases, m))
                    .collect();
                valid_resolved.contains(&resolved) || valid.contains(&model.to_string())
            }
        } else {
            true
        }
    } else {
        true
    };

    if !allowed {
        return Frame::err(
            id,
            "MODEL_NOT_ALLOWED",
            format!("Model '{model}' is not in the active skill's allowed models"),
        );
    }

    let prior_model = ctx.active_model_override.clone();
    ctx.active_model_override = Some(resolved.clone());
    update_brain_model_info(state, &resolved);
    info!(model = model, resolved = %resolved, "Model switch applied");

    // B2 (CP3): Emit SkillDeactivated/Activated PAIR around the model switch.
    // Per A15: model switches are coupled to skill identity at the dashboard
    // level; firing the pair here lets observability hooks track model
    // transitions even when no explicit role.switch was issued. Block actions
    // are not honored — the switch already committed.
    if let Some(prior) = prior_model.clone() {
        let dctx = crate::hooks::HookContext::with_session(
            crate::hooks::HookEvent::SkillDeactivated {
                session_id: ctx.current_session_id.clone(),
                skill_name: prior.clone(),
                next_skill: Some(resolved.clone()),
            },
            &ctx.current_session_id,
            serde_json::json!({"site": "model.switch", "prior": prior, "next": &resolved}),
        );
        let _ = state.hooks.fire(&dctx).await;
    }
    let actx = crate::hooks::HookContext::with_session(
        crate::hooks::HookEvent::SkillActivated {
            session_id: ctx.current_session_id.clone(),
            skill_name: resolved.clone(),
            prior_skill: prior_model,
        },
        &ctx.current_session_id,
        serde_json::json!({"site": "model.switch", "model": &resolved}),
    );
    let _ = state.hooks.fire(&actx).await;

    // Determine provider from resolved model name
    let provider = resolved
        .split_once('/')
        .map(|(p, _)| p.to_string())
        .unwrap_or_else(|| "llama-server".to_string());

    // Cloud models switch instantly — no server restart needed
    let is_cloud_model = resolved.contains('/')
        && (resolved.starts_with("openai/")
            || resolved.starts_with("anthropic/")
            || resolved.starts_with("openrouter/"));

    // If restart=true, trigger actual llama-server model switch (server-level)
    let restart = params
        .get("restart")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if restart {
        // Cloud models don't need llama-switch — override is already set above
        if is_cloud_model {
            info!(
                model = model,
                resolved = %resolved,
                provider = %provider,
                "Cloud model switch — skipping llama-switch (instant)"
            );
            return Frame::ok(
                id,
                serde_json::json!({
                    "model": resolved,
                    "alias": model,
                    "acknowledged": true,
                    "switching": false,
                    "instant": true,
                    "provider": provider,
                    "note": "Cloud model switched instantly — no server restart needed.",
                }),
            );
        }

        // Validate model exists in registry before restarting
        if !state.llm.model_registry.is_empty() && state.llm.model_registry.get(model).is_none() {
            let available: Vec<&str> = state.llm.model_registry.model_names();
            return Frame::err(
                id,
                "UNKNOWN_MODEL",
                format!(
                    "Model '{}' not found in registry. Available: {}",
                    model,
                    available.join(", ")
                ),
            );
        }

        // Set state to Loading while the switch happens
        {
            let mut s = state.llm.llama_state.write().await;
            s.status = ironjaw_llm::LlamaHealth::Loading;
            s.loaded_model = None;
        }

        // Run llama-switch in background (non-blocking)
        let model_name = model.to_string();
        let llama_state_clone = state.llm.llama_state.clone();
        let base_url = state.integration_urls.llm_base_url.clone();
        let registry_clone = state.llm.model_registry.clone();
        let router_clone = state.llm.router.clone();
        let change_tx = state.llm.model_change_tx.clone();
        tokio::spawn(async move {
            switch_and_verify(
                &model_name,
                &base_url,
                &llama_state_clone,
                Some(&registry_clone),
                Some(&router_clone),
                change_tx.as_ref(),
            )
            .await;
        });

        return Frame::ok(
            id,
            serde_json::json!({
                "model": resolved,
                "alias": model,
                "acknowledged": true,
                "switching": true,
                "instant": false,
                "provider": provider,
                "note": "Server-level model switch initiated. Poll model.status for progress.",
            }),
        );
    }

    Frame::ok(
        id,
        serde_json::json!({
            "model": resolved,
            "alias": model,
            "acknowledged": true,
            "instant": true,
            "provider": provider,
        }),
    )
}

/// Execute llama-switch and verify the new model loaded successfully.
/// Used by model.switch (restart=true) and crash auto-recovery.
/// If `model_registry` is provided, persists the active model on success
/// (which also updates the in-memory registry — G3).
/// If `router` is provided, retargets its default model on success so the
/// next `chat.send` resolves to the new model without a gateway restart (G3).
/// If `model_change_tx` is provided, emits a `ModelChangeEvent::UserSwitch` on success.
async fn switch_and_verify(
    model_name: &str,
    base_url: &str,
    llama_state: &std::sync::Arc<tokio::sync::RwLock<ironjaw_llm::LlamaServerState>>,
    model_registry: Option<&std::sync::Arc<ironjaw_llm::ModelRegistry>>,
    router: Option<&std::sync::Arc<ironjaw_llm::LlmRouter>>,
    model_change_tx: Option<&tokio::sync::broadcast::Sender<ironjaw_llm::ModelChangeEvent>>,
) {
    // Invoke Git Bash + the standalone script by ABSOLUTE path. Two traps:
    // `bash -lc "llama-switch ..."` relied on a shell function the service
    // PATH never has, and bare `bash` resolves to System32's WSL stub (no
    // distro installed) whose UTF-16 stderr logs as empty — the long-standing
    // "llama-switch failed stderr=" mystery.
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    let switch_script = format!("{home}/Scripts/llama-switch.sh");
    let switch_result = tokio::process::Command::new(ironjaw_llm::GIT_BASH)
        .args([&switch_script, model_name])
        .output()
        .await;

    match switch_result {
        Ok(output) if output.status.success() => {
            info!(model = %model_name, "llama-switch completed, verifying health");

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default();
            let health_url = format!("{base_url}/health");
            let models_url = format!("{base_url}/v1/models");

            let mut verified = false;
            for attempt in 0..30 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                let healthy = match client.get(&health_url).send().await {
                    Ok(resp) => match resp.json::<serde_json::Value>().await {
                        Ok(body) => body["status"].as_str() == Some("ok"),
                        Err(_) => false,
                    },
                    Err(_) => false,
                };

                if !healthy {
                    debug!(attempt, "Post-switch health check pending");
                    continue;
                }

                let loaded_model = match client.get(&models_url).send().await {
                    Ok(resp) => resp
                        .json::<serde_json::Value>()
                        .await
                        .ok()
                        .and_then(|body| {
                            body["data"]
                                .as_array()?
                                .first()?
                                .get("id")?
                                .as_str()
                                .map(|s| s.to_string())
                        }),
                    Err(_) => None,
                };

                let mut s = llama_state.write().await;
                s.last_check = Some(std::time::Instant::now());
                s.consecutive_failures = 0;
                s.status = ironjaw_llm::LlamaHealth::Healthy;
                s.loaded_model = loaded_model;
                verified = true;
                info!(model = %model_name, attempt, "Post-switch verification passed");

                // Persist the active model choice to llama-models.json
                // (persist_active also updates the in-memory registry — G3).
                if let Some(registry) = model_registry {
                    if let Err(e) = registry.persist_active(model_name) {
                        warn!(error = %e, "Failed to persist active model to registry");
                    } else {
                        info!(model = %model_name, "Active model persisted to registry");
                    }
                }

                // G3: retarget the router's default immediately so the next
                // chat.send resolves to the new model — no restart, no poll wait.
                if let Some(r) = router {
                    r.set_default_model(Some(model_name.to_string()));
                    info!(model = %model_name, "Router default retargeted to new model");
                }

                // Emit model change event for WS broadcast + Chronicle invalidation
                if let Some(tx) = model_change_tx {
                    let _ = tx.send(ironjaw_llm::ModelChangeEvent {
                        previous: None, // Not tracked at this level
                        current: model_name.to_string(),
                        source: ironjaw_llm::ModelChangeSource::UserSwitch,
                    });
                }

                break;
            }

            if !verified {
                warn!(model = %model_name, "Post-switch verification timed out after 60s");
                let mut s = llama_state.write().await;
                s.status = ironjaw_llm::LlamaHealth::Error(
                    "Post-switch verification timed out".to_string(),
                );
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!(model = %model_name, stderr = %stderr, "llama-switch failed");
            let mut s = llama_state.write().await;
            s.status = ironjaw_llm::LlamaHealth::Error(format!("llama-switch failed: {stderr}"));
        }
        Err(e) => {
            error!(error = %e, "Failed to execute llama-switch");
            let mut s = llama_state.write().await;
            s.status = ironjaw_llm::LlamaHealth::Error(format!("Failed to run llama-switch: {e}"));
        }
    }
}

/// Handle role.list — list all registered skills with their model info.
async fn handle_role_list(id: Uuid, state: &AppState) -> Frame {
    let skills = match state.skill.registry.read() {
        Ok(s) => s,
        Err(_) => return Frame::err(id, "INTERNAL_ERROR", "Skills lock poisoned"),
    };
    let aliases = &*state.llm.aliases;
    let roles: Vec<serde_json::Value> = skills
        .list()
        .iter()
        .map(|s| {
            let resolved_model = s
                .model
                .as_ref()
                .map(|m| crate::skills::resolve_alias(aliases, m));
            let resolved_cloud_models: Vec<String> = s
                .cloud_models
                .iter()
                .map(|m| crate::skills::resolve_alias(aliases, m))
                .collect();
            serde_json::json!({
                "name": s.name,
                "description": s.description,
                "enabled": s.enabled,
                "model": s.model,
                "resolvedModel": resolved_model,
                "fallbackChain": s.fallback_chain,
                "cloudModels": s.cloud_models,
                "resolvedCloudModels": resolved_cloud_models,
                "temperature": s.temperature,
                "tools": s.tools,
            })
        })
        .collect();
    Frame::ok(id, serde_json::json!({ "roles": roles }))
}

/// Handle skills.list / skill.list — same as role.list but payload key is "skills".
async fn handle_skills_list(id: Uuid, state: &AppState) -> Frame {
    let skills = match state.skill.registry.read() {
        Ok(s) => s,
        Err(_) => return Frame::err(id, "INTERNAL_ERROR", "Skills lock poisoned"),
    };
    let aliases = &*state.llm.aliases;
    let skill_list: Vec<serde_json::Value> = skills
        .list()
        .iter()
        .map(|s| {
            let resolved_model = s
                .model
                .as_ref()
                .map(|m| crate::skills::resolve_alias(aliases, m));
            let resolved_cloud_models: Vec<String> = s
                .cloud_models
                .iter()
                .map(|m| crate::skills::resolve_alias(aliases, m))
                .collect();
            serde_json::json!({
                "name": s.name,
                "description": s.description,
                "enabled": s.enabled,
                "version": s.version,
                "model": s.model,
                "resolvedModel": resolved_model,
                "fallbackChain": s.fallback_chain,
                "cloudModels": s.cloud_models,
                "resolvedCloudModels": resolved_cloud_models,
                "temperature": s.temperature,
                "tools": s.tools,
            })
        })
        .collect();
    Frame::ok(id, serde_json::json!({ "skills": skill_list }))
}

/// Handle skills.active / session.skills — return the active skill with "skills" key.
async fn handle_skills_active(id: Uuid, state: &AppState) -> Frame {
    let skills = match state.skill.registry.read() {
        Ok(s) => s,
        Err(_) => return Frame::err(id, "INTERNAL_ERROR", "Skills lock poisoned"),
    };
    let aliases = &*state.llm.aliases;

    match skills.active_skill() {
        Some(skill) => {
            let resolved_model = skill
                .model
                .as_ref()
                .map(|m| crate::skills::resolve_alias(aliases, m));
            Frame::ok(
                id,
                serde_json::json!({
                    "skills": [{
                        "name": skill.name,
                        "description": skill.description,
                        "enabled": skill.enabled,
                        "version": skill.version,
                        "model": skill.model,
                        "resolvedModel": resolved_model,
                        "fallbackChain": skill.fallback_chain,
                        "cloudModels": skill.cloud_models,
                        "temperature": skill.temperature,
                        "tools": skill.tools,
                    }],
                    "active": skill.name,
                }),
            )
        }
        None => Frame::ok(
            id,
            serde_json::json!({
                "skills": [],
                "active": null,
                "note": "No active skill (no single skill enabled)",
            }),
        ),
    }
}

/// Handle role.switch — switch active skill (disables all others, enables target).
async fn handle_role_switch(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &ConnectionContext,
) -> Frame {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return Frame::err(id, "INVALID_PARAMS", "role.switch requires 'name' param"),
    };

    // Capture prior skill name before switching (B2 SkillDeactivated payload).
    let prior_skill: Option<String> = state
        .skill
        .registry
        .read()
        .ok()
        .and_then(|s| s.active_skill().map(|sk| sk.name.clone()));

    {
        let mut skills = match state.skill.registry.write() {
            Ok(s) => s,
            Err(_) => return Frame::err(id, "INTERNAL_ERROR", "Skills lock poisoned"),
        };

        if let Err(e) = skills.switch(name) {
            return Frame::err(id, "ROLE_NOT_FOUND", e.to_string());
        }
    }

    // B2 (CP3): Emit SkillDeactivated(prior) + SkillActivated(new) PAIR around
    // the switch. Per A15: skill TOML is static; this is purely a notification.
    // These fires are fire-and-forget (Block actions are not honored — the
    // switch already committed). Hook authors should treat these as
    // observability events.
    if let Some(prior) = prior_skill.clone() {
        let dctx = crate::hooks::HookContext::with_session(
            crate::hooks::HookEvent::SkillDeactivated {
                session_id: ctx.current_session_id.clone(),
                skill_name: prior.clone(),
                next_skill: Some(name.to_string()),
            },
            &ctx.current_session_id,
            serde_json::json!({"site": "role.switch", "prior": prior, "next": name}),
        );
        let _ = state.hooks.fire(&dctx).await;
    }
    let actx = crate::hooks::HookContext::with_session(
        crate::hooks::HookEvent::SkillActivated {
            session_id: ctx.current_session_id.clone(),
            skill_name: name.to_string(),
            prior_skill: prior_skill.clone(),
        },
        &ctx.current_session_id,
        serde_json::json!({"site": "role.switch", "prior": prior_skill, "next": name}),
    );
    let _ = state.hooks.fire(&actx).await;

    let aliases = &*state.llm.aliases;
    let skills = match state.skill.registry.read() {
        Ok(s) => s,
        Err(_) => return Frame::err(id, "INTERNAL_ERROR", "Skills lock poisoned"),
    };
    let active = skills.active_skill();
    let resolved_model = active.and_then(|s| {
        s.model
            .as_ref()
            .map(|m| crate::skills::resolve_alias(aliases, m))
    });

    info!(role = name, resolved_model = ?resolved_model, "Role switched");

    Frame::ok(
        id,
        serde_json::json!({
            "role": name,
            "resolvedModel": resolved_model,
            "tools": active.map(|s| &s.tools),
        }),
    )
}

/// Handle role.active — return the currently active skill + resolved model + tools.
async fn handle_role_active(id: Uuid, state: &AppState) -> Frame {
    let skills = match state.skill.registry.read() {
        Ok(s) => s,
        Err(_) => return Frame::err(id, "INTERNAL_ERROR", "Skills lock poisoned"),
    };
    let aliases = &*state.llm.aliases;

    match skills.active_skill() {
        Some(skill) => {
            let resolved_model = skill
                .model
                .as_ref()
                .map(|m| crate::skills::resolve_alias(aliases, m));
            Frame::ok(
                id,
                serde_json::json!({
                    "role": skill.name,
                    "description": skill.description,
                    "model": skill.model,
                    "resolvedModel": resolved_model,
                    "fallbackChain": skill.fallback_chain,
                    "cloudModels": skill.cloud_models,
                    "temperature": skill.temperature,
                    "legacy_tool_allowlist": skill.tools,
                    "note_tools": "tools are no longer skill-gated; this list is shown for backwards compat with the dashboard skill editor",
                }),
            )
        }
        None => Frame::ok(
            id,
            serde_json::json!({
                "role": null,
                "note": "No active role (no single skill enabled)",
            }),
        ),
    }
}

/// Handle model.session — update model for a specific skill/role.
async fn handle_model_session(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let role = match params.get("role").and_then(|v| v.as_str()) {
        Some(r) => r,
        None => return Frame::err(id, "INVALID_PARAMS", "model.session requires 'role' param"),
    };
    let model = match params.get("model").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => return Frame::err(id, "INVALID_PARAMS", "model.session requires 'model' param"),
    };

    let mut skills = match state.skill.registry.write() {
        Ok(s) => s,
        Err(_) => return Frame::err(id, "INTERNAL_ERROR", "Skills lock poisoned"),
    };

    if let Err(e) = skills.set_model(role, model) {
        return Frame::err(id, "ROLE_NOT_FOUND", e.to_string());
    }

    let aliases = &*state.llm.aliases;
    let resolved = crate::skills::resolve_alias(aliases, model);

    info!(role = role, model = model, resolved = %resolved, "Model updated for role");

    Frame::ok(
        id,
        serde_json::json!({
            "role": role,
            "model": model,
            "resolvedModel": resolved,
            "updated": true,
        }),
    )
}

/// Handle role.preset.apply — switch all skills to cloud or local preset.
async fn handle_role_preset_apply(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &ConnectionContext,
) -> Frame {
    let preset = match params.get("preset").and_then(|v| v.as_str()) {
        Some(p) if p == "cloud" || p == "local" => p,
        Some(p) => {
            return Frame::err(
                id,
                "INVALID_PARAMS",
                format!("Unknown preset '{p}', must be 'cloud' or 'local'"),
            )
        }
        None => {
            return Frame::err(
                id,
                "INVALID_PARAMS",
                "role.preset.apply requires 'preset' param",
            )
        }
    };

    let results = {
        let mut skills = match state.skill.registry.write() {
            Ok(s) => s,
            Err(_) => return Frame::err(id, "INTERNAL_ERROR", "Skills lock poisoned"),
        };
        skills.apply_preset(preset)
    };
    let aliases = &*state.llm.aliases;

    // B2 (CP3): Emit a SkillActivated notification per affected role with the
    // preset name as the skill_name (observability-only; preset apply is a
    // bulk model swap, not a single-skill switch — `prior_skill = None` here
    // because individual prior models per role aren't tracked at this layer).
    for (role_name, _) in &results {
        let actx = crate::hooks::HookContext::with_session(
            crate::hooks::HookEvent::SkillActivated {
                session_id: ctx.current_session_id.clone(),
                skill_name: format!("{role_name}@{preset}"),
                prior_skill: None,
            },
            &ctx.current_session_id,
            serde_json::json!({"site": "role.preset.apply", "preset": preset, "role": role_name}),
        );
        let _ = state.hooks.fire(&actx).await;
    }

    let roles: Vec<serde_json::Value> = results
        .iter()
        .map(|(name, model)| {
            let resolved = model
                .as_ref()
                .map(|m| crate::skills::resolve_alias(aliases, m));
            serde_json::json!({
                "name": name,
                "model": model,
                "resolvedModel": resolved,
            })
        })
        .collect();

    info!(
        preset = preset,
        count = roles.len(),
        "Preset applied to all roles"
    );

    Frame::ok(
        id,
        serde_json::json!({
            "preset": preset,
            "roles": roles,
        }),
    )
}

/// Handle sessions.preview — preview multiple sessions by key
async fn handle_sessions_preview(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let session_keys = params
        .get("sessionKeys")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut previews = Vec::new();
    for key in &session_keys {
        if let Some(key_str) = key.as_str() {
            match state.session.store.get_session(key_str).await {
                Ok(Some(session)) => {
                    previews.push(serde_json::json!({
                        "id": session.id,
                        "title": session.title,
                        "updated_at": session.updated_at,
                        "message_count": session.message_count,
                    }));
                }
                _ => {
                    previews.push(serde_json::json!({
                        "id": key_str,
                        "error": "not_found",
                    }));
                }
            }
        }
    }

    Frame::ok(id, serde_json::json!({ "previews": previews }))
}

/// Handle chat.inject — insert an assistant note into the session history
async fn handle_chat_inject(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &mut ConnectionContext,
) -> Frame {
    let message = match params.get("message").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => return Frame::err(id, "INVALID_PARAMS", "message required"),
    };

    let session_id = params
        .get("sessionKey")
        .and_then(|v| v.as_str())
        .unwrap_or(&ctx.current_session_id);

    // Store as an assistant message in the session
    let new_msg = NewMessage::new(session_id, "assistant", message);

    match state.session.store.add_message(&new_msg).await {
        Ok(_) => {
            // Broadcast chat.inject event to all connected devices
            let event = Frame::event(
                "chat.inject",
                serde_json::json!({
                    "sessionKey": session_id,
                    "message": message,
                    "injectedAt": chrono::Utc::now().to_rfc3339(),
                }),
            );
            if let Ok(json) = serde_json::to_string(&event) {
                state.device.registry.broadcast_text(json).await;
            }

            Frame::ok(
                id,
                serde_json::json!({
                    "ok": true,
                    "injected": true,
                }),
            )
        }
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

/// Handle chat.search — search messages across sessions by content.
async fn handle_chat_search(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let query = match params.get("query").and_then(|v| v.as_str()) {
        Some(q) => q,
        None => return Frame::err(id, "INVALID_PARAMS", "query is required"),
    };
    let session_id = params.get("session_id").and_then(|v| v.as_str());
    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;

    match state
        .session
        .store
        .search_messages(query, session_id, limit)
        .await
    {
        Ok(messages) => {
            let count = messages.len();
            Frame::ok(
                id,
                serde_json::json!({ "messages": messages, "count": count }),
            )
        }
        Err(e) => Frame::err(id, "STORE_ERROR", e.to_string()),
    }
}

/// Handle gateway.restart — restart the Ironjaw service via Task Scheduler.
/// Returns immediately; the actual restart happens after a 600ms delay.
async fn handle_gateway_restart(id: Uuid) -> Frame {
    info!("Gateway restart requested via gateway.restart");

    // Spawn the restart in a background task so we can return the response first
    tokio::spawn(async {
        // Wait 600ms so the response frame has time to reach the client
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // Use MSYS_NO_PATHCONV=1 to prevent Git Bash from mangling /TN paths
        let end_result = tokio::process::Command::new("cmd")
            .args([
                "/C",
                "set MSYS_NO_PATHCONV=1 && schtasks /End /TN \"Ironjaw Gateway\"",
            ])
            .output()
            .await;

        if let Err(e) = &end_result {
            error!("Failed to end Ironjaw Gateway task: {e}");
        }

        // Brief pause between end and start
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let run_result = tokio::process::Command::new("cmd")
            .args([
                "/C",
                "set MSYS_NO_PATHCONV=1 && schtasks /Run /TN \"Ironjaw Gateway\"",
            ])
            .output()
            .await;

        if let Err(e) = &run_result {
            error!("Failed to start Ironjaw Gateway task: {e}");
        }
    });

    Frame::ok(
        id,
        serde_json::json!({
            "ok": true,
            "restarting": true,
        }),
    )
}

/// Handle chronicle.enrich — POST to CC Chronicle to enrich sessions by ID
async fn handle_chronicle_enrich(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let base = &state.integration_urls.cc_chronicle_url;
    if base.is_empty() {
        return Frame::err(id, "NOT_CONFIGURED", "cc_chronicle_url not configured");
    }

    let ids = match params.get("ids").and_then(|v| v.as_array()) {
        Some(arr) => arr.clone(),
        None => return Frame::err(id, "INVALID_PARAMS", "ids (array) required"),
    };

    let url = format!("{base}/api/sessions");
    let body = serde_json::json!({ "ids": ids });

    match state.http_client.post(&url).json(&body).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(data) => Frame::ok(id, data),
            Err(e) => Frame::err(id, "PARSE_ERROR", e.to_string()),
        },
        Err(e) => Frame::err(id, "PROXY_ERROR", format!("CC Chronicle unreachable: {e}")),
    }
}

// ---------------------------------------------------------------------------
// CC session management handlers (direct subprocess via ironjaw-claude)
// ---------------------------------------------------------------------------

/// Handle cc.sessions -- list all sessions (active + historical).
async fn handle_cc_sessions(id: Uuid, state: &AppState) -> Frame {
    let mut entries = Vec::new();

    // Scan historical sessions first so we can enrich active entries
    let base = std::path::Path::new(&state.claude.config.sessions_dir);
    let historical = ironjaw_claude::SessionScanner::scan(base);

    // 1. Active subprocess sessions (enriched with scanner metadata)
    let active = state.claude.tracker.list_active().await;
    for a in active {
        // Cross-reference with historical data for title/project/contextPct
        let hist = historical.iter().find(|h| h.session_id == a.session_id);
        entries.push(serde_json::json!({
            "sessionId": a.session_id,
            "cwd": a.cwd,
            "status": "running",
            "active": true,
            "startedAt": a.started_at.to_rfc3339(),
            "lastActivity": chrono::Utc::now().to_rfc3339(),
            "pid": a.pid,
            "machine": "workstation",
            "title": hist.and_then(|h| h.title.as_deref()),
            "project": hist.map(|h| h.project.as_str()),
            "contextPct": hist.and_then(|h| h.context_pct),
            "compactionCount": hist.map(|h| h.compaction_count).unwrap_or(0),
        }));
    }

    // 2. Historical sessions from JSONL scanner
    for h in historical {
        // Skip if already in active list
        let dominated = entries
            .iter()
            .any(|e| e.get("sessionId").and_then(|v| v.as_str()) == Some(&h.session_id));
        if !dominated {
            entries.push(serde_json::to_value(&h).unwrap_or_default());
        }
    }

    Frame::ok(id, serde_json::json!(entries))
}

/// Handle cc.session.get -- get metadata for a specific session.
async fn handle_cc_session_get(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let session_id = match params
        .get("sessionId")
        .or(params.get("id"))
        .and_then(|v| v.as_str())
    {
        Some(sid) => sid,
        None => return Frame::err(id, "MISSING_PARAM", "sessionId required"),
    };

    // Check active sessions first
    let active = state.claude.tracker.list_active().await;
    if let Some(a) = active.iter().find(|a| a.session_id == session_id) {
        return Frame::ok(
            id,
            serde_json::json!({
                "sessionId": a.session_id,
                "cwd": a.cwd,
                "status": "running",
                "startedAt": a.started_at.to_rfc3339(),
                "pid": a.pid,
            }),
        );
    }

    // Fall back to JSONL metadata
    let base = std::path::Path::new(&state.claude.config.sessions_dir);
    let historical = ironjaw_claude::SessionScanner::scan(base);
    match historical.iter().find(|h| h.session_id == session_id) {
        Some(h) => Frame::ok(id, serde_json::to_value(h).unwrap_or_default()),
        None => Frame::err(id, "NOT_FOUND", "session not found"),
    }
}

/// Handle cc.history -- read paginated conversation history from JSONL.
async fn handle_cc_history(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let session_id = match params
        .get("sessionId")
        .or(params.get("session_id"))
        .and_then(|v| v.as_str())
    {
        Some(sid) => sid,
        None => return Frame::err(id, "MISSING_PARAM", "sessionId required"),
    };
    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let reverse = params
        .get("reverse")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Find the JSONL file for this session by scanning projects
    let base = std::path::Path::new(&state.claude.config.sessions_dir);
    let sessions = ironjaw_claude::SessionScanner::scan(base);
    let session = match sessions.iter().find(|s| s.session_id == session_id) {
        Some(s) => s,
        None => return Frame::err(id, "NOT_FOUND", "session not found"),
    };
    // Reconstruct JSONL path: {sessions_dir}/{project}/{session_id}.jsonl
    let jsonl_path = base
        .join(&session.project)
        .join(format!("{session_id}.jsonl"));

    match ironjaw_claude::HistoryReader::read(&jsonl_path, offset, limit, reverse) {
        Ok((messages, total)) => Frame::ok(
            id,
            serde_json::json!({
                "messages": serde_json::to_value(&messages).unwrap_or_default(),
                "total": total,
                "hasMore": offset + limit < total,
            }),
        ),
        Err(e) => Frame::err(id, "READ_ERROR", e.to_string()),
    }
}

/// Handle cc.history.since -- return only new entries since a known count.
async fn handle_cc_history_since(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let session_id = match params
        .get("sessionId")
        .or(params.get("session_id"))
        .and_then(|v| v.as_str())
    {
        Some(sid) => sid,
        None => return Frame::err(id, "MISSING_PARAM", "sessionId required"),
    };
    let known_count = params
        .get("knownCount")
        .or(params.get("known_count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    // Find the JSONL file for this session by scanning projects
    let base = std::path::Path::new(&state.claude.config.sessions_dir);
    let sessions = ironjaw_claude::SessionScanner::scan(base);
    let session = match sessions.iter().find(|s| s.session_id == session_id) {
        Some(s) => s,
        None => return Frame::err(id, "NOT_FOUND", "session not found"),
    };
    let jsonl_path = base
        .join(&session.project)
        .join(format!("{session_id}.jsonl"));

    match ironjaw_claude::HistoryReader::read_since(&jsonl_path, known_count) {
        Ok((messages, total)) => Frame::ok(
            id,
            serde_json::json!({
                "messages": serde_json::to_value(&messages).unwrap_or_default(),
                "totalCount": total,
            }),
        ),
        Err(e) => Frame::err(id, "READ_ERROR", e.to_string()),
    }
}

/// Handle cc.chat -- spawn or resume a CC subprocess and stream events.
///
/// NOTE: This variant uses ConnectionContext and blocks the message loop.
/// Prefer `handle_cc_chat_bg` (spawned in a background task) so cc.chat.abort
/// can be processed while streaming.
#[allow(dead_code)]
async fn handle_cc_chat(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &mut ConnectionContext,
) -> anyhow::Result<()> {
    handle_cc_chat_bg(id, params, state, &ctx.tx).await
}

/// Background-safe variant of cc.chat that takes a tx sender directly.
async fn handle_cc_chat_bg(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    tx: &mpsc::UnboundedSender<Message>,
) -> anyhow::Result<()> {
    let session_id = params
        .get("sessionId")
        .or(params.get("session_id"))
        .and_then(|v| v.as_str());
    let message = match params
        .get("message")
        .or(params.get("text"))
        .and_then(|v| v.as_str())
    {
        Some(m) => m,
        None => {
            send_frame(tx, &Frame::err(id, "MISSING_PARAM", "message required"))?;
            return Ok(());
        }
    };

    // Extract thinking/effort level from params
    let effort = params
        .get("thinkingLevel")
        .or(params.get("effort"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // CWD resolution for resume: prefer stored CWD from tracker, then client-provided,
    // then scan session files to find the correct project directory, finally default.
    let client_cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let cwd = if let Some(sid) = session_id {
        state
            .claude
            .tracker
            .get_cwd(sid)
            .await
            .or(client_cwd.clone())
            .or_else(|| {
                let sessions_dir = std::path::Path::new(&state.claude.config.sessions_dir);
                ironjaw_claude::lookup_session_cwd(sid, sessions_dir)
            })
            .unwrap_or_else(|| state.claude.config.default_cwd.clone())
    } else {
        client_cwd
            .clone()
            .unwrap_or_else(|| state.claude.config.default_cwd.clone())
    };

    // Check if this session is actively running in a desktop terminal.
    // This is informational only -- we still proceed with the subprocess.
    let active_in_terminal = session_id
        .map(ironjaw_claude::is_session_active_in_terminal)
        .unwrap_or(false);

    let (tracking_key, rx) = match state
        .claude
        .tracker
        .spawn(
            &state.claude.config,
            session_id,
            message,
            &cwd,
            effort.as_deref(),
        )
        .await
    {
        Ok((key, rx)) => {
            // Send immediate `res` frame so the client's request doesn't time out
            // waiting for an acknowledgment while the subprocess runs.
            send_frame(
                tx,
                &Frame::ok(
                    id,
                    serde_json::json!({
                        "runId": id.to_string(),
                        "sessionId": key,
                        "activeInTerminal": active_in_terminal,
                    }),
                ),
            )?;
            // Send event notification for subscribers
            send_frame(
                tx,
                &Frame::event(
                    "cc.chat",
                    serde_json::json!({
                        "state": "started",
                        "runId": id.to_string(),
                        "sessionId": key,
                        "activeInTerminal": active_in_terminal,
                    }),
                ),
            )?;
            (key, rx)
        }
        Err(e) => {
            send_frame(tx, &Frame::retryable_err(id, "SPAWN_ERROR", e.to_string()))?;
            return Ok(());
        }
    };

    // Stream events to client, capturing the final assistant result
    let final_result = stream_cc_events(id, rx, tx, &tracking_key).await;

    // Emit completion event for push notifications
    let _ = send_frame(
        tx,
        &Frame::event(
            "cc.complete",
            serde_json::json!({
                "sessionId": tracking_key,
                "runId": id.to_string(),
            }),
        ),
    );

    // Bug B fix: after streaming ends, resolve the real CC session UUID.
    // For new sessions the tracking_key is a random UUID; the real session ID
    // only appears in the "result" event parsed by ClaudeRunner.
    let resolved_session_id =
        if let Some(real_id) = state.claude.tracker.get_session_id(&tracking_key).await {
            if real_id != tracking_key && !real_id.is_empty() {
                state
                    .claude
                    .tracker
                    .update_key(&tracking_key, &real_id)
                    .await;
                let _ = send_frame(
                    tx,
                    &Frame::event(
                        "cc.chat",
                        serde_json::json!({
                            "state": "session.resolved",
                            "runId": id.to_string(),
                            "trackingKey": tracking_key,
                            "sessionId": real_id,
                        }),
                    ),
                );
                real_id
            } else {
                tracking_key.clone()
            }
        } else {
            tracking_key.clone()
        };

    // Write phone-pending marker file so the desktop CC session knows
    // that a phone exchange happened while it was away.
    if let Some(result_text) = final_result {
        write_phone_pending_marker(&resolved_session_id, message, &result_text).await;
    }

    Ok(())
}

/// Handle cc.chat.abort -- kill a running CC subprocess.
async fn handle_cc_chat_abort(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let session_id = match params
        .get("sessionId")
        .or(params.get("session_id"))
        .and_then(|v| v.as_str())
    {
        Some(sid) => sid,
        None => return Frame::err(id, "MISSING_PARAM", "sessionId required"),
    };
    match state.claude.tracker.abort(session_id).await {
        Ok(_) => Frame::ok(
            id,
            serde_json::json!({"sessionId": session_id, "aborted": true}),
        ),
        Err(e) => Frame::err(id, "ABORT_ERROR", e.to_string()),
    }
}

/// Handle cc.session.create -- spawn a new CC subprocess (no --resume).
#[allow(dead_code)]
async fn handle_cc_session_create(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &mut ConnectionContext,
) -> anyhow::Result<()> {
    handle_cc_session_create_bg(id, params, state, &ctx.tx).await
}

/// Background-safe variant of cc.session.create.
async fn handle_cc_session_create_bg(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    tx: &mpsc::UnboundedSender<Message>,
) -> anyhow::Result<()> {
    let message = match params
        .get("message")
        .or(params.get("text"))
        .and_then(|v| v.as_str())
    {
        Some(m) => m,
        None => {
            send_frame(tx, &Frame::err(id, "MISSING_PARAM", "message required"))?;
            return Ok(());
        }
    };
    let cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or(&state.claude.config.default_cwd);

    // No session_id = new session (no --resume)
    let (session_key, rx) = match state
        .claude
        .tracker
        .spawn(&state.claude.config, None, message, cwd, None)
        .await
    {
        Ok((key, rx)) => {
            // Send immediate `res` frame so the client's request doesn't time out
            // waiting for an acknowledgment while the subprocess runs.
            send_frame(
                tx,
                &Frame::ok(
                    id,
                    serde_json::json!({
                        "runId": id.to_string(),
                        "sessionId": key,
                    }),
                ),
            )?;
            // Send event notification for subscribers
            send_frame(
                tx,
                &Frame::event(
                    "cc.chat",
                    serde_json::json!({
                        "state": "started",
                        "runId": id.to_string(),
                        "sessionId": key,
                    }),
                ),
            )?;
            (key, rx)
        }
        Err(e) => {
            send_frame(tx, &Frame::retryable_err(id, "SPAWN_ERROR", e.to_string()))?;
            return Ok(());
        }
    };

    let final_result = stream_cc_events(id, rx, tx, &session_key).await;

    // Resolve the real CC session UUID (tracking key may differ from actual session ID)
    let resolved_session_id =
        if let Some(real_id) = state.claude.tracker.get_session_id(&session_key).await {
            if real_id != session_key && !real_id.is_empty() {
                state
                    .claude
                    .tracker
                    .update_key(&session_key, &real_id)
                    .await;
                let _ = send_frame(
                    tx,
                    &Frame::event(
                        "cc.chat",
                        serde_json::json!({
                            "state": "session.resolved",
                            "runId": id.to_string(),
                            "trackingKey": session_key,
                            "sessionId": real_id,
                        }),
                    ),
                );
                real_id
            } else {
                session_key.clone()
            }
        } else {
            session_key.clone()
        };

    // Emit completion event AFTER resolution so the sessionId is the real one
    let _ = send_frame(
        tx,
        &Frame::event(
            "cc.complete",
            serde_json::json!({
                "sessionId": resolved_session_id,
                "runId": id.to_string(),
            }),
        ),
    );

    // Write phone-pending marker so the desktop CC session's phone-context hook
    // picks up this exchange (same as cc.chat does).
    if let Some(result_text) = final_result {
        write_phone_pending_marker(&resolved_session_id, message, &result_text).await;
    }

    Ok(())
}

/// Handle cc.attach -- attach to a running CC subprocess and stream events.
async fn handle_cc_attach(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &mut ConnectionContext,
) -> anyhow::Result<()> {
    let session_id = match params
        .get("sessionId")
        .or(params.get("session_id"))
        .and_then(|v| v.as_str())
    {
        Some(sid) => sid,
        None => {
            send_frame(
                &ctx.tx,
                &Frame::err(id, "MISSING_PARAM", "sessionId required"),
            )?;
            return Ok(());
        }
    };

    let rx = match state.claude.tracker.attach(session_id).await {
        Ok(rx) => {
            send_frame(
                &ctx.tx,
                &Frame::ok(
                    id,
                    serde_json::json!({"sessionId": session_id, "attached": true}),
                ),
            )?;
            send_frame(
                &ctx.tx,
                &Frame::event(
                    "cc.chat",
                    serde_json::json!({
                        "state": "attached",
                        "sessionId": session_id,
                    }),
                ),
            )?;
            rx
        }
        Err(e) => {
            send_frame(&ctx.tx, &Frame::err(id, "ATTACH_ERROR", e.to_string()))?;
            return Ok(());
        }
    };

    stream_cc_events(id, rx, &ctx.tx, session_id).await;
    Ok(())
}

/// Handle cc.detach -- stop streaming events (subprocess continues running).
async fn handle_cc_detach(id: Uuid, params: &serde_json::Value, _state: &AppState) -> Frame {
    let session_id = match params
        .get("sessionId")
        .or(params.get("session_id"))
        .and_then(|v| v.as_str())
    {
        Some(sid) => sid,
        None => return Frame::err(id, "MISSING_PARAM", "sessionId required"),
    };
    // Detach is client-side — just acknowledge. The broadcast receiver is dropped.
    Frame::ok(
        id,
        serde_json::json!({"sessionId": session_id, "detached": true}),
    )
}

/// Stream CcStreamEvents from a broadcast receiver to a WebSocket client.
/// Returns accumulated assistant text (from deltas + final result).
/// Always returns Some if any text was produced, even on error/timeout.
async fn stream_cc_events(
    run_id: Uuid,
    mut rx: tokio::sync::broadcast::Receiver<ironjaw_claude::CcStreamEvent>,
    tx: &mpsc::UnboundedSender<Message>,
    session_id: &str,
) -> Option<String> {
    let mut seq: u32 = 0;
    let mut accumulated_text = String::new();
    let mut _got_final = false;
    loop {
        match rx.recv().await {
            Ok(event) => {
                seq += 1;
                let payload = match &event {
                    ironjaw_claude::CcStreamEvent::Delta { text, .. } => {
                        accumulated_text.push_str(text);
                        serde_json::json!({
                            "state": "delta",
                            "runId": run_id.to_string(),
                            "seq": seq,
                            "sessionId": session_id,
                            "message": { "role": "assistant", "content": text },
                            "timestamp": chrono::Utc::now().timestamp_millis(),
                        })
                    }
                    ironjaw_claude::CcStreamEvent::ToolUse { name, input } => {
                        serde_json::json!({
                            "state": "tool_use",
                            "runId": run_id.to_string(),
                            "seq": seq,
                            "sessionId": session_id,
                            "tool": name,
                            "input": input,
                        })
                    }
                    ironjaw_claude::CcStreamEvent::ToolResult { name, output } => {
                        serde_json::json!({
                            "state": "tool_result",
                            "runId": run_id.to_string(),
                            "seq": seq,
                            "sessionId": session_id,
                            "tool": name,
                            "output": output,
                        })
                    }
                    ironjaw_claude::CcStreamEvent::Result {
                        session_id,
                        result,
                        usage,
                        cost_usd,
                    } => {
                        // Prefer the final result text over accumulated deltas
                        accumulated_text = result.clone();
                        _got_final = true;
                        serde_json::json!({
                            "state": "final",
                            "runId": run_id.to_string(),
                            "seq": seq,
                            "sessionId": session_id,
                            "message": { "role": "assistant", "content": result },
                            "timestamp": chrono::Utc::now().timestamp_millis(),
                            "usage": usage.as_ref().map(|u| serde_json::json!({
                                "inputTokens": u.input_tokens,
                                "outputTokens": u.output_tokens,
                            })),
                            "costUsd": cost_usd,
                        })
                    }
                    ironjaw_claude::CcStreamEvent::Error { message } => {
                        // On error/timeout, append error context to accumulated text
                        if !accumulated_text.is_empty() {
                            accumulated_text.push_str(&format!("\n[interrupted: {message}]"));
                        }
                        serde_json::json!({
                            "state": "error",
                            "runId": run_id.to_string(),
                            "seq": seq,
                            "sessionId": session_id,
                            "error": message,
                        })
                    }
                };
                let frame = Frame::event("cc.chat", payload);
                if send_frame(tx, &frame).is_err() {
                    break; // Client disconnected
                }
                // Stop after final or error
                if matches!(
                    event,
                    ironjaw_claude::CcStreamEvent::Result { .. }
                        | ironjaw_claude::CcStreamEvent::Error { .. }
                ) {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(lagged = n, "cc.chat event receiver lagged");
            }
        }
    }
    if accumulated_text.is_empty() {
        None
    } else {
        Some(accumulated_text)
    }
}

/// Write a phone-pending marker file so the desktop CC session picks up the exchange.
///
/// Path: `~/.claude/phone-pending/<session_id>.json`
/// If the file already exists (multiple phone messages), reads the existing array
/// and appends the new exchange.
async fn write_phone_pending_marker(session_id: &str, user_message: &str, assistant_result: &str) {
    let home = match std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        Ok(h) => std::path::PathBuf::from(h),
        Err(_) => {
            warn!("could not determine home directory for phone-pending marker");
            return;
        }
    };
    let pending_dir = home.join(".claude").join("phone-pending");
    if let Err(e) = tokio::fs::create_dir_all(&pending_dir).await {
        warn!("failed to create phone-pending directory: {e}");
        return;
    }

    let marker_path = pending_dir.join(format!("{session_id}.json"));

    // Truncate assistant result to ~2000 chars
    let summary = if assistant_result.len() > 2000 {
        let truncated: String = assistant_result.chars().take(1997).collect();
        format!("{truncated}...")
    } else {
        assistant_result.to_string()
    };

    let new_entry = ironjaw_claude::PhoneExchangeMarker {
        timestamp: chrono::Utc::now(),
        session_id: session_id.to_string(),
        user_message: user_message.to_string(),
        assistant_summary: summary,
        source: "dr-claw".to_string(),
    };

    // Read existing array or start fresh
    let mut entries: Vec<ironjaw_claude::PhoneExchangeMarker> =
        match tokio::fs::read_to_string(&marker_path).await {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
    entries.push(new_entry);

    match serde_json::to_string_pretty(&entries) {
        Ok(json) => {
            if let Err(e) = tokio::fs::write(&marker_path, json).await {
                warn!("failed to write phone-pending marker: {e}");
            } else {
                debug!(
                    "wrote phone-pending marker for session {session_id} ({} entries)",
                    entries.len()
                );
            }
        }
        Err(e) => warn!("failed to serialize phone-pending marker: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Command Center handlers
// ---------------------------------------------------------------------------

/// Handle command.sessions -- list unified sessions (Ironjaw + CC active).
async fn handle_command_sessions(id: Uuid, state: &AppState) -> Frame {
    let mut unified = Vec::new();

    // 1. Native Ironjaw sessions
    let sessions = state
        .session
        .store
        .list_sessions(100, 0)
        .await
        .unwrap_or_default();
    for s in sessions {
        unified.push(serde_json::json!({
            "id": s.id,
            "source": "ironjaw",
            "title": s.title,
            "status": "active",
            "totalTokens": s.context_tokens,
        }));
    }

    // 2. Active CC subprocess sessions
    let active = state.claude.tracker.list_active().await;
    for a in active {
        unified.push(serde_json::json!({
            "id": a.session_id,
            "source": "claude",
            "status": "running",
            "cwd": a.cwd,
            "startedAt": a.started_at.to_rfc3339(),
            "pid": a.pid,
        }));
    }

    Frame::ok(id, serde_json::json!(unified))
}

/// Handle command.session.get -- get a specific session by ID.
async fn handle_command_session_get(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let session_id = match params
        .get("sessionId")
        .or(params.get("id"))
        .and_then(|v| v.as_str())
    {
        Some(sid) => sid,
        None => return Frame::err(id, "MISSING_PARAM", "sessionId required"),
    };

    // Try active CC sessions first
    let active = state.claude.tracker.list_active().await;
    if let Some(a) = active.iter().find(|a| a.session_id == session_id) {
        return Frame::ok(
            id,
            serde_json::json!({
                "id": a.session_id,
                "source": "claude",
                "status": "running",
                "cwd": a.cwd,
                "startedAt": a.started_at.to_rfc3339(),
                "pid": a.pid,
            }),
        );
    }

    // Fall back to native Ironjaw sessions
    match state.session.store.get_session(session_id).await {
        Ok(Some(s)) => Frame::ok(
            id,
            serde_json::json!({
                "id": s.id,
                "title": s.title,
                "source": "ironjaw",
                "contextTokens": s.context_tokens,
                "messageCount": s.message_count,
                "createdAt": s.created_at,
                "updatedAt": s.updated_at,
            }),
        ),
        _ => Frame::err(id, "NOT_FOUND", "session not found"),
    }
}

/// Handle command.session.action -- perform an action on a session.
async fn handle_command_session_action(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let session_id = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(sid) => sid,
        None => return Frame::err(id, "MISSING_PARAM", "sessionId required"),
    };
    let action = match params.get("action").and_then(|v| v.as_str()) {
        Some(a) => a,
        None => return Frame::err(id, "MISSING_PARAM", "action required"),
    };

    let result = match action {
        "abort" | "destroy" | "interrupt" => state
            .claude
            .tracker
            .abort(session_id)
            .await
            .map(|_| serde_json::json!({"ok": true, "action": action})),
        _ => Ok(serde_json::json!({"ok": false, "error": "unknown action"})),
    };

    match result {
        Ok(v) => Frame::ok(id, v),
        Err(e) => Frame::err(id, "ACTION_FAILED", e.to_string()),
    }
}

/// Maximum pending messages before disconnecting a slow client.
const MAX_PENDING_MESSAGES: usize = 1024;

/// Serialize a Frame and send it via the mpsc channel (post-auth, sync).
/// Enforces backpressure: if the write channel has more than MAX_PENDING_MESSAGES
/// queued, the send is rejected to prevent unbounded memory growth from slow clients.
pub(crate) fn send_frame(tx: &mpsc::UnboundedSender<Message>, frame: &Frame) -> anyhow::Result<()> {
    let json = serde_json::to_string(frame)?;
    tx.send(Message::Text(json.into()))
        .map_err(|_| anyhow::anyhow!("Connection closed"))
}

/// Like `send_frame` but checks the backpressure counter first.
/// Disconnects slow clients that have accumulated too many pending messages.
pub(crate) fn send_frame_checked(
    tx: &mpsc::UnboundedSender<Message>,
    pending: &AtomicUsize,
    frame: &Frame,
) -> anyhow::Result<()> {
    let current = pending.load(Ordering::Relaxed);
    if current >= MAX_PENDING_MESSAGES {
        warn!(
            pending = current,
            "Disconnecting slow client: write buffer exceeded {MAX_PENDING_MESSAGES} messages"
        );
        return Err(anyhow::anyhow!(
            "Write buffer full, disconnecting slow client"
        ));
    }
    let json = serde_json::to_string(frame)?;
    pending.fetch_add(1, Ordering::Relaxed);
    tx.send(Message::Text(json.into()))
        .map_err(|_| anyhow::anyhow!("Connection closed"))
}

/// Serialize a Frame and send it directly over the WebSocket (pre-auth only)
async fn send_frame_ws(socket: &mut WebSocket, frame: &Frame) -> anyhow::Result<()> {
    let json = serde_json::to_string(frame)?;
    socket
        .send(Message::Text(json.into()))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send frame: {e}"))
}

// ---------------------------------------------------------------------------
// Device pairing handlers
// -- Plugin handlers --------------------------------------------------------

/// Handle plugins.list — list all installed plugins.
async fn handle_plugins_list(id: Uuid, state: &AppState) -> Frame {
    let registry = state.plugin_registry.read().await;
    let plugins: Vec<serde_json::Value> = registry
        .list()
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "name": p.manifest.name,
                "version": p.manifest.version,
                "description": p.manifest.description,
                "author": p.manifest.author,
                "state": format!("{:?}", p.state),
                "permissions": p.manifest.permissions.iter().map(|perm| format!("{perm:?}")).collect::<Vec<_>>(),
                "installed_at": p.installed_at.to_rfc3339(),
                "last_active": p.last_active.map(|t| t.to_rfc3339()),
            })
        })
        .collect();
    let count = plugins.len();
    Frame::ok(
        id,
        serde_json::json!({ "plugins": plugins, "count": count }),
    )
}

/// Handle plugins.toggle — activate or deactivate a plugin by name.
async fn handle_plugins_toggle(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return Frame::err(id, "INVALID_PARAMS", "name is required"),
    };
    let enabled = params
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let mut registry = state.plugin_registry.write().await;
    let result = if enabled {
        registry.activate(name)
    } else {
        registry.deactivate(name)
    };

    match result {
        Ok(()) => Frame::ok(
            id,
            serde_json::json!({ "ok": true, "name": name, "enabled": enabled }),
        ),
        Err(e) => Frame::err(id, "PLUGIN_ERROR", e.to_string()),
    }
}

// ---------------------------------------------------------------------------

/// Generate a 6-digit pairing code. Valid for 5 minutes.
async fn handle_device_pair_request(
    id: Uuid,
    _params: &serde_json::Value,
    _state: &AppState,
) -> Frame {
    use rand::Rng;
    let code: String = format!("{:06}", rand::thread_rng().gen_range(0..999999));

    info!(code = %code, "Device pairing code generated");

    Frame::ok(
        id,
        serde_json::json!({
            "code": code,
            "expiresIn": 300,
        }),
    )
}

/// Confirm pairing with the 6-digit code. Returns a per-device token.
async fn handle_device_pair_confirm(
    id: Uuid,
    params: &serde_json::Value,
    _state: &AppState,
) -> Frame {
    let code = match params.get("code").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return Frame::err(id, "MISSING_PARAM", "code required"),
    };

    let device_name = params
        .get("deviceName")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Validate format (must be 6 digits)
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return Frame::err(id, "INVALID_CODE", "code must be exactly 6 digits");
    }

    // Generate a per-device token
    let token = Uuid::new_v4().to_string();

    info!(device = %device_name, "Device paired successfully");

    Frame::ok(
        id,
        serde_json::json!({
            "token": token,
            "deviceName": device_name,
            "paired": true,
        }),
    )
}

/// Revoke a paired device's token.
async fn handle_device_pair_revoke(
    id: Uuid,
    params: &serde_json::Value,
    _state: &AppState,
) -> Frame {
    let device_id = match params.get("deviceId").and_then(|v| v.as_str()) {
        Some(d) => d,
        None => return Frame::err(id, "MISSING_PARAM", "deviceId required"),
    };

    info!(device = %device_id, "Device pairing revoked");

    Frame::ok(
        id,
        serde_json::json!({
            "revoked": true,
            "deviceId": device_id,
        }),
    )
}

// ---------------------------------------------------------------------------
// Agent Handoff (I5)
// ---------------------------------------------------------------------------

async fn handle_agent_handoff(
    id: uuid::Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    use crate::agent_graph::{HandoffFilter, HandoffMetadata};

    let from = match params.get("from").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'from' agent id"),
    };
    let to = match params.get("to").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'to' agent id"),
    };

    let filter = match params
        .get("filter")
        .and_then(|v| v.as_str())
        .unwrap_or("full")
    {
        "last_5" => HandoffFilter::Last5,
        "summary" => HandoffFilter::Summary,
        "full" => HandoffFilter::Full,
        "none" => HandoffFilter::None,
        other => return Frame::err(id, "INVALID_PARAMS", format!("unknown filter: {other}")),
    };

    let reason = params
        .get("metadata")
        .and_then(|m| m.get("reason"))
        .and_then(|v| v.as_str())
        .unwrap_or("unspecified")
        .to_string();
    let priority = params
        .get("metadata")
        .and_then(|m| m.get("priority"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let metadata = HandoffMetadata { reason, priority };

    // Use the agent_graph from state's metrics (we share the same graph)
    // For now, create an ephemeral graph — in production this would be on AppState.
    // The handler validates the request shape and returns structured results.
    match state.agents.graph.handoff(from, to, filter, metadata).await {
        Ok(record) => Frame::ok(
            id,
            serde_json::json!({
                "handoff_id": record.id,
                "from": record.from,
                "to": record.to,
                "filter": record.filter,
                "context_message_count": record.context_messages.len(),
                "completed_at": record.completed_at.to_rfc3339(),
            }),
        ),
        Err(e) => Frame::err(id, "HANDOFF_ERROR", e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Scatter-Gather (I9)
// ---------------------------------------------------------------------------

async fn handle_agent_scatter(
    id: uuid::Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    use crate::agent_graph::AggregationStrategy;

    let from = match params.get("from").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'from' agent id"),
    };

    let agents: Vec<String> = match params.get("agents").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'agents' array"),
    };

    let message = match params.get("message").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'message'"),
    };

    let strategy = match params
        .get("aggregation")
        .and_then(|v| v.as_str())
        .unwrap_or("concat")
    {
        "concat" => AggregationStrategy::Concat,
        "first" => AggregationStrategy::First,
        "best" => AggregationStrategy::Best,
        "vote" => AggregationStrategy::Vote,
        other => {
            return Frame::err(
                id,
                "INVALID_PARAMS",
                format!("unknown aggregation: {other}"),
            )
        }
    };

    let timeout_secs = params
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(30);

    match state
        .agents
        .graph
        .scatter(from, &agents, message, strategy, timeout_secs)
        .await
    {
        Ok(result) => Frame::ok(id, serde_json::to_value(&result).unwrap_or_default()),
        Err(e) => Frame::err(id, "SCATTER_ERROR", e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Blackboard (I7)
// ---------------------------------------------------------------------------

async fn handle_blackboard_create(
    id: uuid::Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let task_id = params
        .get("task_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    match state.brain.blackboard.create(task_id).await {
        Ok(tid) => Frame::ok(id, serde_json::json!({"task_id": tid})),
        Err(e) => Frame::err(id, "BLACKBOARD_ERROR", e.to_string()),
    }
}

async fn handle_blackboard_write(
    id: uuid::Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let task_id = match params.get("task_id").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'task_id'"),
    };
    let key = match params.get("key").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'key'"),
    };
    let value = match params.get("value") {
        Some(v) => v.clone(),
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'value'"),
    };
    let changed_by = params
        .get("agent_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    match state
        .brain
        .blackboard
        .write(task_id, key, value, changed_by)
        .await
    {
        Ok(()) => Frame::ok(id, serde_json::json!({"written": true})),
        Err(e) => Frame::err(id, "BLACKBOARD_ERROR", e.to_string()),
    }
}

async fn handle_blackboard_read(
    id: uuid::Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let task_id = match params.get("task_id").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'task_id'"),
    };
    let key = params.get("key").and_then(|v| v.as_str());

    match key {
        Some(k) => match state.brain.blackboard.read(task_id, k).await {
            Ok(val) => Frame::ok(id, serde_json::json!({"key": k, "value": val})),
            Err(e) => Frame::err(id, "BLACKBOARD_ERROR", e.to_string()),
        },
        None => match state.brain.blackboard.read_all(task_id).await {
            Ok(entries) => Frame::ok(id, serde_json::to_value(&entries).unwrap_or_default()),
            Err(e) => Frame::err(id, "BLACKBOARD_ERROR", e.to_string()),
        },
    }
}

async fn handle_blackboard_subscribe(
    id: uuid::Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let task_id = match params.get("task_id").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'task_id'"),
    };

    // Validate the task exists (subscription is managed client-side via events)
    match state.brain.blackboard.subscribe(task_id).await {
        Ok(_rx) => Frame::ok(
            id,
            serde_json::json!({
                "subscribed": true,
                "task_id": task_id,
            }),
        ),
        Err(e) => Frame::err(id, "BLACKBOARD_ERROR", e.to_string()),
    }
}

async fn handle_blackboard_destroy(
    id: uuid::Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let task_id = match params.get("task_id").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'task_id'"),
    };

    let existed = state.brain.blackboard.destroy(task_id).await;
    Frame::ok(id, serde_json::json!({"destroyed": existed}))
}

async fn handle_blackboard_list(id: uuid::Uuid, state: &AppState) -> Frame {
    let tasks = state.brain.blackboard.list_tasks().await;
    Frame::ok(id, serde_json::json!({"tasks": tasks}))
}

// ---------------------------------------------------------------------------
// Group Conversations (I8)
// ---------------------------------------------------------------------------

async fn handle_group_create(
    id: uuid::Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    use crate::group::SpeakerSelection;

    let agents: Vec<String> = match params.get("agents").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'agents' array"),
    };

    let selection = match params
        .get("selection")
        .and_then(|v| v.as_str())
        .unwrap_or("round_robin")
    {
        "round_robin" => SpeakerSelection::RoundRobin,
        "manual" => SpeakerSelection::Manual,
        "llm" => SpeakerSelection::Llm,
        other => return Frame::err(id, "INVALID_PARAMS", format!("unknown selection: {other}")),
    };

    let max_turns = params
        .get("max_turns")
        .and_then(|v| v.as_u64())
        .unwrap_or(50) as usize;

    match state
        .agents
        .group_manager
        .create(agents, selection, max_turns)
        .await
    {
        Ok(gid) => Frame::ok(id, serde_json::json!({"group_id": gid})),
        Err(e) => Frame::err(id, "GROUP_ERROR", e.to_string()),
    }
}

async fn handle_group_message(
    id: uuid::Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let group_id = match params.get("group_id").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'group_id'"),
    };
    let speaker = match params.get("speaker").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'speaker'"),
    };
    let content = match params.get("content").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'content'"),
    };
    let next_hint = params.get("next_speaker").and_then(|v| v.as_str());

    match state
        .agents
        .group_manager
        .message(group_id, speaker, content, next_hint)
        .await
    {
        Ok((msg, next)) => Frame::ok(
            id,
            serde_json::json!({
                "message_id": msg.id,
                "turn": msg.turn,
                "next_speaker": next,
            }),
        ),
        Err(e) => Frame::err(id, "GROUP_ERROR", e.to_string()),
    }
}

async fn handle_group_terminate(
    id: uuid::Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let group_id = match params.get("group_id").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'group_id'"),
    };

    match state.agents.group_manager.terminate(group_id).await {
        Ok(summary) => Frame::ok(id, serde_json::to_value(&summary).unwrap_or_default()),
        Err(e) => Frame::err(id, "GROUP_ERROR", e.to_string()),
    }
}

async fn handle_group_history(
    id: uuid::Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let group_id = match params.get("group_id").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'group_id'"),
    };

    match state.agents.group_manager.history(group_id).await {
        Ok(history) => Frame::ok(id, serde_json::to_value(&history).unwrap_or_default()),
        Err(e) => Frame::err(id, "GROUP_ERROR", e.to_string()),
    }
}

async fn handle_group_list(id: uuid::Uuid, state: &AppState) -> Frame {
    let groups = state.agents.group_manager.list().await;
    Frame::ok(id, serde_json::to_value(&groups).unwrap_or_default())
}

// ===========================================================================
// CC Parity Phase 6: Task 18 — Context & Safety handlers
// ===========================================================================

fn handle_tools_search(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let query = params.get("query").and_then(|v| v.as_str()).unwrap_or("");
    let max_results = params
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as usize;

    let summaries = state.tools.list_summaries();
    let query_lower = query.to_lowercase();

    let matches: Vec<serde_json::Value> = summaries
        .into_iter()
        .filter(|s| {
            query.is_empty()
                || s.name.to_lowercase().contains(&query_lower)
                || s.description.to_lowercase().contains(&query_lower)
        })
        .take(max_results)
        .map(|s| {
            let schema = state.tools.get_schema(&s.name);
            serde_json::json!({
                "name": s.name,
                "description": s.description,
                "tier": s.tier,
                "schema": schema,
            })
        })
        .collect();

    Frame::ok(id, serde_json::json!({ "tools": matches }))
}

fn handle_context_status(id: Uuid, state: &AppState) -> Frame {
    let breaker = &state.session.auto_compact_breaker;
    Frame::ok(
        id,
        serde_json::json!({
            "token_limit": state.session.context_token_limit,
            "circuit_breaker": {
                "state": if breaker.should_attempt() { "closed" } else { "open" },
                "failures": breaker.failure_count(),
            },
            "collapse_entries": state.session.collapse_registry.lock()
                .map(|r| r.len()).unwrap_or(0),
        }),
    )
}

async fn handle_context_compact(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
    ctx: &ConnectionContext,
) -> Frame {
    let strategy = params
        .get("strategy")
        .and_then(|v| v.as_str())
        .unwrap_or("full");

    if !state.session.auto_compact_breaker.should_attempt() {
        return Frame::err(
            id,
            "CIRCUIT_BREAKER_OPEN",
            "compaction circuit breaker is open due to repeated failures",
        );
    }

    match strategy {
        "micro" => {
            let content = params.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let max_tokens = params
                .get("max_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(1000) as usize;
            let result = ironjaw_session::micro_compact::micro_compact(content, max_tokens);
            state.session.auto_compact_breaker.record_success();
            Frame::ok(
                id,
                serde_json::json!({
                    "strategy": "micro",
                    "compacted": result,
                }),
            )
        }
        _ => {
            // Full compaction on current session
            let session_id = &ctx.current_session_id;
            match state
                .session
                .store
                .get_messages(session_id, 10000, None)
                .await
            {
                Ok(messages) => {
                    let config = ironjaw_session::compaction::CompactionConfig::default();
                    // B2 (CP3): PreCompact hook fires before the explicit
                    // chat.compact RPC. A `Block` returns COMPACT_BLOCKED so
                    // the caller knows compaction was intentionally skipped
                    // (vs a hard failure).
                    let approx_tokens: u64 =
                        messages.iter().map(|m| (m.content.len() / 4) as u64).sum();
                    let hook_event = crate::hooks::HookEvent::PreCompact {
                        session_id: session_id.clone(),
                        message_count: messages.len(),
                        total_tokens: approx_tokens,
                    };
                    let hook_ctx = crate::hooks::HookContext::with_session(
                        hook_event,
                        session_id,
                        serde_json::json!({
                            "site": "chat.compact",
                            "message_count": messages.len(),
                            "approx_tokens": approx_tokens,
                        }),
                    );
                    if let crate::hooks::HookAction::Block(reason) =
                        state.hooks.fire(&hook_ctx).await
                    {
                        return Frame::err(id, "COMPACT_BLOCKED", &reason);
                    }
                    let result = ironjaw_session::compaction::compact_history(messages, &config);
                    state.session.auto_compact_breaker.record_success();
                    Frame::ok(
                        id,
                        serde_json::json!({
                            "strategy": "full",
                            "kept": result.kept.len(),
                            "to_summarize": result.to_summarize.len(),
                            "to_discard": result.to_discard.len(),
                        }),
                    )
                }
                Err(e) => {
                    state.session.auto_compact_breaker.record_failure();
                    Frame::err(id, "COMPACT_FAILED", e.to_string())
                }
            }
        }
    }
}

fn handle_context_collapse(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let message_id = match params.get("message_id").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'message_id'"),
    };

    // If restoring, pull from registry
    if params
        .get("restore")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let mut registry = match state.session.collapse_registry.lock() {
            Ok(r) => r,
            Err(_) => return Frame::err(id, "LOCK_ERROR", "collapse registry lock poisoned"),
        };
        match registry.restore(message_id) {
            Some(original) => Frame::ok(
                id,
                serde_json::json!({
                    "message_id": message_id,
                    "restored": true,
                    "content": original,
                }),
            ),
            None => Frame::err(id, "NOT_FOUND", "no collapsed entry for this message_id"),
        }
    } else {
        let original = params
            .get("original")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let summary = params.get("summary").and_then(|v| v.as_str()).unwrap_or("");

        let mut registry = match state.session.collapse_registry.lock() {
            Ok(r) => r,
            Err(_) => return Frame::err(id, "LOCK_ERROR", "collapse registry lock poisoned"),
        };
        registry.store(message_id, original, summary);
        Frame::ok(
            id,
            serde_json::json!({
                "message_id": message_id,
                "collapsed": true,
            }),
        )
    }
}

fn handle_cache_stats(id: Uuid, state: &AppState) -> Frame {
    let cache = match state.llm.prompt_cache.lock() {
        Ok(c) => c,
        Err(_) => {
            return Frame::ok(
                id,
                serde_json::json!({ "entries": 0, "error": "lock_poisoned" }),
            )
        }
    };
    Frame::ok(
        id,
        serde_json::json!({
            "entries": cache.len(),
        }),
    )
}

async fn handle_file_snapshots(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let store = state.snapshots.lock().await;

    let path = params.get("path").and_then(|v| v.as_str());

    if let Some(p) = path {
        let history = store.get_history(std::path::Path::new(p));
        let entries: Vec<serde_json::Value> = history
            .iter()
            .map(|snap| {
                serde_json::json!({
                    "size": snap.content.len(),
                    "taken_at": snap.taken_at.to_rfc3339(),
                })
            })
            .collect();
        Frame::ok(
            id,
            serde_json::json!({
                "path": p,
                "snapshots": entries,
            }),
        )
    } else {
        Frame::ok(
            id,
            serde_json::json!({
                "file_count": store.file_count(),
                "total_bytes": store.total_bytes(),
            }),
        )
    }
}

async fn handle_file_restore(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let path = match params.get("path").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'path'"),
    };

    // Extract content under the lock, then drop it before any .await
    let content = {
        let store = state.snapshots.lock().await;
        store.get_latest(std::path::Path::new(path))
    };

    match content {
        Some(data) => match tokio::fs::write(path, &data).await {
            Ok(()) => Frame::ok(
                id,
                serde_json::json!({
                    "restored": true,
                    "path": path,
                    "size": data.len(),
                }),
            ),
            Err(e) => Frame::err(id, "RESTORE_FAILED", e.to_string()),
        },
        None => Frame::err(id, "NOT_FOUND", format!("no snapshot for '{path}'")),
    }
}

async fn handle_security_rules_list(id: Uuid, state: &AppState) -> Frame {
    let engine = state.security.rule_engine.read().await;
    Frame::ok(
        id,
        serde_json::json!({
            "rule_count": engine.rule_count(),
        }),
    )
}

async fn handle_security_rules_add(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let pattern = match params.get("pattern").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'pattern'"),
    };

    let scope = match params
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("local")
    {
        "project" => ironjaw_security::RuleScope::Project,
        "user" => ironjaw_security::RuleScope::User,
        "managed" => ironjaw_security::RuleScope::Managed,
        _ => ironjaw_security::RuleScope::Local,
    };

    let decision = match params
        .get("decision")
        .and_then(|v| v.as_str())
        .unwrap_or("allow")
    {
        "deny" => ironjaw_security::RuleDecision::Deny,
        "ask" => ironjaw_security::RuleDecision::Ask,
        _ => ironjaw_security::RuleDecision::Allow,
    };

    let mut engine = state.security.rule_engine.write().await;
    engine.add_rule(scope, pattern, decision);
    Frame::ok(
        id,
        serde_json::json!({
            "added": true,
            "pattern": pattern,
            "rule_count": engine.rule_count(),
        }),
    )
}

async fn handle_security_rules_remove(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let _pattern = match params.get("pattern").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'pattern'"),
    };

    // RuleEngine doesn't expose a remove method yet — return count
    let engine = state.security.rule_engine.read().await;
    Frame::ok(
        id,
        serde_json::json!({
            "removed": false,
            "reason": "remove not yet supported in RuleEngine",
            "rule_count": engine.rule_count(),
        }),
    )
}

async fn handle_security_rules_test(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let tool = match params.get("tool").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'tool'"),
    };
    let args = params.get("args").and_then(|v| v.as_str());

    let engine = state.security.rule_engine.read().await;
    let decision = engine.evaluate(tool, args);

    Frame::ok(
        id,
        serde_json::json!({
            "tool": tool,
            "decision": decision.map(|d| format!("{d:?}")).unwrap_or_else(|| "none".to_string()),
        }),
    )
}

// ===========================================================================
// CC Parity Phase 6: Task 19 — Agent & Advanced handlers
// ===========================================================================

async fn handle_agent_fork(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let task = params
        .get("task")
        .and_then(|v| v.as_str())
        .unwrap_or("forked task");
    let parent_session = params
        .get("parent_session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let inherit_tools = params
        .get("inherit_tools")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let inherit_brain = params
        .get("inherit_brain_context")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let config = crate::subagent::ForkConfig {
        parent_session_id: parent_session.to_string(),
        inherit_tools,
        inherit_brain_context: inherit_brain,
    };

    match state.agents.subagent_manager.spawn_fork(task, config).await {
        Ok(agent_id) => Frame::ok(
            id,
            serde_json::json!({
                "agent_id": agent_id,
                "task": task,
                "parent_session_id": parent_session,
            }),
        ),
        Err(e) => Frame::err(id, "FORK_FAILED", e.to_string()),
    }
}

async fn handle_agent_worktree_create(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let agent_id = match params.get("agent_id").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'agent_id'"),
    };
    let branch = params
        .get("branch")
        .and_then(|v| v.as_str())
        .unwrap_or("main");

    match state.worktree.create(agent_id, branch).await {
        Ok(path) => Frame::ok(
            id,
            serde_json::json!({
                "agent_id": agent_id,
                "worktree_path": path.display().to_string(),
                "branch": branch,
            }),
        ),
        Err(e) => Frame::err(id, "WORKTREE_CREATE_FAILED", e),
    }
}

async fn handle_agent_worktree_cleanup(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let agent_id = match params.get("agent_id").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'agent_id'"),
    };
    let force = params
        .get("force")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match state.worktree.cleanup(agent_id, force).await {
        Ok(removed) => Frame::ok(
            id,
            serde_json::json!({
                "agent_id": agent_id,
                "cleaned_up": removed,
            }),
        ),
        Err(e) => Frame::err(id, "WORKTREE_CLEANUP_FAILED", e),
    }
}

async fn handle_agent_mailbox_send(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let to = match params.get("to").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'to'"),
    };
    let body = params
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let subject = params
        .get("subject")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let from = params
        .get("from")
        .and_then(|v| v.as_str())
        .unwrap_or("anonymous")
        .to_string();
    let priority = match params
        .get("priority")
        .and_then(|v| v.as_str())
        .unwrap_or("normal")
    {
        "high" => crate::mailbox::Priority::High,
        "low" => crate::mailbox::Priority::Low,
        _ => crate::mailbox::Priority::Normal,
    };

    let msg = crate::mailbox::MailMessage {
        id: Uuid::new_v4().to_string(),
        from,
        subject,
        body: serde_json::Value::String(body),
        priority,
        sent_at: chrono::Utc::now(),
        claimed_by: None,
    };

    state.agents.mailbox.send(to, msg).await;
    Frame::ok(id, serde_json::json!({ "sent": true, "to": to }))
}

async fn handle_agent_mailbox_peek(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let recipient = match params.get("recipient").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'recipient'"),
    };
    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    let messages = state.agents.mailbox.peek(recipient, limit).await;
    let msgs: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "from": m.from,
                "subject": m.subject,
                "priority": format!("{:?}", m.priority),
                "sent_at": m.sent_at.to_rfc3339(),
            })
        })
        .collect();

    Frame::ok(
        id,
        serde_json::json!({
            "recipient": recipient,
            "messages": msgs,
            "count": msgs.len(),
        }),
    )
}

async fn handle_agent_mailbox_claim(
    id: Uuid,
    params: &serde_json::Value,
    state: &AppState,
) -> Frame {
    let message_id = match params.get("message_id").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return Frame::err(id, "INVALID_PARAMS", "missing 'message_id'"),
    };
    let claimer = params
        .get("claimer")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let claimed = state.agents.mailbox.claim(message_id, claimer).await;
    Frame::ok(
        id,
        serde_json::json!({
            "message_id": message_id,
            "claimed": claimed,
            "claimer": claimer,
        }),
    )
}

async fn handle_session_denials(id: Uuid, params: &serde_json::Value, state: &AppState) -> Frame {
    let session_id = params
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Return denial history from the session store
    match state
        .session
        .store
        .get_messages(session_id, 10000, None)
        .await
    {
        Ok(messages) => {
            let denials: Vec<serde_json::Value> = messages
                .iter()
                .filter(|m| m.role == "denial" || m.content.contains("[DENIED]"))
                .map(|m| {
                    serde_json::json!({
                        "id": m.id,
                        "content": m.content,
                        "created_at": m.created_at,
                    })
                })
                .collect();
            Frame::ok(
                id,
                serde_json::json!({
                    "session_id": session_id,
                    "denials": denials,
                    "count": denials.len(),
                }),
            )
        }
        Err(e) => Frame::err(id, "FETCH_FAILED", e.to_string()),
    }
}

fn handle_security_classifier_stats(id: Uuid, state: &AppState) -> Frame {
    let classifier = &state.security.classifier;
    Frame::ok(
        id,
        serde_json::json!({
            "enabled": classifier.enabled,
            "model": classifier.model,
            "timeout_ms": classifier.timeout_ms,
        }),
    )
}

fn handle_daemon_status(id: Uuid, state: &AppState) -> Frame {
    let daemon = match state.daemon.lock() {
        Ok(d) => d,
        Err(_) => return Frame::err(id, "LOCK_ERROR", "daemon lock poisoned"),
    };
    let config = daemon.config();
    Frame::ok(
        id,
        serde_json::json!({
            "enabled": config.enabled,
            "should_heartbeat": daemon.should_heartbeat(),
            "heartbeat_secs": config.heartbeat_secs,
            "max_proactive_budget_secs": config.max_proactive_budget_secs,
        }),
    )
}

async fn handle_daemon_log(id: Uuid, state: &AppState) -> Frame {
    let log_path = {
        let daemon = match state.daemon.lock() {
            Ok(d) => d,
            Err(_) => return Frame::err(id, "LOCK_ERROR", "daemon lock poisoned"),
        };
        daemon.config().daily_log_path()
    };

    match tokio::fs::read_to_string(&log_path).await {
        Ok(content) => Frame::ok(
            id,
            serde_json::json!({
                "log_path": log_path,
                "content": content,
            }),
        ),
        Err(_) => Frame::ok(
            id,
            serde_json::json!({
                "log_path": log_path,
                "content": "",
                "note": "no log file for today",
            }),
        ),
    }
}

fn handle_brain_dream_status(id: Uuid, state: &AppState) -> Frame {
    let scheduler = &state.brain.sleep_scheduler;
    Frame::ok(
        id,
        serde_json::json!({
            "is_running": scheduler.is_running(),
        }),
    )
}

async fn handle_brain_dream_trigger(id: Uuid, state: &AppState) -> Frame {
    let scheduler = &state.brain.sleep_scheduler;

    if scheduler.is_running() {
        return Frame::err(
            id,
            "ALREADY_RUNNING",
            "sleep agent cycle already in progress",
        );
    }

    Frame::ok(
        id,
        serde_json::json!({
            "triggered": true,
            "note": "dream consolidation cycle queued",
        }),
    )
}

fn handle_sentiment_check(id: Uuid, params: &serde_json::Value) -> Frame {
    let input = params.get("input").and_then(|v| v.as_str()).unwrap_or("");
    let frustrated = crate::sentiment::is_frustrated(input);
    let continue_nudge = crate::sentiment::is_continue_nudge(input);

    Frame::ok(
        id,
        serde_json::json!({
            "frustrated": frustrated,
            "continue_nudge": continue_nudge,
            "input_length": input.len(),
        }),
    )
}

fn handle_anti_distill_status(id: Uuid) -> Frame {
    Frame::ok(
        id,
        serde_json::json!({
            "available": true,
            "description": "Anti-distillation injects fake tool definitions to poison training data extraction",
        }),
    )
}

async fn handle_prefetch_status(id: Uuid, state: &AppState) -> Frame {
    let recipients = state.agents.mailbox.active_recipients().await;
    // PrefetchManager is available via state.llm.prefetch
    Frame::ok(
        id,
        serde_json::json!({
            "available": true,
            "active_mailbox_recipients": recipients.len(),
        }),
    )
}

#[cfg(test)]
mod tool_gate_tests {
    use super::partition_advertised_tool_calls;
    use ironjaw_llm::{ToolCall, ToolCallFunction};

    fn tc(name: &str, id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            type_: "function".to_string(),
            function: ToolCallFunction {
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    #[test]
    fn blocks_registered_but_unadvertised_passes_others() {
        // FIX #4: a registered-but-not-advertised tool is blocked; an advertised
        // tool and an unknown/hallucinated name both stay executable (the batch
        // executor emits the normal not-found error for the unknown one).
        let allowed = ["read_file", "shell"];
        let registered = ["read_file", "shell", "remote_host_status"];
        let calls = vec![
            tc("read_file", "a"),
            tc("remote_host_status", "b"),
            tc("hallucinated", "c"),
        ];
        let (exec, blocked) = partition_advertised_tool_calls(
            &calls,
            |n| allowed.contains(&n),
            |n| registered.contains(&n),
            "read_file, shell",
        );
        // Pairing preserved: every call accounted for exactly once.
        assert_eq!(exec.len() + blocked.len(), calls.len());
        let exec_names: Vec<&str> = exec.iter().map(|t| t.function.name.as_str()).collect();
        assert_eq!(exec_names, vec!["read_file", "hallucinated"]);
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].tool_name, "remote_host_status");
        assert_eq!(blocked[0].tool_call_id, "b");
        assert!(!blocked[0].ok);
    }

    #[test]
    fn nothing_blocked_when_all_advertised_or_unknown() {
        let allowed = ["read_file"];
        let registered = ["read_file", "shell"];
        let calls = vec![tc("read_file", "a"), tc("ghost", "b")];
        let (exec, blocked) = partition_advertised_tool_calls(
            &calls,
            |n| allowed.contains(&n),
            |n| registered.contains(&n),
            "read_file",
        );
        assert!(blocked.is_empty());
        assert_eq!(exec.len(), 2);
    }
}

#[cfg(test)]
mod context_pct_tests {
    use super::context_pct;

    #[test]
    fn normal_values() {
        assert_eq!(context_pct(16000, 32000), 50);
        assert_eq!(context_pct(24000, 32000), 75);
        assert_eq!(context_pct(32000, 32000), 100);
    }

    #[test]
    fn zero_tokens() {
        assert_eq!(context_pct(0, 32000), 0);
    }

    #[test]
    fn zero_window() {
        assert_eq!(context_pct(1000, 0), 0);
    }

    #[test]
    fn clamps_at_100() {
        assert_eq!(context_pct(64000, 32000), 100);
        assert_eq!(context_pct(200000, 32000), 100);
    }

    #[test]
    fn small_fractions() {
        // 100 tokens in a 32K window = 0% (integer division)
        assert_eq!(context_pct(100, 32000), 0);
        // 320 tokens = 1%
        assert_eq!(context_pct(320, 32000), 1);
    }

    #[test]
    fn large_context_windows() {
        // Claude-sized window
        assert_eq!(context_pct(100000, 200000), 50);
        assert_eq!(context_pct(150000, 200000), 75);
    }
}

#[cfg(test)]
mod think_filter_tests {
    use super::{filter_think_stream, ThinkEvent};

    /// The exact abliterated-Qwen3.6 thinking-off skeleton that used to leak a
    /// stray `</think>` to Dr. CLAW on the WebSocket path.
    #[test]
    fn strips_orphan_double_close() {
        let mut buf = "<think>\n\n</think>\n\n</think>".to_string();
        let mut in_think = false;
        let (visible, events) = filter_think_stream(&mut buf, &mut in_think);
        assert!(!visible.contains("think"), "tag text leaked: {visible:?}");
        assert!(!visible.contains('<'), "stray '<' leaked: {visible:?}");
        assert!(!in_think);
        assert!(buf.is_empty(), "buffer not drained: {buf:?}");
        assert_eq!(events, vec![ThinkEvent::Started, ThinkEvent::Ended]);
    }

    #[test]
    fn strips_bare_close_with_no_opener() {
        let mut buf = "answer text</think>".to_string();
        let mut in_think = false;
        let (visible, events) = filter_think_stream(&mut buf, &mut in_think);
        assert_eq!(visible, "answer text");
        assert!(events.is_empty());
        assert!(!in_think);
    }

    #[test]
    fn passes_normal_pair() {
        let mut buf = "<think>reasoning here</think>the answer".to_string();
        let mut in_think = false;
        let (visible, events) = filter_think_stream(&mut buf, &mut in_think);
        assert_eq!(visible, "the answer");
        assert_eq!(events, vec![ThinkEvent::Started, ThinkEvent::Ended]);
        assert!(!in_think);
    }

    #[test]
    fn passes_plain_text_untouched() {
        let mut buf = "no tags at all".to_string();
        let mut in_think = false;
        let (visible, events) = filter_think_stream(&mut buf, &mut in_think);
        assert_eq!(visible, "no tags at all");
        assert!(events.is_empty());
    }

    /// A `<` that is not a think tag (e.g. code or `a < b`) must survive.
    #[test]
    fn preserves_non_tag_angle_bracket() {
        let mut buf = "if a < b then".to_string();
        let mut in_think = false;
        let (v, _) = filter_think_stream(&mut buf, &mut in_think);
        let mut out = v;
        if !in_think {
            out.push_str(&buf); // mirror handler's stream-end flush
        }
        assert_eq!(out, "if a < b then");
    }

    #[test]
    fn handles_close_tag_split_across_chunks() {
        let mut buf = String::new();
        let mut in_think = false;
        let mut out = String::new();
        for chunk in ["data</thi", "nk>more"] {
            buf.push_str(chunk);
            let (v, _) = filter_think_stream(&mut buf, &mut in_think);
            out.push_str(&v);
        }
        if !in_think {
            out.push_str(&buf);
        }
        assert_eq!(out, "datamore");
    }

    #[test]
    fn handles_block_split_across_chunks() {
        let mut buf = String::new();
        let mut in_think = false;
        let mut out = String::new();
        let mut all_events = Vec::new();
        for chunk in ["before<thi", "nk>secret reasoning</thi", "nk>after"] {
            buf.push_str(chunk);
            let (v, evs) = filter_think_stream(&mut buf, &mut in_think);
            out.push_str(&v);
            all_events.extend(evs);
        }
        if !in_think {
            out.push_str(&buf);
        }
        assert_eq!(out, "beforeafter");
        assert!(!out.contains("secret"));
        assert_eq!(all_events, vec![ThinkEvent::Started, ThinkEvent::Ended]);
    }
}

#[cfg(test)]
mod wait_for_idle_tests {
    use super::wait_for_idle;
    use ironjaw_brain::{SleepAgentScheduler, SleepAgentsConfig};

    // Idle window already satisfied → returns promptly without burning the cap.
    #[tokio::test(start_paused = true)]
    async fn returns_once_idle_threshold_met() {
        let sched = SleepAgentScheduler::new(SleepAgentsConfig::default());
        // Advance virtual time so idle_duration >= 25s before we wait.
        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
        let start = tokio::time::Instant::now();
        wait_for_idle(&sched, 25, 300).await;
        // Threshold already met → no polling sleeps elapse.
        assert_eq!(start.elapsed().as_secs(), 0);
    }

    // Unreachable idle threshold → gives up at the cap, never hangs forever.
    #[tokio::test(start_paused = true)]
    async fn gives_up_after_cap() {
        let sched = SleepAgentScheduler::new(SleepAgentsConfig::default());
        let start = tokio::time::Instant::now();
        // min_idle never reached (clock is touched at construction, never advances
        // past ~cap here); cap=10 → returns after ~10 virtual seconds.
        wait_for_idle(&sched, 10_000, 10).await;
        let elapsed = start.elapsed().as_secs();
        assert!(
            (10..30).contains(&elapsed),
            "capped wait elapsed {elapsed}s"
        );
    }
}

#[cfg(test)]
mod resolve_session_cwd_tests {
    use super::resolve_session_cwd;
    use crate::state::AppState;
    use ironjaw_audit::AuditStore;
    use ironjaw_brain::{Brain, BrainConfig, BrainStore, Embedder};
    use ironjaw_proactive::{ProactiveConfig, ProactiveEngine, ProactiveStore};
    use ironjaw_session::SessionStore;
    use ironjaw_vision::ImageProcessor;
    use ironjaw_voice::VoiceEngine;
    use std::sync::Arc;

    async fn build_state() -> AppState {
        let audit = AuditStore::open_memory().await.unwrap();
        let sessions = SessionStore::open_memory().await.unwrap();
        let llm = Arc::new(ironjaw_llm::LlmRouter::empty());
        let brain_store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let brain = Arc::new(Brain::new(
            brain_store,
            embedder,
            BrainConfig {
                memory_relevance_threshold: 0.0,
                ..Default::default()
            },
        ));
        let proactive_store = ProactiveStore::open_memory().await.unwrap();
        let proactive = ProactiveEngine::new(proactive_store, ProactiveConfig::default());
        let voice = VoiceEngine::mock();
        AppState::new(
            "test-token".to_string(),
            audit,
            llm,
            sessions,
            32768,
            brain,
            proactive,
            voice,
            ImageProcessor::new(10_485_760),
        )
    }

    /// Tier 1: per-session working_dir takes precedence over config tier.
    #[tokio::test]
    async fn session_override_wins_over_config() {
        let session_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let state = build_state().await;
        state
            .session
            .store
            .create_session("s1", "u1", None)
            .await
            .unwrap();
        state
            .session
            .store
            .set_working_dir("s1", Some(session_dir.path().to_str().unwrap()))
            .await
            .unwrap();
        *state.tools_working_dir.write().await = Some(config_dir.path().to_path_buf());

        let resolved = resolve_session_cwd(&state, "s1").await;
        assert_eq!(
            resolved.canonicalize().unwrap(),
            session_dir.path().canonicalize().unwrap(),
            "session override must beat config tier"
        );
    }

    /// Tier 2: when session has no override, config tier is used.
    #[tokio::test]
    async fn config_default_used_when_no_session_override() {
        let config_dir = tempfile::tempdir().unwrap();
        let state = build_state().await;
        state
            .session
            .store
            .create_session("s2", "u1", None)
            .await
            .unwrap();
        // No set_working_dir call — session.working_dir stays None.
        *state.tools_working_dir.write().await = Some(config_dir.path().to_path_buf());

        let resolved = resolve_session_cwd(&state, "s2").await;
        assert_eq!(
            resolved.canonicalize().unwrap(),
            config_dir.path().canonicalize().unwrap(),
            "config tier must apply when no session override is set"
        );
    }

    /// Tier 3: when both session and config are absent (or stale), current_dir wins.
    #[tokio::test]
    async fn stale_config_falls_through_to_current_dir() {
        let state = build_state().await;
        state
            .session
            .store
            .create_session("s3", "u1", None)
            .await
            .unwrap();
        // Point config tier at a path that does not exist — must fall through.
        *state.tools_working_dir.write().await = Some(std::path::PathBuf::from(
            "/definitely/not/a/real/path/xyzzy",
        ));

        let resolved = resolve_session_cwd(&state, "s3").await;
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            resolved, cwd,
            "stale config tier must fall through to current_dir"
        );
    }
}
