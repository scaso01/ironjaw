//! Block-aware session compaction and memory flush.
//!
//! Manages session history when context grows too large, using a three-tier
//! approach: recent messages are kept verbatim, mid-range messages are
//! summarized, and old messages are discarded (after extracting key facts).
//!
//! Tool-use / tool-result pairs are treated as atomic blocks and never split.

use crate::types::SessionMessage;
use ironjaw_llm::router::LlmRouter;
use ironjaw_llm::types::{ChatMessage, ChatRequest, Role};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use tokio_stream::StreamExt;

// ---------------------------------------------------------------------------
// Auto-compact circuit breaker
// ---------------------------------------------------------------------------

/// Circuit breaker for auto-compaction. CC burned ~250K API calls/day
/// from a bug where autocompaction retried endlessly. This prevents that.
pub struct AutoCompactBreaker {
    consecutive_failures: AtomicU32,
    max_failures: u32,
}

impl AutoCompactBreaker {
    pub fn new(max_failures: u32) -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            max_failures,
        }
    }

    pub fn should_attempt(&self) -> bool {
        self.consecutive_failures.load(Ordering::Relaxed) < self.max_failures
    }

    pub fn record_failure(&self) {
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    pub fn failure_count(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Controls how session history is compacted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Number of most-recent messages to keep verbatim.
    pub recent_count: usize,
    /// Number of mid-range messages to summarize.
    pub mid_count: usize,
    /// Token threshold that triggers compaction.
    pub token_limit: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            recent_count: 10,
            mid_count: 20,
            token_limit: 32_768,
        }
    }
}

/// Controls memory extraction from discarded messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFlushConfig {
    /// Whether memory flush is enabled.
    pub enabled: bool,
    /// Maximum number of facts to extract per compaction.
    pub max_facts: usize,
    /// Whether to use LLM-powered extraction (falls back to keyword if unavailable).
    #[serde(default = "default_use_llm")]
    pub use_llm: bool,
    /// Timeout in seconds for LLM extraction requests.
    #[serde(default = "default_llm_timeout")]
    pub llm_timeout_secs: u64,
}

fn default_use_llm() -> bool {
    true
}

fn default_llm_timeout() -> u64 {
    // FIX #5: 60s, not 10. The old 10s was cloud-tuned; a local model doing memory
    // extraction over a large discarded history routinely needs longer, and a
    // premature timeout silently loses every fact from that compaction.
    60
}

impl Default for MemoryFlushConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_facts: 5,
            use_llm: true,
            llm_timeout_secs: 60,
        }
    }
}

// ---------------------------------------------------------------------------
// Flush metrics
// ---------------------------------------------------------------------------

/// Tracks memory flush extraction metrics.
#[derive(Debug, Default)]
pub struct FlushMetrics {
    pub extractions_attempted: AtomicU64,
    pub extractions_succeeded: AtomicU64,
    pub facts_extracted_count: AtomicU64,
    pub llm_fallback_count: AtomicU64,
}

impl FlushMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> FlushMetricsSnapshot {
        FlushMetricsSnapshot {
            extractions_attempted: self.extractions_attempted.load(Ordering::Relaxed),
            extractions_succeeded: self.extractions_succeeded.load(Ordering::Relaxed),
            facts_extracted_count: self.facts_extracted_count.load(Ordering::Relaxed),
            llm_fallback_count: self.llm_fallback_count.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time snapshot of flush metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlushMetricsSnapshot {
    pub extractions_attempted: u64,
    pub extractions_succeeded: u64,
    pub facts_extracted_count: u64,
    pub llm_fallback_count: u64,
}

// ---------------------------------------------------------------------------
// Block detection
// ---------------------------------------------------------------------------

/// An atomic block of one or more messages that must not be split.
///
/// A tool-use message and its following tool-result message form a single block.
/// All other messages are standalone blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageBlock {
    /// The messages in this block (1 for normal, 2 for tool-use/tool-result pair).
    pub messages: Vec<SessionMessage>,
}

/// Group a flat message list into atomic blocks.
///
/// A `tool_use` message immediately followed by a `tool` message forms a
/// two-message block. Everything else is a single-message block.
pub fn group_into_blocks(messages: Vec<SessionMessage>) -> Vec<MessageBlock> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role == "tool_use"
            && i + 1 < messages.len()
            && messages[i + 1].role == "tool"
        {
            blocks.push(MessageBlock {
                messages: vec![messages[i].clone(), messages[i + 1].clone()],
            });
            i += 2;
        } else {
            blocks.push(MessageBlock {
                messages: vec![messages[i].clone()],
            });
            i += 1;
        }
    }
    blocks
}

/// Flatten blocks back into a message list.
pub fn flatten_blocks(blocks: Vec<MessageBlock>) -> Vec<SessionMessage> {
    blocks.into_iter().flat_map(|b| b.messages).collect()
}

// ---------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------

/// Result of a compaction operation.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// Messages to keep (recent tier, verbatim).
    pub kept: Vec<SessionMessage>,
    /// Messages selected for summarization (mid tier).
    pub to_summarize: Vec<SessionMessage>,
    /// Messages that will be discarded (old tier).
    pub to_discard: Vec<SessionMessage>,
    /// Summary text (populated after `summarize` is called).
    pub summary: Option<String>,
}

/// Compact a session's message history according to the three-tier strategy.
///
/// The function groups messages into atomic blocks (preserving tool-use/tool-result
/// pairs), then partitions them into three tiers counted by *blocks*:
///
/// - **Recent** (last `config.recent_count` blocks): kept verbatim
/// - **Mid** (next `config.mid_count` blocks before recent): to be summarized
/// - **Old** (everything before mid): to be discarded
///
/// This function does NOT call the LLM. The caller should use the returned
/// `to_summarize` messages with [`summarize`] and the `to_discard` messages
/// with [`flush_memories`] before finalizing.
pub fn compact_history(
    messages: Vec<SessionMessage>,
    config: &CompactionConfig,
) -> CompactionResult {
    let blocks = group_into_blocks(messages);
    let total = blocks.len();

    // Partition blocks into tiers (from the end)
    let recent_start = total.saturating_sub(config.recent_count);
    let mid_start = recent_start.saturating_sub(config.mid_count);

    let old_blocks = &blocks[..mid_start];
    let mid_blocks = &blocks[mid_start..recent_start];
    let recent_blocks = &blocks[recent_start..];

    CompactionResult {
        kept: flatten_blocks(recent_blocks.to_vec()),
        to_summarize: flatten_blocks(mid_blocks.to_vec()),
        to_discard: flatten_blocks(old_blocks.to_vec()),
        summary: None,
    }
}

