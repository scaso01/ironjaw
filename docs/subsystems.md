# Subsystem Deep-Dives

Extracted from CLAUDE.md on 2026-07-31 so it stops loading into every session. Read this when working inside a specific subsystem.


### WebSocket Server (ironjaw-ws)

axum 0.8 with `tokio-tungstenite` WebSocket upgrade. Two HTTP endpoints:
- `GET /health` -- returns `{"status": "ok", "uptime": <seconds>}`
- `GET /ws` -- WebSocket upgrade, full-duplex Protocol v4

`AppState` carries all shared state via 9 sub-state structs (SecurityState, LlmState, BrainState, SessionState, ClaudeState, DeviceState, AgentState, SkillState, ObservabilityState) plus 14 flat fields. Cloned per connection.

`ConnectionContext` tracks per-connection state:
- `device_id`: unique device identifier
- `session_id`: active session (set after `auth.connect`)
- `authenticated`: bool
- `tx`: sender half of WebSocket for async events

### LLM Router (ironjaw-llm)

`LlmRouter` selects providers by model name prefix. Built-in providers:
- **llama-server**: local llama.cpp via OpenAI-compatible API (`/v1/chat/completions`)
- **OpenAI**: cloud API (optional)
- **Anthropic**: Claude API (OAuth from Claude Max or direct API key)
- **OpenRouter**: 300+ models via OpenAI-compatible API (free tier + BYOK). Configured as `type = "openai"` with `extra_headers` for HTTP-Referer/X-Title.
- **Mock**: deterministic responses for tests

Provider selection via model name prefix: `"openrouter/google/gemini-2.5-pro"` routes to OpenRouter, `"anthropic/claude-sonnet-4-6"` routes to Anthropic. `has_provider()` matches by configured name, not enum variant.

Streaming: providers yield token deltas, handler sends `stream` frames.

**OpenAiProvider extra_headers**: Any OpenAI-compatible provider can carry arbitrary extra HTTP headers via `extra_headers` config. Used by OpenRouter for `HTTP-Referer` and `X-Title`. Zero cost if empty.

**ComplexityScorer**: analyzes prompt complexity (token count, tool references, multi-step indicators) to route simple queries to smaller/faster models and complex queries to larger models. Configurable via `[llm.routing]`.

**RoutingOverrides**: per-method or per-skill model overrides (e.g., force `qwen3-coder` for code generation, `qwen3:8b` for simple Q&A).

**Provider Decorators** (composable wrappers):
- `RetryProvider`: exponential backoff retry on transient failures
- `FailoverProvider`: automatic failover to secondary provider on error
- `CacheProvider`: TTL-based response caching for identical prompts

**UsageTracker**: per-model token accounting (prompt/completion tokens, cost estimation, rate tracking). Exposed via `infra.metrics`.

**NudgeEngine**: generates follow-up suggestions based on conversation context.

**JSON Repair**: fixes common LLM output issues (trailing commas, unquoted keys, truncated JSON) before parsing tool calls.

### Sessions (ironjaw-session)

SQLite with WAL mode for concurrent reads. Two tables:
- `sessions`: id, name, created_at, updated_at
- `messages`: id, session_id, role, content, created_at

Pagination via cursor-based `chat.history`. Context window management via `context_token_limit`.

### Security (ironjaw-security)

**Permission Tiers** -- 4 levels enforced on every method call:

| Tier | Name | Behavior |
|------|------|----------|
| 0 | Auto | Proceeds immediately |
| 1 | Notify | Proceeds, sends `permission.notify` event |
| 2 | Confirm | Blocks, sends `permission.request`, waits for approval |
| 3 | MultiConfirm | Same as Confirm with extra confirmation text |

Unknown methods default to Confirm for safety.

**Rate Limiter** -- sliding window per-method:
- `chat.send`: 30 requests / 60s
- `session.create`: 10 / 60s
- `tools.execute`: 60 / 60s
- default: 120 / 60s

Opt-in via `rate_limit_enabled = true`.

**Input Sanitizer** -- strips prompt injection markers:
- `<system>`, `</system>`, `[INST]`, `[/INST]`
- `<<SYS>>`, `<</SYS>>`, `Human:`, `Assistant:`
- Similar injection patterns

### Audit Trail (ironjaw-audit)

Append-only SQLite. Every method call logged with timestamp, method, params (sanitized), result, user/session context.

