//! Tool nudging / outcome gate for LLM responses.
//!
//! When an LLM responds with text-only instead of using available tools,
//! the `ToolNudger` can detect this and generate a nudge message to retry
//! the request with a hint to use tools.

/// Nudges the LLM to use tools when it responds with text instead of tool calls.
#[derive(Debug, Clone)]
pub struct ToolNudger {
    /// Maximum number of nudge retries before accepting the text response
    max_retries: usize,
}

/// Decision on whether to nudge the LLM to use tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NudgeDecision {
    /// Response is acceptable, no nudge needed
    Accept,
    /// Response should be retried with a nudge message appended
    Nudge {
        /// The nudge message to append to the conversation
        message: String,
    },
    /// Maximum retries exceeded, accept the text response as-is
    MaxRetriesExceeded,
}

impl ToolNudger {
    /// Create a new nudger with the given maximum retry count.
    pub fn new(max_retries: usize) -> Self {
        Self { max_retries }
    }

    /// Check if a response needs nudging.
    ///
    /// A response needs nudging when:
    /// - Available tools list is non-empty (tools were provided but not used)
    /// - The response contains no JSON tool calls
    /// - The response appears conversational/explanatory rather than action-oriented
    /// - The retry count has not exceeded the maximum
    pub fn needs_nudge(
        &self,
        response: &str,
        available_tools: &[String],
        retry_count: usize,
    ) -> NudgeDecision {
        // No tools available -- nothing to nudge toward
        if available_tools.is_empty() {
            return NudgeDecision::Accept;
        }

        // A response that already contains tool-call indicators is an answer.
        if self.contains_tool_call(response) {
            return NudgeDecision::Accept;
        }

        // Conversational (describes an action instead of taking it) → nudge.
        // FIX #7: report MaxRetriesExceeded ONLY for a response that still needs a
        // nudge but has run out of retries — that is the signal the caller uses to
        // stop nudging and force a synthesis pass. A genuine answer that arrives
        // after the retries are spent is accepted here, not discarded.
        if self.is_conversational(response) {
            if retry_count >= self.max_retries {
                return NudgeDecision::MaxRetriesExceeded;
            }
            return NudgeDecision::Nudge {
                message: self.nudge_message(available_tools),
            };
        }

        NudgeDecision::Accept
    }

    /// Generate the nudge message to append to the conversation.
    pub fn nudge_message(&self, available_tools: &[String]) -> String {
        let tool_list = available_tools.join(", ");
        format!(
            "You have tools available ({tool_list}). \
             Please use the appropriate tool to complete this task \
             rather than describing what you would do."
        )
    }

    /// Check if the response contains JSON tool call patterns.
    fn contains_tool_call(&self, response: &str) -> bool {
        // Look for common tool call JSON patterns
        // Pattern: {"name": "...", or {"tool": "...",
        let patterns = [
            r#""name""#,
            r#""tool""#,
            r#""function""#,
            r#""tool_call""#,
            r#""tool_use""#,
        ];

        // Must also have something that looks like JSON structure
        let has_json = response.contains('{') && response.contains('}');
        if !has_json {
            return false;
        }

        patterns.iter().any(|p| response.contains(p))
    }

    /// Check if the response is conversational/explanatory.
    ///
    /// Heuristic: responses that describe what could be done rather than doing it.
    fn is_conversational(&self, response: &str) -> bool {
        let conversational_markers = [
            "I would ",
            "I can ",
            "I'll ",
            "I will ",
            "I need to ",
            "I should ",
            "I'm going to ",
            "Let me check",
            "Let me look",
            "Let me find",
            "Let me search",
            "Let me see",
            "Let me explain",
            // Action verbs the model announces but doesn't act on (session a5822fe7,
            // "Let me read the device specs directly via ADB." stopped with no tool call).
            "Let me read",
            "Let me get",
            "Let me pull",
            "Let me run",
            "Let me grab",
            "Let me fetch",
            "Let me query",
            "Let me retrieve",
            "Let me gather",
            "Let me inspect",
            "Let me use ",
            "Let me try",
            "You can ",
            "You could ",
            "You should ",
            "Here's how ",
            "Here is how ",
            "To do this",
            "The steps are",
            "First, ",
            "Would you like me to",
            "I could ",
            "I'd recommend",
        ];

        let lower = response.to_lowercase();
        conversational_markers
            .iter()
            .any(|marker| lower.contains(&marker.to_lowercase()))
    }
}

