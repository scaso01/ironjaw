//! MicroCompact: zero-API-call local trimming of tool results and verbose output.
//! Triggered on every tool result that exceeds a token threshold.

/// Trims a tool result to fit within a token budget. Zero API calls.
/// Preserves head (first 40%) and tail (last 20%) of content, inserting
/// a `[trimmed N lines]` marker in the middle.
pub fn micro_compact(content: &str, max_tokens: usize) -> String {
    let estimated_tokens = content.len() / 4;
    if estimated_tokens <= max_tokens {
        return content.to_string();
    }

    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= 4 {
        // Too few lines to split meaningfully — truncate chars
        let max_chars = max_tokens * 4;
        let keep = max_chars.saturating_sub(40); // Reserve space for marker
        let half = keep / 2;
        return format!(
            "{}\n[trimmed to ~{max_tokens} tokens]\n{}",
            &content[..half.min(content.len())],
            &content[content.len().saturating_sub(half)..],
        );
    }

    let head_count = (lines.len() * 40) / 100; // 40% head
    let tail_count = (lines.len() * 20) / 100; // 20% tail
    let trimmed_count = lines.len() - head_count - tail_count;

    let head: String = lines[..head_count].join("\n");
    let tail: String = lines[lines.len() - tail_count..].join("\n");

    // Check if this fits budget
    let combined = format!("{head}\n[trimmed {trimmed_count} lines]\n{tail}");
    let combined_tokens = combined.len() / 4;

    if combined_tokens <= max_tokens {
        return combined;
    }

    // Still too big — further trim head
    let target_chars = max_tokens * 4;
    let marker = format!("\n[trimmed {trimmed_count} lines]\n");
    let available = target_chars.saturating_sub(marker.len());
    let head_chars = (available * 2) / 3;
    let tail_chars = available - head_chars;

    format!(
        "{}{}{}",
        &head[..head_chars.min(head.len())],
        marker,
        &tail[tail.len().saturating_sub(tail_chars)..],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_micro_compact_trims_large_tool_result() {
        let result = "x".repeat(8000); // ~2000 tokens
        let compacted = micro_compact(&result, 500);
        let estimated_tokens = compacted.len() / 4;
        assert!(estimated_tokens <= 550); // Allow small overshoot
        assert!(compacted.contains("[trimmed")); // Trimming marker
    }

    #[test]
    fn test_micro_compact_preserves_small_result() {
        let result = "short result";
        let compacted = micro_compact(result, 500);
        assert_eq!(compacted, result);
    }

    #[test]
    fn test_micro_compact_preserves_head_and_tail() {
        let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let result = lines.join("\n");
        let compacted = micro_compact(&result, 100);
        assert!(compacted.contains("line 0"));
        assert!(compacted.contains("line 99"));
    }
}
