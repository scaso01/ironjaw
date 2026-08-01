use crate::brain::{estimate_tokens, Brain};
use crate::graph::KnowledgeGraph;
use crate::store::BrainStore;
use crate::types::{MemoryType, NewMemory, SourceType, TrustLevel};
use ironjaw_llm::types::{ChatMessage, ChatRequest, Role};
use regex::Regex;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;
use tokio_stream::StreamExt;
use tracing::{debug, info, warn};

// ── Report ────────────────────────────────────────────────────────────

/// Report from an auto-learning run.
#[derive(Debug, Clone, Default)]
pub struct LearnReport {
    /// Number of facts extracted from the conversation.
    pub extracted: usize,
    /// Number of facts that passed deduplication and were stored.
    pub stored: usize,
    /// Number of facts skipped due to deduplication (too similar to existing).
    pub deduplicated: usize,
    /// Number of facts that superseded an older memory.
    pub superseded: usize,
}

// ── Candidate ─────────────────────────────────────────────────────────

/// Intermediate extraction result -- a candidate fact to be gated before storage.
#[derive(Debug, Clone)]
pub struct LearnCandidate {
    pub content: String,
    pub memory_type: MemoryType,
    pub confidence: f32,
    pub tags: Vec<String>,
    /// Text description of what this replaces (if anything).
    pub supersedes_hint: Option<String>,
    /// Trust origin of the candidate. ToolVerified/SystemPolicy bypass the
    /// stricter novelty threshold so live-state writebacks aren't rejected
    /// for being similar to prior tool calls on the same entity/key.
    pub trust_level: Option<TrustLevel>,
}

// ── Extraction Prompt ─────────────────────────────────────────────────

/// Builds the LLM extraction prompt from conversation messages.
pub struct ExtractionPrompt;

impl ExtractionPrompt {
    /// Build a prompt asking the LLM to extract JSON facts from a conversation.
    ///
    /// `messages` is a slice of `(role, content)` pairs from recent turns.
    pub fn build(messages: &[(String, String)]) -> String {
        let mut formatted = String::new();
        for (role, content) in messages {
            formatted.push_str(&format!("[{role}]: {content}\n"));
        }

        format!(
            "Extract learnable facts from this conversation as JSON array.\n\
             \n\
             CRITICAL: Extract SPECIFIC values — config flags, version numbers, port numbers, \
             file paths, command-line arguments, concrete decisions with parameters. \
             Do NOT extract vague intent like 'decided to configure X' — extract \
             'X configured with flag=value'.\n\
             \n\
             For each fact:\n\
             - \"content\": the fact with SPECIFIC values (concise, standalone sentence)\n\
             - \"type\": one of [\"fact\", \"preference\", \"correction\", \"command\", \"context\", \"event\"]\n\
             - \"confidence\": 0.0-1.0 how certain this is\n\
             - \"tags\": array of topic categories\n\
             - \"supersedes\": description of any older fact this replaces, or null\n\
             \n\
             REJECT (return empty array):\n\
             - Task descriptions or to-do items (\"need to investigate X\")\n\
             - Vague intent without concrete values (\"updated the configuration\")\n\
             - Greetings, acknowledgments, filler\n\
             - Code snippets or generic knowledge\n\
             - Anything recoverable from git log or code inspection\n\
             \n\
             EXAMPLES:\n\
             BAD: \"Decided to configure llama-server parallel processing\"\n\
             GOOD: \"llama-server configured with --parallel 4 --ctx-size 131072 \
             --cache-type-k q8_0 --cache-type-v q8_0\"\n\
             \n\
             BAD: \"Updated the port configuration\"\n\
             GOOD: \"Ironjaw gateway moved from port 18789 to port 18794\"\n\
             \n\
             BAD: \"Started working on the search feature\"\n\
             GOOD: (empty array — this is a task, not a fact)\n\
             \n\
             Conversation:\n\
             {formatted}"
        )
    }
}

// ── Write Gate ────────────────────────────────────────────────────────

/// Result of write-time gating for a candidate.
#[derive(Debug, Clone, PartialEq)]
pub enum GateResult {
    /// Memory is novel enough to store.
    Store,
    /// Memory should be skipped (duplicate or too similar). Carries the id of
    /// the existing memory that blocked the write, when known, so callers can
    /// recover by explicitly superseding it.
    Skip(Option<i64>),
    /// Memory supersedes an older memory (old_id).
    Supersede(i64),
}

/// Write-time gating -- decides whether a candidate should be stored, skipped,
/// or stored as a supersession of an existing memory.
pub struct WriteGate;

impl WriteGate {
    /// Compute a deterministic hex digest of a content string for deduplication.
    ///
    /// Uses a double-hash approach (two passes with different seeds) to produce
    /// a 32-character hex string with good collision resistance for dedup purposes.
    /// When the `sha2` crate is available, this can be upgraded to real SHA-256.
    pub fn content_hash(content: &str) -> String {
        let mut h1 = DefaultHasher::new();
        content.hash(&mut h1);
        let hash1 = h1.finish();

        let mut h2 = DefaultHasher::new();
        hash1.hash(&mut h2);
        content.hash(&mut h2);
        let hash2 = h2.finish();

        format!("{hash1:016x}{hash2:016x}")
    }

    /// Check if an exact duplicate exists (by content hash).
    pub async fn check_exact_duplicate(
        store: &BrainStore,
        content_hash: &str,
    ) -> anyhow::Result<bool> {
        Ok(store.find_by_content_hash(content_hash).await?.is_some())
    }

    /// Compute novelty score via KNN search.
    ///
    /// Embeds the candidate, finds top-K similar existing memories, and
    /// returns `1.0 - max_similarity` plus the id of the most similar memory.
    /// If no existing memories, returns `(1.0, embedding, None)`.
    pub async fn compute_novelty(
        brain: &Brain,
        content: &str,
        limit: usize,
    ) -> anyhow::Result<(f32, Vec<f32>, Option<i64>)> {
        let embedding = brain.embed_text(content).await?;
        let results = brain.store().knn_search(&embedding, limit).await?;

        let nearest = results.iter().max_by(|a, b| a.1.total_cmp(&b.1)).copied();
        match nearest {
            Some((id, max_similarity)) => Ok((1.0 - max_similarity, embedding, Some(id))),
            None => Ok((1.0, embedding, None)),
        }
    }

    /// Detect if the candidate supersedes an existing memory by tag overlap.
    ///
    /// If the candidate has a `supersedes_hint`, search existing memories
    /// by tag overlap and return the ID of the best match.
    pub async fn detect_supersession(
        store: &BrainStore,
        candidate: &LearnCandidate,
    ) -> anyhow::Result<Option<i64>> {
        let hint = match &candidate.supersedes_hint {
            Some(h) if !h.is_empty() => h,
            _ => return Ok(None),
        };

        if candidate.tags.is_empty() {
            return Ok(None);
        }

        let existing = store.find_by_tags(&candidate.tags).await?;
        if existing.is_empty() {
            return Ok(None);
        }

        // Find the entry with the most tag overlap
        let hint_lower = hint.to_lowercase();
        let mut best_id = None;
        let mut best_overlap = 0usize;

        for (id, tags_str) in &existing {
            let existing_tags: Vec<&str> = tags_str.split(',').map(|t| t.trim()).collect();
            let overlap = candidate
                .tags
                .iter()
                .filter(|t| existing_tags.contains(&t.as_str()))
                .count();

            // Also consider if the hint text matches any tag
            let hint_bonus = if existing_tags
                .iter()
                .any(|t| hint_lower.contains(&t.to_lowercase()))
            {
                1
            } else {
                0
            };

            let score = overlap + hint_bonus;
            if score > best_overlap {
                best_overlap = score;
                best_id = Some(*id);
            }
        }

        Ok(best_id)
    }

    /// Detect contradiction signals between a candidate and a highly similar existing memory.
    ///
    /// High similarity (> threshold) combined with negation keywords suggests
    /// the new fact contradicts an existing one.
    pub fn detect_contradiction(content: &str, similarity: f32, threshold: f32) -> bool {
        if similarity < threshold {
            return false;
        }

        let lower = content.to_lowercase();
        let negation_signals = [
            "not ",
            "no longer",
            "instead of",
            "changed from",
            "actually",
            "wrong",
            "incorrect",
            "no longer",
            "replaced",
            "was renamed",
            "should be",
        ];

        negation_signals.iter().any(|sig| lower.contains(sig))
    }

