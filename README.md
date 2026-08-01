# Ironjaw

Ironjaw -- Rust WebSocket gateway for the Dr. CLAW AI assistant platform. Replaces OpenClaw Gateway, CC Bridge, CC Chronicle, and all other Node.js services with a single binary.

- **Language:** Rust 1.93.1 (MSVC toolchain)
- **Framework:** axum 0.8 + tokio
- **Tests:** unit and integration suites across all 16 crates, plus dashboard E2E via headless Chrome
- **Protocol:** v4 with 8 frame types and ~166 WebSocket methods
- **License:** AGPL-3.0-or-later

## Features

- **Protocol v4 WebSocket** -- 8 frame types (req, res, stream, event, cmd, cmd.res, ui, ui.res) with ~166 methods
- **LLM router** -- Streaming via llama-server + OpenAI providers, complexity-based routing, retry/failover/cache decorators, per-model token accounting
- **Persistent sessions** -- SQLite WAL, CRUD, cursor-based pagination, context compaction, session repair
- **4-tier permission system** -- Auto/Notify/Confirm/MultiConfirm with HITL gates
- **Tool engine** -- 10 built-in tools + 15 integration tools, parallel fan-out execution
- **RAG memory (Brain v2)** -- 3-layer context (identity, working, long-term), hybrid search (vector + FTS5 + RRF fusion), knowledge graph, temporal facts, salience decay, sleep-time maintenance agents
- **Proactive engine** -- Cron scheduler + workflow DAG executor
- **Voice pipeline** -- STT via Whisper, TTS via Orpheus (llama-server)
- **Vision/multimodal** -- Image analysis via multimodal LLM, OCR tool
- **Multi-device** -- Device registry, offline message queue, push notifications
- **Claude Code integration** -- CC subprocess management, bidirectional session sync, JSONL history parsing
- **MCP client + server** -- Expose 17 brain tools over MCP, via stdio or a native Streamable HTTP endpoint on the gateway; consume external MCP servers
- **OpenAI-compatible API** -- Drop-in `/v1/chat/completions`, `/v1/models`, `/v1/embeddings` endpoints
- **A2A protocol** -- Agent-to-Agent communication following the Google A2A specification
- **Skills, hooks, plugins** -- Dynamic skill loading, lifecycle hooks, runtime plugin system
- **Agentic tool execution (Phase 18)** -- Iterative tool-call loop for `chat.send`: executes LLM tool calls, feeds results back, re-prompts until a text-only turn; HITL session-cache, `ToolVerified` Brain writeback, `tool.start`/`tool.result` events, mid-stream reconnect `catchup`
- **Agent infrastructure** -- Sub-agent delegation, conversation forking, inter-agent routing graph
- **Dashboard + mobile API** -- REST API for web dashboard and Dr. CLAW Android app
- **Circuit breakers and metrics** -- Per-service circuit breakers, internal counters
- **Append-only audit trail** -- SQLite audit log for every method call

## Quick Start

Prerequisites: Rust 1.93.1+ (MSVC toolchain), llama-server (optional)

```bash
cargo build
cargo run -- config/ironjaw.toml     # WebSocket gateway on :18794
cargo run -- --mcp-server config/ironjaw.toml  # MCP server mode (stdio)
cargo test                            # lib tests across all 16 crates
```

## Architecture

| Crate | Purpose |
|---|---|
| `ironjaw-core` | Binary entry point, TOML config (13 sections), hot-reload watcher |
| `ironjaw-ws` | axum WebSocket server, Protocol v4, ~166 method handlers, dashboard, mobile API, ops, plugins, skills, hooks, agent graph, A2A, OpenAI-compat |
| `ironjaw-security` | 4-tier permissions, HITL gates, rate limiting, input sanitization, output scrubbing, SSRF protection, taint tracking, loop guard, leak detection, chain alerting |
| `ironjaw-audit` | Append-only SQLite audit trail |
| `ironjaw-llm` | LLM provider abstraction (llama-server, OpenAI, Mock), complexity routing, retry/failover/cache decorators, token accounting, JSON repair |
| `ironjaw-session` | Persistent session + message storage (SQLite WAL), context compaction, session repair |
| `ironjaw-tools` | Tool execution engine with trait-based registry, 10 built-in tools, parallel executor |
| `ironjaw-brain` | RAG memory v2 with hybrid search (vector + FTS5 + RRF), knowledge graph, temporal facts, salience decay, sleep-time maintenance agents |
| `ironjaw-integrations` | 15 integration tools (search, CC, infra, Docker, ADB, git, HA, secrets) |
| `ironjaw-proactive` | Cron scheduler + workflow DAG engine |
| `ironjaw-voice` | Voice pipeline (STT/TTS via llama-server) |
| `ironjaw-vision` | Image processing, OCR tool, multimodal LLM analysis |
| `ironjaw-mcp` | MCP client + server (expose brain tools, consume external MCP servers) |
| `ironjaw-cli` | CLI client (interactive REPL, session/tool/brain/status commands) |
| `ironjaw-claude` | CC subprocess management, JSONL history parsing, bidirectional session sync |

## Protocol v4

### Frame Types

| Frame | Direction | Description |
|---|---|---|
| `req` | Client -> Server | Client method request |
| `res` | Server -> Client | Method response |
| `stream` | Server -> Client | Streaming token output |
| `event` | Server -> Client | Server-pushed event (permission.notify, permission.request, tool.start, tool.result) |
| `cmd` | Bidirectional | Device command |
| `cmd.res` | Bidirectional | Command response / HITL approval or denial |
| `ui` | Bidirectional | UI state update |
| `ui.res` | Bidirectional | UI response |

