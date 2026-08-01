use crate::embedder::Embedder;
use crate::graph::KnowledgeGraph;
use crate::hybrid_search::{filter_superseded, should_skip_retrieval, HybridSearch};
use crate::learner::{GateResult, LearnCandidate, WriteGate};
use crate::quality;
use crate::store::BrainStore;
use crate::types::{Memory, MemoryType, NewMemory, TrustLevel};
use chrono::Utc;
use ironjaw_llm::types::{ChatMessage, Role};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::RwLock;
use tracing::{debug, info, warn};

/// Rough token estimate: ~4 chars per token.
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

/// Token cost breakdown of the system prompt injected before user content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptStats {
    pub total_chars: usize,
    pub estimated_tokens: usize,
    pub layers: Vec<PromptLayerStats>,
}

/// Individual layer contribution to system prompt size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptLayerStats {
    pub name: String,
    pub chars: usize,
    pub estimated_tokens: usize,
}

/// Configuration for automatic brain memory injection before LLM routing.
#[derive(Debug, Clone)]
pub struct AutoInjectConfig {
    pub enabled: bool,
    pub max_memories: usize,
    pub min_score: f32,
    pub max_context_budget: usize,
    pub exclude_voice: bool,
    /// Maximum milliseconds to wait for brain context before proceeding without it.
    /// 0 = no timeout (wait indefinitely).
    pub timeout_ms: u64,
}

impl Default for AutoInjectConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_memories: 5,
            min_score: 0.3,
            max_context_budget: 2000,
            exclude_voice: true,
            timeout_ms: 150,
        }
    }
}

/// A memory paired with its composite relevance score.
#[derive(Debug, Clone)]
pub struct ScoredMemory {
    pub memory: Memory,
    pub score: f32,
}

/// Report from brain bootstrap operation.
#[derive(Debug, Clone, Default)]
pub struct BrainBootstrapReport {
    pub layers_loaded: usize,
    pub core_tokens: usize,
    pub total_tokens: usize,
}

/// Live-state filtering configuration (Phase 10).
///
/// Mirrors `ironjaw_core::config::BrainLiveStateConfig` — kept here to avoid a
/// circular crate dependency (ironjaw-core → ironjaw-brain → ironjaw-core).
#[derive(Debug, Clone)]
pub struct BrainLiveStateConfig {
    /// Master killswitch. When false, Phase 10 behaviour is entirely bypassed.
    pub enabled: bool,
    /// When true, skip brain injection entirely on live-state queries (stricter mode).
    pub skip_on_live_state: bool,
    /// Seconds after which a memory is considered stale for live-state queries.
    pub freshness_window_secs: u64,
    /// Minimum classifier confidence to trigger live-state filtering.
    pub confidence_threshold: f32,
    /// Whether an LLM fallback may be used for ambiguous queries (Phase 18).
    pub llm_fallback: bool,
    /// Exponential decay rate applied to time-sensitive memory types (per hour).
    pub decay_rate: f32,
}

impl Default for BrainLiveStateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            skip_on_live_state: false,
            freshness_window_secs: 300,
            confidence_threshold: 0.7,
            llm_fallback: false,
            decay_rate: 0.01,
        }
    }
}

/// Configuration for the Brain orchestrator
#[derive(Debug, Clone)]
pub struct BrainConfig {
    /// Max memories before LRU eviction
    pub max_memories: usize,
    /// Default top-K for recall
    pub auto_recall_top_k: usize,
    /// Directory containing brain layer files (identity.md, user.md, knowledge.md)
    pub brain_files_dir: String,
    /// Token budget for Layer 1-2 sections in local LLM context
    pub layer1_budget_tokens: usize,
    /// Minimum similarity score for memory recall (0.0-1.0)
    pub memory_relevance_threshold: f32,
    /// Enable auto-learning from conversations
    pub auto_learn: bool,
    /// Embedding similarity threshold for deduplication during learning
    pub learn_dedup_threshold: f32,
    /// Trigger learning after this many conversation turns
    pub learn_after_turns: usize,
    /// Use LLM to extract and classify learned facts
    pub learn_use_llm: bool,
    /// Minimum novelty score (0.0-1.0) for a fact to be stored
    pub learn_novelty_threshold: f32,
    /// Hours between automatic consolidation runs
    pub consolidation_interval_hours: u64,
    /// Strength boost applied per memory access during recall
    pub salience_boost_per_access: f64,
    /// Max token budget for context variable injection
    pub context_var_max_tokens: usize,
    /// Enable automatic entity extraction on remember()
    pub auto_entity_extraction: bool,
    /// Enable automatic session summary generation
    pub session_summary_enabled: bool,
    /// Generate session summary on context compaction
    pub session_summary_on_compact: bool,
    /// Generate session summary on session switch
    pub session_summary_on_switch: bool,
    /// Auto-inject configuration for chat.send memory injection
    pub auto_inject: AutoInjectConfig,
    /// When true, keep system prompt static (cacheable) and move dynamic
    /// context (memories, keyword sections, context vars) to a User-role
    /// message prefixed with [Context]. Enables llama-server KV cache reuse.
    pub stable_prefix: bool,
    /// Live-state query detection and stale-memory filtering (Phase 10).
    pub live_state: BrainLiveStateConfig,
}

impl Default for BrainConfig {
    fn default() -> Self {
        Self {
            max_memories: 10_000,
            auto_recall_top_k: 3,
            brain_files_dir: "config/brain".to_string(),
            layer1_budget_tokens: 600,
            memory_relevance_threshold: 0.3,
            auto_learn: true,
            learn_dedup_threshold: 0.85,
            learn_after_turns: 3,
            learn_use_llm: true,
            learn_novelty_threshold: 0.3,
            consolidation_interval_hours: 6,
            salience_boost_per_access: 0.02,
            context_var_max_tokens: 4000,
            auto_entity_extraction: true,
            session_summary_enabled: true,
            session_summary_on_compact: true,
            session_summary_on_switch: true,
            auto_inject: AutoInjectConfig::default(),
            stable_prefix: false,
            live_state: BrainLiveStateConfig::default(),
        }
    }
}

/// Brain orchestrator — ties BrainStore + Embedder together.
///
/// 3-layer memory model:
/// - Layer 1 (Identity): Always loaded from `identity` table
/// - Layer 2 (Working Memory): Per-session conversation context (handled by SessionStore)
/// - Layer 3 (Long-term): Vector store with semantic search
pub struct Brain {
    store: BrainStore,
    embedder: Embedder,
    config: BrainConfig,
    /// Runtime model/provider info injected into context (e.g. "llama-server / qwen3-coder-abliterated")
    model_info: RwLock<Option<String>>,
    /// Optional knowledge graph for auto entity extraction on remember()
    graph: Option<KnowledgeGraph>,
    /// Hybrid (vector + FTS5) search engine. Initialized lazily via `init_fts()`.
    hybrid: HybridSearch,
}

impl std::fmt::Debug for Brain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Brain {{ .. }}")
    }
}

impl Brain {
    pub fn new(store: BrainStore, embedder: Embedder, config: BrainConfig) -> Self {
        let hybrid = HybridSearch::with_threshold(
            store.clone(),
            embedder.clone(),
            config.memory_relevance_threshold,
        );
        Self {
            store,
            embedder,
            config,
            model_info: RwLock::new(None),
            graph: None,
            hybrid,
        }
    }

    /// Create a Brain with a KnowledgeGraph attached for auto entity extraction
    /// (write path) and graph-aware recall (the graph becomes a third RRF
    /// retriever in hybrid search).
    pub fn with_graph(
        store: BrainStore,
        embedder: Embedder,
        config: BrainConfig,
        graph: KnowledgeGraph,
    ) -> Self {
        let mut hybrid = HybridSearch::with_threshold(
            store.clone(),
            embedder.clone(),
            config.memory_relevance_threshold,
        );
        hybrid.set_graph(graph.clone());
        Self {
            store,
            embedder,
            config,
            model_info: RwLock::new(None),
            graph: Some(graph),
            hybrid,
        }
    }

    /// Initialize the FTS5 full-text search index for hybrid recall.
    ///
    /// Creates the `memories_fts` virtual table and sync triggers if they do not
    /// already exist. Non-fatal: if FTS setup fails the brain continues to operate
    /// in vector-only mode. Call once at startup after Brain construction.
    pub async fn init_fts(&self) -> anyhow::Result<()> {
        self.hybrid.init_fts().await?;
        Ok(())
    }

    /// Set the knowledge graph after construction. Also attaches it to hybrid
    /// search so recall uses the graph as a third RRF retriever.
    pub fn set_graph(&mut self, graph: KnowledgeGraph) {
        self.hybrid.set_graph(graph.clone());
        self.graph = Some(graph);
    }

    /// Set the runtime model/provider info injected into every context build.
    /// Call after LlmRouter is initialized, and on model switches.
    pub fn set_model_info(&self, info: String) {
        *self.model_info.write().unwrap() = Some(info);
    }

