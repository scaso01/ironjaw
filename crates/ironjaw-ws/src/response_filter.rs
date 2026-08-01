// Pre-response model name validator.
//
// Before an assistant response is sent to the WebSocket client, checks if it
// contains a self-identification claim with a model name that doesn't match
// the actual loaded model. If a mismatch is found, appends a correction note.
//
// Integration point: call `validate_model_claims()` in handler.rs on each
// completed response text (or accumulated streaming buffer) before sending
// to the client.
//
// Example usage in handler.rs:
//
//   use crate::response_filter::validate_model_claims;
//   let filtered = validate_model_claims(&response_text, &actual_model);

/// Known model name patterns to detect in self-identification claims.
///
/// Each entry is a lowercase substring that identifies a model family.
/// We only flag these when they appear inside self-identification phrases,
/// not arbitrary mentions (e.g., "compare llama vs mistral" is fine).
const MODEL_PATTERNS: &[&str] = &[
    "qwen3-coder",
    "qwen3:30b",
    "qwen3:8b",
    "qwen3:14b",
    "qwen3:32b",
    "qwen3:70b",
    "qwen3:4b",
    "qwen3.5",
    "qwen2.5",
    "qwen2",
    "qwen3",
    "qwen",
    "gemma3",
    "gemma2",
    "gemma",
    "llama4",
    "llama3",
    "llama2",
    "llama",
    "mistral",
    "mixtral",
    "codestral",
    "phi-4",
    "phi-3",
    "phi3",
    "phi4",
    "deepseek",
    "deepseek-r1",
    "deepseek-v3",
    "starcoder",
    "codellama",
    "yi-coder",
    "command-r",
    "claude",
    "gpt-4",
    "gpt-3.5",
    "chatgpt",
    "o1-preview",
    "o1-mini",
    "o3-mini",
    "gemini",
    "copilot",
    "devstral",
];

/// Phrases that indicate the model is identifying itself.
/// We look for these immediately before a model name to distinguish
/// self-identification ("I am Qwen") from casual mention ("compare Qwen vs Llama").
const SELF_ID_PREFIXES: &[&str] = &[
    "i am ",
    "i'm ",
    "my name is ",
    "i'm called ",
    "i am called ",
    "my model is ",
    "i'm running ",
    "i am running ",
    "i run on ",
    "i'm based on ",
    "i am based on ",
    "i am a ",
    "i'm a ",
    "i was built on ",
    "powered by ",
    "built on ",
    "running on ",
    "i'm powered by ",
    "i am powered by ",
    "as a ",
    "as an ",
    "model: ",
    "model name: ",
    "i'm an ",
    "i am an ",
];

