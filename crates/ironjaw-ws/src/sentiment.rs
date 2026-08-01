//! Regex-based frustration detection on user input.
//! No LLM cost — pure pattern matching.

use regex::Regex;
use std::sync::LazyLock;

static FRUSTRATION_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)\bwtf\b").unwrap(),
        Regex::new(r"(?i)\bffs\b").unwrap(),
        Regex::new(r"(?i)\bthis sucks\b").unwrap(),
        Regex::new(r"(?i)\bugh\b").unwrap(),
        Regex::new(r"(?i)\bomg\b").unwrap(),
        Regex::new(r"(?i)\bwhy (won'?t|doesn'?t|isn'?t|can'?t)").unwrap(),
        Regex::new(r"(?i)\bbroken again\b").unwrap(),
        Regex::new(r"(?i)\bstill (broken|not working|failing)\b").unwrap(),
        Regex::new(r"(?i)\bgive up\b").unwrap(),
        Regex::new(r"\?{2,}|!{3,}").unwrap(), // Multiple ?'s or !!!'s
    ]
});

static CONTINUE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)^(continue|keep going|go on|carry on|proceed)$").unwrap(),
        Regex::new(r"(?i)^(yes|y|ok|yep|sure)$").unwrap(),
    ]
});

pub fn is_frustrated(input: &str) -> bool {
    FRUSTRATION_PATTERNS.iter().any(|re| re.is_match(input))
}

pub fn is_continue_nudge(input: &str) -> bool {
    CONTINUE_PATTERNS.iter().any(|re| re.is_match(input.trim()))
}

pub struct ContinueCounter {
    count: u32,
}

impl ContinueCounter {
    pub fn new() -> Self {
        Self { count: 0 }
    }

    pub fn record(&mut self, input: &str) {
        if is_continue_nudge(input) {
            self.count += 1;
        }
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    pub fn is_stalled(&self) -> bool {
        self.count >= 3
    }

    pub fn reset(&mut self) {
        self.count = 0;
    }
}

impl Default for ContinueCounter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detects_frustration() {
        assert!(is_frustrated("wtf is going on"));
        assert!(is_frustrated("this is broken again ffs"));
        assert!(is_frustrated("ugh this sucks"));
        assert!(is_frustrated("why won't this work?!"));
    }

    #[test]
    fn test_not_frustrated_on_normal_input() {
        assert!(!is_frustrated("please help me fix the login bug"));
        assert!(!is_frustrated("can you add a new endpoint"));
        assert!(!is_frustrated("what does this function do"));
    }

    #[test]
    fn test_continue_counter() {
        let mut counter = ContinueCounter::new();
        counter.record("continue");
        counter.record("keep going");
        counter.record("go on");
        assert_eq!(counter.count(), 3);
        assert!(counter.is_stalled()); // 3+ nudges = stalled agent
    }

    #[test]
    fn test_continue_counter_ignores_non_nudge() {
        let mut counter = ContinueCounter::new();
        counter.record("write a function");
        counter.record("fix the bug");
        assert_eq!(counter.count(), 0);
        assert!(!counter.is_stalled());
    }

    #[test]
    fn test_continue_counter_reset() {
        let mut counter = ContinueCounter::new();
        counter.record("yes");
        counter.record("yes");
        counter.record("yes");
        assert!(counter.is_stalled());
        counter.reset();
        assert!(!counter.is_stalled());
        assert_eq!(counter.count(), 0);
    }

    #[test]
    fn test_exclamation_marks_detected() {
        assert!(is_frustrated("this is terrible!!!"));
        assert!(is_frustrated("what??"));
    }
}
