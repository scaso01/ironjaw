/// Agentic tool-call loop helpers for `chat.send`.
///
/// This module is gated behind `state.chat_agentic.enabled` (the Phase 4
/// killswitch).  When disabled, `handle_chat_send` falls through to the
/// pre-agentic single-pass code path.
///
/// # Key types
/// - [`ExecutedToolResult`] – outcome record for one tool invocation.
/// - [`execute_tool_batch`] – parallel/serial tool executor with HITL checking
///   and per-tool timeout; emits `tool.start` / `tool.result` events.
///
/// # Parallel execution
/// Auto-tier tools (no HITL, no user confirmation needed) are run concurrently
/// via [`futures_util::future::join_all`].  Confirm/Notify tools run serially
/// to preserve HITL ordering.  Output order always matches input `tool_calls`
/// order regardless of completion order.
///
/// Toggle: set [`PARALLEL_AUTO_ENABLED`] to `false` to revert to serial-only.
/// This constant is the stand-in for `chat_agentic.parallel_execution` until
/// that field is promoted into `ChatAgenticConfig` / `ironjaw.toml`.
use crate::handler::send_frame;
use crate::protocol::Frame;
use axum::extract::ws::Message;
use dashmap::DashMap;
use futures_util::future::join_all;
use ironjaw_audit::{ActionType, AuditEntry, AuditStore};
use ironjaw_brain::{
    types::{NewMemory, TrustLevel},
    Brain,
};
use ironjaw_llm::{ToolCall, ToolCallFunction};
use ironjaw_security::{HitlGate, PermissionTier};
use ironjaw_session::types::SessionMode;
use ironjaw_tools::{MemoryHint, ToolContext, ToolRegistry, ToolResult};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use uuid::Uuid;

/// Master switch for Auto-tier parallel execution.
///
/// When `true`, all Auto-tier tools in a batch run concurrently; Confirm/Notify
/// tools continue to run serially.  Set to `false` to fall back to the original
/// fully-serial behaviour (useful for debugging or when tool ordering matters
/// across all tiers).  This will be promoted to a `ChatAgenticConfig` field and
/// exposed in `ironjaw.toml` as `parallel_execution`.
const PARALLEL_AUTO_ENABLED: bool = true;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// RAII guard that removes a tool entry from the in-flight registry on drop.
///
/// This ensures the registry stays consistent even when `execute_single` returns
/// early (parse error, HITL denial, panic) without explicit cleanup code at each
/// exit path.
struct InFlightGuard {
    registry: Arc<DashMap<String, HashMap<String, String>>>,
    session_id: String,
    tool_call_id: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Some(mut entry) = self.registry.get_mut(&self.session_id) {
            entry.remove(&self.tool_call_id);
        }
    }
}

