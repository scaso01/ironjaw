# Changelog

All notable changes to Ironjaw are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added

**Native MCP over Streamable HTTP — `POST /mcp` (2026-07-29)**

The gateway now serves the Model Context Protocol itself. Remote MCP clients
(the claude.ai and Gemini Spark connectors, via the Oracle jh-exit-2 front door)
reach the brain directly instead of going through a supergateway stdio→HTTP bridge.

Why: supergateway spawned one `ironjaw --mcp-server` child per MCP session
(~550 MB each) and never reaped it. Thirteen sessions meant thirteen resident
brains, 7.1 GB, and thirteen handles on `brain.db` — at which point FTS init
started failing with "database is locked" and `brain_recall` silently degraded to
vector-only. Dropping `--stateful` was measurably worse (one child per *request*).
Serving MCP in-process removes the middleman: one process, one brain, one DB handle.

- `crates/ironjaw-ws/src/mcp_http.rs` — JSON response mode of the Streamable HTTP
  transport. `POST` returns the JSON-RPC reply (202, no body, for notifications);
  `GET`/`DELETE` return 405, which the spec permits for a server with no
  server-initiated messages and no session state.
- `[server] mcp_token` — bearer token required on `/mcp`. Empty (the default)
  leaves the route unregistered, so the endpoint is opt-in and fail-closed; the
  gateway binds `0.0.0.0` and the brain exposes destructive tools. Deliberately
  **not** `[auth] token`, so the public front door can carry it without also
  handing out gateway access.
- `AppState.mcp_http` carries the server plus token; `main.rs` binds the tool
  handler to the gateway's own `Brain`.

Verified end-to-end: 401 without the token, 17 tools listed (9 read-only), and
`brain_recall` returning real memories through Caddy → reverse SSH tunnel → `/mcp`.

**Loopback callers skip the token (2026-07-29)**

`/mcp` now serves 127.0.0.1 without an `Authorization` header, so local tooling
(health checks, scripts, curl) can reach the brain without handling the secret.

It was built to get Claude Desktop off its own private `--mcp-server` brain, and
**that did not work**. Desktop's custom connectors require an OAuth sign-in
service, not a bearer header: it rejects `http://` outright, accepts an https URL,
then fails at "Couldn't register with <name>'s sign-in service" because the server
advertises no OAuth metadata and no registration endpoint. Supporting it means
building discovery + dynamic client registration + authorize + token into Ironjaw —
a few hundred lines whose entire payoff is ~1 GB of RAM while the Desktop app is
open. Judged not worth it; Desktop stays on stdio. Do not re-derive this without
testing against a real Desktop build.

Not a downgrade: any process on this machine can already launch that same brain
with no credential at all, so local-means-trusted is the level the brain already
sat at. Verified: loopback without a token → 200 with 17 tools, workstation's LAN
address without a token → 401, LAN address with the token → 200.

Consequence worth knowing: the public front door arrives through a reverse SSH
tunnel that lands on 127.0.0.1, so remote traffic is exempt too. That path is
gated by its secret URL and an IP allowlist in jh-exit-2's Caddy — which is what
it was always gated by; the token is a LAN guard, since the gateway binds
`0.0.0.0`. Pointing the tunnel at workstation's LAN address would re-arm the token for
remote callers, at the cost of making the brain depend on that NIC being up.

`serve()` now uses `into_make_service_with_connect_info` so peer addresses reach
the handler. `mcp_post` reads `ConnectInfo` off the request rather than taking an
`Option<ConnectInfo<_>>` extractor — axum 0.8 has no optional extractor for it,
and a required one would 500 on any router served without connect info. A missing
peer address counts as remote, so the check fails closed.

### Fixed

**Agentic context meter counted cumulative spend as live context (2026-07-23)**

`run_agentic_chat` accumulated `prompt_tokens` across loop iterations (each re-prompt
re-counts the whole system + 83-tool payload + replayed history) and reported that sum
as the session's context occupancy — a 52-char question that ran a 14-iteration tool
loop showed ~104K "context used"; one session read 205K on a 131K model window.
The inflated sum also fed the 65K `should_compact` trigger, so chats looked nearly
full at ~7% real occupancy. New `last_call_tokens` (last LLM call's prompt +
completion = true occupancy, since each prompt subsumes all prior turns) now drives
`contextTokens`/`contextPct` on all terminal chat events, `update_context_tokens`,
and the compaction check. Cumulative `tokens_in` is retained where cumulative is
correct: `run_token_ceiling` spend guard, `inputTokens` in the final event, and
persisted per-turn usage. Stale inflated `sessions.context_tokens` rows self-heal on
each session's next turn. Single-pass (non-agentic) path was already last-value and
is unchanged.

