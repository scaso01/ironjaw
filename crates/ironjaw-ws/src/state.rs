use crate::agent_graph::AgentGraph;
use crate::blackboard::Blackboard;
use crate::circuit_breaker::{CircuitBreakerRegistry, Metrics};
use crate::code_act::CodeActExecutor;
use crate::context_health::ContextHealthMonitor;
use crate::devices::DeviceRegistry;
use crate::forking::ForkManager;
use crate::group::GroupManager;
use crate::hooks::HookRegistry;
use crate::mailbox::Mailbox;
use crate::meta_config::MetaConfigOptimizer;
use crate::plugins::PluginRegistry;
use crate::queue::MessageQueue;
use crate::sentiment::ContinueCounter;
use crate::session_snapshot::SessionSnapshotStore;
use crate::skill_library::SkillLibrary;
use crate::skills::SkillRegistry;
use crate::subagent::SubAgentManager;
use crate::tool_registry::DynamicToolRegistry;
use dashmap::DashMap;
use ironjaw_audit::AuditStore;
use ironjaw_brain::{
    Brain, ExtractionConfig, ExtractionPipeline, SleepAgentScheduler, SleepAgentsConfig,
};
use ironjaw_claude;
use ironjaw_llm::prefetch::PrefetchManager;
use ironjaw_llm::prompt_cache::PromptCache;
use ironjaw_llm::{CloudModelCatalog, LlamaServerState, LlmRouter, ModelRegistry, PerfTracker};
use ironjaw_mcp::McpClient;
use ironjaw_proactive::daemon::{DaemonConfig, ProactiveDaemon};
use ironjaw_proactive::ProactiveEngine;
use ironjaw_security::classifier::PermissionClassifier;
use ironjaw_security::{HitlGate, RateLimiter, RuleEngine, SanitizeConfig};
use ironjaw_session::compaction::AutoCompactBreaker;
use ironjaw_session::context_collapse::CollapseRegistry;
use ironjaw_session::SessionStore;
use ironjaw_tools::snapshots::FileSnapshotStore;
use ironjaw_tools::worktree::WorktreeManager;
use ironjaw_tools::ToolRegistry;
use ironjaw_vision::ImageProcessor;
use ironjaw_voice::VoiceEngine;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// Integration service URLs for HTTP proxying
#[derive(Clone, Debug)]
pub struct IntegrationUrls {
    pub cc_bridge_url: String,
    pub cc_chronicle_url: String,
    pub llm_base_url: String,
}

/// Authentication, rate limiting, HITL, and input sanitization.
#[derive(Clone)]
pub struct SecurityState {
    pub rate_limiter: Arc<RateLimiter>,
    pub hitl_gate: Arc<HitlGate>,
    pub sanitize: Arc<SanitizeConfig>,
    pub rule_engine: Arc<tokio::sync::RwLock<RuleEngine>>,
    pub classifier: Arc<PermissionClassifier>,
}

/// LLM provider routing, model registry, and performance tracking.
#[derive(Clone)]
pub struct LlmState {
    pub router: Arc<LlmRouter>,
    pub llama_state: Arc<tokio::sync::RwLock<LlamaServerState>>,
    pub model_registry: Arc<ModelRegistry>,
    pub cloud_catalogs: Arc<Vec<CloudModelCatalog>>,
    pub aliases: Arc<HashMap<String, String>>,
    pub perf_tracker: Arc<PerfTracker>,
    pub prompt_cache: Arc<Mutex<PromptCache>>,
    pub prefetch: Arc<PrefetchManager>,
    /// Sender for model change events (set when llama-server monitor is active).
    pub model_change_tx: Option<tokio::sync::broadcast::Sender<ironjaw_llm::ModelChangeEvent>>,
}

/// Brain/RAG memory, embeddings, knowledge graph, context health.
#[derive(Clone)]
pub struct BrainState {
    pub engine: Arc<Brain>,
    pub extraction_pipeline: Arc<ExtractionPipeline>,
    pub sleep_scheduler: Arc<SleepAgentScheduler>,
    pub context_health: Arc<ContextHealthMonitor>,
    pub blackboard: Arc<Blackboard>,
    pub snapshots: Arc<ironjaw_brain::SnapshotStore>,
}

