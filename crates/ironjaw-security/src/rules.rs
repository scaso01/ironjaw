//! Pattern-based permission rules with scoping layers.
//! CC-style: Tool(specifier) patterns like Bash(git diff *), Read(./.env)

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RuleScope {
    Local = 0,
    Project = 1,
    User = 2,
    Managed = 3,
}

#[derive(Debug, Clone)]
pub struct PermissionRule {
    pub tool_name: String,
    pub specifier: Option<String>,
    pub decision: RuleDecision,
    pub scope: RuleScope,
}

impl PermissionRule {
    pub fn parse(pattern: &str) -> Option<Self> {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return None;
        }

        if let Some(paren_pos) = pattern.find('(') {
            if pattern.ends_with(')') {
                let tool_name = pattern[..paren_pos].to_string();
                let specifier = pattern[paren_pos + 1..pattern.len() - 1].to_string();
                return Some(Self {
                    tool_name,
                    specifier: Some(specifier),
                    decision: RuleDecision::Allow, // Set by caller
                    scope: RuleScope::Local,       // Set by caller
                });
            }
        }

        Some(Self {
            tool_name: pattern.to_string(),
            specifier: None,
            decision: RuleDecision::Allow,
            scope: RuleScope::Local,
        })
    }

    /// Check if this rule matches the given tool name and arguments.
    /// Specifier uses glob-style matching: `*` matches any sequence of chars.
    pub fn matches(&self, tool_name: &str, args: Option<&str>) -> bool {
        if self.tool_name != tool_name {
            return false;
        }

        match (&self.specifier, args) {
            (None, _) => true, // No specifier = match any args
            (Some(spec), None) => spec == "*",
            (Some(spec), Some(arg_str)) => glob_match(spec, arg_str),
        }
    }
}

/// Simple glob matching: `*` matches any sequence of characters.
fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }

    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if let Some(found) = text[pos..].find(part) {
            if i == 0 && found != 0 {
                return false; // First part must match at start
            }
            pos += found + part.len();
        } else {
            return false;
        }
    }

    // If pattern ends with *, any trailing text is ok
    if pattern.ends_with('*') {
        return true;
    }
    // Otherwise text must be fully consumed
    pos == text.len()
}

pub struct RuleEngine {
    rules: Vec<PermissionRule>,
}

impl RuleEngine {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn add_rule(&mut self, scope: RuleScope, pattern: &str, decision: RuleDecision) {
        if let Some(mut rule) = PermissionRule::parse(pattern) {
            rule.scope = scope;
            rule.decision = decision;
            self.rules.push(rule);
        }
    }