**Dr. CLAW local-model path hardening (2026-07-18 → 2026-07-19)**

The local (llama-server `qwen3.6:35b-a3b`) path the phone actually uses was diverging
from the intended Claude-Code-like behavior. Root-caused with a 5-agent code trace + an
A/B probe on the exact bare-model client path, fixed in four deployed phases.

- **#0 local-model detection was prefix-gated** (`is_local_model = model.starts_with("llama-server/")`). The phone resolves its model to the BARE `qwen3.6:35b-a3b` via `router.default_model()`, so the behavioral base prompt + curated tool set never fired on the real path. Now gated on the resolved provider (`LlmRouter::is_llama_server_model`). Verified live: bare model uses only the curated tool set.
- **#1 brain SQLite** now opens with WAL + 5s `busy_timeout` + `synchronous=Normal` (was rollback journal + 0ms busy → up to 41s stalls that blew the 3s brain-inject budget, dropping brain context from turns).
- **#2 agentic-loop hang / missing terminal event**: 120s timeout + `kill_on_drop` on the llama-switch subprocess; a per-turn wall-clock deadline (`turn_deadline_secs`, default 360); and a guaranteed terminal frame so a run can never sit mid-stream forever.
- **#3 raw-JSON tool results**: infra-tool `{"output": …}` blobs are flattened to their dominant text field before feeding the small model (was dumping the whole escaped-JSON blob).
- **#4 tool EXECUTION gated to the advertised set**: a registered-but-not-advertised tool call is blocked with a synthetic error the model self-corrects on, instead of silently reaching a tool the curated set was meant to hide (the curated set was advertising-only). Unknown names still fall through to the not-found path.
- **#5 memory-flush extraction** no longer loses facts under load: flush LLM timeout 10s→60s, extraction input capped to the last 40 messages, and `generate_session_summary` bounded by a 60s timeout (was unbounded). Memory-extraction path only — live context unaffected.
- **#6 tool-memory writeback** derives a short plain-text digest when a tool sets no `MemoryHint.content`, instead of dumping raw serde JSON the ingestion quality filter always rejected as a >500-char JSON blob.
- **#7 nudge flail → forced synthesis**: `needs_nudge` reports `MaxRetriesExceeded` only for a still-conversational non-answer (not merely a spent retry budget); the loop then breaks to the tools-removed synthesis pass instead of accepting prose or burning iterations.
- **#8 dead-socket detection**: a writer-task sink failure cancels a `socket_dead` token folded into the drain-mode selects, so a truly-gone client flips to drain/buffer at once (skipping the forced-synthesis LLM call) instead of waiting minutes for read-side Close/None on Windows. A3 catchup still covers reconnect.
- **Log hygiene**: `mdns_sd` capped at warn (benign LAN noise); benign ingestion-filter/dedup writeback rejections log at debug while real brain I/O failures stay at warn; one `info!` per completed agentic turn (run_id + stopReason + answer length) for forensics.

### Added

**Brain write-gate upsert fix (2026-07-02)**
- `brain.remember` / `remember_full` with an explicit `supersedes` id now bypasses the novelty gate entirely — a caller-directed upsert is expected to be near-identical to the entry it replaces, and the similarity check was vetoing legitimate updates (the memory-file sync bug: 23 of 113 curated files could never propagate edits).
- `GateResult::Skip` now carries the id of the blocking memory (`Skip(Option<i64>)`); the rejection error reads `... too similar to existing (id=N)` so clients can recover by superseding exactly that entry.
- Tag-hinted supersession (`detect_supersession`) is checked BEFORE the low-novelty rejection instead of only in the 0.15–0.40 novelty band — an update carrying a supersedes hint no longer loses to its own similarity.
- `WriteGate::compute_novelty` additionally returns the nearest memory id.
- 3 new tests (blocking-id skip, tag-supersession-beats-similarity, explicit-supersedes upsert). Client fix in dotfiles `scripts/memory_to_brain.py`: `supersedes` must be a JSON number — the handler's `as_i64()` silently dropped the string form the script had always sent.

