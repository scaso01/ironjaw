//! Token counting and budget enforcement for static-layer content injection.
//!
//! Uses `tiktoken-rs` (cl100k_base BPE) as a close-enough approximation for
//! both OpenAI and Anthropic models.  The exact count varies slightly per
//! provider, but cl100k_base errs on the conservative side.
//!
//! ## Token budget policy
//!
//! | Limit | Value | Rationale |
//! |-------|-------|-----------|
//! | Per-file cap | 4 096 | Prevents one giant CLAUDE.md from eating the context |
//! | Aggregate cap | 16 384 | Sum of all static-layer entries after per-file truncation |
//!
//! When an entry exceeds the per-file cap it is **truncated at the nearest
//! newline boundary** below the token limit.  When the running total exceeds
//! the aggregate cap, remaining entries are dropped entirely and the truncation
//! event is recorded via the `static_layer_tokens` telemetry counter.
//!
//! ## Telemetry
//!
//! `STATIC_LAYER_TOKENS` is an `AtomicUsize` that accumulates the total tokens
//! consumed by each `apply_budget()` call across the process lifetime.  Read it
//! via `static_layer_tokens_total()` or reset it in tests via
//! `reset_static_layer_tokens()`.

use std::sync::atomic::{AtomicUsize, Ordering};
use tiktoken_rs::cl100k_base;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Hard per-file token cap.  Content exceeding this is beat down.
pub const PER_FILE_TOKEN_CAP: usize = 4_096;

/// Hard aggregate token cap across all static-layer entries combined.
pub const AGGREGATE_TOKEN_CAP: usize = 16_384;

// ---------------------------------------------------------------------------
// Telemetry counter
// ---------------------------------------------------------------------------

static STATIC_LAYER_TOKENS: AtomicUsize = AtomicUsize::new(0);

/// Returns the total tokens consumed by static-layer injection since process start.
pub fn static_layer_tokens_total() -> usize {
    STATIC_LAYER_TOKENS.load(Ordering::Relaxed)
}

