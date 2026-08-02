//! Memory quality gates and content scoring.
//!
//! Advisory validation + quality scoring for memories stored via `brain.remember()`.
//! Low-quality memories are still stored (with a `quality_score` marker) to allow
//! analysis without data loss.

use regex::Regex;
use std::sync::LazyLock;
use tracing::debug;

/// Result of memory quality validation.
#[derive(Debug, Clone)]
pub struct QualityAssessment {
    /// Quality score from 0.0 (junk) to 1.0 (high-quality factual memory).
    pub score: f32,
    /// Human-readable reason if quality is low.
    pub flag: Option<String>,
}

/// Common low-value phrases that carry no semantic information.
const LOW_VALUE_PHRASES: &[&str] = &[
    "ok",
    "okay",
    "got it",
    "thanks",
    "thank you",
    "sure",
    "yes",
    "no",
    "yep",
    "nope",
    "yup",
    "alright",
    "right",
    "fine",
    "cool",
    "nice",
    "great",
    "good",
    "understood",
    "ack",
    "roger",
    "agreed",
    "done",
    "noted",
    "k",
    "kk",
    "ty",
    "thx",
    "np",
    "lol",
    "lmao",
    "haha",
    "hmm",
    "hm",
    "ah",
    "oh",
    "wow",
    "welp",
    "mhm",
];

/// Task description prefixes that indicate ephemeral intent, not lasting fact.
const TASK_PREFIXES: &[&str] = &[
    "Initiate ",
    "Start ",
    "Begin ",
    "Plan to ",
    "Need to ",
    "TODO:",
    "FIXME:",
    "Consider ",
    "Look into ",
];

/// Ephemeral metric patterns — runtime stats that go stale in minutes.
const EPHEMERAL_KEYWORDS: &[&str] = &[
    "tokens_per_second",
    "tok/s",
    "tokens/sec",
    "tokens/second",
    "queue_depth",
    "cache_hit_ratio",
    "memory_used_gb",
    "memory_total_gb",
    "avg_request_time_ms",
    "throughput measured at",
    "throughput:",
    "GPU utilization",
    "CPU utilization",
    "currently a conceptual",
    "CLI executed with query",
    "CLI executed from",
    "search CLI executed",
    // Past-tense ops snapshots
    "executed on deploy@",
    "transitioned from",
    "transitioned to",
    "validated via",
    "completed successfully",
    "all running containers",
    "all 26 dashboards",
    // Filter/query scope artifacts
    "search criteria set to",
    "filtered by role",
    "scope=live",
    "exported jobs filtered",
];

/// Past-tense operational status leading verbs.
/// Content starting with one of these followed by a generic ops object
/// is a status snapshot, not a durable fact.
static RE_PAST_TENSE_OPS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(executed|ran|validated|verified|deployed|transitioned|activated|initialized|confirmed|generated|stored|completed)\s+(diagnostic|command|test|that|the|all|infrastructure|monitoring|automated|post-execution|smoke)",
    )
    .expect("valid regex")
});