/// Outcome record for a single tool call within one agentic loop iteration.
#[derive(Debug, Clone)]
pub struct ExecutedToolResult {
    /// LLM-assigned tool-call ID (links back to the assistant message).
    pub tool_call_id: String,
    /// Name of the tool that was invoked.
    pub tool_name: String,
    /// Raw result returned by the tool.
    pub result: ToolResult,
    /// Whether the tool completed without error.
    pub ok: bool,
    /// Wall-clock duration of the tool call in milliseconds.
    pub duration_ms: u64,
    /// Error string when `ok == false`.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Batch executor
// ---------------------------------------------------------------------------

/// Execute a batch of [`ToolCall`]s from one LLM iteration.
///
/// Behaviour per tool tier:
/// - **Auto / Notify** – executed immediately (parallel futures).
/// - **Confirm / MultiConfirm** – HITL check via `hitl_gate`.  If the gate is
///   in auto-approve mode the call proceeds immediately; otherwise it blocks
///   until the user approves or the timeout fires.
///
/// For each tool the function emits:
/// 1. A `tool.start` event before execution begins.
/// 2. A `tool.result` event after execution (success or error).
///
/// Returns one [`ExecutedToolResult`] per input `ToolCall` in the same order.
#[allow(clippy::too_many_arguments)]
pub async fn execute_tool_batch(
    tool_calls: &[ToolCall],
    registry: &Arc<ToolRegistry>,
    hitl: &Arc<HitlGate>,
    tx: &mpsc::UnboundedSender<Message>,
    tool_context: &ToolContext,
    timeout_secs: u64,
    session_id: &str,
    hitl_session_cache_enabled: bool,
    // Phase 11: optional brain reference for tool-memory writeback.
    brain: Option<&Arc<Brain>>,
    // Phase 11: when `true` and `brain` is `Some`, successful tool executions
    // write a `ToolVerified` memory back to the brain.
    writeback_enabled: bool,
    // A4: per-session HITL override mode.
    session_mode: SessionMode,
    // A10: optional audit store for tool.start/tool.result entries.
    audit: Option<&AuditStore>,
    // C2-A: in-flight tool registry; entries are inserted at tool.start and
    // removed (via RAII guard) when tool.result is emitted.
    in_flight_tools: &Arc<DashMap<String, HashMap<String, String>>>,
    // C2-B: cancellation token — fired by chat.abort to stop in-flight tools.
    cancel: CancellationToken,
) -> Vec<ExecutedToolResult> {
    // If the parallel-execution feature flag is off, fall back to the original
    // fully-serial loop so callers see identical behaviour with no overhead.
    if !PARALLEL_AUTO_ENABLED {
        let mut results = Vec::with_capacity(tool_calls.len());
        for call in tool_calls {
            let result = execute_single(
                call,
                registry,
                hitl,
                tx,
                tool_context,
                timeout_secs,
                session_id,
                hitl_session_cache_enabled,
                brain,
                writeback_enabled,
                session_mode.clone(),
                audit,
                in_flight_tools,
                cancel.clone(),
            )
            .await;
            results.push(result);
        }
        return results;
    }

    // -----------------------------------------------------------------------
    // Partition the batch into two groups, keyed by their original index so
    // we can reconstruct the input order after parallel execution completes.
    //
    // - parallel_safe: Auto-tier tools — no HITL, no user prompt, safe to
    //   run concurrently.
    // - serial: Notify / Confirm / MultiConfirm — must run one at a time so
    //   HITL prompts don't interleave and session-scoped approvals propagate
    //   correctly between tools.
    // -----------------------------------------------------------------------
    let mut parallel_safe: Vec<(usize, &ToolCall)> = Vec::new();
    let mut serial: Vec<(usize, &ToolCall)> = Vec::new();

    for (idx, call) in tool_calls.iter().enumerate() {
        let tier = registry
            .get(&call.function.name)
            .map(|t| t.tier())
            .unwrap_or(PermissionTier::Confirm); // unknown → safe default

        if tier == PermissionTier::Auto {
            parallel_safe.push((idx, call));
        } else {
            serial.push((idx, call));
        }
    }

    // Slot-indexed output buffer — filled in any order, unwrapped at the end.
    let mut output: Vec<Option<ExecutedToolResult>> = (0..tool_calls.len()).map(|_| None).collect();

    // --- Run Auto-tier tools concurrently ----------------------------------
    if !parallel_safe.is_empty() {
        let audit_opt = audit.cloned();
        let futures = parallel_safe.iter().map(|(idx, call)| {
            // Clone everything the async block needs to own.
            let idx = *idx;
            let call = (*call).clone();
            let registry = Arc::clone(registry);
            let hitl = Arc::clone(hitl);
            let tx = tx.clone();
            let tool_context = tool_context.clone();
            let brain_opt = brain.map(Arc::clone);
            let mode = session_mode.clone();
            let audit_opt = audit_opt.clone();
            let in_flight_tools = Arc::clone(in_flight_tools);
            let cancel = cancel.clone();
            async move {
                let result = execute_single(
                    &call,
                    &registry,
                    &hitl,
                    &tx,
                    &tool_context,
                    timeout_secs,
                    session_id,
                    hitl_session_cache_enabled,
                    brain_opt.as_ref(),
                    writeback_enabled,
                    mode,
                    audit_opt.as_ref(),
                    &in_flight_tools,
                    cancel,
                )
                .await;
                (idx, result)
            }
        });

        let completed = join_all(futures).await;
        for (idx, result) in completed {
            output[idx] = Some(result);
        }
    }

    // --- Run Confirm/Notify tools sequentially -----------------------------
    for (idx, call) in &serial {
        let result = execute_single(
            call,
            registry,
            hitl,
            tx,
            tool_context,
            timeout_secs,
            session_id,
            hitl_session_cache_enabled,
            brain,
            writeback_enabled,
            session_mode.clone(),
            audit,
            in_flight_tools,
            cancel.clone(),
        )
        .await;
        output[*idx] = Some(result);
    }

    // Unwrap the slot buffer — every position was filled above.
    output
        .into_iter()
        .map(|slot| slot.expect("output slot unfilled — this is a bug in execute_tool_batch"))
        .collect()
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn execute_single(
    call: &ToolCall,
    registry: &Arc<ToolRegistry>,
    hitl: &Arc<HitlGate>,
    tx: &mpsc::UnboundedSender<Message>,
    tool_context: &ToolContext,
    timeout_secs: u64,
    session_id: &str,
    hitl_session_cache_enabled: bool,
    brain: Option<&Arc<Brain>>,
    writeback_enabled: bool,
    session_mode: SessionMode,
    audit: Option<&AuditStore>,
    in_flight_tools: &Arc<DashMap<String, HashMap<String, String>>>,
    cancel: CancellationToken,
) -> ExecutedToolResult {
    let tool_name = call.function.name.clone();
    let tool_call_id = call.id.clone();

    // Emit tool.start event with parsed arguments so clients (Dr. CLAW UI)
    // can show the user *what* will run before/while it executes.
    let parsed_args: serde_json::Value = serde_json::from_str(&call.function.arguments)
        .unwrap_or_else(|_| serde_json::Value::String(call.function.arguments.clone()));
    let start_event = Frame::event(
        "tool.start",
        serde_json::json!({
            "tool_name": &tool_name,
            "tool_call_id": &tool_call_id,
            "arguments": parsed_args,
        }),
    );
    let _ = send_frame(tx, &start_event);

    // C2-A: register this tool as in-flight. The RAII guard ensures the entry
    // is removed even if we return early (parse error, HITL denial, panic).
    in_flight_tools
        .entry(session_id.to_string())
        .or_default()
        .insert(tool_call_id.clone(), tool_name.clone());
    let _guard = InFlightGuard {
        registry: Arc::clone(in_flight_tools),
        session_id: session_id.to_string(),
        tool_call_id: tool_call_id.clone(),
    };

    // A10: fire-and-forget audit log for tool.start.
    if let Some(store) = audit.cloned() {
        let tool_name_a = tool_name.clone();
        let request_id = Uuid::new_v4();
        tokio::spawn(async move {
            let _ = store
                .log(&AuditEntry {
                    action_type: ActionType::ToolCall,
                    method: Some("tool.start".to_string()),
                    tool_name: Some(tool_name_a),
                    params: None,
                    result: None,
                    ok: true,
                    error: None,
                    llm_model: None,
                    tokens_in: None,
                    tokens_out: None,
                    cost_usd: None,
                    latency_ms: None,
                    session_id: None,
                    device_id: None,
                    request_id,
                    iteration: None,
                    tool_calls: None,
                })
                .await;
        });
    }

    let start = std::time::Instant::now();

    // Parse arguments (the LLM provides a JSON string).
    let params: serde_json::Value = match serde_json::from_str(&call.function.arguments) {
        Ok(v) => v,
        Err(e) => {
            let err_msg = format!("Failed to parse tool arguments: {e}");
            warn!(tool = %tool_name, error = %err_msg, "Tool argument parse error");
            let duration_ms = start.elapsed().as_millis() as u64;
            let result = ToolResult::error("PARSE_ERROR", err_msg.clone());
            emit_tool_result(tx, &tool_name, &tool_call_id, &result, duration_ms);
            // A10: audit tool.result for parse error.
            if let Some(store) = audit.cloned() {
                let tool_name_a = tool_name.clone();
                let err_a = err_msg.clone();
                let request_id = Uuid::new_v4();
                tokio::spawn(async move {
                    let _ = store
                        .log(&AuditEntry {
                            action_type: ActionType::ToolCall,
                            method: Some("tool.result".to_string()),
                            tool_name: Some(tool_name_a),
                            params: None,
                            result: None,
                            ok: false,
                            error: Some(err_a),
                            llm_model: None,
                            tokens_in: None,
                            tokens_out: None,
                            cost_usd: None,
                            latency_ms: Some(duration_ms as i64),
                            session_id: None,
                            device_id: None,
                            request_id,
                            iteration: None,
                            tool_calls: None,
                        })
                        .await;
                });
            }
            return ExecutedToolResult {
                tool_call_id,
                tool_name,
                result,
                ok: false,
                duration_ms,
                error: Some(err_msg),
            };
        }
    };

    // HITL gate check for Confirm/MultiConfirm tools.
    let tier = registry
        .get(&tool_name)
        .map(|t| t.tier())
        .unwrap_or(PermissionTier::Confirm); // default safe

    let needs_approval = match session_mode {
        SessionMode::AutoAccept => false,
        SessionMode::Plan => true,
        SessionMode::Default => tier.requires_approval() && !hitl.is_auto_approve(),
    };
    if needs_approval {
        // Phase 6: session-scoped HITL cache — skip gate if this tool was
        // already approved for the current session.
        let session_cached = hitl_session_cache_enabled
            && !session_id.is_empty()
            && hitl.has_session_approval(session_id, &tool_name);

        if !session_cached {
            let (request_id, rx) = hitl.request_approval();
            debug!(tool = %tool_name, %request_id, "HITL approval requested");

            // Associate the tool name so the cmd.res handler can record a
            // session-scoped approval when the client responds with scope="session".
            hitl.register_tool_for_request(request_id, &tool_name);

            // Emit a permission.request event so the client can show a dialog.
            let perm_event = Frame::event(
                "permission.request",
                serde_json::json!({
                    "tool": &tool_name,
                    "params": &params,
                    "request_id": request_id.to_string(),
                    "source": "agentic_loop",
                }),
            );
            let _ = send_frame(tx, &perm_event);

            // HitlGate::wait_for_approval uses the gate's configured timeout internally.
            let approved = hitl.wait_for_approval(rx).await;
            if !approved {
                let err_msg = format!("HITL approval denied or timed out for tool '{tool_name}'");
                warn!(%err_msg);
                let duration_ms = start.elapsed().as_millis() as u64;
                let result = ToolResult::error("HITL_DENIED", err_msg.clone());
                emit_tool_result(tx, &tool_name, &tool_call_id, &result, duration_ms);
                // A10: audit tool.result for HITL denial.
                if let Some(store) = audit.cloned() {
                    let tool_name_a = tool_name.clone();
                    let err_a = err_msg.clone();
                    let request_id = Uuid::new_v4();
                    tokio::spawn(async move {
                        let _ = store
                            .log(&AuditEntry {
                                action_type: ActionType::ToolCall,
                                method: Some("tool.result".to_string()),
                                tool_name: Some(tool_name_a),
                                params: None,
                                result: None,
                                ok: false,
                                error: Some(err_a),
                                llm_model: None,
                                tokens_in: None,
                                tokens_out: None,
                                cost_usd: None,
                                latency_ms: Some(duration_ms as i64),
                                session_id: None,
                                device_id: None,
                                request_id,
                                iteration: None,
                                tool_calls: None,
                            })
                            .await;
                    });
                }
                return ExecutedToolResult {
                    tool_call_id,
                    tool_name,
                    result,
                    ok: false,
                    duration_ms,
                    error: Some(err_msg),
                };
            }
        } else {
            debug!(tool = %tool_name, session_id = %session_id, "HITL skipped: session-scoped approval cached");
        }
    }

    // Execute with timeout — also races against the connection's abort token so
    // that chat.abort propagates into running tools cooperatively (C2-B).
    let exec_fut = registry.execute(&tool_name, params.clone(), tool_context, cancel.clone());
    let tool_result = tokio::select! {
        r = timeout(Duration::from_secs(timeout_secs), exec_fut) => match r {
            Ok(r) => r,
            Err(_) => ToolResult::error(
                "TIMEOUT",
                format!("Tool '{tool_name}' timed out after {timeout_secs}s"),
            ),
        },
        _ = cancel.cancelled() => {
            debug!(tool = %tool_name, "Tool cancelled via abort token (C2-B)");
            ToolResult::cancelled(format!("Tool '{tool_name}' was cancelled"))
        },
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    let ok = !tool_result.is_error() && !tool_result.is_cancelled();
    let error = if ok {
        None
    } else {
        // Extract message from the Error or Cancelled variant.
        match &tool_result {
            ToolResult::Error { message, .. } => Some(message.clone()),
            ToolResult::Cancelled { reason } => Some(reason.clone()),
            _ => Some("Unknown tool error".to_string()),
        }
    };

    emit_tool_result(tx, &tool_name, &tool_call_id, &tool_result, duration_ms);

    // A10: audit tool.result for main execution path.
    if let Some(store) = audit.cloned() {
        let tool_name_a = tool_name.clone();
        let error_a = error.clone();
        let request_id = Uuid::new_v4();
        tokio::spawn(async move {
            let _ = store
                .log(&AuditEntry {
                    action_type: ActionType::ToolCall,
                    method: Some("tool.result".to_string()),
                    tool_name: Some(tool_name_a),
                    params: None,
                    result: None,
                    ok,
                    error: error_a,
                    llm_model: None,
                    tokens_in: None,
                    tokens_out: None,
                    cost_usd: None,
                    latency_ms: Some(duration_ms as i64),
                    session_id: None,
                    device_id: None,
                    request_id,
                    iteration: None,
                    tool_calls: None,
                })
                .await;
        });
    }

    // Phase 11: tool-memory writeback — persist live-state facts to the brain
    // with TrustLevel::ToolVerified so future chat.send auto-recall surfaces
    // the fresh answer instead of a stale memory.
    //
    // The writeback is fire-and-forget: we spawn a background task so that the
    // (potentially slow) brain I/O does not block the next tool call.  Failure
    // already only produces a `warn!`, so detaching the task is safe — we lose
    // nothing the caller depended on.
    if ok && writeback_enabled {
        if let Some(brain) = brain {
            let output_json = serde_json::to_value(&tool_result).unwrap_or(serde_json::Value::Null);
            let hints: Vec<MemoryHint> = registry
                .get(&tool_name)
                .map(|t| t.memory_hints(&params, &output_json))
                .unwrap_or_default();

            if !hints.is_empty() {
                // Clone what the spawned task needs; `brain` is an &Arc<Brain>
                // so one clone gives us an owned Arc without touching the Brain.
                let brain_arc = Arc::clone(brain);
                let tool_name_owned = tool_name.clone();

                tokio::spawn(async move {
                    for hint in hints {
                        // Compute expiry as ISO-8601 string: now + hint.volatility.
                        let valid_to = {
                            let expiry_secs = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs()
                                .saturating_add(hint.volatility.as_secs());
                            // Build an ISO-8601 timestamp without pulling in extra
                            // deps — chrono is already in the workspace.
                            let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(
                                expiry_secs as i64,
                                0,
                            )
                            .unwrap_or_default();
                            Some(dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
                        };

                        // FIX #6: no explicit hint content → derive a plain-text
                        // digest instead of dumping raw serde JSON (always rejected
                        // by the ingestion quality filter as a JSON blob).
                        let content = hint.content.clone().unwrap_or_else(|| {
                            digest_tool_output(&hint.entity, &hint.key, &output_json)
                        });
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
                            // Phase 4: benign ingestion-filter / dedup rejections
                            // ("memory rejected …") are expected for live-state tool
                            // output — log at debug so they don't spam warn. Real
                            // writeback failures (brain I/O) stay at warn.
                            let msg = e.to_string();
                            if msg.starts_with("memory rejected") {
                                debug!(
                                    tool = %tool_name_owned,
                                    entity = %hint.entity,
                                    key = %hint.key,
                                    reason = %msg,
                                    "Phase 11: tool memory writeback skipped (filtered)"
                                );
                            } else {
                                warn!(
                                    tool = %tool_name_owned,
                                    entity = %hint.entity,
                                    key = %hint.key,
                                    error = %msg,
                                    "Phase 11: tool memory writeback failed"
                                );
                            }
                        } else {
                            debug!(
                                tool = %tool_name_owned,
                                entity = %hint.entity,
                                key = %hint.key,
                                "Phase 11: tool memory writeback succeeded"
                            );
                        }
                    }
                });
                // Task handle is intentionally dropped — fire-and-forget.
            }
        }
    }

    ExecutedToolResult {
        tool_call_id,
        tool_name,
        result: tool_result,
        ok,
        duration_ms,
        error,
    }
}

/// FIX #3: render a JSON tool result as text a small local model can actually
/// read. Infra tools (remote_host_status, host_status, docker_manage) return a
/// `{"output": "...", "host": ..., "via": ...}` object; dumping the whole
/// escaped-JSON blob (remote_host_status → 3896 chars) made the abliterated model
/// flail. If one obvious human-readable field carries the payload, feed just
/// that plain text; otherwise pretty-print (still far more legible than the
/// compact escaped form). No information is dropped — the fallback is lossless.
fn flatten_json_result(v: &serde_json::Value) -> String {
    if let Some(obj) = v.as_object() {
        for key in ["output", "stdout", "text", "result", "message", "content"] {
            if let Some(s) = obj.get(key).and_then(|f| f.as_str()) {
                if !s.trim().is_empty() {
                    return s.to_string();
                }
            }
        }
        return serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    }
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// FIX #6: derive a short plain-text digest for brain writeback when a tool
/// provides no explicit `MemoryHint.content`. The old path dumped the raw serde
/// JSON blob, which `quality::should_ingest` rejects as a ">500 char JSON blob"
/// (starts with `{`/`[`), so infra-tool memories were ALWAYS lost. Prefixing with
/// the hint's entity/key guarantees the text never starts with `{`/`[` and anchors
/// it semantically, and the dominant human-readable field is preferred so durable
/// facts survive. Genuinely ephemeral snapshots are still caught by the filter's
/// other rules (ephemeral-metric / past-tense-ops) — we do not relax them.
fn digest_tool_output(entity: &str, key: &str, output: &serde_json::Value) -> String {
    let body = flatten_json_result(output);
    let body = body.trim();
    let body = if body.chars().count() > 400 {
        let head: String = body.chars().take(400).collect();
        format!("{head}…")
    } else {
        body.to_string()
    };
    format!("{entity} {key}: {body}")
}

/// Extract a tool result's text content for the LLM context, truncating to
/// `max_chars` characters (`0` = unlimited). Oversized payloads (82KB
/// cc_chronicle dumps, 66KB read_file results) multiply per-iteration prefill
/// time and exhaust the run token ceiling; the marker tells the model it is
/// looking at a prefix so it can re-query narrower if it needs the tail.
pub(crate) fn tool_result_content(result: &ToolResult, max_chars: usize) -> String {
    let content = match result {
        ToolResult::Text { content } => content.clone(),
        ToolResult::Json { content } => flatten_json_result(content),
        ToolResult::Error { message, .. } => message.clone(),
        ToolResult::Cancelled { reason } => reason.clone(),
    };
    if max_chars == 0 {
        return content;
    }
    let total = content.chars().count();
    if total <= max_chars {
        return content;
    }
    let truncated: String = content.chars().take(max_chars).collect();
    format!("{truncated}\n\n[tool output truncated: showing {max_chars} of {total} chars]")
}

fn emit_tool_result(
    tx: &mpsc::UnboundedSender<Message>,
    tool_name: &str,
    tool_call_id: &str,
    result: &ToolResult,
    duration_ms: u64,
) {
    let is_error = result.is_error();
    // Serialize the full result as content; callers can extract the relevant field.
    let content = serde_json::to_value(result).unwrap_or(serde_json::Value::Null);
    let event = Frame::event(
        "tool.result",
        serde_json::json!({
            "tool_name": tool_name,
            "tool_call_id": tool_call_id,
            "ok": !is_error,
            "content": content,
            "duration_ms": duration_ms,
        }),
    );
    let _ = send_frame(tx, &event);
}

// ---------------------------------------------------------------------------
// C2-A tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tool_result_content_tests {
    use super::*;

    #[test]
    fn under_cap_passes_through_unchanged() {
        let r = ToolResult::Text {
            content: "short output".to_string(),
        };
        assert_eq!(tool_result_content(&r, 100), "short output");
    }

    #[test]
    fn zero_cap_means_unlimited() {
        let big = "x".repeat(50_000);
        let r = ToolResult::Text {
            content: big.clone(),
        };
        assert_eq!(tool_result_content(&r, 0), big);
    }

    #[test]
    fn over_cap_truncates_with_marker() {
        let r = ToolResult::Text {
            content: "a".repeat(20_000),
        };
        let out = tool_result_content(&r, 1_000);
        assert!(out.starts_with(&"a".repeat(1_000)));
        assert!(
            out.ends_with("[tool output truncated: showing 1000 of 20000 chars]"),
            "marker missing or wrong: ...{}",
            &out[out.len().saturating_sub(80)..]
        );
        // 1000 payload chars + marker, nowhere near the original size
        assert!(out.len() < 1_200);
    }

    #[test]
    fn truncation_is_char_safe_on_multibyte_utf8() {
        // 4-byte emoji repeated; a byte-based cut would panic or split a char.
        let r = ToolResult::Text {
            content: "🦀".repeat(5_000),
        };
        let out = tool_result_content(&r, 100);
        assert!(out.starts_with(&"🦀".repeat(100)));
        assert!(out.contains("[tool output truncated: showing 100 of 5000 chars]"));
    }

    #[test]
    fn json_error_and_cancelled_variants_extract_and_cap() {
        let json = ToolResult::Json {
            content: serde_json::json!({ "k": "v".repeat(9_000) }),
        };
        assert!(tool_result_content(&json, 500).contains("[tool output truncated"));

        let err = ToolResult::Error {
            code: "boom".to_string(),
            message: "failed".to_string(),
        };
        assert_eq!(tool_result_content(&err, 500), "failed");

        let cancelled = ToolResult::Cancelled {
            reason: "aborted by user".to_string(),
        };
        assert_eq!(tool_result_content(&cancelled, 500), "aborted by user");
    }

    #[test]
    fn json_dominant_output_field_is_fed_as_plain_text() {
        // FIX #3: remote_host_status-shaped result — the model should see the plain
        // `output` text, not the escaped `{"output":...,"host":...}` blob.
        let remote_host = ToolResult::Json {
            content: serde_json::json!({
                "output": "docker-host IP: 192.0.2.20\nDocker: 19 containers up",
                "host": "192.0.2.20",
                "via": "ssh",
            }),
        };
        let out = tool_result_content(&remote_host, 0);
        assert_eq!(out, "docker-host IP: 192.0.2.20\nDocker: 19 containers up");
        assert!(!out.contains('{'), "must not contain raw JSON braces");
    }

    #[test]
    fn json_without_dominant_field_pretty_prints() {
        // No output/stdout/text/result/message/content key → lossless pretty JSON.
        let obj = ToolResult::Json {
            content: serde_json::json!({ "count": 3, "names": ["a", "b"] }),
        };
        let out = tool_result_content(&obj, 0);
        assert!(out.contains("\"count\": 3"));
        assert!(out.contains("\"names\""));
    }

    #[test]
    fn digest_prefixes_entity_and_never_starts_with_brace() {
        // FIX #6: remote_host_status-shaped output → digest carries the plain `output`
        // text, anchored by entity/key, and never starts with `{` (so it can't be
        // rejected as a raw-JSON blob by the ingestion quality filter).
        let out = serde_json::json!({
            "output": "Docker: 19 containers up",
            "detail": "brief",
        });
        let d = digest_tool_output("docker-host", "services", &out);
        assert_eq!(d, "docker-host services: Docker: 19 containers up");
        assert!(!d.starts_with('{'));

        // No dominant field → still prefixed, still not a raw JSON blob, and capped.
        let big = serde_json::json!({ "blob": "x".repeat(2000) });
        let d2 = digest_tool_output("workstation", "status", &big);
        assert!(d2.starts_with("workstation status: "));
        assert!(!d2.starts_with('{'));
        assert!(d2.chars().count() <= "workstation status: ".chars().count() + 401);
    }
}

#[cfg(test)]
mod in_flight_registry_tests {
    use super::*;

    /// Build a registry with a session entry that already has one tool recorded.
    fn make_registry_with_entry(
        session_id: &str,
        tool_call_id: &str,
        tool_name: &str,
    ) -> Arc<DashMap<String, HashMap<String, String>>> {
        let r: Arc<DashMap<String, HashMap<String, String>>> = Arc::new(DashMap::new());
        r.entry(session_id.to_string())
            .or_default()
            .insert(tool_call_id.to_string(), tool_name.to_string());
        r
    }

    /// Test 1: InFlightGuard removes the entry on drop.
    #[test]
    fn guard_removes_entry_on_drop() {
        let session_id = "sess-a";
        let tool_call_id = "tc-1";
        let registry = make_registry_with_entry(session_id, tool_call_id, "read_file");

        assert_eq!(registry.get(session_id).unwrap().len(), 1);

        {
            let _guard = InFlightGuard {
                registry: Arc::clone(&registry),
                session_id: session_id.to_string(),
                tool_call_id: tool_call_id.to_string(),
            };
            // entry still present while guard is alive
            assert_eq!(registry.get(session_id).unwrap().len(), 1);
        }
        // after drop the entry should be gone
        assert_eq!(
            registry.get(session_id).map(|m| m.len()).unwrap_or(0),
            0,
            "InFlightGuard should remove the entry on drop"
        );
    }

    /// Test 2: Guard for one tool does not disturb a second concurrent tool in
    /// the same session.
    #[test]
    fn guard_only_removes_own_entry() {
        let session_id = "sess-b";
        let registry: Arc<DashMap<String, HashMap<String, String>>> = Arc::new(DashMap::new());
        // Insert two tools.
        registry
            .entry(session_id.to_string())
            .or_default()
            .insert("tc-a".to_string(), "read_file".to_string());
        registry
            .entry(session_id.to_string())
            .or_default()
            .insert("tc-b".to_string(), "shell".to_string());

        assert_eq!(registry.get(session_id).unwrap().len(), 2);

        {
            let _guard = InFlightGuard {
                registry: Arc::clone(&registry),
                session_id: session_id.to_string(),
                tool_call_id: "tc-a".to_string(),
            };
        }

        // tc-b must still be present.
        let remaining = registry.get(session_id).unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "only the guarded entry should be removed"
        );
        assert!(
            remaining.contains_key("tc-b"),
            "tc-b must survive tc-a drop"
        );
    }

    /// Test 3: Two sessions are fully independent — dropping a guard for one
    /// session does not touch the other session's registry entry.
    #[test]
    fn guard_does_not_cross_sessions() {
        let sess_x = "sess-x";
        let sess_y = "sess-y";
        let registry: Arc<DashMap<String, HashMap<String, String>>> = Arc::new(DashMap::new());
        registry
            .entry(sess_x.to_string())
            .or_default()
            .insert("tc-x".to_string(), "list_files".to_string());
        registry
            .entry(sess_y.to_string())
            .or_default()
            .insert("tc-y".to_string(), "write_file".to_string());

        {
            let _guard = InFlightGuard {
                registry: Arc::clone(&registry),
                session_id: sess_x.to_string(),
                tool_call_id: "tc-x".to_string(),
            };
        }

        // sess-x entry should be gone; sess-y untouched.
        assert_eq!(
            registry.get(sess_x).map(|m| m.len()).unwrap_or(0),
            0,
            "sess-x entry should have been removed"
        );
        assert_eq!(
            registry.get(sess_y).map(|m| m.len()).unwrap_or(0),
            1,
            "sess-y entry must not be disturbed"
        );
    }

    /// Test 4: The in-flight registry correctly reflects the blocked condition
    /// that `handle_session_set_cwd` checks — if any entries exist for a session
    /// the count is > 0, and after the guard drops it falls to 0.
    #[test]
    fn registry_reflects_blocked_then_clear() {
        let session_id = "sess-cwd";
        let registry: Arc<DashMap<String, HashMap<String, String>>> = Arc::new(DashMap::new());

        // Initially no tools in flight → set_cwd check passes.
        let count_before = registry.get(session_id).map(|m| m.len()).unwrap_or(0);
        assert_eq!(count_before, 0, "should be clear before any tools start");

        // Simulate tool starting.
        registry
            .entry(session_id.to_string())
            .or_default()
            .insert("tc-1".to_string(), "shell".to_string());

        let count_during = registry.get(session_id).map(|m| m.len()).unwrap_or(0);
        assert!(count_during > 0, "set_cwd should be blocked now");

        // Simulate tool finishing (guard drop).
        let guard = InFlightGuard {
            registry: Arc::clone(&registry),
            session_id: session_id.to_string(),
            tool_call_id: "tc-1".to_string(),
        };
        drop(guard);

        let count_after = registry.get(session_id).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            count_after, 0,
            "set_cwd should be unblocked after tool completes"
        );
    }
}

// ---------------------------------------------------------------------------
// Text-format ("pythonic") tool-call recovery
// ---------------------------------------------------------------------------

/// Recover text-format ("pythonic") tool calls that llama-server's Hermes
/// parser left as plain content.
///
/// Abliterated/finetuned models intermittently emit a call as text such as
/// `[list_files(path="C:\\Users\\deploy\\Projects")]` instead of the structured
/// `<tool_call>{…}` Hermes form, so the OpenAI-compat layer returns it as
/// `content` with `finish_reason: "stop"` and no `tool_calls`. This scans for
/// `name(args)` where `name` is an advertised tool and rebuilds proper
/// [`ToolCall`]s so the agentic loop can execute them.
///
/// Security: only names present in `advertised` are emitted — never an arbitrary
/// parsed identifier. At most `max_calls` are returned.
///
/// ponytail: hand-rolled scan, no regex dep. Naive if an argument value contains
/// an unescaped `)` outside of quotes; upgrade to a real tokenizer if the model
/// starts emitting such args.
pub fn parse_text_tool_calls(text: &str, advertised: &[String], max_calls: usize) -> Vec<ToolCall> {
    let mut out: Vec<ToolCall> = Vec::new();
    if max_calls == 0 {
        return out;
    }
    for name in advertised {
        let needle = format!("{name}(");
        let mut search_from = 0usize;
        while let Some(rel) = text[search_from..].find(&needle) {
            let start = search_from + rel;
            let args_open = start + needle.len();
            search_from = args_open;
            // Left word boundary: preceding char must not be an identifier char,
            // so tool `read_file` does not match inside `xread_file(`.
            let left_ok = text[..start]
                .chars()
                .next_back()
                .map(|c| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(true);
            if !left_ok {
                continue;
            }
            if let Some(close_rel) = find_close_paren(&text[args_open..]) {
                let arg_str = &text[args_open..args_open + close_rel];
                out.push(ToolCall {
                    id: format!("txt_{}", out.len()),
                    type_: "function".to_string(),
                    function: ToolCallFunction {
                        name: name.clone(),
                        arguments: parse_pythonic_args(arg_str),
                    },
                });
                if out.len() >= max_calls {
                    return out;
                }
            }
        }
    }
    out
}

/// Byte offset of the `)` that closes the already-opened `(`, respecting nested
/// parens and quoted strings. `None` if unbalanced.
fn find_close_paren(s: &str) -> Option<usize> {
    let mut depth = 1i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
        } else {
            match c {
                '"' | '\'' => quote = Some(c),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Convert a pythonic arg list (`path="x", recursive=true`) into a JSON object
/// string suitable for [`ToolCallFunction::arguments`]. Positional (unnamed)
/// args are ignored because the tool registry addresses parameters by name.
fn parse_pythonic_args(s: &str) -> String {
    let mut map = serde_json::Map::new();
    for part in split_top_level_commas(s) {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if let Some(eq) = p.find('=') {
            let key = p[..eq].trim().trim_matches(|c| c == '"' || c == '\'');
            if !key.is_empty() {
                map.insert(key.to_string(), json_value_from_token(p[eq + 1..].trim()));
            }
        }
    }
    serde_json::Value::Object(map).to_string()
}

/// Best-effort conversion of a single pythonic value token into a JSON value.
fn json_value_from_token(t: &str) -> serde_json::Value {
    use serde_json::Value;
    let t = t.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        // Prefer strict JSON (handles \\, \", \uXXXX). Fall back to a lenient
        // strip+unescape for Windows paths with single backslashes that are not
        // valid JSON escapes (e.g. `"C:\Users\x"`).
        if let Ok(v @ Value::String(_)) = serde_json::from_str::<Value>(t) {
            return v;
        }
        return Value::String(lenient_unescape(&t[1..t.len() - 1]));
    }
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return Value::String(lenient_unescape(&t[1..t.len() - 1]));
    }
    match t {
        "true" | "True" => return Value::Bool(true),
        "false" | "False" => return Value::Bool(false),
        "null" | "None" | "" => return Value::Null,
        _ => {}
    }
    if let Ok(v @ Value::Number(_)) = serde_json::from_str::<Value>(t) {
        return v;
    }
    Value::String(t.to_string())
}

/// Collapse the common backslash escapes; keep any other `\x` sequence verbatim
/// so Windows paths survive (`C:\Users` stays `C:\Users`).
fn lenient_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('\'') => out.push('\''),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Split on commas that are not inside quotes or parens/brackets/braces.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut last = 0usize;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&s[last..i]);
                last = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[last..]);
    parts
}

