use crate::store::BrainStore;
use crate::types::{NewPendingMemory, SourceType};
use chrono::NaiveDateTime;
use ironjaw_llm::types::{ChatMessage, ChatRequest, Role};
use ironjaw_llm::LlmRouter;
use tokio_stream::StreamExt;
use tracing::{info, warn};

/// Memory maintenance operations: decay, pruning, deduplication, cleanup.
pub struct MemoryMaintenance {
    store: BrainStore,
}

impl MemoryMaintenance {
    pub fn new(store: BrainStore) -> Self {
        Self { store }
    }

    /// Decay strength of all active memories using Ebbinghaus forgetting curve.
    ///
    /// Formula: `new_strength = strength * (decay_factor ^ days_since_last_access)`
    /// Default decay_factor is 0.95 (called from `run_all`).
    /// Skips memories already below 0.05 strength.
    pub async fn decay_strength(&self, decay_factor: f64) -> anyhow::Result<u64> {
        let memories = self.store.get_active_memories_for_decay().await?;
        let now = chrono::Utc::now().naive_utc();

        let mut updates: Vec<(i64, f64)> = Vec::new();
        for (id, strength, accessed_at) in &memories {
            if *strength < 0.05 {
                continue;
            }

            let days = parse_days_since(accessed_at, now);
            if days < 0.001 {
                continue;
            }

            let new_strength = strength * decay_factor.powf(days);
            updates.push((*id, new_strength));
        }

        let count = updates.len() as u64;
        if !updates.is_empty() {
            self.store.update_strength(&updates).await?;
            info!(count, decay_factor, "Decayed memory strength");
        }
        Ok(count)
    }

    /// 3-tier salience decay based on recency and access frequency.
    ///
    /// - **Hot** (accessed < `hot_days` OR access_count >= 10): decay = 0.99
    /// - **Warm** (accessed < `warm_days` OR access_count >= 3): decay = 0.95
    /// - **Cold** (everything else): decay = 0.90
    ///
    /// Skips memories already below 0.05 strength.
    pub async fn decay_strength_tiered_with(
        &self,
        hot_days: u64,
        warm_days: u64,
    ) -> anyhow::Result<u64> {
        let memories = self.store.get_active_memories_for_tiered_decay().await?;
        let now = chrono::Utc::now().naive_utc();

        let hot_days_f = hot_days as f64;
        let warm_days_f = warm_days as f64;

        let mut updates: Vec<(i64, f64)> = Vec::new();
        for (id, strength, accessed_at, access_count) in &memories {
            if *strength < 0.05 {
                continue;
            }

            let days = parse_days_since(accessed_at, now);
            if days < 0.001 {
                continue;
            }

            // Determine decay tier
            let decay_factor: f64 = if days < hot_days_f || *access_count >= 10 {
                0.99 // Hot
            } else if days < warm_days_f || *access_count >= 3 {
                0.95 // Warm
            } else {
                0.90 // Cold
            };

            let new_strength = strength * decay_factor.powf(days);
            updates.push((*id, new_strength));
        }

        let count = updates.len() as u64;
        if !updates.is_empty() {
            self.store.update_strength(&updates).await?;
            info!(
                count,
                hot_days, warm_days, "Decayed memory strength (tiered)"
            );
        }
        Ok(count)
    }

    /// 3-tier salience decay using default thresholds (7 hot / 30 warm).
    ///
    /// Convenience wrapper around [`decay_strength_tiered_with`].
    pub async fn decay_strength_tiered(&self) -> anyhow::Result<u64> {
        self.decay_strength_tiered_with(7, 30).await
    }

    /// Soft-delete memories with importance below `threshold`, or with
    /// very low strength (< 0.05) and low access count (< 3).
    pub async fn prune_low_importance(&self, threshold: f64) -> anyhow::Result<u64> {
        // Prune by low importance. File-backed memories are exempt — see
        // `is_file_backed`.
        let result_importance = sqlx::query(
            "UPDATE memories SET deleted = 1 \
             WHERE deleted = 0 AND importance < ?1 AND source_type != 'markdown'",
        )
        .bind(threshold)
        .execute(self.store.pool())
        .await?;

        // Prune by low strength + low access count. A memory file is never
        // "accessed", so this rule alone deleted 269 curated chunks.
        let result_strength = sqlx::query(
            "UPDATE memories SET deleted = 1 WHERE deleted = 0 AND strength < 0.05 \
             AND access_count < 3 AND source_type != 'markdown'",
        )
        .execute(self.store.pool())
        .await?;

        let affected = result_importance.rows_affected() + result_strength.rows_affected();
        if affected > 0 {
            info!(
                affected,
                by_importance = result_importance.rows_affected(),
                by_strength = result_strength.rows_affected(),
                threshold,
                "Pruned low-importance/low-strength memories"
            );
        }
        Ok(affected)
    }

    /// Hard-delete memories that have been soft-deleted for more than `retention_days`.
    ///
    /// Uses `accessed_at` as a proxy since there is no `deleted_at` column.
    pub async fn cleanup_deleted(&self, retention_days: u64) -> anyhow::Result<u64> {
        let result = sqlx::query(
            r#"DELETE FROM memories WHERE deleted = 1
               AND accessed_at < strftime('%Y-%m-%dT%H:%M:%f', 'now',
                   '-' || ?1 || ' days')"#,
        )
        .bind(retention_days as i64)
        .execute(self.store.pool())
        .await?;

        let affected = result.rows_affected();
        if affected > 0 {
            info!(affected, retention_days, "Hard-deleted old memories");
        }
        Ok(affected)
    }

    /// Merge near-duplicate memories based on cosine similarity and matching type.
    ///
    /// For each pair with cosine similarity >= 0.92 AND same memory_type:
    /// - Keep the one with higher importance
    /// - Soft-delete the other
    /// - Set supersedes/superseded_by links between them
    pub async fn merge_near_duplicates(&self) -> anyhow::Result<u64> {
        let embeddings = self.store.get_all_embeddings().await?;
        let mut merged = 0u64;
        // Track already-merged IDs to avoid re-merging
        let mut deleted_ids = std::collections::HashSet::new();

        for i in 0..embeddings.len() {
            if deleted_ids.contains(&embeddings[i].0) {
                continue;
            }
            for j in (i + 1)..embeddings.len() {
                if deleted_ids.contains(&embeddings[j].0) {
                    continue;
                }
                let sim = crate::embedder::cosine_similarity(&embeddings[i].1, &embeddings[j].1);
                if f64::from(sim) < 0.92 {
                    continue;
                }

                let mem1 = self.store.get_memory(embeddings[i].0).await?;
                let mem2 = self.store.get_memory(embeddings[j].0).await?;

                match (mem1, mem2) {
                    (Some(m1), Some(m2)) => {
                        // Must have same memory_type
                        if m1.memory_type != m2.memory_type {
                            continue;
                        }

                        // Keep higher importance; tie-break by access_count
                        let (keep, discard) = if m1.importance > m2.importance {
                            (m1, m2)
                        } else if m2.importance > m1.importance {
                            (m2, m1)
                        } else if m1.access_count >= m2.access_count {
                            (m1, m2)
                        } else {
                            (m2, m1)
                        };

                        // Never discard a file-backed memory (see `is_file_backed`)
                        if is_file_backed(&discard.source_type) {
                            continue;
                        }

                        // Soft-delete the loser and set supersession links
                        if self.store.delete_memory(discard.id).await? {
                            self.store.set_superseded_by(discard.id, keep.id).await?;
                            deleted_ids.insert(discard.id);
                            merged += 1;
                        }
                    }
                    _ => continue,
                }
            }
        }

        if merged > 0 {
            info!(merged, "Merged near-duplicate memories");
        }
        Ok(merged)
    }

    /// Find pairs of very similar memories by embedding cosine similarity.
    ///
    /// Returns `(id1, id2, similarity)` for each pair above `threshold`.
    pub async fn find_duplicates(&self, threshold: f64) -> anyhow::Result<Vec<(i64, i64, f64)>> {
        let embeddings = self.store.get_all_embeddings().await?;
        let mut duplicates = Vec::new();

        for i in 0..embeddings.len() {
            for j in (i + 1)..embeddings.len() {
                let sim = crate::embedder::cosine_similarity(&embeddings[i].1, &embeddings[j].1);
                if f64::from(sim) >= threshold {
                    duplicates.push((embeddings[i].0, embeddings[j].0, f64::from(sim)));
                }
            }
        }

        if !duplicates.is_empty() {
            info!(
                count = duplicates.len(),
                threshold, "Found duplicate memories"
            );
        }
        Ok(duplicates)
    }

    /// Deduplicate by soft-deleting the lower-importance member of each pair.
    ///
    /// Tie-breaker: higher access_count wins.
    pub async fn deduplicate(&self, threshold: f64) -> anyhow::Result<u64> {
        let pairs = self.find_duplicates(threshold).await?;
        let mut deleted = 0u64;

        for (id1, id2, _sim) in &pairs {
            let mem1 = self.store.get_memory(*id1).await?;
            let mem2 = self.store.get_memory(*id2).await?;

            match (mem1, mem2) {
                (Some(m1), Some(m2)) => {
                    // Keep the one with higher importance; tie-break by access_count
                    let discard = if m1.importance > m2.importance {
                        m2
                    } else if m2.importance > m1.importance {
                        m1
                    } else if m1.access_count >= m2.access_count {
                        m2
                    } else {
                        m1
                    };
                    // Never discard a file-backed memory (see `is_file_backed`)
                    if is_file_backed(&discard.source_type) {
                        continue;
                    }
                    let delete_id = discard.id;
                    if self.store.delete_memory(delete_id).await? {
                        deleted += 1;
                    }
                }
                _ => continue,
            }
        }

        if deleted > 0 {
            info!(deleted, "Deduplicated memories");
        }
        Ok(deleted)
    }