### Tool Engine (ironjaw-tools)

`Tool` trait with `name()`, `description()`, `schema()`, `tier()`, `execute(input, ctx, cancel)`. The `cancel: tokio_util::sync::CancellationToken` parameter (C2 Sub-B, 2026-05-04) propagates `chat.abort` into running tools — long-running tools wrap their `await` points in `tokio::select!` to honor cancellation; pure-compute tools take the parameter and ignore it. `ToolResult::Cancelled` is the new variant returned on cooperative abort. `ToolRegistry` holds `Vec<Box<dyn Tool>>`. `ParallelExecutor` handles fan-out execution of multiple tools concurrently.

**10 Built-in Tools:**

| Tool | Tier | Description |
|------|------|-------------|
| `read_file` | Auto | Read file content (path must be under working_dir) |
| `list_files` | Auto | List directory entries, optionally recursive |
| `write_file` | Confirm | Write file content (HITL approval required) |
| `shell` | Confirm | Execute shell command (HITL, timeout, 64KB output cap) |
| `process` | Notify | Process listing and management (ps, kill) |
| `patch` | Confirm | Apply unified diffs to files |
| `event_publish` | Notify | Emit custom events to connected clients |
| `schedule_create` | Confirm | Create a proactive schedule |
| `schedule_list` | Auto | List proactive schedules |
| `schedule_toggle` | Notify | Enable/disable a schedule |

### Integrations (ironjaw-integrations)

15 integration tools registered when `[integrations] enabled = true`:

| Category | Tool | Description |
|----------|------|-------------|
| Search | `searxng` | SearXNG meta-search (docker-host :8080) |
| Search | `perplexica` | Perplexica AI search (docker-host :3001) |
| Search | `web_search` | Combined SearXNG + Perplexica |
| CC | `cc_sessions` | CC Bridge session listing |
| CC | `cc_chronicle` | CC Chronicle session search |
| Infra | `host_status` | workstation service status |
| Infra | `remote_host_status` | docker-host service status (SSH) |
| Infra | `docker_manage` | Docker container management (SSH) |
| Infra | `docker_exec` | Execute commands inside Docker containers (SSH) |
| Android | `adb` | Android Debug Bridge (defaults to phone serial; rish-over-ssh fallback for `shell` when adb is unreachable) |
| Git | `git_status` | Git repository status |
| Git | `git_log` | Git commit log |
| HA | `homeassistant` | Home Assistant control (optional) |
| Secrets | `secrets_get` | Read from encrypted vault |
| Secrets | `secrets_set` | Write to encrypted vault |

Home Assistant tool only registers if `ha_url` and `ha_token` are configured.
14 base tools + 1 optional HA = 15 max.

### Brain / RAG Memory (ironjaw-brain)

**3-layer memory model:**

| Layer | Name | Storage | Scope |
|-------|------|---------|-------|
| 1 | Identity | `identity` KV table | Global, always injected |
| 2 | Working Memory | Session messages | Per-session conversation |
| 3 | Long-term | Vector store + FTS5 | Global, hybrid search |

**Auto-retrieval** on every `chat.send`:
1. Query Layer 3 with user message via HybridSearch (vector + FTS5 + knowledge-graph + RRF fusion)
2. Load Layer 1 identity pairs
3. Load context variables (named documents marked as "loaded")
4. Prepend as system messages to LLM call

**Brain v2 features** (2026-03-22):
- **Temporal facts**: `valid_from`/`valid_to` on memories. Contradictions auto-close old facts. Point-in-time queries via `brain.timeline`.
- **3-tier salience decay**: Hot (0.99), Warm (0.95), Cold (0.90) based on recency + access frequency. Reinforcement on recall (+0.02/access).
- **Session summaries**: Auto-generated on session switch, delete, and compaction. Stored as `session_summary` memory type.
- **Context variables**: Named documents loadable/unloadable from LLM context on demand (`brain.context.*` methods).
- **Progressive disclosure**: `brain.recall` supports `detail: "compact"/"full"/"deep"` response depth.
- **Auto entity extraction**: `brain.remember` auto-detects entities and creates knowledge graph edges.

**Embedder**: llama-server `/v1/embeddings` endpoint (OpenAI-compatible).
Embeddings stored as JSON arrays in SQLite. Cosine similarity search O(n), bounded by `max_memories` with LRU eviction.

