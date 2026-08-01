# Configuration Reference

Extracted from CLAUDE.md on 2026-07-31 so it stops loading into every session. Read this when changing config or env vars.


All config in `config/ironjaw.toml`. Sections marked (opt-in) default to disabled.

### [server]

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | String | -- | Bind address |
| `port` | u16 | -- | Bind port |

### [auth]

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `token` | String | -- | Client auth token |

### [audit]

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `database_path` | String | -- | SQLite file path |

### [logging]

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `level` | String | -- | Log level (trace/debug/info/warn/error) |
| `format` | String | `"pretty"` | Log format (pretty/json/compact) |

### [session]

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `database_path` | String | `"data/sessions.db"` | SQLite file |
| `context_token_limit` | u64 | `32768` | Max context tokens per session |

### [security]

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `hitl_timeout_secs` | u64 | `30` | HITL approval timeout |
| `rate_limit_enabled` | bool | `false` | Enable rate limiter |
| `sanitize_enabled` | bool | `true` | Enable input sanitizer |

### [tools]

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `working_dir` | String | `"."` | Tier 2 of `resolve_session_cwd` chain (per-session override → this → process cwd). Wired at startup (gateway restart to change). `"."` means "use process cwd" — equivalent to falling through to tier 3. |
| `shell_timeout_secs` | u64 | `30` | Shell command timeout |

### [brain] (opt-in)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable brain subsystem |
| `database_path` | String | `"data/brain.db"` | SQLite file |
| `embedding_url` | String | `"http://localhost:8080"` | llama-server URL |
| `embedding_model` | String | `"nomic-embed-text"` | Embedding model |
| `auto_recall_top_k` | usize | `3` | Memories per chat.send |
| `max_memories` | usize | `10000` | LRU eviction threshold |
| `stable_prefix` | bool | `false` | Keep system prompt static for KV cache reuse (dynamic context moves to User role) |

### [integrations] (opt-in)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable integration tools |
| `searxng_url` | String | `"http://192.0.2.20:8080"` | SearXNG endpoint |
| `perplexica_url` | String | `"http://192.0.2.20:3001"` | Perplexica endpoint |
| `cc_bridge_url` | String | `"http://127.0.0.1:18790"` | CC Bridge endpoint |
| `cc_chronicle_url` | String | `"http://127.0.0.1:18793"` | CC Chronicle endpoint |
| `remote_host` | String | `"192.0.2.20"` | docker-host SSH host |
| `remote_user` | String | `"deploy"` | docker-host SSH user |
| `secrets_database_path` | String | `"data/secrets.db"` | Secrets vault path |
| `ha_url` | Option | `None` | Home Assistant URL |
| `ha_token` | Option | `None` | Home Assistant token |

### [proactive] (opt-in)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable proactive engine |
| `database_path` | String | `"data/proactive.db"` | SQLite file |
| `check_interval_secs` | u64 | `60` | Schedule check interval |
| `max_concurrent_workflows` | usize | `3` | Max parallel workflows |

### [voice] (opt-in)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable voice pipeline |
| `stt_url` | String | `"http://localhost:8080"` | STT endpoint |
| `stt_model` | String | `"whisper-large-v3-turbo"` | STT model |
| `tts_url` | String | `"http://localhost:8080"` | TTS endpoint |
| `tts_model` | String | `"orpheus"` | TTS model |
| `tts_voice` | String | `"tara"` | TTS voice |
| `sample_rate` | u32 | `16000` | Audio sample rate |
| `max_audio_buffer_bytes` | usize | `16777216` | Max audio buffer (16MB) |
| `inactivity_timeout_secs` | u64 | `30` | Voice session timeout |

### [vision] (opt-in)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable vision processor |
| `max_image_size_bytes` | usize | `10485760` | Max image size (10MB) |

### [llm]

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `default_model` | Option | `None` | Default model name |
| `providers` | Array | `[]` | Provider configurations |

