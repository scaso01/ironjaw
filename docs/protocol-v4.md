# Protocol v4 Reference

Extracted from CLAUDE.md on 2026-07-31 so it stops loading into every session. Read this when changing the wire protocol.


### Frame Types

| Type | Direction | Fields | Purpose |
|------|-----------|--------|---------|
| `req` | Client → Server | id, method, params | Request |
| `res` | Server → Client | id, ok, data?, error? | Response |
| `stream` | Server → Client | id, delta?, done?, usage? | Streaming tokens |
| `event` | Server → Client | event, data | Server-pushed events |
| `cmd` | Bidirectional | id, tool, params | Device commands |
| `cmd.res` | Bidirectional | id, ok, data?, error? | Command responses |
| `ui` | Server → Client | id, component, schema | Generative UI |
| `ui.res` | Client → Server | id, action, data? | UI interaction response |

### Event Types

| Event | Trigger | Data |
|-------|---------|------|
| `chat` | Agentic-loop turn streaming (`state`: delta/thinking/thinking_done/aborted/error/final); A3 adds `catchup` — sent once on reconnect to a still-live run, replaying the full accumulated text before live deltas resume | state, runId, seq, sessionKey, message |
| `permission.notify` | Notify-tier method executed | method, params |
| `permission.request` | Confirm-tier method pending | method, params, request_id, source? |
| `tool.start` | Agentic loop begins tool execution | tool_name, tool_call_id |
| `tool.result` | Agentic loop tool execution complete | tool_name, tool_call_id, ok, content, duration_ms |
| `voice.ready` | Voice session established | session_id |
| `voice.transcript` | STT result available | text, is_final |

### Complete Method Reference

