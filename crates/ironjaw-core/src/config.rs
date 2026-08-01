use ironjaw_llm::ApiKeySource;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub audit: AuditConfig,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    #[allow(dead_code)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub brain: BrainConfig,
    #[serde(default)]
    pub integrations: ironjaw_integrations::IntegrationsConfig,
    #[serde(default)]
    pub proactive: ironjaw_proactive::ProactiveConfig,
    #[serde(default)]
    pub voice: ironjaw_voice::VoiceConfig,
    #[serde(default)]
    pub vision: ironjaw_vision::VisionConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub claude: ironjaw_claude::CcConfig,
    /// Skill definitions loaded from config
    #[serde(default)]
    pub skills: Vec<SkillConfig>,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub snapshots: SnapshotsConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub anti_distill: AntiDistillConfig,
    #[serde(default)]
    pub chat: ChatConfig,
    #[serde(default)]
    pub hooks: HooksConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// Enable OpenAI-compatible REST API endpoints (/v1/*)
    #[serde(default)]
    pub openai_compat: bool,
    /// How long to wait for the initial auth.connect message before closing the connection.
    #[serde(default = "default_auth_timeout_secs")]
    pub auth_timeout_secs: u64,
    /// Bearer token required by the native MCP endpoint (`POST /mcp`).
    ///
    /// Empty (the default) leaves the route unregistered: MCP over HTTP is
    /// opt-in and fail-closed, because the gateway binds `0.0.0.0` and the
    /// brain exposes destructive tools. Distinct from `[auth] token` so the
    /// public front door can carry it without granting gateway access.
    #[serde(default)]
    pub mcp_token: String,
}

fn default_auth_timeout_secs() -> u64 {
    30
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuditConfig {
    pub database_path: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LoggingConfig {
    pub level: String,
    #[serde(default = "default_format")]
    pub format: String,
}

fn default_format() -> String {
    "pretty".to_string()
}

fn default_timeout_secs() -> u64 {
    300
}

fn default_hitl_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SecurityConfig {
    #[serde(default = "default_true")]
    pub hitl_enabled: bool,
    #[serde(default = "default_hitl_timeout")]
    pub hitl_timeout_secs: u64,
    #[serde(default)]
    pub rate_limit_enabled: bool,
    #[serde(default = "default_sanitize_enabled")]
    pub sanitize_enabled: bool,
    /// Pattern-based permission rules
    #[serde(default)]
    pub rules: Vec<SecurityRuleConfig>,
    /// Background permission classifier
    #[serde(default)]
    pub classifier: ClassifierConfig,
}

fn default_sanitize_enabled() -> bool {
    true
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            hitl_enabled: true,
            hitl_timeout_secs: default_hitl_timeout(),
            rate_limit_enabled: false,
            sanitize_enabled: true,
            rules: Vec::new(),
            classifier: ClassifierConfig::default(),
        }
    }
}

/// A single permission rule entry from config.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SecurityRuleConfig {
    /// Rule scope: "global" or "project"
    #[serde(default = "default_rule_scope")]
    pub scope: String,
    /// Pattern to match (e.g. "shell(git *)")
    pub pattern: String,
    /// Decision: "allow" or "deny"
    pub decision: String,
}

fn default_rule_scope() -> String {
    "project".to_string()
}

/// Background permission pre-classifier config.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ClassifierConfig {
    #[serde(default)]
    pub enabled: bool,
    /// LLM model for pre-screening
    #[serde(default = "default_classifier_model")]
    pub model: String,
    /// Classifier timeout in milliseconds
    #[serde(default = "default_classifier_timeout")]
    pub timeout_ms: u64,
}

fn default_classifier_model() -> String {
    "qwen3:8b".to_string()
}

fn default_classifier_timeout() -> u64 {
    2000
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: default_classifier_model(),
            timeout_ms: default_classifier_timeout(),
        }
    }
}

fn default_working_dir() -> String {
    ".".to_string()
}

fn default_shell_timeout() -> u64 {
    30
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ToolsConfig {
    #[serde(default = "default_working_dir")]
    pub working_dir: String,
    #[serde(default = "default_shell_timeout")]
    pub shell_timeout_secs: u64,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            working_dir: default_working_dir(),
            shell_timeout_secs: default_shell_timeout(),
        }
    }
}

fn default_chronicle_poll() -> u64 {
    60
}

fn default_markdown_debounce() -> u64 {
    5
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct IndexersConfig {
    #[serde(default)]
    pub chronicle: ChronicleIndexerConfig,
    #[serde(default)]
    pub markdown: MarkdownIndexerConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChronicleIndexerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub db_path: String,
    #[serde(default = "default_chronicle_poll")]
    pub poll_interval_secs: u64,
}

impl Default for ChronicleIndexerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            db_path: String::new(),
            poll_interval_secs: default_chronicle_poll(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarkdownIndexerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub watch_dirs: Vec<String>,
    #[serde(default = "default_markdown_debounce")]
    pub debounce_secs: u64,
}

impl Default for MarkdownIndexerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            watch_dirs: Vec::new(),
            debounce_secs: default_markdown_debounce(),
        }
    }
}

fn default_brain_db_path() -> String {
    "data/brain.db".to_string()
}

fn default_embed_url() -> String {
    "http://localhost:8080".to_string()
}

fn default_embed_model() -> String {
    "nomic-embed-text".to_string()
}

fn default_embedding_backend() -> String {
    "http".to_string()
}

fn default_recall_top_k() -> usize {
    3
}

fn default_max_memories() -> usize {
    10_000
}

fn default_brain_files_dir() -> String {
    "config/brain".to_string()
}

fn default_layer1_budget_tokens() -> usize {
    600
}

fn default_memory_relevance_threshold() -> f32 {
    0.3
}

fn default_learn_dedup_threshold() -> f32 {
    0.85
}

fn default_learn_after_turns() -> usize {
    3
}

fn default_learn_use_llm() -> bool {
    true
}

fn default_learn_novelty_threshold() -> f32 {
    0.15
}

fn default_consolidation_interval_hours() -> u64 {
    6
}

fn default_salience_hot_days() -> u64 {
    7
}

fn default_salience_warm_days() -> u64 {
    30
}

fn default_salience_boost_per_access() -> f64 {
    0.02
}

fn default_true() -> bool {
    true
}

fn default_context_var_max_tokens() -> usize {
    4000
}

fn default_auto_approve_confidence() -> f32 {
    0.9
}

fn default_sleep_batch() -> usize {
    20
}

fn default_pattern_days() -> u64 {
    30
}

fn default_freshness_window_secs() -> u64 {
    300
}

fn default_live_state_confidence_threshold() -> f32 {
    0.7
}

