use serde::{Deserialize, Serialize};

/// Result of a verification check on a generated response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    /// Whether the response passed verification.
    pub passed: bool,
    /// Specific discrepancies found, if any.
    pub discrepancies: Vec<String>,
    /// The model that performed verification.
    pub verifier_model: String,
    /// Raw verifier output for debugging.
    pub raw_output: String,
}

/// Configuration for the independent verification stage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerificationConfig {
    /// Whether verification is globally enabled.
    pub enabled: bool,
    /// The model to use for verification. If None, uses the router's default.
    pub verifier_model: Option<String>,
    /// Skill names that should have verification enabled.
    /// Empty = verification disabled for all skills (unless globally forced).
    pub enabled_skills: Vec<String>,
    /// Maximum number of regeneration attempts if verification fails.
    /// 0 = report discrepancies but don't regenerate.
    pub max_retries: u32,
}

impl VerificationConfig {
    /// Check if verification is enabled for a given skill.
    pub fn is_enabled_for(&self, skill_name: Option<&str>) -> bool {
        if !self.enabled {
            return false;
        }
        match skill_name {
            Some(name) => self.enabled_skills.iter().any(|s| s == name),
            None => false,
        }
    }
}

/// Builds the verification prompt that the verifier LLM will receive.
///
/// The verifier sees ONLY the original request + generated response + constraints.
/// It does NOT see planning/reasoning from the planner phase.
pub fn build_verification_prompt(
    original_request: &str,
    generated_response: &str,
    constraints: Option<&str>,
) -> String {
    let mut prompt = String::with_capacity(original_request.len() + generated_response.len() + 512);

    prompt.push_str(
        "You are a response verifier. Your job is to check if the generated response \
         correctly addresses the original request and follows all constraints.\n\n",
    );

    prompt.push_str("## Original Request\n");
    prompt.push_str(original_request);
    prompt.push_str("\n\n## Generated Response\n");
    prompt.push_str(generated_response);

    if let Some(constraints) = constraints {
        prompt.push_str("\n\n## Constraints\n");
        prompt.push_str(constraints);
    }

    prompt.push_str(
        "\n\n## Instructions\n\
         Evaluate whether the response correctly and completely addresses the request. \
         Check for:\n\
         1. Factual accuracy (to the best of your knowledge)\n\
         2. Completeness — does it address all parts of the request?\n\
         3. Constraint adherence — does it follow all specified constraints?\n\
         4. Coherence — is the response internally consistent?\n\n\
         Respond in this exact format:\n\
         VERDICT: PASS or FAIL\n\
         DISCREPANCIES:\n\
         - (list each issue, or \"none\" if PASS)\n",
    );

    prompt
}

/// Parse the verifier's output into a structured result.
pub fn parse_verification_output(output: &str, verifier_model: &str) -> VerificationResult {
    let output_upper = output.to_uppercase();

    // Look for VERDICT line
    let passed = if let Some(verdict_line) = output_upper
        .lines()
        .find(|l| l.trim().starts_with("VERDICT:"))
    {
        verdict_line.contains("PASS")
    } else {
        // If no VERDICT line, assume pass if no FAIL keyword found
        !output_upper.contains("FAIL")
    };

    // Extract discrepancies
    let discrepancies = extract_discrepancies(output);

    VerificationResult {
        passed,
        discrepancies,
        verifier_model: verifier_model.to_string(),
        raw_output: output.to_string(),
    }
}