**Phone reachability + agentic dead-answer hardening (2026-06-26)**
- `adb` tool now passes `-s <serial>` on every command, defaulting to the first network-attached device when no serial is given; an explicit `serial` param overrides. Fixes multi-device ambiguity. Commit `6b46cdf`.
- rish-over-ssh fallback: when adb can't reach the phone (wireless-debug port rotates/turns off) and the action is `shell`, the command runs on-device via Termux ssh (fixed port 8022) → Shizuku `rish` (uid 2000). Result JSON carries `"via": "rish-over-ssh"`. Commit `6b46cdf`.
- Agentic-loop dead-answer fixes for abliterated Qwen3.6 — three failure modes that all ended a tool turn with no usable answer:
  - Blank final turn: re-prompt once ("answer now in plain text, no more tools"), then fall back to the last tool output. Commit `7ff0588`.
  - Announce-but-don't-act: ToolNudger conversational markers extended with action verbs ("Let me read/get/pull/run/…"), 2 regression tests. Commit `7ff0588`.
  - Announce-loop until max_iterations: forced synthesis pass — tools stripped from the request, one final LLM call to answer in plain text; `strip_think_tags` promoted to `pub(crate)`. Commit `d5b59d8`.
- Live-verified end-to-end through the running gateway: phone-specs prompt returns a real answer (via rish) with the phone absent from adb.

**B2 live smokes verified (2026-05-04, follow-up)**
- Smoke 4 (SessionStart subprocess hook), Smoke 5 (UserPromptSubmit Block), and Smoke 6 (`hooks.reload` pickup) executed end-to-end against a live gateway via `samples/hooks/run_smokes.py`. All three PASS.
- Two B2 plumbing gaps were discovered and fixed during the run: (a) `SubprocessHook::new` rejected `marker.cmd`-style files because discovery passes the stem and the constructor required an exact match — fixed in commit `626796a` with extension fallback. (b) The `SessionStart` `HookEvent` variant existed but was never emitted in production code — fixed in commit `9e55e39` (emit fires post-auth, before the message loop).

**C1 tier-3 — Config fallback in `resolve_session_cwd` (2026-05-04, follow-up)**
- New `tools_working_dir: Arc<RwLock<Option<PathBuf>>>` flat field on `AppState`. Wired at startup by `main.rs` from `config.tools.working_dir`; ignored when value is the default `"."` or path is missing.
- `resolve_session_cwd` now follows the full 3-tier chain documented in C1: session override → config default → `current_dir()`.
- Stale config (path no longer exists) logs a warning and falls through to `current_dir()`.
- 3 unit tests in `handler::resolve_session_cwd_tests` covering all three tiers.
- Hot-reload remains unwired in production (consistent with other config fields); gateway restart applies changes.

**Wave C + Wave D — Roadmap closure (2026-05-04)**

Ships C1, C3, C4, C5, C6, C7, C8, D0, D1 in addition to C2 and D2 below. Closes the `rippling-squishing-teapot` parent plan to code-complete; D2's 7-day observation tail starts now.

