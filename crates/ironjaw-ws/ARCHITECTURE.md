# ironjaw-ws: Agentic Tool-Call Loop Architecture

> **Scope:** This document covers the agentic chat subsystem introduced across
> Phases 1-18.  For the full gateway architecture (sessions, brain, voice, etc.)
> see `Ironjaw/CLAUDE.md` and `docs/plans/2026-03-02-ironjaw-design.md`.

---

## 1. Overview

The agentic loop enables Ironjaw to execute multi-step, tool-using conversations
autonomously.  When a `chat.send` request arrives and `[chat.agentic] enabled =
true`, the gateway enters `run_agentic_chat` instead of the legacy single-pass
path.

Key properties:

- **Iterative** — the LLM is called repeatedly until it produces a text-only
  response or the `max_iterations` ceiling is hit.
- **Tool-aware** — streamed `tool_call` deltas are reassembled per iteration
  and dispatched to the tool registry.
- **HITL-safe** — Confirm/MultiConfirm tools block the loop until the client
  approves or times out; the loop never auto-approves without a cached session
  grant.
- **Abort-aware** — a `tokio::select!` on `abort_token.cancelled()` exits the
  outer loop cleanly at any iteration boundary.
- **Brain-integrated** — Phase 11 writeback persists live-state tool results to
  the brain with `TrustLevel::ToolVerified` and a computed `valid_to` TTL.

---

## 2. Data Flow (ASCII)

```
Client                     Ironjaw (handler.rs)             Ironjaw (agentic.rs)
  │                               │                                  │
  │──── chat.send ───────────────>│                                  │
  │                               │ [agentic enabled?]               │
  │<─── res {streaming:true} ─────│ ack immediately                  │
  │                               │                                  │
  │                               │────── run_agentic_chat ─────────>│
  │                               │                                  │
  │      ┌── iteration 0 ─────────────────────────────────────────── │ ──┐
  │      │                        │                                  │   │
  │      │   LLM stream begins    │                                  │   │
  │<─────│── stream {delta} ──────│                                  │   │
  │<─────│── stream {delta} ──────│   (tool_call deltas             │   │
  │<─────│── stream {done} ───────│    accumulated in delta_map)    │   │
  │      │                        │                                  │   │
  │      │   [tool calls present?]│                                  │   │
  │      │                        │──── execute_tool_batch ─────────>│   │
  │      │                        │                                  │   │
  │      │                        │   for each tool call (serial):  │   │
  │<─────│── event tool.start ────│<─────────────────────────────── │   │
  │      │                        │   [Confirm tier?]               │   │
  │<─────│── event permission.req─│<─────────────────────────────── │   │
  │──────│── cmd.res {approved} ──│──────────────────────────────── │   │
  │      │                        │   execute + timeout              │   │
  │<─────│── event tool.result ───│<─────────────────────────────── │   │
  │      │                        │   [Phase 11: brain writeback]   │   │
  │      │                        │                                  │   │
  │      └── tool results → next LLM call ──────────────────────────│ ──┘
  │                               │                                  │
  │      ┌── iteration N ──────── │ ──────────────────────────────  │
  │      │   [LLM text only]      │                                  │
  │<─────│── stream {delta} ──────│                                  │
  │<─────│── stream {done} ───────│  break — no tool_calls          │
  │      └────────────────────────│──────────────────────────────── │
  │                               │<──── Ok(()) ─────────────────── │
```

---

## 3. Frame Protocol Additions (Phase 18)

The agentic loop introduces three new event types on top of the baseline
Protocol v4 frame set.

### 3.1 `tool.start`

Emitted before a tool begins executing.  Lets the client show an "in-progress"
indicator for the named tool.

```json
{
  "type": "event",
  "event": "tool.start",
  "data": {
    "tool_name": "shell",
    "tool_call_id": "call_abc123"
  }
}
```

### 3.2 `tool.result`

Emitted after a tool finishes (success or error).

```json
{
  "type": "event",
  "event": "tool.result",
  "data": {
    "tool_name": "shell",
    "tool_call_id": "call_abc123",
    "ok": true,
    "content": { "stdout": "...", "exit_code": 0 },
    "duration_ms": 142
  }
}
```

When `ok` is `false`, `content` contains an `Error` variant with `code` and
`message` fields (e.g. `HITL_DENIED`, `TIMEOUT`, `PARSE_ERROR`).

### 3.3 `permission.request` (source = "agentic_loop")

Re-uses the existing `permission.request` event type but adds
`"source": "agentic_loop"` to distinguish agentic requests from interactive
HITL prompts.