**KnowledgeGraph** (`graph.rs`): entity-relation triples extracted from conversations. Supports graph traversal queries (neighbors, paths, subgraph extraction). Entities auto-linked on memory store. Auto entity extraction on `brain.remember`. **Graph-aware recall** (2026-06-20): the graph is also a retriever — `HybridSearch.graph_search()` matches query entities to `recall_by_entity` results and folds them into RRF as a third leg (see below). Wired on BOTH the gateway and `--mcp-server` Brain construction paths.

**MemoryCortex** (`cortex.rs`): pub/sub bulletin board for memory events. Subsystems can subscribe to memory creation/update/deletion events. Used by maintenance and graph for reactive updates.

**HybridSearch** (`hybrid_search.rs`): combines vector similarity (cosine), FTS5 full-text search, knowledge-graph entity match, and Reciprocal Rank Fusion (RRF) to merge results. RRF leg weights: vector 1.0 (implicit), `FTS_RRF_WEIGHT` 2.0, `GRAPH_RRF_WEIGHT` 1.5. Fused RRF score is normalized to [0,1] before composite scoring so it competes fairly with recency/importance/trust/strength/frequency. Significantly better recall than vector-only search.

**composite_score() weights** (function in `hybrid_search.rs`, used by `recall_scored()`):

| Signal | Weight | Notes |
|--------|--------|-------|
| semantic | 0.35 | Cosine similarity to query embedding |
| recency | 0.20 | Exponential decay from `accessed_at`; half-life ≈ 30 days |
| importance | 0.15 | `memory.importance` field (0.0–1.0) |
| trust | 0.15 | `TrustLevel.score()` — see trust levels below |
| strength | 0.10 | `memory.strength` field (0.0–1.0), reinforced on recall (+0.02) |
| frequency | 0.05 | Normalized `access_count` capped at 10 |

**TrustLevel enum** (`types.rs`), score = variant_ordinal / 5.0:

| Variant | Score | Assigned by |
|---------|-------|-------------|
| `ModelInferred` (1) | 0.20 | Auto-learn / LLM extraction |
| `Retrieved` (2) | 0.40 | External source retrieval |
| `UserStated` (3) | 0.60 | Explicit `brain.remember` / pending approve |
| `ToolVerified` (4) | 0.80 | Agentic loop writeback after tool execution |
| `SystemPolicy` (5) | 1.00 | System-level facts (set by server) |

**brain.recall `explain: true`**: returns per-memory JSON breakdown with `trust_level`, `trust_score`, `composite_score`, and the `score_weights` map. Useful for debugging recall ranking.

**MemoryMaintenance** (`maintenance.rs`): background task for memory hygiene -- 3-tier salience decay, pruning (remove low-score memories), compaction (merge similar memories), integrity checks.

**File-backed memories are exempt from all of it** (`is_file_backed()`, 2026-07-27). A memory whose `source_type` is `markdown` is a projection of a file on disk: reproducible, curated, and never "accessed", so decay drove it under the 0.05 strength floor in ~26 days and the prune deleted it. Prune, merge, dedup and trust-resolution now skip these, and both decay queries in `store.rs` exclude them. Their lifecycle belongs to the indexer via `soft_delete_by_source`. Six tests cover the guards; note a just-created memory has `days < 0.001` and decay skips it before any guard runs, so a test here must age its rows or it proves nothing.

**MarkdownIndexer** (`indexers/markdown.rs`): watches `[brain.indexers.markdown] watch_dirs` and stores each `.md` file as heading-split chunks -- `trust_level: UserStated`, `importance: 0.8`, `scope: /memory/<stem>`, `source_type: markdown`. It writes through `store.add_memory`, so `quality::should_ingest` never runs on this path. Sections over `MAX_CHUNK_CHARS` (2000) become heading-prefixed continuation chunks rather than being truncated. **Watch the real directory, not a directory of symlinks** -- a file watcher gets no event when a symlink's target is written, and watching both the link and target directories is worse (each file indexes under one path and re-indexes under the other, while `soft_delete_by_source` only clears the path it is given).

### Proactive Engine (ironjaw-proactive)

Cron-based scheduler + workflow DAG executor.

- **Schedules**: cron expressions, enable/disable toggle, last_run tracking
- **Workflows**: multi-step DAGs with sequential/parallel execution
- **Store**: SQLite persistence for schedules and workflow state

`ProactiveEngine` runs a background loop checking schedules every `check_interval_secs`.