- **C1** — Per-session working directory: `Session.working_dir: Option<String>`, idempotent `ALTER TABLE` migration in `ironjaw-session`, new `session.set_cwd` RPC (Confirm tier), `resolve_session_cwd()` helper threads override into both `ToolContext` construction sites in `handler.rs`. Config-tier fallback (3rd resolution tier) shipped 2026-05-04 in commit `193c724` — full 3-tier chain (session → `[tools] working_dir` → `current_dir()`) is now live.
- **C3** — Static layer ingestion: new `ironjaw-ws/src/static_layer.rs` module discovers `<cwd>/CLAUDE.md`, `<cwd>/.claude/CLAUDE.md`, walks parents until `.git/`, and includes `~/.claude/CLAUDE.md`. Content-hash dedup handles symlinks. Injected as `SystemPolicy` (5) trust messages in handler prompt assembly.
- **C4** — Token budget governor: `tiktoken-rs` workspace dep + new `ironjaw-llm/src/tokenizer.rs`. `static_layer/cache.rs` keyed by `(path, mtime)`. Per-file 4K token cap with head-keep + truncation marker; aggregate cap `min(12K, model.context_window * 0.4)`. Beat-down order on overflow: deepest leaves → root → ~/.claude. Telemetry counter `static_layer_tokens` via A14 extensible counter API.
- **C5** — Layered prompt assembly: refactored `handler.rs` message construction into `crates/ironjaw-ws/src/prompt_builder.rs`. Stages: Identity → StaticLayer → Dynamic (brain auto-inject) → ConversationHistory. Per-layer token counts visible in telemetry. No schema change.
- **C6** — `/init` slash command: writes a starter `CLAUDE.md` into `<cwd>` (resolved via per-session override) if absent. Confirm tier. Registered in the existing B1 `SlashCommandRegistry`.
- **C7 (drclaw)** — Project picker UI: `sessionSetCwd(sessionKey, path)` added to `data/websocket/GatewayClient.kt`. `SessionDrawer.kt` exposes "Switch project" entry point. PermissionDialog reuses B1's `consequences` parameter to display project-switch impact.
- **C8 (drclaw)** — Subagent dispatch UI: dispatch affordance ("+" button) added to subagent section. Subagent results stream as tool-result frames consumed by existing `ToolResultCard`.
- **D0** — Brain snapshot/restore infrastructure: greenfield `crates/ironjaw-brain/src/snapshot.rs`, new `brain_snapshots` table, JSON-serialized payload spanning all 27 Memory fields + identity KV + context variables. New RPCs: `brain.snapshot.create` (Confirm), `brain.snapshot.list` (Auto), `brain.snapshot.restore` (MultiConfirm — destructive), `brain.snapshot.delete` (Confirm). Restore atomically swaps the live memory set in a transaction.
- **D1** — Trust ranking verification: deterministic tests in `ironjaw-brain` assert UserStated > ModelInferred ordering for tied semantic+recency scores. New `explain: true` flag on `brain.recall` returns per-component breakdown (`trust_level`, `trust_score`, `composite_score`, `score_weights`). Trust weights documented in CLAUDE.md.

Test count: 3134 passing across the 16-crate workspace (+44 over the 3090 baseline at `32d1907`). `cargo clippy --workspace --all-targets -- -D warnings` clean. `cargo fmt --all --check` clean. Drclaw `./gradlew assembleDebug` green.

**Stream D2 -- Auto-learn re-enabled with HITL routing + telemetry (2026-05-04)**
- `BrainConfig.auto_learn` default flipped `false → true`; `ExtractionConfig.enabled` default flipped `false → true`
- HITL routing confirmed safe: `Add` actions auto-approve only when `confidence >= 0.9` AND LLM comparison verifies match; corrections, updates, and deletes always route to the pending queue
- Three process-lifetime `AtomicUsize` telemetry counters in `extraction.rs`: `AUTO_LEARN_PENDING_ROUTED`, `AUTO_LEARN_AUTO_APPROVED`, `AUTO_LEARN_DROPPED`
- Public accessor functions exported from `ironjaw-brain`: `auto_learn_pending_total()`, `auto_learn_approved_total()`, `auto_learn_dropped_total()`
- New RPC `brain.auto_learn.report` (Auto tier) returns `{ pending_routed, auto_approved, dropped }` lifetime counter snapshot

**Wave C2 -- In-flight Tool Registry + session.set_cwd Guard (2026-05-04)**
- `AppState` gains `in_flight_tools: Arc<DashMap<String, HashMap<String, String>>>` and `cwd_locks: Arc<DashMap<String, Arc<Mutex<()>>>>` (both constructors)
- `InFlightGuard` RAII struct in `agentic.rs` inserts registry entry on `tool.start` emit and removes it on drop (panic-safe, covers all three exit paths in `execute_single`)
- `handle_session_set_cwd` rejects with `IN_FLIGHT_TOOLS` error when tools are executing for the session; per-session `cwd_lock` serialises concurrent set_cwd requests; audit-logs every attempt
- 4 unit tests in `agentic::in_flight_registry_tests`