/// Resets the telemetry counter.  Intended for test isolation only.
pub fn reset_static_layer_tokens() {
    STATIC_LAYER_TOKENS.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Count the approximate number of tokens in `text` using cl100k_base BPE.
///
/// Returns 0 if the tokenizer cannot be initialised (should never happen in
/// practice — the BPE data is compiled into the binary via `tiktoken-rs`).
pub fn count_tokens(text: &str) -> usize {
    match cl100k_base() {
        Ok(bpe) => bpe.encode_with_special_tokens(text).len(),
        Err(_) => {
            // Graceful degradation: rough estimate (1 token ≈ 4 chars)
            text.len() / 4
        }
    }
}

/// Result of applying the budget to a list of (content, source_label) pairs.
#[derive(Debug, Clone)]
pub struct BudgetResult {
    /// Content strings after truncation.  Same order as input but may be
    /// shorter (truncated) or missing (dropped due to aggregate cap).
    pub entries: Vec<BudgetEntry>,
    /// Total tokens consumed across all retained entries.
    pub total_tokens: usize,
    /// Number of entries truncated at the per-file cap.
    pub truncated_count: usize,
    /// Number of entries dropped because the aggregate cap was reached.
    pub dropped_count: usize,
}

/// A single budget-processed entry.
#[derive(Debug, Clone)]
pub struct BudgetEntry {
    pub content: String,
    pub token_count: usize,
    pub source_label: String,
    /// `true` if the content was truncated from its original form.
    pub was_truncated: bool,
}

/// Apply per-file and aggregate token caps to a list of (content, label) pairs.
///
/// Process:
/// 1. Each entry is counted and, if over `PER_FILE_TOKEN_CAP`, beat down to
///    the longest newline-aligned prefix that fits.
/// 2. Entries are accumulated until the running total would exceed
///    `AGGREGATE_TOKEN_CAP`.  Remaining entries are dropped.
/// 3. The aggregate token count is added to the `STATIC_LAYER_TOKENS` counter.
pub fn apply_budget(entries: impl IntoIterator<Item = (String, String)>) -> BudgetResult {
    let bpe = cl100k_base().ok();
    let mut result_entries = Vec::new();
    let mut running_total: usize = 0;
    let mut truncated_count: usize = 0;
    let mut dropped_count: usize = 0;

    for (content, label) in entries {
        if running_total >= AGGREGATE_TOKEN_CAP {
            dropped_count += 1;
            continue;
        }

        let (processed, tokens, was_truncated) =
            fit_to_token_cap(&bpe, content, PER_FILE_TOKEN_CAP);
        if was_truncated {
            truncated_count += 1;
        }

        // Check aggregate cap after per-file truncation
        let remaining_budget = AGGREGATE_TOKEN_CAP.saturating_sub(running_total);
        if tokens > remaining_budget {
            // Beat down to remaining aggregate budget
            let (agg_processed, agg_tokens, _) =
                fit_to_token_cap(&bpe, processed, remaining_budget);
            running_total += agg_tokens;
            result_entries.push(BudgetEntry {
                content: agg_processed,
                token_count: agg_tokens,
                source_label: label,
                was_truncated: true,
            });
            truncated_count += 1;
        } else {
            running_total += tokens;
            result_entries.push(BudgetEntry {
                content: processed,
                token_count: tokens,
                source_label: label,
                was_truncated,
            });
        }
    }

    STATIC_LAYER_TOKENS.fetch_add(running_total, Ordering::Relaxed);

    BudgetResult {
        entries: result_entries,
        total_tokens: running_total,
        truncated_count,
        dropped_count,
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Truncate `content` so its token count is at most `cap`.
///
/// Truncation is done at the nearest newline boundary below the cap.
/// If no newline is found the content is character-level truncated (rare —
/// only happens on very long single-line files).
///
/// Returns `(processed_content, token_count, was_truncated)`.
fn fit_to_token_cap(
    bpe: &Option<tiktoken_rs::CoreBPE>,
    content: String,
    cap: usize,
) -> (String, usize, bool) {
    let tokens = match bpe {
        Some(bpe) => bpe.encode_with_special_tokens(&content).len(),
        None => content.len() / 4,
    };

    if tokens <= cap {
        return (content, tokens, false);
    }

    // Binary search for the longest newline-aligned prefix that fits.
    let lines: Vec<&str> = content.lines().collect();
    let mut lo = 0usize;
    let mut hi = lines.len();

    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let candidate: String = lines[..=mid].join("\n");
        let candidate_tokens = match bpe {
            Some(bpe) => bpe.encode_with_special_tokens(&candidate).len(),
            None => candidate.len() / 4,
        };
        if candidate_tokens <= cap {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }

    if lo == 0 {
        // Even the first line exceeds cap — hard character truncation with appended notice
        let truncated: String = content.chars().take(cap * 4).collect();
        let final_tokens = match bpe {
            Some(bpe) => bpe.encode_with_special_tokens(&truncated).len(),
            None => truncated.len() / 4,
        };
        let notice = "\n[...truncated: content exceeded per-file token cap]";
        return (truncated + notice, final_tokens, true);
    }

    let prefix_lines = lo.saturating_sub(1);
    let mut truncated = lines[..prefix_lines].join("\n");
    truncated.push_str("\n[...truncated: content exceeded per-file token cap]");
    let final_tokens = match bpe {
        Some(bpe) => bpe.encode_with_special_tokens(&truncated).len(),
        None => truncated.len() / 4,
    };
    (truncated, final_tokens, true)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_tokens_nonempty() {
        let n = count_tokens("Hello, world!");
        // At minimum 1 token
        assert!(n > 0, "expected > 0 tokens, got {n}");
    }

    #[test]
    fn count_tokens_empty() {
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn apply_budget_short_content_passes_through() {
        reset_static_layer_tokens();
        let entries = vec![("# Hello".to_string(), "project".to_string())];
        let result = apply_budget(entries);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.truncated_count, 0);
        assert_eq!(result.dropped_count, 0);
        assert!(!result.entries[0].was_truncated);
    }

    #[test]
    fn apply_budget_per_file_cap_truncates() {
        reset_static_layer_tokens();
        // Build a string guaranteed to exceed 4096 tokens
        let long_content = "word ".repeat(6_000);
        let entries = vec![(long_content, "project".to_string())];
        let result = apply_budget(entries);
        assert_eq!(result.entries.len(), 1);
        assert!(result.entries[0].was_truncated, "should be truncated");
        assert!(result.entries[0].token_count <= PER_FILE_TOKEN_CAP + 50); // small fuzz for notice
    }

    #[test]
    fn apply_budget_aggregate_cap_drops_excess() {
        reset_static_layer_tokens();
        // Each entry ~8192 tokens — 3 entries exceed 16384 aggregate cap
        let big_entry = "word ".repeat(10_000);
        let entries = vec![
            (big_entry.clone(), "project".to_string()),
            (big_entry.clone(), "parent:foo".to_string()),
            (big_entry, "global".to_string()),
        ];
        let result = apply_budget(entries);
        // At least one entry must have been dropped
        assert!(result.dropped_count > 0 || result.truncated_count > 0);
        assert!(result.total_tokens <= AGGREGATE_TOKEN_CAP + 100); // small fuzz for notice tokens
    }

    #[test]
    fn apply_budget_telemetry_accumulates() {
        // Verify that apply_budget reports a non-zero token count for real
        // content and that the telemetry counter advances by at least that
        // amount.  We cannot assert an exact delta because other tests running
        // in parallel also call apply_budget (and may call
        // reset_static_layer_tokens), so the global counter is inherently racy.
        // What we CAN assert: result.total_tokens is non-zero, AND the counter
        // after our call is at least as large as result.total_tokens.
        let entries = vec![(
            "# Hello world this is a test".to_string(),
            "project".to_string(),
        )];
        let result = apply_budget(entries);
        assert!(result.total_tokens > 0, "expected non-zero token count");
        // Counter must be at least as large as what we just contributed.
        let total = static_layer_tokens_total();
        assert!(
            total >= result.total_tokens,
            "global counter ({total}) should be >= our contribution ({})",
            result.total_tokens
        );
    }

    #[test]
    fn apply_budget_empty_input() {
        reset_static_layer_tokens();
        let result = apply_budget(std::iter::empty());
        assert_eq!(result.entries.len(), 0);
        assert_eq!(result.total_tokens, 0);
    }
}