    /// Evaluate rules for a tool call. Returns None if no rule matches
    /// (falls through to tier-based system). Deny always wins.
    pub fn evaluate(&self, tool_name: &str, args: Option<&str>) -> Option<RuleDecision> {
        let mut matching: Vec<&PermissionRule> = self
            .rules
            .iter()
            .filter(|r| r.matches(tool_name, args))
            .collect();

        if matching.is_empty() {
            return None;
        }

        // Deny always wins
        if matching.iter().any(|r| r.decision == RuleDecision::Deny) {
            return Some(RuleDecision::Deny);
        }

        // Sort by scope (highest first) and take first match
        matching.sort_by_key(|r| std::cmp::Reverse(r.scope));
        Some(matching[0].decision)
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

impl Default for RuleEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rule_with_specifier() {
        let rule = PermissionRule::parse("shell(git diff *)").unwrap();
        assert_eq!(rule.tool_name, "shell");
        assert_eq!(rule.specifier, Some("git diff *".to_string()));
    }

    #[test]
    fn test_parse_rule_without_specifier() {
        let rule = PermissionRule::parse("read_file").unwrap();
        assert_eq!(rule.tool_name, "read_file");
        assert_eq!(rule.specifier, None);
    }

    #[test]
    fn test_parse_empty_returns_none() {
        assert!(PermissionRule::parse("").is_none());
        assert!(PermissionRule::parse("  ").is_none());
    }

    #[test]
    fn test_rule_matches_tool_and_args() {
        let rule = PermissionRule::parse("shell(git *)").unwrap();
        assert!(rule.matches("shell", Some("git status")));
        assert!(rule.matches("shell", Some("git diff HEAD")));
        assert!(!rule.matches("shell", Some("rm -rf /")));
        assert!(!rule.matches("read_file", Some("git status")));
    }

    #[test]
    fn test_rule_without_specifier_matches_any_args() {
        let rule = PermissionRule::parse("shell").unwrap();
        assert!(rule.matches("shell", Some("anything")));
        assert!(rule.matches("shell", None));
    }

    #[test]
    fn test_rule_engine_deny_wins() {
        let mut engine = RuleEngine::new();
        engine.add_rule(RuleScope::Project, "shell(git *)", RuleDecision::Allow);
        engine.add_rule(
            RuleScope::User,
            "shell(git push --force *)",
            RuleDecision::Deny,
        );

        // Deny wins regardless of scope order
        let decision = engine.evaluate("shell", Some("git push --force origin main"));
        assert_eq!(decision, Some(RuleDecision::Deny));
    }

    #[test]
    fn test_rule_engine_scope_precedence() {
        let mut engine = RuleEngine::new();
        engine.add_rule(RuleScope::Local, "shell", RuleDecision::Allow);
        engine.add_rule(RuleScope::Managed, "shell", RuleDecision::Deny);

        // Managed > User > Project > Local — but Deny always wins
        let decision = engine.evaluate("shell", None);
        assert_eq!(decision, Some(RuleDecision::Deny));
    }

    #[test]
    fn test_rule_engine_no_match_returns_none() {
        let engine = RuleEngine::new();
        let decision = engine.evaluate("shell", None);
        assert_eq!(decision, None); // Falls through to tier-based system
    }

    #[test]
    fn test_rule_engine_ask_decision() {
        let mut engine = RuleEngine::new();
        engine.add_rule(RuleScope::Project, "write_file", RuleDecision::Ask);

        let decision = engine.evaluate("write_file", None);
        assert_eq!(decision, Some(RuleDecision::Ask));
    }

    #[test]
    fn test_rule_engine_rule_count() {
        let mut engine = RuleEngine::new();
        assert_eq!(engine.rule_count(), 0);
        engine.add_rule(RuleScope::Project, "shell(git *)", RuleDecision::Allow);
        engine.add_rule(RuleScope::User, "read_file(.env*)", RuleDecision::Deny);
        assert_eq!(engine.rule_count(), 2);
    }

    #[test]
    fn test_glob_match_exact() {
        assert!(glob_match("hello", "hello"));
        assert!(!glob_match("hello", "world"));
    }

    #[test]
    fn test_glob_match_star_suffix() {
        assert!(glob_match("git *", "git status"));
        assert!(glob_match("git *", "git diff HEAD"));
        assert!(!glob_match("git *", "rm -rf /"));
    }

    #[test]
    fn test_glob_match_star_prefix() {
        assert!(glob_match("*.env", "prod.env"));
        assert!(!glob_match("*.env", "env.txt"));
    }

    #[test]
    fn test_glob_match_star_middle() {
        assert!(glob_match("git*status", "git status"));
        assert!(glob_match("git*status", "git-long-status"));
    }

    #[test]
    fn test_scope_ordering() {
        assert!(RuleScope::Managed > RuleScope::User);
        assert!(RuleScope::User > RuleScope::Project);
        assert!(RuleScope::Project > RuleScope::Local);
    }

    #[test]
    fn test_scope_precedence_allow_vs_allow() {
        let mut engine = RuleEngine::new();
        engine.add_rule(RuleScope::Local, "shell(git *)", RuleDecision::Ask);
        engine.add_rule(RuleScope::User, "shell(git *)", RuleDecision::Allow);

        // User scope wins over Local
        let decision = engine.evaluate("shell", Some("git status"));
        assert_eq!(decision, Some(RuleDecision::Allow));
    }

    #[test]
    fn test_default() {
        let engine = RuleEngine::default();
        assert_eq!(engine.rule_count(), 0);
    }
}
