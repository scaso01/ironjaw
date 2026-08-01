use crate::embedder::Embedder;
use crate::graph::KnowledgeGraph;
use crate::store::BrainStore;
use crate::types::Memory;
use sqlx::Row;
use tracing::{debug, warn};

/// Memory types that are time-sensitive and should receive a freshness penalty
/// during live-state queries. Non-time-sensitive types (Fact, Identity, etc.)
/// pass through without penalty.
///
/// Decision, Context, Observation change frequently and are cheapest to re-verify.
fn is_time_sensitive_type(memory_type: &str) -> bool {
    matches!(memory_type, "decision" | "context" | "observation")
}

/// Composite re-ranking score combining multiple signals.
///
/// Blends RRF fusion score with recency, importance, trust level,
/// Ebbinghaus strength, and access frequency into a single ranking signal.
///
/// Weights: semantic 0.35, recency 0.20, importance 0.15, trust 0.15,
/// strength 0.10, frequency 0.05.
///
/// When `live_state_active` is true, time-sensitive memory types
/// (Decision, Context, Observation) receive an additional exponential freshness
/// multiplier: `exp(-age_hours * decay_rate)`. Non-time-sensitive types are
/// unaffected. A `decay_rate` of 0.0 disables the penalty entirely.
#[allow(clippy::too_many_arguments)]
pub fn composite_score(
    rrf_score: f32,
    recency_days: f64,
    importance: f64,
    strength: f64,
    access_count: i64,
    max_access: i64,
    trust_score: f32,
    live_state_active: bool,
    memory_type: &str,
    decay_rate: f32,
) -> f32 {
    let semantic = rrf_score;
    let recency = (1.0 / (1.0 + recency_days / 30.0)) as f32;
    let freq = if max_access > 0 {
        access_count as f32 / max_access as f32
    } else {
        0.0
    };

    let base = semantic * 0.35
        + recency * 0.20
        + importance as f32 * 0.15
        + trust_score * 0.15
        + strength as f32 * 0.10
        + freq * 0.05;

    if live_state_active && decay_rate > 0.0 && is_time_sensitive_type(memory_type) {
        let age_hours = recency_days * 24.0;
        let freshness_mult = (-(age_hours as f32) * decay_rate).exp();
        base * freshness_mult
    } else {
        base
    }
}

/// Weight multiplier applied to the FTS (keyword) retriever's contribution in
/// Reciprocal Rank Fusion. Values >1.0 favor exact-term / proper-noun matches
/// over purely semantic neighbors; vector hits keep an implicit weight of 1.0.
const FTS_RRF_WEIGHT: f64 = 2.0;

/// Weight multiplier for the knowledge-graph retriever's contribution in RRF.
/// Positioned between vector (1.0) and FTS (2.0): graph hits are exact-entity
/// matches (higher signal than a fuzzy vector neighbor) but are unranked within
/// the graph result set (lower signal than an FTS exact-term hit).
const GRAPH_RRF_WEIGHT: f64 = 1.5;

/// Returns true if the query is empty / whitespace-only, i.e. not worth a
/// retrieval call.
///
/// Single-word queries are intentionally allowed: keyword (FTS) recall is
/// strongest on exactly those (proper nouns like "proxmox"). The previous
/// 2-word / 3-char minimum was a holdover from the vector-only era -- short
/// queries embedded poorly and produced noise -- but it now blocks the precise
/// lookups hybrid search was added to handle.
pub fn should_skip_retrieval(query: &str) -> bool {
    query.trim().is_empty()
}

/// Filter superseded memories from search results, replacing them with their
/// latest version by following the supersession chain.
///
/// If multiple results resolve to the same latest version, deduplicates them.
pub async fn filter_superseded(
    results: Vec<Memory>,
    store: &BrainStore,
) -> anyhow::Result<Vec<Memory>> {
    let mut seen_ids = std::collections::HashSet::new();
    let mut filtered = Vec::with_capacity(results.len());

    for mem in results {
        if mem.superseded_by.is_some() {
            // Follow chain to the latest version
            let latest_id = store.follow_supersession_chain(mem.id).await?;
            if !seen_ids.insert(latest_id) {
                // Already included this latest version -- skip duplicate
                continue;
            }
            if let Some(latest) = store.get_memory(latest_id).await? {
                filtered.push(latest);
            }
        } else {
            if !seen_ids.insert(mem.id) {
                continue;
            }
            filtered.push(mem);
        }
    }

    Ok(filtered)
}