/// Session storage, context limits, conversation forking.
#[derive(Clone)]
pub struct SessionState {
    pub store: SessionStore,
    pub context_token_limit: u64,
    pub fork_manager: Arc<tokio::sync::RwLock<ForkManager>>,
    pub collapse_registry: Arc<Mutex<CollapseRegistry>>,
    pub auto_compact_breaker: Arc<AutoCompactBreaker>,
}

/// Claude Code subprocess management.
#[derive(Clone)]
pub struct ClaudeState {
    pub tracker: Arc<ironjaw_claude::ActiveSessionTracker>,
    pub config: ironjaw_claude::CcConfig,
}

/// Multi-device registry and offline message queue.
#[derive(Clone)]
pub struct DeviceState {
    pub registry: Arc<DeviceRegistry>,
    pub message_queue: Arc<MessageQueue>,
}

/// Inter-agent communication and group management.
#[derive(Clone)]
pub struct AgentState {
    pub graph: Arc<AgentGraph>,
    pub group_manager: Arc<GroupManager>,
    pub mailbox: Arc<Mailbox>,
    pub subagent_manager: Arc<SubAgentManager>,
}

/// Skill registry and skill library.
#[derive(Clone)]
pub struct SkillState {
    pub registry: Arc<RwLock<SkillRegistry>>,
    pub library: Arc<SkillLibrary>,
}

/// Metrics, circuit breakers, observability.
#[derive(Clone)]
pub struct ObservabilityState {
    pub metrics: Arc<Metrics>,
    pub circuit_breakers: Arc<CircuitBreakerRegistry>,
    pub counters: Arc<DashMap<String, AtomicU64>>,
}

impl ObservabilityState {
    /// Increment a named counter by 1.
    pub fn inc(&self, key: &str) {
        self.counters
            .entry(key.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Increment a named counter by an arbitrary delta.
    pub fn inc_by(&self, key: &str, delta: u64) {
        self.counters
            .entry(key.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(delta, Ordering::Relaxed);
    }

    /// Read a named counter (returns 0 if never set).
    pub fn get_counter(&self, key: &str) -> u64 {
        self.counters
            .get(key)
            .map(|v| v.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Return a point-in-time snapshot of all counters as plain u64 values.
    pub fn snapshot_counters(&self) -> HashMap<String, u64> {
        self.counters
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect()
    }
}

/// Feature flags and limits for the agentic tool-call loop (chat.send).
/// Mirrors `ironjaw_core::config::ChatAgenticConfig`; duplicated here to avoid
/// a circular crate dependency (ironjaw-core → ironjaw-ws → ironjaw-core).
#[derive(Debug, Clone)]
pub struct ChatAgenticConfig {
    /// Master on/off switch for the agentic loop.  Defaults to `false`.
    pub enabled: bool,
    /// Maximum LLM↔tool round-trips before forcing a synthesis pass.
    pub max_iterations: usize,
    /// Per-tool execution timeout in seconds.
    pub tool_timeout_secs: u64,
    /// Phase 5: enable the ToolNudger re-prompt when the LLM describes a tool
    /// instead of calling it.
    pub nudger_enabled: bool,
    /// Phase 5: maximum nudge retries per user turn.
    pub nudger_max_retries: usize,
    /// Phase 6: enable session-scoped HITL cache.  When `true`, a tool approved
    /// with `scope = "session"` is auto-approved for the remainder of the session.
    pub hitl_session_cache_enabled: bool,
    /// Phase 11: when `true`, successful tool executions write a `ToolVerified`
    /// memory back to the brain so future auto-recall surfaces fresh live-state.
    pub tool_memory_writeback_enabled: bool,
    /// A2: hard ceiling on cumulative (prompt + completion) tokens across all
    /// iterations of one `chat.send` turn.  `0` disables the ceiling.
    pub run_token_ceiling: u64,
    /// Cap on the characters of a single tool result fed back into the LLM
    /// context (and persisted to the session).  `0` disables the cap.
    pub tool_result_max_chars: usize,
    /// G2: minimum characters (trimmed) for a final agentic answer.  A shorter
    /// answer triggers ONE re-prompt; the second answer is accepted as-is.
    /// `0` disables the guard.
    pub min_answer_chars: usize,
    /// FIX #2b: per-turn wall-clock deadline in seconds.  Checked at the top of
    /// each iteration; on expiry the loop breaks to forced synthesis so a
    /// slow/wedged local model can never hang a turn forever.  `0` disables it.
    pub turn_deadline_secs: u64,
}

impl Default for ChatAgenticConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_iterations: 5,
            tool_timeout_secs: 30,
            nudger_enabled: true,
            nudger_max_retries: 2,
            hitl_session_cache_enabled: true,
            tool_memory_writeback_enabled: true,
            run_token_ceiling: 0,
            tool_result_max_chars: 12_288,
            min_answer_chars: 20,
            turn_deadline_secs: 360,
        }
    }
}

/// Local-model behavioral prompt + curated tool set (mirrors
/// `ironjaw_core::config::ChatLocalConfig`; duplicated here to avoid a circular
/// crate dependency). Applies only to llama-server models. `tools` empty →
/// fall back to the full registry.
#[derive(Debug, Clone)]
pub struct LocalModelConfig {
    pub enabled: bool,
    pub system_prompt_file: String,
    pub tools: Vec<String>,
}

impl Default for LocalModelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            system_prompt_file: "config/prompts/local_agentic.md".to_string(),
            tools: Vec::new(),
        }
    }
}