fn default_live_state_decay_rate() -> f32 {
    0.01
}

/// Configuration for live-state query detection and stale-memory filtering (Phase 10).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct BrainLiveStateConfig {
    /// Master killswitch. When false, Phase 10 behaviour is entirely bypassed
    /// and the system behaves exactly as it did before Phase 10.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// When true, skip brain injection entirely on live-state queries instead of
    /// filtering to ToolVerified-only memories.
    #[serde(default)]
    pub skip_on_live_state: bool,
    /// Memories older than this many seconds are treated as stale (dropped) on live-state
    /// queries, unless their trust_level >= ToolVerified.
    #[serde(default = "default_freshness_window_secs")]
    pub freshness_window_secs: u64,
    /// Minimum classifier confidence to trigger live-state filtering.
    #[serde(default = "default_live_state_confidence_threshold")]
    pub confidence_threshold: f32,
    /// Reserved for Phase 18: LLM-based fallback classifier. Must remain false for now.
    #[serde(default)]
    pub llm_fallback: bool,
    /// Exponential decay rate (per hour) applied to time-sensitive memory types
    /// in composite_score when live_state_active is true.
    #[serde(default = "default_live_state_decay_rate")]
    pub decay_rate: f32,
}

impl Default for BrainLiveStateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            skip_on_live_state: false,
            freshness_window_secs: default_freshness_window_secs(),
            confidence_threshold: default_live_state_confidence_threshold(),
            llm_fallback: false,
            decay_rate: default_live_state_decay_rate(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrainConfig {
    #[serde(default = "default_brain_db_path")]
    pub database_path: String,
    #[serde(default = "default_embed_url")]
    pub embedding_url: String,
    #[serde(default = "default_embed_model")]
    pub embedding_model: String,
    /// Embedding backend: "http" (llama-server) or "fastembed" (local ONNX)
    #[serde(default = "default_embedding_backend")]
    pub embedding_backend: String,
    #[serde(default = "default_recall_top_k")]
    pub auto_recall_top_k: usize,
    #[serde(default = "default_max_memories")]
    pub max_memories: usize,
    #[serde(default)]
    pub enabled: bool,
    /// Directory containing brain layer files (identity.md, user.md, knowledge.md)
    #[serde(default = "default_brain_files_dir")]
    pub brain_files_dir: String,
    /// Token budget for Layer 1-2 sections in local LLM context
    #[serde(default = "default_layer1_budget_tokens")]
    pub layer1_budget_tokens: usize,
    /// Minimum similarity score for memory recall (0.0-1.0)
    #[serde(default = "default_memory_relevance_threshold")]
    pub memory_relevance_threshold: f32,
    /// Enable auto-learning from conversations
    #[serde(default)]
    pub auto_learn: bool,
    /// Embedding similarity threshold for deduplication during learning
    #[serde(default = "default_learn_dedup_threshold")]
    pub learn_dedup_threshold: f32,
    /// Trigger learning after this many conversation turns
    #[serde(default = "default_learn_after_turns")]
    pub learn_after_turns: usize,
    /// Use LLM to extract and classify learned facts
    #[serde(default = "default_learn_use_llm")]
    pub learn_use_llm: bool,
    /// Minimum novelty score (0.0-1.0) for a fact to be stored
    #[serde(default = "default_learn_novelty_threshold")]
    pub learn_novelty_threshold: f32,
    /// Hours between automatic consolidation runs
    #[serde(default = "default_consolidation_interval_hours")]
    pub consolidation_interval_hours: u64,
    /// Days threshold for "hot" salience tier (high decay protection)
    #[serde(default = "default_salience_hot_days")]
    pub salience_hot_days: u64,
    /// Days threshold for "warm" salience tier (moderate decay)
    #[serde(default = "default_salience_warm_days")]
    pub salience_warm_days: u64,
    /// Strength boost applied per memory access during recall
    #[serde(default = "default_salience_boost_per_access")]
    pub salience_boost_per_access: f64,
    /// Enable automatic session summary generation
    #[serde(default = "default_true")]
    pub session_summary_enabled: bool,
    /// Generate session summary on context compaction
    #[serde(default = "default_true")]
    pub session_summary_on_compact: bool,
    /// Generate session summary on session switch
    #[serde(default = "default_true")]
    pub session_summary_on_switch: bool,
    /// Max token budget for context variable injection
    #[serde(default = "default_context_var_max_tokens")]
    pub context_var_max_tokens: usize,
    /// Enable automatic entity extraction on remember()
    #[serde(default = "default_true")]
    pub auto_entity_extraction: bool,
    /// Enable LLM-powered sleep-time maintenance agents
    #[serde(default = "default_true")]
    pub sleep_agents_enabled: bool,
    /// Enable per-message extraction pipeline (LLM extracts facts from every response)
    /// DISABLED by default: extraction has no HITL gate and can store hallucinations.
    #[serde(default)]
    pub extraction_enabled: bool,
    /// Minimum confidence for auto-approving extracted facts (skip pending queue)
    #[serde(default = "default_auto_approve_confidence")]
    pub auto_approve_confidence: f32,
    /// Max memories to process per sleep-agent batch
    #[serde(default = "default_sleep_batch")]
    pub sleep_agents_batch_size: usize,
    /// Number of days of session summaries to analyze for pattern detection
    #[serde(default = "default_pattern_days")]
    pub sleep_agents_pattern_days: u64,
    /// Indexer configuration (chronicle + markdown watchers)
    #[serde(default)]
    pub indexers: IndexersConfig,
    /// Auto-inject brain memories before LLM routing
    #[serde(default)]
    pub auto_inject: AutoInjectConfig,
    /// Keep system prompt prefix static for KV cache reuse.
    /// Dynamic context (memories, keyword sections) moves to User role.
    #[serde(default)]
    pub stable_prefix: bool,
    /// Dream consolidation configuration
    #[serde(default)]
    pub dream: DreamConfig,
    /// Live-state query detection and stale-memory filtering (Phase 10)
    #[serde(default)]
    pub live_state: BrainLiveStateConfig,
}

/// Configuration for automatic brain memory injection before LLM routing.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AutoInjectConfig {
    /// Enable auto-injection of brain memories into chat context
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum number of memories to inject per chat.send
    #[serde(default = "default_auto_inject_max_memories")]
    pub max_memories: usize,
    /// Minimum composite score threshold (0.0-1.0) for injection
    #[serde(default = "default_auto_inject_min_score")]
    pub min_score: f32,
    /// Maximum characters of brain context to inject
    #[serde(default = "default_auto_inject_context_budget")]
    pub max_context_budget: usize,
    /// Skip injection during voice mode (latency sensitive)
    #[serde(default = "default_true")]
    pub exclude_voice: bool,
    /// Maximum milliseconds to wait for brain context before proceeding without it.
    /// Set to 0 to disable timeout (wait indefinitely).
    #[serde(default = "default_auto_inject_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for AutoInjectConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_memories: default_auto_inject_max_memories(),
            min_score: default_auto_inject_min_score(),
            max_context_budget: default_auto_inject_context_budget(),
            exclude_voice: true,
            timeout_ms: default_auto_inject_timeout_ms(),
        }
    }
}