### Voice Pipeline (ironjaw-voice)

STT (speech-to-text) and TTS (text-to-speech) via llama-server.

- **STT**: Whisper model, audio buffer → text transcription
- **TTS**: Orpheus model, text → audio synthesis
- **VoiceSession**: per-connection voice state, inactivity timeout

Audio format: configurable sample rate (default 16000), max buffer 16MB.

### Vision / Multimodal (ironjaw-vision)

Image analysis via multimodal LLM (llava).

- `ImageProcessor`: validates image size, calls vision model
- `OcrTool`: registered as integration tool for text extraction
- Max image size: configurable (default 10MB)

### Multi-Device & Queue (ironjaw-ws)

**DeviceRegistry**: tracks connected devices by device_id. Supports multiple simultaneous connections.

**MessageQueue**: offline message delivery. When a device disconnects, queued messages are stored and delivered on reconnect.
- Configurable max queue size (default 1000)
- Message TTL (default 24 hours)

**Notifications**: push notification dispatch to registered devices.

### Circuit Breakers & Metrics (ironjaw-ws)

**CircuitBreakerRegistry**: per-service circuit breakers (5 failures → open, 60s reset).
Protects external service calls (LLM, integrations, SSH) from cascading failures.

**Metrics**: internal counters for:
- Requests per method
- Errors per method
- Active connections
- LLM token usage

### Skills System (ironjaw-ws/skills.rs)

`SkillRegistry` manages named skill definitions. Each `Skill` bundles a system prompt fragment, tool allowlist, and model preference. Skills can be activated per-session or per-request (e.g., "coding", "research", "minimal"). Dynamic loading allows runtime skill registration.

### Hook System (ironjaw-ws/hooks.rs)

`HookRegistry` provides lifecycle hooks at key points: pre-tool-execution, post-tool-execution, pre-message, post-message, session-start, session-end. Hooks can modify, block, or augment operations. Used for guardrails, logging, and custom behaviors.

### Participant Awareness (ironjaw-ws/participants.rs)

`ParticipantTracker` manages multi-user presence in sessions. Tracks who is connected, typing indicators, last-seen timestamps. Supports shared sessions where multiple devices or users collaborate.

### Channel Adapters (ironjaw-ws/channel.rs)

`ChannelAdapter` trait abstracts the transport layer. Built-in adapters: WebSocket (primary), REST (for mobile/CLI), SSE (for dashboard streaming). Handlers work against the adapter trait, not raw WebSocket.

### Task Pool / Concurrency (ironjaw-ws/concurrency.rs)

`TaskPool` provides bounded concurrent task execution. Prevents unbounded spawning of background tasks (tool executions, LLM calls, integrations). Configurable pool size with backpressure. Used by parallel tool execution and sub-agent delegation.

### Shell Streaming (ironjaw-ws/shell_stream.rs)

`ShellStream` provides real-time streaming of shell command output over WebSocket. Instead of waiting for command completion, output lines are streamed as `stream` frames. Supports PTY-like interaction for long-running commands.

### MCP Client & Server (ironjaw-mcp)

**McpServer** (`server.rs`): exposes 17 brain tools over MCP protocol. Two transports, one implementation — `handle_request()` is transport-agnostic:

- **stdio** (`--mcp-server` CLI mode) for local clients. Registered in Claude Code via `~/.claude.json` as the `ironjaw-brain` MCP server. Each client spawns its own process, with its own `brain.db` handle.
- **Streamable HTTP** (`POST /mcp` on the gateway, `ironjaw-ws/src/mcp_http.rs`) for remote clients. Shares the gateway's Brain, so no extra process and no extra DB handle. Bearer-token gated via `[server] mcp_token`.

`initialize` returns an `instructions` string that tells the model to call `brain_recall` before answering anything about the user, unprompted — that field is what makes the brain fire without being named. `tools/list` attaches `annotations`: the 9 tools in `READ_ONLY_TOOLS` advertise `readOnlyHint: true` so clients skip the confirmation prompt; everything absent from that list is advertised as destructive, so a new tool costs an extra prompt rather than an unprompted mutation.

**McpClient** (`client.rs`): connects to external MCP servers defined in `[[mcp.servers]]` config. Imported tools are registered in the ToolRegistry alongside built-in tools. Supports tool discovery, invocation, and resource access.

### Agent Communication Graph (ironjaw-ws/agent_graph.rs)