/// B2 CP6: holds the discovery context required to rebuild the hook
/// catalog at runtime (`hooks.reload`). Cheap to clone (Arc fields). Built
/// by main.rs at startup when `[hooks] enabled = true`.
#[derive(Clone)]
pub struct HooksRuntime {
    /// Resolved canonical path to the hooks directory.
    pub dir: std::path::PathBuf,
    /// Cached `[hooks.subprocess].enabled` flag.
    pub subprocess_enabled: bool,
    /// Cached `[hooks.subprocess].enabled_events` allowlist.
    pub subprocess_enabled_events: Arc<Vec<String>>,
    /// Cached `[hooks.subprocess].default_timeout_secs`.
    pub subprocess_default_timeout_secs: u64,
    /// Shared sandbox for all WASM hooks (one engine, many plugins).
    pub sandbox: Arc<crate::wasm_sandbox::WasmSandbox>,
}

/// Shared application state passed to all WebSocket handlers
#[derive(Clone)]
pub struct AppState {
    // Grouped sub-states
    pub security: SecurityState,
    pub llm: LlmState,
    pub brain: BrainState,
    pub session: SessionState,
    pub claude: ClaudeState,
    pub device: DeviceState,
    pub agents: AgentState,
    pub skill: SkillState,
    pub observability: ObservabilityState,