fn default_auto_inject_max_memories() -> usize {
    5
}

fn default_auto_inject_min_score() -> f32 {
    0.3
}

fn default_auto_inject_context_budget() -> usize {
    2000
}

fn default_auto_inject_timeout_ms() -> u64 {
    150
}

/// Dream consolidation (sleep-agent) configuration.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DreamConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Minimum hours between dream runs
    #[serde(default = "default_dream_min_interval")]
    pub min_interval_hours: u64,
    /// Minimum sessions before consolidation is worthwhile
    #[serde(default = "default_dream_min_sessions")]
    pub min_sessions: u64,
}

fn default_dream_min_interval() -> u64 {
    24
}

fn default_dream_min_sessions() -> u64 {
    5
}

impl Default for DreamConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_interval_hours: default_dream_min_interval(),
            min_sessions: default_dream_min_sessions(),
        }
    }
}

impl Default for BrainConfig {
    fn default() -> Self {
        Self {
            database_path: default_brain_db_path(),
            embedding_url: default_embed_url(),
            embedding_model: default_embed_model(),
            embedding_backend: default_embedding_backend(),
            auto_recall_top_k: default_recall_top_k(),
            max_memories: default_max_memories(),
            enabled: false,
            brain_files_dir: default_brain_files_dir(),
            layer1_budget_tokens: default_layer1_budget_tokens(),
            memory_relevance_threshold: default_memory_relevance_threshold(),
            auto_learn: false,
            learn_dedup_threshold: default_learn_dedup_threshold(),
            learn_after_turns: default_learn_after_turns(),
            learn_use_llm: default_learn_use_llm(),
            learn_novelty_threshold: default_learn_novelty_threshold(),
            consolidation_interval_hours: default_consolidation_interval_hours(),
            salience_hot_days: default_salience_hot_days(),
            salience_warm_days: default_salience_warm_days(),
            salience_boost_per_access: default_salience_boost_per_access(),
            session_summary_enabled: true,
            session_summary_on_compact: true,
            session_summary_on_switch: true,
            context_var_max_tokens: default_context_var_max_tokens(),
            auto_entity_extraction: true,
            sleep_agents_enabled: true,
            extraction_enabled: false,
            auto_approve_confidence: default_auto_approve_confidence(),
            sleep_agents_batch_size: default_sleep_batch(),
            sleep_agents_pattern_days: default_pattern_days(),
            indexers: IndexersConfig::default(),
            auto_inject: AutoInjectConfig::default(),
            stable_prefix: false,
            dream: DreamConfig::default(),
            live_state: BrainLiveStateConfig::default(),
        }
    }
}

fn default_session_db_path() -> String {
    "data/sessions.db".to_string()
}

fn default_context_token_limit() -> u64 {
    32768
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionConfig {
    #[serde(default = "default_session_db_path")]
    pub database_path: String,
    #[serde(default = "default_context_token_limit")]
    pub context_token_limit: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            database_path: default_session_db_path(),
            context_token_limit: default_context_token_limit(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LlmConfig {
    pub default_model: Option<String>,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub routing: RoutingConfig,
    /// Model aliases: short name → full provider/model string (e.g. "coder" → "llama-server/qwen3-coder-abliterated")
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    /// Cloud model catalogs — models available from cloud providers (Anthropic, etc.)
    #[serde(default)]
    pub cloud_models: Vec<CloudModelCatalog>,
}

/// A cloud model catalog entry for a specific provider.
#[derive(Debug, Clone, Deserialize)]
pub struct CloudModelCatalog {
    pub provider: String,
    pub models: Vec<CloudModelEntry>,
}

/// A single cloud model definition.
#[derive(Debug, Clone, Deserialize)]
pub struct CloudModelEntry {
    pub id: String,
    #[serde(default)]
    pub description: String,
}

/// Resolve a model name through aliases. Returns the alias target if found, otherwise the original name.
/// Note: runtime alias resolution uses `ironjaw_ws::skills::resolve_alias` to avoid circular deps.
#[cfg(test)]
pub fn resolve_alias(aliases: &HashMap<String, String>, name: &str) -> String {
    aliases
        .get(name)
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

/// Configuration for a skill loaded from TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default = "default_skill_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub fallback_chain: Vec<String>,
    #[serde(default)]
    pub cloud_models: Vec<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
}

fn default_skill_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RoutingConfig {
    /// Model override for simple requests (short messages, no tools)
    pub simple_model: Option<String>,
    /// Model override for medium complexity requests
    pub medium_model: Option<String>,
    /// Model override for complex requests (many messages, tool calls, large prompts)
    pub complex_model: Option<String>,
    /// Enable complexity-based routing (default: false)
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub base_url: String,
    pub api_key: Option<String>,
    /// Environment variable name to read API key from (e.g. "ANTHROPIC_API_KEY").
    /// Resolved at startup; takes precedence over `api_key` if set.
    pub api_key_env: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Extra HTTP headers to include on every request to this provider.
    /// Useful for OpenRouter (HTTP-Referer, X-Title) or other proxies.
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
}

impl ProviderConfig {
    /// Resolve the API key: env var → direct config value → Claude OAuth token.
    ///
    /// For the `"anthropic"` provider, falls back to the Claude Code OAuth
    /// token at `~/.claude/.credentials.json` (used by Claude Max subscriptions).
    pub fn resolve_api_key(&self) -> Option<String> {
        // 1. Environment variable
        if let Some(env_var) = &self.api_key_env {
            if let Ok(val) = std::env::var(env_var) {
                if !val.is_empty() {
                    return Some(val);
                }
            }
        }
        // 2. Direct value in config
        if self.api_key.is_some() {
            return self.api_key.clone();
        }
        // 3. Claude OAuth credentials for anthropic provider (Max subscription)
        if self.provider_type == "anthropic" {
            if let Some(token) = Self::read_claude_oauth_token() {
                return Some(token);
            }
        }
        None
    }

    /// Read the OAuth access token from Claude Code's credentials file.
    fn read_claude_oauth_token() -> Option<String> {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .ok()?;
        let path = std::path::Path::new(&home)
            .join(".claude")
            .join(".credentials.json");
        let data = std::fs::read_to_string(path).ok()?;
        let json: serde_json::Value = serde_json::from_str(&data).ok()?;
        json.get("claudeAiOauth")?
            .get("accessToken")?
            .as_str()
            .map(|s| s.to_string())
    }

    /// Resolve the API key source for providers that support dynamic resolution.
    ///
    /// Returns an `ApiKeySource` that tells the provider HOW to get its key:
    /// - `Static` for env vars and direct config values (resolved once)
    /// - `OAuthFile` for Anthropic OAuth (re-read on every request)
    pub fn api_key_source(&self) -> Option<ApiKeySource> {
        // 1. Environment variable → Static
        if let Some(env_var) = &self.api_key_env {
            if let Ok(val) = std::env::var(env_var) {
                if !val.is_empty() {
                    return Some(ApiKeySource::Static { key: val });
                }
            }
        }
        // 2. Direct value in config → Static
        if let Some(key) = &self.api_key {
            return Some(ApiKeySource::Static { key: key.clone() });
        }
        // 3. Claude OAuth credentials for anthropic provider → OAuthFile
        if self.provider_type == "anthropic" {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .ok()?;
            let credentials_path = std::path::Path::new(&home)
                .join(".claude")
                .join(".credentials.json");
            if credentials_path.exists() {
                return Some(ApiKeySource::OAuthFile { credentials_path });
            }
        }
        None
    }
}

/// Context compaction configuration.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ContextConfig {
    /// Token threshold for micro-compaction trigger
    #[serde(default = "default_micro_compact_threshold")]
    pub micro_compact_threshold_tokens: usize,
    /// Max consecutive compaction failures before circuit breaker trips
    #[serde(default = "default_auto_compact_max_failures")]
    pub auto_compact_max_failures: u32,
    /// Max entries in the collapse undo registry
    #[serde(default = "default_collapse_max_entries")]
    pub collapse_registry_max_entries: usize,
}

fn default_micro_compact_threshold() -> usize {
    1000
}

fn default_auto_compact_max_failures() -> u32 {
    3
}

fn default_collapse_max_entries() -> usize {
    50
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            micro_compact_threshold_tokens: default_micro_compact_threshold(),
            auto_compact_max_failures: default_auto_compact_max_failures(),
            collapse_registry_max_entries: default_collapse_max_entries(),
        }
    }
}

