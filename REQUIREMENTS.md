# Ironjaw — Business Requirements

## Stakeholders

- **Sean Caso** — author
- **Dr. CLAW Android app** — primary client application

---

## Vision

Single Rust binary replacing 7+ Node.js services (OpenClaw Gateway, CC Bridge, CC Chronicle, and
future services) with better performance, reliability, and a unified codebase.

The goal is one process, one config file, one port — with no Node.js runtime dependency, no
inter-process latency, and a clean architecture that can be extended without touching existing
service code.

---

## Business Requirements

### BR-001 — WebSocket Gateway with Token Auth and Protocol v4

A WebSocket server must accept connections on a configurable port (default: 18794), enforce token
authentication within 10 seconds of connection, and speak Protocol v4 with 8 frame types: Cmd,
CmdRes, Stream, StreamEnd, Event, Err, Ping, Pong.

**Acceptance**: Connection with valid token succeeds. Connection with invalid or missing token is
rejected within 10 seconds. All 8 frame types round-trip correctly.

---

### BR-002 — LLM Routing with Streaming

The server must route LLM requests to one or more backends (llama-server on localhost:8080,
OpenAI-compatible APIs) and stream token-by-token responses back over the WebSocket using Stream
and StreamEnd frames. Provider selection and model routing must be configurable.

**Acceptance**: Streamed responses appear progressively on the client. Provider fallback works when
primary is unreachable. Router respects per-request model overrides.

---

### BR-003 — Persistent Multi-Session Management

Sessions and messages must persist in SQLite (WAL mode). The server must support multiple
concurrent sessions, cursor-based pagination for message history, and session metadata (title,
timestamps, model, token counts).

**Acceptance**: Sessions survive server restart. Pagination returns correct pages. Concurrent
sessions do not bleed state.

---

### BR-004 — 4-Tier Permission Enforcement with HITL Gates and Rate Limiting

Every method call must be evaluated against a 4-tier permission model (Read, Execute, Confirm,
Admin). Confirm-tier and Admin-tier actions must pause execution, emit an approval request to the
client, and wait for explicit human-in-the-loop (HITL) approval before proceeding. Rate limiting
must be enforced per-connection.

**Acceptance**: Unauthorized calls return an Err frame. Confirm-tier calls block until approved or
rejected. Rejected calls are not executed. Rate limit violations return an Err frame without
executing the call.

---

### BR-005 — Tool Execution Engine with Plugin Architecture

A tool trait must define the interface for all tools. A registry must allow tools to be registered
at startup and looked up by name. The 4 built-in tools (shell exec, file read, file write,
HTTP fetch) must be implemented and registered by default.

**Acceptance**: All 4 built-in tools execute correctly. Unknown tool names return an Err frame.
Tool registry is extensible without modifying core dispatch code.

---

### BR-006 — RAG Memory with 3-Layer Context and Auto-Retrieval

A RAG brain must maintain 3 layers of memory: working memory (in-session), episodic memory
(cross-session, embedding-retrieved), and semantic memory (long-term facts). Relevant context must
be automatically retrieved and injected into LLM prompts based on embedding similarity.

**Acceptance**: Queries return semantically relevant results. Memory persists across server
restarts. Context injection improves LLM response relevance without exceeding token budget.

---

### BR-007 — Integration Tools (13 External Service Tools)

13 integration tools must be registered: search (SearXNG, Perplexica, LDR), CC tools (session
list, session read, session search), infra tools (Docker status, SSH exec), git tools (status,
log, diff), secrets vault (get/set with AES-256-GCM), and Home Assistant (entity control).

**Acceptance**: All 13 tools execute against real services in integration tests. Secrets vault
encrypts at rest. HA tool controls entities. Tool failures return structured errors, not panics.

---

### BR-008 — Proactive Engine (Cron Scheduler and Workflow DAG)

A proactive engine must execute scheduled tasks (cron syntax) and multi-step workflow DAGs
autonomously. Workflows must support approval gates at any step — pausing execution and requesting
human approval before continuing. Completed tasks must be persisted.