    // Remaining flat fields
    pub auth_token: String,
    pub audit: AuditStore,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub tools: Arc<ToolRegistry>,
    pub commands: Arc<crate::commands::SlashCommandRegistry>,
    /// B2: lifecycle hook registry. Cuts across many subsystems
    /// (compaction, chat.send, skill switching) so it lives as a flat
    /// `AppState` field rather than nested in any sub-state.
    pub hooks: Arc<HookRegistry>,
    /// B2 CP6: discovered-hook catalog. Populated at startup (and again on
    /// `hooks.reload`) by the loader; queried by `hooks.list`. Independent
    /// of the registry (which holds Hook trait objects) so listing doesn't
    /// require dispatching into hooks.
    pub hooks_catalog: crate::hooks::HookCatalog,
    /// B2 CP6: discovery context for `hooks.reload`. `None` when hooks are
    /// disabled in config (no allocation cost). Populated by main.rs at
    /// startup when `[hooks] enabled = true`.
    pub hooks_runtime: Option<HooksRuntime>,
    pub proactive: Arc<ProactiveEngine>,
    pub voice: Arc<VoiceEngine>,
    pub image_processor: Arc<ImageProcessor>,
    pub http_client: reqwest::Client,
    pub integration_urls: IntegrationUrls,
    pub openai_compat: bool,
    /// How long to wait for the initial auth.connect message before closing the connection.
    pub auth_timeout_secs: u64,
    pub dynamic_tools: Arc<DynamicToolRegistry>,
    pub chat_agentic: ChatAgenticConfig,
    /// Local-model base prompt + curated tools (llama-server models only).
    pub local_model: LocalModelConfig,
    pub meta_config: Arc<MetaConfigOptimizer>,
    pub plugin_registry: Arc<tokio::sync::RwLock<PluginRegistry>>,
    pub code_act: Arc<CodeActExecutor>,
    pub snapshots: Arc<tokio::sync::Mutex<FileSnapshotStore>>,
    pub session_snapshots: Arc<SessionSnapshotStore>,
    pub worktree: Arc<WorktreeManager>,
    pub daemon: Arc<Mutex<ProactiveDaemon>>,
    pub continue_counter: Arc<Mutex<ContinueCounter>>,
    /// C2-A: tracks tools currently executing within each session.
    /// Key = session_id, Value = map of tool_call_id → tool_name.
    /// Consulted by `session.set_cwd` to reject mid-flight directory changes.
    pub in_flight_tools: Arc<DashMap<String, HashMap<String, String>>>,
    /// C2-A: per-session mutex that serialises concurrent `session.set_cwd`
    /// requests so only one writer modifies the working directory at a time.
    pub cwd_locks: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// C1 tier-3: optional global default working directory from `[tools] working_dir`
    /// config. Resolution chain in `resolve_session_cwd`:
    ///   1. `session.working_dir` (per-session override)
    ///   2. `tools_working_dir` (this field — global default)
    ///   3. `std::env::current_dir()` (process default)
    ///
    /// Populated once at startup by `main.rs` from `config.tools.working_dir`.
    /// `None` when config value is the default (".") since that is equivalent
    /// to falling through to `current_dir()`.
    pub tools_working_dir: Arc<tokio::sync::RwLock<Option<std::path::PathBuf>>>,
    /// Live MCP client handles, one per successfully-initialized `[[mcp.servers]]` entry.
    /// Stored here so they stay alive for the gateway lifetime (their background actor tasks
    /// are tied to `Arc` ref-counts).
    pub mcp_clients: Vec<Arc<McpClient>>,
    /// Native MCP-over-HTTP endpoint wiring, set by `main.rs` when
    /// `[server] mcp_token` is configured. `None` leaves `POST /mcp` unregistered.
    pub mcp_http: Option<crate::mcp_http::McpHttp>,
    /// A3: registry of live [`crate::stream_buffer::RunBuffer`]s, one per
    /// session with an in-flight `chat.send` agentic-loop run. Lets a
    /// reconnecting client on the same `session_id` catch up on a run that
    /// kept generating after the original socket dropped, and lets
    /// `chat.abort` reach a run that has outlived its original connection.
    pub chat_runs: Arc<crate::stream_buffer::RunRegistry>,
}