### Methods (~166 total)

| Namespace | Methods | Examples |
|---|---|---|
| `auth.*` | 1 | `auth.connect` -- authenticate + resume/create session |
| `ping` | 1 | Liveness check with uptime |
| `chat.*` | 4 | `send`, `history`, `abort`, `search`, `inject` |
| `session.*` | 8 | `list`, `get`, `create`, `switch`, `delete`, `rename`, `pin`, `archive`, `fork`, `resolved` |
| `model.*` | 6 | `list`, `status`, `available`, `llm`, `perf`, `session`, `switch` |
| `tools.*` | 2 | `list`, `execute` |
| `brain.*` | 12 | `remember`, `recall`, `forget`, `import`, `timeline`, `summaries`, `context.*` (set/load/list/delete/peek), `consolidate`, `memories`, `reload`, `status` |
| `schedule.*` | 3 | `list`, `create`, `delete`, `toggle` |
| `workflow.*` | 3 | `status`, `create`, `cancel`, `approve` |
| `voice.*` / `tts.*` | 5 | `start`, `stop`, `tts`, `voices`, `generate`/`convert` |
| `device.*` | 5 | `list`, `cmd`, `pair.request`, `pair.confirm`, `pair.revoke` |
| `notify.*` | 1 | `send` -- push notification to device |
| `infra.*` | 2 | `status`, `metrics` |
| `export.*` / `backup.*` | 3 | `conversations`, `brain`, `backup.create` |
| `cc.*` | 10 | `sessions`, `session.get`, `history`, `chat`, `chat.abort`, `session.create`, `attach`, `detach`, `history.since`, `complete` |
| `ccbridge.*` | 3 | `sessions`, `chat`, `sessions.rename` |
| `chronicle.*` | 7 | `enrich`, `insights`, `search`, `session`, `sessions`, `stats`, `timeline`, `topics` |
| `command.*` | 4 | `sessions`, `session.get`, `session.action`, `usage.*` |
| `plugins.*` | 2 | `list`, `toggle` |
| `role.*` | 4 | `list`, `switch`, `active`, `preset.apply` |
| Other | 5 | `file.get`, `gateway.restart`, `system.resources`, `project.status`, `image.received` |

See [CLAUDE.md](CLAUDE.md) for the complete method reference with tier assignments and descriptions.

## Configuration

`config/ironjaw.toml` supports 13 sections:

```toml
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "your-secret-token"

[audit]
database_path = "data/audit.db"

[logging]
level = "info"
format = "pretty"  # or "json"

[llm]
# Provider selection: llama-server or openai
default_provider = "llama-server"

[session]
database_path = "data/sessions.db"
context_token_limit = 32768

[security]
rate_limit_enabled = false
sanitize_enabled = true
hitl_timeout_secs = 30

[tools]
working_dir = "."
shell_timeout_secs = 30

[brain]
enabled = false
database_path = "data/brain.db"
embedding_url = "http://localhost:8080"
embedding_model = "nomic-embed-text"
auto_recall_top_k = 3
max_memories = 10000

[integrations]
# Search, CC Bridge, CC Chronicle, git, Home Assistant, secrets vault

[proactive]
# Cron schedules and workflow DAG definitions

[voice]
# STT (Whisper) and TTS (Orpheus) endpoint configuration

[vision]
# Image size limits, accepted formats, base64 encoding settings
```

## Endpoints

| Endpoint | Method | Description |
|---|---|---|
| `/health` | GET | Health check (JSON) with uptime |
| `/ws` | WebSocket | Protocol v4 WebSocket (primary interface) |
| `/v1/chat/completions` | POST | OpenAI-compatible chat completion (streaming + non-streaming) |
| `/v1/models` | GET | List available models (OpenAI format) |
| `/v1/embeddings` | POST | Generate embeddings (proxied to llama-server) |
| `/api/dashboard/*` | GET/POST | Dashboard REST API (sessions, metrics, config, brain) |
| `/api/brain/pending` | GET | List pending memories awaiting approval |
| `/api/brain/pending/{id}/approve` | POST | Approve a pending memory -> permanent UserStated memory |
| `/api/brain/pending/{id}/reject` | POST | Reject a pending memory (soft delete) |
| `/api/brain/browse` | GET | Paginated memory browser (scope_prefix, trust_level_min, type, sort) |
| `/api/mobile/*` | GET/POST | Mobile-optimized endpoints for Dr. CLAW app |
| `/mcp` | POST | Native MCP Streamable HTTP -- 17 brain tools. Bearer token (`[server] mcp_token`) required except from loopback; unregistered when the token is unset |
| MCP stdio | -- | `--mcp-server` mode, same 17 tools, for local clients |
| A2A HTTP | POST | Agent-to-Agent task delegation and discovery |

## Documentation

| File | Description |
|---|---|
| `CLAUDE.md` | Developer reference: build, test, architecture, all method signatures |
| `SETUP.md` | Installation and first-run configuration guide |
| `CHANGELOG.md` | Version history and release notes |
| `REQUIREMENTS.md` | Full feature requirements for all 13 phases |
| `HANDOFF.md` | Session handoff notes and phase completion status |
| `docs/AUDIT_BASELINE.md` | Audit trail schema, event types, and compliance notes |

## Port Allocation

| Port | Service |
|---|---|
| 18794 | Ironjaw (development) |
| 18789 | OpenClaw Gateway (retired -- superseded by Ironjaw) |
| 18790 | CC Bridge (legacy) |
| 18793 | CC Chronicle (legacy) |
| 8080 | llama-server |
| 443 | Caddy (TLS termination) |