    /// Boost importance of frequently accessed memories.
    ///
    /// Memories accessed 5+ times get a 10% importance boost,
    /// capped at 1.0.
    pub async fn boost_frequently_accessed(&self) -> anyhow::Result<u64> {
        let frequent = self.store.get_frequently_accessed(5).await?;
        let mut boosted = 0u64;

        for (id, importance) in &frequent {
            let new_importance = (importance * 1.10).min(1.0);
            if (new_importance - importance).abs() > f64::EPSILON {
                self.store.update_importance(*id, new_importance).await?;
                boosted += 1;
            }
        }

        if boosted > 0 {
            info!(boosted, "Boosted frequently-accessed memory importance");
        }
        Ok(boosted)
    }

    /// Decay multiplier based on memory source type.
    ///
    /// Some memory types should persist longer:
    /// - Corrections decay at 0.5x normal rate (important to remember)
    /// - Decisions decay at 0.7x normal rate
    /// - Session summaries decay at 1.5x normal rate (less important over time)
    /// - Facts/other: normal (1.0x) decay rate
    pub fn decay_multiplier_for_memory_type(memory_type: &str) -> f64 {
        match memory_type {
            "correction" => 0.5,
            "decision" => 0.7,
            "preference" => 0.7,
            "identity" => 0.3,
            "session_summary" => 1.5,
            "observation" => 1.3,
            "context" => 1.5,
            _ => 1.0,
        }
    }

    /// Decay multiplier based on memory source type.
    ///
    /// More authoritative sources decay slower:
    /// - Brain (direct user input): 0.8x (slower decay)
    /// - CcChronicle (structured summaries): 0.9x
    /// - Markdown (curated notes): 0.85x
    /// - CcBridge / CcSession: 1.0x (normal)
    pub fn decay_multiplier_for_source(source_type: &SourceType) -> f64 {
        match source_type {
            SourceType::Brain => 0.8,
            SourceType::Markdown => 0.85,
            SourceType::CcChronicle => 0.9,
            SourceType::CcBridge | SourceType::CcSession => 1.0,
        }
    }

    /// Source-aware tiered decay: applies both tier-based and source/type multipliers.
    ///
    /// The effective decay factor is: `base_tier_decay ^ (days * type_mult * source_mult)`
    pub async fn decay_strength_source_aware(&self) -> anyhow::Result<u64> {
        let memories = self.store.get_active_memories_for_tiered_decay().await?;
        let now = chrono::Utc::now().naive_utc();

        let mut updates: Vec<(i64, f64)> = Vec::new();
        for (id, strength, accessed_at, access_count) in &memories {
            if *strength < 0.05 {
                continue;
            }

            let days = parse_days_since(accessed_at, now);
            if days < 0.001 {
                continue;
            }

            // Determine base tier decay
            let base_decay: f64 = if days < 7.0 || *access_count >= 10 {
                0.99 // Hot
            } else if days < 30.0 || *access_count >= 3 {
                0.95 // Warm
            } else {
                0.90 // Cold
            };

            // Load memory to get memory_type and source_type
            let (type_mult, source_mult) = match self.store.get_memory(*id).await {
                Ok(Some(mem)) => {
                    let type_mult = Self::decay_multiplier_for_memory_type(&mem.memory_type);
                    let source_mult = match mem.source_type.parse::<SourceType>() {
                        Ok(st) => Self::decay_multiplier_for_source(&st),
                        Err(_) => 1.0,
                    };
                    (type_mult, source_mult)
                }
                _ => (1.0, 1.0),
            };

            let effective_days = days * type_mult * source_mult;
            let new_strength = strength * base_decay.powf(effective_days);
            updates.push((*id, new_strength));
        }

        let count = updates.len() as u64;
        if !updates.is_empty() {
            self.store.update_strength(&updates).await?;
            info!(count, "Decayed memory strength (source-aware)");
        }
        Ok(count)
    }

    /// Cross-source deduplication during consolidation.
    ///
    /// Finds near-duplicate memories ACROSS source types. When merging:
    /// - Keeps the memory from the more authoritative source
    /// - Authority: brain > markdown > cc_chronicle > cc_bridge > cc_session
    /// - Links merged memories via supersedes/superseded_by
    pub async fn merge_cross_source_duplicates(&self) -> anyhow::Result<u64> {
        let embeddings = self.store.get_all_embeddings().await?;
        let mut merged = 0u64;
        let mut deleted_ids = std::collections::HashSet::new();

        for i in 0..embeddings.len() {
            if deleted_ids.contains(&embeddings[i].0) {
                continue;
            }
            for j in (i + 1)..embeddings.len() {
                if deleted_ids.contains(&embeddings[j].0) {
                    continue;
                }
                let sim = crate::embedder::cosine_similarity(&embeddings[i].1, &embeddings[j].1);
                if f64::from(sim) < 0.92 {
                    continue;
                }

                let mem1 = self.store.get_memory(embeddings[i].0).await?;
                let mem2 = self.store.get_memory(embeddings[j].0).await?;

                match (mem1, mem2) {
                    (Some(m1), Some(m2)) => {
                        // Only merge across different source types
                        if m1.source_type == m2.source_type {
                            continue;
                        }

                        let (keep, discard) = if source_authority(&m1.source_type)
                            >= source_authority(&m2.source_type)
                        {
                            (m1, m2)
                        } else {
                            (m2, m1)
                        };

                        // Never discard a file-backed memory (see `is_file_backed`).
                        // `brain` outranks `markdown` in source_authority, so a
                        // 300-char memory_to_brain.py stub would otherwise delete
                        // the full-content chunk of the same file.
                        if is_file_backed(&discard.source_type) {
                            continue;
                        }

                        if self.store.delete_memory(discard.id).await? {
                            self.store.set_superseded_by(discard.id, keep.id).await?;
                            deleted_ids.insert(discard.id);
                            merged += 1;
                        }
                    }
                    _ => continue,
                }
            }
        }

        if merged > 0 {
            info!(merged, "Merged cross-source duplicate memories");
        }
        Ok(merged)
    }

    /// Run full consolidation with cross-source awareness.
    ///
    /// Returns a `ConsolidationReport` with detailed counts.
    pub async fn consolidate(&self) -> anyhow::Result<ConsolidationReport> {
        let decayed = self.decay_strength_source_aware().await.unwrap_or(0);
        let pruned = self.prune_low_importance(0.1).await.unwrap_or(0);
        let deduped = self.merge_near_duplicates().await.unwrap_or(0);
        let cross_source_merged = self.merge_cross_source_duplicates().await.unwrap_or(0);

        let report = ConsolidationReport {
            decayed,
            deduped,
            pruned,
            cross_source_merged,
        };
        info!(?report, "Consolidation complete");
        Ok(report)
    }

    /// Resolve contradictions between memories using trust hierarchy.
    ///
    /// For each pair of near-duplicate memories (cosine >= 0.92) with the same
    /// memory_type: the memory with higher trust_level wins. Equal trust → the
    /// newer memory (higher ID) wins. The loser is soft-deleted and linked via
    /// supersedes/superseded_by.
    pub async fn resolve_by_trust(&self) -> anyhow::Result<u64> {
        let embeddings = self.store.get_all_embeddings().await?;
        let mut resolved = 0u64;
        let mut deleted_ids = std::collections::HashSet::new();

        for i in 0..embeddings.len() {
            if deleted_ids.contains(&embeddings[i].0) {
                continue;
            }
            for j in (i + 1)..embeddings.len() {
                if deleted_ids.contains(&embeddings[j].0) {
                    continue;
                }
                let sim = crate::embedder::cosine_similarity(&embeddings[i].1, &embeddings[j].1);
                if f64::from(sim) < 0.92 {
                    continue;
                }

                let mem1 = self.store.get_memory(embeddings[i].0).await?;
                let mem2 = self.store.get_memory(embeddings[j].0).await?;

                match (mem1, mem2) {
                    (Some(m1), Some(m2)) => {
                        if m1.memory_type != m2.memory_type {
                            continue;
                        }

                        // Higher trust wins; equal trust → newer (higher ID) wins
                        let (keep, discard) = if m1.trust_level > m2.trust_level {
                            (m1, m2)
                        } else if m2.trust_level > m1.trust_level {
                            (m2, m1)
                        } else if m1.id > m2.id {
                            (m1, m2)
                        } else {
                            (m2, m1)
                        };

                        // Never discard a file-backed memory (see `is_file_backed`)
                        if is_file_backed(&discard.source_type) {
                            continue;
                        }

                        if self.store.delete_memory(discard.id).await? {
                            self.store.set_superseded_by(discard.id, keep.id).await?;
                            deleted_ids.insert(discard.id);
                            resolved += 1;
                        }
                    }
                    _ => continue,
                }
            }
        }

        if resolved > 0 {
            info!(resolved, "Resolved contradictions by trust hierarchy");
        }
        Ok(resolved)
    }

    /// Run all maintenance tasks with configurable decay tiers.
    ///
    /// Order: decay_strength -> prune -> merge -> boost -> deduplicate -> cleanup.
    pub async fn run_all_with(
        &self,
        hot_days: u64,
        warm_days: u64,
    ) -> anyhow::Result<MaintenanceReport> {
        let decayed = self
            .decay_strength_tiered_with(hot_days, warm_days)
            .await
            .unwrap_or(0);
        let pruned = self.prune_low_importance(0.1).await.unwrap_or(0);
        let merged = self.merge_near_duplicates().await.unwrap_or(0);
        let boosted = self.boost_frequently_accessed().await.unwrap_or(0);
        let deduplicated = self.deduplicate(0.95).await.unwrap_or(0);
        let cleaned = self.cleanup_deleted(30).await.unwrap_or(0);

        let report = MaintenanceReport {
            decayed,
            pruned,
            deduplicated,
            cleaned,
            merged,
            boosted,
        };
        info!(?report, "Maintenance complete");
        Ok(report)
    }