impl AppState {
    /// Create AppState with auto-approve HITL gate (for backward-compatible tests).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        auth_token: String,
        audit: AuditStore,
        llm: Arc<LlmRouter>,
        sessions: SessionStore,
        context_token_limit: u64,
        brain: Arc<Brain>,
        proactive: ProactiveEngine,
        voice: VoiceEngine,
        image_processor: ImageProcessor,
    ) -> Self {
        let mut tools = ToolRegistry::new();
        ironjaw_tools::register_builtins(&mut tools);
        let snapshots = Arc::new(tokio::sync::Mutex::new(FileSnapshotStore::new(
            100,
            50 * 1024 * 1024,
        )));
        ironjaw_tools::register_restore_file(&mut tools, snapshots.clone());
        ironjaw_tools::register_todo(&mut tools);
        let cc_config = ironjaw_claude::CcConfig::default();
        let cc_tracker = Arc::new(ironjaw_claude::ActiveSessionTracker::new(
            cc_config.max_concurrent,
        ));
        let brain_pool = brain.store().pool().clone();
        Self {
            security: SecurityState {
                rate_limiter: Arc::new(RateLimiter::with_defaults()),
                hitl_gate: Arc::new(HitlGate::auto_approve()),
                sanitize: Arc::new(SanitizeConfig::default_patterns()),
                rule_engine: Arc::new(tokio::sync::RwLock::new(RuleEngine::new())),
                classifier: Arc::new(PermissionClassifier::new("fast", 500)),
            },
            llm: LlmState {
                router: llm,
                llama_state: Arc::new(tokio::sync::RwLock::new(LlamaServerState::default())),
                model_registry: Arc::new(ModelRegistry {
                    default: String::new(),
                    active: std::sync::RwLock::new(None),
                    models: HashMap::new(),
                    source_path: None,
                }),
                cloud_catalogs: Arc::new(Vec::new()),
                aliases: Arc::new(HashMap::new()),
                perf_tracker: Arc::new(PerfTracker::new()),
                prompt_cache: Arc::new(Mutex::new(PromptCache::new(128))),
                prefetch: Arc::new(PrefetchManager::new()),
                model_change_tx: None,
            },
            brain: BrainState {
                engine: brain,
                extraction_pipeline: Arc::new(ExtractionPipeline::new(ExtractionConfig::default())),
                sleep_scheduler: Arc::new(SleepAgentScheduler::new(SleepAgentsConfig::default())),
                context_health: Arc::new(ContextHealthMonitor::new()),
                blackboard: Arc::new(Blackboard::new(3600)),
                snapshots: Arc::new(ironjaw_brain::SnapshotStore::from_pool(brain_pool)),
            },
            session: SessionState {
                store: sessions,
                context_token_limit,
                fork_manager: Arc::new(tokio::sync::RwLock::new(ForkManager::new())),
                collapse_registry: Arc::new(Mutex::new(CollapseRegistry::new(256))),
                auto_compact_breaker: Arc::new(AutoCompactBreaker::new(3)),
            },
            claude: ClaudeState {
                tracker: cc_tracker,
                config: cc_config,
            },
            device: DeviceState {
                registry: Arc::new(DeviceRegistry::new()),
                message_queue: Arc::new(MessageQueue::new(1000, 24)),
            },
            agents: AgentState {
                graph: Arc::new(AgentGraph::new(10000)),
                group_manager: Arc::new(GroupManager::new()),
                mailbox: Arc::new(Mailbox::new(1000)),
                subagent_manager: Arc::new(SubAgentManager::default()),
            },
            skill: SkillState {
                registry: Arc::new(RwLock::new(SkillRegistry::new())),
                library: Arc::new(SkillLibrary::new()),
            },
            observability: ObservabilityState {
                metrics: Arc::new(Metrics::new()),
                circuit_breakers: Arc::new(CircuitBreakerRegistry::new(5, 60)),
                counters: Arc::new(DashMap::new()),
            },
            auth_token,
            audit,
            started_at: chrono::Utc::now(),
            tools: Arc::new(tools),
            commands: Arc::new(crate::commands::SlashCommandRegistry::new()),
            hooks: Arc::new(HookRegistry::new()),
            hooks_catalog: crate::hooks::empty_catalog(),
            hooks_runtime: None,
            proactive: Arc::new(proactive),
            voice: Arc::new(voice),
            image_processor: Arc::new(image_processor),
            http_client: reqwest::Client::builder()
                .tcp_nodelay(true)
                .build()
                .expect("http client"),
            integration_urls: IntegrationUrls {
                cc_bridge_url: String::new(),
                cc_chronicle_url: String::new(),
                llm_base_url: String::new(),
            },
            openai_compat: false,
            auth_timeout_secs: 30,
            dynamic_tools: Arc::new(DynamicToolRegistry::new()),
            chat_agentic: ChatAgenticConfig::default(),
            local_model: LocalModelConfig::default(),
            meta_config: Arc::new(MetaConfigOptimizer::new()),
            plugin_registry: Arc::new(tokio::sync::RwLock::new(PluginRegistry::new())),
            code_act: Arc::new(CodeActExecutor::new()),
            snapshots,
            session_snapshots: Arc::new(SessionSnapshotStore::new(5)),
            worktree: Arc::new(WorktreeManager::new(".", ".worktrees")),
            daemon: Arc::new(Mutex::new(ProactiveDaemon::new(DaemonConfig::default()))),
            continue_counter: Arc::new(Mutex::new(ContinueCounter::new())),
            in_flight_tools: Arc::new(DashMap::new()),
            cwd_locks: Arc::new(DashMap::new()),
            tools_working_dir: Arc::new(tokio::sync::RwLock::new(None)),
            mcp_clients: Vec::new(),
            mcp_http: None,
            chat_runs: Arc::new(crate::stream_buffer::RunRegistry::new()),
        }
    }

    /// Create AppState with custom security components.
    #[allow(clippy::too_many_arguments)]
    pub fn with_security(
        auth_token: String,
        audit: AuditStore,
        llm: Arc<LlmRouter>,
        sessions: SessionStore,
        context_token_limit: u64,
        rate_limiter: RateLimiter,
        hitl_gate: HitlGate,
        sanitize: SanitizeConfig,
        tools: ToolRegistry,
        brain: Arc<Brain>,
        proactive: ProactiveEngine,
        voice: VoiceEngine,
        image_processor: ImageProcessor,
        integration_urls: IntegrationUrls,
    ) -> Self {
        let cc_config = ironjaw_claude::CcConfig::default();
        let cc_tracker = Arc::new(ironjaw_claude::ActiveSessionTracker::new(
            cc_config.max_concurrent,
        ));
        let brain_pool = brain.store().pool().clone();
        Self {
            security: SecurityState {
                rate_limiter: Arc::new(rate_limiter),
                hitl_gate: Arc::new(hitl_gate),
                sanitize: Arc::new(sanitize),
                rule_engine: Arc::new(tokio::sync::RwLock::new(RuleEngine::new())),
                classifier: Arc::new(PermissionClassifier::new("fast", 500)),
            },
            llm: LlmState {
                router: llm,
                llama_state: Arc::new(tokio::sync::RwLock::new(LlamaServerState::default())),
                model_registry: Arc::new(ModelRegistry {
                    default: String::new(),
                    active: std::sync::RwLock::new(None),
                    models: HashMap::new(),
                    source_path: None,
                }),
                cloud_catalogs: Arc::new(Vec::new()),
                aliases: Arc::new(HashMap::new()),
                perf_tracker: Arc::new(PerfTracker::new()),
                prompt_cache: Arc::new(Mutex::new(PromptCache::new(128))),
                prefetch: Arc::new(PrefetchManager::new()),
                model_change_tx: None,
            },
            brain: BrainState {
                engine: brain,
                extraction_pipeline: Arc::new(ExtractionPipeline::new(ExtractionConfig::default())),
                sleep_scheduler: Arc::new(SleepAgentScheduler::new(SleepAgentsConfig::default())),
                context_health: Arc::new(ContextHealthMonitor::new()),
                blackboard: Arc::new(Blackboard::new(3600)),
                snapshots: Arc::new(ironjaw_brain::SnapshotStore::from_pool(brain_pool)),
            },
            session: SessionState {
                store: sessions,
                context_token_limit,
                fork_manager: Arc::new(tokio::sync::RwLock::new(ForkManager::new())),
                collapse_registry: Arc::new(Mutex::new(CollapseRegistry::new(256))),
                auto_compact_breaker: Arc::new(AutoCompactBreaker::new(3)),
            },
            claude: ClaudeState {
                tracker: cc_tracker,
                config: cc_config,
            },
            device: DeviceState {
                registry: Arc::new(DeviceRegistry::new()),
                message_queue: Arc::new(MessageQueue::new(1000, 24)),
            },
            agents: AgentState {
                graph: Arc::new(AgentGraph::new(10000)),
                group_manager: Arc::new(GroupManager::new()),
                mailbox: Arc::new(Mailbox::new(1000)),
                subagent_manager: Arc::new(SubAgentManager::default()),
            },
            skill: SkillState {
                registry: Arc::new(RwLock::new(SkillRegistry::new())),
                library: Arc::new(SkillLibrary::new()),
            },
            observability: ObservabilityState {
                metrics: Arc::new(Metrics::new()),
                circuit_breakers: Arc::new(CircuitBreakerRegistry::new(5, 60)),
                counters: Arc::new(DashMap::new()),
            },
            auth_token,
            audit,
            started_at: chrono::Utc::now(),
            tools: Arc::new(tools),
            commands: Arc::new(crate::commands::SlashCommandRegistry::new()),
            hooks: Arc::new(HookRegistry::new()),
            hooks_catalog: crate::hooks::empty_catalog(),
            hooks_runtime: None,
            proactive: Arc::new(proactive),
            voice: Arc::new(voice),
            image_processor: Arc::new(image_processor),
            http_client: reqwest::Client::builder()
                .tcp_nodelay(true)
                .build()
                .expect("http client"),
            integration_urls,
            openai_compat: false,
            auth_timeout_secs: 30,
            dynamic_tools: Arc::new(DynamicToolRegistry::new()),
            chat_agentic: ChatAgenticConfig::default(),
            local_model: LocalModelConfig::default(),
            meta_config: Arc::new(MetaConfigOptimizer::new()),
            plugin_registry: Arc::new(tokio::sync::RwLock::new(PluginRegistry::new())),
            code_act: Arc::new(CodeActExecutor::new()),
            snapshots: Arc::new(tokio::sync::Mutex::new(FileSnapshotStore::new(
                100,
                50 * 1024 * 1024,
            ))),
            session_snapshots: Arc::new(SessionSnapshotStore::new(5)),
            worktree: Arc::new(WorktreeManager::new(".", ".worktrees")),
            daemon: Arc::new(Mutex::new(ProactiveDaemon::new(DaemonConfig::default()))),
            continue_counter: Arc::new(Mutex::new(ContinueCounter::new())),
            in_flight_tools: Arc::new(DashMap::new()),
            cwd_locks: Arc::new(DashMap::new()),
            tools_working_dir: Arc::new(tokio::sync::RwLock::new(None)),
            mcp_clients: Vec::new(),
            mcp_http: None,
            chat_runs: Arc::new(crate::stream_buffer::RunRegistry::new()),
        }
    }

    /// Minimal in-memory AppState for unit tests.
    #[cfg(test)]
    pub async fn new_for_tests() -> Self {
        use ironjaw_brain::{Brain, BrainConfig, BrainStore, Embedder};
        use ironjaw_proactive::{ProactiveConfig, ProactiveEngine, ProactiveStore};

        let audit = ironjaw_audit::AuditStore::open_memory().await.unwrap();
        let sessions = ironjaw_session::SessionStore::open_memory().await.unwrap();
        let llm = Arc::new(ironjaw_llm::LlmRouter::empty());
        let store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let brain = Arc::new(Brain::new(store, embedder, BrainConfig::default()));
        let proactive_store = ProactiveStore::open_memory().await.unwrap();
        let proactive = ProactiveEngine::new(proactive_store, ProactiveConfig::default());
        let voice = ironjaw_voice::VoiceEngine::mock();
        Self::new(
            "test-token".to_string(),
            audit,
            llm,
            sessions,
            32768,
            brain,
            proactive,
            voice,
            ironjaw_vision::ImageProcessor::new(10_485_760),
        )
    }
}

