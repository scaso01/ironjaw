//! Fast pre-classifier for permission decisions. Uses a cheap/fast model
//! to screen tool calls before escalating to HITL. Deliberately excludes
//! the agent's reasoning to prevent prompt injection via persuasion.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClassifierDecision {
    Allow,    // Safe to proceed
    Deny,     // Clearly dangerous
    Escalate, // Ambiguous — send to HITL
}

/// Build classifier prompt — deliberately excludes agent reasoning.
/// Only includes tool name and parameters. This prevents the agent
/// from persuading the classifier to approve dangerous operations.
pub fn build_classifier_prompt(tool_name: &str, params: &serde_json::Value) -> String {
    format!(
        "You are a security classifier. Evaluate whether this tool call is safe.\n\
         Respond with exactly one word: ALLOW, DENY, or ESCALATE.\n\n\
         Tool: {tool_name}\n\
         Parameters: {params}\n\n\
         Rules:\n\
         - ALLOW: Read-only operations, safe file paths, non-destructive commands\n\
         - DENY: Destructive operations (rm -rf, DROP TABLE, force push), accessing credentials\n\
         - ESCALATE: Ambiguous or context-dependent operations\n\n\
         Decision:"
    )
}

pub fn parse_response(response: &str) -> ClassifierDecision {
    let upper = response.trim().to_uppercase();
    if upper.starts_with("ALLOW") {
        ClassifierDecision::Allow
    } else if upper.starts_with("DENY") {
        ClassifierDecision::Deny
    } else {
        ClassifierDecision::Escalate
    }
}

/// Trait for LLM classify calls — allows mocking in tests.
#[async_trait::async_trait]
pub trait LlmClassifyFn: Send + Sync {
    async fn classify(&self, model: &str, prompt: &str) -> Result<String, String>;
}

/// Classifier that uses a fast/cheap model for pre-screening.
pub struct PermissionClassifier {
    pub enabled: bool,
    pub model: String,   // e.g., "haiku" or fast local model
    pub timeout_ms: u64, // Fast timeout — classifier must be quick
}

impl PermissionClassifier {
    pub fn new(model: &str, timeout_ms: u64) -> Self {
        Self {
            enabled: true,
            model: model.to_string(),
            timeout_ms,
        }
    }

    /// Pre-classify a tool call. Returns None if classifier is disabled or times out.
    pub async fn classify(
        &self,
        tool_name: &str,
        params: &serde_json::Value,
        llm: &dyn LlmClassifyFn,
    ) -> Option<ClassifierDecision> {
        if !self.enabled {
            return None;
        }

        let prompt = build_classifier_prompt(tool_name, params);

        match tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms),
            llm.classify(&self.model, &prompt),
        )
        .await
        {
            Ok(Ok(response)) => Some(parse_response(&response)),
            Ok(Err(_)) | Err(_) => None, // Error or timeout — fall through to HITL
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classifier_prompt_excludes_agent_reasoning() {
        let prompt = build_classifier_prompt("shell", &serde_json::json!({"command": "rm -rf /"}));
        assert!(prompt.contains("shell"));
        assert!(prompt.contains("rm -rf /"));
        assert!(!prompt.contains("reasoning"));
        assert!(!prompt.contains("because"));
    }

    #[test]
    fn test_parse_classifier_response_allow() {
        assert_eq!(parse_response("ALLOW"), ClassifierDecision::Allow);
        assert_eq!(
            parse_response("allow - safe read operation"),
            ClassifierDecision::Allow
        );
    }

    #[test]
    fn test_parse_classifier_response_deny() {
        assert_eq!(parse_response("DENY"), ClassifierDecision::Deny);
        assert_eq!(
            parse_response("DENY - destructive operation"),
            ClassifierDecision::Deny
        );
    }

    #[test]
    fn test_parse_classifier_response_escalate() {
        assert_eq!(parse_response("ESCALATE"), ClassifierDecision::Escalate);
        assert_eq!(
            parse_response("unclear intent"),
            ClassifierDecision::Escalate
        );
    }

    #[test]
    fn test_parse_classifier_empty_string() {
        assert_eq!(parse_response(""), ClassifierDecision::Escalate);
    }

    struct MockLlm {
        response: String,
    }

    #[async_trait::async_trait]
    impl LlmClassifyFn for MockLlm {
        async fn classify(&self, _model: &str, _prompt: &str) -> Result<String, String> {
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn test_classifier_with_mock_llm() {
        let classifier = PermissionClassifier::new("haiku", 5000);
        let llm = MockLlm {
            response: "ALLOW".to_string(),
        };
        let result = classifier
            .classify("read_file", &serde_json::json!({"path": "/tmp/test"}), &llm)
            .await;
        assert_eq!(result, Some(ClassifierDecision::Allow));
    }

    #[tokio::test]
    async fn test_classifier_disabled_returns_none() {
        let mut classifier = PermissionClassifier::new("haiku", 5000);
        classifier.enabled = false;
        let llm = MockLlm {
            response: "ALLOW".to_string(),
        };
        let result = classifier
            .classify("read_file", &serde_json::json!({"path": "/tmp/test"}), &llm)
            .await;
        assert!(result.is_none());
    }

    struct FailingLlm;

    #[async_trait::async_trait]
    impl LlmClassifyFn for FailingLlm {
        async fn classify(&self, _model: &str, _prompt: &str) -> Result<String, String> {
            Err("LLM error".to_string())
        }
    }

    #[tokio::test]
    async fn test_classifier_error_returns_none() {
        let classifier = PermissionClassifier::new("haiku", 5000);
        let llm = FailingLlm;
        let result = classifier
            .classify("shell", &serde_json::json!({"command": "ls"}), &llm)
            .await;
        assert!(result.is_none());
    }
}