#[cfg(test)]
mod text_parse_tests {
    use super::*;

    fn tools() -> Vec<String> {
        vec!["list_files".to_string(), "read_file".to_string()]
    }

    fn args_of(c: &ToolCall) -> serde_json::Value {
        serde_json::from_str(&c.function.arguments).unwrap()
    }

    #[test]
    fn recovers_bracketed_pythonic_call_with_windows_path() {
        let txt = r#"[list_files(path="C:\\Users\\deploy\\Projects")]"#;
        let calls = parse_text_tool_calls(txt, &tools(), 4);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "list_files");
        assert_eq!(args_of(&calls[0])["path"], "C:\\Users\\deploy\\Projects");
    }

    #[test]
    fn recovers_unbracketed_call_with_typed_args() {
        let calls = parse_text_tool_calls(
            r#"read_file(path="a.txt", recursive=true, n=3)"#,
            &tools(),
            4,
        );
        assert_eq!(calls.len(), 1);
        let a = args_of(&calls[0]);
        assert_eq!(a["path"], "a.txt");
        assert_eq!(a["recursive"], true);
        assert_eq!(a["n"], 3);
    }

    #[test]
    fn ignores_unadvertised_tool_names() {
        let calls = parse_text_tool_calls("[delete_everything(x=1)]", &tools(), 4);
        assert!(calls.is_empty());
    }

    #[test]
    fn word_boundary_prevents_substring_match() {
        let calls = parse_text_tool_calls(r#"xread_file(path="a")"#, &tools(), 4);
        assert!(calls.is_empty());
    }

    #[test]
    fn ignores_prose_without_a_call() {
        let calls = parse_text_tool_calls("I'll check the files for you.", &tools(), 4);
        assert!(calls.is_empty());
    }

    #[test]
    fn respects_max_calls_cap() {
        let txt = r#"[list_files(path="a"), list_files(path="b"), list_files(path="c")]"#;
        let calls = parse_text_tool_calls(txt, &tools(), 2);
        assert_eq!(calls.len(), 2);
    }
}