**C2 Sub-B — Cooperative tool cancellation propagation (2026-05-04, follow-up)**
- Initially deferred during C2 because the Tool trait change cascaded to ~39 impl sites, above the ≤25 stop-condition threshold. Shipped in a dedicated follow-up wave the same day.
- `Tool::execute` now takes `cancel: tokio_util::sync::CancellationToken`; the loop in `agentic.rs::execute_tool_batch` threads `ConnectionContext.abort_token` through to each call so `chat.abort` actually kills running tools (not just the LLM stream).
- New `ToolResult::Cancelled` variant — distinct from generic errors so callers (and eventually the drclaw `ToolResultCard`) can render aborted tools differently. Pure-compute tools take the parameter and ignore it; long-running tools (shell streaming, HTTP, watchers) wrap their `await` points in `tokio::select! { _ = cancel.cancelled() => Cancelled, result = work => result }`.
- WASM plugin tools deliberately do not honor cancellation — they're sandboxed under the WASM runtime which already enforces fuel/memory caps; threading a separate token into the WASM context is out of scope.
- 3 integration smokes (`tests/cancellation_smokes.rs`): pure-compute completion, pre-cancelled token, mid-execution cancel firing 50ms into a 10s sleep. Workspace at 3140 tests, 0 failed.
- Commits: `7bee55f` (cascade across 47 files), `4caa5cd` (tests), `ff2a991` (rustfmt sweep on the cascade).

**Wave B2 -- Hook Framework Expansion (CP1-CP6, 2026-05-03)**
- Refactored `crates/ironjaw-ws/src/hooks.rs` into a `hooks/` module: `types.rs`, `registry.rs`, `subprocess.rs`, `wasm_bridge.rs`, `config.rs`, `emit_tests.rs` (CP1-CP6)
- Four new `HookEvent` variants: `PreCompact`, `UserPromptSubmit`, `SkillActivated`, `SkillDeactivated` (latter two per A15 skills↔hooks integration spec)
- Async `Hook` trait + `DashMap`-backed `HookRegistry` (mirrors `commands::SlashCommandRegistry`); sync compatibility shim preserves Stream 8.2 closure tests (CP2)
- Emit sites wired in `handler.rs`: `UserPromptSubmit` after sanitize/injection-detection in `handle_chat_send`, `PreCompact` at three `compact_history` callers, `SkillActivated`/`SkillDeactivated` paired around `model.switch`/`role.switch`/`role.preset.apply` (CP3)
- `SubprocessHook` (`hooks/subprocess.rs`) — fixed-directory enforcement (`~/.config/ironjaw/hooks/<name>`), env scrubbing via `shell::sanitize_env`, 16 KB stdout cap, audit per invocation (CP4)
- `WasmHook` (`hooks/wasm_bridge.rs`) — wraps existing `WasmSandbox` (no fork); `HookContext` flattened into `initial_config`; plugin returns i32 status + reason via `host_log` (CP5)
- Hook discovery (`hooks/config.rs`): scans `<dir>` for `<name>.wasm` / `<name>.{sh,py,exe,bat,cmd,ps1}` paired with sidecar `<name>.toml` manifests; subprocess hooks gated on `[hooks.subprocess].enabled` AND `enabled_events` allowlist; malformed manifests warn-and-skip (CP6)
- New RPC methods: `hooks.list` (Auto, dumps the discovered-hook catalog) and `hooks.reload` (MultiConfirm, rebuilds registry + catalog from disk)
- New config sections: `[hooks]` (master killswitch + dir) and `[hooks.subprocess]` (enabled, timeouts, event allowlist)
- AppState gains `hooks_catalog: HookCatalog` (parallel metadata for `hooks.list`) and `hooks_runtime: Option<HooksRuntime>` (sandbox + paths for `hooks.reload`)
- 74 hook-specific unit tests (target was 30+); 1393 → 1447 total tests on the four B2-touched crates
- Documentation: CLAUDE.md gains `[hooks]` / `[hooks.subprocess]` config tables and `hooks.list` / `hooks.reload` method rows

**Stream 8.9 -- Browser Automation**
- `BrowserAutomation` -- lightweight CDP (Chrome DevTools Protocol) client using reqwest + tokio-tungstenite, no heavy external browser crates
- `BrowserConfig` with validation (port range, timeout bounds, Chrome path verification)
- Core operations: `navigate`, `get_content`, `get_text`, `screenshot`, `click`, `type_text`, `evaluate_js`, `wait_for_selector`
- `CdpConnection` -- WebSocket-based CDP command transport with atomic message ID tracking
- `BrowserTool` registered with Confirm permission tier -- validates URLs, CSS selectors, and JavaScript inputs before execution
- Input sanitization: URL scheme allowlisting (http/https only), CSS selector injection prevention, JavaScript code safety checks (blocks `require('fs')`, `file://`, `__proto__`, etc.)
- `PageInfo` struct for page metadata (URL, title, content)
- `BrowserError` enum with 16 typed error variants
- Cross-platform Chrome discovery (`find_chrome()`) for Windows, macOS, and Linux
- 72 unit tests (no live browser required -- all tests exercise config, validation, error paths, serialization, and tool trait compliance)

