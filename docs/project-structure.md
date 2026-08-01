# Project Structure

Extracted from CLAUDE.md on 2026-07-31 so it stops loading into every session. Read this when finding your way around the tree.


```
ironjaw/
  Cargo.toml                    # workspace root (16 crates)
  rustfmt.toml                  # max_width=100, edition=2021
  config/
    ironjaw.toml                # runtime config (13 sections)
  hooks/
    pre-commit                  # secret scan + cargo fmt + clippy
    install.sh                  # copies hook to .git/hooks/
  docs/
    plans/
      2026-03-02-ironjaw-design.md
    AUDIT_BASELINE.md           # code quality baseline
  crates/
    ironjaw-core/               # binary entry point, config, startup
      src/main.rs               # tokio::main, config loading, service init
      src/config.rs             # Config struct (13 sections), TOML parsing
      src/hot_reload.rs         # config hot-reload watcher
    ironjaw-ws/                 # WebSocket server, protocol, handlers
      src/server.rs             # axum router, /health, /ws, /v1/* endpoints
      src/protocol.rs           # Frame enum (8 variants), ErrorPayload
      src/handler.rs            # handle_connection, method dispatch (~166 methods)
      src/auth.rs               # auth.connect handler, token validation
      src/browser.rs            # BrowserAutomation, lightweight CDP client for browser control
      src/state.rs              # AppState (9 sub-state structs + 14 flat fields)
      src/devices.rs            # DeviceRegistry, device tracking
      src/notifications.rs      # push notification dispatch
      src/queue.rs              # MessageQueue for offline devices
      src/circuit_breaker.rs    # CircuitBreakerRegistry, Metrics
      src/skills.rs             # SkillRegistry, Skill trait, dynamic skill loading
      src/hooks.rs              # HookRegistry, lifecycle hooks (pre/post tool, message)
      src/participants.rs       # ParticipantTracker, presence awareness
      src/agent_graph.rs        # AgentCommunicationGraph, inter-agent routing
      src/channel.rs            # ChannelAdapter trait, WebSocket/REST/SSE adapters
      src/concurrency.rs        # TaskPool, bounded concurrent task execution
      src/shell_stream.rs       # ShellStream, real-time shell output streaming
      src/dashboard.rs          # Dashboard REST API (expanded, metrics + config)
      src/mobile_api.rs         # Mobile-optimized REST endpoints
      src/ops.rs                # OpsManager, service health + lifecycle management
      src/plugins.rs            # PluginHost, dynamic plugin loading + isolation
      src/forking.rs            # ConversationForker, branch/merge conversations
      src/subagent.rs           # SubAgentDelegator, spawn + supervise child agents
      src/a2a.rs                # A2A protocol (Agent-to-Agent communication)
      src/openai_compat.rs      # OpenAI-compatible REST API (/v1/chat/completions, etc.)
      tests/ws_integration.rs   # WebSocket integration tests
    ironjaw-security/           # permission enforcement
      src/tiers.rs              # PermissionTier enum (Auto/Notify/Confirm/MultiConfirm)
      src/method_tiers.rs       # static method-to-tier mapping (38 entries)
      src/hitl.rs               # HitlGate, approval/denial flow
      src/rate_limit.rs         # sliding window rate limiter
      src/sanitize.rs           # prompt injection marker stripping
      src/validate.rs           # input validation (schema, bounds)
      src/scrubber.rs           # output scrubbing (PII, secrets)
      src/ssrf.rs               # SSRF protection (URL allowlist)
      src/taint.rs              # taint tracking for untrusted data
      src/loop_guard.rs         # infinite loop detection + circuit break
      src/leak_detector.rs      # credential/key leak detection
      src/chain_alert.rs        # prompt chain attack alerting
    ironjaw-audit/              # append-only audit trail
      src/store.rs              # AuditStore (SQLite), log_event
      src/types.rs              # AuditEvent, AuditFilter
    ironjaw-session/            # persistent sessions + messages
      src/store.rs              # SessionStore (SQLite/WAL), CRUD, pagination
      src/types.rs              # Session, Message, SessionFilter
      src/compaction.rs         # context compaction (summarize + prune old messages)
      src/repair.rs             # session repair (orphan cleanup, integrity checks)
    ironjaw-llm/                # LLM provider abstraction
      src/router.rs             # LlmRouter, provider selection
      src/llama_server.rs       # llama-server provider (OpenAI-compat)
      src/openai.rs             # OpenAI provider
      src/provider.rs           # Provider trait, MockProvider for tests
      src/types.rs              # ChatMessage, ChatRequest, ChatResponse, Usage
      src/complexity.rs         # ComplexityScorer, prompt complexity routing
      src/usage.rs              # UsageTracker, per-model token accounting
      src/decorators.rs         # RetryProvider, FailoverProvider, CacheProvider
      src/cache.rs              # ResponseCache, TTL-based LLM response caching
      src/nudge.rs              # NudgeEngine, follow-up suggestion generation
      src/json_repair.rs        # JSON repair for malformed LLM output
    ironjaw-tools/              # tool execution engine
      src/registry.rs           # ToolRegistry, Tool trait
      src/types.rs              # ToolResult, ToolInput, ToolSchema
      src/parallel.rs           # ParallelExecutor, fan-out tool execution
      src/builtin/
        read_file.rs            # read_file tool (Auto)
        list_files.rs           # list_files tool (Auto)
        write_file.rs           # write_file tool (Confirm)
        shell.rs                # shell tool (Confirm)
        process.rs              # ProcessTool, process listing/management
        patch.rs                # PatchTool, unified diff application
        event_publish.rs        # EventPublishTool, emit custom events
        schedule_create.rs      # ScheduleCreateTool (wraps proactive)
        schedule_list.rs        # ScheduleListTool
        schedule_toggle.rs      # ScheduleToggleTool
    ironjaw-brain/              # RAG memory with embeddings
      src/brain.rs              # Brain struct, auto-recall, context building
      src/store.rs              # MemoryStore (SQLite), vector search
      src/embedder.rs           # Embedder (llama-server /v1/embeddings)
      src/types.rs              # Memory, MemoryFilter, SearchResult
      src/graph.rs              # KnowledgeGraph, entity/relation triples
      src/cortex.rs             # MemoryCortex, pub/sub memory bulletin board
      src/hybrid_search.rs      # HybridSearch (Vector + FTS5 + Graph + RRF fusion)
      src/learner.rs            # Learner, session summary generation, entity extraction prompts
      src/maintenance.rs        # MemoryMaintenance, decay/pruning/compaction + sleep-time agents
    ironjaw-integrations/       # 13+ integration tools
      src/lib.rs                # IntegrationsConfig, register_integrations
      src/search/
        searxng.rs              # SearxngTool
        perplexica.rs           # PerplexicaTool
        web_search.rs           # WebSearchTool (combined)
      src/cc/
        sessions.rs             # CcSessionsTool (cc-bridge)
        chronicle.rs            # CcChronicleTool (cc-chronicle)
      src/infra/
        workstation.rs                # HostStatusTool
        docker-host.rs              # RemoteHostStatusTool (SSH)
        docker.rs               # DockerManageTool (SSH)
        docker_exec.rs          # DockerExecTool (remote container exec)
      src/android/
        adb.rs                  # AdbTool (Android Debug Bridge integration)
      src/git/
        tools.rs                # GitStatusTool, GitLogTool
      src/homeassistant/
        tool.rs                 # HomeAssistantTool (optional)
      src/secrets/
        vault.rs                # SecretsVault (encrypted SQLite)
        tools.rs                # SecretsGetTool, SecretsSetTool
    ironjaw-proactive/          # cron scheduler + workflow DAG
      src/engine.rs             # ProactiveEngine, schedule checking
      src/scheduler.rs          # CronScheduler, next-run calculation
      src/store.rs              # ScheduleStore (SQLite)
      src/types.rs              # Schedule, Workflow, WorkflowStep
      src/workflow.rs           # WorkflowRunner, step execution
    ironjaw-voice/              # STT/TTS pipeline
      src/engine.rs             # VoiceEngine, session management
      src/session.rs            # VoiceSession lifecycle
      src/stt.rs                # SttClient (llama-server whisper)
      src/tts.rs                # TtsClient (llama-server orpheus)
      src/types.rs              # VoiceSession, AudioFormat
    ironjaw-vision/             # multimodal image processing
      src/processor.rs          # ImageProcessor, image analysis
      src/ocr_tool.rs           # OcrTool registration
      src/types.rs              # ImageAnalysis, VisionRequest
    ironjaw-mcp/                # MCP (Model Context Protocol) client + server
      src/server.rs             # McpServer, expose 11 brain tools over MCP (stdio mode)
      src/client.rs             # McpClient, consume external MCP servers
      src/transport.rs          # stdio/HTTP transport adapters
      src/types.rs              # McpTool, McpResource, McpPrompt
      src/lib.rs                # register_brain_tools(), crate re-exports
      src/error.rs              # MCP-specific error types
    ironjaw-cli/                # CLI client (14th crate)
      src/lib.rs                # CLI entry point, argument parsing
      src/commands/
        mod.rs                  # command dispatch
        chat.rs                 # interactive chat REPL
        session.rs              # session management commands
        tools.rs                # tool listing/execution
        brain.rs                # memory query commands
        status.rs               # infrastructure status
    ironjaw-claude/             # CC subprocess management + session scanning
      src/lib.rs                # crate root, re-exports
      src/types.rs              # ClaudeConfig, CcStreamEvent, HistoricalSession
      src/runner.rs             # ClaudeRunner subprocess manager (immediate ack, streaming sessionId)
      src/history.rs            # HistoryReader JSONL parser (read, read_since, reverse)
      src/scanner.rs            # SessionScanner (scan ~/.claude/projects, CWD lookup)
      src/tracker.rs            # ActiveSessionTracker registry
      src/cost.rs               # Cost tracking for CC sessions
```
