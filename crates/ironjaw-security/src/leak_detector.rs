use regex::Regex;

/// Action to take when a leak is detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeakAction {
    /// Block the content entirely.
    Block,
    /// Redact the matched content.
    Redact,
    /// Warn but allow through.
    Warn,
}

/// A single finding from a leak scan.
#[derive(Debug, Clone)]
pub struct LeakFinding {
    /// Name of the pattern that matched.
    pub pattern_name: String,
    /// Preview of matched content (first 8 chars + "...").
    pub matched: String,
    /// Byte position in the scanned text.
    pub position: usize,
}

/// Result of a leak scan.
#[derive(Debug, Clone)]
pub struct LeakResult {
    /// Whether any leaks were detected.
    pub detected: bool,
    /// Individual findings.
    pub findings: Vec<LeakFinding>,
    /// Recommended action based on the most severe finding.
    pub action: LeakAction,
}

impl LeakResult {
    fn clean() -> Self {
        Self {
            detected: false,
            findings: Vec::new(),
            action: LeakAction::Warn,
        }
    }
}

/// A compiled pattern for detecting a specific type of secret.
struct LeakPattern {
    name: &'static str,
    regex: Regex,
    severity: LeakAction,
}

/// Detects secrets and credentials in text before sending to LLM or in tool output.
pub struct LeakDetector {
    patterns: Vec<LeakPattern>,
    default_action: LeakAction,
}

impl std::fmt::Debug for LeakDetector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeakDetector")
            .field("pattern_count", &self.patterns.len())
            .field("default_action", &self.default_action)
            .finish()
    }
}

impl LeakDetector {
    /// Create a new leak detector with all built-in patterns.
    pub fn new(default_action: LeakAction) -> Self {
        let patterns = Self::build_patterns();
        Self {
            patterns,
            default_action,
        }
    }

    /// Scan text for leaked secrets/credentials.
    pub fn scan(&self, text: &str) -> LeakResult {
        let mut findings = Vec::new();
        let mut worst_action = &self.default_action;

        for pattern in &self.patterns {
            for m in pattern.regex.find_iter(text) {
                let matched_text = m.as_str();
                let preview = if matched_text.len() > 8 {
                    format!("{}...", &matched_text[..8])
                } else {
                    matched_text.to_string()
                };

                findings.push(LeakFinding {
                    pattern_name: pattern.name.to_string(),
                    matched: preview,
                    position: m.start(),
                });

                // Track the most severe action
                worst_action = most_severe(worst_action, &pattern.severity);
            }
        }

        if findings.is_empty() {
            return LeakResult::clean();
        }

        tracing::warn!(
            count = findings.len(),
            "Leak detector found {} potential secrets",
            findings.len()
        );

        LeakResult {
            detected: true,
            action: worst_action.clone(),
            findings,
        }
    }