### [[llm.providers]]

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | String | -- | Provider identifier (used for prefix routing, e.g. "openrouter") |
| `type` | String | -- | Provider type (llama-server/openai/anthropic) |
| `base_url` | String | -- | API base URL |
| `api_key` | Option | `None` | API key (direct value) |
| `api_key_env` | Option | `None` | Env var name to read API key from (takes precedence) |
| `timeout_secs` | u64 | `300` | Request timeout |
| `extra_headers` | Map | `{}` | Extra HTTP headers injected on all requests (e.g. OpenRouter's HTTP-Referer) |

### [llm.routing] (opt-in)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable complexity-based routing |
| `simple_model` | Option | `None` | Model for simple queries |
| `complex_model` | Option | `None` | Model for complex queries |
| `complexity_threshold` | f32 | `0.5` | Score threshold (0-1) |

### [[mcp.servers]] (opt-in)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | String | -- | Server identifier |
| `transport` | String | `"stdio"` | Transport type (stdio/http) |
| `command` | Option | `None` | Command to launch (stdio transport) |
| `args` | Array | `[]` | Command arguments |
| `url` | Option | `None` | Server URL (http transport) |

### [server] (additional fields)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `openai_compat` | bool | `false` | Enable OpenAI-compatible /v1/* endpoints |
| `auth_timeout_secs` | u64 | `30` | Seconds to wait for the initial `auth.connect` frame before closing the connection |
| `mcp_token` | String | `""` | Bearer token for the native MCP endpoint (`POST /mcp`). Empty leaves the route unregistered — opt-in and fail-closed, since the gateway binds `0.0.0.0` and the brain has destructive tools. Distinct from `[auth] token` on purpose. |

### [claude]

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable CC subprocess management |
| `claude_cmd` | String | `"claude"` | Path to claude CLI |
| `default_cwd` | String | `"."` | Default working directory for new sessions |
| `sessions_dir` | String | `"~/.claude/projects"` | JSONL session file directory |
| `timeout_ms` | u64 | `120000` | Global subprocess timeout |
| `idle_timeout_ms` | u64 | `120000` | Idle timeout (no output) |
| `max_concurrent` | u32 | `3` | Max concurrent CC subprocesses |

### [chat.agentic] (opt-in, Phase 18)

Killswitch-gated iterative tool-call loop. When enabled, `chat.send` enters an agentic loop that executes tool calls returned by the LLM, feeds results back, and re-prompts until the LLM produces a text-only turn or `max_iterations` is reached.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master killswitch for the agentic loop |
| `max_iterations` | usize | `5` | Max LLM re-prompts per `chat.send` turn |
| `tool_timeout_secs` | u64 | `30` | Per-tool execution timeout |
| `hitl_session_cache_enabled` | bool | `true` | Cache session-scoped HITL approvals (skip re-prompt for same tool+session) |
| `tool_memory_writeback_enabled` | bool | `true` | Write `ToolVerified` memories to Brain after tool execution |
| `run_token_ceiling` | u64 | `0` | Hard cap on cumulative tokens per `chat.send` turn; 0 = off. On trip, loop stops cleanly and forced synthesis answers with what was gathered |
| `tool_result_max_chars` | usize | `12288` | Per-tool-result char cap fed back into context (and persisted); 0 = off |
| `min_answer_chars` | usize | `20` | G2: final answers shorter than this (trimmed) get ONE re-prompt; second answer accepted as-is; 0 = off |
| `turn_deadline_secs` | u64 | `360` | Per-turn wall-clock deadline; on expiry the loop breaks to forced synthesis so a turn always ends within the bound; 0 = off |

### [brain.live_state] (opt-in, Phase 18)

Live-state freshness classifier. Detects tool results that represent live/volatile state (e.g., current weather, live metrics) and optionally skips brain auto-inject for those results to prevent stale memory contamination.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable live-state freshness classification |
| `skip_on_live_state` | bool | `false` | Skip brain recall when query targets live state |
| `freshness_window_secs` | u64 | `300` | Window within which a memory is considered fresh |
| `confidence_threshold` | f32 | `0.7` | Minimum classifier confidence to act on classification |
| `llm_fallback` | bool | `false` | Use LLM for classification when heuristics are insufficient |
| `decay_rate` | f32 | `0.01` | Per-second staleness decay rate applied to live memories |

### [hooks] (opt-in, B2)

Lifecycle hook framework. WASM hooks are the default sandboxed-in-process path (memory + fuel capped via `WasmSandboxConfig`); subprocess hooks require a separate opt-in plus a per-event allowlist. Discovery scans `dir` at startup and on `hooks.reload`. Hooks register against four new event variants beyond the original eight: `PreCompact`, `UserPromptSubmit`, `SkillActivated`, `SkillDeactivated`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master killswitch — when false, no discovery, no registration |
| `dir` | String | `"~/.config/ironjaw/hooks"` | Hook discovery directory; resolved against `$HOME`/`%USERPROFILE%` |

### [hooks.subprocess] (opt-in, B2)

Subprocess hook gating. Both `enabled = true` AND a non-empty `enabled_events` allowlist are required for any subprocess hook to fire. Per-hook timeout is clamped to `timeout_ceiling_secs`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Allow subprocess hooks at all |
| `default_timeout_secs` | u64 | `5` | Per-hook timeout when manifest does not override |
| `timeout_ceiling_secs` | u64 | `60` | Hard upper bound on per-hook timeout |
| `enabled_events` | Vec\<String\> | `[]` | Allowlist of event names (e.g. `["SessionStart"]`); empty = subprocess unusable |