/// Prompt cache configuration.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CacheConfig {
    /// Max entries in prompt cache LRU
    #[serde(default = "default_prompt_cache_max")]
    pub prompt_cache_max_entries: usize,
    /// Static/dynamic split marker for prompt caching
    #[serde(default = "default_prompt_boundary")]
    pub prompt_boundary: String,
}

fn default_prompt_cache_max() -> usize {
    10
}

fn default_prompt_boundary() -> String {
    "__PROMPT_BOUNDARY__".to_string()
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            prompt_cache_max_entries: default_prompt_cache_max(),
            prompt_boundary: default_prompt_boundary(),
        }
    }
}

/// File snapshot/checkpoint configuration.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SnapshotsConfig {
    /// Max number of snapshot files to retain
    #[serde(default = "default_snapshot_max_files")]
    pub max_files: usize,
    /// Max total bytes for snapshot storage
    #[serde(default = "default_snapshot_max_bytes")]
    pub max_bytes: usize,
}

fn default_snapshot_max_files() -> usize {
    100
}

fn default_snapshot_max_bytes() -> usize {
    26_214_400 // 25MB
}

impl Default for SnapshotsConfig {
    fn default() -> Self {
        Self {
            max_files: default_snapshot_max_files(),
            max_bytes: default_snapshot_max_bytes(),
        }
    }
}

/// Proactive daemon configuration.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DaemonConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Heartbeat interval in seconds
    #[serde(default = "default_daemon_heartbeat")]
    pub heartbeat_secs: u64,
    /// Max seconds for proactive budget per heartbeat
    #[serde(default = "default_daemon_budget")]
    pub max_proactive_budget_secs: u64,
    /// Directory for daemon log files
    #[serde(default = "default_daemon_log_dir")]
    pub log_dir: String,
}

fn default_daemon_heartbeat() -> u64 {
    300
}

fn default_daemon_budget() -> u64 {
    15
}

fn default_daemon_log_dir() -> String {
    "data/proactive".to_string()
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            heartbeat_secs: default_daemon_heartbeat(),
            max_proactive_budget_secs: default_daemon_budget(),
            log_dir: default_daemon_log_dir(),
        }
    }
}

/// Agent orchestration configuration.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AgentsConfig {
    /// Max agent nesting depth
    #[serde(default = "default_agent_max_depth")]
    pub max_depth: u32,
    /// Max concurrent agents
    #[serde(default = "default_agent_max_concurrent")]
    pub max_concurrent: u32,
    /// Base directory for worktree isolation
    #[serde(default = "default_worktree_base")]
    pub worktree_base: String,
}

fn default_agent_max_depth() -> u32 {
    3
}

fn default_agent_max_concurrent() -> u32 {
    5
}

fn default_worktree_base() -> String {
    "data/worktrees".to_string()
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            max_depth: default_agent_max_depth(),
            max_concurrent: default_agent_max_concurrent(),
            worktree_base: default_worktree_base(),
        }
    }
}

/// Anti-distillation defense configuration.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AntiDistillConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Number of fake tools to generate
    #[serde(default = "default_fake_tool_count")]
    pub fake_tool_count: u32,
    /// Seed for deterministic fake tool generation
    #[serde(default = "default_anti_distill_seed")]
    pub seed: u64,
}

fn default_fake_tool_count() -> u32 {
    3
}

fn default_anti_distill_seed() -> u64 {
    42
}

impl Default for AntiDistillConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fake_tool_count: default_fake_tool_count(),
            seed: default_anti_distill_seed(),
        }
    }
}

// ---------------------------------------------------------------------------
// Chat / Agentic loop config
// ---------------------------------------------------------------------------

fn default_nudger_enabled() -> bool {
    true
}

fn default_nudger_max_retries() -> usize {
    2
}

fn default_agentic_max_iterations() -> usize {
    5
}

fn default_agentic_tool_timeout_secs() -> u64 {
    30
}

fn default_run_token_ceiling() -> u64 {
    0
}

fn default_tool_result_max_chars() -> usize {
    12_288
}

fn default_turn_deadline_secs() -> u64 {
    360
}