| Method | Tier | Phase | Description |
|--------|------|-------|-------------|
| `auth.connect` | Auto | 1 | Authenticate + resume/create session |
| `ping` | Auto | 1 | Health check, returns uptime |
| `session.list` | Auto | 3 | List sessions with pagination |
| `session.get` | Auto | 3 | Get session details |
| `session.create` | Notify | 3 | Create new session |
| `session.switch` | Notify | 3 | Switch active session |
| `session.delete` | Confirm | 3 | Delete session (not active) |
| `session.rename` | Auto | 3 | Rename session |
| `chat.send` | Notify | 3 | Send message, stream response. Supports `ephemeral_model` (one-shot model override) and `force_fresh: true` (skip brain auto-inject for this turn) |
| `chat.history` | Auto | 3 | Paginated message history |
| `chat.abort` | Notify | 3 | Abort streaming response |
| `model.list` | Auto | 2 | List available LLM models |
| `model.status` | Auto | 2 | Current model info |
| `tools.list` | Auto | 5 | List registered tools |
| `tools.execute` | Notify | 5 | Execute tool by name |
| `brain.remember` | Notify | 6 | Store memory with embedding + auto entity extraction |
| `brain.recall` | Auto | 6 | Semantic search memories (detail: compact/full/deep) |
| `brain.forget` | Confirm | 6 | Soft-delete memory |
| `brain.import` | Confirm | 6 | Bulk import memories |
| `brain.timeline` | Auto | 6 | Point-in-time temporal memory query |
| `brain.summaries` | Auto | 6 | List session summary memories |
| `brain.context.set` | Notify | 6 | Store named context document |
| `brain.context.load` | Auto | 6 | Toggle context document in/out of LLM context |
| `brain.context.list` | Auto | 6 | List all context variables |
| `brain.context.delete` | Confirm | 6 | Remove context variable |
| `brain.context.peek` | Auto | 6 | Windowed slice of context document |
| `infra.status` | Auto | 7 | Infrastructure service status |
| `infra.metrics` | Auto | 12 | System metrics snapshot |
| `schedule.list` | Auto | 8 | List cron schedules |
| `schedule.create` | Confirm | 8 | Create new schedule |
| `schedule.delete` | Confirm | 8 | Delete schedule |
| `schedule.toggle` | Notify | 8 | Enable/disable schedule |
| `workflow.status` | Auto | 8 | Workflow execution status |
| `workflow.create` | Confirm | 8 | Create workflow DAG |
| `workflow.cancel` | Confirm | 8 | Cancel running workflow |
| `workflow.approve` | Notify | 8 | Approve workflow step |
| `voice.tts` | Auto | 9 | Text-to-speech synthesis |
| `voice.start` | Notify | 9 | Start voice session |
| `voice.stop` | Notify | 9 | Stop voice session |
| `device.list` | Auto | 11 | List connected devices |
| `device.cmd` | Confirm | 11 | Send command to device |
| `notify.send` | Notify | 11 | Push notification to device |
| `export.conversations` | Confirm | 12 | Export conversation history |
| `export.brain` | Confirm | 12 | Export brain memories |
| `backup.create` | Confirm | 12 | Create full backup |
| `cc.sessions` | Auto | CC | List all CC sessions (active + historical) |
| `cc.session.get` | Auto | CC | Get CC session metadata |
| `cc.history` | Auto | CC | Read paginated JSONL conversation history |
| `cc.chat` | Notify | CC | Send message to CC subprocess, immediate ack + stream response |
| `cc.chat.abort` | Notify | CC | Abort running CC subprocess |
| `cc.session.create` | Confirm | CC | Create new CC subprocess session |
| `cc.attach` | Auto | CC | Attach to running CC subprocess stream |
| `cc.detach` | Auto | CC | Detach from CC subprocess (process continues) |
| `cc.history.since` | Auto | CC | Return new JSONL entries since known count (bidirectional sync) |
| `cc.complete` | Auto | CC | Mark CC subprocess as complete |
| `chat.search` | Auto | 3 | Search messages across sessions |
| `session.pin` | Auto | 3 | Pin/unpin a session |
| `session.archive` | Auto | 3 | Archive/unarchive a session |
| `session.fork` | Confirm | 3 | Fork session at message point |
| `session.set_cwd` | Confirm | C1 | Set per-session working directory override; null/missing path clears the override. Tools resolve against this when set, else `current_dir()`. |
| `plugins.list` | Auto | 12 | List installed plugins |
| `plugins.toggle` | Notify | 12 | Enable/disable a plugin |
| `brain.consolidate` | Notify | 6 | Trigger memory consolidation/compaction |
| `brain.memories` | Auto | 6 | List raw memories (paginated) |
| `brain.pending.list` | Auto | 6 | List auto-extracted memories awaiting approval |
| `brain.pending.approve` | Notify | 6 | Approve pending memory → permanent with UserStated trust |
| `brain.pending.reject` | Notify | 6 | Reject pending memory (soft-delete) |
| `brain.reload` | Notify | 6 | Reload brain from disk |
| `brain.status` | Auto | 6 | Brain subsystem stats (memory count, graph size) |
| `brain.snapshot.create` | Confirm | D0 | Capture full memory + identity + context-variable state into a versioned blob |
| `brain.snapshot.list` | Auto | D0 | List available snapshots (id, label, captured_at, memory_count) |
| `brain.snapshot.restore` | MultiConfirm | D0 | Atomically swap the live memory set to a snapshot. Destructive. |
| `brain.snapshot.delete` | Confirm | D0 | Remove a snapshot |
| `brain.auto_learn.report` | Auto | D2 | Lifetime telemetry counters: `pending_routed`, `auto_approved`, `dropped` |
| `model.available` | Auto | 2 | Check if model is available |
| `model.llm` | Auto | 2 | Get current LLM provider info |
| `model.perf` | Auto | 2 | Model performance metrics |
| `model.session` | Auto | 2 | Get model session state |
| `model.switch` | Confirm | 2 | Switch active LLM model. Cloud models (openai/anthropic/openrouter prefix) switch instantly (`instant: true`), local models use llama-switch |
| `prompt.stats` | Auto | 2 | System prompt token cost breakdown by layer (identity, memories, context_vars, etc.) |
| `chat.inject` | Notify | 3 | Inject system message into session |
| `file.get` | Auto | 5 | Read file via REST (dashboard) |
| `gateway.restart` | Confirm | 12 | Restart Ironjaw gateway |
| `system.resources` | Auto | 12 | System resource usage (CPU, RAM, disk) |
| `tts.voices` | Auto | 9 | List available TTS voices |
| `tts.generate` / `tts.convert` | Auto | 9 | Generate TTS audio (alias pair) |
| `image.received` | Auto | 11 | Process received image |
| `project.status` | Auto | 12 | Project working directory status |
| `role.list` | Auto | 3 | List available roles |
| `role.switch` | Notify | 3 | Switch active role |
| `role.active` | Auto | 3 | Get active role |
| `role.preset.apply` | Notify | 3 | Apply role preset |
| `device.pair.request` | Confirm | 11 | Request device pairing |
| `device.pair.confirm` | Confirm | 11 | Confirm device pairing |
| `device.pair.revoke` | Confirm | 11 | Revoke device pairing |
| `session.resolved` | Auto | 3 | Mark session as resolved |
| `sessions.delete` | Confirm | CC | Delete CC session |
| `sessions.patch` | Notify | CC | Patch CC session metadata |
| `sessions.preview` | Auto | CC | Preview CC session content |
| `ccbridge.sessions` | Auto | CC | List CC Bridge sessions |
| `ccbridge.chat` | Notify | CC | Send message via CC Bridge |
| `ccbridge.sessions.rename` | Notify | CC | Rename CC Bridge session |
| `chronicle.enrich` | Notify | CC | Enrich chronicle session with metadata |
| `chronicle.insights` | Auto | CC | Get chronicle insights |
| `chronicle.search` | Auto | CC | Search chronicle sessions |
| `chronicle.session` | Auto | CC | Get single chronicle session |
| `chronicle.sessions` | Auto | CC | List chronicle sessions |
| `chronicle.stats` | Auto | CC | Chronicle statistics |
| `chronicle.timeline` | Auto | CC | Chronicle timeline view |
| `chronicle.topics` | Auto | CC | Chronicle topic listing |
| `command.sessions` | Auto | 12 | List command center sessions |
| `command.session.get` | Auto | 12 | Get command session detail |
| `command.session.action` | Confirm | 12 | Execute command center action |
| `command.usage.summary` | Auto | 12 | Command usage summary |
| `command.usage.by_role` | Auto | 12 | Command usage by role |
| `hooks.list` | Auto | B2 | List discovered-hook catalog (name, event, kind, enabled, skip_reason) |
| `hooks.reload` | MultiConfirm | B2 | Re-scan hooks dir, rebuild registry + catalog |

