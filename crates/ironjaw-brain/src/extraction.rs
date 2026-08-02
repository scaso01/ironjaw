//! Per-message auto-extraction pipeline.
//!
//! After each assistant response, runs async extraction via LLM to decide:
//! ADD new memory / UPDATE existing / DELETE contradicted / NOOP.
//!
//! Runs as `tokio::spawn` — non-blocking, doesn't delay chat response.
//! Rate-limited: max 1 extraction per `debounce_secs` (default 30s).

use crate::brain::Brain;
use crate::learner::WriteGate;
use crate::types::{MemoryType, NewMemory, NewPendingMemory, TrustLevel};
use ironjaw_llm::types::{ChatMessage, ChatRequest, Role};
use ironjaw_llm::LlmRouter;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;
use tracing::{debug, info};

// ── Telemetry counters ───────────────────────────────────────────────

/// Total facts routed to the pending queue across this process lifetime.
static AUTO_LEARN_PENDING_ROUTED: AtomicUsize = AtomicUsize::new(0);

/// Total facts auto-approved (bypassed pending queue) across this process lifetime.
static AUTO_LEARN_AUTO_APPROVED: AtomicUsize = AtomicUsize::new(0);

/// Total facts dropped (below min_confidence or dedup) across this process lifetime.
static AUTO_LEARN_DROPPED: AtomicUsize = AtomicUsize::new(0);

/// Returns total facts routed to the pending queue since process start.
pub fn auto_learn_pending_total() -> usize {
    AUTO_LEARN_PENDING_ROUTED.load(Ordering::Relaxed)
}

/// Returns total facts auto-approved since process start.
pub fn auto_learn_approved_total() -> usize {
    AUTO_LEARN_AUTO_APPROVED.load(Ordering::Relaxed)
}

/// Returns total facts dropped since process start.
pub fn auto_learn_dropped_total() -> usize {
    AUTO_LEARN_DROPPED.load(Ordering::Relaxed)
}

// ── Config ───────────────────────────────────────────────────────────

/// Configuration for the auto-extraction pipeline.
#[derive(Debug, Clone)]
pub struct ExtractionConfig {
    /// Whether extraction is enabled
    pub enabled: bool,
    /// Minimum seconds between extraction runs (debounce rapid messages)
    pub debounce_secs: u64,
    /// Maximum number of extractions per run
    pub max_extractions: usize,
    /// Minimum confidence to store an extracted fact
    pub min_confidence: f32,
    /// Minimum confidence for auto-approving (skip pending queue)
    pub auto_approve_confidence: f32,
}

impl Default for ExtractionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            debounce_secs: 30,
            max_extractions: 5,
            min_confidence: 0.5,
            auto_approve_confidence: 0.9,
        }
    }
}

// ── Action ───────────────────────────────────────────────────────────

/// Action decided by the LLM for each extracted fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractionAction {
    /// Store as a new memory
    Add,
    /// Merge into an existing memory (target ID)
    Update(i64),
    /// Close validity on an existing memory (contradicted ID)
    Delete(i64),
    /// No action needed
    Noop,
}

/// A single extracted fact with its decided action.
#[derive(Debug, Clone)]
pub struct ExtractedFact {
    pub content: String,
    pub memory_type: MemoryType,
    pub confidence: f32,
    pub action: ExtractionAction,
    pub tags: Vec<String>,
}

// ── Report ───────────────────────────────────────────────────────────

/// Report from a single extraction run.
#[derive(Debug, Clone, Default)]
pub struct ExtractionReport {
    pub added: usize,
    pub updated: usize,
    pub deleted: usize,
    pub skipped: usize,
    pub auto_approved: usize,
}

// ── Pipeline ─────────────────────────────────────────────────────────

/// Per-message extraction pipeline. Owns debounce state.
pub struct ExtractionPipeline {
    config: ExtractionConfig,
    last_run: Arc<Mutex<Option<tokio::time::Instant>>>,
}