/// Configuration for the agentic tool-call loop inside `chat.send`.
///
/// When `enabled = false` (the default) the handler behaves exactly as before
/// Phase 4: a single LLM pass with no tool execution.  Set `enabled = true`
/// only after end-to-end acceptance testing passes (the flag is the killswitch
/// that guards the entire agentic feature).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ChatAgenticConfig {
    /// Master on/off switch.  Defaults to `false` so the old path is preserved.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum number of LLM↔tool round-trips before forcing a synthesis pass.
    #[serde(default = "default_agentic_max_iterations")]
    pub max_iterations: usize,
    /// Per-tool execution timeout in seconds.
    #[serde(default = "default_agentic_tool_timeout_secs")]
    pub tool_timeout_secs: u64,
    /// Phase 5: re-prompt the LLM to use a tool when it emits prose-only despite
    /// having tools available.
    #[serde(default = "default_nudger_enabled")]
    pub nudger_enabled: bool,
    /// Phase 5: maximum nudge retries per user turn before accepting the prose.
    #[serde(default = "default_nudger_max_retries")]
    pub nudger_max_retries: usize,
    /// Phase 6: when `true`, a user who approves a HITL gate with
    /// `scope = "session"` will not be prompted again for the same tool within
    /// the same session.  Defaults to `true`.
    #[serde(default = "default_hitl_session_cache_enabled")]
    pub hitl_session_cache_enabled: bool,
    /// Phase 11: when `true`, successful tool executions that return a
    /// `MemoryHint` will write a `ToolVerified` memory to the brain so future
    /// `chat.send` calls get the fresh answer via auto-inject.  Defaults to `true`.
    #[serde(default = "default_tool_memory_writeback_enabled")]
    pub tool_memory_writeback_enabled: bool,
    /// A2: hard ceiling on cumulative (prompt + completion) tokens across all
    /// iterations of one `chat.send` turn.  `0` disables the ceiling.  When
    /// exceeded, the loop stops cleanly at the top of the next iteration and
    /// persists an error-style assistant message instead of re-prompting.
    #[serde(default = "default_run_token_ceiling")]
    pub run_token_ceiling: u64,
    /// Cap on the characters of a single tool result fed back into the LLM
    /// context (and persisted to the session).  Uncapped results (82KB
    /// cc_chronicle dumps, 66KB read_file payloads) blow up per-iteration
    /// prefill time and exhaust `run_token_ceiling`.  `0` disables the cap.
    #[serde(default = "default_tool_result_max_chars")]
    pub tool_result_max_chars: usize,
    /// G2: minimum characters (trimmed) for a final agentic answer.  A shorter
    /// answer triggers ONE re-prompt for a full answer; the second answer is
    /// accepted as-is.  `0` disables the guard.
    #[serde(default = "default_min_answer_chars")]
    pub min_answer_chars: usize,
    /// FIX #2b: per-turn wall-clock deadline in seconds.  Checked at the top of
    /// each agentic iteration; on expiry the loop breaks to the forced-synthesis
    /// pass so a slow/wedged local model can never hang a turn indefinitely.
    /// `0` disables the deadline.
    #[serde(default = "default_turn_deadline_secs")]
    pub turn_deadline_secs: u64,
}

fn default_min_answer_chars() -> usize {
    20
}

fn default_hitl_session_cache_enabled() -> bool {
    true
}

fn default_tool_memory_writeback_enabled() -> bool {
    true
}

impl Default for ChatAgenticConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_iterations: default_agentic_max_iterations(),
            tool_timeout_secs: default_agentic_tool_timeout_secs(),
            nudger_enabled: default_nudger_enabled(),
            nudger_max_retries: default_nudger_max_retries(),
            hitl_session_cache_enabled: default_hitl_session_cache_enabled(),
            tool_memory_writeback_enabled: default_tool_memory_writeback_enabled(),
            run_token_ceiling: default_run_token_ceiling(),
            tool_result_max_chars: default_tool_result_max_chars(),
            min_answer_chars: default_min_answer_chars(),
            turn_deadline_secs: default_turn_deadline_secs(),
        }
    }
}

fn default_local_system_prompt_file() -> String {
    "config/prompts/local_agentic.md".to_string()
}

fn default_local_tools() -> Vec<String> {
    [
        "read_file",
        "list_files",
        "grep",
        "write_file",
        "apply_patch",
        "shell",
        "powershell",
        "web_search",
        "process_list",
        "whoami",
        "todo",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// `[chat.local]` — behavioral system prompt + curated tool set for LOCAL
/// (llama-server) models only; cloud models are unaffected. When `enabled`,
/// `chat.send` prepends the contents of `system_prompt_file` as the first
/// system message (re-read per turn so edits apply live) and advertises only
/// `tools` instead of the full registry. Motivation: local models get no
/// behavioral prompt today, so the agentic loop relies on runtime band-aids.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ChatLocalConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_local_system_prompt_file")]
    pub system_prompt_file: String,
    #[serde(default = "default_local_tools")]
    pub tools: Vec<String>,
}

impl Default for ChatLocalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            system_prompt_file: default_local_system_prompt_file(),
            tools: default_local_tools(),
        }
    }
}

/// Top-level `[chat]` config section.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct ChatConfig {
    #[serde(default)]
    pub agentic: ChatAgenticConfig,
    #[serde(default)]
    pub local: ChatLocalConfig,
}

/// Configuration for MCP (Model Context Protocol) server connections.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfigEntry>,
}

/// A single MCP server entry in `[[mcp.servers]]`.
///
/// The `transport` tag selects the variant:
///
/// ```toml
/// [[mcp.servers]]
/// name    = "claude-peers"
/// transport = "stdio"
/// command = "bun"
/// args    = ["run", "/path/to/claude-peers-mcp.ts"]
///
/// [[mcp.servers]]
/// name      = "my-http-server"
/// transport = "http"
/// url       = "http://localhost:3000/mcp"
/// api_key   = "sk-..."
/// ```
///
/// For backward compatibility, entries that omit the `transport` key but
/// include a `url` field are deserialized as the `Http` variant by providing
/// a default via `#[serde(default = "default_transport")]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpServerConfigEntry {
    /// Stdio-based MCP server (spawn a subprocess).
    Stdio {
        /// Logical name used as the tool prefix (e.g. `"claude-peers"` →
        /// `mcp__claude-peers__<tool>`).
        name: String,
        /// Executable to launch (e.g. `"bun"`, `"python"`).
        command: String,
        /// Command-line arguments passed after `command`.
        #[serde(default)]
        args: Vec<String>,
    },
    /// HTTP-based MCP server.
    Http {
        /// Logical name for the server.
        name: String,
        /// Full URL of the MCP endpoint (e.g. `"http://localhost:3000/mcp"`).
        url: String,
        /// Optional Bearer token.
        api_key: Option<String>,
    },
}

impl McpServerConfigEntry {
    /// Returns the logical server name regardless of transport variant.
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio { name, .. } => name,
            Self::Http { name, .. } => name,
        }
    }
}