### Additional Endpoints (non-WebSocket)

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/v1/chat/completions` | POST | OpenAI-compatible chat completion (streaming + non-streaming) |
| `/v1/models` | GET | List available models (OpenAI format) |
| `/v1/embeddings` | POST | Generate embeddings (proxied to llama-server) |
| `/api/dashboard/*` | GET/POST | Dashboard REST API (sessions, metrics, config, brain) |
| `/api/brain/pending` | GET | List pending memories awaiting approval (query: limit, offset) |
| `/api/brain/pending/{id}/approve` | POST | Approve a pending memory → permanent UserStated memory |
| `/api/brain/pending/{id}/reject` | POST | Reject a pending memory (soft delete) |
| `/api/brain/browse` | GET | Paginated memory browser (scope_prefix, trust_level_min, type, sort) |
| `/api/mobile/*` | GET/POST | Mobile-optimized endpoints for Dr. CLAW app |
| `/mcp` | POST | Native MCP Streamable HTTP (JSON response mode). Requires `Authorization: Bearer <[server] mcp_token>` **unless the peer is loopback** — 127.0.0.1 is exempt so Claude Desktop, whose connector UI cannot send headers, can share the gateway's brain. Registered only when the token is set. `GET`/`DELETE` → 405 (no SSE stream, no sessions). |
| MCP stdio | -- | `--mcp-server` stdio mode for local clients (Claude Code) |
| A2A HTTP | POST | Agent-to-Agent task delegation and discovery |