```json
{
  "type": "event",
  "event": "permission.request",
  "data": {
    "tool": "write_file",
    "params": { "path": "...", "content": "..." },
    "request_id": "550e8400-e29b-41d4-a716-446655440000",
    "source": "agentic_loop"
  }
}
```

The client responds with a `cmd.res` frame:

```json
{
  "type": "cmd.res",
  "id": "<request_id>",
  "ok": true,
  "data": { "scope": "session" }
}
```

`scope = "session"` triggers Phase 6 caching — subsequent iterations skip the
HITL dialog for the same tool in this session.

### 3.4 `force_fresh` in `chat.send`

Phase 10 — client can include `"force_fresh": true` in a `chat.send` params to
skip brain auto-inject for that single request.  The flag propagates into
`build_auto_inject_context` inside `handle_chat_send`.

```json
{
  "type": "req",
  "id": "uuid",
  "method": "chat.send",
  "params": {
    "message": "What is the current CPU usage?",
    "force_fresh": true
  }
}
```

---

## 4. Feature Flag Matrix

All agentic features are individually gated.  The table below shows which config
key controls each capability and the code site that reads it.

| Feature | Config key | Default | Code site |
|---------|-----------|---------|-----------|
| Agentic loop master switch | `[chat.agentic] enabled` | `false` | `handler.rs: run_agentic_chat` call guard |
| Max LLM iterations | `[chat.agentic] max_iterations` | `5` | `run_agentic_chat` outer loop bound |
| Per-tool timeout | `[chat.agentic] tool_timeout_secs` | `30` | `execute_tool_batch` timeout arg |
| Session-scoped HITL cache | `[chat.agentic] hitl_session_cache_enabled` | `true` | `execute_single` session-cache check |
| Brain writeback | `[chat.agentic] tool_memory_writeback_enabled` | `true` | `execute_single` Phase 11 block |
| Live-state classifier | `[brain.live_state] enabled` | `true` | `build_auto_inject_context` freshness gate |
| Live-state skip-on-hit | `[brain.live_state] skip_on_live_state` | `false` | freshness filter decision |
| Live-state freshness window | `[brain.live_state] freshness_window_secs` | `300` | staleness threshold |
| Live-state confidence threshold | `[brain.live_state] confidence_threshold` | `0.7` | classifier accept threshold |
| HITL global enable | `[security] hitl_enabled` | `false` | `HitlGate::is_auto_approve()` |
| HITL timeout | `[security] hitl_timeout_secs` | `30` | `HitlGate::wait_for_approval` |

Setting `[security] hitl_enabled = false` puts `HitlGate` into auto-approve
mode — all Confirm-tier tool calls proceed immediately.  This is the default for
development; enable in production for full human oversight.

---

## 5. Configuration Reference

### `[chat.agentic]`

```toml
[chat.agentic]
enabled                    = true   # Master killswitch (flipped ON: Phase 18 finale)
max_iterations             = 5      # Upper bound on LLM→tool→LLM cycles per request
tool_timeout_secs          = 30     # Per-tool execution timeout
hitl_session_cache_enabled = true   # Cache session-scoped HITL approvals (Phase 6)
tool_memory_writeback_enabled = true  # Write tool results to brain (Phase 11)
```

### `[brain.live_state]`

Controls the live-state freshness classifier that filters stale memories on
time-sensitive queries before they reach the LLM.

```toml
[brain.live_state]
enabled                = true   # Enable freshness classifier (Phase 10)
skip_on_live_state     = false  # When true, skip auto-inject entirely if query is live-state
freshness_window_secs  = 300    # Memories older than this (seconds) are considered stale
confidence_threshold   = 0.7    # Classifier must exceed this to flag a query as live-state
llm_fallback           = false  # Use LLM if classifier is uncertain (slower; off by default)
decay_rate             = 0.01   # Per-second salience decay applied to stale memories
```

---

## 6. Dr. CLAW State Machine (Agentic Events)

Dr. CLAW's `AttachedSessionViewModel` tracks the agentic loop state using the
events emitted by Ironjaw.