#[cfg(test)]
mod observability_counter_tests {
    use super::*;
    use crate::circuit_breaker::{CircuitBreakerRegistry, Metrics};

    fn make_obs() -> ObservabilityState {
        ObservabilityState {
            metrics: Arc::new(Metrics::new()),
            circuit_breakers: Arc::new(CircuitBreakerRegistry::new(5, 60)),
            counters: Arc::new(DashMap::new()),
        }
    }

    #[test]
    fn inc_starts_at_zero_then_increments() {
        let obs = make_obs();
        assert_eq!(obs.get_counter("hits"), 0);
        obs.inc("hits");
        assert_eq!(obs.get_counter("hits"), 1);
        obs.inc("hits");
        assert_eq!(obs.get_counter("hits"), 2);
    }

    #[test]
    fn inc_by_adds_delta() {
        let obs = make_obs();
        obs.inc_by("bytes", 100);
        obs.inc_by("bytes", 200);
        assert_eq!(obs.get_counter("bytes"), 300);
    }

    #[test]
    fn snapshot_counters_round_trips() {
        let obs = make_obs();
        obs.inc("a");
        obs.inc("a");
        obs.inc_by("b", 5);
        let snap = obs.snapshot_counters();
        assert_eq!(snap.get("a").copied(), Some(2));
        assert_eq!(snap.get("b").copied(), Some(5));
        assert!(!snap.contains_key("c"));
    }

    #[test]
    fn independent_keys_do_not_collide() {
        let obs = make_obs();
        obs.inc("x");
        obs.inc_by("y", 10);
        assert_eq!(obs.get_counter("x"), 1);
        assert_eq!(obs.get_counter("y"), 10);
    }

    #[test]
    fn clone_shares_counter_state() {
        let obs = make_obs();
        let obs2 = obs.clone();
        obs.inc("shared");
        assert_eq!(obs2.get_counter("shared"), 1);
    }
}