    /// Run all maintenance tasks with default parameters.
    ///
    /// Order: decay_strength -> prune -> merge -> boost -> deduplicate -> cleanup.
    pub async fn run_all(&self) -> anyhow::Result<MaintenanceReport> {
        let decayed = self.decay_strength_tiered().await.unwrap_or(0);
        let pruned = self.prune_low_importance(0.1).await.unwrap_or(0);
        let merged = self.merge_near_duplicates().await.unwrap_or(0);
        let boosted = self.boost_frequently_accessed().await.unwrap_or(0);
        let deduplicated = self.deduplicate(0.95).await.unwrap_or(0);
        let cleaned = self.cleanup_deleted(30).await.unwrap_or(0);

        let report = MaintenanceReport {
            decayed,
            pruned,
            deduplicated,
            cleaned,
            merged,
            boosted,
        };
        info!(?report, "Maintenance complete");
        Ok(report)
    }

    /// Run all maintenance tasks with LLM-powered sleep-time agents.
    ///
    /// First runs all rule-based tasks, then (if an LLM router is provided and
    /// healthy) runs the 5 LLM-powered maintenance agents. Each LLM agent is
    /// fire-and-forget with error logging -- one failure does not block others.
    ///
    /// The `cycle_count` parameter is used to run some agents less frequently
    /// (e.g. pattern detection every other cycle).
    pub async fn run_all_with_llm(
        &self,
        llm: Option<&LlmRouter>,
        hot_days: u64,
        warm_days: u64,
        batch_size: usize,
        pattern_days: u64,
        cycle_count: u64,
    ) -> anyhow::Result<MaintenanceReport> {
        // Run rule-based tasks first
        let report = self.run_all_with(hot_days, warm_days).await?;

        // LLM-powered tasks (only if LLM available)
        if let Some(llm) = llm {
            if let Err(e) = self.detect_contradictions(llm, batch_size).await {
                warn!(error = %e, "Sleep agent: detect_contradictions failed");
            }
            if let Err(e) = self.synthesize_insights(llm, batch_size).await {
                warn!(error = %e, "Sleep agent: synthesize_insights failed");
            }
            if let Err(e) = self.discover_relationships(llm, batch_size).await {
                warn!(error = %e, "Sleep agent: discover_relationships failed");
            }
            // Less frequent tasks: every other cycle
            if cycle_count.is_multiple_of(2) {
                if let Err(e) = self.verify_temporal_facts(llm, batch_size).await {
                    warn!(error = %e, "Sleep agent: verify_temporal_facts failed");
                }
                if let Err(e) = self.detect_patterns(llm, pattern_days).await {
                    warn!(error = %e, "Sleep agent: detect_patterns failed");
                }
            }
        }

        Ok(report)
    }

    // ---- LLM-powered sleep-time agents ----

