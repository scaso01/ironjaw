use crate::store::BrainStore;
use crate::types::{Memory, SourceType};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: i64,
    pub name: String,
    pub entity_type: String,
    pub confidence: f64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relation {
    pub id: i64,
    pub from_id: i64,
    pub to_id: i64,
    pub relation_type: String,
    pub confidence: f64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEntity {
    pub name: String,
    pub entity_type: String,
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewRelation {
    pub from_id: i64,
    pub to_id: i64,
    pub relation_type: String,
    pub confidence: Option<f64>,
}

/// Knowledge graph backed by SQLite
#[derive(Clone)]
pub struct KnowledgeGraph {
    pool: SqlitePool,
}

impl KnowledgeGraph {
    /// Initialize the knowledge graph using an existing pool (shared with BrainStore)
    pub async fn init(pool: SqlitePool) -> anyhow::Result<Self> {
        sqlx::query(include_str!("../migrations/002_create_knowledge_graph.sql"))
            .execute(&pool)
            .await?;
        info!("Knowledge graph initialized");
        Ok(Self { pool })
    }

    // ── Entity CRUD ──────────────────────────────────────────────

    pub async fn add_entity(&self, entity: &NewEntity) -> anyhow::Result<i64> {
        let confidence = entity.confidence.unwrap_or(0.8);
        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO entities (name, entity_type, confidence) \
             VALUES (?1, ?2, ?3) RETURNING id",
        )
        .bind(&entity.name)
        .bind(&entity.entity_type)
        .bind(confidence)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn get_entity(&self, id: i64) -> anyhow::Result<Option<Entity>> {
        let row = sqlx::query(
            "SELECT id, name, entity_type, confidence, created_at \
             FROM entities WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| self.row_to_entity(&r)))
    }

    pub async fn find_entity_by_name(&self, name: &str) -> anyhow::Result<Option<Entity>> {
        let row = sqlx::query(
            "SELECT id, name, entity_type, confidence, created_at \
             FROM entities WHERE name = ?1 COLLATE NOCASE",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| self.row_to_entity(&r)))
    }

    pub async fn search_entities(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Entity>> {
        let pattern = format!("%{query}%");
        let rows = sqlx::query(
            "SELECT id, name, entity_type, confidence, created_at \
             FROM entities WHERE name LIKE ?1 COLLATE NOCASE \
             ORDER BY confidence DESC LIMIT ?2",
        )
        .bind(&pattern)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| self.row_to_entity(r)).collect())
    }

    pub async fn delete_entity(&self, id: i64) -> anyhow::Result<bool> {
        // Cascade deletes relations via FK
        let result = sqlx::query("DELETE FROM entities WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn count_entities(&self) -> anyhow::Result<i64> {
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM entities")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    // ── Relation CRUD ────────────────────────────────────────────

    pub async fn add_relation(&self, rel: &NewRelation) -> anyhow::Result<i64> {
        let confidence = rel.confidence.unwrap_or(0.8);
        let id = sqlx::query_scalar::<_, i64>(
            "INSERT OR IGNORE INTO relations (from_id, to_id, relation_type, confidence) \
             VALUES (?1, ?2, ?3, ?4) RETURNING id",
        )
        .bind(rel.from_id)
        .bind(rel.to_id)
        .bind(&rel.relation_type)
        .bind(confidence)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(0);
        Ok(id)
    }

    pub async fn get_relations_from(&self, entity_id: i64) -> anyhow::Result<Vec<Relation>> {
        let rows = sqlx::query(
            "SELECT id, from_id, to_id, relation_type, confidence, created_at \
             FROM relations WHERE from_id = ?1 ORDER BY confidence DESC",
        )
        .bind(entity_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| self.row_to_relation(r)).collect())
    }

    pub async fn get_relations_to(&self, entity_id: i64) -> anyhow::Result<Vec<Relation>> {
        let rows = sqlx::query(
            "SELECT id, from_id, to_id, relation_type, confidence, created_at \
             FROM relations WHERE to_id = ?1 ORDER BY confidence DESC",
        )
        .bind(entity_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| self.row_to_relation(r)).collect())
    }

    pub async fn delete_relation(&self, id: i64) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM relations WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn count_relations(&self) -> anyhow::Result<i64> {
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM relations")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    // ── Graph Traversal ──────────────────────────────────────────

    /// Get all entities connected to the given entity within `depth` hops
    pub async fn traverse(&self, entity_id: i64, depth: u32) -> anyhow::Result<Vec<Entity>> {
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        let mut result = Vec::new();

        visited.insert(entity_id);
        queue.push_back((entity_id, 0u32));

        while let Some((current_id, current_depth)) = queue.pop_front() {
            if current_depth > 0 {
                if let Some(entity) = self.get_entity(current_id).await? {
                    result.push(entity);
                }
            }

            if current_depth < depth {
                // Get outgoing relations
                let outgoing = self.get_relations_from(current_id).await?;
                for rel in &outgoing {
                    if visited.insert(rel.to_id) {
                        queue.push_back((rel.to_id, current_depth + 1));
                    }
                }
                // Get incoming relations
                let incoming = self.get_relations_to(current_id).await?;
                for rel in &incoming {
                    if visited.insert(rel.from_id) {
                        queue.push_back((rel.from_id, current_depth + 1));
                    }
                }
            }
        }

        Ok(result)
    }

    // ── Auto Entity Extraction ─────────────────────────────────

    /// Extract entity candidates from content text and link them to
    /// a memory via `mentioned_in` relations.
    ///
    /// Uses heuristics to detect entities:
    /// - Capitalized multi-word phrases (e.g. "Ironjaw Gateway", "Dr. CLAW")
    /// - Quoted strings (e.g. `"session_summary"`)
    /// - Existing entities already in the graph (case-insensitive match)
    ///
    /// For each candidate:
    /// - If it already exists, reuses the existing entity ID
    /// - If new, creates it with `entity_type = "concept"` and `confidence = 0.5`
    /// - Creates a `mentioned_in` relation from a synthetic "memory:{id}" entity
    ///   to the detected entity
    ///
    /// Returns the list of entity IDs that were linked.
    pub async fn extract_and_link_entities(
        &self,
        memory_id: i64,
        content: &str,
    ) -> anyhow::Result<Vec<i64>> {
        let candidates = Self::extract_entity_candidates(content);
        if candidates.is_empty() {
            return Ok(vec![]);
        }

        // Create or find a memory entity node
        let memory_name = format!("memory:{memory_id}");
        let memory_entity_id = match self.find_entity_by_name(&memory_name).await? {
            Some(e) => e.id,
            None => {
                self.add_entity(&NewEntity {
                    name: memory_name,
                    entity_type: "memory".to_string(),
                    confidence: Some(1.0),
                })
                .await?
            }
        };

        let mut linked_ids = Vec::new();

        for candidate in &candidates {
            // Find or create the entity
            let entity_id = match self.find_entity_by_name(candidate).await? {
                Some(e) => e.id,
                None => {
                    self.add_entity(&NewEntity {
                        name: candidate.clone(),
                        entity_type: "concept".to_string(),
                        confidence: Some(0.5),
                    })
                    .await?
                }
            };

            // Create relation: memory -> mentioned_in -> entity
            self.add_relation(&NewRelation {
                from_id: memory_entity_id,
                to_id: entity_id,
                relation_type: "mentioned_in".to_string(),
                confidence: Some(0.6),
            })
            .await?;

            linked_ids.push(entity_id);
        }

        if !linked_ids.is_empty() {
            info!(
                memory_id,
                entities = linked_ids.len(),
                "entity extraction: linked entities to memory"
            );
        }

        Ok(linked_ids)
    }

    /// Extract entities from a memory with source-type tracking and cross-source linking.
    ///
    /// Like `extract_and_link_entities`, but additionally:
    /// - Stores which `source_type` the entity was seen in (via entity metadata)
    /// - When an entity already linked from a DIFFERENT source_type, creates a
    ///   `cross_source_link` relation between the two memory nodes
    /// - Detects potential contradictions for flagging
    ///
    /// Returns the list of entity IDs that were linked.
    pub async fn extract_and_link_entities_with_source(
        &self,
        memory_id: i64,
        content: &str,
        source_type: &SourceType,
    ) -> anyhow::Result<Vec<i64>> {
        let candidates = Self::extract_entity_candidates(content);
        if candidates.is_empty() {
            return Ok(vec![]);
        }

        let source_str = source_type.to_string();

        // Create or find the memory entity node
        let memory_name = format!("memory:{memory_id}");
        let memory_entity_id = match self.find_entity_by_name(&memory_name).await? {
            Some(e) => e.id,
            None => {
                self.add_entity(&NewEntity {
                    name: memory_name,
                    entity_type: "memory".to_string(),
                    confidence: Some(1.0),
                })
                .await?
            }
        };

        // Tag the memory entity with its source_type via a source relation
        let source_entity_name = format!("source:{source_str}");
        let source_entity_id = match self.find_entity_by_name(&source_entity_name).await? {
            Some(e) => e.id,
            None => {
                self.add_entity(&NewEntity {
                    name: source_entity_name,
                    entity_type: "source".to_string(),
                    confidence: Some(1.0),
                })
                .await?
            }
        };
        self.add_relation(&NewRelation {
            from_id: memory_entity_id,
            to_id: source_entity_id,
            relation_type: "from_source".to_string(),
            confidence: Some(1.0),
        })
        .await?;

        let mut linked_ids = Vec::new();

        for candidate in &candidates {
            // Find or create the entity
            let entity_id = match self.find_entity_by_name(candidate).await? {
                Some(e) => e.id,
                None => {
                    self.add_entity(&NewEntity {
                        name: candidate.clone(),
                        entity_type: "concept".to_string(),
                        confidence: Some(0.5),
                    })
                    .await?
                }
            };

            // Create mentioned_in relation
            self.add_relation(&NewRelation {
                from_id: memory_entity_id,
                to_id: entity_id,
                relation_type: "mentioned_in".to_string(),
                confidence: Some(0.6),
            })
            .await?;

            // Check for cross-source linking: find other memory nodes that mention
            // this same entity but come from a different source_type
            let incoming = self.get_relations_to(entity_id).await?;
            for rel in &incoming {
                if rel.relation_type != "mentioned_in" || rel.from_id == memory_entity_id {
                    continue;
                }
                // Check if the other memory has a different source
                let other_source = self.get_relations_from(rel.from_id).await?;
                let other_is_different_source = other_source
                    .iter()
                    .any(|r| r.relation_type == "from_source" && r.to_id != source_entity_id);

                if other_is_different_source {
                    // Create cross_source_link between the two memory entities
                    self.add_relation(&NewRelation {
                        from_id: memory_entity_id,
                        to_id: rel.from_id,
                        relation_type: "cross_source_link".to_string(),
                        confidence: Some(0.7),
                    })
                    .await?;
                }
            }

            linked_ids.push(entity_id);
        }

        if !linked_ids.is_empty() {
            info!(
                memory_id,
                source = %source_str,
                entities = linked_ids.len(),
                "entity extraction: linked entities with source tracking"
            );
        }

        Ok(linked_ids)
    }

    /// Graph-traversal recall: find all memories connected to an entity by name.
    ///
    /// Looks up the entity, traverses all `mentioned_in` relations pointing to it,
    /// then loads the actual Memory objects from the store.
    /// Returns memories from ALL source types that mention this entity.
    pub async fn recall_by_entity(
        &self,
        entity_name: &str,
        store: &BrainStore,
        limit: usize,
    ) -> anyhow::Result<Vec<Memory>> {
        let entity = match self.find_entity_by_name(entity_name).await? {
            Some(e) => e,
            None => return Ok(vec![]),
        };

        // Find all memory nodes that have a `mentioned_in` relation to this entity
        let incoming = self.get_relations_to(entity.id).await?;
        let mut memories = Vec::new();

        for rel in &incoming {
            if rel.relation_type != "mentioned_in" {
                continue;
            }
            // rel.from_id is a "memory:N" entity -- extract the memory ID
            if let Some(mem_entity) = self.get_entity(rel.from_id).await? {
                if let Some(id_str) = mem_entity.name.strip_prefix("memory:") {
                    if let Ok(mem_id) = id_str.parse::<i64>() {
                        if let Ok(Some(memory)) = store.get_memory(mem_id).await {
                            memories.push(memory);
                        }
                    }
                }
            }
            if memories.len() >= limit {
                break;
            }
        }

        Ok(memories)
    }

    /// Create a `contradicts` relation between two memory entities.
    ///
    /// Used when two memories about the same entity have conflicting information.
    pub async fn add_contradiction(
        &self,
        memory_id_a: i64,
        memory_id_b: i64,
    ) -> anyhow::Result<()> {
        let name_a = format!("memory:{memory_id_a}");
        let name_b = format!("memory:{memory_id_b}");

        let entity_a = match self.find_entity_by_name(&name_a).await? {
            Some(e) => e.id,
            None => {
                self.add_entity(&NewEntity {
                    name: name_a,
                    entity_type: "memory".to_string(),
                    confidence: Some(1.0),
                })
                .await?
            }
        };

        let entity_b = match self.find_entity_by_name(&name_b).await? {
            Some(e) => e.id,
            None => {
                self.add_entity(&NewEntity {
                    name: name_b,
                    entity_type: "memory".to_string(),
                    confidence: Some(1.0),
                })
                .await?
            }
        };

        self.add_relation(&NewRelation {
            from_id: entity_a,
            to_id: entity_b,
            relation_type: "contradicts".to_string(),
            confidence: Some(0.8),
        })
        .await?;

        info!(memory_id_a, memory_id_b, "Created contradiction relation");
        Ok(())
    }

    /// Extract entity candidate names from text using heuristics.
    ///
    /// Detects:
    /// - Capitalized multi-word sequences (2+ words, each starting uppercase)
    ///   with common leading articles/prepositions stripped
    /// - Single-quoted or double-quoted strings of reasonable length
    ///
    /// `pub(crate)` so the recall path (hybrid_search.rs) can extract entity
    /// candidates from a query string, mirroring the write-path extraction.
    pub(crate) fn extract_entity_candidates(content: &str) -> Vec<String> {
        let mut candidates = std::collections::HashSet::new();

        // Common words that should not start an entity name
        const STRIP_LEADING: &[&str] = &[
            "The", "A", "An", "This", "That", "These", "Those", "My", "Your", "His", "Her", "Its",
            "Our", "Their", "In", "On", "At", "By", "For", "With", "From", "To", "And", "Or",
            "But", "So", "If", "When", "Where",
        ];

        // 1. Capitalized multi-word phrases: 2+ consecutive capitalized words
        let cap_re = regex::Regex::new(r"\b([A-Z][a-zA-Z.]*(?:\s+[A-Z][a-zA-Z.]*)+)\b")
            .expect("valid regex");

        for cap in cap_re.captures_iter(content) {
            let mut phrase = cap[1].trim().to_string();

            // Strip common leading articles/prepositions
            for &prefix in STRIP_LEADING {
                if let Some(rest) = phrase.strip_prefix(prefix) {
                    let rest = rest.trim_start();
                    // Only strip if what remains still has a capitalized word
                    if rest.starts_with(|c: char| c.is_ascii_uppercase()) {
                        phrase = rest.to_string();
                        break;
                    }
                }
            }

            // Must still be multi-word and reasonable length
            if phrase.contains(' ') && phrase.len() >= 3 && phrase.len() <= 60 {
                candidates.insert(phrase);
            }
        }

        // 2. Quoted strings (double or single quotes)
        let quote_re = regex::Regex::new(r#"["']([A-Za-z][A-Za-z0-9_ .-]{1,58}[A-Za-z0-9])["']"#)
            .expect("valid regex");

        for cap in quote_re.captures_iter(content) {
            let quoted = cap[1].trim().to_string();
            if quoted.len() >= 3 && quoted.len() <= 60 {
                candidates.insert(quoted);
            }
        }

        candidates.into_iter().collect()
    }

    // ── I10: Causal Graph ──────────────────────────────────────

    /// Causal relation types for the knowledge graph.
    pub const CAUSAL_RELATION_TYPES: &[&str] = &["caused_by", "leads_to", "prevented_by"];

    /// Add a causal relation between two entities.
    /// `relation_type` must be one of: "caused_by", "leads_to", "prevented_by".
    pub async fn add_causal_relation(
        &self,
        from_id: i64,
        to_id: i64,
        relation_type: &str,
        confidence: Option<f64>,
    ) -> anyhow::Result<i64> {
        if !Self::CAUSAL_RELATION_TYPES.contains(&relation_type) {
            anyhow::bail!(
                "invalid causal relation type: {relation_type}, \
                 expected one of: caused_by, leads_to, prevented_by"
            );
        }
        self.add_relation(&NewRelation {
            from_id,
            to_id,
            relation_type: relation_type.to_string(),
            confidence,
        })
        .await
    }

    /// Trace causal links from an entity in a given direction.
    ///
    /// - `direction = "causes"`: follow `leads_to` outgoing edges
    /// - `direction = "effects"`: follow `caused_by` incoming edges (what was caused by this)
    /// - `direction = "both"`: follow both directions
    ///
    /// Returns entities reachable within `depth` hops along causal edges.
    pub async fn trace(
        &self,
        entity_id: i64,
        direction: &str,
        depth: u32,
    ) -> anyhow::Result<Vec<(Entity, String, u32)>> {
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        let mut result = Vec::new();

        visited.insert(entity_id);
        queue.push_back((entity_id, 0u32));

        while let Some((current_id, current_depth)) = queue.pop_front() {
            if current_depth >= depth {
                continue;
            }

            let mut neighbors = Vec::new();

            // Forward: follow leads_to outgoing, caused_by incoming (things this causes)
            if direction == "causes" || direction == "both" {
                let outgoing = self.get_relations_from(current_id).await?;
                for rel in &outgoing {
                    if rel.relation_type == "leads_to" {
                        neighbors.push((rel.to_id, "leads_to".to_string()));
                    }
                }
                let incoming = self.get_relations_to(current_id).await?;
                for rel in &incoming {
                    if rel.relation_type == "caused_by" {
                        neighbors.push((rel.from_id, "caused_by".to_string()));
                    }
                }
            }

            // Backward: follow caused_by outgoing, leads_to incoming (things that cause this)
            if direction == "effects" || direction == "both" {
                let outgoing = self.get_relations_from(current_id).await?;
                for rel in &outgoing {
                    if rel.relation_type == "caused_by" {
                        neighbors.push((rel.to_id, "caused_by".to_string()));
                    }
                }
                let incoming = self.get_relations_to(current_id).await?;
                for rel in &incoming {
                    if rel.relation_type == "leads_to" {
                        neighbors.push((rel.from_id, "leads_to".to_string()));
                    }
                }
            }

            // Also follow prevented_by in both directions
            if direction == "both" || direction == "causes" || direction == "effects" {
                let outgoing = self.get_relations_from(current_id).await?;
                for rel in &outgoing {
                    if rel.relation_type == "prevented_by" {
                        neighbors.push((rel.to_id, "prevented_by".to_string()));
                    }
                }
                let incoming = self.get_relations_to(current_id).await?;
                for rel in &incoming {
                    if rel.relation_type == "prevented_by" {
                        neighbors.push((rel.from_id, "prevented_by".to_string()));
                    }
                }
            }

            for (neighbor_id, rel_type) in neighbors {
                if visited.insert(neighbor_id) {
                    if let Some(entity) = self.get_entity(neighbor_id).await? {
                        result.push((entity, rel_type, current_depth + 1));
                    }
                    queue.push_back((neighbor_id, current_depth + 1));
                }
            }
        }

        Ok(result)
    }

    /// Get all causal relations (caused_by, leads_to, prevented_by) for an entity.
    pub async fn get_causal_relations(&self, entity_id: i64) -> anyhow::Result<Vec<Relation>> {
        let outgoing = self.get_relations_from(entity_id).await?;
        let incoming = self.get_relations_to(entity_id).await?;

        let mut causal = Vec::new();
        for rel in outgoing.into_iter().chain(incoming) {
            if Self::CAUSAL_RELATION_TYPES.contains(&rel.relation_type.as_str()) {
                causal.push(rel);
            }
        }
        Ok(causal)
    }

    // ── Helpers ──────────────────────────────────────────────────

    fn row_to_entity(&self, row: &sqlx::sqlite::SqliteRow) -> Entity {
        Entity {
            id: row.get("id"),
            name: row.get("name"),
            entity_type: row.get("entity_type"),
            confidence: row.get("confidence"),
            created_at: row.get("created_at"),
        }
    }

    fn row_to_relation(&self, row: &sqlx::sqlite::SqliteRow) -> Relation {
        Relation {
            id: row.get("id"),
            from_id: row.get("from_id"),
            to_id: row.get("to_id"),
            relation_type: row.get("relation_type"),
            confidence: row.get("confidence"),
            created_at: row.get("created_at"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_graph() -> KnowledgeGraph {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        // Run the base brain schema first
        sqlx::query(include_str!("../migrations/001_create_brain.sql"))
            .execute(&pool)
            .await
            .unwrap();
        KnowledgeGraph::init(pool).await.unwrap()
    }

    fn person(name: &str) -> NewEntity {
        NewEntity {
            name: name.to_string(),
            entity_type: "person".to_string(),
            confidence: None,
        }
    }

    fn concept(name: &str) -> NewEntity {
        NewEntity {
            name: name.to_string(),
            entity_type: "concept".to_string(),
            confidence: Some(0.9),
        }
    }

    #[tokio::test]
    async fn test_add_and_get_entity() {
        let graph = test_graph().await;
        let id = graph.add_entity(&person("Alice")).await.unwrap();
        assert!(id > 0);

        let entity = graph.get_entity(id).await.unwrap().unwrap();
        assert_eq!(entity.name, "Alice");
        assert_eq!(entity.entity_type, "person");
        assert!((entity.confidence - 0.8).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_find_by_name() {
        let graph = test_graph().await;
        graph.add_entity(&person("Bob")).await.unwrap();

        // Case-insensitive lookup
        let found = graph.find_entity_by_name("bob").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "Bob");

        let not_found = graph.find_entity_by_name("Charlie").await.unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_search_entities() {
        let graph = test_graph().await;
        graph
            .add_entity(&concept("Rust programming"))
            .await
            .unwrap();
        graph.add_entity(&concept("Rust language")).await.unwrap();
        graph.add_entity(&concept("Python")).await.unwrap();

        let results = graph.search_entities("Rust", 10).await.unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_delete_entity() {
        let graph = test_graph().await;
        let id = graph.add_entity(&person("Delete Me")).await.unwrap();
        assert!(graph.delete_entity(id).await.unwrap());
        assert!(graph.get_entity(id).await.unwrap().is_none());
        assert!(!graph.delete_entity(id).await.unwrap()); // Already deleted
    }

    #[tokio::test]
    async fn test_add_and_get_relations() {
        let graph = test_graph().await;
        let alice = graph.add_entity(&person("Alice")).await.unwrap();
        let rust = graph.add_entity(&concept("Rust")).await.unwrap();

        let rel_id = graph
            .add_relation(&NewRelation {
                from_id: alice,
                to_id: rust,
                relation_type: "knows".to_string(),
                confidence: Some(0.95),
            })
            .await
            .unwrap();
        assert!(rel_id > 0);

        let from_rels = graph.get_relations_from(alice).await.unwrap();
        assert_eq!(from_rels.len(), 1);
        assert_eq!(from_rels[0].relation_type, "knows");

        let to_rels = graph.get_relations_to(rust).await.unwrap();
        assert_eq!(to_rels.len(), 1);
    }

    #[tokio::test]
    async fn test_duplicate_relation_ignored() {
        let graph = test_graph().await;
        let a = graph.add_entity(&person("A")).await.unwrap();
        let b = graph.add_entity(&person("B")).await.unwrap();

        let rel = NewRelation {
            from_id: a,
            to_id: b,
            relation_type: "knows".to_string(),
            confidence: None,
        };
        graph.add_relation(&rel).await.unwrap();
        graph.add_relation(&rel).await.unwrap(); // Duplicate -- should be ignored

        let rels = graph.get_relations_from(a).await.unwrap();
        assert_eq!(rels.len(), 1);
    }

    #[tokio::test]
    async fn test_traverse_depth_1() {
        let graph = test_graph().await;
        let alice = graph.add_entity(&person("Alice")).await.unwrap();
        let bob = graph.add_entity(&person("Bob")).await.unwrap();
        let charlie = graph.add_entity(&person("Charlie")).await.unwrap();

        graph
            .add_relation(&NewRelation {
                from_id: alice,
                to_id: bob,
                relation_type: "knows".to_string(),
                confidence: None,
            })
            .await
            .unwrap();
        graph
            .add_relation(&NewRelation {
                from_id: bob,
                to_id: charlie,
                relation_type: "knows".to_string(),
                confidence: None,
            })
            .await
            .unwrap();

        // Depth 1: only Bob
        let connected = graph.traverse(alice, 1).await.unwrap();
        assert_eq!(connected.len(), 1);
        assert_eq!(connected[0].name, "Bob");

        // Depth 2: Bob + Charlie
        let connected = graph.traverse(alice, 2).await.unwrap();
        assert_eq!(connected.len(), 2);
    }

    #[tokio::test]
    async fn test_traverse_bidirectional() {
        let graph = test_graph().await;
        let a = graph.add_entity(&person("A")).await.unwrap();
        let b = graph.add_entity(&person("B")).await.unwrap();

        // Only A -> B, but traverse should find B from A and A from B
        graph
            .add_relation(&NewRelation {
                from_id: a,
                to_id: b,
                relation_type: "linked".to_string(),
                confidence: None,
            })
            .await
            .unwrap();

        let from_a = graph.traverse(a, 1).await.unwrap();
        assert_eq!(from_a.len(), 1);
        assert_eq!(from_a[0].name, "B");

        let from_b = graph.traverse(b, 1).await.unwrap();
        assert_eq!(from_b.len(), 1);
        assert_eq!(from_b[0].name, "A");
    }

    #[tokio::test]
    async fn test_traverse_no_cycle() {
        let graph = test_graph().await;
        let a = graph.add_entity(&person("A")).await.unwrap();
        let b = graph.add_entity(&person("B")).await.unwrap();

        // A -> B -> A (cycle)
        graph
            .add_relation(&NewRelation {
                from_id: a,
                to_id: b,
                relation_type: "x".to_string(),
                confidence: None,
            })
            .await
            .unwrap();
        graph
            .add_relation(&NewRelation {
                from_id: b,
                to_id: a,
                relation_type: "x".to_string(),
                confidence: None,
            })
            .await
            .unwrap();

        // Should not loop forever
        let connected = graph.traverse(a, 5).await.unwrap();
        assert_eq!(connected.len(), 1); // Only B
    }

    #[tokio::test]
    async fn test_count() {
        let graph = test_graph().await;
        assert_eq!(graph.count_entities().await.unwrap(), 0);
        assert_eq!(graph.count_relations().await.unwrap(), 0);

        let a = graph.add_entity(&person("A")).await.unwrap();
        let b = graph.add_entity(&person("B")).await.unwrap();
        graph
            .add_relation(&NewRelation {
                from_id: a,
                to_id: b,
                relation_type: "x".to_string(),
                confidence: None,
            })
            .await
            .unwrap();

        assert_eq!(graph.count_entities().await.unwrap(), 2);
        assert_eq!(graph.count_relations().await.unwrap(), 1);
    }

    // -- Entity extraction heuristic tests --

    #[test]
    fn test_extract_entity_candidates_capitalized_phrases() {
        let content = "The Ironjaw Gateway runs on Primary Machine.";
        let candidates = KnowledgeGraph::extract_entity_candidates(content);
        assert!(
            candidates.iter().any(|c| c == "Ironjaw Gateway"),
            "Should extract 'Ironjaw Gateway', got: {candidates:?}",
        );
        assert!(
            candidates.iter().any(|c| c == "Primary Machine"),
            "Should extract 'Primary Machine', got: {candidates:?}",
        );
    }

    #[test]
    fn test_extract_entity_candidates_strips_articles() {
        let content = "A Quick Brown Fox jumps over The Lazy Dog.";
        let candidates = KnowledgeGraph::extract_entity_candidates(content);
        // "A Quick Brown Fox" -> "Quick Brown Fox"
        assert!(
            candidates.iter().any(|c| c == "Quick Brown Fox"),
            "Should strip leading 'A', got: {candidates:?}",
        );
        // "The Lazy Dog" -> "Lazy Dog"
        assert!(
            candidates.iter().any(|c| c == "Lazy Dog"),
            "Should strip leading 'The', got: {candidates:?}",
        );
    }

    #[test]
    fn test_extract_entity_candidates_quoted_strings() {
        let content = r#"The memory_type is "session_summary" and the source is "auto-learn"."#;
        let candidates = KnowledgeGraph::extract_entity_candidates(content);
        assert!(
            candidates.iter().any(|c| c == "session_summary"),
            "Should extract 'session_summary', got: {candidates:?}",
        );
        assert!(
            candidates.iter().any(|c| c == "auto-learn"),
            "Should extract 'auto-learn', got: {candidates:?}",
        );
    }

    #[test]
    fn test_extract_entity_candidates_empty() {
        let content = "this is all lowercase with no special names.";
        let candidates = KnowledgeGraph::extract_entity_candidates(content);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_extract_entity_candidates_deduplicates() {
        let content = "Ironjaw Gateway is great. Ironjaw Gateway runs on port 18794.";
        let candidates = KnowledgeGraph::extract_entity_candidates(content);
        let gateway_count = candidates
            .iter()
            .filter(|c| *c == "Ironjaw Gateway")
            .count();
        assert_eq!(gateway_count, 1, "Should deduplicate entity candidates");
    }

    #[tokio::test]
    async fn test_extract_and_link_entities() {
        let graph = test_graph().await;

        let content = "The Ironjaw Gateway communicates with Dr. CLAW via WebSocket.";
        let linked = graph.extract_and_link_entities(1, content).await.unwrap();

        // Should have extracted at least "Ironjaw Gateway"
        assert!(!linked.is_empty(), "Should link at least one entity");

        // A memory:1 entity should exist
        let mem_entity = graph.find_entity_by_name("memory:1").await.unwrap();
        assert!(mem_entity.is_some(), "memory:1 entity should exist");
        let mem_id = mem_entity.unwrap().id;

        // Should have outgoing "mentioned_in" relations
        let rels = graph.get_relations_from(mem_id).await.unwrap();
        assert!(
            !rels.is_empty(),
            "memory entity should have outgoing relations"
        );
        assert_eq!(rels[0].relation_type, "mentioned_in");
    }

    #[tokio::test]
    async fn test_extract_and_link_entities_empty_content() {
        let graph = test_graph().await;
        let linked = graph
            .extract_and_link_entities(1, "just lowercase words")
            .await
            .unwrap();
        assert!(linked.is_empty());
    }

    #[tokio::test]
    async fn test_extract_and_link_entities_reuses_existing() {
        let graph = test_graph().await;

        // Pre-create an entity
        let existing_id = graph.add_entity(&concept("Ironjaw Gateway")).await.unwrap();

        let content = "The Ironjaw Gateway is running well.";
        let linked = graph.extract_and_link_entities(1, content).await.unwrap();

        // Should reuse the existing entity (not create a new one)
        assert!(linked.contains(&existing_id));

        // Entity count should be: 1 pre-existing + 1 memory:1 = 2
        let count = graph.count_entities().await.unwrap();
        assert_eq!(count, 2);
    }

    // -- Cross-source entity linking tests --

    #[tokio::test]
    async fn test_extract_and_link_with_source_creates_source_entity() {
        let graph = test_graph().await;

        let content = "The Ironjaw Gateway is great.";
        let linked = graph
            .extract_and_link_entities_with_source(1, content, &SourceType::Brain)
            .await
            .unwrap();

        assert!(!linked.is_empty());

        // Source entity should exist
        let source_entity = graph.find_entity_by_name("source:brain").await.unwrap();
        assert!(source_entity.is_some(), "source:brain entity should exist");

        // Memory entity should have from_source relation
        let mem_entity = graph
            .find_entity_by_name("memory:1")
            .await
            .unwrap()
            .unwrap();
        let rels = graph.get_relations_from(mem_entity.id).await.unwrap();
        let has_source_rel = rels.iter().any(|r| r.relation_type == "from_source");
        assert!(has_source_rel, "memory should have from_source relation");
    }

    #[tokio::test]
    async fn test_cross_source_link_created() {
        let graph = test_graph().await;

        // First memory from Brain source mentions "Ironjaw Gateway"
        let content1 = "The Ironjaw Gateway runs on port 18794.";
        graph
            .extract_and_link_entities_with_source(1, content1, &SourceType::Brain)
            .await
            .unwrap();

        // Second memory from CcChronicle source also mentions "Ironjaw Gateway"
        let content2 = "The Ironjaw Gateway was restarted today.";
        graph
            .extract_and_link_entities_with_source(2, content2, &SourceType::CcChronicle)
            .await
            .unwrap();

        // Should have a cross_source_link relation between memory:2 and memory:1
        let mem2_entity = graph
            .find_entity_by_name("memory:2")
            .await
            .unwrap()
            .unwrap();
        let rels = graph.get_relations_from(mem2_entity.id).await.unwrap();
        let has_cross_link = rels.iter().any(|r| r.relation_type == "cross_source_link");
        assert!(
            has_cross_link,
            "Should create cross_source_link when same entity from different sources, got: {:?}",
            rels.iter().map(|r| &r.relation_type).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_no_cross_source_link_same_source() {
        let graph = test_graph().await;

        // Two memories from same source mentioning same entity
        let content1 = "The Ironjaw Gateway runs on port 18794.";
        graph
            .extract_and_link_entities_with_source(1, content1, &SourceType::Brain)
            .await
            .unwrap();

        let content2 = "The Ironjaw Gateway is fast.";
        graph
            .extract_and_link_entities_with_source(2, content2, &SourceType::Brain)
            .await
            .unwrap();

        // Should NOT have cross_source_link (same source)
        let mem2_entity = graph
            .find_entity_by_name("memory:2")
            .await
            .unwrap()
            .unwrap();
        let rels = graph.get_relations_from(mem2_entity.id).await.unwrap();
        let has_cross_link = rels.iter().any(|r| r.relation_type == "cross_source_link");
        assert!(
            !has_cross_link,
            "Should NOT create cross_source_link for same source"
        );
    }

    #[tokio::test]
    async fn test_recall_by_entity() {
        let graph = test_graph().await;

        // Set up a BrainStore for recall
        let store = BrainStore::open_memory().await.unwrap();

        // Store two memories
        let mem1_id = store
            .add_memory(&crate::types::NewMemory {
                content: "Ironjaw Gateway runs on port 18794.".to_string(),
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

        let mem2_id = store
            .add_memory(&crate::types::NewMemory {
                content: "Ironjaw Gateway was restarted.".to_string(),
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
                source_type: Some("cc_chronicle".to_string()),
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

        // Link entities
        let content1 = "The Ironjaw Gateway runs on port 18794.";
        graph
            .extract_and_link_entities_with_source(mem1_id, content1, &SourceType::Brain)
            .await
            .unwrap();

        let content2 = "The Ironjaw Gateway was restarted.";
        graph
            .extract_and_link_entities_with_source(mem2_id, content2, &SourceType::CcChronicle)
            .await
            .unwrap();

        // Recall by entity
        let recalled = graph
            .recall_by_entity("Ironjaw Gateway", &store, 10)
            .await
            .unwrap();

        assert_eq!(
            recalled.len(),
            2,
            "Should recall memories from both sources"
        );

        let sources: Vec<&str> = recalled.iter().map(|m| m.source_type.as_str()).collect();
        assert!(sources.contains(&"brain"));
        assert!(sources.contains(&"cc_chronicle"));
    }

    #[tokio::test]
    async fn test_recall_by_entity_unknown() {
        let graph = test_graph().await;
        let store = BrainStore::open_memory().await.unwrap();

        let recalled = graph
            .recall_by_entity("NonexistentEntity", &store, 10)
            .await
            .unwrap();

        assert!(recalled.is_empty());
    }

    #[tokio::test]
    async fn test_add_contradiction() {
        let graph = test_graph().await;

        graph.add_contradiction(1, 2).await.unwrap();

        let entity_a = graph
            .find_entity_by_name("memory:1")
            .await
            .unwrap()
            .unwrap();
        let rels = graph.get_relations_from(entity_a.id).await.unwrap();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].relation_type, "contradicts");
    }

    // ── I10: Causal Graph tests ─────────────────────────────

    #[tokio::test]
    async fn test_causal_relation_types() {
        let graph = test_graph().await;

        let a = graph.add_entity(&person("Bug A")).await.unwrap();
        let b = graph.add_entity(&person("Bug B")).await.unwrap();
        let c = graph.add_entity(&person("Fix C")).await.unwrap();

        // A caused_by B
        graph
            .add_causal_relation(a, b, "caused_by", Some(0.9))
            .await
            .unwrap();

        // A leads_to C
        graph
            .add_causal_relation(a, c, "leads_to", Some(0.8))
            .await
            .unwrap();

        // Invalid relation type should fail
        let result = graph.add_causal_relation(a, b, "invalid_type", None).await;
        assert!(result.is_err());

        // Verify causal relations
        let causal = graph.get_causal_relations(a).await.unwrap();
        assert_eq!(causal.len(), 2);
    }

    #[tokio::test]
    async fn test_trace_causes() {
        let graph = test_graph().await;

        // Create chain: A -leads_to-> B -leads_to-> C
        let a = graph.add_entity(&person("Event A")).await.unwrap();
        let b = graph.add_entity(&person("Event B")).await.unwrap();
        let c = graph.add_entity(&person("Event C")).await.unwrap();

        graph
            .add_causal_relation(a, b, "leads_to", Some(0.9))
            .await
            .unwrap();
        graph
            .add_causal_relation(b, c, "leads_to", Some(0.8))
            .await
            .unwrap();

        // Trace causes from A (what A leads to)
        let traced = graph.trace(a, "causes", 3).await.unwrap();
        assert_eq!(traced.len(), 2); // B and C
        assert_eq!(traced[0].0.name, "Event B");
        assert_eq!(traced[0].2, 1); // depth 1
        assert_eq!(traced[1].0.name, "Event C");
        assert_eq!(traced[1].2, 2); // depth 2
    }

    #[tokio::test]
    async fn test_trace_effects() {
        let graph = test_graph().await;

        // A -caused_by-> B (B caused A)
        let a = graph.add_entity(&person("Effect")).await.unwrap();
        let b = graph.add_entity(&person("Root Cause")).await.unwrap();

        graph
            .add_causal_relation(a, b, "caused_by", Some(0.9))
            .await
            .unwrap();

        // Trace effects from A (what caused A)
        let traced = graph.trace(a, "effects", 3).await.unwrap();
        assert_eq!(traced.len(), 1);
        assert_eq!(traced[0].0.name, "Root Cause");
    }

    #[tokio::test]
    async fn test_trace_prevented_by() {
        let graph = test_graph().await;

        let a = graph.add_entity(&person("Bug")).await.unwrap();
        let b = graph.add_entity(&person("Fix")).await.unwrap();

        graph
            .add_causal_relation(a, b, "prevented_by", Some(0.9))
            .await
            .unwrap();

        // Both directions should find the prevented_by relation
        let traced = graph.trace(a, "both", 2).await.unwrap();
        assert_eq!(traced.len(), 1);
        assert_eq!(traced[0].0.name, "Fix");
        assert_eq!(traced[0].1, "prevented_by");
    }

    #[tokio::test]
    async fn test_trace_depth_limit() {
        let graph = test_graph().await;

        // Create chain: A -> B -> C -> D
        let a = graph.add_entity(&person("A")).await.unwrap();
        let b = graph.add_entity(&person("B")).await.unwrap();
        let c = graph.add_entity(&person("C")).await.unwrap();
        let d = graph.add_entity(&person("D")).await.unwrap();

        graph
            .add_causal_relation(a, b, "leads_to", None)
            .await
            .unwrap();
        graph
            .add_causal_relation(b, c, "leads_to", None)
            .await
            .unwrap();
        graph
            .add_causal_relation(c, d, "leads_to", None)
            .await
            .unwrap();

        // Depth 1 should only find B
        let traced = graph.trace(a, "causes", 1).await.unwrap();
        assert_eq!(traced.len(), 1);
        assert_eq!(traced[0].0.name, "B");

        // Depth 2 should find B and C
        let traced = graph.trace(a, "causes", 2).await.unwrap();
        assert_eq!(traced.len(), 2);
    }
}