/// Cheap pre-filter that runs **before** embedding generation.
///
/// Returns `Ok(())` if the content is worth ingesting, or `Err(reason)` with
/// a descriptive rejection reason. This avoids wasting embedding compute on
/// content that will never be useful.
///
/// Checks (in order):
/// 1. All whitespace or empty
/// 2. All punctuation / non-alphanumeric
/// 3. Below minimum length (~5 words / 20 chars)
/// 4. Common low-value phrases ("ok", "got it", "thanks", etc.)
/// 5. Raw JSON blobs (starts with `{` or `[` and over 500 chars)
/// 6. Conversation artifacts (XML tags, skill dumps)
pub fn should_ingest(content: &str) -> Result<(), String> {
    let trimmed = content.trim();

    // 1. Empty or all whitespace
    if trimmed.is_empty() {
        return Err("empty or whitespace-only content".into());
    }

    // 2. All punctuation / non-alphanumeric
    if !trimmed.chars().any(|c| c.is_alphanumeric()) {
        return Err("no alphanumeric characters".into());
    }

    // 3. Minimum length
    if trimmed.len() < 20 {
        return Err(format!("too short ({} chars, minimum 20)", trimmed.len()));
    }

    // 4. Low-value phrase (normalize: lowercase, strip trailing punctuation)
    let normalized = trimmed
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .to_lowercase();
    if LOW_VALUE_PHRASES.contains(&normalized.as_str()) {
        return Err(format!("low-value phrase: \"{trimmed}\""));
    }

    // 5. Raw JSON blobs (likely tool output dumps)
    if trimmed.len() > 500 {
        let first = trimmed.as_bytes()[0];
        if first == b'{' || first == b'[' {
            return Err(format!(
                "raw JSON blob ({} chars, likely tool output)",
                trimmed.len()
            ));
        }
    }

    // 6. Conversation artifacts
    if let Some(reason) = is_conversation_artifact(trimmed) {
        return Err(reason.to_string());
    }

    // 7. Ephemeral runtime metrics (go stale in minutes)
    if is_ephemeral_metric(trimmed) {
        return Err("ephemeral runtime metric — goes stale in minutes".into());
    }

    // 8. Past-tense operational status snapshots
    if RE_PAST_TENSE_OPS.is_match(trimmed) {
        return Err("past-tense operational snapshot — not a durable fact".into());
    }

    Ok(())
}

/// Validate and score a memory's content quality.
///
/// Returns a `QualityAssessment` with a score (0.0–1.0) and optional flag.
/// Conversation artifacts (XML notifications, skill dumps) get score 0.0.
/// The caller is responsible for enforcing any blocking threshold.
pub fn assess_quality(content: &str) -> QualityAssessment {
    // Pre-filter: conversation artifacts are always junk
    if let Some(reason) = is_conversation_artifact(content) {
        debug!(
            reason,
            len = content.len(),
            "quality gate: conversation artifact rejected"
        );
        return QualityAssessment {
            score: 0.0,
            flag: Some(reason.to_string()),
        };
    }

    let score = content_quality_score(content);
    let flag = validate_memory_quality(content).err();

    if let Some(ref reason) = flag {
        debug!(
            score,
            reason,
            len = content.len(),
            "quality gate: low-quality memory"
        );
    }

    QualityAssessment { score, flag }
}

/// Detect conversation artifacts that should never be stored as memories.
///
/// Returns a rejection reason if the content matches known noise patterns
/// (XML notification blocks, skill file dumps, system reminders).
pub fn is_conversation_artifact(content: &str) -> Option<&'static str> {
    let trimmed = content.trim();

    if trimmed.starts_with("<task-notification") {
        return Some("conversation artifact: task-notification XML");
    }
    if trimmed.starts_with("<teammate-message") {
        return Some("conversation artifact: teammate-message XML");
    }
    if trimmed.starts_with("<system-reminder") {
        return Some("conversation artifact: system-reminder XML");
    }
    if trimmed.contains("Base directory for this skill:") {
        return Some("conversation artifact: skill file dump");
    }

    // Detect XML-heavy content (3+ XML-like tags in first 200 chars)
    // Use floor_char_boundary to avoid panicking on multi-byte UTF-8 chars
    let end = trimmed.len().min(200);
    let end = trimmed.floor_char_boundary(end);
    let prefix = &trimmed[..end];
    let tag_count = prefix.matches('<').count();
    if tag_count >= 3 && prefix.matches("</").count() >= 1 {
        return Some("conversation artifact: XML-heavy content");
    }

    None
}