/// Extract discrepancy bullet points from verifier output.
fn extract_discrepancies(output: &str) -> Vec<String> {
    let mut discrepancies = Vec::new();
    let mut in_discrepancies = false;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.to_uppercase().starts_with("DISCREPANCIES:") {
            in_discrepancies = true;
            continue;
        }
        if in_discrepancies {
            if trimmed.starts_with('-') || trimmed.starts_with('*') {
                let text = trimmed.trim_start_matches(['-', '*', ' ']).trim();
                if !text.is_empty() && text.to_lowercase() != "none" && text.to_lowercase() != "n/a"
                {
                    discrepancies.push(text.to_string());
                }
            } else if !trimmed.is_empty() && !trimmed.starts_with('-') && !trimmed.starts_with('*')
            {
                // End of discrepancy section
                break;
            }
        }
    }

    discrepancies
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verification_config_default() {
        let config = VerificationConfig::default();
        assert!(!config.enabled);
        assert!(config.verifier_model.is_none());
        assert!(config.enabled_skills.is_empty());
        assert_eq!(config.max_retries, 0);
    }

    #[test]
    fn test_is_enabled_for_disabled_globally() {
        let config = VerificationConfig {
            enabled: false,
            verifier_model: None,
            enabled_skills: vec!["coding".to_string()],
            max_retries: 1,
        };
        assert!(!config.is_enabled_for(Some("coding")));
        assert!(!config.is_enabled_for(None));
    }

    #[test]
    fn test_is_enabled_for_specific_skill() {
        let config = VerificationConfig {
            enabled: true,
            verifier_model: None,
            enabled_skills: vec!["coding".to_string(), "analysis".to_string()],
            max_retries: 1,
        };
        assert!(config.is_enabled_for(Some("coding")));
        assert!(config.is_enabled_for(Some("analysis")));
        assert!(!config.is_enabled_for(Some("chat")));
        assert!(!config.is_enabled_for(None));
    }

    #[test]
    fn test_build_verification_prompt_without_constraints() {
        let prompt = build_verification_prompt("What is 2+2?", "2+2 equals 4.", None);
        assert!(prompt.contains("## Original Request"));
        assert!(prompt.contains("What is 2+2?"));
        assert!(prompt.contains("## Generated Response"));
        assert!(prompt.contains("2+2 equals 4."));
        assert!(!prompt.contains("## Constraints"));
        assert!(prompt.contains("VERDICT: PASS or FAIL"));
    }

    #[test]
    fn test_build_verification_prompt_with_constraints() {
        let prompt = build_verification_prompt(
            "Write a poem",
            "Roses are red...",
            Some("Must be exactly 4 lines"),
        );
        assert!(prompt.contains("## Constraints"));
        assert!(prompt.contains("Must be exactly 4 lines"));
    }

    #[test]
    fn test_parse_verification_pass() {
        let output = "VERDICT: PASS\nDISCREPANCIES:\n- none\n";
        let result = parse_verification_output(output, "test-model");
        assert!(result.passed);
        assert!(result.discrepancies.is_empty());
        assert_eq!(result.verifier_model, "test-model");
    }

    #[test]
    fn test_parse_verification_fail() {
        let output = "VERDICT: FAIL\nDISCREPANCIES:\n- Missing the second part of the question\n- Factual error about dates\n";
        let result = parse_verification_output(output, "verifier");
        assert!(!result.passed);
        assert_eq!(result.discrepancies.len(), 2);
        assert_eq!(
            result.discrepancies[0],
            "Missing the second part of the question"
        );
        assert_eq!(result.discrepancies[1], "Factual error about dates");
    }

    #[test]
    fn test_parse_verification_no_verdict_line_defaults_pass() {
        let output = "The response looks good and addresses the request correctly.";
        let result = parse_verification_output(output, "model");
        assert!(result.passed);
        assert!(result.discrepancies.is_empty());
    }

    #[test]
    fn test_parse_verification_no_verdict_with_fail_keyword() {
        let output = "The response FAIL to address the core question.";
        let result = parse_verification_output(output, "model");
        assert!(!result.passed);
    }

    #[test]
    fn test_parse_discrepancies_asterisk_bullets() {
        let output = "VERDICT: FAIL\nDISCREPANCIES:\n* Issue one\n* Issue two\n";
        let result = parse_verification_output(output, "model");
        assert_eq!(result.discrepancies.len(), 2);
        assert_eq!(result.discrepancies[0], "Issue one");
        assert_eq!(result.discrepancies[1], "Issue two");
    }

    #[test]
    fn test_parse_discrepancies_stops_at_non_bullet() {
        let output = "VERDICT: FAIL\nDISCREPANCIES:\n- Issue one\nSome other text\n- This should not be included\n";
        let result = parse_verification_output(output, "model");
        assert_eq!(result.discrepancies.len(), 1);
        assert_eq!(result.discrepancies[0], "Issue one");
    }

    #[test]
    fn test_verification_result_serialization() {
        let result = VerificationResult {
            passed: true,
            discrepancies: vec![],
            verifier_model: "test".to_string(),
            raw_output: "VERDICT: PASS".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: VerificationResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.passed);
        assert!(parsed.discrepancies.is_empty());
    }

    #[test]
    fn test_verification_config_serialization() {
        let config = VerificationConfig {
            enabled: true,
            verifier_model: Some("qwen3:8b".to_string()),
            enabled_skills: vec!["coding".to_string()],
            max_retries: 2,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: VerificationConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.verifier_model.as_deref(), Some("qwen3:8b"));
        assert_eq!(parsed.enabled_skills, vec!["coding"]);
        assert_eq!(parsed.max_retries, 2);
    }

    #[test]
    fn test_verification_result_with_discrepancies_serialization() {
        let result = VerificationResult {
            passed: false,
            discrepancies: vec!["Error 1".to_string(), "Error 2".to_string()],
            verifier_model: "verifier".to_string(),
            raw_output: "VERDICT: FAIL\n".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"passed\":false"));
        assert!(json.contains("Error 1"));
        assert!(json.contains("Error 2"));
    }

    #[test]
    fn test_parse_verdict_case_insensitive() {
        let output = "verdict: pass\ndiscrepancies:\n- none\n";
        let result = parse_verification_output(output, "model");
        assert!(result.passed);
    }

    #[test]
    fn test_parse_verdict_with_whitespace() {
        let output = "  VERDICT:  PASS  \nDISCREPANCIES:\n- none\n";
        let result = parse_verification_output(output, "model");
        assert!(result.passed);
    }

    #[test]
    fn test_parse_na_discrepancies_ignored() {
        let output = "VERDICT: PASS\nDISCREPANCIES:\n- N/A\n";
        let result = parse_verification_output(output, "model");
        assert!(result.discrepancies.is_empty());
    }

    #[test]
    fn test_empty_output_defaults_pass() {
        let result = parse_verification_output("", "model");
        assert!(result.passed);
        assert!(result.discrepancies.is_empty());
    }
}