`AgentCommunicationGraph` manages inter-agent message routing. Agents (local or remote) register as nodes; edges define allowed communication paths. Supports broadcast, point-to-point, and topic-based routing. Foundation for multi-agent workflows.

### CLI Client (ironjaw-cli)

Standalone CLI binary for interacting with a running Ironjaw instance over WebSocket. Commands: `chat` (interactive REPL), `session` (list/create/switch/delete), `tools` (list/execute), `brain` (remember/recall/forget), `status` (infrastructure health). Connects to `ws://host:port/ws` with token auth.

### Bidirectional CC Session Sync (2026-03-25)

Dr. CLAW phone sessions sync bidirectionally with Claude Code desktop sessions:

- **cc.history.since**: Returns only new JSONL entries since a known count. Used by phone to poll for new CC conversation entries without re-fetching entire history.
- **cc.chat immediate ack**: `cc.chat` returns an immediate ack frame with the resolved `sessionId` before streaming begins, so the phone can start tracking the session immediately.
- **Reverse history**: `cc.history` supports `reverse: true` for newest-first pagination.
- **CWD lookup**: `SessionScanner` resolves session IDs to working directories for context.
- **Phone-pending markers**: `write_phone_pending_marker()` writes `~/.claude/phone-pending/<session_id>.json` on both success AND error/timeout, so the desktop CC session's phone-context hook always picks up the exchange.
- **activeInTerminal PID detection**: Detects if a CC session is actively running in a terminal.
- **SO_REUSEADDR**: Server socket bind uses SO_REUSEADDR to avoid "address in use" errors on restart.

### Sleep-Time Agents (ironjaw-brain/maintenance.rs)

5 LLM-powered background maintenance tasks that run during idle periods:

1. **Contradiction detection**: Finds memories that contradict each other, auto-closes older facts
2. **Insight synthesis**: Generates higher-order insights from related memories
3. **Relationship discovery**: Discovers implicit entity relationships across memories
4. **Temporal verification**: Validates temporal facts against current state
5. **Pattern detection**: Identifies recurring patterns across conversation history

### Dashboard REST API (ironjaw-ws/dashboard.rs)

Expanded REST API for the web dashboard. Endpoints for session management, metrics, configuration, brain state, schedule management, device listing, and system health. Serves embedded static files (HTML/JS/CSS) for the dashboard UI.

### Mobile API (ironjaw-ws/mobile_api.rs)

Mobile-optimized REST endpoints for the Dr. CLAW Android app. Compact JSON responses, pagination, push notification registration, offline sync support. Designed for bandwidth-constrained and high-latency mobile connections.

### Ops Manager (ironjaw-ws/ops.rs)

`OpsManager` tracks service health and lifecycle for all subsystems. Provides structured health checks (`/health` expanded), graceful shutdown coordination, subsystem restart capability. Feeds the dashboard and mobile status views.

### Plugin System (ironjaw-ws/plugins.rs)

`PluginHost` manages dynamically loaded plugins. Plugins can register tools, hooks, skills, and event handlers. Isolation via trait boundaries. Supports enable/disable at runtime. Current plugins load at startup from config.

### Conversation Forking (ironjaw-ws/forking.rs)

`ConversationForker` creates branch points in conversations. Fork a session at any message to explore alternative paths. Supports branch comparison and optional merge-back. Useful for "what-if" exploration.

### Sub-agent Delegation (ironjaw-ws/subagent.rs)

`SubAgentDelegator` spawns child agents for subtasks. Parent agent decomposes complex requests, delegates to specialized sub-agents (each with own skill/tool set), collects results. Bounded by TaskPool concurrency. Supports timeout and cancellation.

### A2A Protocol (ironjaw-ws/a2a.rs)

Agent-to-Agent (A2A) protocol implementation. Allows Ironjaw to communicate with other A2A-compatible agents over HTTP/WebSocket. Supports task delegation, result collection, and capability discovery. Follows the Google A2A specification.

### OpenAI-compatible REST API (ironjaw-ws/openai_compat.rs)

Drop-in OpenAI API compatibility layer. Exposes `/v1/chat/completions`, `/v1/models`, and `/v1/embeddings` endpoints. Allows any OpenAI-compatible client (Cursor, Continue, Open WebUI, etc.) to use Ironjaw as a backend. Enabled via `openai_compat = true` in `[server]` config.