impl ExtractionPipeline {
    pub fn new(config: ExtractionConfig) -> Self {
        Self {
            config,
            last_run: Arc::new(Mutex::new(None)),
        }
    }

    /// Check whether we should run extraction (debounce guard).
    ///
    /// Returns `true` if enough time has passed since the last run.
    /// Also updates the last-run timestamp if returning true.
    pub async fn should_run(&self) -> bool {
        if !self.config.enabled {
            return false;
        }

        let mut last = self.last_run.lock().await;
        let now = tokio::time::Instant::now();

        if let Some(prev) = *last {
            if now.duration_since(prev).as_secs() < self.config.debounce_secs {
                debug!(
                    debounce_secs = self.config.debounce_secs,
                    "Extraction debounced"
                );
                return false;
            }
        }

        *last = Some(now);
        true
    }

    /// Run extraction on a user+assistant message pair.
    ///
    /// Sends both messages to the LLM to extract actionable facts, then
    /// applies each action (ADD/UPDATE/DELETE) to the store.
    pub async fn extract(
        &self,
        brain: &Brain,
        llm: &LlmRouter,
        user_message: &str,
        assistant_message: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<ExtractionReport> {
        let store = brain.store();
        let embedder = brain.embedder();
        if !self.should_run().await {
            return Ok(ExtractionReport::default());
        }

        // Build extraction prompt
        let prompt = build_extraction_prompt(user_message, assistant_message);

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

        let response = collect_llm_response(llm, request).await?;
        let facts = parse_extraction_response(&response);

        let mut report = ExtractionReport::default();
        let max = self.config.max_extractions;
        let min_conf = self.config.min_confidence;

        for fact in facts.into_iter().take(max) {
            if fact.confidence < min_conf {
                report.skipped += 1;
                AUTO_LEARN_DROPPED.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            match &fact.action {
                ExtractionAction::Add => {
                    // Dedup check before storing
                    let hash = WriteGate::content_hash(&fact.content);
                    if WriteGate::check_exact_duplicate(store, &hash).await? {
                        report.skipped += 1;
                        AUTO_LEARN_DROPPED.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }

                    let is_correction = fact.memory_type == MemoryType::Correction;

                    // Try to embed once — reuse for both knn lookup and
                    // storage if auto-approved
                    let fact_embedding = if !is_correction
                        && fact.confidence >= self.config.auto_approve_confidence
                    {
                        embedder.embed(&fact.content).await.ok()
                    } else {
                        None
                    };

                    // LLM comparison gate: embed → find nearest → ask LLM
                    let auto_approved = if is_correction {
                        // Corrections always go to pending
                        false
                    } else if let Some(ref emb) = fact_embedding {
                        // High confidence — find nearest existing memory
                        match store.knn_search(emb, 1).await {
                            Ok(results) if !results.is_empty() => {
                                let (nearest_id, similarity) = results[0];
                                if similarity >= 0.7 {
                                    // Topically related — ask LLM to compare
                                    if let Ok(Some(existing)) = store.get_memory(nearest_id).await {
                                        verify_with_llm(llm, &existing.content, &fact.content).await
                                    } else {
                                        false
                                    }
                                } else {
                                    false // Novel topic → pending
                                }
                            }
                            _ => false,
                        }
                    } else {
                        false // Low confidence or correction → pending
                    };

                    let tags_str = if fact.tags.is_empty() {
                        None
                    } else {
                        Some(fact.tags.join(","))
                    };

                    if auto_approved {
                        // Auto-store: LLM confirmed it matches existing knowledge
                        let new_mem = NewMemory {
                            content: fact.content.clone(),
                            source: Some("extraction-pipeline".to_string()),
                            embedding: fact_embedding,
                            tags: tags_str,
                            memory_type: Some(fact.memory_type.to_string()),
                            importance: Some(fact.memory_type.base_importance()),
                            strength: Some(1.0),
                            supersedes: None,
                            content_hash: Some(hash),
                            valid_from: None,
                            valid_to: None,
                            source_type: Some("extraction-auto-approved".to_string()),
                            reflection: None,
                            trust_level: Some(TrustLevel::ModelInferred),
                            evidence_session_id: session_id.map(|s| s.to_string()),
                            evidence_message_id: None,
                            scope: None,
                            procedure: None,
                            quality_score: None,
                            quality_flag: None,
                        };
                        store.add_memory(&new_mem).await?;
                        report.auto_approved += 1;
                        AUTO_LEARN_AUTO_APPROVED.fetch_add(1, Ordering::Relaxed);
                        info!(
                            content = %fact.content,
                            confidence = fact.confidence,
                            "Extraction: auto-approved (LLM confirmed match)"
                        );
                    } else {
                        // Pending queue: novel, contradictory, or low-confidence
                        let pending = NewPendingMemory {
                            content: fact.content.clone(),
                            source: "extraction-pipeline".to_string(),
                            memory_type: fact.memory_type.to_string(),
                            tags: tags_str,
                            importance: fact.memory_type.base_importance(),
                            confidence: f64::from(fact.confidence),
                            evidence_session_id: session_id.map(|s| s.to_string()),
                            embedding: None,
                        };
                        store.add_pending_memory(&pending).await?;
                        report.added += 1;
                        AUTO_LEARN_PENDING_ROUTED.fetch_add(1, Ordering::Relaxed);
                        info!(
                            content = %fact.content,
                            confidence = fact.confidence,
                            is_correction,
                            "Extraction: routed to pending queue"
                        );
                    }
                }
                ExtractionAction::Update(target_id) => {
                    // Route updates through pending queue — direct
                    // mutation from LLM extraction is too dangerous
                    let pending = NewPendingMemory {
                        content: format!("[UPDATE memory {}] {}", target_id, fact.content),
                        source: "extraction-pipeline".to_string(),
                        memory_type: "correction".to_string(),
                        tags: Some("extraction,update".to_string()),
                        importance: fact.memory_type.base_importance(),
                        confidence: f64::from(fact.confidence),
                        evidence_session_id: session_id.map(|s| s.to_string()),
                        embedding: None,
                    };
                    store.add_pending_memory(&pending).await?;
                    report.updated += 1;
                    AUTO_LEARN_PENDING_ROUTED.fetch_add(1, Ordering::Relaxed);
                    info!(
                        target_id,
                        content = %fact.content,
                        "Extraction: UPDATE routed to pending queue"
                    );
                }
                ExtractionAction::Delete(target_id) => {
                    // Route deletes through pending queue — a hallucinated
                    // delete could destroy a good memory
                    let pending = NewPendingMemory {
                        content: format!(
                            "[DELETE memory {}] Contradicted by: {}",
                            target_id, fact.content
                        ),
                        source: "extraction-pipeline".to_string(),
                        memory_type: "correction".to_string(),
                        tags: Some("extraction,delete".to_string()),
                        importance: 0.8,
                        confidence: f64::from(fact.confidence),
                        evidence_session_id: session_id.map(|s| s.to_string()),
                        embedding: None,
                    };
                    store.add_pending_memory(&pending).await?;
                    report.deleted += 1;
                    AUTO_LEARN_PENDING_ROUTED.fetch_add(1, Ordering::Relaxed);
                    info!(target_id, "Extraction: DELETE routed to pending queue");
                }
                ExtractionAction::Noop => {
                    report.skipped += 1;
                    AUTO_LEARN_DROPPED.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        if report.added > 0 || report.updated > 0 || report.deleted > 0 {
            info!(?report, "Extraction pipeline completed");
        }

        Ok(report)
    }
}

// ── Prompt ───────────────────────────────────────────────────────────

/// Build the LLM prompt for per-message extraction.
fn build_extraction_prompt(user_message: &str, assistant_message: &str) -> String {
    format!(
        "Analyze this conversation exchange and extract facts worth remembering long-term.\n\
         \n\
         CRITICAL: Extract SPECIFIC values — config flags, version numbers, port numbers, \
         file paths, command-line arguments, concrete decisions with parameters. \
         Do NOT extract vague intent like 'decided to configure X' — extract \
         'X configured with flag=value'.\n\
         \n\
         REJECT these (respond NOOP):\n\
         - Task descriptions or to-do items (\"need to investigate X\")\n\
         - Vague intent without concrete values (\"updated the configuration\")\n\
         - Greetings, acknowledgments, filler\n\
         - Generic knowledge findable via search\n\
         - Anything recoverable from git log or code inspection\n\
         - Ephemeral runtime metrics (GPU %, throughput tok/s, queue depth, \
         latency ms, memory usage, CPU %, cache hit ratios)\n\
         - Infrastructure facts already well-known (model name, port, endpoint, \
         host identity, provider name) — only extract if CHANGED\n\
         - Search queries or tool invocations (what was searched, not what was found)\n\
         - Single-line fragments without actionable context\n\
         - Hardware specs from shopping/research (prices, financing, model numbers) \
         unless a purchase DECISION was made\n\
         - Past-tense operational status snapshots (\"X validated\", \"Y deployed\", \
         \"Z transitioned to GREEN\", \"command executed\", \"healthcheck verified\") — \
         these are events, not durable facts\n\
         - Diagnostic command logs (\"Executed: docker ps ...\", \"Ran: curl ...\") — \
         the command isn't a memory; only its outcome (if structural) might be\n\
         - Filter/query scope (\"search criteria set to: ...\", \"filtered by Role/Salary/...\", \
         \"scope=Live\") — these are ephemeral queries, not config\n\
         \n\
         TIME TEST: Before extracting, ask \"Will this still be true in 6 months?\"\n\
         - \"Container restarted after config change\" → NO (event)\n\
         - \"All dashboards transitioned from RED to GREEN\" → NO (status snapshot)\n\
         - \"Healthcheck tests validated for all running containers\" → NO (past-tense ops)\n\
         - \"Ironjaw gateway listens on port 18794\" → YES (durable config)\n\
         - \"AdGuard Home is primary DNS resolver for Meshnet (192.0.2.20:53, DoH enabled)\" → YES (architectural decision)\n\
         - \"llama-server uses --cache-type-k q8_0 for 35B models\" → YES (durable parameter)\n\
         If the answer is NO or \"depends on what's running right now\", respond NOOP.\n\
         \n\
         EXAMPLES:\n\
         BAD: \"Decided to configure llama-server parallel processing\"\n\
         GOOD: \"llama-server configured with --parallel 4 --ctx-size 131072 \
         --cache-type-k q8_0 --cache-type-v q8_0\"\n\
         \n\
         BAD: \"Updated the port configuration\"\n\
         GOOD: \"Ironjaw gateway moved from port 18789 to port 18794\"\n\
         \n\
         BAD: \"Initiate research on llama-server replacement\"\n\
         GOOD: (NOOP — this is a task, not a fact worth remembering)\n\
         \n\
         BAD: \"All dashboards transitioned from RED to GREEN status\"\n\
         GOOD: (NOOP — status snapshot, not durable)\n\
         \n\
         BAD: \"Executed diagnostic command on deploy@192.0.2.20: docker ps --filter ...\"\n\
         GOOD: (NOOP — diagnostic command log)\n\
         \n\
         For each fact, decide the action:\n\
         - ADD: new fact not already known\n\
         - UPDATE <id>: fact updates existing memory (provide the memory ID)\n\
         - DELETE <id>: fact contradicts existing memory (provide the memory ID to retire)\n\
         - NOOP: nothing worth extracting\n\
         \n\
         Respond with ONLY a JSON array. Each element:\n\
         {{\"content\": \"...\", \"type\": \"fact|preference|correction|event|decision|observation\", \
         \"confidence\": 0.0-1.0, \"action\": \"ADD|UPDATE|DELETE|NOOP\", \
         \"target_id\": null or number, \"tags\": [\"...\"]}}\n\
         If nothing worth extracting, respond with []\n\
         \n\
         [User]: {user_message}\n\
         [Assistant]: {assistant_message}"
    )
}

// ── Parsing ──────────────────────────────────────────────────────────

/// Parse the LLM response into extracted facts.
fn parse_extraction_response(response: &str) -> Vec<ExtractedFact> {
    let trimmed = response.trim();
    let start = match trimmed.find('[') {
        Some(s) => s,
        None => return Vec::new(),
    };
    let end = match trimmed.rfind(']') {
        Some(e) if e > start => e,
        _ => return Vec::new(),
    };

    let json_str = &trimmed[start..=end];
    let parsed: Result<Vec<serde_json::Value>, _> = serde_json::from_str(json_str);

    match parsed {
        Ok(items) => items
            .into_iter()
            .filter_map(|item| {
                let content = item.get("content")?.as_str()?.to_string();
                if content.is_empty() {
                    return None;
                }

                let memory_type = item
                    .get("type")
                    .and_then(|t| t.as_str())
                    .and_then(|t| t.parse::<MemoryType>().ok())
                    .unwrap_or(MemoryType::Fact);

                let confidence = item
                    .get("confidence")
                    .and_then(|c| c.as_f64())
                    .unwrap_or(0.5) as f32;

                let action_str = item.get("action").and_then(|a| a.as_str()).unwrap_or("ADD");

                let target_id = item.get("target_id").and_then(|t| t.as_i64());

                let action = match action_str.to_uppercase().as_str() {
                    "UPDATE" => target_id
                        .map(ExtractionAction::Update)
                        .unwrap_or(ExtractionAction::Add),
                    "DELETE" => target_id
                        .map(ExtractionAction::Delete)
                        .unwrap_or(ExtractionAction::Noop),
                    "NOOP" => ExtractionAction::Noop,
                    _ => ExtractionAction::Add,
                };

                let tags = item
                    .get("tags")
                    .and_then(|t| t.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();

                Some(ExtractedFact {
                    content,
                    memory_type,
                    confidence,
                    action,
                    tags,
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Collect a streaming LLM response into a single string.
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

// ── LLM Verification ────────────────────────────────────────────────

/// Ask the LLM whether a new extraction confirms or contradicts an existing memory.
/// Returns `true` only if the LLM says CONFIRM.
async fn verify_with_llm(llm: &LlmRouter, existing_content: &str, new_content: &str) -> bool {
    let prompt = format!(
        "You are a fact verification system. Compare these two statements and respond with \
         exactly one word: CONFIRM, CONTRADICT, or NOVEL.\n\n\
         CONFIRM: The new statement says essentially the same thing as the existing one.\n\
         CONTRADICT: The new statement conflicts with or corrects the existing one.\n\
         NOVEL: The new statement adds new information not covered by the existing one.\n\n\
         Existing memory: \"{existing_content}\"\n\n\
         New extraction: \"{new_content}\"\n\n\
         Your verdict (one word):"
    );

    let request = ChatRequest {
        model: String::new(),
        messages: vec![ChatMessage {
            role: Role::User,
            content: prompt,
            images: None,
            ..Default::default()
        }],
        temperature: Some(0.0),
        max_tokens: Some(10),
        extra: None,
        thinking: None,
        tools: None,
        tool_choice: None,
    };

    match collect_llm_response(llm, request).await {
        Ok(response) => {
            let trimmed = response.trim().to_uppercase();
            // Only auto-approve on explicit CONFIRM
            trimmed.starts_with("CONFIRM")
        }
        Err(_) => false, // LLM error → safe fallback to pending
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::BrainStore;

    #[test]
    fn test_parse_extraction_response_valid() {
        let response = r#"[
            {"content": "User prefers Rust", "type": "preference", "confidence": 0.9, "action": "ADD", "target_id": null, "tags": ["language"]},
            {"content": "Project uses tokio", "type": "fact", "confidence": 0.8, "action": "ADD", "target_id": null, "tags": ["tech"]}
        ]"#;

        let facts = parse_extraction_response(response);
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].content, "User prefers Rust");
        assert_eq!(facts[0].memory_type, MemoryType::Preference);
        assert!((facts[0].confidence - 0.9).abs() < 0.01);
        assert_eq!(facts[0].action, ExtractionAction::Add);
        assert_eq!(facts[0].tags, vec!["language".to_string()]);
    }

    #[test]
    fn test_parse_extraction_response_empty() {
        let response = "[]";
        let facts = parse_extraction_response(response);
        assert!(facts.is_empty());
    }

    #[test]
    fn test_parse_extraction_response_with_preamble() {
        let response = "Here are the facts:\n[{\"content\": \"test fact\", \"type\": \"fact\", \"confidence\": 0.7, \"action\": \"ADD\", \"target_id\": null, \"tags\": []}]";
        let facts = parse_extraction_response(response);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "test fact");
    }

    #[test]
    fn test_parse_extraction_response_update_action() {
        let response = r#"[{"content": "updated name", "type": "fact", "confidence": 0.9, "action": "UPDATE", "target_id": 42, "tags": []}]"#;
        let facts = parse_extraction_response(response);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].action, ExtractionAction::Update(42));
    }

    #[test]
    fn test_parse_extraction_response_delete_action() {
        let response = r#"[{"content": "contradicts old", "type": "correction", "confidence": 0.95, "action": "DELETE", "target_id": 7, "tags": []}]"#;
        let facts = parse_extraction_response(response);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].action, ExtractionAction::Delete(7));
    }

    #[test]
    fn test_parse_extraction_response_noop() {
        let response = r#"[{"content": "nothing new", "type": "fact", "confidence": 0.1, "action": "NOOP", "target_id": null, "tags": []}]"#;
        let facts = parse_extraction_response(response);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].action, ExtractionAction::Noop);
    }

    #[test]
    fn test_parse_extraction_response_missing_target_fallback() {
        // UPDATE without target_id falls back to ADD
        let response = r#"[{"content": "no target", "type": "fact", "confidence": 0.5, "action": "UPDATE", "target_id": null, "tags": []}]"#;
        let facts = parse_extraction_response(response);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].action, ExtractionAction::Add);
    }

    #[test]
    fn test_parse_extraction_response_invalid_json() {
        let response = "not json at all";
        let facts = parse_extraction_response(response);
        assert!(facts.is_empty());
    }

    #[test]
    fn test_parse_extraction_response_unknown_type_defaults_to_fact() {
        let response =
            r#"[{"content": "foo", "type": "unknown_type", "confidence": 0.5, "action": "ADD"}]"#;
        let facts = parse_extraction_response(response);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].memory_type, MemoryType::Fact);
    }

    #[test]
    fn test_parse_extraction_response_empty_content_skipped() {
        let response =
            r#"[{"content": "", "type": "fact", "confidence": 0.5, "action": "ADD", "tags": []}]"#;
        let facts = parse_extraction_response(response);
        assert!(facts.is_empty());
    }

    #[test]
    fn test_build_extraction_prompt_contains_messages() {
        let prompt = build_extraction_prompt("hello", "hi there");
        assert!(prompt.contains("[User]: hello"));
        assert!(prompt.contains("[Assistant]: hi there"));
        assert!(prompt.contains("ADD"));
        assert!(prompt.contains("UPDATE"));
        assert!(prompt.contains("DELETE"));
        assert!(prompt.contains("NOOP"));
        // Specificity guidance
        assert!(prompt.contains("SPECIFIC values"));
        assert!(prompt.contains("REJECT"));
        assert!(prompt.contains("EXAMPLES"));
        assert!(prompt.contains("BAD:"));
        assert!(prompt.contains("GOOD:"));
    }

    #[test]
    fn test_default_config() {
        let config = ExtractionConfig::default();
        assert!(config.enabled); // D2: default flipped to true
        assert_eq!(config.debounce_secs, 30);
        assert_eq!(config.max_extractions, 5);
        assert!((config.min_confidence - 0.5).abs() < 0.01);
        assert!((config.auto_approve_confidence - 0.9).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_debounce_first_call_runs() {
        let pipeline = ExtractionPipeline::new(ExtractionConfig {
            enabled: true,
            debounce_secs: 30,
            ..Default::default()
        });
        assert!(pipeline.should_run().await);
    }

    #[tokio::test]
    async fn test_debounce_second_call_blocked() {
        let pipeline = ExtractionPipeline::new(ExtractionConfig {
            enabled: true,
            debounce_secs: 30,
            ..Default::default()
        });
        assert!(pipeline.should_run().await);
        // Immediately calling again should be debounced
        assert!(!pipeline.should_run().await);
    }

    #[tokio::test]
    async fn test_debounce_disabled() {
        let pipeline = ExtractionPipeline::new(ExtractionConfig {
            enabled: false,
            debounce_secs: 0,
            ..Default::default()
        });
        assert!(!pipeline.should_run().await);
    }

    #[tokio::test]
    async fn test_debounce_zero_seconds_always_runs() {
        let pipeline = ExtractionPipeline::new(ExtractionConfig {
            enabled: true,
            debounce_secs: 0,
            ..Default::default()
        });
        assert!(pipeline.should_run().await);
        assert!(pipeline.should_run().await);
    }

    #[test]
    fn test_extraction_report_default() {
        let report = ExtractionReport::default();
        assert_eq!(report.added, 0);
        assert_eq!(report.updated, 0);
        assert_eq!(report.deleted, 0);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.auto_approved, 0);
    }

    #[tokio::test]
    async fn test_extract_stores_pending_memory() {
        let store = BrainStore::open_memory().await.unwrap();
        let _pipeline = ExtractionPipeline::new(ExtractionConfig {
            enabled: true,
            debounce_secs: 0,
            ..Default::default()
        });

        // Simulate what extract() does for an ADD action — now routes to pending queue
        let pending = NewPendingMemory {
            content: "test fact from extraction".to_string(),
            source: "extraction-pipeline".to_string(),
            memory_type: "fact".to_string(),
            tags: Some("test".to_string()),
            importance: 0.5,
            confidence: 0.8,
            evidence_session_id: None,
            embedding: None,
        };
        let id = store.add_pending_memory(&pending).await.unwrap();
        assert!(id > 0);

        // Verify it landed in pending queue
        let pending_list = store.list_pending_memories(10, 0).await.unwrap();
        assert_eq!(pending_list.len(), 1);
        assert_eq!(pending_list[0].content, "test fact from extraction");
        assert_eq!(pending_list[0].source, "extraction-pipeline");
    }

    #[test]
    fn test_parse_multiple_mixed_actions() {
        let response = r#"[
            {"content": "new fact", "type": "fact", "confidence": 0.8, "action": "ADD", "target_id": null, "tags": ["a"]},
            {"content": "updated", "type": "preference", "confidence": 0.9, "action": "UPDATE", "target_id": 5, "tags": []},
            {"content": "old is wrong", "type": "correction", "confidence": 0.95, "action": "DELETE", "target_id": 3, "tags": []},
            {"content": "boring", "type": "fact", "confidence": 0.2, "action": "NOOP", "target_id": null, "tags": []}
        ]"#;

        let facts = parse_extraction_response(response);
        assert_eq!(facts.len(), 4);
        assert_eq!(facts[0].action, ExtractionAction::Add);
        assert_eq!(facts[1].action, ExtractionAction::Update(5));
        assert_eq!(facts[2].action, ExtractionAction::Delete(3));
        assert_eq!(facts[3].action, ExtractionAction::Noop);
    }
}
