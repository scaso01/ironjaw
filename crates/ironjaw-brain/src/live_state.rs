/// Live-state query classifier.
///
/// Detects whether a user query is asking about transient, real-time state
/// (e.g. "what model is running?", "is the server up?") so that stale cached
/// memories can be filtered out before LLM context injection.
///
/// # Confidence model
/// - 1 pattern match  → 0.70
/// - 2 pattern matches → 0.85
/// - 3+ pattern matches → 0.95
///
/// # PHASE 18: optional LLM fallback
/// When config `llm_fallback = true`, an LLM call may be used to classify
/// ambiguous queries that score below the confidence threshold.
/// The fallback is NOT implemented here; this comment marks the integration point.
use regex::Regex;

/// Classification result returned by [`LiveStateClassifier::classify`].
#[derive(Debug, Clone, PartialEq)]
pub struct LiveStateClass {
    /// True if the query appears to be asking about live / real-time state.
    pub is_live: bool,
    /// Classifier confidence in [0.0, 1.0].
    pub confidence: f32,
    /// A tool that could authoritatively answer this query, if known.
    pub suggested_tool: Option<String>,
}

/// Regex-based classifier for live-state queries.
pub struct LiveStateClassifier {
    patterns: Vec<Regex>,
}

impl LiveStateClassifier {
    /// Construct a classifier with the default pattern set.
    ///
    /// Patterns are compiled once and reused for every call to [`classify`].
    pub fn new() -> Self {
        let raw = [
            // Temporal / real-time indicators
            r"(?i)\b(current(?:ly)?|right\s+now|at\s+the\s+moment|active|running|latest|fresh|live|real-?time)\b",
            // Direct capability/status questions
            r"(?i)\bwhat\s+(?:model|version|status|service|process)\b",
            // "is/are <subject> up/down/running/alive/active" — subject can be multiple words
            r"(?i)\b(?:is|are)\s+(?:\w+\s+)+(?:up|down|running|alive|active)\b",
        ];

        let patterns = raw
            .iter()
            .map(|p| Regex::new(p).expect("live_state: pattern compile failed"))
            .collect();

        Self { patterns }
    }

    /// Classify `query` and return a [`LiveStateClass`].
    pub fn classify(&self, query: &str) -> LiveStateClass {
        let matches: usize = self.patterns.iter().filter(|re| re.is_match(query)).count();

        let (is_live, confidence) = match matches {
            0 => (false, 0.0),
            1 => (true, 0.70),
            2 => (true, 0.85),
            _ => (true, 0.95),
        };

        // PHASE 18: optional LLM fallback — insert here when enabled via config.

        LiveStateClass {
            is_live,
            confidence,
            suggested_tool: None,
        }
    }
}

impl Default for LiveStateClassifier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cls() -> LiveStateClassifier {
        LiveStateClassifier::new()
    }

    #[test]
    fn test_what_model_is_running() {
        let r = cls().classify("what model is running right now?");
        assert!(r.is_live, "expected is_live=true");
        assert!(
            r.confidence > 0.7,
            "expected confidence > 0.7, got {}",
            r.confidence
        );
    }

    #[test]
    fn test_infrastructure_overview() {
        let r = cls().classify("Tell me about our infrastructure");
        assert!(!r.is_live, "expected is_live=false");
    }

    #[test]
    fn test_server_up() {
        let r = cls().classify("is the server up");
        assert!(r.is_live, "expected is_live=true");
        // Pattern 3 ("is X up") matches → at least one pattern fires
        assert!(
            r.confidence >= 0.7,
            "expected confidence >= 0.7, got {}",
            r.confidence
        );
    }

    #[test]
    fn test_cooking_dinner() {
        let r = cls().classify("I want to cook dinner");
        assert!(!r.is_live, "expected is_live=false");
    }

    #[test]
    fn test_latest_rust_release() {
        // Conservative: "latest" triggers the first pattern → accepted false positive.
        let r = cls().classify("what's the latest Rust release");
        assert!(r.is_live, "expected is_live=true (accepted false positive)");
    }

    #[test]
    fn test_confidence_model_one_match() {
        // Pattern 1 ("running") + Pattern 3 ("is X running") both fire → 2 matches = 0.85
        let r = cls().classify("is postgres running");
        assert_eq!(r.confidence, 0.85);
    }

    #[test]
    fn test_confidence_model_two_matches() {
        // Pattern 1 ("running", "right now") + Pattern 3 ("is X running") = 2 unique patterns → 0.85
        // (Pattern 1 fires once regardless of how many keywords it contains)
        let r = cls().classify("is the service running right now");
        assert_eq!(r.confidence, 0.85);
    }

    #[test]
    fn test_confidence_model_three_matches() {
        // "currently" (pattern 1), "what model" (pattern 2), "is X running" (pattern 3)
        let r = cls().classify("what model is currently running");
        assert_eq!(r.confidence, 0.95);
    }

    #[test]
    fn test_empty_query() {
        let r = cls().classify("");
        assert!(!r.is_live);
        assert_eq!(r.confidence, 0.0);
    }
}