/// Resolve `${ENV_VAR}` placeholders in a string with actual environment variable values.
/// If the env var is not set, the placeholder is left as-is (preserving literal fallback values).
fn resolve_env_vars(s: &str) -> String {
    let re = regex::Regex::new(r"\$\{([^}]+)\}").expect("valid regex");
    re.replace_all(s, |caps: &regex::Captures| {
        std::env::var(&caps[1]).unwrap_or_else(|_| caps[0].to_string())
    })
    .to_string()
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&content)?;
        config.resolve_placeholders();
        Ok(config)
    }

    /// Post-processing: resolve `${ENV_VAR}` placeholders in string config values.
    /// Also checks `IRONJAW_AUTH_TOKEN` env var as an explicit override for auth.token.
    fn resolve_placeholders(&mut self) {
        // First resolve any ${...} placeholders in the token
        self.auth.token = resolve_env_vars(&self.auth.token);
        // Then check explicit env var override (takes precedence even over hardcoded values)
        if let Ok(val) = std::env::var("IRONJAW_AUTH_TOKEN") {
            if !val.is_empty() {
                self.auth.token = val;
            }
        }
        // MCP endpoint token: same placeholder + env-override treatment as auth.token,
        // so the live value never has to sit in a committed config file.
        self.server.mcp_token = resolve_env_vars(&self.server.mcp_token);
        if let Ok(val) = std::env::var("IRONJAW_MCP_TOKEN") {
            if !val.is_empty() {
                self.server.mcp_token = val;
            }
        }
        // An unresolved placeholder means the env var is missing. Blank it rather than
        // registering /mcp behind a literal "${...}" — the endpoint must fail closed.
        if self.server.mcp_token.contains("${") {
            tracing::warn!(
                "[server] mcp_token placeholder did not resolve; POST /mcp will not be registered"
            );
            self.server.mcp_token = String::new();
        }
        self.audit.database_path = resolve_env_vars(&self.audit.database_path);
        self.server.host = resolve_env_vars(&self.server.host);
        self.logging.level = resolve_env_vars(&self.logging.level);
        self.session.database_path = resolve_env_vars(&self.session.database_path);
        self.brain.database_path = resolve_env_vars(&self.brain.database_path);
        self.brain.embedding_url = resolve_env_vars(&self.brain.embedding_url);
        self.brain.embedding_model = resolve_env_vars(&self.brain.embedding_model);
        self.brain.brain_files_dir = resolve_env_vars(&self.brain.brain_files_dir);
        self.tools.working_dir = resolve_env_vars(&self.tools.working_dir);

        // Resolve provider API keys and base URLs
        for provider in &mut self.llm.providers {
            provider.base_url = resolve_env_vars(&provider.base_url);
            if let Some(ref key) = provider.api_key {
                provider.api_key = Some(resolve_env_vars(key));
            }
        }

        // Resolve MCP server URLs, commands, and keys
        for server in &mut self.mcp.servers {
            match server {
                McpServerConfigEntry::Http { url, api_key, .. } => {
                    *url = resolve_env_vars(url);
                    if let Some(ref k) = *api_key {
                        *api_key = Some(resolve_env_vars(k));
                    }
                }
                McpServerConfigEntry::Stdio { command, args, .. } => {
                    *command = resolve_env_vars(command);
                    for arg in args.iter_mut() {
                        *arg = resolve_env_vars(arg);
                    }
                }
            }
        }

        // Resolve hooks dir env vars (e.g. $HOME / ~)
        self.hooks.dir = resolve_env_vars(&self.hooks.dir);
    }
}

fn default_hooks_dir() -> String {
    "~/.config/ironjaw/hooks".to_string()
}

fn default_hook_timeout_secs() -> u64 {
    5
}

fn default_hook_timeout_ceiling_secs() -> u64 {
    60
}

/// Top-level B2 hook framework configuration.
///
/// `enabled = false` is the master killswitch — hook discovery is skipped at
/// startup and the registry stays empty. WASM hooks are the default execution
/// path; subprocess hooks require a second opt-in (`subprocess.enabled`) AND a
/// per-event allowlist (`subprocess.enabled_events`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct HooksConfig {
    /// Master killswitch. Default false.
    #[serde(default)]
    pub enabled: bool,
    /// Directory from which hooks are loaded. Default `~/.config/ironjaw/hooks`.
    /// Resolved against `$HOME`/`~` at startup.
    #[serde(default = "default_hooks_dir")]
    pub dir: String,
    /// Subprocess-specific opt-in gates. WASM hooks are always allowed when
    /// `enabled = true`.
    #[serde(default)]
    pub subprocess: HooksSubprocessConfig,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_hooks_dir(),
            subprocess: HooksSubprocessConfig::default(),
        }
    }
}

/// Subprocess hook gating. Two layers of opt-in: the global `enabled` flag,
/// and a per-event `enabled_events` allowlist. Both must pass for a subprocess
/// hook to fire.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct HooksSubprocessConfig {
    /// Whether subprocess hooks are allowed at all. Default false.
    #[serde(default)]
    pub enabled: bool,
    /// Per-hook execution timeout default in seconds. Clamped to
    /// `timeout_ceiling_secs`. Default 5s.
    #[serde(default = "default_hook_timeout_secs")]
    pub default_timeout_secs: u64,
    /// Hard upper bound on per-hook timeout overrides. Default 60s.
    #[serde(default = "default_hook_timeout_ceiling_secs")]
    pub timeout_ceiling_secs: u64,
    /// Allowlist of event names (e.g. `["SessionStart", "PreCompact"]`) that
    /// subprocess hooks are permitted to handle. Empty = subprocess hooks
    /// cannot fire even if `enabled = true`.
    #[serde(default)]
    pub enabled_events: Vec<String>,
}