    /// Collect a streaming chat response into a single string.
    async fn collect_llm_response(llm: &LlmRouter, request: ChatRequest) -> anyhow::Result<String> {
        let mut stream = llm
            .chat(request)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            if let Some(err) = &chunk.error {
                return Err(anyhow::anyhow!("LLM error: {err}"));
            }
            text.push_str(&chunk.delta);
        }
        Ok(text)
    }

    /// Detect contradictions among recent memories using an LLM.
    ///
    /// Loads the most recent active memories, sends them to the LLM to find
    /// contradictions, then closes validity on the older member of each pair.
    /// Cap: 5 resolutions per run.
    pub async fn detect_contradictions(
        &self,
        llm: &LlmRouter,
        batch_size: usize,
    ) -> anyhow::Result<u64> {
        let memories = self.store.get_recent_active_memories(batch_size).await?;

        if memories.len() < 2 {
            return Ok(0);
        }

        let now = chrono::Utc::now().naive_utc();
        let mut numbered = String::new();
        for m in &memories {
            let days = parse_days_since(&m.created_at, now) as u64;
            numbered.push_str(&format!(
                "[id={}] (type={}, age={}d, importance={:.2}, source={}, trust={})\n  {}\n",
                m.id,
                m.memory_type,
                days,
                m.importance,
                m.source.as_deref().unwrap_or("unknown"),
                m.trust_level,
                m.content,
            ));
        }

        let prompt = format!(
            "You are reviewing a personal knowledge base for contradictions. Each memory \
             includes metadata: type, age in days, importance score, source, and trust level.\n\n\
             RULES:\n\
             1. When a technology/tool is REPLACED (e.g. Ollama → llama-server, OpenClaw → Ironjaw), \
             ALL memories about the OLD tool's settings are stale — flag them.\n\
             2. Same topic + different values = likely contradiction. The OLDER one is usually stale.\n\
             3. Higher trust_level memories (UserStated, ToolVerified) are more reliable than ModelInferred.\n\
             4. Consider memory type: 'fact' contradicts 'fact', but 'preference' rarely contradicts 'fact'.\n\n\
             EXAMPLES of subtle contradictions:\n\
             - \"OLLAMA_NUM_PARALLEL=4\" vs \"llama-server --parallel 4\" → Ollama was replaced, Ollama setting is stale\n\
             - \"Gateway on port 18789\" vs \"Gateway on port 18794\" → port changed, older is stale\n\
             - \"Default model: qwen3:8b\" vs \"Default model: qwen3.5:35b-a3b\" → model changed, older is stale\n\n\
             Respond with ONLY a JSON array of objects:\n\
             [{{\"older_id\": <id>, \"newer_id\": <id>, \"confidence\": 0.0-1.0, \"reason\": \"...\"}}]\n\
             If none contradict, respond with [].\n\n\
             MEMORIES:\n{numbered}"
        );

        let request = ChatRequest {
            model: String::new(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: prompt,
                images: None,
                ..Default::default()
            }],
            temperature: Some(0.1),
            max_tokens: Some(512),
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        let response = Self::collect_llm_response(llm, request).await?;
        let pairs = parse_json_pairs(&response);

        let mut resolved = 0u64;
        for (older, _newer) in pairs {
            if resolved >= 5 {
                break;
            }
            if memories.iter().any(|m| m.id == older) {
                self.store.close_validity(older).await?;
                resolved += 1;
                info!(older_id = older, "Sleep agent: closed contradicted memory");
            }
        }

        if resolved > 0 {
            info!(resolved, "Sleep agent: contradictions resolved");
        }
        Ok(resolved)
    }

    /// Synthesize insights from clusters of related memories.
    ///
    /// Finds memories with high cosine similarity, groups them, and asks the LLM
    /// to produce a higher-level insight. The insight is stored as a new memory
    /// with type "observation". Cap: 3 syntheses per run.
    pub async fn synthesize_insights(
        &self,
        llm: &LlmRouter,
        batch_size: usize,
    ) -> anyhow::Result<u64> {
        let embeddings = self.store.get_all_embeddings().await?;
        if embeddings.len() < 3 {
            return Ok(0);
        }

        // Build clusters of similar memories (cosine > 0.7)
        let mut used = std::collections::HashSet::new();
        let mut clusters: Vec<Vec<i64>> = Vec::new();

        for i in 0..embeddings.len().min(batch_size) {
            if used.contains(&embeddings[i].0) {
                continue;
            }
            let mut cluster = vec![embeddings[i].0];
            for j in (i + 1)..embeddings.len() {
                if used.contains(&embeddings[j].0) {
                    continue;
                }
                let sim = crate::embedder::cosine_similarity(&embeddings[i].1, &embeddings[j].1);
                if f64::from(sim) >= 0.7 {
                    cluster.push(embeddings[j].0);
                }
            }
            if cluster.len() >= 3 {
                for &id in &cluster {
                    used.insert(id);
                }
                clusters.push(cluster);
            }
        }

        let mut synthesized = 0u64;
        for cluster in clusters.iter().take(3) {
            let mut facts = String::new();
            for &id in cluster {
                if let Ok(Some(mem)) = self.store.get_memory(id).await {
                    facts.push_str(&format!(
                        "- [type={}, source={}, trust={}] {}\n",
                        mem.memory_type,
                        mem.source.as_deref().unwrap_or("unknown"),
                        mem.trust_level,
                        mem.content,
                    ));
                }
            }

            if facts.is_empty() {
                continue;
            }

            let prompt = format!(
                "Synthesize these related facts into one ACTIONABLE insight (1-2 sentences).\n\n\
                 RULES:\n\
                 - The insight MUST be specific and actionable, not a vague theme.\n\
                 - Include concrete names, values, frequencies, or patterns.\n\
                 - BAD: \"User works on infrastructure regularly\"\n\
                 - GOOD: \"llama-server is reconfigured approximately weekly — parallel count, \
                 model, and cache settings change most often\"\n\
                 - BAD: \"User manages multiple projects\"\n\
                 - GOOD: \"Ironjaw and ExamplePipeline are the most actively developed projects, \
                 with 3-4 sessions per week each\"\n\n\
                 Respond with ONLY the insight, no preamble:\n\n{facts}"
            );

            let request = ChatRequest {
                model: String::new(),
                messages: vec![ChatMessage {
                    role: Role::User,
                    content: prompt,
                    images: None,
                    ..Default::default()
                }],
                temperature: Some(0.3),
                max_tokens: Some(256),
                extra: None,
                thinking: None,
                tools: None,
                tool_choice: None,
            };

            match Self::collect_llm_response(llm, request).await {
                Ok(insight) => {
                    let insight = insight.trim().to_string();
                    if !insight.is_empty() {
                        let pending = NewPendingMemory {
                            content: insight,
                            source: "sleep-agent:synthesize".to_string(),
                            memory_type: "observation".to_string(),
                            tags: Some("sleep-agent,synthesis".to_string()),
                            importance: 0.6,
                            confidence: 0.5,
                            evidence_session_id: None,
                            embedding: None,
                        };
                        self.store.add_pending_memory(&pending).await?;
                        synthesized += 1;
                        info!(
                            cluster_size = cluster.len(),
                            "Sleep agent: insight synthesized"
                        );
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Sleep agent: synthesis LLM call failed");
                }
            }
        }

        if synthesized > 0 {
            info!(synthesized, "Sleep agent: insights synthesized");
        }
        Ok(synthesized)
    }

    /// Discover entity relationships from recent memories using LLM.
    ///
    /// Loads recent memories that have no entity relations, asks the LLM to
    /// extract entities and relationships. Cap: 10 memories per run.
    pub async fn discover_relationships(
        &self,
        llm: &LlmRouter,
        batch_size: usize,
    ) -> anyhow::Result<u64> {
        let memories = self
            .store
            .get_recent_active_memories(batch_size.min(10))
            .await?;

        if memories.is_empty() {
            return Ok(0);
        }

        let mut numbered = String::new();
        for m in &memories {
            numbered.push_str(&format!(
                "[id={}, type={}, source={}] {}\n",
                m.id,
                m.memory_type,
                m.source.as_deref().unwrap_or("unknown"),
                m.content,
            ));
        }

        let prompt = format!(
            "Extract entities and relationships from these facts.\n\n\
             ENTITY TYPES: person, project, technology, concept, service, config\n\n\
             RELATIONSHIP TYPES (use these when applicable):\n\
             - uses, configures, depends_on, part_of, maintained_by\n\
             - replaces (A replaces B — marks A as the successor)\n\
             - replaced_by (B replaced_by A — marks B as deprecated)\n\
             - runs_on, connects_to, hosts\n\n\
             IMPORTANT: When a technology/tool migration is mentioned (e.g. \"switched from \
             Ollama to llama-server\", \"replaced OpenClaw with Ironjaw\"), emit BOTH:\n\
             - {{\"from\": \"new_tool\", \"to\": \"old_tool\", \"relation\": \"replaces\"}}\n\
             - {{\"from\": \"old_tool\", \"to\": \"new_tool\", \"relation\": \"replaced_by\"}}\n\
             This marks the old tool's relationships as potentially stale.\n\n\
             Return ONLY a JSON object:\n\
             {{\"entities\": [{{\"name\": \"...\", \"type\": \"...\"}}],\n\
             \"relationships\": [{{\"from\": \"...\", \"to\": \"...\", \"relation\": \"...\"}}]}}\n\
             If none found, return {{\"entities\": [], \"relationships\": []}}.\n\n{numbered}"
        );

        let request = ChatRequest {
            model: String::new(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: prompt,
                images: None,
                ..Default::default()
            }],
            temperature: Some(0.1),
            max_tokens: Some(1024),
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        let response = Self::collect_llm_response(llm, request).await?;

        // Parse the JSON response -- best-effort
        let parsed: serde_json::Value = extract_json_object(&response)
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::json!({"entities": [], "relationships": []}));

        let entities = parsed["entities"].as_array().map(|a| a.len()).unwrap_or(0);
        let relations = parsed["relationships"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);

        if entities > 0 || relations > 0 {
            info!(
                entities,
                relations, "Sleep agent: discovered entities/relationships"
            );
        }
        Ok(entities as u64 + relations as u64)
    }

    /// Verify temporal facts that may have become stale.
    ///
    /// Loads memories where `valid_to` is NULL and `created_at` is older than
    /// 30 days. Asks the LLM to rate confidence. If < 0.3, closes validity.
    /// Cap: 5 verifications per run.
    pub async fn verify_temporal_facts(
        &self,
        llm: &LlmRouter,
        batch_size: usize,
    ) -> anyhow::Result<u64> {
        let stale = self
            .store
            .get_stale_open_memories(30, batch_size.min(10))
            .await?;

        if stale.is_empty() {
            return Ok(0);
        }

        let mut expired = 0u64;
        for mem in stale.iter().take(5) {
            let days = {
                let now = chrono::Utc::now().naive_utc();
                parse_days_since(&mem.created_at, now) as u64
            };

            let prompt = format!(
                "Is this fact still likely true? Rate confidence 0.0-1.0.\n\n\
                 DURABILITY GUIDE by category:\n\
                 - PIDs, process states, \"currently running\" → ephemeral (hours). \
                 If age > 1 day, likely stale.\n\
                 - Port numbers, config values, IP addresses → semi-stable (weeks). \
                 If age > 30 days, verify.\n\
                 - Architecture decisions, tool choices, project structure → durable (months). \
                 Usually valid for 90+ days.\n\
                 - User preferences, workflows, habits → very durable (years). \
                 Rarely expire.\n\n\
                 SOURCE RELIABILITY:\n\
                 - source_type={}: {} are more reliable than model-inferred facts.\n\
                 - trust_level={}: higher trust = slower decay.\n\n\
                 Fact: \"{}\"\n\
                 (type={}, recorded {} days ago, importance={:.2}, source={})\n\n\
                 Respond with ONLY a number (0.0-1.0).",
                mem.source_type,
                match mem.source_type.as_str() {
                    "user_stated" | "UserStated" => "User-stated facts",
                    "tool_verified" | "ToolVerified" => "Tool-verified facts",
                    _ => "Model-inferred facts",
                },
                mem.trust_level,
                mem.content,
                mem.memory_type,
                days,
                mem.importance,
                mem.source.as_deref().unwrap_or("unknown"),
            );

            let request = ChatRequest {
                model: String::new(),
                messages: vec![ChatMessage {
                    role: Role::User,
                    content: prompt,
                    images: None,
                    ..Default::default()
                }],
                temperature: Some(0.1),
                max_tokens: Some(32),
                extra: None,
                thinking: None,
                tools: None,
                tool_choice: None,
            };

            match Self::collect_llm_response(llm, request).await {
                Ok(response) => {
                    let confidence = response.trim().parse::<f64>().unwrap_or(1.0);
                    if confidence < 0.3 {
                        self.store.close_validity(mem.id).await?;
                        expired += 1;
                        info!(
                            id = mem.id,
                            confidence, "Sleep agent: expired low-confidence fact"
                        );
                    }
                }
                Err(e) => {
                    warn!(id = mem.id, error = %e, "Sleep agent: verify LLM call failed");
                }
            }
        }

        if expired > 0 {
            info!(expired, "Sleep agent: stale facts expired");
        }
        Ok(expired)
    }

    /// Detect recurring patterns across session summaries.
    ///
    /// Loads session summary memories from the last `pattern_days` days, asks
    /// the LLM to identify themes, and stores each pattern as a new observation.
    /// Cap: 3 patterns per run.
    pub async fn detect_patterns(&self, llm: &LlmRouter, pattern_days: u64) -> anyhow::Result<u64> {
        let summaries = self.store.get_recent_summaries(pattern_days, 20).await?;

        if summaries.len() < 3 {
            return Ok(0);
        }

        let mut text = String::new();
        for s in &summaries {
            text.push_str(&format!(
                "- [source={}, created={}] {}\n",
                s.source.as_deref().unwrap_or("unknown"),
                &s.created_at[..10.min(s.created_at.len())],
                s.content,
            ));
        }

        let prompt = format!(
            "Identify SPECIFIC, recurring patterns across these session summaries \
             from the last {pattern_days} days.\n\n\
             RULES:\n\
             - Each pattern MUST include concrete details: names, frequencies, values.\n\
             - BAD: \"User frequently works on infrastructure\"\n\
             - GOOD: \"User configures llama-server parallel=4 and ctx-size=131072 as \
             standard defaults after each upgrade\"\n\
             - BAD: \"Testing is a common activity\"\n\
             - GOOD: \"Test suite runs average 3x per session across Ironjaw (cargo test) \
             and ExamplePipeline (pytest), usually before commits\"\n\
             - Look for: repeated config changes, recurring error patterns, workflow habits, \
             tool usage sequences, time-of-day patterns.\n\n\
             List 1-3 patterns, one per line. Respond with ONLY the patterns, no preamble:\n\n{text}"
        );

        let request = ChatRequest {
            model: String::new(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: prompt,
                images: None,
                ..Default::default()
            }],
            temperature: Some(0.3),
            max_tokens: Some(512),
            extra: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };

        let response = Self::collect_llm_response(llm, request).await?;

        let mut stored = 0u64;
        for line in response.lines() {
            let line = line.trim().trim_start_matches('-').trim();
            if line.is_empty() || stored >= 3 {
                continue;
            }

            let pending = NewPendingMemory {
                content: line.to_string(),
                source: "sleep-agent:patterns".to_string(),
                memory_type: "observation".to_string(),
                tags: Some("sleep-agent,patterns".to_string()),
                importance: 0.6,
                confidence: 0.5,
                evidence_session_id: None,
                embedding: None,
            };
            self.store.add_pending_memory(&pending).await?;
            stored += 1;
        }

        if stored > 0 {
            info!(stored, "Sleep agent: patterns detected and stored");
        }
        Ok(stored)
    }

    // ---- Cleanup RPCs ----

    /// Retire stale memories — those older than `days` with fewer than
    /// `min_accesses` touches.  Returns `CleanupReport`.
    pub async fn cleanup_stale(
        &self,
        days: u64,
        min_accesses: i64,
    ) -> anyhow::Result<CleanupReport> {
        let stale = self.store.query_stale_memories(days, min_accesses).await?;
        let ids: Vec<i64> = stale.iter().map(|m| m.id).collect();
        let retired = self.store.retire_batch(&ids).await?;
        info!(retired, days, min_accesses, "cleanup_stale complete");
        Ok(CleanupReport {
            retired,
            merged: 0,
            errors: vec![],
        })
    }

    /// Find and retire contradictory memories using an LLM.
    /// Requires an LLM router to detect contradictions.
    pub async fn cleanup_contradictions(
        &self,
        llm: &LlmRouter,
        batch_size: usize,
    ) -> anyhow::Result<CleanupReport> {
        let retired = self.detect_contradictions(llm, batch_size).await?;
        Ok(CleanupReport {
            retired,
            merged: 0,
            errors: vec![],
        })
    }

    /// Find and merge near-duplicate memories (cosine similarity >= threshold).
    /// Returns the count of merged (retired) duplicates.
    pub async fn cleanup_duplicates(&self, threshold: f64) -> anyhow::Result<CleanupReport> {
        let merged = self.deduplicate(threshold).await?;
        Ok(CleanupReport {
            retired: 0,
            merged,
            errors: vec![],
        })
    }
}