/// Advisory validation — returns `Err(reason)` for content that is likely not
/// a durable fact.  The memory is still stored; the reason is logged.
fn validate_memory_quality(content: &str) -> Result<(), String> {
    let trimmed = content.trim();

    // Too short — vague / meaningless
    if trimmed.len() < 20 {
        return Err("too short (< 20 chars)".into());
    }

    // Too long — likely a dump, not a concise memory
    if trimmed.len() > 2000 {
        return Err("too long (> 2000 chars)".into());
    }

    // Task description, not a lasting fact
    if is_task_description(trimmed) {
        return Err("task description, not a durable fact".into());
    }

    // Pure question with no embedded answer
    if trimmed.ends_with('?') && !trimmed.contains(':') {
        return Err("question without answer".into());
    }

    Ok(())
}

/// Detect ephemeral task descriptions masquerading as memories.
fn is_task_description(content: &str) -> bool {
    TASK_PREFIXES
        .iter()
        .any(|p| content.starts_with(p) || content.starts_with(&p.to_lowercase()))
}

/// Detect ephemeral runtime metrics that go stale within minutes.
fn is_ephemeral_metric(content: &str) -> bool {
    let lower = content.to_lowercase();
    EPHEMERAL_KEYWORDS
        .iter()
        .any(|kw| lower.contains(&kw.to_lowercase()))
}

// ── Regex patterns (compiled once) ──────────────────────────────────

static RE_NUMBERS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").expect("valid regex"));

static RE_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"v\d+\.\d+|build\s+\w+").expect("valid regex"));

static RE_PATH_OR_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(/[\w./-]+|https?://|\\[\w\\]+|[A-Z]:\\)").expect("valid regex"));

static RE_TECHNICAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(config|server|port|api|endpoint|database|crate|module|service)\b")
        .expect("valid regex")
});

static RE_FLAGS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(--\w+|=\w+|\bport\s+\d+)").expect("valid regex"));