    /// Detect cross-source contradiction: high similarity from a different source
    /// with differing content suggests a conflict.
    ///
    /// Returns `Some(old_memory_id)` if a cross-source contradiction is found.
    pub async fn detect_cross_source_contradiction(
        brain: &Brain,
        content: &str,
        source_type: &SourceType,
        threshold: f32,
    ) -> anyhow::Result<Option<i64>> {
        let embedding = brain.embed_text(content).await?;
        let results = brain.store().knn_search(&embedding, 5).await?;

        for (mem_id, score) in &results {
            if *score < threshold {
                continue;
            }
            // Load the memory and check source_type
            if let Some(mem) = brain.store().get_memory(*mem_id).await? {
                let mem_source: Result<SourceType, _> = mem.source_type.parse();
                if let Ok(mem_source) = mem_source {
                    if mem_source != *source_type {
                        // Different source, high similarity -- check for contradiction signals
                        if Self::detect_contradiction(content, *score, threshold) {
                            return Ok(Some(*mem_id));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// Run the full gating pipeline for a candidate.
    ///
    /// Returns a `GateResult` indicating whether to store, skip, or supersede.
    pub async fn gate(
        brain: &Brain,
        store: &BrainStore,
        candidate: &LearnCandidate,
        content_hash: &str,
    ) -> anyhow::Result<GateResult> {
        // 1. Exact duplicate check
        if let Some(dup_id) = store.find_by_content_hash(content_hash).await? {
            debug!(content = %candidate.content, dup_id, "WriteGate: exact duplicate, skipping");
            return Ok(GateResult::Skip(Some(dup_id)));
        }

        // 2. Hinted supersession via tags — checked BEFORE the similarity
        // rejection, because an update to a tagged entry is expected to be
        // near-identical to the entry it replaces.
        if let Ok(Some(old_id)) = Self::detect_supersession(store, candidate).await {
            debug!(
                old_id,
                content = %candidate.content,
                "WriteGate: supersedes existing memory (tag match)"
            );
            return Ok(GateResult::Supersede(old_id));
        }

        // 3. Compute novelty via embedding similarity
        let (novelty, _embedding, nearest_id) =
            match Self::compute_novelty(brain, &candidate.content, 5).await {
                Ok(result) => result,
                Err(e) => {
                    warn!(error = %e, "WriteGate: novelty computation failed, allowing store");
                    return Ok(GateResult::Store);
                }
            };

        // 4. Low novelty -> skip (trust-aware: tool/system memories use 0.05)
        let novelty_min = match candidate.trust_level {
            Some(TrustLevel::ToolVerified) | Some(TrustLevel::SystemPolicy) => 0.05,
            _ => 0.15,
        };
        if novelty < novelty_min {
            debug!(
                novelty,
                threshold = novelty_min,
                content = %candidate.content,
                "WriteGate: too similar to existing, skipping"
            );
            return Ok(GateResult::Skip(nearest_id));
        }

        // 5. Medium novelty -> check for contradiction
        if novelty < 0.40 {
            // Check contradiction via negation signals
            let similarity = 1.0 - novelty;
            if Self::detect_contradiction(&candidate.content, similarity, 0.85) {
                // Find the most similar memory to supersede
                let results = brain
                    .store()
                    .knn_search(
                        &brain
                            .embed_text(&candidate.content)
                            .await
                            .unwrap_or_default(),
                        1,
                    )
                    .await?;

                if let Some((old_id, _score)) = results.first() {
                    debug!(
                        novelty,
                        old_id,
                        content = %candidate.content,
                        "WriteGate: contradiction detected, superseding"
                    );
                    return Ok(GateResult::Supersede(*old_id));
                }
            }
        }

        // 6. Novel enough -> store
        Ok(GateResult::Store)
    }
}

// ── Learner ───────────────────────────────────────────────────────────

/// Auto-learning pipeline: extracts factual statements from conversation
/// turns, deduplicates against existing memories, and stores new ones.
///
/// Supports two extraction modes:
/// - **LLM-driven**: builds a prompt, receives JSON, parses into candidates
/// - **Heuristic**: sentence splitting + keyword matching (fallback)
#[derive(Default)]
pub struct Learner {
    /// Whether to attempt LLM-based extraction before falling back to heuristic.
    pub use_llm: bool,
}

impl Learner {
    /// Create a new Learner with the specified LLM extraction preference.
    pub fn new(use_llm: bool) -> Self {
        Self { use_llm }
    }

    // ── Extraction ────────────────────────────────────────────────

    /// Build the LLM extraction prompt for the given messages.
    pub fn build_extraction_prompt(messages: &[(String, String)]) -> String {
        ExtractionPrompt::build(messages)
    }

    /// Parse an LLM response string into `Vec<LearnCandidate>`.
    ///
    /// Robust against markdown code fences, trailing commas, and other
    /// common LLM output quirks. On parse failure, returns an empty vec.
    pub fn extract_with_llm(llm_response: &str) -> Vec<LearnCandidate> {
        let cleaned = strip_markdown_fences(llm_response);
        let cleaned = strip_trailing_commas(&cleaned);

        let parsed: Vec<RawLlmCandidate> = match serde_json::from_str(&cleaned) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "extract_with_llm: JSON parse failed, falling back");
                return Vec::new();
            }
        };

        parsed
            .into_iter()
            .filter_map(|raw| {
                let memory_type = parse_memory_type(&raw.r#type).unwrap_or(MemoryType::Fact);
                let confidence = raw.confidence.unwrap_or(0.5).clamp(0.0, 1.0);

                if raw.content.trim().is_empty() {
                    return None;
                }

                Some(LearnCandidate {
                    content: raw.content,
                    memory_type,
                    confidence,
                    tags: raw.tags.unwrap_or_default(),
                    supersedes_hint: raw.supersedes,
                    trust_level: None,
                })
            })
            .collect()
    }

    /// Heuristic extraction: splits assistant text into sentences,
    /// filters for statements that look like learnable facts.
    ///
    /// This is the original `extract_candidates` renamed -- kept as fallback.
    pub fn extract_heuristic(messages: &[(String, String)]) -> Vec<LearnCandidate> {
        let mut candidates = Vec::new();

        for (role, content) in messages {
            if role != "assistant" {
                continue;
            }

            for sentence in split_sentences(content) {
                let trimmed = sentence.trim();
                let tokens = estimate_tokens(trimmed);
                if !(5..=200).contains(&tokens) {
                    continue;
                }
                if trimmed.starts_with('#')
                    || trimmed.starts_with("```")
                    || trimmed.starts_with('>')
                    || trimmed.starts_with('-')
                    || trimmed.starts_with('*')
                    || trimmed.ends_with('?')
                {
                    continue;
                }
                if is_factual(trimmed) {
                    let memory_type = classify_heuristic(trimmed);
                    candidates.push(LearnCandidate {
                        content: trimmed.to_string(),
                        memory_type,
                        confidence: 0.5,
                        tags: Vec::new(),
                        supersedes_hint: None,
                        trust_level: None,
                    });
                }
            }
        }

        candidates
    }

    /// Extract candidates from messages using LLM response or heuristic fallback.
    ///
    /// If `use_llm` is true and `llm_response` is provided, parse the LLM output.
    /// On failure or if LLM is not used, fall back to heuristic extraction.
    pub fn extract(
        &self,
        messages: &[(String, String)],
        llm_response: Option<&str>,
    ) -> Vec<LearnCandidate> {
        if self.use_llm {
            if let Some(response) = llm_response {
                let candidates = Self::extract_with_llm(response);
                if !candidates.is_empty() {
                    debug!(count = candidates.len(), "LLM extraction succeeded");
                    return candidates;
                }
                warn!("LLM extraction returned empty, falling back to heuristic");
            }
        }

        Self::extract_heuristic(messages)
    }

    // ── Learn Pipeline ────────────────────────────────────────────

    /// Run the full learn pipeline: extract -> gate -> store.
    ///
    /// `messages` is a slice of (role, content) pairs from the conversation.
    /// `llm_response` is an optional LLM extraction response (if LLM was called externally).
    /// `dedup_threshold` is kept for backward compatibility but the new gating
    /// pipeline uses novelty scoring instead.
    pub async fn learn(
        &self,
        brain: &Brain,
        messages: &[(String, String)],
        llm_response: Option<&str>,
        _dedup_threshold: f32,
    ) -> anyhow::Result<LearnReport> {
        let candidates = self.extract(messages, llm_response);
        if candidates.is_empty() {
            return Ok(LearnReport::default());
        }

        let mut report = LearnReport {
            extracted: candidates.len(),
            ..Default::default()
        };

        let store = brain.store();

        for candidate in &candidates {
            // Compute content hash
            let hash = WriteGate::content_hash(&candidate.content);

            // Run write gating
            let gate_result = match WriteGate::gate(brain, store, candidate, &hash).await {
                Ok(result) => result,
                Err(e) => {
                    warn!(error = %e, content = %candidate.content, "Auto-learn: gate failed");
                    continue;
                }
            };

            match gate_result {
                GateResult::Skip(_) => {
                    report.deduplicated += 1;
                }
                GateResult::Store => {
                    match self.store_candidate(brain, candidate, &hash, None).await {
                        Ok(()) => report.stored += 1,
                        Err(e) => {
                            warn!(error = %e, content = %candidate.content, "Auto-learn: store failed");
                        }
                    }
                }
                GateResult::Supersede(old_id) => {
                    match self
                        .store_candidate(brain, candidate, &hash, Some(old_id))
                        .await
                    {
                        Ok(()) => {
                            report.stored += 1;
                            report.superseded += 1;
                        }
                        Err(e) => {
                            warn!(error = %e, content = %candidate.content, "Auto-learn: supersede-store failed");
                        }
                    }
                }
            }
        }

        if report.stored > 0 {
            info!(
                extracted = report.extracted,
                stored = report.stored,
                deduped = report.deduplicated,
                superseded = report.superseded,
                "Auto-learn complete"
            );
        } else {
            debug!(
                extracted = report.extracted,
                deduped = report.deduplicated,
                "Auto-learn: nothing new to store"
            );
        }

        Ok(report)
    }

    /// Source-aware confidence multiplier.
    ///
    /// - `CcChronicle` data is structured summaries: higher confidence (1.2x)
    /// - `Brain` and `Markdown` are direct user input: normal confidence (1.0x)
    /// - `CcSession` raw transcripts: lower confidence (0.8x)
    /// - `CcBridge` relayed data: moderate confidence (0.9x)
    pub fn source_confidence_multiplier(source_type: &SourceType) -> f32 {
        match source_type {
            SourceType::CcChronicle => 1.2,
            SourceType::Brain | SourceType::Markdown => 1.0,
            SourceType::CcBridge => 0.9,
            SourceType::CcSession => 0.8,
        }
    }

    /// Source-aware importance adjustment.
    ///
    /// Adjusts base importance by source reliability:
    /// - `Brain` memories get a 10% boost (direct user intent)
    /// - `CcChronicle` memories get a 5% boost (structured summaries)
    /// - `CcSession` memories get a 10% penalty (raw, noisy)
    pub fn source_importance_adjustment(source_type: &SourceType) -> f64 {
        match source_type {
            SourceType::Brain => 0.10,
            SourceType::CcChronicle => 0.05,
            SourceType::Markdown => 0.0,
            SourceType::CcBridge => 0.0,
            SourceType::CcSession => -0.10,
        }
    }

    /// Run the full learn pipeline with source-type awareness.
    ///
    /// Like `learn()`, but additionally:
    /// - Adjusts confidence based on source reliability
    /// - Checks for cross-source contradictions
    /// - Creates entity links with source tracking in the knowledge graph
    pub async fn learn_with_source(
        &self,
        brain: &Brain,
        messages: &[(String, String)],
        llm_response: Option<&str>,
        source_type: &SourceType,
        graph: Option<&KnowledgeGraph>,
    ) -> anyhow::Result<LearnReport> {
        let candidates = self.extract(messages, llm_response);
        if candidates.is_empty() {
            return Ok(LearnReport::default());
        }

        let confidence_mult = Self::source_confidence_multiplier(source_type);
        let importance_adj = Self::source_importance_adjustment(source_type);

        let mut report = LearnReport {
            extracted: candidates.len(),
            ..Default::default()
        };

        let store = brain.store();

        for candidate in &candidates {
            // Adjust confidence by source
            let adjusted_confidence = (candidate.confidence * confidence_mult).clamp(0.0, 1.0);

            let adjusted_candidate = LearnCandidate {
                content: candidate.content.clone(),
                memory_type: candidate.memory_type,
                confidence: adjusted_confidence,
                tags: candidate.tags.clone(),
                supersedes_hint: candidate.supersedes_hint.clone(),
                trust_level: candidate.trust_level,
            };

            let hash = WriteGate::content_hash(&adjusted_candidate.content);

            // Check for cross-source contradiction
            if let Ok(Some(old_id)) = WriteGate::detect_cross_source_contradiction(
                brain,
                &adjusted_candidate.content,
                source_type,
                0.85,
            )
            .await
            {
                info!(
                    old_id,
                    source = %source_type,
                    "Cross-source contradiction detected"
                );
                // Flag contradiction in graph
                if let Some(graph) = graph {
                    let _ = graph.add_contradiction(old_id, 0).await; // 0 = not stored yet
                }
                // Store as supersession of the contradicted memory
                match self
                    .store_candidate_with_source(
                        brain,
                        &adjusted_candidate,
                        &hash,
                        Some(old_id),
                        source_type,
                        importance_adj,
                    )
                    .await
                {
                    Ok(mem_id) => {
                        report.stored += 1;
                        report.superseded += 1;
                        // Link entities with source tracking
                        if let Some(graph) = graph {
                            let _ = graph
                                .extract_and_link_entities_with_source(
                                    mem_id,
                                    &adjusted_candidate.content,
                                    source_type,
                                )
                                .await;
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "learn_with_source: contradiction store failed");
                    }
                }
                continue;
            }

            // Normal gating
            let gate_result = match WriteGate::gate(brain, store, &adjusted_candidate, &hash).await
            {
                Ok(result) => result,
                Err(e) => {
                    warn!(error = %e, "learn_with_source: gate failed");
                    continue;
                }
            };

            match gate_result {
                GateResult::Skip(_) => {
                    report.deduplicated += 1;
                }
                GateResult::Store | GateResult::Supersede(_) => {
                    let supersedes = match &gate_result {
                        GateResult::Supersede(old_id) => Some(*old_id),
                        _ => None,
                    };
                    match self
                        .store_candidate_with_source(
                            brain,
                            &adjusted_candidate,
                            &hash,
                            supersedes,
                            source_type,
                            importance_adj,
                        )
                        .await
                    {
                        Ok(mem_id) => {
                            report.stored += 1;
                            if supersedes.is_some() {
                                report.superseded += 1;
                            }
                            // Link entities with source tracking
                            if let Some(graph) = graph {
                                let _ = graph
                                    .extract_and_link_entities_with_source(
                                        mem_id,
                                        &adjusted_candidate.content,
                                        source_type,
                                    )
                                    .await;
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "learn_with_source: store failed");
                        }
                    }
                }
            }
        }

        if report.stored > 0 {
            info!(
                extracted = report.extracted,
                stored = report.stored,
                deduped = report.deduplicated,
                superseded = report.superseded,
                source = %source_type,
                "Auto-learn with source complete"
            );
        }

        Ok(report)
    }

    /// Store a single candidate as a new memory.
    async fn store_candidate(
        &self,
        brain: &Brain,
        candidate: &LearnCandidate,
        content_hash: &str,
        supersedes: Option<i64>,
    ) -> anyhow::Result<()> {
        self.store_candidate_with_source(
            brain,
            candidate,
            content_hash,
            supersedes,
            &SourceType::CcSession,
            0.0,
        )
        .await?;
        Ok(())
    }

    /// Store a single candidate as a new memory with source-type awareness.
    ///
    /// Returns the new memory ID on success.
    async fn store_candidate_with_source(
        &self,
        brain: &Brain,
        candidate: &LearnCandidate,
        content_hash: &str,
        supersedes: Option<i64>,
        source_type: &SourceType,
        importance_adjustment: f64,
    ) -> anyhow::Result<i64> {
        // Compute embedding
        let embedding = match brain.embed_text(&candidate.content).await {
            Ok(emb) => Some(emb),
            Err(e) => {
                warn!(error = %e, "store_candidate: embedding failed, storing without vector");
                None
            }
        };

        let tags_str = if candidate.tags.is_empty() {
            Some("auto-learned".to_string())
        } else {
            Some(candidate.tags.join(","))
        };

        let memory_type_str = candidate.memory_type.to_string();
        let base_importance = candidate.memory_type.base_importance() * candidate.confidence as f64;
        let importance = (base_importance + importance_adjustment).clamp(0.0, 1.0);

        let new_mem = NewMemory {
            content: candidate.content.clone(),
            source: Some("auto-learn".to_string()),
            tags: tags_str,
            embedding,
            memory_type: Some(memory_type_str),
            importance: Some(importance),
            strength: Some(1.0),
            supersedes,
            content_hash: Some(content_hash.to_string()),
            valid_from: None,
            valid_to: None,
            source_type: Some(source_type.to_string()),
            reflection: None,
            trust_level: None,
            evidence_session_id: None,
            evidence_message_id: None,
            scope: None,
            procedure: None,
            quality_score: None,
            quality_flag: None,
        };

        let id = brain.store().add_memory(&new_mem).await?;
        Ok(id)
    }
}

// ── Model Self-Reference Stripping ─────────────────────────────────

/// Known model family names that LLMs may use to self-identify.
const MODEL_NAMES: &[&str] = &[
    "qwen",
    "llama",
    "mistral",
    "gemma",
    "claude",
    "gpt",
    "phi",
    "deepseek",
    "command-r",
    "codestral",
    "mixtral",
    "vicuna",
    "alpaca",
    "falcon",
    "yi",
    "internlm",
    "baichuan",
    "chatglm",
    "solar",
    "orca",
    "hermes",
    "nous",
    "wizardlm",
    "openchat",
    "starling",
    "zephyr",
    "neural-chat",
    "tinyllama",
];

/// Compiled regex for stripping model self-references from LLM output.
///
/// Matches patterns like:
/// - "As Qwen, I ..."  /  "As a Qwen model, ..."
/// - "I am Qwen" / "I'm Qwen" / "My name is Qwen"
/// - "As a large language model" / "As an AI assistant called Qwen"
/// - "I, Qwen, ..." / "I (Qwen) ..."
///
/// Does NOT match legitimate third-person references like "the user asked about Llama models".
fn model_reference_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let names = MODEL_NAMES.join("|");

        // Version suffix: handles "Qwen-2.5", "GPT-4o", "Llama-3.1" etc.
        let ver = r"(?:[- ][\w.]+)*";

        // Build pattern components:
        // 1. "As <model>[,] ..." at sentence start (case-insensitive model name)
        let as_model = format!(
            r"(?i)\bAs (?:a |an )?(?:{names}){ver},? "
        );
        // 2. "As a large language model" / "As an AI assistant" with optional "called <name>"
        let as_llm = format!(
            r"(?i)\bAs (?:a |an )(?:large )?(?:language model|LLM|AI(?:\s+(?:assistant|model|chatbot))?)(?:\s+(?:called|named|known as)\s+(?:{names}){ver})?,? "
        );
        // 3. "I am/I'm <model>" / "My name is <model>"
        let i_am = format!(
            r"(?i)\b(?:I am|I'm|my name is)\s+(?:{names}){ver}(?:[,.]|\b)"
        );
        // 4. "I, <model>," / "I (<model>)"
        let i_appositive = format!(
            r"(?i)\bI[, ]+(?:\()?(?:{names}){ver}(?:\))?[, ]+"
        );

        let full = format!("(?:{as_model})|(?:{as_llm})|(?:{i_am})|(?:{i_appositive})");
        Regex::new(&full).expect("model reference regex should compile")
    })
}

/// Strip LLM model self-references from text.
///
/// Removes sentences or phrases where the model identifies itself by name
/// (e.g., "As Qwen, I summarized...", "I'm Claude, and I helped...").
/// Preserves legitimate third-person mentions like "the user asked about Llama".
pub fn strip_model_references(text: &str) -> String {
    let re = model_reference_regex();
    let result = re.replace_all(text, "");

    // Clean up artifacts: double spaces, leading commas/spaces on lines
    let result = result
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return String::new();
            }
            // Remove leading punctuation artifacts from stripped phrases
            let cleaned = trimmed.trim_start_matches([',', ' ']);
            // Capitalize first letter if it became lowercase after stripping
            if cleaned.is_empty() {
                return String::new();
            }
            cleaned.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Collapse multiple consecutive blank lines
    let mut prev_blank = false;
    let result: Vec<&str> = result
        .lines()
        .filter(|line| {
            let is_blank = line.trim().is_empty();
            let keep = !(is_blank && prev_blank);
            prev_blank = is_blank;
            keep
        })
        .collect();