    /// Build all built-in detection patterns.
    fn build_patterns() -> Vec<LeakPattern> {
        let defs: Vec<(&str, &str, LeakAction)> = vec![
            // AWS
            ("aws_access_key", r"AKIA[0-9A-Z]{16}", LeakAction::Block),
            // GitHub tokens
            ("github_pat", r"gh[ps]_[A-Za-z0-9_]{36,}", LeakAction::Block),
            ("github_oauth", r"gho_[A-Za-z0-9_]{36,}", LeakAction::Block),
            // JWT
            (
                "jwt",
                r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
                LeakAction::Warn,
            ),
            // PEM private keys
            (
                "pem_private_key",
                r"-----BEGIN (RSA |EC |DSA )?PRIVATE KEY-----",
                LeakAction::Block,
            ),
            // SSH private key
            (
                "ssh_private_key",
                r"-----BEGIN OPENSSH PRIVATE KEY-----",
                LeakAction::Block,
            ),
            // Database URLs
            (
                "database_url",
                r"(postgres|mysql|mongodb)://[^\s]+",
                LeakAction::Redact,
            ),
            // Slack tokens
            (
                "slack_token",
                r"xox[bpras]-[A-Za-z0-9-]+",
                LeakAction::Block,
            ),
            // Generic API keys
            (
                "generic_api_key",
                r#"(api[_\-]?key|apikey|secret[_\-]?key)['"\s:=]+[A-Za-z0-9_\-]{20,}"#,
                LeakAction::Redact,
            ),
            // Bearer tokens
            (
                "bearer_token",
                r"Bearer [A-Za-z0-9_\-\.]{20,}",
                LeakAction::Redact,
            ),
            // Anthropic API key
            ("anthropic_key", r"sk-ant-[A-Za-z0-9_-]+", LeakAction::Block),
            // OpenAI API key
            ("openai_key", r"sk-[A-Za-z0-9]{32,}", LeakAction::Block),
            // Google API key
            (
                "google_api_key",
                r"AIza[A-Za-z0-9_\-]{35}",
                LeakAction::Block,
            ),
            // Stripe keys
            (
                "stripe_key",
                r"[rs]k_(test|live)_[A-Za-z0-9]{24,}",
                LeakAction::Block,
            ),
            // Twilio
            ("twilio_key", r"SK[a-f0-9]{32}", LeakAction::Block),
            // SendGrid
            (
                "sendgrid_key",
                r"SG\.[A-Za-z0-9_\-]{22,}\.[A-Za-z0-9_\-]{43,}",
                LeakAction::Block,
            ),
            // npm token
            ("npm_token", r"npm_[A-Za-z0-9]{36}", LeakAction::Block),
            // PyPI token
            ("pypi_token", r"pypi-[A-Za-z0-9]{36,}", LeakAction::Block),
            // Hex high-entropy (40+ hex chars that look like secrets)
            (
                "hex_high_entropy",
                r"(?i)(?:secret|token|password|key|auth)[_\-\s]*[:=]\s*[0-9a-f]{40,}",
                LeakAction::Redact,
            ),
            // Private key base64 blocks (long base64 after key header)
            (
                "base64_private_key",
                r"(?:PRIVATE KEY-----\s*\n?)([A-Za-z0-9+/=]{64,})",
                LeakAction::Block,
            ),
            // Azure connection strings
            (
                "azure_connection",
                r"DefaultEndpointsProtocol=https;AccountName=[^;]+;AccountKey=[A-Za-z0-9+/=]+",
                LeakAction::Block,
            ),
            // Heroku API key
            (
                "heroku_api_key",
                r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
                LeakAction::Warn,
            ),
        ];

        defs.into_iter()
            .filter_map(|(name, pattern, severity)| match Regex::new(pattern) {
                Ok(regex) => Some(LeakPattern {
                    name,
                    regex,
                    severity,
                }),
                Err(e) => {
                    tracing::warn!(pattern = name, error = %e, "Failed to compile leak pattern");
                    None
                }
            })
            .collect()
    }
}

/// Return the more severe of two actions.
fn most_severe<'a>(a: &'a LeakAction, b: &'a LeakAction) -> &'a LeakAction {
    match (a, b) {
        (LeakAction::Block, _) | (_, LeakAction::Block) => {
            if matches!(a, LeakAction::Block) {
                a
            } else {
                b
            }
        }
        (LeakAction::Redact, _) | (_, LeakAction::Redact) => {
            if matches!(a, LeakAction::Redact) {
                a
            } else {
                b
            }
        }
        _ => a,
    }
}

impl Clone for LeakDetector {
    fn clone(&self) -> Self {
        Self::new(self.default_action.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> LeakDetector {
        LeakDetector::new(LeakAction::Warn)
    }

    #[test]
    fn test_clean_text() {
        let d = detector();
        let result = d.scan("Hello world, this is a normal message.");
        assert!(!result.detected);
        assert!(result.findings.is_empty());
    }

    #[test]
    fn test_aws_access_key() {
        let d = detector();
        let result = d.scan("My key is AKIAIOSFODNN7EXAMPLE");
        assert!(result.detected);
        assert_eq!(result.findings[0].pattern_name, "aws_access_key");
        assert_eq!(result.action, LeakAction::Block);
    }

    #[test]
    fn test_github_pat() {
        let d = detector();
        let result = d.scan("token: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij");
        assert!(result.detected);
        assert_eq!(result.findings[0].pattern_name, "github_pat");
    }

    #[test]
    fn test_github_secret() {
        let d = detector();
        let result = d.scan("secret: ghs_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij");
        assert!(result.detected);
        assert_eq!(result.findings[0].pattern_name, "github_pat");
    }

    #[test]
    fn test_github_oauth() {
        let d = detector();
        let result = d.scan("oauth: gho_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij");
        assert!(result.detected);
        assert_eq!(result.findings[0].pattern_name, "github_oauth");
    }

    #[test]
    fn test_jwt() {
        let d = detector();
        let result = d.scan(
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        );
        assert!(result.detected);
        assert_eq!(result.findings[0].pattern_name, "jwt");
    }

    #[test]
    fn test_pem_private_key() {
        let d = detector();
        let result = d.scan("-----BEGIN RSA PRIVATE KEY-----\nMIIEpA...");
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "pem_private_key"));
        assert_eq!(result.action, LeakAction::Block);
    }