/// Check if a response contains a model self-identification claim that doesn't
/// match the actual loaded model. If so, append a correction note.
///
/// Returns the original text if no mismatched claim is found, or the text with
/// a correction note appended.
pub fn validate_model_claims(response_text: &str, actual_model: &str) -> String {
    if response_text.is_empty() || actual_model.is_empty() {
        return response_text.to_string();
    }

    let lower = response_text.to_lowercase();
    let actual_lower = actual_model.to_lowercase();

    // For each self-identification prefix, check if any model name follows it
    for prefix in SELF_ID_PREFIXES {
        let mut search_from = 0;
        while let Some(prefix_pos) = lower[search_from..].find(prefix) {
            let abs_pos = search_from + prefix_pos;
            let after_prefix = abs_pos + prefix.len();

            if after_prefix >= lower.len() {
                break;
            }

            let remainder = &lower[after_prefix..];

            for pattern in MODEL_PATTERNS {
                if remainder.starts_with(pattern) {
                    // Found a self-identification claim with a model name.
                    // Check if it matches the actual model.
                    if !actual_lower.contains(pattern) && !pattern.contains(&actual_lower) {
                        // Mismatch: the model claimed to be something it's not.
                        return format!(
                            "{response_text}\n\n[System: The model incorrectly \
                             identified itself. Actual model: {actual_model}]"
                        );
                    }
                    // Match found, no correction needed — but keep scanning
                    // in case there's a conflicting claim later.
                }
            }

            search_from = after_prefix;
        }
    }

    response_text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_claim_unchanged() {
        let text = "Hello! How can I help you today?";
        let result = validate_model_claims(text, "qwen3-coder:30b");
        assert_eq!(result, text);
    }

    #[test]
    fn test_correct_claim_unchanged() {
        let text = "I am Qwen3-Coder, a helpful assistant.";
        let result = validate_model_claims(text, "qwen3-coder:30b");
        assert_eq!(result, text);
    }

    #[test]
    fn test_wrong_claim_appends_correction() {
        let text = "I am Llama3, a large language model.";
        let result = validate_model_claims(text, "qwen3-coder:30b");
        assert!(result.contains("[System: The model incorrectly identified itself."));
        assert!(result.contains("Actual model: qwen3-coder:30b"));
        assert!(result.starts_with(text));
    }

    #[test]
    fn test_casual_mention_not_flagged() {
        let text = "Llama3 and Mistral are both popular open-source models.";
        let result = validate_model_claims(text, "qwen3-coder:30b");
        assert_eq!(result, text);
    }

    #[test]
    fn test_im_prefix() {
        let text = "I'm Gemma2, happy to help!";
        let result = validate_model_claims(text, "qwen3:30b");
        assert!(result.contains("[System: The model incorrectly identified itself."));
        assert!(result.contains("Actual model: qwen3:30b"));
    }

    #[test]
    fn test_powered_by_prefix() {
        let text = "This response is powered by Mistral.";
        let result = validate_model_claims(text, "qwen3-coder:30b");
        assert!(result.contains("[System: The model incorrectly identified itself."));
    }

    #[test]
    fn test_based_on_prefix() {
        let text = "I'm based on DeepSeek-R1, a reasoning model.";
        let result = validate_model_claims(text, "qwen3:30b");
        assert!(result.contains("[System: The model incorrectly identified itself."));
    }

    #[test]
    fn test_empty_text() {
        assert_eq!(validate_model_claims("", "qwen3"), "");
    }

    #[test]
    fn test_empty_model() {
        let text = "I am Llama3.";
        assert_eq!(validate_model_claims(text, ""), text);
    }

    #[test]
    fn test_actual_model_matches_pattern() {
        // actual_model is "llama3:70b" and response says "I am Llama3"
        let text = "I am Llama3, a large language model.";
        let result = validate_model_claims(text, "llama3:70b");
        assert_eq!(result, text);
    }

    #[test]
    fn test_case_insensitive() {
        let text = "I am QWEN3-CODER and I can help with coding.";
        let result = validate_model_claims(text, "qwen3-coder:30b");
        assert_eq!(result, text);
    }

    #[test]
    fn test_model_prefix_match() {
        let text = "I'm running Qwen3:8b for this session.";
        let result = validate_model_claims(text, "qwen3:30b");
        // qwen3:8b != qwen3:30b — should flag
        assert!(result.contains("[System: The model incorrectly identified itself."));
    }

    #[test]
    fn test_multiple_claims_first_mismatch() {
        let text = "I am Mistral. Also, I'm based on Llama3.";
        let result = validate_model_claims(text, "qwen3:30b");
        assert!(result.contains("[System: The model incorrectly identified itself."));
        // Only one correction note should be appended
        assert_eq!(
            result.matches("[System:").count(),
            1,
            "Should only append one correction"
        );
    }

    #[test]
    fn test_claude_claim_on_local_model() {
        let text = "I'm Claude, made by Anthropic.";
        let result = validate_model_claims(text, "qwen3-coder:30b");
        assert!(result.contains("[System: The model incorrectly identified itself."));
    }

    #[test]
    fn test_gpt_claim_on_local_model() {
        let text = "I am GPT-4, developed by OpenAI.";
        let result = validate_model_claims(text, "qwen3:30b");
        assert!(result.contains("[System: The model incorrectly identified itself."));
    }

    #[test]
    fn test_as_a_prefix() {
        let text = "As a Llama3 model, I can explain this.";
        let result = validate_model_claims(text, "qwen3-coder:30b");
        assert!(result.contains("[System: The model incorrectly identified itself."));
    }

    #[test]
    fn test_my_name_is_prefix() {
        let text = "My name is Mistral and I'm here to help.";
        let result = validate_model_claims(text, "qwen3:30b");
        assert!(result.contains("[System: The model incorrectly identified itself."));
    }

    #[test]
    fn test_qwen35_claim_on_qwen3() {
        // qwen3.5 is a different model family than qwen3
        let text = "I am Qwen3.5, a newer model.";
        let result = validate_model_claims(text, "qwen3:30b");
        assert!(result.contains("[System: The model incorrectly identified itself."));
    }
}