impl Default for ToolNudger {
    fn default() -> Self {
        Self { max_retries: 3 }
    }
}

/// Web-search tool names that should NOT be the first call when the user's
/// prompt references a local project folder.
pub const WEB_SEARCH_TOOLS: &[&str] = &[
    "monster_search",
    "web_search",
    "searxng",
    "vane",
    "fyin_search",
    "khoj_query",
    "zoekt_search",
];

/// Detect a "wrong tool" choice: LLM picked a web-search tool but the user
/// asked about something whose name matches a folder under `working_dir`.
///
/// Returns a nudge message to inject as a user-role re-prompt, or `None` if
/// the tool choice looks fine.
///
/// `local_folder_names` should be the list of subdirectory names directly
/// inside the session's working directory (caller enumerates this once per
/// turn). Folder names shorter than 3 chars are ignored to avoid false
/// positives on words like "ai", "go", "rs".
pub fn wrong_tool_nudge(
    user_prompt: &str,
    first_tool: &str,
    local_folder_names: &[String],
) -> Option<String> {
    if !WEB_SEARCH_TOOLS.contains(&first_tool) {
        return None;
    }
    let lower_prompt = user_prompt.to_lowercase();
    for name in local_folder_names {
        if name.len() < 3 {
            continue;
        }
        let lower = name.to_lowercase();
        let matches = lower_prompt
            .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
            .any(|tok| tok == lower);
        if matches {
            return Some(format!(
                "That looks like a LOCAL folder named `{name}` under your working directory, \
                 not a web search topic. Use `list_files` or `read_file` to inspect the local \
                 project first. `{first_tool}` is for genuinely external/public-web questions \
                 (news, public docs, prices). Retry with the correct tool."
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<String> {
        vec![
            "read_file".to_string(),
            "write_file".to_string(),
            "shell".to_string(),
        ]
    }

    #[test]
    fn test_accept_when_no_tools() {
        let nudger = ToolNudger::new(2);
        let decision = nudger.needs_nudge("Here is some text", &[], 0);
        assert_eq!(decision, NudgeDecision::Accept);
    }

    #[test]
    fn test_accept_when_tool_call_present() {
        let nudger = ToolNudger::new(2);
        let response = r#"{"name": "read_file", "params": {"path": "/tmp/test"}}"#;
        let decision = nudger.needs_nudge(response, &tools(), 0);
        assert_eq!(decision, NudgeDecision::Accept);
    }

    #[test]
    fn test_nudge_conversational_response() {
        let nudger = ToolNudger::new(2);
        let response = "I would read the file at /tmp/test to check its contents.";
        let decision = nudger.needs_nudge(response, &tools(), 0);
        assert!(matches!(decision, NudgeDecision::Nudge { .. }));
    }

    #[test]
    fn test_nudge_describes_steps() {
        let nudger = ToolNudger::new(2);
        let response = "Here's how to solve this: First, read the config file.";
        let decision = nudger.needs_nudge(response, &tools(), 0);
        assert!(matches!(decision, NudgeDecision::Nudge { .. }));
    }

    #[test]
    fn test_max_retries_exceeded() {
        let nudger = ToolNudger::new(2);
        let response = "I would read the file.";
        let decision = nudger.needs_nudge(response, &tools(), 2);
        assert_eq!(decision, NudgeDecision::MaxRetriesExceeded);
    }

    #[test]
    fn test_max_retries_exceeded_at_boundary() {
        let nudger = ToolNudger::new(2);
        let response = "I would read the file.";

        // retry 0 and 1 should nudge
        assert!(matches!(
            nudger.needs_nudge(response, &tools(), 0),
            NudgeDecision::Nudge { .. }
        ));
        assert!(matches!(
            nudger.needs_nudge(response, &tools(), 1),
            NudgeDecision::Nudge { .. }
        ));
        // retry 2 should exceed
        assert_eq!(
            nudger.needs_nudge(response, &tools(), 2),
            NudgeDecision::MaxRetriesExceeded
        );
    }

    #[test]
    fn test_real_answer_after_retries_spent_is_accepted_not_discarded() {
        // FIX #7 regression: MaxRetriesExceeded must reflect a STILL-conversational
        // non-answer, not merely a spent retry budget. A genuine answer that lands
        // after the nudges are used up is accepted so the caller keeps it.
        let nudger = ToolNudger::new(2);
        let answer = "The file contains 42 lines of code.";
        assert_eq!(
            nudger.needs_nudge(answer, &tools(), 2),
            NudgeDecision::Accept
        );
        // …while a still-conversational turn at the same retry count forces synthesis.
        assert_eq!(
            nudger.needs_nudge("I would read the file.", &tools(), 2),
            NudgeDecision::MaxRetriesExceeded
        );
    }

    #[test]
    fn test_nudge_message_contains_tools() {
        let nudger = ToolNudger::new(2);
        let msg = nudger.nudge_message(&tools());
        assert!(msg.contains("read_file"));
        assert!(msg.contains("write_file"));
        assert!(msg.contains("shell"));
        assert!(msg.contains("Please use the appropriate tool"));
    }

    #[test]
    fn test_accept_non_conversational_text() {
        let nudger = ToolNudger::new(2);
        // Plain factual response without conversational markers
        let response = "The file contains 42 lines of code.";
        let decision = nudger.needs_nudge(response, &tools(), 0);
        assert_eq!(decision, NudgeDecision::Accept);
    }

    #[test]
    fn test_accept_tool_use_pattern() {
        let nudger = ToolNudger::new(2);
        let response = r#"Using tool: {"tool_use": "shell", "input": "ls"}"#;
        let decision = nudger.needs_nudge(response, &tools(), 0);
        assert_eq!(decision, NudgeDecision::Accept);
    }

    #[test]
    fn test_default_max_retries() {
        let nudger = ToolNudger::default();
        assert_eq!(nudger.max_retries, 3);
    }

    // Regression tests for Qwen3.6 chat-tool failure modes observed in
    // sessions.db (session 9c8f9668, 2026-04-23). After the first nudge
    // cycle exhausted on "I'll check...", the model adapted to use
    // "Let me check..." and "I need to..." — phrases the original
    // conversational_markers list didn't catch, so the nudger accepted
    // them as final and no tool ever fired.

    #[test]
    fn test_nudge_let_me_check() {
        let nudger = ToolNudger::new(3);
        let response = "Let me check the Ironjaw project files for recent changes.";
        let decision = nudger.needs_nudge(response, &tools(), 0);
        assert!(matches!(decision, NudgeDecision::Nudge { .. }));
    }

    #[test]
    fn test_nudge_let_me_look() {
        let nudger = ToolNudger::new(3);
        let response = "Let me look at the source and git history.";
        let decision = nudger.needs_nudge(response, &tools(), 0);
        assert!(matches!(decision, NudgeDecision::Nudge { .. }));
    }

    #[test]
    fn test_nudge_i_need_to() {
        let nudger = ToolNudger::new(3);
        let response = "I need to actually look. Let me check the source and git history.";
        let decision = nudger.needs_nudge(response, &tools(), 0);
        assert!(matches!(decision, NudgeDecision::Nudge { .. }));
    }

    #[test]
    fn test_nudge_i_should() {
        let nudger = ToolNudger::new(3);
        let response = "I should read the config file first.";
        let decision = nudger.needs_nudge(response, &tools(), 0);
        assert!(matches!(decision, NudgeDecision::Nudge { .. }));
    }

    #[test]
    fn test_nudge_im_going_to() {
        let nudger = ToolNudger::new(3);
        let response = "I'm going to search for recent changes.";
        let decision = nudger.needs_nudge(response, &tools(), 0);
        assert!(matches!(decision, NudgeDecision::Nudge { .. }));
    }

    #[test]
    fn test_nudge_let_me_read_via_adb() {
        // session a5822fe7: model announced an action then stopped with no tool call.
        let nudger = ToolNudger::new(3);
        let response = "\n\nLet me read the device specs directly via ADB.\n\n";
        let decision = nudger.needs_nudge(response, &["adb".to_string()], 0);
        assert!(matches!(decision, NudgeDecision::Nudge { .. }));
    }

    #[test]
    fn test_nudge_let_me_action_verbs() {
        let nudger = ToolNudger::new(3);
        for r in [
            "Let me get the battery level.",
            "Let me pull the logs.",
            "Let me run dumpsys.",
            "Let me fetch the package list.",
            "Let me retrieve the model number.",
        ] {
            assert!(
                matches!(
                    nudger.needs_nudge(r, &tools(), 0),
                    NudgeDecision::Nudge { .. }
                ),
                "expected nudge for {r:?}"
            );
        }
    }

    #[test]
    fn test_nudge_with_single_tool() {
        let nudger = ToolNudger::new(1);
        let msg = nudger.nudge_message(&["search".to_string()]);
        assert!(msg.contains("search"));
    }

    #[test]
    fn test_case_insensitive_markers() {
        let nudger = ToolNudger::new(2);
        let response = "i would suggest reading the file first.";
        let decision = nudger.needs_nudge(response, &tools(), 0);
        assert!(matches!(decision, NudgeDecision::Nudge { .. }));
    }

    #[test]
    fn test_you_could_marker() {
        let nudger = ToolNudger::new(2);
        let response = "You could use the shell tool to run ls.";
        let decision = nudger.needs_nudge(response, &tools(), 0);
        assert!(matches!(decision, NudgeDecision::Nudge { .. }));
    }

    #[test]
    fn test_tool_call_with_function_key() {
        let nudger = ToolNudger::new(2);
        let response = r#"{"function": "read_file", "arguments": {}}"#;
        let decision = nudger.needs_nudge(response, &tools(), 0);
        assert_eq!(decision, NudgeDecision::Accept);
    }

    // -- wrong_tool_nudge tests ------------------------------------------

    fn folders() -> Vec<String> {
        vec![
            "ironjaw".to_string(),
            "ExamplePipeline".to_string(),
            "example-project".to_string(),
            "monster-search".to_string(),
            "ai".to_string(), // too short — should be ignored
        ]
    }

    #[test]
    fn test_wrong_tool_fires_for_web_search_on_local_folder() {
        let nudge = wrong_tool_nudge("what's the status of ironjaw", "monster_search", &folders());
        assert!(nudge.is_some());
        let msg = nudge.unwrap();
        assert!(msg.contains("ironjaw"));
        assert!(msg.contains("LOCAL folder"));
    }

    #[test]
    fn test_wrong_tool_fires_for_hyphenated_match() {
        let nudge = wrong_tool_nudge(
            "tell me about example-project progress",
            "web_search",
            &folders(),
        );
        assert!(nudge.is_some());
    }

    #[test]
    fn test_wrong_tool_skips_if_tool_is_not_web_search() {
        let nudge = wrong_tool_nudge("look at ironjaw", "list_files", &folders());
        assert!(nudge.is_none());
    }

    #[test]
    fn test_wrong_tool_skips_if_no_folder_match() {
        let nudge = wrong_tool_nudge(
            "what is the latest news on rust async",
            "monster_search",
            &folders(),
        );
        assert!(nudge.is_none());
    }

    #[test]
    fn test_wrong_tool_ignores_short_folder_names() {
        // "ai" appears in folders but is < 3 chars, so prompt "tell me about ai"
        // should NOT fire the nudge.
        let nudge = wrong_tool_nudge("tell me about ai", "monster_search", &folders());
        assert!(nudge.is_none());
    }

    #[test]
    fn test_wrong_tool_word_boundary_not_substring() {
        // "ironjawesome" should NOT match "ironjaw" (token-based, not substring).
        let nudge = wrong_tool_nudge("ironjawesome library lookup", "monster_search", &folders());
        assert!(nudge.is_none());
    }

    #[test]
    fn test_wrong_tool_case_insensitive() {
        let nudge = wrong_tool_nudge("status of IRONJAW please", "vane", &folders());
        assert!(nudge.is_some());
    }

    #[test]
    fn test_wrong_tool_fires_for_zoekt() {
        // Lower-case query against the CamelCase folder name: the point of this
        // case is that matching ignores case.
        let nudge = wrong_tool_nudge("show me examplepipeline logs", "zoekt_search", &folders());
        assert!(nudge.is_some());
    }
}