/// Check whether compaction should be triggered for the given token count.
pub fn should_compact(context_tokens: u64, config: &CompactionConfig) -> bool {
    context_tokens > config.token_limit
}

/// Summarize a set of messages into a single summary string.
///
/// This is an async function signature for future LLM integration.
/// Currently returns a placeholder that concatenates message roles/content.
pub async fn summarize(messages: &[SessionMessage]) -> String {
    if messages.is_empty() {
        return String::new();
    }
    // Placeholder: produce a brief outline of the messages.
    // The real implementation will call the LLM router.
    let mut lines = Vec::with_capacity(messages.len());
    for msg in messages {
        let preview = if msg.content.len() > 80 {
            format!("{}...", &msg.content[..80])
        } else {
            msg.content.clone()
        };
        lines.push(format!("[{}] {preview}", msg.role));
    }
    format!(
        "[Summary of {} messages]\n{}",
        messages.len(),
        lines.join("\n")
    )
}

// ---------------------------------------------------------------------------
// Memory flush
// ---------------------------------------------------------------------------

/// Keywords that signal important facts worth extracting.
const MEMORY_KEYWORDS: &[&str] = &[
    "always",
    "never",
    "prefer",
    "remember",
    "important",
    "decision",
    "agreed",
];

/// Extract key facts from messages about to be discarded.
///
/// Uses a simple heuristic: finds sentences containing keywords that signal
/// preferences, decisions, or important information. The real implementation
/// will use an LLM for better extraction.
pub async fn flush_memories(
    messages: &[SessionMessage],
    config: &MemoryFlushConfig,
) -> Vec<String> {
    if !config.enabled || messages.is_empty() {
        return Vec::new();
    }

    let mut facts: Vec<String> = Vec::new();

    for msg in messages {
        // Split content into sentences (rough heuristic)
        let sentences = split_sentences(&msg.content);
        for sentence in sentences {
            let lower = sentence.to_lowercase();
            if MEMORY_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
                let trimmed = sentence.trim().to_string();
                if !trimmed.is_empty() && !facts.contains(&trimmed) {
                    facts.push(trimmed);
                    if facts.len() >= config.max_facts {
                        return facts;
                    }
                }
            }
        }
    }

    facts
}

/// Split text into sentences using common delimiters.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();

    for (i, &b) in bytes.iter().enumerate() {
        if b == b'.' || b == b'!' || b == b'?' || b == b'\n' {
            let candidate = &text[start..=i];
            let trimmed = candidate.trim();
            if !trimmed.is_empty() {
                sentences.push(trimmed);
            }
            start = i + 1;
        }
    }

    // Trailing text without terminator
    if start < text.len() {
        let trimmed = text[start..].trim();
        if !trimmed.is_empty() {
            sentences.push(trimmed);
        }
    }

    sentences
}

// ---------------------------------------------------------------------------
// Flush candidate extraction
// ---------------------------------------------------------------------------

/// Extract `(role, content)` pairs from session messages for brain learning.
///
/// Filters to only `user` and `assistant` roles (skipping tool_use, tool,
/// system messages) since those are the conversational content worth
/// summarizing or learning from.
///
/// Called from `handler.rs` before compaction discards messages, to feed
/// into `generate_session_summary()` or `Learner::learn()`.
pub fn extract_flush_candidates(messages: &[SessionMessage]) -> Vec<(String, String)> {
    messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .map(|m| (m.role.clone(), m.content.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// LLM-powered extraction
// ---------------------------------------------------------------------------

/// A structured memory candidate extracted by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryCandidate {
    pub content: String,
    pub memory_type: MemoryType,
    pub importance: f64,
}

/// Classification of extracted memory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MemoryType {
    Fact,
    Preference,
    Decision,
    Correction,
    Event,
    Observation,
}

impl std::fmt::Display for MemoryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryType::Fact => write!(f, "Fact"),
            MemoryType::Preference => write!(f, "Preference"),
            MemoryType::Decision => write!(f, "Decision"),
            MemoryType::Correction => write!(f, "Correction"),
            MemoryType::Event => write!(f, "Event"),
            MemoryType::Observation => write!(f, "Observation"),
        }
    }
}

/// FIX #5: upper bound on messages fed into LLM memory extraction. The tail of a
/// discarded history holds the freshest facts; feeding the whole thing to a local
/// model blows the extraction timeout and loses every fact from that compaction.
const MAX_EXTRACTION_MESSAGES: usize = 40;

/// Keep only the most recent `n` messages (tail slice) for memory extraction.
fn cap_recent_messages(messages: &[SessionMessage], n: usize) -> &[SessionMessage] {
    if messages.len() > n {
        &messages[messages.len() - n..]
    } else {
        messages
    }
}

/// Build the LLM prompt for memory extraction.
fn build_extraction_prompt(messages: &[SessionMessage], max_facts: usize) -> Vec<ChatMessage> {
    let mut conversation = String::new();
    for msg in messages {
        if msg.role == "user" || msg.role == "assistant" {
            conversation.push_str(&format!("[{}]: {}\n", msg.role, msg.content));
        }
    }

    vec![
        ChatMessage {
            role: Role::System,
            content: format!(
                "You are a memory extraction assistant. Extract key facts, decisions, user preferences, \
                 and corrections from the following conversation messages that are about to be discarded. \
                 Return ONLY a JSON array (no markdown, no explanation) of up to {max_facts} objects with fields: \
                 \"content\" (string — the extracted fact), \
                 \"memory_type\" (string — one of: Fact, Preference, Decision, Correction, Event, Observation), \
                 \"importance\" (float 0.0 to 1.0). \
                 Focus on information that would be valuable to remember for future conversations. \
                 Skip trivial greetings and transient task details."
            ),
            images: None,
            ..Default::default()
        },
        ChatMessage {
            role: Role::User,
            content: conversation,
            images: None,
            ..Default::default()
        },
    ]
}