    #[test]
    fn test_ssh_private_key() {
        let d = detector();
        let result = d.scan("-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blbn...");
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "ssh_private_key"));
    }

    #[test]
    fn test_database_url_postgres() {
        let d = detector();
        let result = d.scan("DATABASE_URL=postgres://user:pass@db.host:5432/mydb");
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "database_url"));
    }

    #[test]
    fn test_database_url_mongodb() {
        let d = detector();
        let result = d.scan("mongodb://admin:secret@mongo.host:27017/test");
        assert!(result.detected);
    }

    #[test]
    fn test_slack_token() {
        let d = detector();
        let result = d.scan("SLACK_TOKEN=xoxb-123456789012-1234567890123-AbCdEfGhIj");
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "slack_token"));
    }

    #[test]
    fn test_generic_api_key() {
        let d = detector();
        let result = d.scan(r#"api_key = "abcdefghijklmnopqrstuvwxyz1234""#);
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "generic_api_key"));
    }

    #[test]
    fn test_bearer_token() {
        let d = detector();
        let result = d.scan("Authorization: Bearer eyJhbGciOiJSUzI1NiIsInR5c.long-token-value");
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "bearer_token"));
    }

    #[test]
    fn test_anthropic_key() {
        let d = detector();
        let result = d.scan("ANTHROPIC_API_KEY=sk-ant-api03-abc123def456");
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "anthropic_key"));
        assert_eq!(result.action, LeakAction::Block);
    }

    #[test]
    fn test_openai_key() {
        let d = detector();
        let result = d.scan("OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz123456");
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "openai_key"));
    }

    #[test]
    fn test_google_api_key() {
        let d = detector();
        let result = d.scan("GOOGLE_API_KEY=AIzaSyA1234567890abcdefghijklmnopqrstuv");
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "google_api_key"));
    }

    #[test]
    fn test_stripe_key() {
        let d = detector();
        let result = d.scan("sk_test_abcdefghijklmnopqrstuvwxyz");
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "stripe_key"));
    }

    #[test]
    fn test_twilio_key() {
        let d = detector();
        let result = d.scan("TWILIO_KEY=SK0123456789abcdef0123456789abcdef");
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "twilio_key"));
    }

    #[test]
    fn test_sendgrid_key() {
        let d = detector();
        let result =
            d.scan("SG.abcdefghijklmnopqrstuv.ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuv-");
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "sendgrid_key"));
    }

    #[test]
    fn test_npm_token() {
        let d = detector();
        let result = d.scan("npm_abcdefghijklmnopqrstuvwxyz0123456789");
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "npm_token"));
    }

    #[test]
    fn test_pypi_token() {
        let d = detector();
        let result = d.scan("pypi-abcdefghijklmnopqrstuvwxyz0123456789AB");
        assert!(result.detected);
        assert!(result
            .findings
            .iter()
            .any(|f| f.pattern_name == "pypi_token"));
    }

    #[test]
    fn test_hex_high_entropy() {
        let d = detector();
        let result =
            d.scan("secret_key=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        assert!(result.detected);
    }

    #[test]
    fn test_multiple_findings() {
        let d = detector();
        let text = "AWS: AKIAIOSFODNN7EXAMPLE and github: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let result = d.scan(text);
        assert!(result.detected);
        assert!(result.findings.len() >= 2);
        assert_eq!(result.action, LeakAction::Block);
    }

    #[test]
    fn test_finding_preview_truncation() {
        let d = detector();
        let result = d.scan("AKIAIOSFODNN7EXAMPLE");
        assert!(result.detected);
        let finding = &result.findings[0];
        // Preview should be first 8 chars + "..."
        assert!(finding.matched.len() <= 11);
        assert!(finding.matched.ends_with("..."));
    }

    #[test]
    fn test_severity_escalation() {
        // Warn is the default, but Block pattern should escalate
        let d = LeakDetector::new(LeakAction::Warn);
        let result = d.scan("AKIAIOSFODNN7EXAMPLE");
        assert_eq!(result.action, LeakAction::Block);
    }

    #[test]
    fn test_ec_private_key() {
        let d = detector();
        let result = d.scan("-----BEGIN EC PRIVATE KEY-----");
        assert!(result.detected);
    }

    #[test]
    fn test_dsa_private_key() {
        let d = detector();
        let result = d.scan("-----BEGIN DSA PRIVATE KEY-----");
        assert!(result.detected);
    }

    #[test]
    fn test_generic_private_key() {
        let d = detector();
        let result = d.scan("-----BEGIN PRIVATE KEY-----");
        assert!(result.detected);
    }
}