    result.join("\n").trim().to_string()
}

// ── Session Summary Generation ──────────────────────────────────────

/// Generate a 2-6 bullet summary of a conversation session and store it
/// as a `session_summary` memory.
///
/// Requires both a `BrainStore` (for persistence) and an `LlmRouter`
/// (for summarization). Called from handler.rs where both are available.
///
/// Returns `Ok(Some(memory_id))` on success, `Ok(None)` if skipped,
/// or `Err` on failure.
pub async fn generate_session_summary(
    store: &BrainStore,
    llm: &ironjaw_llm::LlmRouter,
    session_id: &str,
    messages: &[(String, String)],
) -> anyhow::Result<Option<i64>> {
    // Skip if too few messages
    if messages.len() < 3 {
        debug!(
            session_id,
            count = messages.len(),
            "session_summary: too few messages, skipping"
        );
        return Ok(None);
    }

    // Skip if a summary for this session already exists
    let existing = store.get_summaries(Some(session_id), 1).await?;
    if !existing.is_empty() {
        debug!(session_id, "session_summary: already summarized, skipping");
        return Ok(None);
    }

    // Build the conversation text, truncated to ~4000 chars
    let mut conversation = String::new();
    for (role, content) in messages {
        let line = format!("{role}: {content}\n");
        if conversation.len() + line.len() > 4000 {
            conversation.push_str("...(truncated)\n");
            break;
        }
        conversation.push_str(&line);
    }

    let prompt = format!(
        "Summarize this conversation in 2-6 concise bullet points. Focus on:\n\
         - Key decisions made\n\
         - Important facts learned\n\
         - Actions taken or planned\n\
         \n\
         Conversation:\n\
         {conversation}"
    );

    let request = ChatRequest {
        model: String::new(), // use router default
        messages: vec![ChatMessage {
            role: Role::User,
            content: prompt,
            images: None,
            ..Default::default()
        }],
        temperature: Some(0.3),
        max_tokens: Some(500),
        extra: None,
        thinking: None,
        tools: None,
        tool_choice: None,
    };

    // FIX #5: bound the summary LLM call. A wedged local model would otherwise
    // hang the compaction / session-switch path forever (this was previously
    // unbounded). The prompt is already truncated to ~4000 chars, so 60s is
    // generous; on timeout we proceed with whatever partial text was collected.
    const SUMMARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    let mut stream = tokio::time::timeout(SUMMARY_TIMEOUT, llm.chat(request))
        .await
        .map_err(|_| anyhow::anyhow!("session_summary LLM call timed out"))?
        .map_err(|e| anyhow::anyhow!("session_summary LLM call failed: {e}"))?;

    let mut summary = String::new();
    let collected = tokio::time::timeout(SUMMARY_TIMEOUT, async {
        while let Some(chunk) = stream.next().await {
            if chunk.done {
                break;
            }
            summary.push_str(&chunk.delta);
        }
    })
    .await;
    if collected.is_err() {
        warn!(
            session_id,
            "session_summary: LLM stream collection timed out"
        );
    }

    let summary = strip_model_references(summary.trim());
    if summary.is_empty() {
        warn!(session_id, "session_summary: LLM returned empty summary");
        return Ok(None);
    }

    // Store as a session_summary memory
    let new_mem = NewMemory {
        content: summary,
        source: Some(session_id.to_string()),
        tags: Some("session_summary".to_string()),
        embedding: None,
        memory_type: Some("session_summary".to_string()),
        importance: Some(0.65),
        strength: Some(1.0),
        supersedes: None,
        content_hash: None,
        valid_from: None,
        valid_to: None,
        source_type: Some("cc_session".to_string()),
        reflection: None,
        trust_level: None,
        evidence_session_id: None,
        evidence_message_id: None,
        scope: None,
        procedure: None,
        quality_score: None,
        quality_flag: None,
    };

    let memory_id = store.add_memory(&new_mem).await?;

    info!(
        session_id,
        memory_id, "session_summary: stored summary as memory"
    );

    Ok(Some(memory_id))
}