/// Parse the LLM response into memory candidates, using json_repair for robustness.
fn parse_extraction_response(response: &str, max_facts: usize) -> Vec<MemoryCandidate> {
    // Try to extract JSON array from response (handles surrounding text, code fences, etc.)
    let json_str = ironjaw_llm::json_repair::extract_json(response)
        .or_else(|| ironjaw_llm::json_repair::repair_json(response).ok())
        .unwrap_or_default();

    if json_str.is_empty() {
        tracing::warn!("LLM extraction: no valid JSON found in response");
        return Vec::new();
    }

    // Parse as array of raw values first for resilience
    let array: Vec<serde_json::Value> = match serde_json::from_str(&json_str) {
        Ok(arr) => arr,
        Err(e) => {
            tracing::warn!(error = %e, "LLM extraction: failed to parse JSON array");
            return Vec::new();
        }
    };

    let mut candidates = Vec::new();
    for item in array {
        let content = item
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if content.is_empty() {
            continue;
        }

        let memory_type = match item
            .get("memory_type")
            .and_then(|v| v.as_str())
            .unwrap_or("Fact")
        {
            "Preference" => MemoryType::Preference,
            "Decision" => MemoryType::Decision,
            "Correction" => MemoryType::Correction,
            "Event" => MemoryType::Event,
            "Observation" => MemoryType::Observation,
            _ => MemoryType::Fact,
        };

        let importance = item
            .get("importance")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.5)
            .clamp(0.0, 1.0);

        candidates.push(MemoryCandidate {
            content,
            memory_type,
            importance,
        });

        if candidates.len() >= max_facts {
            break;
        }
    }

    candidates
}

/// Extract memory candidates from messages using the LLM.
///
/// Sends discarded messages to the LLM with a structured prompt and parses
/// the JSON response. Returns an empty vec on any failure (caller should
/// fall back to keyword extraction).
pub async fn extract_flush_candidates_llm(
    messages: &[SessionMessage],
    llm: &LlmRouter,
    config: &MemoryFlushConfig,
) -> Result<Vec<MemoryCandidate>, String> {
    if messages.is_empty() {
        return Ok(Vec::new());
    }

    if !llm.has_providers() {
        return Err("no LLM providers available".to_string());
    }

    // FIX #5: cap extraction input to the most recent messages. On a local model
    // a large discarded-history prefill blows the extraction timeout (losing every
    // fact); the tail carries the freshest, most memorable content. This is the
    // memory-flush path only — it does not affect live conversation context.
    let recent = cap_recent_messages(messages, MAX_EXTRACTION_MESSAGES);
    let prompt_messages = build_extraction_prompt(recent, config.max_facts);
    let request = ChatRequest {
        model: String::new(), // use default/routed model
        messages: prompt_messages,
        temperature: Some(0.1), // low temperature for structured extraction
        max_tokens: Some(1024),
        extra: None,
        thinking: None,
        tools: None,
        tool_choice: None,
    };

    let timeout = std::time::Duration::from_secs(config.llm_timeout_secs);

    // Send request and collect the full streamed response
    let stream = tokio::time::timeout(timeout, llm.chat(request))
        .await
        .map_err(|_| "LLM extraction timed out waiting for stream".to_string())?
        .map_err(|e| format!("LLM chat error: {e}"))?;

    let mut stream = stream;
    let mut full_response = String::new();

    let collect_timeout = std::time::Duration::from_secs(config.llm_timeout_secs);
    let collect_result = tokio::time::timeout(collect_timeout, async {
        while let Some(chunk) = stream.next().await {
            full_response.push_str(&chunk.delta);
            if let Some(err) = &chunk.error {
                return Err(format!("LLM stream error: {err}"));
            }
            if chunk.done {
                break;
            }
        }
        Ok(())
    })
    .await;

    match collect_result {
        Err(_) => return Err("LLM extraction timed out collecting response".to_string()),
        Ok(Err(e)) => return Err(e),
        Ok(Ok(())) => {}
    }

    if full_response.is_empty() {
        return Err("LLM returned empty response".to_string());
    }

    let candidates = parse_extraction_response(&full_response, config.max_facts);
    if candidates.is_empty() {
        return Err("LLM response contained no valid candidates".to_string());
    }

    Ok(candidates)
}