/// Report from a targeted cleanup operation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CleanupReport {
    pub retired: u64,
    pub merged: u64,
    pub errors: Vec<String>,
}

// ---------------------------------------------------------------------------
// LLM response parsing helpers
// ---------------------------------------------------------------------------

/// Parse a JSON array of contradiction pairs from LLM output.
///
/// Supports two formats:
/// - Object format: `[{"older_id": 1, "newer_id": 2, "confidence": 0.9, "reason": "..."}]`
/// - Legacy array format: `[[1, 2]]`
///
/// Tolerates preamble text before and after the JSON array.
fn parse_json_pairs(response: &str) -> Vec<(i64, i64)> {
    // Try to find a JSON array in the response
    let trimmed = response.trim();
    let start = trimmed.find('[');
    let end = trimmed.rfind(']');

    let json_str = match (start, end) {
        (Some(s), Some(e)) if e > s => &trimmed[s..=e],
        _ => return Vec::new(),
    };

    // Try object format first: [{"older_id": N, "newer_id": N, ...}]
    if let Ok(objects) = serde_json::from_str::<Vec<serde_json::Value>>(json_str) {
        let pairs: Vec<(i64, i64)> = objects
            .iter()
            .filter_map(|obj| {
                let older = obj.get("older_id")?.as_i64()?;
                let newer = obj.get("newer_id")?.as_i64()?;
                Some((older, newer))
            })
            .collect();
        if !pairs.is_empty() {
            return pairs;
        }

        // Fall back to legacy array-of-arrays format: [[1, 2]]
        return objects
            .iter()
            .filter_map(|v| {
                let arr = v.as_array()?;
                if arr.len() == 2 {
                    Some((arr[0].as_i64()?, arr[1].as_i64()?))
                } else {
                    None
                }
            })
            .collect();
    }

    Vec::new()
}