---

## [0.13.0] - 2026-03-03

Phases 10-13: Vision, Dr. CLAW v2, Polish, Cutover

### Added

**Phase 10 — Vision/Multimodal Pipeline**
- `ImageProcessor` with format detection (JPEG, PNG, GIF, WebP, BMP), base64 encode/decode, and size validation
- `OcrTool` registered in the tool registry for image-to-text extraction
- Binary WebSocket frame handling — images sent as binary frames are buffered in `ConnectionContext.pending_images` and consumed by the next `chat.send`
- `VisionConfig` — `max_image_size_bytes` (default 10 MB)

**Phase 11 — Dr. CLAW v2 (Multi-Device + Notifications)**
- `DeviceRegistry` — connection tracking with device type and capabilities per connection
- `MessageQueue` — offline message delivery with per-device queuing, max 1000 messages, and 24-hour TTL
- Notification system — `Priority` enum (low / medium / high / urgent), push notifications dispatched to registered devices
- Protocol v4 additions — `ui` and `ui.res` frame types for generative UI components
- New RPC methods: `device.list`, `device.cmd`, `notify.send`

**Phase 12 — Circuit Breakers and Metrics**
- Circuit breaker registry — per-service failure tracking with configurable failure threshold (default 5) and recovery window (default 60 s); open / closed / half-open states
- Internal metrics — request, connection, and error counters; per-method latency tracking
- New RPC methods: `backup.create`, `export.conversations`, `export.brain`, `infra.metrics`

**Phase 13 — Cutover Preparation**
- Task Scheduler wrappers: `ironjaw-wrapper.cmd` and `ironjaw-wrapper-hidden.vbs` for no-console-window service launch on workstation

---

## [0.9.0] - 2026-03-03

Phase 9: Voice

### Added

- Voice pipeline — STT via llama-server `/v1/audio/transcriptions` (Whisper), TTS via `/v1/audio/speech` (Orpheus)
- `VoiceEngine` with per-connection session management, audio buffer accumulation, and inactivity timeout
- `VoiceSession` — push-to-talk and VAD modes; configurable sample rate (16 kHz default) and max buffer size (16 MB)
- `VoiceConfig` — `stt_url`, `stt_model`, `tts_url`, `tts_model`, `tts_voice`, `sample_rate`, `max_audio_buffer_bytes`, `inactivity_timeout_secs`
- New RPC methods: `voice.start`, `voice.stop`, `voice.tts`

---

## [0.8.0] - 2026-03-03

Phase 8: Proactive Engine

### Added

- Cron scheduler — SQLite-backed schedule store with enabled/disabled toggle and cron expression support
- Workflow DAG — multi-step workflows with four action types (Tool, LLM, Notification, Gate) and state tracking (pending / running / completed / failed)
- `ProactiveEngine` — background check loop with configurable interval and automatic workflow step execution
- `ProactiveConfig` — `database_path`, `check_interval_secs`, `max_concurrent_workflows`
- New RPC methods: `schedule.list`, `schedule.create`, `schedule.delete`, `schedule.toggle`, `workflow.create`, `workflow.status`, `workflow.cancel`, `workflow.approve`

---

## [0.7.0] - 2026-03-03

Phase 7: Integrations

### Added

- 13 integration tools (12 base + 1 optional Home Assistant) implemented via the `Tool` trait:
  - **Search**: `SearXNG`, `Perplexica`, `WebSearch` (combined SearXNG + Perplexica)
  - **Claude Code**: `CcSessions` (via cc-bridge), `CcChronicle` (via cc-chronicle)
  - **Infrastructure**: `HostStatus`, `RemoteHostStatus`, `DockerManage`
  - **Git**: `GitStatus`, `GitLog`
  - **Home Assistant**: `HomeAssistant` (optional; requires `ha_url` + `ha_token`)
  - **Secrets**: `SecretsGet`, `SecretsSet` (encrypted vault, AES-256-GCM via the `ring` crate)
- `IntegrationsConfig` — all endpoint URLs, docker-host SSH config, secrets DB path, and HA config
- Secrets vault — SQLite storage with AES-256-GCM envelope encryption

---

## [0.6.0] - 2026-03-02

Phase 6: Brain / RAG

### Added