// ── Raw LLM JSON schema ──────────────────────────────────────────────

/// Internal struct for deserializing LLM JSON output.
#[derive(serde::Deserialize)]
struct RawLlmCandidate {
    content: String,
    r#type: String,
    confidence: Option<f32>,
    tags: Option<Vec<String>>,
    supersedes: Option<String>,
}

// ── Helper functions ─────────────────────────────────────────────────

/// Strip markdown code fences from LLM output.
///
/// Handles ```` ```json ... ``` ```` and ```` ``` ... ``` ```` patterns.
fn strip_markdown_fences(text: &str) -> String {
    let trimmed = text.trim();

    // Find opening fence
    let start = if let Some(pos) = trimmed.find("```json") {
        pos + "```json".len()
    } else if let Some(pos) = trimmed.find("```") {
        pos + "```".len()
    } else {
        return trimmed.to_string();
    };

    // Find closing fence
    let rest = &trimmed[start..];
    let end = if let Some(pos) = rest.rfind("```") {
        start + pos
    } else {
        return rest.trim().to_string();
    };

    trimmed[start..end].trim().to_string()
}

/// Strip trailing commas before closing brackets (common LLM mistake).
fn strip_trailing_commas(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();

    let mut i = 0;
    while i < len {
        if chars[i] == ',' {
            // Look ahead past whitespace for ] or }
            let mut j = i + 1;
            while j < len && chars[j].is_whitespace() {
                j += 1;
            }
            if j < len && (chars[j] == ']' || chars[j] == '}') {
                // Skip the trailing comma
                i += 1;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Parse a memory type string from LLM output into `MemoryType`.
fn parse_memory_type(s: &str) -> Option<MemoryType> {
    s.to_lowercase().parse().ok()
}

/// Classify a sentence into a `MemoryType` using heuristic keyword matching.
fn classify_heuristic(sentence: &str) -> MemoryType {
    let lower = sentence.to_lowercase();

    // Correction indicators first (most specific)
    let correction_words = [
        "actually ",
        "correct ",
        "instead ",
        "should be ",
        "fixed ",
        "changed ",
        "updated ",
        "renamed ",
        "moved ",
        "replaced ",
        "wrong",
        "incorrect",
    ];
    if correction_words.iter().any(|w| lower.contains(w)) {
        return MemoryType::Correction;
    }

    // Command indicators
    let command_words = ["run ", "execute ", "use the command", "invoke ", "call "];
    if command_words.iter().any(|w| lower.contains(w)) {
        return MemoryType::Command;
    }

    // Preference indicators
    let preference_words = ["prefer", "always ", "never ", "like ", "want ", "don't "];
    if preference_words.iter().any(|w| lower.contains(w)) {
        return MemoryType::Preference;
    }

    // Event indicators
    let event_words = ["happened", "occurred", "started", "finished", "completed"];
    if event_words.iter().any(|w| lower.contains(w)) {
        return MemoryType::Event;
    }

    // Default to Fact
    MemoryType::Fact
}

/// Split text into sentences. Handles common abbreviations.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0;

    for (i, c) in text.char_indices() {
        if c == '\n' {
            let chunk = &text[start..i];
            if !chunk.trim().is_empty() {
                sentences.push(chunk.trim());
            }
            start = i + 1;
        } else if c == '.' || c == '!' {
            let next_idx = i + c.len_utf8();
            let is_boundary = next_idx >= text.len()
                || text[next_idx..].starts_with(' ')
                || text[next_idx..].starts_with('\n');
            if is_boundary {
                let chunk = &text[start..=i];
                if !chunk.trim().is_empty() {
                    sentences.push(chunk.trim());
                }
                start = next_idx;
            }
        }
    }

    let remainder = &text[start..];
    if !remainder.trim().is_empty() {
        sentences.push(remainder.trim());
    }

    sentences
}

/// Check if a sentence looks like a learnable fact.
/// Uses keyword heuristics -- no LLM call needed.
fn is_factual(sentence: &str) -> bool {
    let lower = sentence.to_lowercase();

    let fact_indicators = [
        " is ",
        " are ",
        " was ",
        " were ",
        " has ",
        " have ",
        " uses ",
        " runs ",
        " located ",
        " stored ",
        " configured ",
        " installed ",
        " requires ",
        " supports ",
        " provides ",
        " connects ",
        " listens ",
        " serves ",
        " defaults ",
        " means ",
        " called ",
        " named ",
        " known as ",
    ];

    let correction_indicators = [
        "actually ",
        "correct ",
        "instead ",
        "should be ",
        "not ",
        "rather ",
        "fixed ",
        "changed ",
        "updated ",
        "renamed ",
        "moved ",
        "replaced ",
    ];

    let preference_indicators = ["prefer", "always ", "never ", "like ", "want ", "don't "];

    fact_indicators.iter().any(|ind| lower.contains(ind))
        || correction_indicators.iter().any(|ind| lower.contains(ind))
        || preference_indicators.iter().any(|ind| lower.contains(ind))
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Sentence splitting (preserved from original) ──────────────

    #[test]
    fn test_split_sentences_basic() {
        let text = "Hello world. This is a test. Another sentence!";
        let sentences = split_sentences(text);
        assert_eq!(sentences.len(), 3);
        assert_eq!(sentences[0], "Hello world.");
        assert_eq!(sentences[1], "This is a test.");
        assert_eq!(sentences[2], "Another sentence!");
    }

    #[test]
    fn test_split_sentences_newlines() {
        let text = "Line one\nLine two\nLine three";
        let sentences = split_sentences(text);
        assert_eq!(sentences.len(), 3);
    }

    #[test]
    fn test_split_sentences_empty() {
        let sentences = split_sentences("");
        assert!(sentences.is_empty());
    }

    // ── Factual detection (preserved from original) ──────────────

    #[test]
    fn test_is_factual_fact() {
        assert!(is_factual("The server is running on port 8080"));
        assert!(is_factual("Docker containers are managed by compose"));
        assert!(is_factual("The config file uses TOML format"));
    }

    #[test]
    fn test_is_factual_correction() {
        assert!(is_factual("Actually the port should be 18794"));
        assert!(is_factual("The service was renamed to Ironjaw"));
    }

    #[test]
    fn test_is_factual_preference() {
        assert!(is_factual("You prefer using Rust for systems code"));
        assert!(is_factual("Never use sudo without checking first"));
    }

    #[test]
    fn test_is_factual_negative() {
        assert!(!is_factual("OK"));
        assert!(!is_factual("Sure"));
        assert!(!is_factual("Let me help you with that"));
    }

    // ── Extraction prompt building ───────────────────────────────

    #[test]
    fn test_extraction_prompt_building() {
        let messages = vec![
            (
                "user".to_string(),
                "What port does Ironjaw run on?".to_string(),
            ),
            (
                "assistant".to_string(),
                "Ironjaw runs on port 18794.".to_string(),
            ),
        ];

        let prompt = Learner::build_extraction_prompt(&messages);

        assert!(prompt.contains("Extract learnable facts"));
        assert!(prompt.contains("JSON array"));
        assert!(prompt.contains("[user]: What port does Ironjaw run on?"));
        assert!(prompt.contains("[assistant]: Ironjaw runs on port 18794."));
        assert!(prompt.contains("\"content\""));
        assert!(prompt.contains("\"type\""));
        assert!(prompt.contains("\"confidence\""));
        assert!(prompt.contains("\"tags\""));
        assert!(prompt.contains("\"supersedes\""));
        // Specificity guidance
        assert!(prompt.contains("SPECIFIC values"));
        assert!(prompt.contains("REJECT"));
        assert!(prompt.contains("EXAMPLES"));
        assert!(prompt.contains("BAD:"));
        assert!(prompt.contains("GOOD:"));
    }

    // ── LLM response parsing ─────────────────────────────────────

    #[test]
    fn test_parse_llm_response_valid() {
        let response = r#"[
            {
                "content": "Ironjaw runs on port 18794",
                "type": "fact",
                "confidence": 0.95,
                "tags": ["infrastructure", "ironjaw"],
                "supersedes": null
            },
            {
                "content": "The user prefers Rust for systems code",
                "type": "preference",
                "confidence": 0.8,
                "tags": ["coding"],
                "supersedes": null
            }
        ]"#;

        let candidates = Learner::extract_with_llm(response);
        assert_eq!(candidates.len(), 2);

        assert_eq!(candidates[0].content, "Ironjaw runs on port 18794");
        assert_eq!(candidates[0].memory_type, MemoryType::Fact);
        assert!((candidates[0].confidence - 0.95).abs() < 0.001);
        assert_eq!(candidates[0].tags, vec!["infrastructure", "ironjaw"]);
        assert!(candidates[0].supersedes_hint.is_none());

        assert_eq!(
            candidates[1].content,
            "The user prefers Rust for systems code"
        );
        assert_eq!(candidates[1].memory_type, MemoryType::Preference);
    }

    #[test]
    fn test_parse_llm_response_with_markdown_fences() {
        let response = "Here are the extracted facts:\n\n```json\n[\n  {\n    \"content\": \"workstation runs Ironjaw on port 18794\",\n    \"type\": \"fact\",\n    \"confidence\": 0.9,\n    \"tags\": [\"infrastructure\"],\n    \"supersedes\": null\n  }\n]\n```\n";

        let candidates = Learner::extract_with_llm(response);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].content,
            "workstation runs Ironjaw on port 18794"
        );
    }

    #[test]
    fn test_parse_llm_response_with_plain_fences() {
        let response = "```\n[\n  {\n    \"content\": \"Test fact\",\n    \"type\": \"fact\",\n    \"confidence\": 0.7,\n    \"tags\": [],\n    \"supersedes\": null\n  }\n]\n```";

        let candidates = Learner::extract_with_llm(response);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].content, "Test fact");
    }

    #[test]
    fn test_parse_llm_response_with_trailing_commas() {
        let response = r#"[
            {
                "content": "Trailing comma test",
                "type": "fact",
                "confidence": 0.8,
                "tags": ["test",],
                "supersedes": null,
            },
        ]"#;

        let candidates = Learner::extract_with_llm(response);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].content, "Trailing comma test");
    }

    #[test]
    fn test_parse_llm_response_invalid_fallback() {
        let response = "I couldn't extract any facts from this conversation.";
        let candidates = Learner::extract_with_llm(response);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_parse_llm_response_empty_content_filtered() {
        let response = r#"[
            {"content": "", "type": "fact", "confidence": 0.5, "tags": [], "supersedes": null},
            {"content": "Valid fact here", "type": "fact", "confidence": 0.8, "tags": [], "supersedes": null}
        ]"#;

        let candidates = Learner::extract_with_llm(response);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].content, "Valid fact here");
    }

    #[test]
    fn test_parse_llm_response_unknown_type_defaults_to_fact() {
        let response = r#"[
            {"content": "Some info", "type": "unknown_type", "confidence": 0.5, "tags": [], "supersedes": null}
        ]"#;

        let candidates = Learner::extract_with_llm(response);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].memory_type, MemoryType::Fact);
    }

    #[test]
    fn test_parse_llm_response_missing_optional_fields() {
        let response = r#"[
            {"content": "Minimal entry", "type": "event"}
        ]"#;

        let candidates = Learner::extract_with_llm(response);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].memory_type, MemoryType::Event);
        assert!((candidates[0].confidence - 0.5).abs() < 0.001);
        assert!(candidates[0].tags.is_empty());
        assert!(candidates[0].supersedes_hint.is_none());
    }

    // ── Heuristic extraction (adapted from original) ─────────────

    #[test]
    fn test_heuristic_extraction() {
        let messages = vec![
            ("user".to_string(), "Tell me about Docker".to_string()),
            (
                "assistant".to_string(),
                "Docker is a containerization platform. It runs on Linux.\n\
                 # This is a header\n\
                 ```code block```\n\
                 Sure thing!"
                    .to_string(),
            ),
        ];

        let candidates = Learner::extract_heuristic(&messages);
        assert!(!candidates.is_empty());
        assert!(candidates
            .iter()
            .any(|c| c.content.contains("Docker is a containerization platform")));

        // All candidates should have a memory_type
        for c in &candidates {
            // Should be classified as Fact since they use "is" / "runs"
            assert_eq!(c.confidence, 0.5);
        }
    }

    #[test]
    fn test_heuristic_extraction_skips_user() {
        let messages = vec![(
            "user".to_string(),
            "The server is running on port 8080.".to_string(),
        )];
        let candidates = Learner::extract_heuristic(&messages);
        assert!(
            candidates.is_empty(),
            "should not extract from user messages"
        );
    }

    #[test]
    fn test_heuristic_extraction_skips_short() {
        let messages = vec![("assistant".to_string(), "OK.".to_string())];
        let candidates = Learner::extract_heuristic(&messages);
        assert!(candidates.is_empty(), "should skip very short sentences");
    }

    #[test]
    fn test_heuristic_classifies_correction() {
        let messages = vec![(
            "assistant".to_string(),
            "Actually the port should be 18794 instead of 8080.".to_string(),
        )];
        let candidates = Learner::extract_heuristic(&messages);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].memory_type, MemoryType::Correction);
    }

    #[test]
    fn test_heuristic_classifies_preference() {
        let messages = vec![(
            "assistant".to_string(),
            "You prefer using Rust for all systems programming work.".to_string(),
        )];
        let candidates = Learner::extract_heuristic(&messages);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].memory_type, MemoryType::Preference);
    }

    // ── Write gate tests ─────────────────────────────────────────

    #[test]
    fn test_content_hash_deterministic() {
        let hash1 = WriteGate::content_hash("test content");
        let hash2 = WriteGate::content_hash("test content");
        assert_eq!(hash1, hash2);

        let hash3 = WriteGate::content_hash("different content");
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_content_hash_is_hex() {
        let hash = WriteGate::content_hash("hello world");
        // Double-hash hex = 32 characters (two u64 hashes)
        assert_eq!(hash.len(), 32);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn test_write_gate_exact_duplicate() {
        let store = crate::store::BrainStore::open_memory().await.unwrap();

        // Store a memory with a known hash
        let hash = WriteGate::content_hash("The server runs on port 18794");
        let mem = NewMemory {
            content: "The server runs on port 18794".to_string(),
            source: Some("auto-learn".to_string()),
            tags: None,
            embedding: None,
            memory_type: Some("fact".to_string()),
            importance: Some(0.5),
            strength: Some(1.0),
            supersedes: None,
            content_hash: Some(hash.clone()),
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
        store.add_memory(&mem).await.unwrap();

        // Check for duplicate
        let is_dup = WriteGate::check_exact_duplicate(&store, &hash)
            .await
            .unwrap();
        assert!(is_dup, "should detect exact duplicate by content hash");

        // Different hash should not be a duplicate
        let other_hash = WriteGate::content_hash("Something else entirely");
        let is_dup = WriteGate::check_exact_duplicate(&store, &other_hash)
            .await
            .unwrap();
        assert!(!is_dup, "different hash should not be a duplicate");
    }

    #[tokio::test]
    async fn test_write_gate_novel_memory() {
        let store = crate::store::BrainStore::open_memory().await.unwrap();
        let embedder = crate::embedder::Embedder::mock(64);
        let brain = Brain::new(store, embedder, crate::brain::BrainConfig::default());

        let candidate = LearnCandidate {
            content: "Completely novel fact about quantum computing.".to_string(),
            memory_type: MemoryType::Fact,
            confidence: 0.9,
            tags: vec!["science".to_string()],
            supersedes_hint: None,
            trust_level: None,
        };

        let hash = WriteGate::content_hash(&candidate.content);
        let result = WriteGate::gate(&brain, brain.store(), &candidate, &hash)
            .await
            .unwrap();

        assert_eq!(result, GateResult::Store, "novel memory should be stored");
    }

    #[tokio::test]
    async fn test_gate_result_skip_low_novelty() {
        let store = crate::store::BrainStore::open_memory().await.unwrap();
        let embedder = crate::embedder::Embedder::mock(64);
        let brain = Brain::new(store, embedder, crate::brain::BrainConfig::default());

        // Store a memory first
        brain
            .remember(
                "The server runs on port 18794.",
                Some("user"),
                Some("infra"),
            )
            .await
            .unwrap();

        // Try to store the exact same content (same text = same mock embedding = similarity 1.0)
        let candidate = LearnCandidate {
            content: "The server runs on port 18794.".to_string(),
            memory_type: MemoryType::Fact,
            confidence: 0.9,
            tags: vec!["infra".to_string()],
            supersedes_hint: None,
            trust_level: None,
        };

        let hash = WriteGate::content_hash(&candidate.content);
        let result = WriteGate::gate(&brain, brain.store(), &candidate, &hash)
            .await
            .unwrap();

        // Identical content -> exact-hash duplicate -> Skip carrying the
        // blocking memory's id so callers can supersede it.
        assert!(
            matches!(result, GateResult::Skip(Some(_))),
            "identical content should be skipped with the blocking id, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_gate_low_novelty_skip_carries_nearest_id() {
        let store = crate::store::BrainStore::open_memory().await.unwrap();
        let embedder = crate::embedder::Embedder::mock(64);
        let brain = Brain::new(store, embedder, crate::brain::BrainConfig::default());

        let (stored, _) = brain
            .remember(
                "The server runs on port 18794.",
                Some("user"),
                Some("infra"),
            )
            .await
            .unwrap();

        // Different hash (no exact-dup hit) but identical mock embedding ->
        // novelty 0.0 -> low-novelty Skip must name the nearest memory.
        let candidate = LearnCandidate {
            content: "The server runs on port 18794.".to_string(),
            memory_type: MemoryType::Fact,
            confidence: 0.9,
            tags: vec!["infra".to_string()],
            supersedes_hint: None,
            trust_level: None,
        };
        let other_hash = WriteGate::content_hash("not the stored content");
        let result = WriteGate::gate(&brain, brain.store(), &candidate, &other_hash)
            .await
            .unwrap();

        assert_eq!(
            result,
            GateResult::Skip(Some(stored.id)),
            "low-novelty skip should carry the nearest memory id"
        );
    }

    #[tokio::test]
    async fn test_gate_tag_supersession_beats_similarity_rejection() {
        let store = crate::store::BrainStore::open_memory().await.unwrap();
        let embedder = crate::embedder::Embedder::mock(64);
        let brain = Brain::new(store, embedder, crate::brain::BrainConfig::default());

        let (stored, _) = brain
            .remember_full(&mut crate::types::NewMemory {
                content: "The server runs on port 18794.".to_string(),
                source: Some("memory/infra.md".to_string()),
                tags: Some("source:memory/infra.md,memory-file".to_string()),
                embedding: None,
                memory_type: Some("context".to_string()),
                importance: Some(0.7),
                strength: Some(1.0),
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
            })
            .await
            .unwrap();

        // Same content (novelty 0.0 under the mock embedder) but a supersedes
        // hint + overlapping tags: the tag-supersession check must win over
        // the similarity rejection.
        let candidate = LearnCandidate {
            content: "The server runs on port 18794.".to_string(),
            memory_type: MemoryType::Fact,
            confidence: 0.9,
            tags: vec![
                "source:memory/infra.md".to_string(),
                "memory-file".to_string(),
            ],
            supersedes_hint: Some("source:memory/infra.md".to_string()),
            trust_level: None,
        };
        let other_hash = WriteGate::content_hash("edited content, same topic");
        let result = WriteGate::gate(&brain, brain.store(), &candidate, &other_hash)
            .await
            .unwrap();

        assert_eq!(
            result,
            GateResult::Supersede(stored.id),
            "tag-hinted supersession should beat the similarity rejection"
        );
    }

    // ── Contradiction detection ──────────────────────────────────

    #[test]
    fn test_detect_contradiction_with_negation() {
        assert!(WriteGate::detect_contradiction(
            "The port is actually 18794, not 8080",
            0.90,
            0.85,
        ));
    }

    #[test]
    fn test_detect_contradiction_no_negation() {
        assert!(!WriteGate::detect_contradiction(
            "The port is 18794",
            0.90,
            0.85,
        ));
    }

    #[test]
    fn test_detect_contradiction_low_similarity() {
        assert!(!WriteGate::detect_contradiction(
            "Actually the port is wrong",
            0.50,
            0.85,
        ));
    }

    #[test]
    fn test_detect_contradiction_various_signals() {
        let signals = [
            "It is no longer running on that port",
            "Instead of 8080, use 18794",
            "Changed from the old configuration",
            "The previous setting was wrong",
            "That information is incorrect",
            "The service was renamed to Ironjaw",
            "The port should be 18794",
            "The old port was replaced by the new one",
        ];

        for signal in &signals {
            assert!(
                WriteGate::detect_contradiction(signal, 0.90, 0.85),
                "should detect contradiction in: {signal}"
            );
        }
    }

    // ── Learn report counts ──────────────────────────────────────

    #[tokio::test]
    async fn test_learn_report_counts() {
        let store = crate::store::BrainStore::open_memory().await.unwrap();
        let embedder = crate::embedder::Embedder::mock(64);
        let brain = Brain::new(store, embedder, crate::brain::BrainConfig::default());
        let learner = Learner::new(false);

        let messages = vec![
            ("user".to_string(), "What port?".to_string()),
            (
                "assistant".to_string(),
                "The server is running on port 18794.".to_string(),
            ),
        ];

        let report = learner.learn(&brain, &messages, None, 0.85).await.unwrap();
        assert!(report.extracted > 0, "should have extracted candidates");
        assert_eq!(
            report.stored, report.extracted,
            "all should be stored (no prior memories)"
        );
        assert_eq!(report.deduplicated, 0);
        assert_eq!(report.superseded, 0);

        // Second run -- same content, should deduplicate
        let report2 = learner.learn(&brain, &messages, None, 0.85).await.unwrap();
        assert!(report2.extracted > 0);
        assert_eq!(report2.stored, 0, "nothing new should be stored");
        assert!(report2.deduplicated > 0, "should have deduplicated");
    }

    #[tokio::test]
    async fn test_learn_empty() {
        let store = crate::store::BrainStore::open_memory().await.unwrap();
        let embedder = crate::embedder::Embedder::mock(64);
        let brain = Brain::new(store, embedder, crate::brain::BrainConfig::default());
        let learner = Learner::default();

        let messages: Vec<(String, String)> = vec![];
        let report = learner.learn(&brain, &messages, None, 0.85).await.unwrap();
        assert_eq!(report.extracted, 0);
        assert_eq!(report.stored, 0);
        assert_eq!(report.superseded, 0);
    }

    #[tokio::test]
    async fn test_learn_stores_novel() {
        let store = crate::store::BrainStore::open_memory().await.unwrap();
        let embedder = crate::embedder::Embedder::mock(64);
        let brain = Brain::new(store, embedder, crate::brain::BrainConfig::default());
        let learner = Learner::new(false);

        let messages = vec![
            ("user".to_string(), "What port?".to_string()),
            (
                "assistant".to_string(),
                "The server is running on port 18794.".to_string(),
            ),
        ];

        let report = learner.learn(&brain, &messages, None, 0.85).await.unwrap();
        assert_eq!(report.stored, 1);

        let count = brain.store().count_memories().await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_learn_with_llm_response() {
        let store = crate::store::BrainStore::open_memory().await.unwrap();
        let embedder = crate::embedder::Embedder::mock(64);
        let brain = Brain::new(store, embedder, crate::brain::BrainConfig::default());
        let learner = Learner::new(true);

        let messages = vec![
            ("user".to_string(), "Tell me about Docker".to_string()),
            ("assistant".to_string(), "Docker runs on Linux.".to_string()),
        ];

        let llm_response = r#"[
            {"content": "Docker is a container runtime", "type": "fact", "confidence": 0.95, "tags": ["docker", "infrastructure"], "supersedes": null}
        ]"#;

        let report = learner
            .learn(&brain, &messages, Some(llm_response), 0.85)
            .await
            .unwrap();
        assert_eq!(report.extracted, 1);
        assert_eq!(report.stored, 1);

        // Verify stored memory has correct metadata
        let memories = brain.store().all_memories().await.unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].content, "Docker is a container runtime");
        assert_eq!(memories[0].memory_type, "fact");
        assert!(memories[0].content_hash.is_some());
    }

    // ── Strip markdown fences ────────────────────────────────────

    #[test]
    fn test_strip_markdown_fences_json() {
        let input = "```json\n[{\"a\": 1}]\n```";
        let result = strip_markdown_fences(input);
        assert_eq!(result, "[{\"a\": 1}]");
    }

    #[test]
    fn test_strip_markdown_fences_plain() {
        let input = "```\n[{\"a\": 1}]\n```";
        let result = strip_markdown_fences(input);
        assert_eq!(result, "[{\"a\": 1}]");
    }

    #[test]
    fn test_strip_markdown_fences_none() {
        let input = "[{\"a\": 1}]";
        let result = strip_markdown_fences(input);
        assert_eq!(result, "[{\"a\": 1}]");
    }

    // ── Strip trailing commas ────────────────────────────────────

    #[test]
    fn test_strip_trailing_commas() {
        let input = r#"[{"a": 1,}, {"b": 2,},]"#;
        let result = strip_trailing_commas(input);
        assert_eq!(result, r#"[{"a": 1}, {"b": 2}]"#);
    }

    #[test]
    fn test_strip_trailing_commas_no_trailing() {
        let input = r#"[{"a": 1}, {"b": 2}]"#;
        let result = strip_trailing_commas(input);
        assert_eq!(result, input);
    }

    // ── Classify heuristic ───────────────────────────────────────

    #[test]
    fn test_classify_heuristic_fact() {
        assert_eq!(
            classify_heuristic("The server is running on port 8080"),
            MemoryType::Fact
        );
    }

    #[test]
    fn test_classify_heuristic_correction() {
        assert_eq!(
            classify_heuristic("Actually the port should be 18794"),
            MemoryType::Correction
        );
    }

    #[test]
    fn test_classify_heuristic_preference() {
        assert_eq!(
            classify_heuristic("I prefer using Rust"),
            MemoryType::Preference
        );
    }

    #[test]
    fn test_classify_heuristic_event() {
        assert_eq!(
            classify_heuristic("The migration happened yesterday"),
            MemoryType::Event
        );
    }

    // ── Learner mode tests ───────────────────────────────────────

    #[test]
    fn test_learner_default_no_llm() {
        let learner = Learner::default();
        assert!(!learner.use_llm);
    }

    #[test]
    fn test_learner_new_with_llm() {
        let learner = Learner::new(true);
        assert!(learner.use_llm);
    }

    #[test]
    fn test_extract_uses_heuristic_when_llm_disabled() {
        let learner = Learner::new(false);
        let messages = vec![(
            "assistant".to_string(),
            "The server is running on port 18794.".to_string(),
        )];

        // Even with LLM response provided, should use heuristic when use_llm=false
        let candidates = learner.extract(
            &messages,
            Some("[{\"content\":\"LLM fact\",\"type\":\"fact\"}]"),
        );
        assert!(candidates
            .iter()
            .any(|c| c.content.contains("server is running")));
    }

    #[test]
    fn test_extract_uses_llm_when_enabled() {
        let learner = Learner::new(true);
        let messages = vec![(
            "assistant".to_string(),
            "The server is running on port 18794.".to_string(),
        )];

        let llm_response = r#"[{"content": "LLM extracted fact", "type": "fact", "confidence": 0.9, "tags": [], "supersedes": null}]"#;
        let candidates = learner.extract(&messages, Some(llm_response));
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].content, "LLM extracted fact");
    }

    #[test]
    fn test_extract_falls_back_to_heuristic_on_bad_llm() {
        let learner = Learner::new(true);
        let messages = vec![(
            "assistant".to_string(),
            "The server is running on port 18794.".to_string(),
        )];

        // Bad LLM response should trigger fallback
        let candidates = learner.extract(&messages, Some("not json at all"));
        assert!(!candidates.is_empty(), "should fall back to heuristic");
        assert!(candidates
            .iter()
            .any(|c| c.content.contains("server is running")));
    }

    // ── Source-aware confidence multiplier tests ─────────────────

    #[test]
    fn test_source_confidence_multiplier_chronicle() {
        let mult = Learner::source_confidence_multiplier(&SourceType::CcChronicle);
        assert!((mult - 1.2).abs() < 0.001);
    }

    #[test]
    fn test_source_confidence_multiplier_brain() {
        let mult = Learner::source_confidence_multiplier(&SourceType::Brain);
        assert!((mult - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_source_confidence_multiplier_session() {
        let mult = Learner::source_confidence_multiplier(&SourceType::CcSession);
        assert!((mult - 0.8).abs() < 0.001);
    }

    #[test]
    fn test_source_importance_adjustment_brain() {
        let adj = Learner::source_importance_adjustment(&SourceType::Brain);
        assert!((adj - 0.10).abs() < 0.001);
    }

    #[test]
    fn test_source_importance_adjustment_session() {
        let adj = Learner::source_importance_adjustment(&SourceType::CcSession);
        assert!((adj - (-0.10)).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_learn_with_source_stores_correct_source_type() {
        let store = crate::store::BrainStore::open_memory().await.unwrap();
        let embedder = crate::embedder::Embedder::mock(64);
        let brain = Brain::new(store, embedder, crate::brain::BrainConfig::default());
        let learner = Learner::new(false);

        let messages = vec![
            ("user".to_string(), "What port?".to_string()),
            (
                "assistant".to_string(),
                "The server is running on port 18794.".to_string(),
            ),
        ];

        let report = learner
            .learn_with_source(&brain, &messages, None, &SourceType::CcChronicle, None)
            .await
            .unwrap();

        assert!(report.stored > 0);

        let memories = brain.store().all_memories().await.unwrap();
        assert!(!memories.is_empty());
        assert_eq!(
            memories[0].source_type, "cc_chronicle",
            "should store with correct source_type"
        );
    }

    #[tokio::test]
    async fn test_learn_with_source_adjusts_importance() {
        let store = crate::store::BrainStore::open_memory().await.unwrap();
        let brain_brain = Brain::new(
            store.clone(),
            crate::embedder::Embedder::mock(64),
            crate::brain::BrainConfig::default(),
        );
        let learner = Learner::new(false);

        let messages = vec![
            ("user".to_string(), "Tell me about Docker".to_string()),
            (
                "assistant".to_string(),
                "Docker is a containerization platform.".to_string(),
            ),
        ];

        // Brain source gives +10% importance boost
        let report = learner
            .learn_with_source(&brain_brain, &messages, None, &SourceType::Brain, None)
            .await
            .unwrap();
        assert!(report.stored > 0);

        let memories = brain_brain.store().all_memories().await.unwrap();
        let brain_importance = memories[0].importance;

        // Now test with CcSession source (-10% penalty) using a fresh store
        let store2 = crate::store::BrainStore::open_memory().await.unwrap();
        let brain_session = Brain::new(
            store2,
            crate::embedder::Embedder::mock(64),
            crate::brain::BrainConfig::default(),
        );

        let report2 = learner
            .learn_with_source(
                &brain_session,
                &messages,
                None,
                &SourceType::CcSession,
                None,
            )
            .await
            .unwrap();
        assert!(report2.stored > 0);

        let memories2 = brain_session.store().all_memories().await.unwrap();
        let session_importance = memories2[0].importance;

        assert!(
            brain_importance > session_importance,
            "Brain source should have higher importance ({brain_importance}) than CcSession ({session_importance})"
        );
    }

    // ── Model self-reference stripping ──────────────────────────

    #[test]
    fn test_strip_model_ref_as_qwen() {
        let input = "As Qwen, I helped the user configure their server.";
        let result = strip_model_references(input);
        assert!(
            !result.to_lowercase().contains("qwen"),
            "should strip 'As Qwen': got '{result}'"
        );
        assert!(result.contains("helped") || result.contains("configure"));
    }

    #[test]
    fn test_strip_model_ref_as_claude() {
        let input = "As Claude, I analyzed the codebase and found three issues.";
        let result = strip_model_references(input);
        assert!(
            !result.to_lowercase().contains("as claude"),
            "should strip 'As Claude': got '{result}'"
        );
    }

    #[test]
    fn test_strip_model_ref_i_am() {
        let input = "I am Qwen, a large language model. The server runs on port 18794.";
        let result = strip_model_references(input);
        assert!(
            !result.to_lowercase().contains("i am qwen"),
            "should strip 'I am Qwen': got '{result}'"
        );
        assert!(result.contains("18794"), "should preserve factual content");
    }

    #[test]
    fn test_strip_model_ref_im_contraction() {
        let input = "I'm Mistral, and I summarized the session.";
        let result = strip_model_references(input);
        assert!(
            !result.to_lowercase().contains("i'm mistral"),
            "should strip \"I'm Mistral\": got '{result}'"
        );
    }

    #[test]
    fn test_strip_model_ref_as_llm() {
        let input = "As a large language model, I cannot access the internet.";
        let result = strip_model_references(input);
        assert!(
            !result.to_lowercase().contains("as a large language model"),
            "should strip 'As a large language model': got '{result}'"
        );
    }

    #[test]
    fn test_strip_model_ref_as_ai_assistant_called() {
        let input = "As an AI assistant called Qwen, I helped with debugging.";
        let result = strip_model_references(input);
        assert!(
            !result
                .to_lowercase()
                .contains("as an ai assistant called qwen"),
            "should strip 'As an AI assistant called Qwen': got '{result}'"
        );
    }

    #[test]
    fn test_strip_model_ref_my_name_is() {
        let input = "My name is DeepSeek. Here is the summary of the session.";
        let result = strip_model_references(input);
        assert!(
            !result.to_lowercase().contains("my name is deepseek"),
            "should strip 'My name is DeepSeek': got '{result}'"
        );
    }

    #[test]
    fn test_strip_model_ref_preserves_third_person() {
        let input = "The user asked about Llama models and their performance.";
        let result = strip_model_references(input);
        assert_eq!(result, input, "should NOT strip third-person references");
    }

    #[test]
    fn test_strip_model_ref_preserves_discussion() {
        let input = "Discussed switching from GPT to Mistral for local inference.";
        let result = strip_model_references(input);
        assert_eq!(result, input, "should NOT strip discussion references");
    }

    #[test]
    fn test_strip_model_ref_preserves_normal_text() {
        let input = "- Configured llama-server with --parallel 4\n- Fixed the auth bug\n- Deployed to docker-host";
        let result = strip_model_references(input);
        assert_eq!(
            result, input,
            "should preserve normal bullet-point summaries"
        );
    }

    #[test]
    fn test_strip_model_ref_multiple_occurrences() {
        let input =
            "As Qwen, I analyzed the code. I'm Qwen, a helpful assistant. The port is 18794.";
        let result = strip_model_references(input);
        assert!(
            !result.to_lowercase().contains("as qwen"),
            "should strip all self-references: got '{result}'"
        );
        assert!(result.contains("18794"), "should preserve facts");
    }

    #[test]
    fn test_strip_model_ref_empty_input() {
        assert_eq!(strip_model_references(""), "");
    }

    #[test]
    fn test_strip_model_ref_case_insensitive() {
        let input = "As QWEN, I helped debug the issue.";
        let result = strip_model_references(input);
        assert!(
            !result.to_lowercase().contains("as qwen"),
            "should be case-insensitive: got '{result}'"
        );
    }

    #[test]
    fn test_strip_model_ref_with_version_suffix() {
        let input = "As Qwen-2.5, I summarized the findings.";
        let result = strip_model_references(input);
        assert!(
            !result.to_lowercase().contains("as qwen"),
            "should strip model names with version suffixes: got '{result}'"
        );
    }

    #[test]
    fn test_strip_model_ref_deepseek() {
        let input = "As DeepSeek, I reviewed the implementation.";
        let result = strip_model_references(input);
        assert!(
            !result.to_lowercase().contains("as deepseek"),
            "should strip DeepSeek: got '{result}'"
        );
    }

    #[test]
    fn test_strip_model_ref_multiline_summary() {
        let input = "As Qwen, I found the following:\n- Bug in auth module\n- Missing test coverage\n- Port misconfiguration";
        let result = strip_model_references(input);
        assert!(
            !result.to_lowercase().contains("as qwen"),
            "should strip from multiline: got '{result}'"
        );
        assert!(result.contains("Bug in auth module"));
        assert!(result.contains("Missing test coverage"));
        assert!(result.contains("Port misconfiguration"));
    }
}