    /// Store a new memory, optionally embedding it. Evicts LRU if over capacity.
    ///
    /// This is the simple API — for full metadata control, use [`remember_full`].
    /// Runs quality gate (blocks score < 0.3) and novelty gate (dedup/supersession).
    /// Returns an error if the memory is rejected by either gate.
    pub async fn remember(
        &self,
        content: &str,
        source: Option<&str>,
        tags: Option<&str>,
    ) -> anyhow::Result<(Memory, bool)> {
        // Pre-filter: reject obviously low-value content before embedding
        if let Err(reason) = quality::should_ingest(content) {
            info!(
                reason,
                "brain.remember: content rejected by ingestion filter"
            );
            anyhow::bail!("memory rejected by ingestion filter: {reason}");
        }

        // Quality gate (advisory — always stores, but marks low-quality)
        let assessment = quality::assess_quality(content);
        if let Some(ref reason) = assessment.flag {
            info!(
                score = assessment.score,
                reason, "brain.remember: low-quality memory flagged"
            );
        }

        // Try to embed — graceful degradation if embedder fails
        let (_embedding, _embedded) = match self.embedder.embed(content).await {
            Ok(emb) => (Some(emb), true),
            Err(e) => {
                warn!(error = %e, "embedding failed, storing without vector");
                (None, false)
            }
        };

        let mut new_mem = NewMemory {
            content: content.to_string(),
            source: source.map(|s| s.to_string()),
            tags: tags.map(|t| t.to_string()),
            embedding: None,
            memory_type: None,
            importance: None,
            strength: None,
            supersedes: None,
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: None,
            reflection: None,
            trust_level: None,
            evidence_session_id: None,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: Some(assessment.score as f64),
            quality_flag: assessment.flag,
        };
        self.remember_inner(&mut new_mem).await
    }

    /// Store a new memory with full metadata control.
    ///
    /// Accepts a pre-built `NewMemory` (embedding will be computed automatically
    /// if not already set). All fields in `NewMemory` are respected, including
    /// `memory_type`, `confidence` (via `importance`), `trust_level`,
    /// `evidence_session_id`, `evidence_message_id`, `scope`, and `supersedes`.
    ///
    /// If `supersedes` is set, the old memory's `valid_to` is closed and its
    /// `superseded_by` back-link is set automatically by the store.
    ///
    /// Enforces quality gate (blocks score < 0.3) and novelty gate
    /// (dedup/supersession). Returns an error if rejected.
    pub async fn remember_full(&self, new_mem: &mut NewMemory) -> anyhow::Result<(Memory, bool)> {
        self.remember_inner(new_mem).await
    }

    /// Shared implementation for remember and remember_full.
    ///
    /// Enforces quality gate (blocks score < 0.3) and novelty gate
    /// (dedup/supersession via WriteGate) before storing.
    async fn remember_inner(&self, new_mem: &mut NewMemory) -> anyhow::Result<(Memory, bool)> {
        // Pre-filter: reject obviously low-value content before embedding
        if let Err(reason) = quality::should_ingest(&new_mem.content) {
            info!(
                reason,
                "brain.remember_inner: content rejected by ingestion filter"
            );
            anyhow::bail!("memory rejected by ingestion filter: {reason}");
        }

        // Quality gate: assess if not already scored by caller
        if new_mem.quality_score.is_none() {
            let assessment = quality::assess_quality(&new_mem.content);
            new_mem.quality_score = Some(assessment.score as f64);
            new_mem.quality_flag = assessment.flag;
        }

        // Block memories below quality threshold (trust-aware: ToolVerified
        // and SystemPolicy memories use 0.10; everything else 0.20).
        if let Some(score) = new_mem.quality_score {
            let threshold = match new_mem.trust_level {
                Some(TrustLevel::ToolVerified) | Some(TrustLevel::SystemPolicy) => 0.10,
                _ => 0.20,
            };
            if score < threshold {
                anyhow::bail!(
                    "memory rejected: quality score {:.2} below threshold {:.2} ({})",
                    score,
                    threshold,
                    new_mem.quality_flag.as_deref().unwrap_or("low quality")
                );
            }
        }

        // Novelty gate: check for duplicates/supersession. Skipped entirely
        // when the caller passed an explicit `supersedes` — that is a
        // caller-directed upsert, and the replacement content is expected to
        // be near-identical to the entry it replaces, so a similarity
        // rejection would wrongly veto legitimate updates (the memory-file
        // sync bug: small edits were "too similar" and never propagated).
        let content_hash = WriteGate::content_hash(&new_mem.content);
        new_mem.content_hash = Some(content_hash.clone());

        if new_mem.supersedes.is_none() {
            let candidate = LearnCandidate {
                content: new_mem.content.clone(),
                memory_type: new_mem
                    .memory_type
                    .as_deref()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(MemoryType::Fact),
                confidence: new_mem.importance.unwrap_or(0.5) as f32,
                tags: new_mem
                    .tags
                    .as_deref()
                    .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
                    .unwrap_or_default(),
                supersedes_hint: None,
                trust_level: new_mem.trust_level,
            };

            match WriteGate::gate(self, &self.store, &candidate, &content_hash).await? {
                GateResult::Skip(Some(blocking_id)) => {
                    anyhow::bail!(
                        "memory rejected: duplicate or too similar to existing (id={blocking_id})"
                    );
                }
                GateResult::Skip(None) => {
                    anyhow::bail!("memory rejected: duplicate or too similar to existing");
                }
                GateResult::Supersede(old_id) => {
                    new_mem.supersedes = Some(old_id);
                }
                GateResult::Store => {}
            }
        }

        // Try to embed — graceful degradation if embedder fails
        let embedded = if new_mem.embedding.is_none() {
            match self.embedder.embed(&new_mem.content).await {
                Ok(emb) => {
                    new_mem.embedding = Some(emb);
                    true
                }
                Err(e) => {
                    warn!(error = %e, "embedding failed, storing without vector");
                    false
                }
            }
        } else {
            true
        };

        let id = self.store.add_memory(new_mem).await?;

        // Evict LRU if over capacity
        let evicted = self.store.evict_lru(self.config.max_memories).await?;
        if evicted > 0 {
            info!(evicted, keep = self.config.max_memories, "LRU eviction");
        }

        let memory = self
            .store
            .get_memory(id)
            .await?
            .expect("just-inserted memory must exist");

        // Auto entity extraction: link entities from content to knowledge graph
        if self.config.auto_entity_extraction {
            if let Some(ref graph) = self.graph {
                if let Err(e) = graph.extract_and_link_entities(id, &new_mem.content).await {
                    warn!(
                        memory_id = id,
                        error = %e,
                        "entity extraction failed (non-fatal)"
                    );
                }
            }
        }

        Ok((memory, embedded))
    }

    /// Semantic search: embed query, KNN via VecIndex (or DB fallback), return top-K.
    ///
    /// Enhanced with:
    /// - **Retrieval gating**: skips search for trivially short queries.
    /// - **Supersession filtering**: replaces superseded memories with their latest version.
    ///
    /// Uses the in-memory VecIndex when available for fast search.
    /// Falls back to O(n) DB scan via `BrainStore::knn_search` otherwise.
    pub async fn recall(&self, query: &str, limit: Option<usize>) -> anyhow::Result<Vec<Memory>> {
        // Retrieval gating: skip for trivially short/single-word queries
        if should_skip_retrieval(query) {
            debug!(query, "recall: skipping retrieval (query too short)");
            return Ok(vec![]);
        }

        let top_k = limit.unwrap_or(self.config.auto_recall_top_k);

        // Hybrid search: vector KNN + FTS5 with RRF fusion and composite re-ranking.
        // HybridSearch::fts_search is defensive -- FTS errors degrade to vector-only.
        // update_accessed is called internally by search_with_reranking.
        let memories = self.hybrid.search_with_reranking(query, top_k).await?;

        // Supersession filtering: replace superseded memories with latest versions
        let memories = filter_superseded(memories, &self.store).await?;

        // Reinforce Ebbinghaus strength for recalled memories
        for mem in &memories {
            self.store
                .boost_strength(mem.id, self.config.salience_boost_per_access)
                .await?;
        }

        Ok(memories)
    }

    /// Check if a query uses procedural language ("how do I", "how to", "steps to", etc.)
    fn is_procedural_query(query: &str) -> bool {
        let lower = query.to_lowercase();
        lower.starts_with("how do i")
            || lower.starts_with("how to")
            || lower.starts_with("steps to")
            || lower.starts_with("how can i")
            || lower.starts_with("what are the steps")
            || lower.starts_with("walk me through")
            || lower.starts_with("show me how")
            || lower.contains("step by step")
            || lower.contains("procedure for")
            || lower.contains("workflow for")
    }