- 3-layer memory model:
  - **Layer 1 — Identity**: key-value pairs from the `identity` table, always injected as the system message
  - **Layer 2 — Working Memory**: per-session conversation context via `SessionStore`
  - **Layer 3 — Long-term**: vector store with cosine similarity search and configurable top-K auto-retrieval
- `Embedder` using llama-server's OpenAI-compatible `/v1/embeddings` endpoint
- Auto-retrieval — before every `chat.send`, `Brain` prepends identity context plus the top-K most relevant memories
- LRU eviction when the `max_memories` threshold is reached
- `BrainConfig` — `database_path`, `embedding_url`, `embedding_model`, `auto_recall_top_k`, `max_memories`, `enabled`
- New RPC methods: `brain.remember`, `brain.recall`, `brain.forget`, `brain.import`

---

## [0.5.0] - 2026-03-02

Phase 5: Tools

### Added

- `Tool` trait with async `execute`, `name`, `description`, `schema`, and `permission_tier`
- `ToolRegistry` for dynamic tool registration and lookup
- 4 built-in tools:
  - `read_file` (Auto tier)
  - `list_files` (Auto tier)
  - `write_file` (Confirm tier)
  - `shell` (Confirm tier, 64 KB output cap)
- `ToolsConfig` — `working_dir`, `shell_timeout_secs`
- New RPC methods: `tools.list`, `tools.execute` (per-tool tier enforcement applied at dispatch)

---

## [0.4.0] - 2026-03-02

Phase 4: Security

### Added

- 4-tier permission system: Auto (0), Notify (1), Confirm (2), MultiConfirm (3)
- Static method-to-tier mapping in `method_tiers.rs` — unknown methods default to Confirm
- HITL approval gate — `permission.request` event, `CmdRes` approval flow, configurable timeout
- Sliding window rate limiter — per-connection, per-method, with configurable limits
- Input sanitization — strips prompt injection markers (`<system>`, `[INST]`, `<<SYS>>`, and similar patterns)
- `SecurityConfig` — `hitl_timeout_secs`, `rate_limit_enabled`, `sanitize_enabled`

---

## [0.3.0] - 2026-03-02

Phase 3: Sessions

### Added

- `SessionStore` — SQLite in WAL mode with auto-migration; supports create, list, get, delete, and rename
- Per-session message history with cursor-based pagination
- Context token limit (default 32 768 tokens) enforced by session-managed context trimming
- Session resume — pass `session_id` in `auth.connect` params to restore a prior session
- New RPC methods: `session.list`, `session.create`, `session.switch`, `session.delete`, `session.rename`, `chat.history`

---

## [0.2.0] - 2026-03-02

Phase 2: LLM Router

### Added

- `LlmRouter` with provider abstraction — llama-server, OpenAI, and Mock provider backends
- Streaming chat completions via `/v1/chat/completions` (SSE) with real-time token delivery over `stream` frames
- `ChatRequest`, `ChatMessage`, and `ChatResponse` types with token usage tracking
- `chat.send` handler with full audit trail (model, token counts, latency)
- `LlmConfig` — `default_model`, `providers` array (each with `name`, `type`, `base_url`, `api_key`, `timeout`)
- New RPC methods: `model.list`, `model.status`

---

## [0.1.0] - 2026-03-02

Phase 1: Foundation

### Added

- Cargo workspace with 12 crates
- axum 0.8 WebSocket server on port 18794
- Protocol v4 — six frame types: `req`, `res`, `stream`, `event`, `cmd`, `cmd.res`
- Token-based authentication via `auth.connect`
- `GET /health` endpoint returning JSON status
- Append-only SQLite audit trail (`ironjaw-audit` crate)
- TOML configuration (`config/ironjaw.toml`)
- Structured logging via `tracing` with pretty and JSON format options
- `ping` method with uptime reporting

---

[Unreleased]: https://github.com/scaso01/ironjaw/compare/v0.13.0...HEAD
[0.13.0]: https://github.com/scaso01/ironjaw/compare/v0.9.0...v0.13.0
[0.9.0]: https://github.com/scaso01/ironjaw/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/scaso01/ironjaw/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/scaso01/ironjaw/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/scaso01/ironjaw/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/scaso01/ironjaw/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/scaso01/ironjaw/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/scaso01/ironjaw/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/scaso01/ironjaw/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/scaso01/ironjaw/releases/tag/v0.1.0