/// Hybrid search combining vector similarity and FTS5 keyword search.
/// Results merged using Reciprocal Rank Fusion (RRF).
pub struct HybridSearch {
    store: BrainStore,
    embedder: Embedder,
    /// Minimum composite score for results (0.0-1.0). Results below this are filtered out.
    memory_relevance_threshold: f32,
    /// Optional knowledge graph for graph-based retrieval in RRF fusion.
    /// `None` = graph retriever disabled (backward-compatible).
    graph: Option<KnowledgeGraph>,
}

impl HybridSearch {
    pub fn new(store: BrainStore, embedder: Embedder) -> Self {
        Self {
            store,
            embedder,
            memory_relevance_threshold: 0.0,
            graph: None,
        }
    }

    /// Create with a relevance threshold. Results with composite score below this are filtered.
    pub fn with_threshold(store: BrainStore, embedder: Embedder, threshold: f32) -> Self {
        Self {
            store,
            embedder,
            memory_relevance_threshold: threshold,
            graph: None,
        }
    }

    /// Attach a knowledge graph as a third retriever in RRF fusion.
    ///
    /// When set, graph-linked memories for the query's entities are folded in
    /// alongside vector and FTS results. When `None` (default), graph retrieval
    /// is a no-op, so existing graph-less callers are unaffected.
    pub fn set_graph(&mut self, graph: KnowledgeGraph) {
        self.graph = Some(graph);
    }

    /// Initialize FTS5 virtual table (call once during setup).
    pub async fn init_fts(&self) -> anyhow::Result<()> {
        // Create FTS5 virtual table linked to memories content
        sqlx::query(
            r#"CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                content,
                source,
                tags,
                content='memories',
                content_rowid='id'
            )"#,
        )
        .execute(self.store.pool())
        .await?;

        // Create triggers to keep FTS in sync with memories table
        // INSERT trigger
        sqlx::query(
            r#"CREATE TRIGGER IF NOT EXISTS memories_fts_insert AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, content, source, tags)
                VALUES (new.id, new.content, new.source, new.tags);
            END"#,
        )
        .execute(self.store.pool())
        .await?;

        // DELETE trigger
        sqlx::query(
            r#"CREATE TRIGGER IF NOT EXISTS memories_fts_delete AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, content, source, tags)
                VALUES ('delete', old.id, old.content, old.source, old.tags);
            END"#,
        )
        .execute(self.store.pool())
        .await?;

        // UPDATE trigger
        sqlx::query(
            r#"CREATE TRIGGER IF NOT EXISTS memories_fts_update AFTER UPDATE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, content, source, tags)
                VALUES ('delete', old.id, old.content, old.source, old.tags);
                INSERT INTO memories_fts(rowid, content, source, tags)
                VALUES (new.id, new.content, new.source, new.tags);
            END"#,
        )
        .execute(self.store.pool())
        .await?;

        // Rebuild FTS index from existing data
        self.rebuild_fts_index().await?;