```
         ┌────────────────────────────────────────┐
         │              IDLE                      │
         │  No active agentic request             │
         └────────────┬───────────────────────────┘
                      │ chat.send dispatched
                      ▼
         ┌────────────────────────────────────────┐
         │           STREAMING                    │
         │  stream {delta} frames arriving        │
         │  ThinkingIndicator shown               │
         └────────────┬───────────────────────────┘
                      │ tool_calls present in stream done
                      ▼
         ┌────────────────────────────────────────┐
         │          TOOL_EXECUTING                │
         │  event: tool.start received            │
         │  ActivityStatusBar shows tool name     │
         └──────┬─────────────────────────┬───────┘
                │ ok                      │ Confirm tier
                ▼                         ▼
         ┌──────────────┐     ┌───────────────────────┐
         │ TOOL_DONE    │     │  AWAITING_PERMISSION  │
         │tool.result   │     │  permission.request   │
         │ (ok=true)    │     │  PermissionDialog shown│
         └──────┬───────┘     └─────────┬─────────────┘
                │                       │ cmd.res {ok}
                │ more iterations?      ▼
                ├──yes──────────> STREAMING
                │
                │ no more tool calls
                ▼
         ┌────────────────────────────────────────┐
         │              IDLE                      │
         │  Final text response displayed         │
         └────────────────────────────────────────┘

  ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─
  Error paths (from any state):
    HITL_DENIED  → tool.result {ok:false, code:"HITL_DENIED"} → IDLE
    TIMEOUT      → tool.result {ok:false, code:"TIMEOUT"}     → IDLE
    abort_token  → loop exits → stream {done}                 → IDLE
```

### Dr. CLAW UI Components Involved

| State | Dr. CLAW component |
|-------|--------------------|
| STREAMING | `StreamingIndicator`, `ThinkingIndicator` (after 3s) |
| TOOL_EXECUTING | `ActivityStatusBar` (shows friendly tool name summary) |
| AWAITING_PERMISSION | `PermissionDialog` in `AttachedSessionScreen` |
| TOOL_DONE | `ToolUseCard` / `ToolResultCard` (collapsible, added v1.1.3) |

---

## 7. Extension Points

### 7.1 Adding a New Tool

1. Implement `Tool` trait in `crates/ironjaw-tools/src/builtin/` or
   `crates/ironjaw-integrations/src/`.
2. Assign a `PermissionTier` via `fn tier()`.
3. To participate in Phase 11 brain writeback, implement `fn memory_hints()`:

```rust
fn memory_hints(&self, params: &Value, output: &Value) -> Vec<MemoryHint> {
    vec![MemoryHint {
        entity: "workstation".into(),
        key: "cpu_usage".into(),
        content: Some(output["cpu_percent"].to_string()),
        volatility: Duration::from_secs(60), // expires in 60s
    }]
}
```

4. Register in `register_builtins()` or `register_integrations()`.
5. The agentic loop picks it up automatically — no changes to `agentic.rs`
   needed.

### 7.2 Changing HITL Behavior

- **Disable HITL globally** (dev/testing): `[security] hitl_enabled = false`
- **Auto-approve for a session**: client sends `cmd.res` with `scope="session"`
  after the first HITL prompt; all subsequent iterations skip the dialog for
  that tool.
- **Custom timeout per tool**: not currently supported; `tool_timeout_secs`
  applies uniformly.  Add a `timeout_secs() -> Option<u64>` method to the
  `Tool` trait and plumb it into `execute_single` if per-tool overrides are
  needed.

### 7.3 Increasing Max Iterations

The default `max_iterations = 5` is conservative.  For longer agentic chains,
raise it in config:

```toml
[chat.agentic]
max_iterations = 15
```

There is no code change required.  The nudge engine (Phase 5) already
re-prompts the LLM when it emits prose without using available tools, so the
iteration budget should be set generously for complex tasks.

### 7.4 Parallel Tool Execution

`execute_tool_batch` runs tools **serially** today (see comment in source: HITL
is not trivially parallelisable across Confirm-tier calls in a single batch).
To add parallel execution for Auto/Notify tools while keeping serial ordering
for Confirm tools:

1. Partition each batch into `auto_calls` and `confirm_calls`.
2. Use `futures::future::join_all` for `auto_calls`.
3. Interleave `confirm_calls` serially at their original batch positions.
4. Merge results in original order before returning.

### 7.5 Live-State Freshness Tuning

The Phase 10 classifier detects time-sensitive queries ("what is the current
CPU usage?") and optionally filters stale brain memories from the LLM context.
Key tuning knobs:

- **Lower `freshness_window_secs`** (e.g. 60): more aggressive staleness —
  any memory older than 1 minute is excluded for live-state queries.
- **Raise `confidence_threshold`** (e.g. 0.9): classifier must be very sure
  the query is live-state before filtering.
- **Set `skip_on_live_state = true`**: skip brain auto-inject entirely for
  live-state queries and let the tool loop populate the answer fresh.

---

*Last updated: 2026-04-22 — Phase 18 finale (all 18 phases complete).*