impl Default for HooksSubprocessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_timeout_secs: default_hook_timeout_secs(),
            timeout_ceiling_secs: default_hook_timeout_ceiling_secs(),
            enabled_events: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_parse_valid_config() {
        let toml_content = r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "test-token-abc123"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "info"
format = "pretty"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 18794);
        assert_eq!(config.auth.token, "test-token-abc123");
        assert_eq!(config.audit.database_path, "data/ironjaw.db");
        assert_eq!(config.logging.level, "info");
    }

    #[test]
    fn test_parse_missing_field_fails() {
        let toml_content = r#"
[server]
host = "127.0.0.1"
"#;
        let result: Result<Config, _> = toml::from_str(toml_content);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_from_file() {
        let dir = std::env::temp_dir().join("ironjaw-test-config");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.toml");
        let mut file = std::fs::File::create(&path).unwrap();
        write!(
            file,
            r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "file-token"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "debug"
"#
        )
        .unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.auth.token, "file-token");
        assert_eq!(config.logging.format, "pretty"); // default

        // cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_parse_config_with_llm() {
        let toml_content = r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "test-token"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "info"

[llm]
default_model = "qwen3-coder"

[[llm.providers]]
name = "llama-server"
type = "llama-server"
base_url = "http://localhost:8080"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.llm.default_model, Some("qwen3-coder".to_string()));
        assert_eq!(config.llm.providers.len(), 1);
        assert_eq!(config.llm.providers[0].name, "llama-server");
        assert_eq!(config.llm.providers[0].provider_type, "llama-server");
        assert_eq!(config.llm.providers[0].timeout_secs, 300); // default
    }

    #[test]
    fn test_parse_config_without_llm_backward_compat() {
        let toml_content = r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "test-token"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "info"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.llm.default_model.is_none());
        assert!(config.llm.providers.is_empty());
    }

    #[test]
    fn test_parse_config_with_aliases() {
        let toml_content = r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "test-token"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "info"

[llm]
default_model = "qwen3-coder"

[llm.aliases]
coder = "llama-server/qwen3-coder-abliterated"
fast = "llama-server/qwen3:8b"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.llm.aliases.len(), 2);
        assert_eq!(
            config.llm.aliases.get("coder").unwrap(),
            "llama-server/qwen3-coder-abliterated"
        );
        assert_eq!(
            config.llm.aliases.get("fast").unwrap(),
            "llama-server/qwen3:8b"
        );
    }

    #[test]
    fn test_resolve_alias_found() {
        let mut aliases = HashMap::new();
        aliases.insert("coder".to_string(), "llama-server/qwen3-coder".to_string());
        assert_eq!(resolve_alias(&aliases, "coder"), "llama-server/qwen3-coder");
    }

    #[test]
    fn test_resolve_alias_not_found() {
        let aliases = HashMap::new();
        assert_eq!(resolve_alias(&aliases, "unknown"), "unknown");
    }

    #[test]
    fn test_parse_config_with_skills() {
        let toml_content = r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "test-token"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "info"

[[skills]]
name = "coder"
description = "Coding assistant"
tools = ["read_file", "write_file", "shell"]
model = "coder"
fallback_chain = ["fast"]
cloud_models = ["cloud"]
temperature = 0.3

[[skills]]
name = "researcher"
description = "Research assistant"
tools = ["web_search", "searxng"]
model = "brain"
enabled = false
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.skills.len(), 2);
        assert_eq!(config.skills[0].name, "coder");
        assert_eq!(config.skills[0].model, Some("coder".to_string()));
        assert_eq!(config.skills[0].fallback_chain, vec!["fast".to_string()]);
        assert_eq!(config.skills[0].cloud_models, vec!["cloud".to_string()]);
        assert_eq!(config.skills[0].temperature, Some(0.3));
        assert!(config.skills[0].enabled); // default true
        assert_eq!(config.skills[1].name, "researcher");
        assert!(!config.skills[1].enabled);
    }

    #[test]
    fn test_config_without_skills_backward_compat() {
        let toml_content = r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "test-token"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "info"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.skills.is_empty());
        assert!(config.llm.aliases.is_empty());
    }

    #[test]
    fn test_parse_config_with_cloud_models() {
        let toml_content = r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "test-token"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "info"

[[llm.cloud_models]]
provider = "anthropic"
models = [
    { id = "claude-sonnet-4-6", description = "Fast" },
    { id = "claude-opus-4-6", description = "Capable" },
]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.llm.cloud_models.len(), 1);
        assert_eq!(config.llm.cloud_models[0].provider, "anthropic");
        assert_eq!(config.llm.cloud_models[0].models.len(), 2);
        assert_eq!(config.llm.cloud_models[0].models[0].id, "claude-sonnet-4-6");
        assert_eq!(config.llm.cloud_models[0].models[1].description, "Capable");
    }

    #[test]
    fn test_config_without_cloud_models_backward_compat() {
        let toml_content = r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "test-token"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "info"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.llm.cloud_models.is_empty());
    }

    #[test]
    fn test_provider_config_api_key_env() {
        let toml_content = r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "test-token"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "info"

[[llm.providers]]
name = "anthropic"
type = "openai"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"
timeout_secs = 120
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.llm.providers.len(), 1);
        assert_eq!(
            config.llm.providers[0].api_key_env,
            Some("ANTHROPIC_API_KEY".to_string())
        );
        assert!(config.llm.providers[0].api_key.is_none());
    }

    #[test]
    fn test_resolve_api_key_from_env() {
        let provider = ProviderConfig {
            name: "test".to_string(),
            provider_type: "openai".to_string(),
            base_url: "http://localhost".to_string(),
            api_key: Some("direct-key".to_string()),
            api_key_env: Some("IRONJAW_TEST_API_KEY_12345".to_string()),
            timeout_secs: 30,
            extra_headers: HashMap::new(),
        };
        // No env var set → falls back to direct key
        assert_eq!(provider.resolve_api_key(), Some("direct-key".to_string()));

        // Set env var → takes precedence
        std::env::set_var("IRONJAW_TEST_API_KEY_12345", "env-key");
        assert_eq!(provider.resolve_api_key(), Some("env-key".to_string()));
        std::env::remove_var("IRONJAW_TEST_API_KEY_12345");
    }

    #[test]
    fn test_resolve_env_vars_basic() {
        std::env::set_var("IRONJAW_TEST_TOKEN_XYZ", "secret123");
        let result = resolve_env_vars("${IRONJAW_TEST_TOKEN_XYZ}");
        assert_eq!(result, "secret123");
        std::env::remove_var("IRONJAW_TEST_TOKEN_XYZ");
    }

    #[test]
    fn test_resolve_env_vars_missing_falls_back() {
        // Ensure the var does NOT exist
        std::env::remove_var("IRONJAW_NONEXISTENT_VAR_ABC");
        let result = resolve_env_vars("${IRONJAW_NONEXISTENT_VAR_ABC}");
        assert_eq!(result, "${IRONJAW_NONEXISTENT_VAR_ABC}");
    }

    #[test]
    fn test_resolve_env_vars_no_placeholder() {
        let result = resolve_env_vars("plain-token-value");
        assert_eq!(result, "plain-token-value");
    }

    #[test]
    fn test_resolve_env_vars_mixed() {
        std::env::set_var("IRONJAW_TEST_HOST_XYZ", "192.0.2.10");
        let result = resolve_env_vars("http://${IRONJAW_TEST_HOST_XYZ}:8080/api");
        assert_eq!(result, "http://192.0.2.10:8080/api");
        std::env::remove_var("IRONJAW_TEST_HOST_XYZ");
    }

    #[test]
    fn test_resolve_env_vars_multiple() {
        std::env::set_var("IRONJAW_TEST_A_XYZ", "alpha");
        std::env::set_var("IRONJAW_TEST_B_XYZ", "beta");
        let result = resolve_env_vars("${IRONJAW_TEST_A_XYZ}-${IRONJAW_TEST_B_XYZ}");
        assert_eq!(result, "alpha-beta");
        std::env::remove_var("IRONJAW_TEST_A_XYZ");
        std::env::remove_var("IRONJAW_TEST_B_XYZ");
    }

    #[test]
    fn test_config_load_resolves_auth_token() {
        let dir = std::env::temp_dir().join("ironjaw-test-env-resolve");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("env_test.toml");
        let mut file = std::fs::File::create(&path).unwrap();
        write!(
            file,
            r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "${{IRONJAW_TEST_LOAD_TOKEN}}"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "info"
"#
        )
        .unwrap();

        // With env var set, token should resolve
        std::env::set_var("IRONJAW_TEST_LOAD_TOKEN", "resolved-secret");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.auth.token, "resolved-secret");
        std::env::remove_var("IRONJAW_TEST_LOAD_TOKEN");

        // Without env var, token stays as literal placeholder
        let config2 = Config::load(&path).unwrap();
        assert_eq!(config2.auth.token, "${IRONJAW_TEST_LOAD_TOKEN}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_resolve_api_key_no_key() {
        let provider = ProviderConfig {
            name: "test".to_string(),
            provider_type: "openai".to_string(),
            base_url: "http://localhost".to_string(),
            api_key: None,
            api_key_env: None,
            timeout_secs: 30,
            extra_headers: HashMap::new(),
        };
        assert!(provider.resolve_api_key().is_none());
    }

    #[test]
    fn test_parse_config_with_new_sections() {
        let toml_content = r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "test-token"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "info"

[context]
micro_compact_threshold_tokens = 2000
auto_compact_max_failures = 5
collapse_registry_max_entries = 100

[cache]
prompt_cache_max_entries = 20
prompt_boundary = "__CUSTOM__"

[snapshots]
max_files = 50
max_bytes = 10485760

[[security.rules]]
scope = "global"
pattern = "shell(git *)"
decision = "allow"

[[security.rules]]
scope = "project"
pattern = "shell(rm *)"
decision = "deny"

[security.classifier]
enabled = true
model = "qwen3:1b"
timeout_ms = 1000

[daemon]
enabled = true
heartbeat_secs = 120
max_proactive_budget_secs = 30
log_dir = "data/daemon"

[brain.dream]
enabled = true
min_interval_hours = 12
min_sessions = 3

[agents]
max_depth = 5
max_concurrent = 10
worktree_base = "data/trees"

[anti_distill]
enabled = true
fake_tool_count = 5
seed = 99
"#;
        let config: Config = toml::from_str(toml_content).unwrap();

        // Context
        assert_eq!(config.context.micro_compact_threshold_tokens, 2000);
        assert_eq!(config.context.auto_compact_max_failures, 5);
        assert_eq!(config.context.collapse_registry_max_entries, 100);

        // Cache
        assert_eq!(config.cache.prompt_cache_max_entries, 20);
        assert_eq!(config.cache.prompt_boundary, "__CUSTOM__");

        // Snapshots
        assert_eq!(config.snapshots.max_files, 50);
        assert_eq!(config.snapshots.max_bytes, 10_485_760);

        // Security rules
        assert_eq!(config.security.rules.len(), 2);
        assert_eq!(config.security.rules[0].scope, "global");
        assert_eq!(config.security.rules[0].pattern, "shell(git *)");
        assert_eq!(config.security.rules[0].decision, "allow");
        assert_eq!(config.security.rules[1].decision, "deny");

        // Classifier
        assert!(config.security.classifier.enabled);
        assert_eq!(config.security.classifier.model, "qwen3:1b");
        assert_eq!(config.security.classifier.timeout_ms, 1000);

        // Daemon
        assert!(config.daemon.enabled);
        assert_eq!(config.daemon.heartbeat_secs, 120);
        assert_eq!(config.daemon.max_proactive_budget_secs, 30);
        assert_eq!(config.daemon.log_dir, "data/daemon");

        // Brain dream
        assert!(config.brain.dream.enabled);
        assert_eq!(config.brain.dream.min_interval_hours, 12);
        assert_eq!(config.brain.dream.min_sessions, 3);

        // Agents
        assert_eq!(config.agents.max_depth, 5);
        assert_eq!(config.agents.max_concurrent, 10);
        assert_eq!(config.agents.worktree_base, "data/trees");

        // Anti-distill
        assert!(config.anti_distill.enabled);
        assert_eq!(config.anti_distill.fake_tool_count, 5);
        assert_eq!(config.anti_distill.seed, 99);
    }

    #[test]
    fn test_new_sections_defaults() {
        let toml_content = r#"
[server]
host = "127.0.0.1"
port = 18794

[auth]
token = "test-token"

[audit]
database_path = "data/ironjaw.db"

[logging]
level = "info"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();

        // All new sections should have sane defaults
        assert_eq!(config.context.micro_compact_threshold_tokens, 1000);
        assert_eq!(config.context.auto_compact_max_failures, 3);
        assert_eq!(config.context.collapse_registry_max_entries, 50);

        assert_eq!(config.cache.prompt_cache_max_entries, 10);
        assert_eq!(config.cache.prompt_boundary, "__PROMPT_BOUNDARY__");

        assert_eq!(config.snapshots.max_files, 100);
        assert_eq!(config.snapshots.max_bytes, 26_214_400);

        assert!(config.security.rules.is_empty());
        assert!(!config.security.classifier.enabled);
        assert_eq!(config.security.classifier.model, "qwen3:8b");
        assert_eq!(config.security.classifier.timeout_ms, 2000);

        assert!(!config.daemon.enabled);
        assert_eq!(config.daemon.heartbeat_secs, 300);
        assert_eq!(config.daemon.max_proactive_budget_secs, 15);
        assert_eq!(config.daemon.log_dir, "data/proactive");

        assert!(!config.brain.dream.enabled);
        assert_eq!(config.brain.dream.min_interval_hours, 24);
        assert_eq!(config.brain.dream.min_sessions, 5);

        assert_eq!(config.agents.max_depth, 3);
        assert_eq!(config.agents.max_concurrent, 5);
        assert_eq!(config.agents.worktree_base, "data/worktrees");

        assert!(!config.anti_distill.enabled);
        assert_eq!(config.anti_distill.fake_tool_count, 3);
        assert_eq!(config.anti_distill.seed, 42);
    }

    #[test]
    fn brain_config_extraction_enabled_defaults_false() {
        let config = BrainConfig::default();
        assert!(
            !config.extraction_enabled,
            "extraction_enabled should default to false"
        );
    }
}