/// LLM-powered memory flush with automatic fallback to keyword extraction.
///
/// If `config.use_llm` is true and an LLM router is provided, attempts
/// LLM-based extraction first. On any failure, falls back to keyword-based
/// extraction. Tracks metrics for both paths.
pub async fn flush_memories_llm(
    messages: &[SessionMessage],
    config: &MemoryFlushConfig,
    llm: Option<&LlmRouter>,
    metrics: Option<&FlushMetrics>,
) -> Vec<MemoryCandidate> {
    if !config.enabled || messages.is_empty() {
        return Vec::new();
    }

    if let Some(m) = metrics {
        m.extractions_attempted.fetch_add(1, Ordering::Relaxed);
    }

    // Try LLM extraction if configured and available
    if config.use_llm {
        if let Some(llm) = llm {
            match extract_flush_candidates_llm(messages, llm, config).await {
                Ok(candidates) => {
                    tracing::info!(
                        count = candidates.len(),
                        method = "llm",
                        "Memory flush extracted facts"
                    );
                    if let Some(m) = metrics {
                        m.extractions_succeeded.fetch_add(1, Ordering::Relaxed);
                        m.facts_extracted_count
                            .fetch_add(candidates.len() as u64, Ordering::Relaxed);
                    }
                    return candidates;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "LLM extraction failed, falling back to keyword extraction"
                    );
                    if let Some(m) = metrics {
                        m.llm_fallback_count.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        } else {
            tracing::debug!("No LLM router provided, using keyword extraction");
            if let Some(m) = metrics {
                m.llm_fallback_count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // Fallback: keyword-based extraction
    let facts = flush_memories(messages, config).await;
    let candidates: Vec<MemoryCandidate> = facts
        .into_iter()
        .map(|content| MemoryCandidate {
            content,
            memory_type: MemoryType::Fact,
            importance: 0.5,
        })
        .collect();

    tracing::info!(
        count = candidates.len(),
        method = "keyword",
        "Memory flush extracted facts (fallback)"
    );

    if let Some(m) = metrics {
        if !candidates.is_empty() {
            m.extractions_succeeded.fetch_add(1, Ordering::Relaxed);
        }
        m.facts_extracted_count
            .fetch_add(candidates.len() as u64, Ordering::Relaxed);
    }

    candidates
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a SessionMessage for testing.
    fn msg(id: i64, role: &str, content: &str) -> SessionMessage {
        SessionMessage {
            id,
            session_id: "test-session".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            model: None,
            tokens: None,
            complete: true,
            created_at: "2026-01-01T00:00:00".to_string(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    // -- Block grouping tests --

    #[test]
    fn test_group_normal_messages() {
        let messages = vec![
            msg(1, "user", "hello"),
            msg(2, "assistant", "hi"),
            msg(3, "user", "bye"),
        ];
        let blocks = group_into_blocks(messages);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].messages.len(), 1);
        assert_eq!(blocks[1].messages.len(), 1);
        assert_eq!(blocks[2].messages.len(), 1);
    }

    #[test]
    fn test_group_tool_use_pair() {
        let messages = vec![
            msg(1, "user", "search for X"),
            msg(2, "tool_use", "search"),
            msg(3, "tool", "results here"),
            msg(4, "assistant", "found it"),
        ];
        let blocks = group_into_blocks(messages);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].messages.len(), 1); // user
        assert_eq!(blocks[1].messages.len(), 2); // tool_use + tool
        assert_eq!(blocks[1].messages[0].role, "tool_use");
        assert_eq!(blocks[1].messages[1].role, "tool");
        assert_eq!(blocks[2].messages.len(), 1); // assistant
    }

    #[test]
    fn test_group_tool_use_without_result() {
        let messages = vec![
            msg(1, "tool_use", "call fn"),
            msg(2, "assistant", "no result"),
        ];
        let blocks = group_into_blocks(messages);
        assert_eq!(blocks.len(), 2);
        // tool_use not followed by tool -> standalone block
        assert_eq!(blocks[0].messages.len(), 1);
        assert_eq!(blocks[0].messages[0].role, "tool_use");
    }

    #[test]
    fn test_group_consecutive_tool_pairs() {
        let messages = vec![
            msg(1, "tool_use", "call A"),
            msg(2, "tool", "result A"),
            msg(3, "tool_use", "call B"),
            msg(4, "tool", "result B"),
        ];
        let blocks = group_into_blocks(messages);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].messages.len(), 2);
        assert_eq!(blocks[1].messages.len(), 2);
    }

    #[test]
    fn test_group_empty() {
        let blocks = group_into_blocks(vec![]);
        assert!(blocks.is_empty());
    }

    #[test]
    fn test_flatten_roundtrip() {
        let messages = vec![
            msg(1, "user", "hello"),
            msg(2, "tool_use", "search"),
            msg(3, "tool", "results"),
            msg(4, "assistant", "done"),
        ];
        let blocks = group_into_blocks(messages.clone());
        let flattened = flatten_blocks(blocks);
        assert_eq!(flattened.len(), messages.len());
        for (a, b) in flattened.iter().zip(messages.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.role, b.role);
            assert_eq!(a.content, b.content);
        }
    }

    // -- Compaction tests --

    #[test]
    fn test_compact_all_recent() {
        let messages = vec![msg(1, "user", "hello"), msg(2, "assistant", "hi")];
        let config = CompactionConfig {
            recent_count: 10,
            mid_count: 5,
            token_limit: 1000,
        };
        let result = compact_history(messages, &config);
        assert_eq!(result.kept.len(), 2);
        assert!(result.to_summarize.is_empty());
        assert!(result.to_discard.is_empty());
    }

    #[test]
    fn test_compact_three_tiers() {
        // 10 messages: old(0-4), mid(5-7), recent(8-9)
        let messages: Vec<_> = (0..10)
            .map(|i| {
                msg(
                    i,
                    if i % 2 == 0 { "user" } else { "assistant" },
                    &format!("msg-{i}"),
                )
            })
            .collect();
        let config = CompactionConfig {
            recent_count: 2,
            mid_count: 3,
            token_limit: 1000,
        };
        let result = compact_history(messages, &config);
        assert_eq!(result.kept.len(), 2);
        assert_eq!(result.kept[0].content, "msg-8");
        assert_eq!(result.kept[1].content, "msg-9");
        assert_eq!(result.to_summarize.len(), 3);
        assert_eq!(result.to_summarize[0].content, "msg-5");
        assert_eq!(result.to_discard.len(), 5);
        assert_eq!(result.to_discard[0].content, "msg-0");
    }

    #[test]
    fn test_compact_preserves_tool_blocks() {
        // Messages: user, tool_use+tool (1 block), assistant, user, assistant
        // = 4 blocks, recent_count=2 -> recent=[user, assistant], mid_count=1 -> mid=[assistant], old=[user, tool_use+tool]
        let messages = vec![
            msg(1, "user", "search for X"),
            msg(2, "tool_use", "search"),
            msg(3, "tool", "results"),
            msg(4, "assistant", "found it"),
            msg(5, "user", "thanks"),
            msg(6, "assistant", "welcome"),
        ];
        let config = CompactionConfig {
            recent_count: 2,
            mid_count: 1,
            token_limit: 1000,
        };
        let result = compact_history(messages, &config);
        // 5 blocks: [user], [tool_use+tool], [assistant], [user], [assistant]
        // recent: last 2 blocks = [user(5), assistant(6)]
        // mid: 1 block before = [assistant(4)]
        // old: first 2 blocks = [user(1), tool_use(2)+tool(3)]
        assert_eq!(result.kept.len(), 2);
        assert_eq!(result.kept[0].content, "thanks");
        assert_eq!(result.kept[1].content, "welcome");
        assert_eq!(result.to_summarize.len(), 1);
        assert_eq!(result.to_summarize[0].content, "found it");
        assert_eq!(result.to_discard.len(), 3);
        // tool_use and tool stay together in old tier
        assert_eq!(result.to_discard[0].role, "user");
        assert_eq!(result.to_discard[1].role, "tool_use");
        assert_eq!(result.to_discard[2].role, "tool");
    }

    #[test]
    fn test_compact_no_mid_tier() {
        let messages: Vec<_> = (0..5)
            .map(|i| msg(i, "user", &format!("msg-{i}")))
            .collect();
        let config = CompactionConfig {
            recent_count: 3,
            mid_count: 0,
            token_limit: 1000,
        };
        let result = compact_history(messages, &config);
        assert_eq!(result.kept.len(), 3);
        assert!(result.to_summarize.is_empty());
        assert_eq!(result.to_discard.len(), 2);
    }

    #[test]
    fn test_compact_empty_history() {
        let config = CompactionConfig::default();
        let result = compact_history(vec![], &config);
        assert!(result.kept.is_empty());
        assert!(result.to_summarize.is_empty());
        assert!(result.to_discard.is_empty());
    }

    // -- should_compact tests --

    #[test]
    fn test_should_compact_over_limit() {
        let config = CompactionConfig {
            token_limit: 1000,
            ..Default::default()
        };
        assert!(should_compact(1001, &config));
        assert!(should_compact(5000, &config));
    }

    #[test]
    fn test_should_compact_under_limit() {
        let config = CompactionConfig {
            token_limit: 1000,
            ..Default::default()
        };
        assert!(!should_compact(999, &config));
        assert!(!should_compact(1000, &config));
    }

    // -- summarize tests --

    #[tokio::test]
    async fn test_summarize_empty() {
        let result = summarize(&[]).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_summarize_produces_output() {
        let messages = vec![
            msg(1, "user", "What is Rust?"),
            msg(2, "assistant", "A systems programming language."),
        ];
        let result = summarize(&messages).await;
        assert!(result.contains("[Summary of 2 messages]"));
        assert!(result.contains("[user]"));
        assert!(result.contains("[assistant]"));
    }

    #[tokio::test]
    async fn test_summarize_truncates_long_content() {
        let long_content = "x".repeat(200);
        let messages = vec![msg(1, "user", &long_content)];
        let result = summarize(&messages).await;
        assert!(result.contains("..."));
    }

    // -- Memory flush tests --

    #[tokio::test]
    async fn test_flush_disabled() {
        let config = MemoryFlushConfig {
            enabled: false,
            max_facts: 5,
            ..Default::default()
        };
        let messages = vec![msg(1, "user", "Always use Rust.")];
        let facts = flush_memories(&messages, &config).await;
        assert!(facts.is_empty());
    }

    #[tokio::test]
    async fn test_flush_empty_messages() {
        let config = MemoryFlushConfig::default();
        let facts = flush_memories(&[], &config).await;
        assert!(facts.is_empty());
    }

    #[tokio::test]
    async fn test_flush_extracts_keyword_sentences() {
        let config = MemoryFlushConfig {
            enabled: true,
            max_facts: 10,
            ..Default::default()
        };
        let messages = vec![
            msg(
                1,
                "user",
                "I always use dark mode. The weather is nice today.",
            ),
            msg(
                2,
                "assistant",
                "That's a good preference. Remember to save often.",
            ),
            msg(3, "user", "We agreed to use axum for the server."),
        ];
        let facts = flush_memories(&messages, &config).await;
        assert!(
            facts.len() >= 3,
            "Expected at least 3 facts, got {}",
            facts.len()
        );
        assert!(facts.iter().any(|f| f.contains("always")));
        assert!(facts.iter().any(|f| f.contains("Remember")));
        assert!(facts.iter().any(|f| f.contains("agreed")));
    }

    #[tokio::test]
    async fn test_flush_respects_max_facts() {
        let config = MemoryFlushConfig {
            enabled: true,
            max_facts: 2,
            ..Default::default()
        };
        let messages = vec![msg(
            1,
            "user",
            "Always do X. Never do Y. Remember Z. Important: W. Prefer A.",
        )];
        let facts = flush_memories(&messages, &config).await;
        assert_eq!(facts.len(), 2);
    }

    #[tokio::test]
    async fn test_flush_no_keywords() {
        let config = MemoryFlushConfig::default();
        let messages = vec![
            msg(1, "user", "Hello there."),
            msg(2, "assistant", "Hi, how can I help?"),
        ];
        let facts = flush_memories(&messages, &config).await;
        assert!(facts.is_empty());
    }

    #[tokio::test]
    async fn test_flush_deduplicates() {
        let config = MemoryFlushConfig {
            enabled: true,
            max_facts: 10,
            ..Default::default()
        };
        let messages = vec![
            msg(1, "user", "Always use Rust."),
            msg(2, "user", "Always use Rust."),
        ];
        let facts = flush_memories(&messages, &config).await;
        assert_eq!(facts.len(), 1);
    }

    // -- split_sentences tests --

    #[test]
    fn test_split_sentences_basic() {
        let sentences = split_sentences("Hello world. How are you? I'm fine!");
        assert_eq!(sentences.len(), 3);
        assert_eq!(sentences[0], "Hello world.");
        assert_eq!(sentences[1], "How are you?");
        assert_eq!(sentences[2], "I'm fine!");
    }

    #[test]
    fn test_split_sentences_newlines() {
        let sentences = split_sentences("Line one\nLine two\nLine three");
        assert_eq!(sentences.len(), 3);
    }

    #[test]
    fn test_split_sentences_trailing_text() {
        let sentences = split_sentences("Hello. World without period");
        assert_eq!(sentences.len(), 2);
        assert_eq!(sentences[1], "World without period");
    }

    #[test]
    fn test_split_sentences_empty() {
        let sentences = split_sentences("");
        assert!(sentences.is_empty());
    }

    // -- Config default tests --

    #[test]
    fn test_compaction_config_defaults() {
        let config = CompactionConfig::default();
        assert_eq!(config.recent_count, 10);
        assert_eq!(config.mid_count, 20);
        assert_eq!(config.token_limit, 32_768);
    }

    #[test]
    fn test_memory_flush_config_defaults() {
        let config = MemoryFlushConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_facts, 5);
    }

    // -- Integration-style tests --

    #[tokio::test]
    async fn test_full_compaction_workflow() {
        // Simulate a real workflow: check trigger, compact, flush, summarize
        let config = CompactionConfig {
            recent_count: 2,
            mid_count: 2,
            token_limit: 1000,
        };
        let flush_config = MemoryFlushConfig {
            enabled: true,
            max_facts: 3,
            ..Default::default()
        };

        let messages = vec![
            msg(1, "user", "We decided to always use SQLite for storage."),
            msg(2, "assistant", "Good decision. I'll remember that."),
            msg(3, "user", "Search for axum docs"),
            msg(4, "tool_use", "web_search"),
            msg(5, "tool", "axum 0.8 documentation..."),
            msg(6, "assistant", "Here are the axum docs."),
            msg(7, "user", "Thanks, now implement the handler."),
            msg(8, "assistant", "Done, handler implemented."),
        ];

        // Check trigger
        assert!(should_compact(2000, &config));

        // Compact
        let result = compact_history(messages, &config);

        // Recent: last 2 blocks = [user(7), assistant(8)]
        assert_eq!(result.kept.len(), 2);
        assert_eq!(result.kept[0].content, "Thanks, now implement the handler.");

        // Mid: 2 blocks before = [tool_use+tool block, assistant(6)]
        // Block 3 = [tool_use(4)+tool(5)], Block 4 = [assistant(6)]
        assert_eq!(result.to_summarize.len(), 3); // 2 from tool pair + 1 assistant

        // Old: first 2 blocks = [user(1), assistant(2)]
        // Actually: blocks = [user(1)], [assistant(2)], [user(3)], [tool_use(4)+tool(5)], [assistant(6)], [user(7)], [assistant(8)]
        // 7 blocks total, recent=2 -> blocks 5-6, mid=2 -> blocks 3-4, old=blocks 0-2
        assert_eq!(result.to_discard.len(), 3);

        // Flush memories from discarded
        let facts = flush_memories(&result.to_discard, &flush_config).await;
        assert!(!facts.is_empty(), "Should extract 'always' keyword fact");

        // Summarize mid tier
        let summary = summarize(&result.to_summarize).await;
        assert!(summary.contains("[Summary of"));
    }

    #[test]
    fn test_compact_only_tool_pairs() {
        // All messages are tool pairs
        let messages = vec![
            msg(1, "tool_use", "call A"),
            msg(2, "tool", "result A"),
            msg(3, "tool_use", "call B"),
            msg(4, "tool", "result B"),
            msg(5, "tool_use", "call C"),
            msg(6, "tool", "result C"),
        ];
        let config = CompactionConfig {
            recent_count: 1,
            mid_count: 1,
            token_limit: 1000,
        };
        let result = compact_history(messages, &config);
        // 3 blocks, recent=1 block (2 msgs), mid=1 block (2 msgs), old=1 block (2 msgs)
        assert_eq!(result.kept.len(), 2);
        assert_eq!(result.kept[0].role, "tool_use");
        assert_eq!(result.kept[0].content, "call C");
        assert_eq!(result.kept[1].role, "tool");
        assert_eq!(result.to_summarize.len(), 2);
        assert_eq!(result.to_discard.len(), 2);
    }

    #[test]
    fn test_compact_single_message() {
        let messages = vec![msg(1, "user", "hello")];
        let config = CompactionConfig::default();
        let result = compact_history(messages, &config);
        assert_eq!(result.kept.len(), 1);
        assert!(result.to_summarize.is_empty());
        assert!(result.to_discard.is_empty());
    }

    #[tokio::test]
    async fn test_flush_multiline_content() {
        let config = MemoryFlushConfig {
            enabled: true,
            max_facts: 5,
            ..Default::default()
        };
        let messages = vec![msg(
            1,
            "user",
            "Some normal text\nThis is important: use Rust\nAnother line",
        )];
        let facts = flush_memories(&messages, &config).await;
        assert!(
            facts.iter().any(|f| f.contains("important")),
            "Should extract sentence with 'important' keyword"
        );
    }

    // -- extract_flush_candidates tests --

    #[test]
    fn test_extract_flush_candidates_filters_roles() {
        let messages = vec![
            msg(1, "user", "Hello there"),
            msg(2, "assistant", "Hi, how can I help?"),
            msg(3, "tool_use", "search"),
            msg(4, "tool", "search results"),
            msg(5, "system", "system message"),
            msg(6, "user", "What about Rust?"),
            msg(7, "assistant", "Rust is a systems language."),
        ];
        let candidates = extract_flush_candidates(&messages);
        assert_eq!(candidates.len(), 4);
        assert_eq!(
            candidates[0],
            ("user".to_string(), "Hello there".to_string())
        );
        assert_eq!(
            candidates[1],
            ("assistant".to_string(), "Hi, how can I help?".to_string())
        );
        assert_eq!(
            candidates[2],
            ("user".to_string(), "What about Rust?".to_string())
        );
        assert_eq!(
            candidates[3],
            (
                "assistant".to_string(),
                "Rust is a systems language.".to_string()
            )
        );
    }

    #[test]
    fn test_extract_flush_candidates_empty() {
        let candidates = extract_flush_candidates(&[]);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_extract_flush_candidates_no_user_assistant() {
        let messages = vec![
            msg(1, "tool_use", "call"),
            msg(2, "tool", "result"),
            msg(3, "system", "info"),
        ];
        let candidates = extract_flush_candidates(&messages);
        assert!(candidates.is_empty());
    }

    // -- LLM extraction tests --

    #[test]
    fn test_parse_extraction_response_valid() {
        let response = r#"[
            {"content": "User prefers dark mode", "memory_type": "Preference", "importance": 0.8},
            {"content": "Decided to use SQLite", "memory_type": "Decision", "importance": 0.9}
        ]"#;
        let candidates = parse_extraction_response(response, 5);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].content, "User prefers dark mode");
        assert_eq!(candidates[0].memory_type, MemoryType::Preference);
        assert!((candidates[0].importance - 0.8).abs() < 0.01);
        assert_eq!(candidates[1].content, "Decided to use SQLite");
        assert_eq!(candidates[1].memory_type, MemoryType::Decision);
        assert!((candidates[1].importance - 0.9).abs() < 0.01);
    }

    #[test]
    fn test_parse_extraction_response_with_code_fence() {
        let response = r#"```json
[{"content": "Always use Rust", "memory_type": "Fact", "importance": 0.7}]
```"#;
        let candidates = parse_extraction_response(response, 5);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].content, "Always use Rust");
    }

    #[test]
    fn test_parse_extraction_response_respects_max_facts() {
        let response = r#"[
            {"content": "Fact 1", "memory_type": "Fact", "importance": 0.5},
            {"content": "Fact 2", "memory_type": "Fact", "importance": 0.5},
            {"content": "Fact 3", "memory_type": "Fact", "importance": 0.5}
        ]"#;
        let candidates = parse_extraction_response(response, 2);
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn test_parse_extraction_response_invalid_json() {
        let response = "This is not JSON at all";
        let candidates = parse_extraction_response(response, 5);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_parse_extraction_response_empty_content_skipped() {
        let response = r#"[
            {"content": "", "memory_type": "Fact", "importance": 0.5},
            {"content": "Valid fact", "memory_type": "Fact", "importance": 0.7}
        ]"#;
        let candidates = parse_extraction_response(response, 5);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].content, "Valid fact");
    }

    #[test]
    fn test_parse_extraction_response_unknown_type_defaults_to_fact() {
        let response = r#"[{"content": "Something", "memory_type": "Unknown", "importance": 0.5}]"#;
        let candidates = parse_extraction_response(response, 5);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].memory_type, MemoryType::Fact);
    }

    #[test]
    fn test_parse_extraction_response_clamps_importance() {
        let response = r#"[
            {"content": "Over", "memory_type": "Fact", "importance": 1.5},
            {"content": "Under", "memory_type": "Fact", "importance": -0.3}
        ]"#;
        let candidates = parse_extraction_response(response, 5);
        assert_eq!(candidates.len(), 2);
        assert!((candidates[0].importance - 1.0).abs() < 0.01);
        assert!((candidates[1].importance - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_extraction_response_missing_fields_use_defaults() {
        let response = r#"[{"content": "Just content"}]"#;
        let candidates = parse_extraction_response(response, 5);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].memory_type, MemoryType::Fact);
        assert!((candidates[0].importance - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_parse_extraction_response_surrounding_text() {
        let response = r#"Here are the extracted memories:
[{"content": "Use axum for HTTP", "memory_type": "Decision", "importance": 0.85}]
That's all I found."#;
        let candidates = parse_extraction_response(response, 5);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].content, "Use axum for HTTP");
    }

    #[test]
    fn test_cap_recent_messages_keeps_tail() {
        // FIX #5: over the cap → last N kept (freshest facts); under → unchanged.
        let many: Vec<SessionMessage> = (0..100).map(|i| msg(i, "user", "x")).collect();
        let capped = cap_recent_messages(&many, MAX_EXTRACTION_MESSAGES);
        assert_eq!(capped.len(), MAX_EXTRACTION_MESSAGES);
        assert_eq!(capped[0].id, (100 - MAX_EXTRACTION_MESSAGES) as i64);
        assert_eq!(capped[capped.len() - 1].id, 99);

        let few = vec![msg(1, "user", "a"), msg(2, "assistant", "b")];
        assert_eq!(cap_recent_messages(&few, MAX_EXTRACTION_MESSAGES).len(), 2);
    }

    #[test]
    fn test_build_extraction_prompt() {
        let messages = vec![
            msg(1, "user", "I prefer dark mode"),
            msg(2, "assistant", "Noted!"),
            msg(3, "tool_use", "search"),
            msg(4, "tool", "results"),
        ];
        let prompt = build_extraction_prompt(&messages, 5);
        assert_eq!(prompt.len(), 2);
        assert_eq!(prompt[0].role, Role::System);
        assert!(prompt[0].content.contains("JSON array"));
        assert!(prompt[0].content.contains("5")); // max_facts
        assert_eq!(prompt[1].role, Role::User);
        assert!(prompt[1].content.contains("dark mode"));
        assert!(prompt[1].content.contains("Noted"));
        // tool_use/tool messages should be filtered out
        assert!(!prompt[1].content.contains("search"));
    }

    #[tokio::test]
    async fn test_extract_flush_candidates_llm_with_mock() {
        let mock_response = r#"[
            {"content": "User prefers Rust", "memory_type": "Preference", "importance": 0.9},
            {"content": "Decided on SQLite storage", "memory_type": "Decision", "importance": 0.8}
        ]"#;
        let llm = LlmRouter::mock(mock_response);
        let config = MemoryFlushConfig {
            enabled: true,
            max_facts: 5,
            use_llm: true,
            llm_timeout_secs: 5,
        };
        let messages = vec![
            msg(1, "user", "Let's use Rust for everything"),
            msg(2, "assistant", "Great choice. I'll use SQLite for storage."),
        ];

        let result = extract_flush_candidates_llm(&messages, &llm, &config).await;
        assert!(result.is_ok());
        let candidates = result.unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].content, "User prefers Rust");
        assert_eq!(candidates[0].memory_type, MemoryType::Preference);
    }

    #[tokio::test]
    async fn test_extract_flush_candidates_llm_empty_messages() {
        let llm = LlmRouter::mock("[]");
        let config = MemoryFlushConfig::default();
        let result = extract_flush_candidates_llm(&[], &llm, &config).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_extract_flush_candidates_llm_no_providers() {
        let llm = LlmRouter::empty();
        let config = MemoryFlushConfig::default();
        let messages = vec![msg(1, "user", "Hello")];
        let result = extract_flush_candidates_llm(&messages, &llm, &config).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no LLM providers"));
    }

    #[tokio::test]
    async fn test_flush_memories_llm_uses_llm_when_available() {
        let mock_response =
            r#"[{"content": "Fact from LLM", "memory_type": "Fact", "importance": 0.7}]"#;
        let llm = LlmRouter::mock(mock_response);
        let config = MemoryFlushConfig {
            enabled: true,
            max_facts: 5,
            use_llm: true,
            llm_timeout_secs: 5,
        };
        let metrics = FlushMetrics::new();
        let messages = vec![
            msg(1, "user", "Something important to remember always"),
            msg(2, "assistant", "Noted."),
        ];

        let candidates = flush_memories_llm(&messages, &config, Some(&llm), Some(&metrics)).await;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].content, "Fact from LLM");

        let snap = metrics.snapshot();
        assert_eq!(snap.extractions_attempted, 1);
        assert_eq!(snap.extractions_succeeded, 1);
        assert_eq!(snap.facts_extracted_count, 1);
        assert_eq!(snap.llm_fallback_count, 0);
    }

    #[tokio::test]
    async fn test_flush_memories_llm_falls_back_on_no_llm() {
        let config = MemoryFlushConfig {
            enabled: true,
            max_facts: 5,
            use_llm: true,
            llm_timeout_secs: 5,
        };
        let metrics = FlushMetrics::new();
        let messages = vec![msg(1, "user", "Always use Rust for systems programming.")];

        // No LLM provided — should fall back to keyword extraction
        let candidates = flush_memories_llm(&messages, &config, None, Some(&metrics)).await;
        assert!(!candidates.is_empty());
        assert!(candidates[0].content.contains("Always"));
        assert_eq!(candidates[0].memory_type, MemoryType::Fact); // keyword fallback always Fact

        let snap = metrics.snapshot();
        assert_eq!(snap.extractions_attempted, 1);
        assert_eq!(snap.llm_fallback_count, 1);
    }

    #[tokio::test]
    async fn test_flush_memories_llm_falls_back_on_bad_response() {
        // Mock returns garbage that won't parse as valid candidates
        let llm = LlmRouter::mock("This is not valid JSON at all, just text");
        let config = MemoryFlushConfig {
            enabled: true,
            max_facts: 5,
            use_llm: true,
            llm_timeout_secs: 5,
        };
        let metrics = FlushMetrics::new();
        let messages = vec![msg(
            1,
            "user",
            "We decided to always use SQLite for storage.",
        )];

        let candidates = flush_memories_llm(&messages, &config, Some(&llm), Some(&metrics)).await;
        // Should fall back to keyword extraction (has "decided" and "always")
        assert!(!candidates.is_empty());

        let snap = metrics.snapshot();
        assert_eq!(snap.llm_fallback_count, 1);
    }

    #[tokio::test]
    async fn test_flush_memories_llm_disabled_uses_keywords() {
        let llm = LlmRouter::mock("should not be called");
        let config = MemoryFlushConfig {
            enabled: true,
            max_facts: 5,
            use_llm: false, // LLM disabled
            llm_timeout_secs: 5,
        };
        let metrics = FlushMetrics::new();
        let messages = vec![msg(1, "user", "Remember to save often.")];

        let candidates = flush_memories_llm(&messages, &config, Some(&llm), Some(&metrics)).await;
        assert!(!candidates.is_empty());
        assert!(candidates[0].content.contains("Remember"));

        let snap = metrics.snapshot();
        assert_eq!(snap.llm_fallback_count, 0); // not a fallback, just configured off
    }

    #[tokio::test]
    async fn test_flush_memories_llm_disabled_globally() {
        let config = MemoryFlushConfig {
            enabled: false,
            max_facts: 5,
            use_llm: true,
            llm_timeout_secs: 5,
        };
        let messages = vec![msg(1, "user", "Always use Rust.")];
        let candidates = flush_memories_llm(&messages, &config, None, None).await;
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_memory_flush_config_defaults_new_fields() {
        let config = MemoryFlushConfig::default();
        assert!(config.use_llm);
        assert_eq!(config.llm_timeout_secs, 60);
    }

    #[test]
    fn test_memory_flush_config_deserialize_backwards_compat() {
        // Old configs without new fields should still deserialize
        let json = r#"{"enabled": true, "max_facts": 3}"#;
        let config: MemoryFlushConfig = serde_json::from_str(json).unwrap();
        assert!(config.enabled);
        assert_eq!(config.max_facts, 3);
        assert!(config.use_llm); // default
        assert_eq!(config.llm_timeout_secs, 60); // default
    }

    #[test]
    fn test_flush_metrics_snapshot() {
        let metrics = FlushMetrics::new();
        metrics
            .extractions_attempted
            .fetch_add(5, Ordering::Relaxed);
        metrics
            .extractions_succeeded
            .fetch_add(4, Ordering::Relaxed);
        metrics
            .facts_extracted_count
            .fetch_add(12, Ordering::Relaxed);
        metrics.llm_fallback_count.fetch_add(1, Ordering::Relaxed);

        let snap = metrics.snapshot();
        assert_eq!(snap.extractions_attempted, 5);
        assert_eq!(snap.extractions_succeeded, 4);
        assert_eq!(snap.facts_extracted_count, 12);
        assert_eq!(snap.llm_fallback_count, 1);
    }

    #[test]
    fn test_memory_type_display() {
        assert_eq!(MemoryType::Fact.to_string(), "Fact");
        assert_eq!(MemoryType::Preference.to_string(), "Preference");
        assert_eq!(MemoryType::Decision.to_string(), "Decision");
        assert_eq!(MemoryType::Correction.to_string(), "Correction");
        assert_eq!(MemoryType::Event.to_string(), "Event");
        assert_eq!(MemoryType::Observation.to_string(), "Observation");
    }

    #[test]
    fn test_all_memory_types_parsed() {
        let types = vec![
            ("Fact", MemoryType::Fact),
            ("Preference", MemoryType::Preference),
            ("Decision", MemoryType::Decision),
            ("Correction", MemoryType::Correction),
            ("Event", MemoryType::Event),
            ("Observation", MemoryType::Observation),
        ];
        for (name, expected) in types {
            let json =
                format!(r#"[{{"content": "test", "memory_type": "{name}", "importance": 0.5}}]"#);
            let candidates = parse_extraction_response(&json, 5);
            assert_eq!(candidates[0].memory_type, expected, "Failed for {name}");
        }
    }

    #[test]
    fn test_auto_compact_circuit_breaker_trips_after_3_failures() {
        let breaker = AutoCompactBreaker::new(3);
        assert!(breaker.should_attempt());

        breaker.record_failure();
        breaker.record_failure();
        assert!(breaker.should_attempt()); // 2 failures — still ok

        breaker.record_failure();
        assert!(!breaker.should_attempt()); // 3 failures — tripped

        breaker.record_success();
        assert!(breaker.should_attempt()); // Reset on success
    }
}