**Acceptance**: Cron tasks fire at correct intervals. Workflow steps execute in DAG order.
Approval gates block until approved. Failed steps emit Event frames to connected clients.

---

### BR-009 — Voice Pipeline (STT and TTS)

A voice pipeline must accept audio input, transcribe via Whisper (STT), and synthesize speech via
Orpheus (TTS). Both push-to-talk and voice activity detection (VAD) modes must be supported.
Audio frames must be transmitted over the WebSocket binary channel.

**Acceptance**: Audio round-trips from client mic to transcription to LLM to synthesized speech.
VAD correctly segments utterances. Push-to-talk mode transmits only while key is held.

---

### BR-010 — Vision and Multimodal Support

The server must accept image payloads, detect format (JPEG, PNG, WebP, GIF), convert to base64,
and include images in LLM prompts for multimodal models. An OCR tool must extract text from
images using a local engine.

**Acceptance**: Image payloads are correctly detected and encoded. Multimodal LLM calls include
image content. OCR tool returns extracted text. Unsupported formats return an Err frame.

---

### BR-011 — Multi-Device Support

A device registry must track connected devices with metadata (name, type, last seen). An offline
message queue must buffer events for disconnected devices and deliver them on reconnect.
Push notifications must be sendable to registered devices.

**Acceptance**: Events queued while device is offline are delivered on reconnect in order.
Device registry survives server restart. Notifications reach registered devices.

---

### BR-012 — Operational Polish

Circuit breakers must prevent cascade failures to external services. Internal metrics must track
request counts, latencies, and error rates. Backup and export endpoints must allow data
portability. Structured logging must be consistent across all crates.

**Acceptance**: Circuit breaker trips after configurable failure threshold and recovers after
timeout. Metrics endpoint returns current counters. Export produces valid JSON. No unstructured
log lines in production paths.

---

### BR-013 — Cutover (Replace Existing Services, Coexist on Port 18794)

Ironjaw must run on port 18794 and coexist with existing services (OpenClaw on 18789, CC Bridge
on 18790, CC Chronicle on 18793) during the transition period. After validation, Caddy, Task
Scheduler, and the Dr. CLAW Android app must be updated to point to 18794, and legacy services
decommissioned.

**Acceptance**: Ironjaw starts on 18794 without conflicting with existing services. All Dr. CLAW
features work against Ironjaw. Legacy services can be stopped without user-visible regression.

---

## Non-Functional Requirements

### Performance
- Single binary — no runtime dependency on Node.js, Python, or any interpreter
- Less than 5 MB per connection memory footprint
- Rust-native async (Tokio) — no blocking in hot paths
- Streaming responses begin within 100ms of first token from LLM

### Security
- Prompt injection defense — 6+ patterns stripped from all tool outputs before injection
- HITL gates on all destructive operations (Confirm + Admin tiers)
- Encrypted secrets vault using AES-256-GCM (ring crate)
- Auth token required for all WebSocket connections
- 10-second auth timeout prevents resource exhaustion from hanging connections
- Shell tool output capped at 64 KB
- File tools restricted to configured working directory

### Reliability
- Circuit breakers on all external service calls (LLM, search, HA, SSH)
- Message queue for offline devices (SQLite-backed, ordered delivery)
- WAL-mode SQLite for all persistent state — concurrent reads, serial writes
- All services restart-safe — no in-memory-only state for anything durable

### Cost
- All integrations free and unlimited — no paid APIs, no quota limits
- llama-server on localhost:8080 as primary LLM (no OpenAI calls unless explicitly routed)

### Observability
- Append-only audit trail for all Confirm and Admin tier actions
- Structured logging via tracing crate (JSON in production, pretty in dev)
- Internal metrics for request throughput, latency, and error rates
- Health endpoint returns structured JSON with all subsystem statuses

---

## Acceptance Criteria (Overall)

- 346 tests passing across all 12 crates (`cargo test --workspace`)
- All 13 phases implemented and tested
- Coexists with existing services on port 18794
- `/health` endpoint returns valid JSON with all subsystem statuses
- All WebSocket methods enforce correct permission tiers
- No `clippy::correctness` warnings in production code
- `rustfmt` clean across workspace