    /// Enhanced recall with scope filtering and procedural boosting.
    ///
    /// - `scope_filter`: if set, boost memories matching this scope prefix
    /// - Procedural queries automatically boost Procedure-type memories
    pub async fn recall_with_options(
        &self,
        query: &str,
        limit: Option<usize>,
        scope_filter: Option<&str>,
    ) -> anyhow::Result<Vec<Memory>> {
        let mut memories = self.recall(query, limit).await?;

        // I20: If scope_filter is set, pull in scope-matched memories and merge
        if let Some(scope) = scope_filter {
            let scoped = self.store.query_by_scope(scope, 10).await?;
            for mem in scoped {
                if !memories.iter().any(|m| m.id == mem.id) {
                    memories.push(mem);
                }
            }
        }

        // I19: Boost procedure memories for procedural queries
        if Self::is_procedural_query(query) {
            let procedures = self.store.query_procedures(scope_filter, 5).await?;
            for proc_mem in procedures {
                if !memories.iter().any(|m| m.id == proc_mem.id) {
                    memories.insert(0, proc_mem); // procedures first
                }
            }
        }

        // Re-sort: procedures first for procedural queries, then by importance
        if Self::is_procedural_query(query) {
            memories.sort_by(|a, b| {
                let a_is_proc = a.memory_type == "procedure";
                let b_is_proc = b.memory_type == "procedure";
                b_is_proc.cmp(&a_is_proc).then(
                    b.importance
                        .partial_cmp(&a.importance)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
            });
        }

        // Trim to requested limit
        let top_k = limit.unwrap_or(self.config.auto_recall_top_k);
        memories.truncate(top_k);

        Ok(memories)
    }

    /// Soft-delete a memory by ID.
    pub async fn forget(&self, memory_id: i64) -> anyhow::Result<bool> {
        self.store.delete_memory(memory_id).await
    }

    /// Get the auto-inject config.
    pub fn auto_inject_config(&self) -> &AutoInjectConfig {
        &self.config.auto_inject
    }

    /// Build context injection for LLM calls.
    ///
    /// Two modes based on `is_cloud`:
    /// - **Cloud** (large context): Full raw brain files + semantic memories
    /// - **Local** (token-budgeted): Compiled core + keyword-selected sections + memories
    ///
    /// Returns system messages to prepend to the LLM call.
    pub async fn build_context(
        &self,
        user_message: &str,
        is_cloud: bool,
    ) -> anyhow::Result<Vec<ChatMessage>> {
        if is_cloud {
            self.build_context_cloud(user_message).await
        } else {
            self.build_context_local(user_message).await
        }
    }

    /// Cloud path: inject full raw brain files + identity table + semantic memories.
    /// Cloud models have 200K+ context — use it all.
    async fn build_context_cloud(&self, user_message: &str) -> anyhow::Result<Vec<ChatMessage>> {
        let mut messages = Vec::new();
        let brain_dir = Path::new(&self.config.brain_files_dir);

        // Layer -1: Dynamic model/provider info
        if let Some(info) = self.model_info.read().unwrap().as_ref() {
            messages.push(ChatMessage {
                role: Role::System,
                content: info.clone(),
                images: None,
                ..Default::default()
            });
        }

        // Layer 0: Full identity.md
        if let Some(content) = Self::read_brain_file(brain_dir, "identity.md") {
            messages.push(ChatMessage {
                role: Role::System,
                content,
                images: None,
                ..Default::default()
            });
        }

        // Layer 1: Full user.md
        if let Some(content) = Self::read_brain_file(brain_dir, "user.md") {
            messages.push(ChatMessage {
                role: Role::System,
                content,
                images: None,
                ..Default::default()
            });
        }

        // Layer 2: Full knowledge.md
        if let Some(content) = Self::read_brain_file(brain_dir, "knowledge.md") {
            messages.push(ChatMessage {
                role: Role::System,
                content,
                images: None,
                ..Default::default()
            });
        }

        // Also include identity table entries (for backward compat / dynamic entries)
        let identity_entries = self.store.get_all_identity().await?;
        if !identity_entries.is_empty() {
            let identity_text = identity_entries
                .iter()
                .map(|e| format!("[{}]\n{}", e.key, e.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            messages.push(ChatMessage {
                role: Role::System,
                content: identity_text,
                images: None,
                ..Default::default()
            });
        }

        if self.config.stable_prefix {
            // Stable prefix mode: static layers stay System, dynamic as User
            let memory_text = self.format_memory_text(user_message).await?;
            let ctx_var_text = self.format_context_var_text(None).await?;

            let mut dynamic_parts = Vec::new();
            if !memory_text.is_empty() {
                dynamic_parts.push(memory_text);
            }
            if !ctx_var_text.is_empty() {
                dynamic_parts.push(ctx_var_text);
            }

            if !dynamic_parts.is_empty() {
                messages.push(ChatMessage {
                    role: Role::User,
                    content: format!("[Context]\n{}", dynamic_parts.join("\n\n")),
                    images: None,
                    ..Default::default()
                });
            }
        } else {
            // Legacy mode
            if !messages.is_empty() {
                messages.push(ChatMessage {
                    role: Role::System,
                    content: "__PROMPT_BOUNDARY__".to_string(),
                    images: None,
                    ..Default::default()
                });
            }

            // Layer 3: Semantic recall
            self.append_memory_context(&mut messages, user_message)
                .await?;

            // Layer 4: Loaded context variables
            self.append_context_vars(&mut messages, None).await?;
        }

        Ok(messages)
    }

    /// Local path: token-budgeted context assembly.
    /// 1. Compiled core (~400 tokens, always)
    /// 2. Keyword-selected sections from knowledge.md (~200-600 tokens)
    /// 3. Semantic recall from memories (~200 tokens)
    async fn build_context_local(&self, user_message: &str) -> anyhow::Result<Vec<ChatMessage>> {
        let mut messages = Vec::new();
        let brain_dir = Path::new(&self.config.brain_files_dir);

        // Layer -1: Dynamic model/provider info
        if let Some(info) = self.model_info.read().unwrap().as_ref() {
            messages.push(ChatMessage {
                role: Role::System,
                content: info.clone(),
                images: None,
                ..Default::default()
            });
        }

        // Layer 0: Compiled core (always loaded, ~400 tokens)
        let core_path = brain_dir.join("compiled").join("core.txt");
        if let Ok(content) = std::fs::read_to_string(&core_path) {
            if !content.is_empty() {
                messages.push(ChatMessage {
                    role: Role::System,
                    content,
                    images: None,
                    ..Default::default()
                });
                debug!("Brain: loaded compiled core");
            }
        }

        // Build keyword-selected section text (used in both modes)
        let selected = self.select_sections(user_message);
        let section_text = if !selected.is_empty() {
            let budget_chars = self.config.layer1_budget_tokens * 4; // ~4 chars per token
            let mut text = String::new();
            for section in &selected {
                if text.len() + section.len() > budget_chars {
                    debug!(
                        budget = budget_chars,
                        current = text.len(),
                        "Brain: section budget exceeded, stopping"
                    );
                    break;
                }
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(section);
            }
            text
        } else {
            String::new()
        };

        // Build identity table text
        let identity_entries = self.store.get_all_identity().await?;
        let identity_text = if !identity_entries.is_empty() {
            const LEGACY_BUDGET: usize = 4_000; // chars
            let mut text = String::new();
            for entry in &identity_entries {
                let fragment = format!("[{}]\n{}", entry.key, entry.content);
                if text.len() + fragment.len() > LEGACY_BUDGET {
                    break;
                }
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&fragment);
            }
            text
        } else {
            String::new()
        };

        if self.config.stable_prefix {
            // Stable prefix mode: static content as System, dynamic as User
            // Identity stays as System (rarely changes)
            if !identity_text.is_empty() {
                messages.push(ChatMessage {
                    role: Role::System,
                    content: identity_text,
                    images: None,
                    ..Default::default()
                });
            }

            // Bundle dynamic content into a single User-role message
            let ctx_budget = self.config.context_var_max_tokens;
            let memory_text = self.format_memory_text(user_message).await?;
            let ctx_var_text = self.format_context_var_text(Some(ctx_budget)).await?;

            let mut dynamic_parts = Vec::new();
            if !section_text.is_empty() {
                dynamic_parts.push(section_text);
            }
            if !memory_text.is_empty() {
                dynamic_parts.push(memory_text);
            }
            if !ctx_var_text.is_empty() {
                dynamic_parts.push(ctx_var_text);
            }

            if !dynamic_parts.is_empty() {
                messages.push(ChatMessage {
                    role: Role::User,
                    content: format!("[Context]\n{}", dynamic_parts.join("\n\n")),
                    images: None,
                    ..Default::default()
                });
            }
        } else {
            // Legacy mode: all content as System messages
            if !section_text.is_empty() {
                messages.push(ChatMessage {
                    role: Role::System,
                    content: section_text,
                    images: None,
                    ..Default::default()
                });
            }
            if !identity_text.is_empty() {
                messages.push(ChatMessage {
                    role: Role::System,
                    content: identity_text,
                    images: None,
                    ..Default::default()
                });
            }

            // --- Static/dynamic boundary for prompt cache ---
            if !messages.is_empty() {
                messages.push(ChatMessage {
                    role: Role::System,
                    content: "__PROMPT_BOUNDARY__".to_string(),
                    images: None,
                    ..Default::default()
                });
            }

            // Layer 3: Semantic recall
            self.append_memory_context(&mut messages, user_message)
                .await?;

            // Layer 4: Loaded context variables (token-budgeted for local models)
            let ctx_budget = self.config.context_var_max_tokens;
            self.append_context_vars(&mut messages, Some(ctx_budget))
                .await?;
        }

        Ok(messages)
    }

    /// Format recalled memories as a text string (for stable_prefix mode).
    async fn format_memory_text(&self, user_message: &str) -> anyhow::Result<String> {
        let memories = self
            .recall(user_message, Some(self.config.auto_recall_top_k))
            .await?;
        if memories.is_empty() {
            return Ok(String::new());
        }
        let memory_text = memories
            .iter()
            .map(|m| format!("- {}", m.content))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(format!("Relevant memories:\n{memory_text}"))
    }

    /// Format loaded context variables as a text string (for stable_prefix mode).
    async fn format_context_var_text(&self, token_budget: Option<usize>) -> anyhow::Result<String> {
        let vars = self.store.get_loaded_context_vars().await?;
        if vars.is_empty() {
            return Ok(String::new());
        }
        let char_budget = token_budget.map(|b| b * 4);
        let mut parts = Vec::new();
        let mut chars_used: usize = 0;
        for var in &vars {
            if let Some(budget) = char_budget {
                if chars_used + var.content.len() > budget {
                    continue;
                }
            }
            chars_used += var.content.len();
            parts.push(format!("[Context: {}]\n{}", var.name, var.content));
        }
        Ok(parts.join("\n\n"))
    }

    /// Append recalled memories as a system message.
    async fn append_memory_context(
        &self,
        messages: &mut Vec<ChatMessage>,
        user_message: &str,
    ) -> anyhow::Result<()> {
        let memories = self
            .recall(user_message, Some(self.config.auto_recall_top_k))
            .await?;
        if !memories.is_empty() {
            let memory_text = memories
                .iter()
                .map(|m| format!("- {}", m.content))
                .collect::<Vec<_>>()
                .join("\n");

            messages.push(ChatMessage {
                role: Role::System,
                content: format!("Relevant memories:\n{memory_text}"),
                images: None,
                ..Default::default()
            });
        }
        Ok(())
    }

    /// Append loaded context variables as system messages.
    ///
    /// When `token_budget` is `Some(budget)`, respects a character budget
    /// (budget * 4 chars). When `None`, includes all loaded vars.
    async fn append_context_vars(
        &self,
        messages: &mut Vec<ChatMessage>,
        token_budget: Option<usize>,
    ) -> anyhow::Result<()> {
        let vars = self.store.get_loaded_context_vars().await?;
        if vars.is_empty() {
            return Ok(());
        }

        let char_budget = token_budget.map(|b| b * 4);
        let mut chars_used: usize = 0;

        for var in &vars {
            if let Some(budget) = char_budget {
                if chars_used + var.content.len() > budget {
                    debug!(
                        name = %var.name,
                        budget,
                        used = chars_used,
                        "Brain: context var skipped (budget exceeded)"
                    );
                    continue;
                }
            }
            chars_used += var.content.len();
            messages.push(ChatMessage {
                role: Role::System,
                content: format!("[Context: {}]\n{}", var.name, var.content),
                images: None,
                ..Default::default()
            });
            debug!(name = %var.name, tokens = var.token_count, "Brain: injected context var");
        }
        Ok(())
    }

    /// Read a brain file from the brain_files_dir. Returns None if missing.
    fn read_brain_file(brain_dir: &Path, filename: &str) -> Option<String> {
        let path = brain_dir.join(filename);
        match std::fs::read_to_string(&path) {
            Ok(content) if !content.is_empty() => {
                debug!(file = filename, chars = content.len(), "Brain: loaded file");
                Some(content)
            }
            Ok(_) => None,
            Err(_) => {
                debug!(file = filename, "Brain: file not found, skipping");
                None
            }
        }
    }

    /// Keyword-based section selector for knowledge.md.
    /// Returns relevant sections based on trigger words in the user message.
    /// Zero cost, deterministic — no LLM or embedding call needed.
    fn select_sections(&self, user_message: &str) -> Vec<String> {
        let brain_dir = Path::new(&self.config.brain_files_dir);
        let knowledge = match Self::read_brain_file(brain_dir, "knowledge.md") {
            Some(k) => k,
            None => return vec![],
        };

        let msg_lower = user_message.to_lowercase();

        // Parse knowledge.md into sections by ## headers
        let sections = Self::parse_sections(&knowledge);

        let mut selected = Vec::new();

        // Keyword triggers → section names
        const TRIGGERS: &[(&[&str], &str)] = &[
            (
                &[
                    "docker-host",
                    "workstation",
                    "docker",
                    "ssh",
                    "service",
                    "server",
                    "port",
                    "caddy",
                    "nordvpn",
                    "deco",
                    "mesh",
                    "llama",
                ],
                "Infrastructure",
            ),
            (
                &[
                    "search",
                    "find",
                    "look up",
                    "research",
                    "searxng",
                    "perplexica",
                ],
                "Search",
            ),
            (
                &[
                    "code", "build", "test", "compile", "rust", "python", "cargo", "project",
                    "ironjaw", "crate",
                ],
                "Coding",
            ),
            (
                &["prefer", "always", "never", "style", "like", "hate", "want"],
                "user",
            ),
            (
                &[
                    "project",
                    "example-pipeline",
                    "quant",
                    "herby",
                    "chronicle",
                    "paint",
                    "mesh-monitor",
                ],
                "Projects",
            ),
        ];

        for &(keywords, section_name) in TRIGGERS {
            if keywords.iter().any(|kw| msg_lower.contains(kw)) {
                if let Some(content) = sections.get(section_name) {
                    selected.push(content.clone());
                }
                // Special case: "user" triggers loading user.md instead of a section
                if section_name == "user" {
                    if let Some(user_content) = Self::read_brain_file(brain_dir, "user.md") {
                        selected.push(user_content);
                    }
                }
            }
        }

        // Always include Mistakes if budget allows (lowest priority)
        if let Some(mistakes) = sections.get("Mistakes") {
            if !selected.iter().any(|s| s.contains("## Mistakes")) {
                selected.push(mistakes.clone());
            }
        }

        selected
    }

    /// Parse markdown into sections keyed by ## header name.
    fn parse_sections(content: &str) -> std::collections::HashMap<String, String> {
        let mut sections = std::collections::HashMap::new();
        let mut current_name = String::new();
        let mut current_content = String::new();

        for line in content.lines() {
            if let Some(header) = line.strip_prefix("## ") {
                // Save previous section
                if !current_name.is_empty() {
                    sections.insert(current_name.clone(), current_content.trim().to_string());
                }
                current_name = header.trim().to_string();
                current_content = format!("## {current_name}\n");
            } else {
                current_content.push_str(line);
                current_content.push('\n');
            }
        }

        // Save last section
        if !current_name.is_empty() {
            sections.insert(current_name, current_content.trim().to_string());
        }

        sections
    }

    /// Bootstrap brain from files on disk.
    /// Reads identity.md, user.md, knowledge.md and stores them in the identity table.
    /// Also reads compiled/core.txt and stores it.
    pub async fn bootstrap_from_files(&self) -> anyhow::Result<BrainBootstrapReport> {
        let brain_dir = Path::new(&self.config.brain_files_dir);
        let mut report = BrainBootstrapReport::default();

        let files = [
            ("identity", "identity.md"),
            ("user", "user.md"),
            ("knowledge", "knowledge.md"),
        ];

        for (key, filename) in &files {
            if let Some(content) = Self::read_brain_file(brain_dir, filename) {
                let tokens = estimate_tokens(&content);
                self.store.set_identity(key, &content).await?;
                info!(key, tokens, file = filename, "Brain: loaded layer file");
                report.layers_loaded += 1;
                report.total_tokens += tokens;
            }
        }

        // Compiled core
        let core_path = brain_dir.join("compiled").join("core.txt");
        if let Ok(content) = std::fs::read_to_string(&core_path) {
            if !content.is_empty() {
                let tokens = estimate_tokens(&content);
                self.store.set_identity("core", &content).await?;
                report.core_tokens = tokens;
                report.total_tokens += tokens;
                info!(tokens, "Brain: loaded compiled core");
            }
        }

        info!(
            layers = report.layers_loaded,
            core = report.core_tokens,
            total = report.total_tokens,
            "Brain bootstrap complete"
        );

        Ok(report)
    }

    /// Embed text using the configured embedder (exposed for learner dedup).
    pub async fn embed_text(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        self.embedder
            .embed(text)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Access the underlying store (for handler-level operations).
    pub fn store(&self) -> &BrainStore {
        &self.store
    }

    /// Access the embedder (for extraction pipeline similarity checks).
    pub fn embedder(&self) -> &Embedder {
        &self.embedder
    }

    /// Access the knowledge graph (for handler-level causal trace, etc.).
    pub fn graph(&self) -> Option<&KnowledgeGraph> {
        self.graph.as_ref()
    }

    /// Access the config (for handler-level operations).
    pub fn config(&self) -> &BrainConfig {
        &self.config
    }

    /// Re-embed all memories that have NULL embeddings.
    /// Returns (processed, failed) counts.
    pub async fn reembed_missing(&self) -> anyhow::Result<(usize, usize)> {
        let rows = self.store.query_unembedded_memories().await?;

        let total = rows.len();
        if total == 0 {
            info!("reembed_missing: no unembedded memories found");
            return Ok((0, 0));
        }

        info!(total, "reembed_missing: starting backfill");
        let mut processed = 0usize;
        let mut failed = 0usize;

        for (id, content) in rows {
            match self.embedder.embed(&content).await {
                Ok(emb) => {
                    if let Err(e) = self.store.update_embedding_and_vec(id, &emb).await {
                        warn!(id, error = %e, "reembed_missing: failed to store embedding");
                        failed += 1;
                    } else {
                        processed += 1;
                    }
                }
                Err(e) => {
                    warn!(id, error = %e, "reembed_missing: embedding failed");
                    failed += 1;
                }
            }

            if processed.is_multiple_of(100) && processed > 0 {
                info!(processed, total, "reembed_missing: progress");
            }
        }

        info!(processed, failed, total, "reembed_missing: complete");
        Ok((processed, failed))
    }

    /// Recall memories with composite relevance scores.
    ///
    /// Returns `ScoredMemory` entries sorted by composite score (descending).
    /// Uses the same KNN + composite_score pipeline as hybrid search but returns
    /// scores alongside memories for threshold filtering by callers.
    pub async fn recall_scored(
        &self,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<ScoredMemory>> {
        if should_skip_retrieval(query) {
            debug!(query, "recall_scored: skipping (query too short)");
            return Ok(vec![]);
        }

        // Hybrid search with scores: vector KNN + FTS5 with RRF fusion and composite re-ranking.
        // update_accessed is called internally by search_with_reranking_scored.
        let pairs = self
            .hybrid
            .search_with_reranking_scored(query, limit)
            .await?;
        if pairs.is_empty() {
            return Ok(vec![]);
        }

        let mut scored: Vec<ScoredMemory> = pairs
            .into_iter()
            .map(|(memory, score)| ScoredMemory { memory, score })
            .collect();

        // Filter superseded, preserving scores
        let memories: Vec<Memory> = scored.iter().map(|s| s.memory.clone()).collect();
        let filtered = filter_superseded(memories, &self.store).await?;
        let filtered_ids: std::collections::HashSet<i64> = filtered.iter().map(|m| m.id).collect();
        scored.retain(|s| filtered_ids.contains(&s.memory.id));

        // Boost strength for recalled memories
        for s in &scored {
            let _ = self
                .store
                .boost_strength(s.memory.id, self.config.salience_boost_per_access)
                .await;
            let _ = self.store.update_accessed(s.memory.id).await;
        }

        Ok(scored)
    }

    /// Parse an `accessed_at` string that may be in any of the formats written by
    /// the brain store:
    ///
    /// * `%Y-%m-%d %H:%M:%S` — legacy space-separated format (original SQLite default)
    /// * `%Y-%m-%dT%H:%M:%S` — ISO 8601 without sub-seconds
    /// * `%Y-%m-%dT%H:%M:%S%.f` — ISO 8601 with milliseconds (current store default via
    ///   `strftime('%Y-%m-%dT%H:%M:%f', 'now')`)
    /// * RFC 3339 with `Z` or numeric offset (e.g. `2026-04-22T10:30:00Z`)
    ///
    /// Returns `None` if none of the formats match, preserving the caller's
    /// `unwrap_or` fallback behaviour unchanged.
    ///
    /// # Phase 19 note
    /// `accessed_at` is currently written in ISO format by `store::update_accessed`
    /// (`strftime('%Y-%m-%dT%H:%M:%f', 'now')`) and by the migration default, so the
    /// space-separated format arm exists only for backward compat with any rows written
    /// before that change. Phase 19 should standardise all write paths to a single format
    /// and remove the legacy arm.
    #[allow(dead_code)]
    fn parse_accessed_at(s: &str) -> Option<chrono::NaiveDateTime> {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f"))
            .or_else(|_| {
                // RFC 3339 with Z or numeric offset — strip timezone, work in UTC
                chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.naive_utc())
            })
            .ok()
    }

    /// Build a brain context injection message for the client-managed chat path.
    ///
    /// Returns `(Some(system_message), injected_memories)` if relevant memories found,
    /// or `(None, vec![])` if auto-inject is disabled, no memories match, or on error.
    /// `injected_memories` contains exactly the `ScoredMemory` entries that fit within
    /// the context budget — callers can convert these to `MemoryRef` for the done frame.
    ///
    /// When `force_fresh` is `true`, brain injection is skipped entirely regardless
    /// of other configuration.
    pub async fn build_auto_inject_context(
        &self,
        user_message: &str,
        force_fresh: bool,
    ) -> anyhow::Result<(Option<ChatMessage>, Vec<ScoredMemory>)> {
        // force_fresh: caller explicitly wants no cached brain context
        if force_fresh {
            debug!("auto-inject: skipped (force_fresh=true)");
            return Ok((None, vec![]));
        }

        let cfg = &self.config.auto_inject;
        if !cfg.enabled {
            return Ok((None, vec![]));
        }

        let scored = self.recall_scored(user_message, cfg.max_memories).await?;
        if scored.is_empty() {
            return Ok((None, vec![]));
        }

        // Filter by minimum score threshold
        let mut above_threshold: Vec<&ScoredMemory> =
            scored.iter().filter(|s| s.score >= cfg.min_score).collect();
        if above_threshold.is_empty() {
            debug!("auto-inject: all memories below min_score threshold");
            return Ok((None, vec![]));
        }

        // Phase 10: live-state freshness filtering
        let ls_cfg = &self.config.live_state;
        if ls_cfg.enabled {
            use crate::live_state::LiveStateClassifier;
            use crate::types::TrustLevel;

            let cls = LiveStateClassifier::new();
            let result = cls.classify(user_message);

            if result.is_live && result.confidence >= ls_cfg.confidence_threshold {
                debug!(
                    confidence = result.confidence,
                    "auto-inject: live-state query detected"
                );

                if ls_cfg.skip_on_live_state {
                    debug!("auto-inject: skipped (skip_on_live_state=true)");
                    return Ok((None, vec![]));
                }

                // Filter stale memories: keep if fresh OR trust_level >= ToolVerified
                let now = Utc::now();
                let window_secs = ls_cfg.freshness_window_secs as i64;
                above_threshold.retain(|sm| {
                    // Always pass through high-trust memories
                    if sm.memory.trust_level >= TrustLevel::ToolVerified {
                        return true;
                    }
                    // Keep if accessed within freshness window
                    chrono::NaiveDateTime::parse_from_str(
                        &sm.memory.accessed_at,
                        "%Y-%m-%d %H:%M:%S",
                    )
                    .map(|dt| {
                        let age_secs = (now - dt.and_utc()).num_seconds();
                        age_secs <= window_secs
                    })
                    .unwrap_or(false) // if parse fails, treat as stale
                });

                debug!(
                    remaining = above_threshold.len(),
                    "auto-inject: after live-state freshness filter"
                );
            }
        }

        // Build the context block, respecting budget.
        // Track which memories actually fit so callers can surface them.
        let mut lines: Vec<String> = vec!["[Brain Context]".to_string()];
        let mut budget_used = lines[0].len();
        let mut injected: Vec<ScoredMemory> = Vec::new();

        for sm in &above_threshold {
            let line = format!(
                "- {} (type: {}, confidence: {:.2})",
                sm.memory.content, sm.memory.memory_type, sm.score
            );
            let line_tokens = estimate_tokens(&line);
            if budget_used + line_tokens > cfg.max_context_budget {
                debug!(
                    budget_used,
                    max = cfg.max_context_budget,
                    "auto-inject: context budget exhausted"
                );
                break;
            }
            budget_used += line_tokens;
            lines.push(line);
            injected.push(ScoredMemory {
                memory: sm.memory.clone(),
                score: sm.score,
            });
        }

        // If only the header line remains, nothing fit
        if lines.len() <= 1 {
            return Ok((None, vec![]));
        }

        let content = lines.join("\n");

        let role = if self.config.stable_prefix {
            Role::User
        } else {
            Role::System
        };
        let msg = ChatMessage {
            role,
            content,
            images: None,
            ..Default::default()
        };

        debug!(
            count = injected.len(),
            budget_used, "auto-inject: brain context built"
        );
        Ok((Some(msg), injected))
    }

    /// Measure the token cost of the current system prompt without sending a chat.
    pub async fn prompt_stats(
        &self,
        user_message: &str,
        is_cloud: bool,
    ) -> anyhow::Result<PromptStats> {
        let messages = self.build_context(user_message, is_cloud).await?;
        let mut layers = Vec::new();
        for msg in &messages {
            // Include both System and User-role context messages (stable_prefix mode
            // moves dynamic context to User role, but it's still prompt cost)
            if msg.role != Role::System && msg.role != Role::User {
                continue;
            }
            let chars = msg.content.len();
            let name = classify_prompt_layer(&msg.content);
            layers.push(PromptLayerStats {
                name,
                chars,
                estimated_tokens: chars.div_ceil(4), // ~4 chars per token estimate
            });
        }
        let total_chars: usize = layers.iter().map(|l| l.chars).sum();
        let estimated_tokens: usize = layers.iter().map(|l| l.estimated_tokens).sum();
        Ok(PromptStats {
            total_chars,
            estimated_tokens,
            layers,
        })
    }
}

/// Classify a system prompt message by its content to identify which layer it comes from.
/// Markers verified against what build_context_local/_cloud actually emit (2026-07-24) —
/// the earlier marker set matched none of them, so every layer reported as "other".
fn classify_prompt_layer(content: &str) -> String {
    if content.contains("__PROMPT_BOUNDARY__") {
        "prompt_cache_boundary".to_string()
    } else if content.contains("Your exact model is:") {
        "model_info".to_string()
    } else if content.starts_with("You are Dr. CLAW") {
        "core_prompt".to_string()
    } else if content.contains("# Identity") || content.contains("identity.md") {
        "identity".to_string()
    } else if content.contains("# User") || content.contains("user.md") {
        "user_profile".to_string()
    } else if content.contains("Relevant memories:") || content.contains("recall_score") {
        "memories".to_string()
    } else if content.contains("[Context:") || content.contains("context_var") {
        "context_variables".to_string()
    } else if content.starts_with('[') && content.contains("]\n") {
        // Identity table entries render as "[key]\ncontent" blocks
        "identity_table".to_string()
    } else if content.starts_with('#') || content.contains("# Knowledge") {
        // Keyword-selected knowledge sections are markdown-headed blocks
        "knowledge_sections".to_string()
    } else if content.len() < 200 {
        "system_core".to_string()
    } else {
        "other".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_brain() -> Brain {
        let store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let config = BrainConfig {
            max_memories: 100,
            auto_recall_top_k: 3,
            // Disable relevance threshold in tests — mock embeddings have arbitrary scores
            memory_relevance_threshold: 0.0,
            ..Default::default()
        };
        Brain::new(store, embedder, config)
    }

    fn upsert_mem(content: &str, supersedes: Option<i64>) -> NewMemory {
        NewMemory {
            content: content.to_string(),
            source: Some("memory/infra.md".to_string()),
            tags: Some("source:memory/infra.md,memory-file".to_string()),
            embedding: None,
            memory_type: Some("context".to_string()),
            importance: Some(0.7),
            strength: Some(1.0),
            supersedes,
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: None,
            reflection: None,
            trust_level: None,
            evidence_session_id: None,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: None,
            quality_flag: None,
        }
    }

    #[tokio::test]
    async fn test_remember_full_explicit_supersedes_bypasses_similarity() {
        let brain = test_brain().await;
        let (old, _) = brain
            .remember_full(&mut upsert_mem(
                "[memory/infra.md] The server runs on port 18794.",
                None,
            ))
            .await
            .unwrap();

        // Identical content without supersedes -> rejected, error names the blocker
        let err = brain
            .remember_full(&mut upsert_mem(
                "[memory/infra.md] The server runs on port 18794.",
                None,
            ))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("too similar"), "unexpected error: {msg}");
        assert!(
            msg.contains(&format!("(id={})", old.id)),
            "error should name the blocking id: {msg}"
        );

        // Same write WITH explicit supersedes -> upsert accepted, new entry stored
        let (updated, _) = brain
            .remember_full(&mut upsert_mem(
                "[memory/infra.md] The server runs on port 18794. Updated docs.",
                Some(old.id),
            ))
            .await
            .unwrap();
        assert_ne!(updated.id, old.id);
    }

    #[tokio::test]
    async fn test_remember_basic() {
        let brain = test_brain().await;
        let (mem, embedded) = brain
            .remember(
                "Ironjaw gateway runs on port 18794 with axum",
                Some("user"),
                Some("lang"),
            )
            .await
            .unwrap();

        assert!(embedded);
        assert_eq!(mem.content, "Ironjaw gateway runs on port 18794 with axum");
        assert_eq!(mem.source.as_deref(), Some("user"));
        assert!(mem.embedding.is_some());
    }

    #[tokio::test]
    async fn test_recall_finds_stored() {
        let brain = test_brain().await;
        brain
            .remember(
                "Ironjaw gateway runs on port 18794 with axum",
                Some("user"),
                None,
            )
            .await
            .unwrap();
        brain
            .remember(
                "Python Flask API runs on port 5000 with gunicorn",
                Some("user"),
                None,
            )
            .await
            .unwrap();

        let results = brain.recall("Ironjaw gateway", None).await.unwrap();
        assert!(!results.is_empty());
    }

    #[tokio::test]
    async fn test_recall_empty_store() {
        let brain = test_brain().await;
        let results = brain.recall("anything", None).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_recall_respects_limit() {
        let brain = test_brain().await;
        for i in 0..10 {
            brain
                .remember(
                    &format!("Server instance {i} runs on port {}", 8080 + i),
                    None,
                    None,
                )
                .await
                .unwrap();
        }

        // Query must be >= 2 words to pass retrieval gating
        let results = brain.recall("server instance", Some(2)).await.unwrap();
        assert_eq!(results.len(), 2);
    }

    // --- Graph-aware recall tests -------------------------------------------

    /// Build a Brain whose hybrid search has a KnowledgeGraph attached as the
    /// third RRF retriever. Returns the brain, a store handle (for inserting
    /// memories that bypass embedding), and the graph (for forging links).
    async fn graph_brain() -> (Brain, BrainStore, KnowledgeGraph) {
        let store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let config = BrainConfig {
            max_memories: 100,
            auto_recall_top_k: 3,
            memory_relevance_threshold: 0.0,
            ..Default::default()
        };
        let graph = KnowledgeGraph::init(store.pool().clone()).await.unwrap();
        let brain = Brain::with_graph(store.clone(), embedder, config, graph.clone());
        brain.init_fts().await.unwrap();
        (brain, store, graph)
    }

    /// Minimal NewMemory builder: content + optional embedding, rest defaulted.
    fn new_mem(content: &str, embedding: Option<Vec<f32>>) -> crate::types::NewMemory {
        crate::types::NewMemory {
            content: content.to_string(),
            source: None,
            tags: None,
            embedding,
            memory_type: None,
            importance: None,
            strength: None,
            supersedes: None,
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: Some("brain".to_string()),
            reflection: None,
            trust_level: None,
            evidence_session_id: None,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: None,
            quality_flag: None,
        }
    }

    #[tokio::test]
    async fn test_recall_surfaces_graph_linked_memory() {
        let (brain, store, graph) = graph_brain().await;

        // Memory B has NO embedding (vector search skips it) and content that
        // shares no terms with the query (FTS misses it). It is reachable ONLY
        // via the knowledge graph.
        let b_id = store
            .add_memory(&new_mem("xyzzy unrelated foobar widget", None))
            .await
            .unwrap();

        // Forge a graph link: memory B mentions the entity "Ironjaw Gateway".
        graph
            .extract_and_link_entities(b_id, "Ironjaw Gateway is connected to xyzzy.")
            .await
            .unwrap();

        let results = brain.recall("Ironjaw Gateway", Some(5)).await.unwrap();
        assert!(
            results.iter().any(|m| m.id == b_id),
            "graph-linked memory B should surface via the graph retriever even though \
             vector (no embedding) and FTS (no term overlap) both miss it"
        );
    }

    #[tokio::test]
    async fn test_recall_scored_graph_linked_memory() {
        let (brain, store, graph) = graph_brain().await;

        let b_id = store
            .add_memory(&new_mem("zzqq lorem ipsum dolor", None))
            .await
            .unwrap();
        graph
            .extract_and_link_entities(b_id, "Ironjaw Gateway powers zzqq.")
            .await
            .unwrap();

        let scored = brain.recall_scored("Ironjaw Gateway", 5).await.unwrap();
        let found = scored.iter().find(|s| s.memory.id == b_id);
        assert!(
            found.is_some(),
            "graph-linked memory should appear in scored recall"
        );
        assert!(
            found.unwrap().score > 0.0,
            "graph-sourced memory should have a non-zero score"
        );
    }

    #[tokio::test]
    async fn test_recall_single_word_entity_via_fallback() {
        let (brain, store, graph) = graph_brain().await;

        // No embedding, no "proxmox" term in stored content.
        let m_id = store
            .add_memory(&new_mem("hypervisor notes for the homelab", None))
            .await
            .unwrap();
        // Quoted single word creates a single-token entity "Proxmox".
        graph
            .extract_and_link_entities(m_id, "'Proxmox' is the hypervisor.")
            .await
            .unwrap();

        // Lowercase single-word query: extract_entity_candidates yields nothing,
        // so the whole-query fallback drives the match (COLLATE NOCASE).
        let results = brain.recall("proxmox", Some(5)).await.unwrap();
        assert!(
            results.iter().any(|m| m.id == m_id),
            "single-word query should match a known entity via the whole-query fallback"
        );
    }

    #[tokio::test]
    async fn test_recall_no_graph_backward_compat() {
        // Brain::new attaches no graph -> graph retriever is a no-op; recall
        // behaves exactly as before (vector + FTS only).
        let brain = test_brain().await;
        brain
            .remember(
                "Ironjaw gateway runs on port 18794 with axum",
                Some("user"),
                None,
            )
            .await
            .unwrap();
        let results = brain.recall("Ironjaw gateway", None).await.unwrap();
        assert!(
            !results.is_empty(),
            "graph-less recall must still find lexical/semantic hits"
        );
    }

    #[tokio::test]
    async fn test_forget() {
        let brain = test_brain().await;
        let (mem, _) = brain
            .remember(
                "Deprecated endpoint /api/v1/old runs on port 3000",
                None,
                None,
            )
            .await
            .unwrap();

        let forgotten = brain.forget(mem.id).await.unwrap();
        assert!(forgotten);

        // Should not appear in recall
        let results = brain.recall("deprecated endpoint", None).await.unwrap();
        assert!(
            results.iter().all(|m| m.id != mem.id),
            "forgotten memory should not appear in recall"
        );
    }

    #[tokio::test]
    async fn test_forget_nonexistent() {
        let brain = test_brain().await;
        let forgotten = brain.forget(999).await.unwrap();
        assert!(!forgotten);
    }

    #[tokio::test]
    async fn test_build_context_empty() {
        let brain = test_brain().await;
        // Local mode: no brain files, no identity, no memories → empty
        let ctx = brain.build_context("hello", false).await.unwrap();
        assert!(ctx.is_empty());
    }

    #[tokio::test]
    async fn test_build_context_with_identity() {
        let brain = test_brain().await;
        brain
            .store()
            .set_identity("personality", "I am helpful")
            .await
            .unwrap();

        // Local mode: identity table entries still loaded
        let ctx = brain.build_context("hello", false).await.unwrap();
        assert!(!ctx.is_empty());
        let has_identity = ctx.iter().any(|m| {
            matches!(m.role, Role::System)
                && m.content.contains("personality")
                && m.content.contains("I am helpful")
        });
        assert!(has_identity);
    }

    #[tokio::test]
    async fn test_build_context_with_memories() {
        let brain = test_brain().await;
        brain
            .remember(
                "The user prefers Rust v1.93 for all backend services",
                Some("user"),
                None,
            )
            .await
            .unwrap();

        let ctx = brain.build_context("What language?", false).await.unwrap();
        // Should have memory context (no identity set)
        let has_memories = ctx.iter().any(|m| {
            m.content.contains("Relevant memories:")
                && m.content.contains("The user prefers Rust v1.93")
        });
        assert!(has_memories);
    }

    #[tokio::test]
    async fn test_build_context_full() {
        let brain = test_brain().await;
        brain
            .store()
            .set_identity("personality", "I am Ironjaw")
            .await
            .unwrap();
        brain
            .remember(
                "User likes Rust v1.93 for systems programming on port 18794",
                None,
                None,
            )
            .await
            .unwrap();

        // Query must be >= 2 words to pass retrieval gating
        let ctx = brain
            .build_context("Rust programming", false)
            .await
            .unwrap();
        // Should have identity + memories
        let has_identity = ctx.iter().any(|m| m.content.contains("I am Ironjaw"));
        let has_memories = ctx
            .iter()
            .any(|m| m.content.contains("User likes Rust v1.93"));
        assert!(has_identity);
        assert!(has_memories);
    }

    #[tokio::test]
    async fn test_build_context_cloud_mode() {
        let brain = test_brain().await;
        brain
            .store()
            .set_identity("personality", "I am Dr. CLAW")
            .await
            .unwrap();

        // Cloud mode: includes identity table entries
        let ctx = brain.build_context("hello", true).await.unwrap();
        let has_identity = ctx.iter().any(|m| m.content.contains("I am Dr. CLAW"));
        assert!(has_identity);
    }

    #[test]
    fn test_parse_sections() {
        let content = "# Top\n## Section A\nLine 1\nLine 2\n## Section B\nLine 3\n";
        let sections = Brain::parse_sections(content);
        assert_eq!(sections.len(), 2);
        assert!(sections.contains_key("Section A"));
        assert!(sections.contains_key("Section B"));
        assert!(sections["Section A"].contains("Line 1"));
        assert!(sections["Section B"].contains("Line 3"));
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("1234"), 1);
        assert_eq!(estimate_tokens("12345678"), 2);
    }

    #[tokio::test]
    async fn test_eviction_on_remember() {
        let store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let config = BrainConfig {
            max_memories: 3, // very small cap
            auto_recall_top_k: 3,
            ..Default::default()
        };
        let brain = Brain::new(store, embedder, config);

        // Add 5 memories — should evict 2
        for i in 0..5 {
            brain
                .remember(
                    &format!("Service instance {i} runs on port {}", 9000 + i),
                    None,
                    None,
                )
                .await
                .unwrap();
        }

        let count = brain.store().count_memories().await.unwrap();
        assert_eq!(count, 3, "should have evicted down to max_memories");
    }

    #[tokio::test]
    async fn test_remember_full_with_metadata() {
        use crate::types::TrustLevel;

        let brain = test_brain().await;
        let mut new_mem = NewMemory {
            content: "User prefers dark mode in the /settings/theme config panel".to_string(),
            source: Some("user".to_string()),
            tags: Some("ui,preference".to_string()),
            embedding: None,
            memory_type: Some("preference".to_string()),
            importance: Some(0.85),
            strength: None,
            supersedes: None,
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: None,
            reflection: None,
            trust_level: Some(TrustLevel::UserStated),
            evidence_session_id: Some("sess-abc123".to_string()),
            evidence_message_id: Some("msg-def456".to_string()),
            scope: Some("/project/ironjaw/ui".to_string()),
            procedure: None,
            quality_score: None,
            quality_flag: None,
        };

        let (memory, embedded) = brain.remember_full(&mut new_mem).await.unwrap();

        assert!(embedded);
        assert_eq!(
            memory.content,
            "User prefers dark mode in the /settings/theme config panel"
        );
        assert_eq!(memory.memory_type, "preference");
        assert!((memory.importance - 0.85).abs() < 0.001);
        assert_eq!(memory.trust_level, TrustLevel::UserStated);
        assert_eq!(memory.evidence_session_id.as_deref(), Some("sess-abc123"));
        assert_eq!(memory.evidence_message_id.as_deref(), Some("msg-def456"));
        assert_eq!(memory.scope.as_deref(), Some("/project/ironjaw/ui"));
    }

    #[tokio::test]
    async fn test_remember_full_minimal_params() {
        let brain = test_brain().await;
        let mut new_mem = NewMemory {
            content: "Brain database is stored at /data/brain.db on port 18794".to_string(),
            source: None,
            tags: None,
            embedding: None,
            memory_type: None,
            importance: None,
            strength: None,
            supersedes: None,
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: None,
            reflection: None,
            trust_level: None,
            evidence_session_id: None,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: None,
            quality_flag: None,
        };

        let (memory, embedded) = brain.remember_full(&mut new_mem).await.unwrap();

        assert!(embedded);
        assert_eq!(
            memory.content,
            "Brain database is stored at /data/brain.db on port 18794"
        );
        // Defaults should be applied by the store
        assert_eq!(memory.trust_level, crate::types::TrustLevel::ModelInferred);
        assert!(memory.evidence_session_id.is_none());
        assert!(memory.scope.is_none());
    }

    #[tokio::test]
    async fn test_remember_full_supersedes() {
        let brain = test_brain().await;

        // Store the original memory
        let (original, _) = brain
            .remember(
                "Ironjaw server default port is 8080 in config",
                Some("config"),
                None,
            )
            .await
            .unwrap();

        assert!(
            original.valid_to.is_none(),
            "original should have no valid_to"
        );

        // Store a superseding memory
        let mut new_mem = NewMemory {
            content: "Ironjaw server port updated to 18794 in production config".to_string(),
            source: Some("config".to_string()),
            tags: None,
            embedding: None,
            memory_type: Some("fact".to_string()),
            importance: Some(0.5),
            strength: None,
            supersedes: Some(original.id),
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: None,
            reflection: None,
            trust_level: Some(crate::types::TrustLevel::ToolVerified),
            evidence_session_id: None,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: None,
            quality_flag: None,
        };

        let (new_memory, embedded) = brain.remember_full(&mut new_mem).await.unwrap();
        assert!(embedded);
        assert_eq!(
            new_memory.content,
            "Ironjaw server port updated to 18794 in production config"
        );
        assert_eq!(new_memory.supersedes, Some(original.id));

        // The original memory should now have valid_to set and superseded_by back-link
        let old_mem = brain
            .store()
            .get_memory(original.id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            old_mem.valid_to.is_some(),
            "original should have valid_to set after being superseded"
        );
        assert_eq!(
            old_mem.superseded_by,
            Some(new_memory.id),
            "original should have superseded_by pointing to new memory"
        );
    }

    #[tokio::test]
    async fn test_auto_inject_returns_context() {
        let store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let config = BrainConfig {
            max_memories: 100,
            auto_recall_top_k: 3,
            memory_relevance_threshold: 0.0,
            auto_inject: AutoInjectConfig {
                enabled: true,
                max_memories: 5,
                min_score: 0.0, // mock embeddings produce low composite scores
                max_context_budget: 2000,
                exclude_voice: true,
                timeout_ms: 150,
            },
            ..Default::default()
        };
        let brain = Brain::new(store, embedder, config);
        brain
            .remember(
                "Ironjaw gateway uses Rust v1.93 on port 18794",
                Some("user"),
                Some("lang"),
            )
            .await
            .unwrap();

        let (msg, count) = brain
            .build_auto_inject_context("What language is Ironjaw?", false)
            .await
            .unwrap();
        assert!(msg.is_some(), "should return a context message");
        assert!(!count.is_empty(), "should inject at least one memory");
        let msg = msg.unwrap();
        assert_eq!(msg.role, Role::System);
        assert!(msg.content.contains("[Brain Context]"));
        assert!(msg.content.contains("Ironjaw gateway uses Rust v1.93"));
    }

    #[tokio::test]
    async fn test_auto_inject_disabled() {
        let store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let config = BrainConfig {
            max_memories: 100,
            auto_recall_top_k: 3,
            memory_relevance_threshold: 0.0,
            auto_inject: AutoInjectConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let brain = Brain::new(store, embedder, config);
        brain
            .remember(
                "Critical config at /etc/ironjaw/production.toml port 18794",
                None,
                None,
            )
            .await
            .unwrap();

        let (msg, count) = brain
            .build_auto_inject_context("important", false)
            .await
            .unwrap();
        assert!(msg.is_none(), "should skip when disabled");
        assert!(count.is_empty());
    }

    #[tokio::test]
    async fn test_auto_inject_empty_query() {
        let brain = test_brain().await;

        // Empty string should be caught by should_skip_retrieval
        let (msg, count) = brain.build_auto_inject_context("", false).await.unwrap();
        assert!(msg.is_none(), "should skip for empty query");
        assert!(count.is_empty());
    }

    #[tokio::test]
    async fn test_auto_inject_no_memories() {
        let brain = test_brain().await;
        // No memories stored
        let (msg, count) = brain
            .build_auto_inject_context("anything at all", false)
            .await
            .unwrap();
        assert!(msg.is_none(), "should return None when no memories exist");
        assert!(count.is_empty());
    }

    #[tokio::test]
    async fn test_auto_inject_config_accessor() {
        let brain = test_brain().await;
        let cfg = brain.auto_inject_config();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_memories, 5);
        assert!((cfg.min_score - 0.3).abs() < 0.01);
        assert_eq!(cfg.max_context_budget, 2000);
    }

    #[test]
    fn test_classify_prompt_layer() {
        assert_eq!(classify_prompt_layer("# Identity\nI am an AI"), "identity");
        assert_eq!(
            classify_prompt_layer("Relevant memories:\n- fact 1"),
            "memories"
        );
        assert_eq!(classify_prompt_layer("Hello"), "system_core");
        assert_eq!(classify_prompt_layer(&"x".repeat(500)), "other");
    }

    #[test]
    fn test_prompt_layer_stats_token_estimate() {
        let stats = PromptLayerStats {
            name: "test".to_string(),
            chars: 400,
            estimated_tokens: 100,
        };
        // ~4 chars per token
        assert!((stats.estimated_tokens as f64 - stats.chars as f64 / 4.0).abs() < 2.0);
    }

    // ── Phase 10: live-state freshness tests ────────────────────────────────

    /// force_fresh=true must always return (None, 0) regardless of stored memories.
    #[tokio::test]
    async fn test_force_fresh_skips_injection() {
        let store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let config = BrainConfig {
            max_memories: 100,
            auto_recall_top_k: 5,
            memory_relevance_threshold: 0.0,
            ..Default::default()
        };
        let brain = Brain::new(store, embedder, config);
        brain
            .remember(
                "The active model is qwen3.5:27b running on llama-server port 8080",
                None,
                None,
            )
            .await
            .unwrap();

        let (msg, count) = brain
            .build_auto_inject_context("what model is running right now", true)
            .await
            .unwrap();
        assert!(msg.is_none(), "force_fresh should suppress all injection");
        assert!(count.is_empty());
    }

    /// skip_on_live_state=true must return (None, 0) when a live-state query is detected.
    #[tokio::test]
    async fn test_live_state_skip_on_live_query() {
        let store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let config = BrainConfig {
            max_memories: 100,
            auto_recall_top_k: 5,
            memory_relevance_threshold: 0.0,
            live_state: BrainLiveStateConfig {
                enabled: true,
                skip_on_live_state: true,
                freshness_window_secs: 300,
                confidence_threshold: 0.6,
                llm_fallback: false,
                decay_rate: 0.01,
            },
            ..Default::default()
        };
        let brain = Brain::new(store, embedder, config);
        brain
            .remember(
                "Current model is qwen3.5:27b running on llama-server port 8080",
                None,
                None,
            )
            .await
            .unwrap();

        // This query triggers live-state detection ("is X running")
        let (msg, count) = brain
            .build_auto_inject_context("is the model currently running", false)
            .await
            .unwrap();
        assert!(
            msg.is_none(),
            "skip_on_live_state should suppress injection on live queries"
        );
        assert!(count.is_empty());
    }

    /// ToolVerified memories must survive the freshness filter even during live-state queries.
    #[tokio::test]
    async fn test_tool_verified_survives_live_state_filter() {
        use crate::types::TrustLevel;

        let store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let config = BrainConfig {
            max_memories: 100,
            auto_recall_top_k: 5,
            memory_relevance_threshold: 0.0,
            live_state: BrainLiveStateConfig {
                enabled: true,
                skip_on_live_state: false,
                // Zero-second window: anything stored before "now" is stale
                freshness_window_secs: 0,
                confidence_threshold: 0.6,
                llm_fallback: false,
                decay_rate: 0.01,
            },
            ..Default::default()
        };
        let brain = Brain::new(store, embedder, config);

        // Store a ToolVerified memory — it must pass through the freshness filter
        let mut new_mem = crate::types::NewMemory {
            content: "Active model confirmed by model.status tool: qwen3.5:27b".to_string(),
            source: None,
            tags: None,
            embedding: None,
            memory_type: Some("fact".to_string()),
            importance: Some(1.0),
            strength: None,
            supersedes: None,
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: None,
            reflection: None,
            trust_level: Some(TrustLevel::ToolVerified),
            evidence_session_id: None,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: None,
            quality_flag: None,
        };
        brain.remember_full(&mut new_mem).await.unwrap();

        // Brief sleep to ensure accessed_at is definitely in the past for the 0-second window
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let (msg, count) = brain
            .build_auto_inject_context("what model is currently running", false)
            .await
            .unwrap();

        assert!(
            msg.is_some(),
            "ToolVerified memory should survive the freshness filter"
        );
        assert!(
            !count.is_empty(),
            "at least the ToolVerified memory should be injected"
        );
        let content = msg.unwrap().content;
        assert!(
            content.contains("qwen3.5:27b"),
            "injected context should contain the ToolVerified fact"
        );
    }

    /// Phase 11 regression: `remember_full` with `trust_level = ToolVerified` must persist
    /// the memory with the correct trust level so that tool-memory writeback facts survive
    /// round-trips through the store and are retrievable with the expected metadata.
    #[tokio::test]
    async fn live_state_query_returns_tool_verified_memory_after_writeback() {
        use crate::types::TrustLevel;

        let store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let config = BrainConfig::default();
        let brain = Brain::new(store, embedder, config);

        // Simulate a tool writeback — identical to what agentic.rs does after a
        // successful tool execution with writeback_enabled = true.
        let mut mem = crate::types::NewMemory {
            content: r#"{"processes":[{"name":"test","pid":1}],"count":1}"#.to_string(),
            source: Some("process_list".to_string()),
            tags: Some("tool_verified,system,processes".to_string()),
            embedding: None,
            memory_type: Some("tool_verified".to_string()),
            importance: Some(0.8),
            strength: None,
            supersedes: None,
            content_hash: None,
            valid_from: None,
            valid_to: None,
            source_type: None,
            reflection: None,
            trust_level: Some(TrustLevel::ToolVerified),
            evidence_session_id: None,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: None,
            quality_flag: None,
        };
        brain.remember_full(&mut mem).await.unwrap();

        // The stored memory must be retrievable and carry TrustLevel::ToolVerified.
        let memories = brain.store().all_memories().await.unwrap();
        let stored = memories
            .iter()
            .find(|m| m.tags.as_deref().unwrap_or("").contains("tool_verified"));
        assert!(
            stored.is_some(),
            "expected a tool_verified tagged memory after remember_full"
        );
        let stored = stored.unwrap();
        assert_eq!(
            stored.trust_level,
            TrustLevel::ToolVerified,
            "stored memory must carry TrustLevel::ToolVerified"
        );
        assert_eq!(
            stored.source.as_deref(),
            Some("process_list"),
            "source field must match the tool name"
        );
        assert_eq!(
            stored.memory_type.as_str(),
            "tool_verified",
            "memory_type must be 'tool_verified'"
        );
    }

    // ── parse_accessed_at ────────────────────────────────────────────────────

    #[test]
    fn parse_accessed_at_space_separated() {
        // Legacy SQLite format written before the ISO migration.
        let dt = Brain::parse_accessed_at("2026-04-22 10:30:00");
        assert!(dt.is_some(), "space-separated format must parse");
        let dt = dt.unwrap();
        assert_eq!(
            dt.date(),
            chrono::NaiveDate::from_ymd_opt(2026, 4, 22).unwrap()
        );
        assert_eq!(
            dt.time(),
            chrono::NaiveTime::from_hms_opt(10, 30, 0).unwrap()
        );
    }

    #[test]
    fn parse_accessed_at_iso_t_separator() {
        // ISO 8601 without sub-seconds.
        let dt = Brain::parse_accessed_at("2026-04-22T10:30:00");
        assert!(dt.is_some(), "ISO T-separator format must parse");
        let dt = dt.unwrap();
        assert_eq!(
            dt.date(),
            chrono::NaiveDate::from_ymd_opt(2026, 4, 22).unwrap()
        );
        assert_eq!(
            dt.time(),
            chrono::NaiveTime::from_hms_opt(10, 30, 0).unwrap()
        );
    }

    #[test]
    fn parse_accessed_at_rfc3339_with_z() {
        // RFC 3339 with Z suffix — current store default via strftime + update_accessed.
        let dt = Brain::parse_accessed_at("2026-04-22T10:30:00Z");
        assert!(dt.is_some(), "RFC 3339 Z-suffix format must parse");
        let dt = dt.unwrap();
        assert_eq!(
            dt.date(),
            chrono::NaiveDate::from_ymd_opt(2026, 4, 22).unwrap()
        );
        assert_eq!(
            dt.time(),
            chrono::NaiveTime::from_hms_opt(10, 30, 0).unwrap()
        );
    }

    #[test]
    fn parse_accessed_at_garbage_returns_none() {
        assert!(
            Brain::parse_accessed_at("not-a-date").is_none(),
            "garbage input must return None"
        );
        assert!(
            Brain::parse_accessed_at("").is_none(),
            "empty string must return None"
        );
    }
}