        debug!("FTS5 hybrid search initialized");
        Ok(())
    }

    /// Rebuild FTS5 index from existing memories (for initial population or repair).
    ///
    /// External-content FTS5 tables MUST be bulk-populated via the special
    /// `'rebuild'` command. A manual `DELETE` + `INSERT ... SELECT` produces a
    /// "database disk image is malformed" error (SQLITE_CORRUPT_VTAB) on
    /// real-world data, even though it happens to pass on tiny fixtures. The
    /// `'rebuild'` command re-derives the whole index from the content table
    /// (`memories`). Soft-deleted rows are indexed too, but recall fetches go
    /// through `store.get_memory`, which filters `deleted = 0`, so they never
    /// surface in results.
    pub async fn rebuild_fts_index(&self) -> anyhow::Result<u64> {
        sqlx::query("INSERT INTO memories_fts(memories_fts) VALUES('rebuild')")
            .execute(self.store.pool())
            .await?;

        // Report the number of indexed documents.
        let row = sqlx::query("SELECT COUNT(*) AS c FROM memories_fts_docsize")
            .fetch_one(self.store.pool())
            .await?;
        let count: i64 = row.get("c");
        let count = count as u64;
        debug!(count, "FTS index rebuilt");
        Ok(count)
    }

    /// Keyword search using FTS5.
    pub async fn fts_search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<(i64, f64)>> {
        // FTS5 match query -- escape special chars for safety
        let safe_query = sanitize_fts_query(query);
        if safe_query.is_empty() {
            return Ok(Vec::new());
        }

        let rows = sqlx::query(
            r#"SELECT rowid, rank FROM memories_fts
               WHERE memories_fts MATCH ?1
               ORDER BY rank
               LIMIT ?2"#,
        )
        .bind(&safe_query)
        .bind(limit as i64)
        .fetch_all(self.store.pool())
        .await;

        match rows {
            Ok(rows) => Ok(rows
                .iter()
                .map(|r| {
                    let id: i64 = r.get("rowid");
                    let rank: f64 = r.get("rank");
                    // FTS5 rank is negative (closer to 0 = better), normalize to [0, 1]
                    let score = 1.0 / (1.0 + rank.abs());
                    (id, score)
                })
                .collect()),
            Err(e) => {
                warn!(error = %e, query = %safe_query, "FTS search failed");
                Ok(Vec::new())
            }
        }
    }

    /// Vector similarity search using VecIndex (fast path) or DB scan (fallback).
    pub async fn vector_search(
        &self,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(i64, f64)>> {
        let query_embedding = match self.embedder.embed(query).await {
            Ok(emb) => emb,
            Err(e) => {
                warn!(error = %e, "vector search embedding failed");
                return Ok(Vec::new());
            }
        };

        // Delegate to BrainStore::knn_search which uses VecIndex if available
        let results = self.store.knn_search(&query_embedding, limit).await?;
        Ok(results
            .into_iter()
            .map(|(id, score)| (id, score as f64))
            .collect())
    }

    /// Graph-based entity retrieval for RRF fusion.
    ///
    /// Extracts entity candidates from `query` (the graph's heuristic extractor
    /// plus the full trimmed query as a fallback, so single-word known-entity
    /// queries like "proxmox" still match), then calls `recall_by_entity` for
    /// each candidate and collects the union of memory IDs in encounter order.
    ///
    /// Returns memory IDs (deduplicated), or an empty vec when no graph is
    /// attached or no entities match. Never errors -- failures degrade to empty.
    async fn graph_search(&self, query: &str, limit: usize) -> Vec<i64> {
        let graph = match &self.graph {
            Some(g) => g,
            None => return Vec::new(),
        };

        // Candidate entity names: heuristic extractor + whole-query fallback.
        let mut candidates = KnowledgeGraph::extract_entity_candidates(query);
        let trimmed = query.trim().to_string();
        if !trimmed.is_empty() && !candidates.contains(&trimmed) {
            candidates.push(trimmed);
        }

        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut ids: Vec<i64> = Vec::new();

        for candidate in &candidates {
            let memories = match graph.recall_by_entity(candidate, &self.store, limit).await {
                Ok(m) => m,
                Err(e) => {
                    warn!(entity = %candidate, error = %e, "graph_search: recall_by_entity failed");
                    continue;
                }
            };
            for mem in memories {
                if seen.insert(mem.id) {
                    ids.push(mem.id);
                }
                if ids.len() >= limit {
                    return ids;
                }
            }
        }

        ids
    }

    /// Hybrid search combining vector + FTS5 using Reciprocal Rank Fusion.
    ///
    /// RRF formula: `score(d) = sum(1 / (k + rank_i(d)))` for each retriever i.
    /// k = 60 (standard constant that balances rank contributions).
    pub async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Memory>> {
        let k = 60.0_f64;
        let candidate_limit = limit * 3; // Fetch more candidates for better RRF merge

        // Run all three retrievers
        let vector_results = self.vector_search(query, candidate_limit).await?;
        let fts_results = self.fts_search(query, candidate_limit).await?;
        // Graph retrieval: entity-match on the query. No-op when graph is None.
        let graph_ids = self.graph_search(query, candidate_limit).await;

        // Build RRF scores
        let mut rrf_scores: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();

        for (rank, (id, _score)) in vector_results.iter().enumerate() {
            *rrf_scores.entry(*id).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }

        for (rank, (id, _score)) in fts_results.iter().enumerate() {
            *rrf_scores.entry(*id).or_default() += FTS_RRF_WEIGHT / (k + rank as f64 + 1.0);
        }

        // Graph ids are in encounter order (unscored), so rank within the graph list.
        for (rank, id) in graph_ids.iter().enumerate() {
            *rrf_scores.entry(*id).or_default() += GRAPH_RRF_WEIGHT / (k + rank as f64 + 1.0);
        }

        // Sort by RRF score descending
        let mut ranked: Vec<(i64, f64)> = rrf_scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit);

        // Apply relevance threshold: filter out low-scoring results,
        // but always return at least 1 result if any candidates exist.
        if self.memory_relevance_threshold > 0.0 && ranked.len() > 1 {
            let threshold = self.memory_relevance_threshold as f64;
            let first = ranked.first().cloned();
            ranked.retain(|(_, score)| *score >= threshold);
            if ranked.is_empty() {
                if let Some(best) = first {
                    ranked.push(best);
                }
            }
        }

        // Fetch full memory objects
        let mut results = Vec::new();
        for (id, _score) in &ranked {
            if let Some(memory) = self.store.get_memory(*id).await? {
                self.store.update_accessed(*id).await?;
                results.push(memory);
            }
        }

        Ok(results)
    }

    /// Enhanced search with composite re-ranking.
    ///
    /// 1. Runs standard RRF hybrid search to get initial candidates.
    /// 2. Fetches full Memory records for composite scoring.
    /// 3. Re-ranks by blending semantic relevance, recency, importance,
    ///    Ebbinghaus strength, and access frequency.
    /// 4. Returns results sorted by composite score (descending).
    pub async fn search_with_reranking(
        &self,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Memory>> {
        let scored = self.search_with_reranking_scored(query, limit).await?;
        Ok(scored.into_iter().map(|(m, _)| m).collect())
    }

    /// Hybrid search with composite re-ranking, returning memories paired with their scores.
    ///
    /// 1. Runs standard RRF hybrid search to get initial candidates.
    /// 2. Fetches full Memory records for composite scoring.
    /// 3. Re-ranks by blending semantic relevance, recency, importance,
    ///    Ebbinghaus strength, and access frequency.
    /// 4. Returns `(Memory, f32)` pairs sorted by composite score (descending).
    pub async fn search_with_reranking_scored(
        &self,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(Memory, f32)>> {
        let k = 60.0_f64;
        // Fetch more candidates than needed to allow re-ranking to surface better results
        let candidate_limit = limit * 3;

        // Run all three retrievers
        let vector_results = self.vector_search(query, candidate_limit).await?;
        let fts_results = self.fts_search(query, candidate_limit).await?;
        // Graph retrieval: entity-match on the query. No-op when graph is None.
        let graph_ids = self.graph_search(query, candidate_limit).await;

        // Build RRF scores
        let mut rrf_scores: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();

        for (rank, (id, _score)) in vector_results.iter().enumerate() {
            *rrf_scores.entry(*id).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }

        for (rank, (id, _score)) in fts_results.iter().enumerate() {
            *rrf_scores.entry(*id).or_default() += FTS_RRF_WEIGHT / (k + rank as f64 + 1.0);
        }

        // Graph ids are in encounter order (unscored), so rank within the graph list.
        for (rank, id) in graph_ids.iter().enumerate() {
            *rrf_scores.entry(*id).or_default() += GRAPH_RRF_WEIGHT / (k + rank as f64 + 1.0);
        }

        // Fetch full memories for all candidates
        let mut candidates: Vec<(Memory, f64)> = Vec::new();
        for (id, rrf) in &rrf_scores {
            if let Some(memory) = self.store.get_memory(*id).await? {
                candidates.push((memory, *rrf));
            }
        }

        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        // Compute max access count for frequency normalization
        let max_access = candidates
            .iter()
            .map(|(m, _)| m.access_count)
            .max()
            .unwrap_or(1);

        // Normalize RRF relevance to [0, 1] across the candidate set so it
        // competes fairly with the other [0,1]-scale signals in composite
        // scoring. Raw RRF values are tiny (~0.02-0.06); without this a
        // high-importance but irrelevant memory can outrank an exact match.
        let max_rrf = candidates
            .iter()
            .map(|(_, rrf)| *rrf)
            .fold(0.0_f64, f64::max);

        let now = chrono::Utc::now();

        // Re-rank with composite scoring
        let mut scored: Vec<(Memory, f32)> = candidates
            .into_iter()
            .map(|(mem, rrf)| {
                let norm_rrf = if max_rrf > 0.0 {
                    (rrf / max_rrf) as f32
                } else {
                    0.0
                };
                let recency_days =
                    chrono::NaiveDateTime::parse_from_str(&mem.accessed_at, "%Y-%m-%dT%H:%M:%S%.f")
                        .or_else(|_| {
                            chrono::NaiveDateTime::parse_from_str(
                                &mem.accessed_at,
                                "%Y-%m-%dT%H:%M:%S",
                            )
                        })
                        .or_else(|_| {
                            mem.accessed_at
                                .parse::<chrono::DateTime<chrono::Utc>>()
                                .map(|dt| dt.naive_utc())
                        })
                        .map(|dt| (now - dt.and_utc()).num_seconds().max(0) as f64 / 86400.0)
                        .unwrap_or(365.0); // default to old if parse fails

                let score = composite_score(
                    norm_rrf,
                    recency_days,
                    mem.importance,
                    mem.strength,
                    mem.access_count,
                    max_access,
                    mem.trust_level.score(),
                    false,
                    &mem.memory_type,
                    0.0,
                );
                (mem, score)
            })
            .collect();

        // Sort descending by composite score
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        // Apply relevance threshold: filter out low-scoring results,
        // but always return at least 1 result if any candidates exist.
        if self.memory_relevance_threshold > 0.0 && scored.len() > 1 {
            let threshold = self.memory_relevance_threshold;
            let first = scored.first().map(|(m, s)| (m.clone(), *s));
            scored.retain(|(_, score)| *score >= threshold);
            if scored.is_empty() {
                if let Some(best) = first {
                    scored.push(best);
                }
            }
        }

        // Update access timestamps for returned results
        for (mem, _score) in &scored {
            self.store.update_accessed(mem.id).await?;
        }

        Ok(scored)
    }
}

/// Sanitize FTS5 query to prevent syntax errors.
/// FTS5 uses special syntax (AND, OR, NOT, *, ", etc.)
/// We strip dangerous characters and wrap terms for safety.
fn sanitize_fts_query(query: &str) -> String {
    // Remove FTS5 special characters that could cause syntax errors
    let cleaned: String = query
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c == '-' || *c == '_')
        .collect();

    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Split into words and join with implicit AND
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.len() == 1 {
        format!("\"{}\"*", words[0])
    } else {
        // Quote each word for exact matching, join with space (implicit AND in FTS5)
        words
            .iter()
            .map(|w| format!("\"{w}\""))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NewMemory;

    async fn setup() -> (BrainStore, HybridSearch) {
        let store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let search = HybridSearch::new(store.clone(), embedder);
        search.init_fts().await.unwrap();
        (store, search)
    }

    fn mem(content: &str) -> NewMemory {
        NewMemory {
            content: content.to_string(),
            source: Some("test".to_string()),
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

    #[test]
    fn test_sanitize_fts_query() {
        assert_eq!(sanitize_fts_query("hello"), "\"hello\"*");
        assert_eq!(sanitize_fts_query("hello world"), "\"hello\" \"world\"");
        assert_eq!(sanitize_fts_query(""), "");
        assert_eq!(sanitize_fts_query("***"), "");
        assert_eq!(
            sanitize_fts_query("rust programming"),
            "\"rust\" \"programming\""
        );
    }

    #[tokio::test]
    async fn test_fts_search() {
        let (store, search) = setup().await;
        store
            .add_memory(&mem("Rust is a systems programming language"))
            .await
            .unwrap();
        store
            .add_memory(&mem("Python is interpreted"))
            .await
            .unwrap();
        store
            .add_memory(&mem("Rust has strong type safety"))
            .await
            .unwrap();

        // Rebuild index to include new memories
        search.rebuild_fts_index().await.unwrap();

        let results = search.fts_search("Rust", 10).await.unwrap();
        assert_eq!(results.len(), 2); // Two memories mention Rust
    }

    #[tokio::test]
    async fn test_fts_search_no_match() {
        let (store, search) = setup().await;
        store.add_memory(&mem("Hello world")).await.unwrap();
        search.rebuild_fts_index().await.unwrap();

        let results = search.fts_search("nonexistent", 10).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_hybrid_search() {
        let (store, search) = setup().await;
        store
            .add_memory(&mem("Rust is a systems programming language"))
            .await
            .unwrap();
        store
            .add_memory(&mem("Python is interpreted"))
            .await
            .unwrap();
        store.add_memory(&mem("Go is compiled")).await.unwrap();

        search.rebuild_fts_index().await.unwrap();

        let results = search.search("Rust programming", 5).await.unwrap();
        // Should find at least the Rust memory via FTS
        assert!(!results.is_empty());
        // First result should be the most relevant
        assert!(results[0].content.contains("Rust"));
    }

    #[tokio::test]
    async fn test_hybrid_search_empty() {
        let (_store, search) = setup().await;
        let results = search.search("anything", 5).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_rebuild_fts_index() {
        let (store, search) = setup().await;
        store.add_memory(&mem("First")).await.unwrap();
        store.add_memory(&mem("Second")).await.unwrap();

        let count = search.rebuild_fts_index().await.unwrap();
        assert_eq!(count, 2);
    }

    // ── Phase 4: Enhanced Retrieval tests ─────────────────────

    #[test]
    fn test_composite_score_weights() {
        // All signals at maximum: rrf=1.0, recency=0 days (so 1.0), importance=1.0,
        // strength=1.0, access=max, trust=1.0
        let score = composite_score(1.0, 0.0, 1.0, 1.0, 10, 10, 1.0, false, "fact", 0.0);
        // Expected: 1.0*0.35 + 1.0*0.20 + 1.0*0.15 + 1.0*0.15 + 1.0*0.10 + 1.0*0.05 = 1.0
        assert!(
            (score - 1.0).abs() < 0.01,
            "all-max score should be ~1.0, got {score}"
        );

        // All signals at minimum: rrf=0.0, very old, importance=0, strength=0, no access, trust=0
        let score_min = composite_score(0.0, 3650.0, 0.0, 0.0, 0, 10, 0.0, false, "fact", 0.0);
        assert!(
            score_min < 0.05,
            "all-min score should be near 0, got {score_min}"
        );

        // Verify semantic weight dominates: high rrf, everything else low
        let semantic_heavy = composite_score(1.0, 365.0, 0.0, 0.0, 0, 10, 0.0, false, "fact", 0.0);
        let recency_heavy = composite_score(0.0, 0.0, 0.0, 0.0, 0, 10, 0.0, false, "fact", 0.0);
        assert!(
            semantic_heavy > recency_heavy,
            "semantic signal should dominate: {semantic_heavy} > {recency_heavy}"
        );

        // Verify recency matters: same rrf, different recency
        let recent = composite_score(0.5, 1.0, 0.5, 0.5, 5, 10, 0.5, false, "fact", 0.0);
        let old = composite_score(0.5, 365.0, 0.5, 0.5, 5, 10, 0.5, false, "fact", 0.0);
        assert!(recent > old, "recent should rank higher: {recent} > {old}");
    }

    #[test]
    fn test_should_skip_retrieval() {
        // Only empty / whitespace-only queries are skipped now.
        assert!(should_skip_retrieval(""));
        assert!(should_skip_retrieval("  "));
    }

    #[test]
    fn test_should_not_skip_real_query() {
        // Single-word queries (esp. proper nouns) must search: keyword recall
        // is strongest on exactly those.
        assert!(!should_skip_retrieval("hi"));
        assert!(!should_skip_retrieval("proxmox"));
        assert!(!should_skip_retrieval("Rust"));
        assert!(!should_skip_retrieval("what is Rust"));
        assert!(!should_skip_retrieval("Rust programming"));
        assert!(!should_skip_retrieval("tell me about memory"));
        assert!(!should_skip_retrieval("how does this work"));
    }

    #[tokio::test]
    async fn test_filter_superseded() {
        let store = BrainStore::open_memory().await.unwrap();

        // Create original memory
        let old_id = store
            .add_memory(&NewMemory {
                content: "Rust 1.70 released".to_string(),
                source: Some("test".to_string()),
                tags: None,
                embedding: None,
                memory_type: None,
                importance: None,
                strength: None,
                supersedes: None,
                content_hash: Some("hash1".to_string()),
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

        // Create newer version that supersedes the old one
        let new_id = store
            .add_memory(&NewMemory {
                content: "Rust 1.93 released (supersedes 1.70 note)".to_string(),
                source: Some("test".to_string()),
                tags: None,
                embedding: None,
                memory_type: None,
                importance: None,
                strength: None,
                supersedes: Some(old_id),
                content_hash: Some("hash2".to_string()),
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

        // Create a standalone memory (not superseded)
        let standalone_id = store
            .add_memory(&NewMemory {
                content: "Python is interpreted".to_string(),
                source: Some("test".to_string()),
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
            })
            .await
            .unwrap();

        // Build a fake search result containing the OLD superseded memory + standalone
        let old_mem = store.get_memory(old_id).await.unwrap().unwrap();
        let standalone_mem = store.get_memory(standalone_id).await.unwrap().unwrap();

        let results = vec![old_mem, standalone_mem];
        let filtered = filter_superseded(results, &store).await.unwrap();

        assert_eq!(filtered.len(), 2, "should have 2 results after filtering");
        // The old memory should be replaced by the new version
        assert_eq!(
            filtered[0].id, new_id,
            "old memory should be replaced by latest"
        );
        assert!(
            filtered[0].content.contains("1.93"),
            "should contain the latest content"
        );
        // Standalone should remain unchanged
        assert_eq!(filtered[1].id, standalone_id);
    }

    #[tokio::test]
    async fn test_filter_superseded_deduplicates() {
        let store = BrainStore::open_memory().await.unwrap();

        // Create chain: v1 -> v2 -> v3
        let v1_id = store
            .add_memory(&NewMemory {
                content: "version 1".to_string(),
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
            })
            .await
            .unwrap();
        let v2_id = store
            .add_memory(&NewMemory {
                content: "version 2".to_string(),
                source: None,
                tags: None,
                embedding: None,
                memory_type: None,
                importance: None,
                strength: None,
                supersedes: Some(v1_id),
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
        let v3_id = store
            .add_memory(&NewMemory {
                content: "version 3".to_string(),
                source: None,
                tags: None,
                embedding: None,
                memory_type: None,
                importance: None,
                strength: None,
                supersedes: Some(v2_id),
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

        // Search results contain both v1 and v2 (both superseded, both point to v3)
        let v1 = store.get_memory(v1_id).await.unwrap().unwrap();
        let v2 = store.get_memory(v2_id).await.unwrap().unwrap();

        let results = vec![v1, v2];
        let filtered = filter_superseded(results, &store).await.unwrap();

        // Both should resolve to v3, but only one copy should appear
        assert_eq!(
            filtered.len(),
            1,
            "should deduplicate to single latest version"
        );
        assert_eq!(filtered[0].id, v3_id);
        assert!(filtered[0].content.contains("version 3"));
    }

    // ── Trust signal tests ───────────────────────────────────

    #[test]
    fn test_composite_score_trust_signal() {
        use crate::types::TrustLevel;

        // Two memories identical except trust: SystemPolicy vs ModelInferred
        let high_trust = composite_score(
            0.5,
            7.0,
            0.5,
            0.5,
            5,
            10,
            TrustLevel::SystemPolicy.score(),
            false,
            "fact",
            0.0,
        );
        let low_trust = composite_score(
            0.5,
            7.0,
            0.5,
            0.5,
            5,
            10,
            TrustLevel::ModelInferred.score(),
            false,
            "fact",
            0.0,
        );
        assert!(
            high_trust > low_trust,
            "higher trust should rank higher: {high_trust} > {low_trust}"
        );

        // Verify trust_score contributes exactly 15% weight
        let with_trust = composite_score(0.0, 36500.0, 0.0, 0.0, 0, 10, 1.0, false, "fact", 0.0);
        assert!(
            (with_trust - 0.15).abs() < 0.01,
            "trust-only score should be ~0.15, got {with_trust}"
        );
    }

    #[test]
    fn test_trust_level_score() {
        use crate::types::TrustLevel;

        assert!((TrustLevel::ModelInferred.score() - 0.2).abs() < 0.01);
        assert!((TrustLevel::Retrieved.score() - 0.4).abs() < 0.01);
        assert!((TrustLevel::UserStated.score() - 0.6).abs() < 0.01);
        assert!((TrustLevel::ToolVerified.score() - 0.8).abs() < 0.01);
        assert!((TrustLevel::SystemPolicy.score() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_trust_level_ordering() {
        use crate::types::TrustLevel;

        assert!(TrustLevel::SystemPolicy > TrustLevel::ToolVerified);
        assert!(TrustLevel::ToolVerified > TrustLevel::UserStated);
        assert!(TrustLevel::UserStated > TrustLevel::Retrieved);
        assert!(TrustLevel::Retrieved > TrustLevel::ModelInferred);
    }

    // ── D1: Deterministic trust-ranking tests ─────────────────────
    //
    // These tests verify that composite_score monotonically increases
    // with trust level when all other signals are held constant.
    //
    // Weight breakdown (weights must sum to 1.0):
    //   semantic  0.35  (RRF / cosine similarity)
    //   recency   0.20  (1 / (1 + days/30))
    //   importance 0.15 (user-assigned 0-1 value)
    //   trust     0.15  (TrustLevel::score(), normalized 0.2–1.0)
    //   strength  0.10  (Ebbinghaus salience decay)
    //   frequency 0.05  (access_count / max_access)

    /// When all signals are equal, higher trust must produce a strictly higher score.
    #[test]
    fn test_trust_ranking_monotonic() {
        use crate::types::TrustLevel;

        // Fix all signals except trust: recent, moderate importance/strength, equal access.
        let fixed = |trust_score: f32| {
            composite_score(
                0.5, // rrf_score
                1.0, // recency_days
                0.5, // importance
                0.5, // strength
                5,   // access_count
                10,  // max_access
                trust_score,
                false,
                "fact",
                0.0,
            )
        };

        let s_model = fixed(TrustLevel::ModelInferred.score());
        let s_retrieved = fixed(TrustLevel::Retrieved.score());
        let s_user = fixed(TrustLevel::UserStated.score());
        let s_tool = fixed(TrustLevel::ToolVerified.score());
        let s_policy = fixed(TrustLevel::SystemPolicy.score());

        assert!(
            s_model < s_retrieved,
            "ModelInferred ({s_model:.4}) must rank below Retrieved ({s_retrieved:.4})"
        );
        assert!(
            s_retrieved < s_user,
            "Retrieved ({s_retrieved:.4}) must rank below UserStated ({s_user:.4})"
        );
        assert!(
            s_user < s_tool,
            "UserStated ({s_user:.4}) must rank below ToolVerified ({s_tool:.4})"
        );
        assert!(
            s_tool < s_policy,
            "ToolVerified ({s_tool:.4}) must rank below SystemPolicy ({s_policy:.4})"
        );
    }

    /// Trust contributes exactly 0.15 * trust_score to composite score.
    /// Verify the per-level trust contribution is within floating-point tolerance.
    #[test]
    fn test_trust_contribution_exact() {
        use crate::types::TrustLevel;

        // Isolate trust signal: set all other signals to 0 (except rrf which can't go negative).
        // Use rrf=0, recency=old (→ near 0), importance=0, strength=0, freq=0.
        // With recency_days=10000, recency factor ≈ 1/(1+333.3) ≈ 0.003 → effectively 0.
        let trust_only = |trust_score: f32| {
            composite_score(
                0.0,
                10000.0,
                0.0,
                0.0,
                0,
                1,
                trust_score,
                false,
                "fact",
                0.0,
            )
        };

        for level in [
            TrustLevel::ModelInferred,
            TrustLevel::Retrieved,
            TrustLevel::UserStated,
            TrustLevel::ToolVerified,
            TrustLevel::SystemPolicy,
        ] {
            let expected = level.score() * 0.15;
            let actual = trust_only(level.score());
            // Allow 0.001 tolerance for recency residual
            assert!(
                (actual - expected).abs() < 0.002,
                "{level}: expected trust contribution ~{expected:.4}, got {actual:.4}"
            );
        }
    }

    /// The delta between adjacent trust levels must be equal (uniform spacing).
    /// Each step up contributes 0.2 * 0.15 = 0.03 to composite score.
    #[test]
    fn test_trust_ranking_uniform_step() {
        use crate::types::TrustLevel;

        let trust_only = |trust_score: f32| {
            composite_score(
                0.0,
                10000.0,
                0.0,
                0.0,
                0,
                1,
                trust_score,
                false,
                "fact",
                0.0,
            )
        };

        let scores: Vec<f32> = [
            TrustLevel::ModelInferred,
            TrustLevel::Retrieved,
            TrustLevel::UserStated,
            TrustLevel::ToolVerified,
            TrustLevel::SystemPolicy,
        ]
        .iter()
        .map(|l| trust_only(l.score()))
        .collect();

        // Deltas between adjacent levels should all be ~0.03 (= 0.2 step * 0.15 weight)
        let expected_step = 0.2 * 0.15_f32;
        for i in 1..scores.len() {
            let delta = scores[i] - scores[i - 1];
            assert!(
                (delta - expected_step).abs() < 0.002,
                "trust step {}->{}: expected {expected_step:.4}, got {delta:.4}",
                i - 1,
                i
            );
        }
    }

    /// SystemPolicy trust must beat low-trust memories even with a significant
    /// recency disadvantage (30-day-old SystemPolicy vs 0-day-old ModelInferred).
    #[test]
    fn test_high_trust_beats_recency_gap() {
        use crate::types::TrustLevel;

        let stale_authoritative = composite_score(
            0.5,
            30.0, // 30 days old
            0.5,
            0.5,
            5,
            10,
            TrustLevel::SystemPolicy.score(),
            false,
            "fact",
            0.0,
        );
        let fresh_noise = composite_score(
            0.5,
            0.0, // brand new
            0.5,
            0.5,
            5,
            10,
            TrustLevel::ModelInferred.score(),
            false,
            "fact",
            0.0,
        );
        assert!(
            stale_authoritative > fresh_noise,
            "SystemPolicy 30d-old ({stale_authoritative:.4}) should beat ModelInferred fresh ({fresh_noise:.4})"
        );
    }
}