/// Extract the first JSON object `{...}` from a string that may contain
/// surrounding text (e.g. LLM preamble).
fn extract_json_object(response: &str) -> Option<String> {
    let trimmed = response.trim();
    let start = trimmed.find('{')?;

    // Simple brace-counting to find the matching close brace
    let mut depth = 0;
    let mut in_string = false;
    let mut escape = false;

    for (i, ch) in trimmed[start..].char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(trimmed[start..start + i + 1].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Summary of a maintenance run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MaintenanceReport {
    pub decayed: u64,
    pub pruned: u64,
    pub deduplicated: u64,
    pub cleaned: u64,
    pub merged: u64,
    pub boosted: u64,
}

/// Summary of a consolidation run with cross-source awareness.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConsolidationReport {
    pub decayed: u64,
    pub deduped: u64,
    pub pruned: u64,
    pub cross_source_merged: u64,
}

/// True for memories that are a projection of a file on disk.
///
/// These are reproducible from their source file and represent curated ground
/// truth, so maintenance must never soft-delete them: not by decay, not by
/// prune, not by dedup/merge. Their lifecycle belongs to the indexer, which
/// calls `soft_delete_by_source` when the file changes or disappears.
///
/// Without this, memory-file chunks (trust `ModelInferred`, never accessed)
/// decayed below the 0.05 strength floor in ~26 days and were pruned, and
/// `merge_cross_source_duplicates` discarded them in favour of lower-content
/// `brain` entries that outrank them in `source_authority`.
pub fn is_file_backed(source_type: &str) -> bool {
    source_type == "markdown"
}

/// Authority ranking for source types (higher = more authoritative).
fn source_authority(source_type: &str) -> u8 {
    match source_type {
        "brain" => 5,
        "markdown" => 4,
        "cc_chronicle" => 3,
        "cc_bridge" => 2,
        "cc_session" => 1,
        _ => 0,
    }
}

/// Parse an ISO 8601 timestamp and compute fractional days since that time.
/// Returns 0.0 if the timestamp cannot be parsed.
fn parse_days_since(timestamp: &str, now: NaiveDateTime) -> f64 {
    // Try with fractional seconds first, then without, then RFC 3339
    let parsed = NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| {
            timestamp
                .parse::<chrono::DateTime<chrono::Utc>>()
                .map(|dt| dt.naive_utc())
        });

    match parsed {
        Ok(ts) => {
            let duration = now.signed_duration_since(ts);
            let secs = duration.num_seconds() as f64;
            if secs < 0.0 {
                0.0
            } else {
                secs / 86400.0
            }
        }
        Err(_) => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{NewMemory, TrustLevel};

    async fn setup() -> (BrainStore, MemoryMaintenance) {
        let store = BrainStore::open_memory().await.unwrap();
        let maint = MemoryMaintenance::new(store.clone());
        (store, maint)
    }

    fn test_memory(content: &str) -> NewMemory {
        NewMemory {
            content: content.to_string(),
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
        }
    }

    fn test_memory_with_importance(content: &str, importance: f64) -> NewMemory {
        NewMemory {
            content: content.to_string(),
            source: None,
            tags: None,
            embedding: None,
            memory_type: None,
            importance: Some(importance),
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
        }
    }

    #[tokio::test]
    async fn test_prune_low_importance() {
        let (store, maint) = setup().await;
        store
            .add_memory(&test_memory_with_importance("high", 0.9))
            .await
            .unwrap();
        store
            .add_memory(&test_memory_with_importance("low", 0.05))
            .await
            .unwrap();

        let pruned = maint.prune_low_importance(0.1).await.unwrap();
        assert_eq!(pruned, 1);
        assert_eq!(store.count_memories().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_find_duplicates() {
        let (store, maint) = setup().await;
        // Same embedding = cosine similarity 1.0
        let emb = vec![1.0, 0.0, 0.0];
        store
            .add_memory(&NewMemory {
                content: "A".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb.clone()),
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
            })
            .await
            .unwrap();
        store
            .add_memory(&NewMemory {
                content: "B".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb),
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
            })
            .await
            .unwrap();

        let dups = maint.find_duplicates(0.95).await.unwrap();
        assert_eq!(dups.len(), 1);
        assert!((dups[0].2 - 1.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_deduplicate() {
        let (store, maint) = setup().await;
        let emb = vec![1.0, 0.0, 0.0];
        store
            .add_memory(&NewMemory {
                content: "Keep".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb.clone()),
                memory_type: None,
                importance: Some(0.9),
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
            })
            .await
            .unwrap();
        store
            .add_memory(&NewMemory {
                content: "Delete".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb),
                memory_type: None,
                importance: Some(0.3),
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
            })
            .await
            .unwrap();

        let deduped = maint.deduplicate(0.95).await.unwrap();
        assert_eq!(deduped, 1);
        assert_eq!(store.count_memories().await.unwrap(), 1);

        // The higher-importance one should remain
        let remaining = store.all_memories().await.unwrap();
        assert_eq!(remaining[0].content, "Keep");
    }

    #[tokio::test]
    async fn test_run_all() {
        let (store, maint) = setup().await;
        store.add_memory(&test_memory("normal")).await.unwrap();

        let report = maint.run_all().await.unwrap();
        // Should complete without errors even with minimal data
        assert_eq!(report.pruned, 0);
    }

    #[tokio::test]
    async fn test_no_duplicates_with_different_embeddings() {
        let (_store, maint) = setup().await;
        // No memories = no duplicates
        let dups = maint.find_duplicates(0.95).await.unwrap();
        assert!(dups.is_empty());
    }

    #[tokio::test]
    async fn test_prune_never_deletes_file_backed_memories() {
        // A memory-file chunk is never "accessed" and its importance is 0.5,
        // so both prune rules used to reach it. The file on disk is the truth;
        // only the indexer may remove it (soft_delete_by_source).
        let (store, maint) = setup().await;

        let mut junk = test_memory_with_importance("junk", 0.01);
        junk.source_type = Some("brain".to_string());
        store.add_memory(&junk).await.unwrap();

        let mut chunk = test_memory_with_importance("curated memory-file chunk", 0.01);
        chunk.source_type = Some("markdown".to_string());
        chunk.strength = Some(0.001); // below the 0.05 prune floor
        store.add_memory(&chunk).await.unwrap();

        let pruned = maint.prune_low_importance(0.1).await.unwrap();
        assert_eq!(
            pruned, 1,
            "only the non-file-backed memory should be pruned"
        );
        assert_eq!(store.count_memories().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_decay_skips_file_backed_memories() {
        let (store, maint) = setup().await;

        let mut chunk = test_memory("file backed chunk");
        chunk.source_type = Some("markdown".to_string());
        let chunk_id = store.add_memory(&chunk).await.unwrap();

        let mut other = test_memory("ordinary memory");
        other.source_type = Some("brain".to_string());
        let other_id = store.add_memory(&other).await.unwrap();

        // Decay is a no-op for anything accessed less than 0.001 days ago, so
        // both rows must be aged or this test passes without the exemption.
        sqlx::query(
            "UPDATE memories SET accessed_at = \
             strftime('%Y-%m-%dT%H:%M:%f', 'now', '-60 days')",
        )
        .execute(store.pool())
        .await
        .unwrap();

        maint.decay_strength(0.5).await.unwrap();
        maint.decay_strength_tiered().await.unwrap();

        let chunk_mem = store.get_memory(chunk_id).await.unwrap().unwrap();
        let other_mem = store.get_memory(other_id).await.unwrap().unwrap();
        assert!(
            other_mem.strength < 1.0,
            "control memory should have decayed, got {} — test is not exercising decay",
            other_mem.strength
        );
        assert!(
            chunk_mem.strength >= 1.0,
            "file-backed strength should not decay, got {}",
            chunk_mem.strength
        );
    }

    /// Build a file-backed chunk and a rival that would beat it in the given
    /// merge path, sharing an embedding so they read as near-duplicates.
    async fn file_backed_vs_rival(
        store: &BrainStore,
        rival: NewMemory,
        mut chunk: NewMemory,
    ) -> i64 {
        let emb = vec![1.0, 0.0, 0.0];
        let mut rival = rival;
        rival.embedding = Some(emb.clone());
        store.add_memory(&rival).await.unwrap();
        chunk.source_type = Some("markdown".to_string());
        chunk.embedding = Some(emb);
        store.add_memory(&chunk).await.unwrap()
    }

    #[tokio::test]
    async fn test_merge_near_duplicates_keeps_file_backed() {
        // Loser is picked by importance; give the chunk the lower one so it
        // would be discarded without the guard.
        let (store, maint) = setup().await;
        let id = file_backed_vs_rival(
            &store,
            test_memory_with_importance("rival with higher importance", 0.9),
            test_memory_with_importance("curated chunk", 0.1),
        )
        .await;

        maint.merge_near_duplicates().await.unwrap();

        assert!(
            store.get_memory(id).await.unwrap().is_some(),
            "file-backed chunk must survive merge_near_duplicates"
        );
    }

    #[tokio::test]
    async fn test_deduplicate_keeps_file_backed() {
        let (store, maint) = setup().await;
        let id = file_backed_vs_rival(
            &store,
            test_memory_with_importance("rival with higher importance", 0.9),
            test_memory_with_importance("curated chunk", 0.1),
        )
        .await;

        maint.deduplicate(0.95).await.unwrap();

        assert!(
            store.get_memory(id).await.unwrap().is_some(),
            "file-backed chunk must survive deduplicate"
        );
    }

    #[tokio::test]
    async fn test_resolve_by_trust_keeps_file_backed() {
        // Loser is picked by trust; give the chunk the lower one.
        let (store, maint) = setup().await;
        let mut rival = test_memory("rival with higher trust");
        rival.trust_level = Some(TrustLevel::SystemPolicy);
        let mut chunk = test_memory("curated chunk");
        chunk.trust_level = Some(TrustLevel::ModelInferred);
        let id = file_backed_vs_rival(&store, rival, chunk).await;

        maint.resolve_by_trust().await.unwrap();

        assert!(
            store.get_memory(id).await.unwrap().is_some(),
            "file-backed chunk must survive resolve_by_trust"
        );
    }

    #[tokio::test]
    async fn test_cross_source_merge_keeps_file_backed_over_brain_stub() {
        // `brain` outranks `markdown` in source_authority, so a 300-char stub
        // would otherwise soft-delete the full-content chunk of the same file.
        let (store, maint) = setup().await;
        let emb = vec![1.0, 0.0, 0.0];

        let mut stub = test_memory("[memory/x.md] frontmatter excerpt...");
        stub.source_type = Some("brain".to_string());
        stub.embedding = Some(emb.clone());
        store.add_memory(&stub).await.unwrap();

        let mut chunk = test_memory("full body of x.md with all the actual content");
        chunk.source_type = Some("markdown".to_string());
        chunk.embedding = Some(emb);
        let chunk_id = store.add_memory(&chunk).await.unwrap();

        maint.merge_cross_source_duplicates().await.unwrap();

        assert!(
            store.get_memory(chunk_id).await.unwrap().is_some(),
            "file-backed chunk must survive cross-source merge"
        );
    }

    #[tokio::test]
    async fn test_prune_preserves_high_importance() {
        let (store, maint) = setup().await;
        store
            .add_memory(&test_memory_with_importance("important", 0.9))
            .await
            .unwrap();
        store
            .add_memory(&test_memory_with_importance("also important", 0.5))
            .await
            .unwrap();

        let pruned = maint.prune_low_importance(0.1).await.unwrap();
        assert_eq!(pruned, 0);
        assert_eq!(store.count_memories().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_cleanup_deleted_no_op_when_empty() {
        let (_store, maint) = setup().await;
        let cleaned = maint.cleanup_deleted(30).await.unwrap();
        assert_eq!(cleaned, 0);
    }

    #[tokio::test]
    async fn test_decay_strength() {
        let (store, maint) = setup().await;
        // Insert a memory with full strength
        let id = store.add_memory(&test_memory("decayable")).await.unwrap();

        // Set accessed_at to 10 days ago so decay has effect
        sqlx::query(
            "UPDATE memories SET accessed_at = strftime('%Y-%m-%dT%H:%M:%f', 'now', '-10 days') \
             WHERE id = ?1",
        )
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

        let decayed = maint.decay_strength(0.95).await.unwrap();
        assert_eq!(decayed, 1);

        // Verify strength decreased: 1.0 * 0.95^10 ~ 0.5987
        let mem = store.get_memory(id).await.unwrap().unwrap();
        assert!(mem.strength < 1.0, "Strength should have decreased");
        assert!(
            mem.strength > 0.5,
            "Strength should not be too low for 10 days"
        );
        assert!(
            (mem.strength - 0.95_f64.powi(10)).abs() < 0.05,
            "Strength should approximate 0.95^10 (~0.5987), got {}",
            mem.strength
        );
    }

    #[tokio::test]
    async fn test_decay_strength_skips_low() {
        let (store, maint) = setup().await;
        // Insert a memory with very low strength (below 0.05 threshold)
        let id = store
            .add_memory(&NewMemory {
                content: "weak".to_string(),
                source: None,
                tags: None,
                embedding: None,
                memory_type: None,
                importance: None,
                strength: Some(0.03),
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

        // Set accessed_at to past so it would normally decay
        sqlx::query(
            "UPDATE memories SET accessed_at = strftime('%Y-%m-%dT%H:%M:%f', 'now', '-5 days') \
             WHERE id = ?1",
        )
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

        let decayed = maint.decay_strength(0.95).await.unwrap();
        assert_eq!(decayed, 0, "Should skip memories with strength < 0.05");
    }

    #[tokio::test]
    async fn test_prune_low_strength() {
        let (store, maint) = setup().await;
        // Memory with decent importance but very low strength and low access
        store
            .add_memory(&NewMemory {
                content: "weak forgotten".to_string(),
                source: None,
                tags: None,
                embedding: None,
                memory_type: None,
                importance: Some(0.5),
                strength: Some(0.02),
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
        // Memory with low strength but high access count (should be preserved)
        let id2 = store
            .add_memory(&NewMemory {
                content: "weak but accessed".to_string(),
                source: None,
                tags: None,
                embedding: None,
                memory_type: None,
                importance: Some(0.5),
                strength: Some(0.02),
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

        // Give id2 enough accesses to survive
        for _ in 0..3 {
            store.update_accessed(id2).await.unwrap();
        }

        // Normal high-importance memory
        store
            .add_memory(&test_memory_with_importance("strong", 0.9))
            .await
            .unwrap();

        // Prune with threshold that won't catch importance-based
        let pruned = maint.prune_low_importance(0.1).await.unwrap();
        // Only the first one should be pruned (low strength + low access)
        assert_eq!(pruned, 1);
        assert_eq!(store.count_memories().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_merge_near_duplicates() {
        let (store, maint) = setup().await;
        // Two memories with identical embeddings and same type
        let emb = vec![1.0, 0.0, 0.0];
        store
            .add_memory(&NewMemory {
                content: "Original fact".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb.clone()),
                memory_type: Some("fact".to_string()),
                importance: Some(0.9),
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
            })
            .await
            .unwrap();
        store
            .add_memory(&NewMemory {
                content: "Duplicate fact".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb.clone()),
                memory_type: Some("fact".to_string()),
                importance: Some(0.3),
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
            })
            .await
            .unwrap();

        let merged = maint.merge_near_duplicates().await.unwrap();
        assert_eq!(merged, 1);
        assert_eq!(store.count_memories().await.unwrap(), 1);

        // The higher-importance one should remain
        let remaining = store.all_memories().await.unwrap();
        assert_eq!(remaining[0].content, "Original fact");
    }

    #[tokio::test]
    async fn test_merge_skips_different_types() {
        let (store, maint) = setup().await;
        // Same embeddings but different types -- should NOT merge
        let emb = vec![1.0, 0.0, 0.0];
        store
            .add_memory(&NewMemory {
                content: "A fact".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb.clone()),
                memory_type: Some("fact".to_string()),
                importance: Some(0.9),
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
            })
            .await
            .unwrap();
        store
            .add_memory(&NewMemory {
                content: "A preference".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb.clone()),
                memory_type: Some("preference".to_string()),
                importance: Some(0.3),
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
            })
            .await
            .unwrap();

        let merged = maint.merge_near_duplicates().await.unwrap();
        assert_eq!(merged, 0, "Should not merge memories of different types");
        assert_eq!(store.count_memories().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_boost_frequently_accessed() {
        let (store, maint) = setup().await;
        let id = store
            .add_memory(&test_memory_with_importance("popular", 0.5))
            .await
            .unwrap();

        // Access it 5 times to make it eligible
        for _ in 0..5 {
            store.update_accessed(id).await.unwrap();
        }

        let boosted = maint.boost_frequently_accessed().await.unwrap();
        assert_eq!(boosted, 1);

        let mem = store.get_memory(id).await.unwrap().unwrap();
        let expected = (0.5_f64 * 1.10).min(1.0);
        assert!(
            (mem.importance - expected).abs() < 0.001,
            "Importance should be ~{expected}, got {}",
            mem.importance
        );
    }

    #[tokio::test]
    async fn test_boost_caps_at_one() {
        let (store, maint) = setup().await;
        let id = store
            .add_memory(&test_memory_with_importance("near max", 0.95))
            .await
            .unwrap();

        // Access it 5 times
        for _ in 0..5 {
            store.update_accessed(id).await.unwrap();
        }

        let boosted = maint.boost_frequently_accessed().await.unwrap();
        assert_eq!(boosted, 1);

        let mem = store.get_memory(id).await.unwrap().unwrap();
        assert!(
            mem.importance <= 1.0,
            "Importance should be capped at 1.0, got {}",
            mem.importance
        );
    }

    #[tokio::test]
    async fn test_boost_skips_low_access() {
        let (store, maint) = setup().await;
        // Memory accessed only 2 times (below threshold of 5)
        let id = store
            .add_memory(&test_memory_with_importance("unpopular", 0.5))
            .await
            .unwrap();

        store.update_accessed(id).await.unwrap();
        store.update_accessed(id).await.unwrap();

        let boosted = maint.boost_frequently_accessed().await.unwrap();
        assert_eq!(boosted, 0, "Should not boost memories with < 5 accesses");
    }

    #[tokio::test]
    async fn test_run_all_with_new_fields() {
        let (store, maint) = setup().await;
        store.add_memory(&test_memory("normal")).await.unwrap();

        let report = maint.run_all().await.unwrap();
        // Should complete without errors and have all fields
        assert_eq!(report.pruned, 0);
        assert_eq!(report.merged, 0);
        assert_eq!(report.boosted, 0);
    }

    #[test]
    fn test_parse_days_since() {
        let now = NaiveDateTime::parse_from_str("2026-03-19T12:00:00.000", "%Y-%m-%dT%H:%M:%S%.f")
            .unwrap();
        let ts = "2026-03-17T12:00:00.000";
        let days = super::parse_days_since(ts, now);
        assert!((days - 2.0).abs() < 0.01, "Expected ~2.0 days, got {days}");
    }

    #[test]
    fn test_parse_days_since_invalid() {
        let now = chrono::Utc::now().naive_utc();
        let days = super::parse_days_since("not-a-date", now);
        assert_eq!(days, 0.0);
    }

    // -- LLM helper tests (no actual LLM needed) --

    #[test]
    fn test_parse_json_pairs_valid() {
        let response = "Here are the contradictions:\n[[1, 5], [3, 7]]";
        let pairs = parse_json_pairs(response);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], (1, 5));
        assert_eq!(pairs[1], (3, 7));
    }

    #[test]
    fn test_parse_json_pairs_empty() {
        let response = "No contradictions found.\n[]";
        let pairs = parse_json_pairs(response);
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_parse_json_pairs_no_json() {
        let response = "I found no contradictions.";
        let pairs = parse_json_pairs(response);
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_parse_json_pairs_malformed() {
        let response = "[[1, 2, 3], [4]]";
        let pairs = parse_json_pairs(response);
        // [1,2,3] has 3 elements (filtered out), [4] has 1 (filtered out)
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_parse_json_pairs_object_format() {
        let response = r#"[
            {"older_id": 1, "newer_id": 5, "confidence": 0.9, "reason": "port changed"},
            {"older_id": 3, "newer_id": 7, "confidence": 0.8, "reason": "tool replaced"}
        ]"#;
        let pairs = parse_json_pairs(response);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], (1, 5));
        assert_eq!(pairs[1], (3, 7));
    }

    #[test]
    fn test_parse_json_pairs_object_with_preamble() {
        let response = "Here are the contradictions:\n\
            [{\"older_id\": 10, \"newer_id\": 20, \"confidence\": 0.95, \"reason\": \"stale config\"}]";
        let pairs = parse_json_pairs(response);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], (10, 20));
    }

    #[test]
    fn test_extract_json_object_valid() {
        let response =
            r#"Here is the result: {"entities": [{"name": "Rust"}], "relationships": []}"#;
        let obj = extract_json_object(response);
        assert!(obj.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&obj.unwrap()).unwrap();
        assert!(parsed["entities"].is_array());
    }

    #[test]
    fn test_extract_json_object_nested() {
        let response = r#"{"outer": {"inner": "value"}}"#;
        let obj = extract_json_object(response);
        assert!(obj.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&obj.unwrap()).unwrap();
        assert_eq!(parsed["outer"]["inner"], "value");
    }

    #[test]
    fn test_extract_json_object_none() {
        let response = "No JSON here at all.";
        let obj = extract_json_object(response);
        assert!(obj.is_none());
    }

    #[test]
    fn test_extract_json_object_with_string_braces() {
        let response = r#"{"key": "value with { braces }"}"#;
        let obj = extract_json_object(response);
        assert!(obj.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&obj.unwrap()).unwrap();
        assert_eq!(parsed["key"], "value with { braces }");
    }

    #[tokio::test]
    async fn test_run_all_with_llm_no_llm() {
        let (store, maint) = setup().await;
        store.add_memory(&test_memory("normal")).await.unwrap();

        // run_all_with_llm with None LLM should just run rule-based tasks
        let report = maint
            .run_all_with_llm(None, 7, 30, 20, 30, 0)
            .await
            .unwrap();
        assert_eq!(report.pruned, 0);
    }

    #[tokio::test]
    async fn test_store_get_recent_active_memories() {
        let (store, _maint) = setup().await;
        store.add_memory(&test_memory("first")).await.unwrap();
        store.add_memory(&test_memory("second")).await.unwrap();
        store.add_memory(&test_memory("third")).await.unwrap();

        let recent = store.get_recent_active_memories(2).await.unwrap();
        assert_eq!(recent.len(), 2);
        // Most recent first
        assert_eq!(recent[0].content, "third");
        assert_eq!(recent[1].content, "second");
    }

    #[tokio::test]
    async fn test_store_get_recent_summaries_empty() {
        let (store, _maint) = setup().await;
        store
            .add_memory(&test_memory("not a summary"))
            .await
            .unwrap();

        let summaries = store.get_recent_summaries(30, 10).await.unwrap();
        assert!(summaries.is_empty());
    }

    #[tokio::test]
    async fn test_store_get_recent_summaries() {
        let (store, _maint) = setup().await;
        store
            .add_memory(&NewMemory {
                content: "Summary of coding session".to_string(),
                source: None,
                tags: None,
                embedding: None,
                memory_type: Some("session_summary".to_string()),
                importance: Some(0.65),
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
            })
            .await
            .unwrap();

        let summaries = store.get_recent_summaries(30, 10).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].content, "Summary of coding session");
    }

    // -- Source-aware decay tests --

    #[test]
    fn test_decay_multiplier_for_memory_type() {
        assert!(
            (MemoryMaintenance::decay_multiplier_for_memory_type("correction") - 0.5).abs() < 0.001
        );
        assert!(
            (MemoryMaintenance::decay_multiplier_for_memory_type("decision") - 0.7).abs() < 0.001
        );
        assert!(
            (MemoryMaintenance::decay_multiplier_for_memory_type("session_summary") - 1.5).abs()
                < 0.001
        );
        assert!((MemoryMaintenance::decay_multiplier_for_memory_type("fact") - 1.0).abs() < 0.001);
        assert!(
            (MemoryMaintenance::decay_multiplier_for_memory_type("unknown") - 1.0).abs() < 0.001
        );
    }

    #[test]
    fn test_decay_multiplier_for_source() {
        assert!(
            (MemoryMaintenance::decay_multiplier_for_source(&SourceType::Brain) - 0.8).abs()
                < 0.001
        );
        assert!(
            (MemoryMaintenance::decay_multiplier_for_source(&SourceType::Markdown) - 0.85).abs()
                < 0.001
        );
        assert!(
            (MemoryMaintenance::decay_multiplier_for_source(&SourceType::CcChronicle) - 0.9).abs()
                < 0.001
        );
        assert!(
            (MemoryMaintenance::decay_multiplier_for_source(&SourceType::CcSession) - 1.0).abs()
                < 0.001
        );
    }

    #[tokio::test]
    async fn test_source_aware_decay() {
        let (store, maint) = setup().await;

        // Insert a correction memory from brain (should decay slower)
        let id = store
            .add_memory(&NewMemory {
                content: "Important correction".to_string(),
                source: None,
                tags: None,
                embedding: None,
                memory_type: Some("correction".to_string()),
                importance: Some(0.8),
                strength: Some(1.0),
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
            })
            .await
            .unwrap();

        // Set accessed_at to 10 days ago
        sqlx::query(
            "UPDATE memories SET accessed_at = strftime('%Y-%m-%dT%H:%M:%f', 'now', '-10 days') \
             WHERE id = ?1",
        )
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

        let decayed = maint.decay_strength_source_aware().await.unwrap();
        assert_eq!(decayed, 1);

        // Check that correction from brain decays slower than normal
        // effective_days = 10 * 0.5 (correction) * 0.8 (brain) = 4.0
        // new_strength = 1.0 * 0.95^4.0 = ~0.8145
        let mem = store.get_memory(id).await.unwrap().unwrap();
        assert!(
            mem.strength > 0.75,
            "Correction from brain should decay slowly, got {}",
            mem.strength
        );
    }

    // -- Cross-source dedup tests --

    #[tokio::test]
    async fn test_merge_cross_source_duplicates() {
        let (store, maint) = setup().await;

        let emb = vec![1.0, 0.0, 0.0];
        // Memory from cc_session (lower authority)
        store
            .add_memory(&NewMemory {
                content: "The port is 18794".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb.clone()),
                memory_type: Some("fact".to_string()),
                importance: Some(0.5),
                strength: None,
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
            })
            .await
            .unwrap();

        // Memory from brain (higher authority) with same embedding
        store
            .add_memory(&NewMemory {
                content: "The port is 18794".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb),
                memory_type: Some("fact".to_string()),
                importance: Some(0.5),
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
            })
            .await
            .unwrap();

        let merged = maint.merge_cross_source_duplicates().await.unwrap();
        assert_eq!(merged, 1, "Should merge one cross-source duplicate");

        // Brain version should survive
        let remaining = store.all_memories().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].source_type, "brain");
    }

    #[tokio::test]
    async fn test_merge_cross_source_skips_same_source() {
        let (store, maint) = setup().await;

        let emb = vec![1.0, 0.0, 0.0];
        store
            .add_memory(&NewMemory {
                content: "Fact A".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb.clone()),
                memory_type: Some("fact".to_string()),
                importance: Some(0.5),
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
            })
            .await
            .unwrap();

        store
            .add_memory(&NewMemory {
                content: "Fact B".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb),
                memory_type: Some("fact".to_string()),
                importance: Some(0.5),
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
            })
            .await
            .unwrap();

        let merged = maint.merge_cross_source_duplicates().await.unwrap();
        assert_eq!(merged, 0, "Should NOT merge same-source duplicates");
        assert_eq!(store.count_memories().await.unwrap(), 2);
    }

    // -- ConsolidationReport test --

    #[tokio::test]
    async fn test_consolidate_returns_report() {
        let (store, maint) = setup().await;
        store.add_memory(&test_memory("normal")).await.unwrap();

        let report = maint.consolidate().await.unwrap();
        // Should complete without errors
        assert_eq!(report.pruned, 0);
        assert_eq!(report.deduped, 0);
        assert_eq!(report.cross_source_merged, 0);
    }

    #[test]
    fn test_source_authority_ranking() {
        assert!(source_authority("brain") > source_authority("markdown"));
        assert!(source_authority("markdown") > source_authority("cc_chronicle"));
        assert!(source_authority("cc_chronicle") > source_authority("cc_bridge"));
        assert!(source_authority("cc_bridge") > source_authority("cc_session"));
        assert_eq!(source_authority("unknown"), 0);
    }

    // ── ReflectionMeta tests ─────────────────────────────────

    #[test]
    fn test_reflection_meta_serialization() {
        use crate::types::ReflectionMeta;

        let meta = ReflectionMeta {
            trigger: "Used wrong API endpoint".to_string(),
            analysis: "Confused /v1/chat with /v1/completions".to_string(),
            lesson: "Always check the API docs before making requests".to_string(),
            confidence: 0.85,
        };

        let json = serde_json::to_string(&meta).unwrap();
        let deserialized: ReflectionMeta = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.trigger, "Used wrong API endpoint");
        assert_eq!(
            deserialized.analysis,
            "Confused /v1/chat with /v1/completions"
        );
        assert_eq!(
            deserialized.lesson,
            "Always check the API docs before making requests"
        );
        assert!((deserialized.confidence - 0.85).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_reflection_meta_persisted() {
        use crate::types::{ReflectionMeta, TrustLevel};

        let store = BrainStore::open_memory().await.unwrap();

        let reflection = ReflectionMeta {
            trigger: "Misread config file".to_string(),
            analysis: "Assumed default values instead of reading actual config".to_string(),
            lesson: "Always read config files before suggesting changes".to_string(),
            confidence: 0.9,
        };

        let id = store
            .add_memory(&NewMemory {
                content: "Correction: always read config first".to_string(),
                source: None,
                tags: None,
                embedding: None,
                memory_type: Some("correction".to_string()),
                importance: Some(0.75),
                strength: None,
                supersedes: None,
                content_hash: None,
                valid_from: None,
                valid_to: None,
                source_type: None,
                reflection: Some(reflection),
                trust_level: Some(TrustLevel::UserStated),
                evidence_session_id: Some("session-123".to_string()),
                evidence_message_id: Some("msg-456".to_string()),
                scope: None,
                procedure: None,
                quality_score: None,
                quality_flag: None,
            })
            .await
            .unwrap();

        let mem = store.get_memory(id).await.unwrap().unwrap();
        assert!(mem.reflection.is_some());
        let r = mem.reflection.unwrap();
        assert_eq!(r.trigger, "Misread config file");
        assert!((r.confidence - 0.9).abs() < 0.001);
        assert_eq!(mem.trust_level, TrustLevel::UserStated);
        assert_eq!(mem.evidence_session_id.as_deref(), Some("session-123"));
        assert_eq!(mem.evidence_message_id.as_deref(), Some("msg-456"));
    }

    // ── Trust-based conflict resolution tests ────────────────

    #[tokio::test]
    async fn test_resolve_by_trust_higher_wins() {
        use crate::types::TrustLevel;

        let (store, maint) = setup().await;
        let emb = vec![1.0, 0.0, 0.0];

        // Low trust memory
        store
            .add_memory(&NewMemory {
                content: "Port is 8080".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb.clone()),
                memory_type: Some("fact".to_string()),
                importance: Some(0.5),
                strength: None,
                supersedes: None,
                content_hash: None,
                valid_from: None,
                valid_to: None,
                source_type: None,
                reflection: None,
                trust_level: Some(TrustLevel::ModelInferred),
                evidence_session_id: None,
                evidence_message_id: None,
                scope: None,
                procedure: None,
                quality_score: None,
                quality_flag: None,
            })
            .await
            .unwrap();

        // High trust memory (same content, same embedding)
        let high_id = store
            .add_memory(&NewMemory {
                content: "Port is 18794".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb),
                memory_type: Some("fact".to_string()),
                importance: Some(0.5),
                strength: None,
                supersedes: None,
                content_hash: None,
                valid_from: None,
                valid_to: None,
                source_type: None,
                reflection: None,
                trust_level: Some(TrustLevel::UserStated),
                evidence_session_id: None,
                evidence_message_id: None,
                scope: None,
                procedure: None,
                quality_score: None,
                quality_flag: None,
            })
            .await
            .unwrap();

        let resolved = maint.resolve_by_trust().await.unwrap();
        assert_eq!(resolved, 1);

        let remaining = store.all_memories().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, high_id);
        assert_eq!(remaining[0].trust_level, TrustLevel::UserStated);
    }

    #[tokio::test]
    async fn test_resolve_by_trust_equal_trust_newer_wins() {
        use crate::types::TrustLevel;

        let (store, maint) = setup().await;
        let emb = vec![1.0, 0.0, 0.0];

        // Older memory (same trust)
        store
            .add_memory(&NewMemory {
                content: "Old version".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb.clone()),
                memory_type: Some("fact".to_string()),
                importance: Some(0.5),
                strength: None,
                supersedes: None,
                content_hash: None,
                valid_from: None,
                valid_to: None,
                source_type: None,
                reflection: None,
                trust_level: Some(TrustLevel::UserStated),
                evidence_session_id: None,
                evidence_message_id: None,
                scope: None,
                procedure: None,
                quality_score: None,
                quality_flag: None,
            })
            .await
            .unwrap();

        // Newer memory (same trust, higher ID)
        let new_id = store
            .add_memory(&NewMemory {
                content: "New version".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb),
                memory_type: Some("fact".to_string()),
                importance: Some(0.5),
                strength: None,
                supersedes: None,
                content_hash: None,
                valid_from: None,
                valid_to: None,
                source_type: None,
                reflection: None,
                trust_level: Some(TrustLevel::UserStated),
                evidence_session_id: None,
                evidence_message_id: None,
                scope: None,
                procedure: None,
                quality_score: None,
                quality_flag: None,
            })
            .await
            .unwrap();

        let resolved = maint.resolve_by_trust().await.unwrap();
        assert_eq!(resolved, 1);

        let remaining = store.all_memories().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].id, new_id,
            "newer memory should win on equal trust"
        );
    }

    #[tokio::test]
    async fn test_resolve_by_trust_skips_different_types() {
        use crate::types::TrustLevel;

        let (store, maint) = setup().await;
        let emb = vec![1.0, 0.0, 0.0];

        // Fact
        store
            .add_memory(&NewMemory {
                content: "A fact".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb.clone()),
                memory_type: Some("fact".to_string()),
                importance: Some(0.5),
                strength: None,
                supersedes: None,
                content_hash: None,
                valid_from: None,
                valid_to: None,
                source_type: None,
                reflection: None,
                trust_level: Some(TrustLevel::ModelInferred),
                evidence_session_id: None,
                evidence_message_id: None,
                scope: None,
                procedure: None,
                quality_score: None,
                quality_flag: None,
            })
            .await
            .unwrap();

        // Decision (different type, same embedding)
        store
            .add_memory(&NewMemory {
                content: "A decision".to_string(),
                source: None,
                tags: None,
                embedding: Some(emb),
                memory_type: Some("decision".to_string()),
                importance: Some(0.5),
                strength: None,
                supersedes: None,
                content_hash: None,
                valid_from: None,
                valid_to: None,
                source_type: None,
                reflection: None,
                trust_level: Some(TrustLevel::UserStated),
                evidence_session_id: None,
                evidence_message_id: None,
                scope: None,
                procedure: None,
                quality_score: None,
                quality_flag: None,
            })
            .await
            .unwrap();

        let resolved = maint.resolve_by_trust().await.unwrap();
        assert_eq!(
            resolved, 0,
            "Should not resolve across different memory types"
        );
        assert_eq!(store.count_memories().await.unwrap(), 2);
    }
}