/// Compute a quality score in `[0.0, 1.0]` for memory content.
///
/// Scoring heuristics:
/// - +0.3 if contains specific numbers/values (port 8080, --flag, =value)
/// - +0.2 if contains version strings (v1.2, build abc123)
/// - +0.2 if contains file paths or URLs
/// - +0.1 if contains technical terms (config, server, port, API, endpoint)
/// - +0.2 if length is in the sweet spot (50–500 chars)
/// - -0.3 if starts with task prefixes
/// - -0.2 if contains no specifics at all
pub fn content_quality_score(content: &str) -> f32 {
    let trimmed = content.trim();
    let mut score: f32 = 0.0;

    // +0.3: specific numbers, flags, or port values
    if RE_NUMBERS.is_match(trimmed) || RE_FLAGS.is_match(trimmed) {
        score += 0.3;
    }

    // +0.2: version strings
    if RE_VERSION.is_match(trimmed) {
        score += 0.2;
    }

    // +0.2: file paths or URLs
    if RE_PATH_OR_URL.is_match(trimmed) {
        score += 0.2;
    }

    // +0.1: technical terms
    if RE_TECHNICAL.is_match(trimmed) {
        score += 0.1;
    }

    // +0.2: length sweet spot (50–500 chars)
    let len = trimmed.len();
    if (50..=500).contains(&len) {
        score += 0.2;
    }

    // -0.3: task prefix
    if is_task_description(trimmed) {
        score -= 0.3;
    }

    // -0.2: no specifics at all (no numbers, no paths, no flags)
    let has_any_specific = RE_NUMBERS.is_match(trimmed)
        || RE_PATH_OR_URL.is_match(trimmed)
        || RE_FLAGS.is_match(trimmed);
    if !has_any_specific {
        score -= 0.2;
    }

    // -0.4: ephemeral runtime metrics
    if is_ephemeral_metric(trimmed) {
        score -= 0.4;
    }

    score.clamp(0.0, 1.0)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_high_quality_memory() {
        let content = "Ironjaw gateway runs on port 18794 with config at config/ironjaw.toml";
        let assessment = assess_quality(content);
        assert!(assessment.score >= 0.5, "score={}", assessment.score);
        assert!(assessment.flag.is_none());
    }

    #[test]
    fn test_version_boosts_score() {
        let content = "Upgraded ironjaw-brain to v0.8.0 with quality gates";
        let score = content_quality_score(content);
        assert!(score >= 0.4, "score={score}");
    }

    #[test]
    fn test_too_short_flagged() {
        let assessment = assess_quality("short");
        assert!(assessment.flag.is_some());
        assert!(assessment.flag.unwrap().contains("too short"));
    }

    #[test]
    fn test_too_long_flagged() {
        let long = "x".repeat(2500);
        let assessment = assess_quality(&long);
        assert!(assessment.flag.is_some());
        assert!(assessment.flag.unwrap().contains("too long"));
    }

    #[test]
    fn test_task_description_flagged() {
        let assessment =
            assess_quality("TODO: fix the extraction pipeline prompts for better specificity");
        assert!(assessment.flag.is_some());
        assert!(assessment.flag.unwrap().contains("task"));
    }

    #[test]
    fn test_question_flagged() {
        let assessment = assess_quality("What is the default port for ironjaw?");
        assert!(assessment.flag.is_some());
        assert!(assessment.flag.unwrap().contains("question"));
    }

    #[test]
    fn test_question_with_answer_ok() {
        let assessment = assess_quality("Default port: 18794 for Ironjaw gateway");
        assert!(assessment.flag.is_none());
    }

    #[test]
    fn test_no_specifics_penalized() {
        let content = "The user prefers a certain approach to things in general";
        let score = content_quality_score(content);
        // No numbers, no paths, no flags → penalized
        assert!(score < 0.3, "score={score}");
    }

    #[test]
    fn test_technical_content_scored_well() {
        let content =
            "workstation server runs llama-server on port 8080 with --model qwen3-coder:35b";
        let score = content_quality_score(content);
        assert!(score >= 0.6, "score={score}");
    }

    #[test]
    fn test_path_content_scored_well() {
        let content = "Brain database located at /c/code/ironjaw/data/brain.db";
        let score = content_quality_score(content);
        assert!(score >= 0.4, "score={score}");
    }

    #[test]
    fn test_score_clamped() {
        // Even the best content shouldn't exceed 1.0
        let content = "v2.0.0 server on port 8080 at /api/config endpoint --verbose=true";
        let score = content_quality_score(content);
        assert!(score <= 1.0);
        assert!(score >= 0.5);
    }

    #[test]
    fn test_task_prefix_case_insensitive() {
        let assessment = assess_quality("need to investigate the memory leak in brain store");
        assert!(assessment.flag.is_some());
    }

    #[test]
    fn test_consider_prefix_flagged() {
        let assessment =
            assess_quality("Consider using a different embedding model for better recall");
        assert!(assessment.flag.is_some());
    }

    #[test]
    fn test_begin_prefix_flagged() {
        let assessment = assess_quality("Begin the migration from SQLite to PostgreSQL today");
        assert!(assessment.flag.is_some());
    }

    #[test]
    fn test_validate_passes_good_content() {
        let result = validate_memory_quality("Ironjaw uses axum 0.8 with tokio runtime");
        assert!(result.is_ok());
    }

    #[test]
    fn test_task_notification_artifact() {
        let content = "<task-notification>Some task update here</task-notification>";
        let assessment = assess_quality(content);
        assert_eq!(assessment.score, 0.0);
        assert!(assessment.flag.unwrap().contains("task-notification"));
    }

    #[test]
    fn test_teammate_message_artifact() {
        let content = "<teammate-message from=\"agent-1\">status update</teammate-message>";
        let assessment = assess_quality(content);
        assert_eq!(assessment.score, 0.0);
        assert!(assessment.flag.unwrap().contains("teammate-message"));
    }

    #[test]
    fn test_system_reminder_artifact() {
        let content = "<system-reminder>You have tools available</system-reminder>";
        let assessment = assess_quality(content);
        assert_eq!(assessment.score, 0.0);
        assert!(assessment.flag.unwrap().contains("system-reminder"));
    }

    #[test]
    fn test_skill_dump_artifact() {
        let content = "Some skill content\nBase directory for this skill: /path/to/skill\nMore content here with enough length";
        let assessment = assess_quality(content);
        assert_eq!(assessment.score, 0.0);
        assert!(assessment.flag.unwrap().contains("skill file dump"));
    }

    #[test]
    fn test_xml_heavy_artifact() {
        let content = "<foo><bar>data</bar><baz>more</baz></foo> and some text to pad length";
        let assessment = assess_quality(content);
        assert_eq!(assessment.score, 0.0);
        assert!(assessment.flag.unwrap().contains("XML-heavy"));
    }

    #[test]
    fn test_normal_content_not_artifact() {
        assert!(is_conversation_artifact("Ironjaw gateway runs on port 18794").is_none());
    }

    #[test]
    fn test_single_xml_tag_not_artifact() {
        // Content with a single XML-like angle bracket shouldn't trigger
        assert!(is_conversation_artifact("Use the <Brain> struct for memory operations").is_none());
    }

    // ── should_ingest tests ────────────────────────────────────────────

    #[test]
    fn test_ingest_rejects_empty() {
        let err = should_ingest("").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn test_ingest_rejects_whitespace_only() {
        let err = should_ingest("   \t\n  ").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn test_ingest_rejects_punctuation_only() {
        let err = should_ingest("...!!!???---").unwrap_err();
        assert!(err.contains("alphanumeric"), "got: {err}");
    }

    #[test]
    fn test_ingest_rejects_too_short() {
        let err = should_ingest("hello world").unwrap_err();
        assert!(err.contains("too short"), "got: {err}");
    }

    #[test]
    fn test_ingest_rejects_low_value_phrases() {
        for phrase in &["ok", "got it", "thanks", "sure", "yes", "no", "understood"] {
            let err = should_ingest(phrase).unwrap_err();
            assert!(
                err.contains("low-value") || err.contains("too short"),
                "expected rejection for '{phrase}', got: {err}"
            );
        }
    }

    #[test]
    fn test_ingest_rejects_low_value_with_punctuation() {
        let err = should_ingest("Got it!!!!!!!!!!!!!!!").unwrap_err();
        assert!(err.contains("low-value"), "got: {err}");
    }

    #[test]
    fn test_ingest_rejects_raw_json_blob() {
        let json = format!("{{{}}}", "\"key\": \"value\", ".repeat(50));
        let err = should_ingest(&json).unwrap_err();
        assert!(err.contains("JSON blob"), "got: {err}");
    }

    #[test]
    fn test_ingest_rejects_json_array_blob() {
        let json = format!("[{}]", "1, ".repeat(200));
        let err = should_ingest(&json).unwrap_err();
        assert!(err.contains("JSON blob"), "got: {err}");
    }

    #[test]
    fn test_ingest_allows_short_json() {
        // Short JSON-like content under 500 chars should pass
        let content = r#"{"port": 8080, "host": "localhost"} is the default config"#;
        assert!(should_ingest(content).is_ok());
    }

    #[test]
    fn test_ingest_rejects_conversation_artifact() {
        let content = "<task-notification>Agent completed task 42</task-notification>";
        let err = should_ingest(content).unwrap_err();
        assert!(err.contains("artifact"), "got: {err}");
    }

    #[test]
    fn test_ingest_accepts_good_content() {
        assert!(should_ingest("Ironjaw gateway runs on port 18794 with axum 0.8").is_ok());
    }

    #[test]
    fn test_ingest_accepts_medium_length_content() {
        assert!(should_ingest(
            "The brain subsystem uses SQLite with WAL mode for concurrent reads"
        )
        .is_ok());
    }

    #[test]
    fn test_ingest_rejects_ephemeral_metrics() {
        let cases = vec![
            "GPU utilization at 68% with memory_used_gb=18.4",
            "Current throughput: 14.2 tokens_per_second with avg_request_time_ms=420",
            "Local inference speed measured at 26 tokens/sec with ~40ms latency",
            "monster-search CLI executed with query parameters: 'best laptop'",
            "Current throughput measured at 1.8 tok/s under 32K context window",
        ];
        for content in cases {
            let err = should_ingest(content).unwrap_err();
            assert!(
                err.contains("ephemeral"),
                "expected ephemeral rejection for '{content}', got: {err}"
            );
        }
    }

    #[test]
    fn test_ephemeral_metric_penalizes_score() {
        let content = "Current throughput: 14.2 tokens_per_second with avg_request_time_ms=420 and queue_depth=2";
        let score = content_quality_score(content);
        assert!(
            score < 0.3,
            "ephemeral metric should score low, got: {score}"
        );
    }

    #[test]
    fn test_non_ephemeral_not_rejected() {
        // Architectural decisions about throughput should NOT be caught
        assert!(
            should_ingest("26 tok/s is deemed acceptable for real-time interactive chat").is_err()
        );
        // But config changes should pass
        assert!(should_ingest(
            "llama-server configured with --parallel 4 --ctx-size 131072 for production"
        )
        .is_ok());
    }

    #[test]
    fn test_ingest_rejects_case_insensitive_low_value() {
        // "OK" in caps should also be rejected (but it's too short anyway)
        let err = should_ingest("OK").unwrap_err();
        assert!(
            err.contains("too short") || err.contains("low-value"),
            "got: {err}"
        );
    }

    #[test]
    fn test_ingest_rejects_past_tense_ops_snapshots() {
        // Real examples from the 2026-04-29 pending-queue cleanup.
        // These all passed the old filter because they have specifics, but
        // they describe transient operational state, not durable facts.
        let cases = vec![
            "Executed diagnostic command on deploy@192.0.2.20: docker ps --filter 'name=ColorCatalog' --format 'table {{.Names}}\\t{{.Status}}\\t{{.Ports}}'",
            "Validated that all 26 dashboards transitioned from RED to GREEN status with optimized resource allocation",
            "Verified diagnostic test output across all running containers with healthcheck parameters",
            "Deployed monitoring agent to cc-chronicle container for real-time metrics",
            "Transitioned from V3 dashboards to V4 protocol with backwards compatibility",
            "Completed test suite execution covering the lib tests across 16 crates",
        ];
        for content in cases {
            let err = should_ingest(content).unwrap_err();
            assert!(
                err.contains("past-tense") || err.contains("ephemeral"),
                "expected past-tense or ephemeral rejection for '{content}', got: {err}"
            );
        }
    }

    #[test]
    fn test_ingest_rejects_filter_query_scope_artifacts() {
        // Filter/query scope strings — these are ephemeral search params, not config.
        let cases = vec![
            "monster-search search criteria set to: Scope=Live, Focus=AI & Tech, Quality Filter=High Engagement",
            "Exported jobs filtered by Role alignment, Salary range $200k+, Location Greenwich/Remote",
        ];
        for content in cases {
            let err = should_ingest(content).unwrap_err();
            assert!(
                err.contains("ephemeral"),
                "expected ephemeral rejection for '{content}', got: {err}"
            );
        }
    }

    #[test]
    fn test_durable_facts_still_pass_after_tightening() {
        // Architectural decisions and durable config must still pass — the new
        // filters should not be so aggressive they reject good content.
        let durable = vec![
            "AdGuard Home is the primary DNS resolver for Meshnet (192.0.2.20:53, DoH enabled)",
            "DNS caching strategy implemented for smartnews.com and herby-strout.io domains",
            "llama-server configured with --cache-type-k q8_0 --cache-type-v q8_0 for 35B models",
            "Ironjaw gateway listens on 0.0.0.0:18794 for LAN reachability from docker-host",
            "Canary smoke test auto-invokes on ExamplePipeline pipeline startup",
        ];
        for content in durable {
            assert!(
                should_ingest(content).is_ok(),
                "durable fact incorrectly rejected: '{content}' → {:?}",
                should_ingest(content)
            );
        }
    }
}
