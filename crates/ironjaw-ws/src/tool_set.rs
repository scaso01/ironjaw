/// Build the tool set advertised to the LLM.
///
/// Two strategies are exposed:
///
/// * [`build_full_tool_set`] -- enumerates every tool registered in the
///   `ToolRegistry`. This is the canonical Claude-Code-style behaviour:
///   tools are advertised unconditionally and gated only by per-tool
///   permission tier and the `[security.rules]` engine.
///
/// * [`build_active_tool_set`] -- legacy skill-allowlist filter. Kept for
///   backwards compatibility (and existing tests). New call sites should
///   prefer `build_full_tool_set`.
use ironjaw_llm::types::ToolDefinition;
use ironjaw_tools::ToolRegistry;

use crate::skills::Skill;

/// Build the full tool set from the registry, ignoring any skill allowlist.
///
/// Returns `Some(Vec<ToolDefinition>)` when the registry contains at least
/// one tool, `None` if the registry is empty (in which case the caller
/// should send the LLM request without a `tools` field).
///
/// Tools are sorted by name for deterministic ordering across requests.
pub fn build_full_tool_set(registry: &ToolRegistry) -> Option<Vec<ToolDefinition>> {
    let summaries = registry.list_summaries();
    if summaries.is_empty() {
        return None;
    }

    let definitions: Vec<ToolDefinition> = summaries
        .iter()
        .filter_map(|s| registry.get(&s.name))
        .map(|tool| tool.to_definition())
        .collect();

    if definitions.is_empty() {
        None
    } else {
        Some(definitions)
    }
}

/// Build a curated tool set for local (llama-server) models from an explicit
/// allowlist of tool names.
///
/// Unknown names are skipped silently (a name may reference a tool that is not
/// registered in the current build). Returns `None` if the allowlist is empty
/// or matches nothing — the caller should fall back to [`build_full_tool_set`].
///
/// Tools are returned in registry order (deterministic per `list_summaries`).
pub fn build_local_tool_set(
    registry: &ToolRegistry,
    allow: &[String],
) -> Option<Vec<ToolDefinition>> {
    if allow.is_empty() {
        return None;
    }

    let definitions: Vec<ToolDefinition> = allow
        .iter()
        .filter_map(|name| registry.get(name))
        .map(|tool| tool.to_definition())
        .collect();

    if definitions.is_empty() {
        None
    } else {
        Some(definitions)
    }
}

/// Build a tool set restricted to a skill's allowlist.
///
/// Deprecated: tool advertising is no longer skill-gated. Use
/// [`build_full_tool_set`] for new code; the `Skill::tools` field is now
/// retained only for the dashboard's legacy display.
#[deprecated(note = "Use build_full_tool_set; skill.tools is now legacy display-only")]
pub fn build_active_tool_set(
    registry: &ToolRegistry,
    skill: Option<&Skill>,
) -> Option<Vec<ToolDefinition>> {
    let skill = skill?;

    if skill.tools.is_empty() {
        return None;
    }

    let definitions: Vec<ToolDefinition> = skill
        .tools
        .iter()
        .filter_map(|name| registry.get(name))
        .map(|tool| tool.to_definition())
        .collect();

    if definitions.is_empty() {
        None
    } else {
        Some(definitions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::Skill;
    use ironjaw_tools::{ToolContext, ToolRegistry, ToolResult};
    use std::path::PathBuf;

    /// Minimal stub tool for testing.
    struct StubTool {
        name: String,
    }

    #[async_trait::async_trait]
    impl ironjaw_tools::Tool for StubTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "A stub tool for testing"
        }

        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        fn tier(&self) -> ironjaw_security::PermissionTier {
            ironjaw_security::PermissionTier::Auto
        }

        async fn execute(
            &self,
            _params: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> ToolResult {
            ToolResult::text("stub")
        }
    }

    fn make_skill(tools: Vec<&str>) -> Skill {
        Skill {
            name: "test-skill".to_string(),
            description: "test".to_string(),
            version: "1.0.0".to_string(),
            tools: tools.into_iter().map(|s| s.to_string()).collect(),
            system_prompt: None,
            enabled: true,
            model: None,
            fallback_chain: vec![],
            cloud_models: vec![],
            temperature: None,
            original_model: None,
        }
    }

    #[allow(dead_code)]
    fn make_context() -> ToolContext {
        ToolContext::new("s1", "c1", PathBuf::from("."))
    }

    #[test]
    #[allow(deprecated)]
    fn no_skill_returns_none() {
        let registry = ToolRegistry::new();
        assert!(build_active_tool_set(&registry, None).is_none());
    }

    #[test]
    #[allow(deprecated)]
    fn skill_with_empty_tools_returns_none() {
        let registry = ToolRegistry::new();
        let skill = make_skill(vec![]);
        assert!(build_active_tool_set(&registry, Some(&skill)).is_none());
    }

    #[test]
    #[allow(deprecated)]
    fn skill_with_unknown_tools_returns_none() {
        let registry = ToolRegistry::new();
        let skill = make_skill(vec!["nonexistent_tool"]);
        // No tools registered — filter_map yields nothing → None
        assert!(build_active_tool_set(&registry, Some(&skill)).is_none());
    }

    #[test]
    #[allow(deprecated)]
    fn skill_with_known_tools_returns_definitions() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StubTool {
            name: "my_tool".to_string(),
        }));

        let skill = make_skill(vec!["my_tool"]);
        let result = build_active_tool_set(&registry, Some(&skill));
        assert!(result.is_some());
        let defs = result.unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "my_tool");
    }

    #[test]
    #[allow(deprecated)]
    fn only_allowlisted_tools_are_returned() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StubTool {
            name: "tool_a".to_string(),
        }));
        registry.register(Box::new(StubTool {
            name: "tool_b".to_string(),
        }));

        // Skill only allows tool_a
        let skill = make_skill(vec!["tool_a"]);
        let result = build_active_tool_set(&registry, Some(&skill)).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "tool_a");
    }

    // -----------------------------------------------------------------------
    // build_full_tool_set tests (A1)
    // -----------------------------------------------------------------------

    #[test]
    fn full_tool_set_empty_registry_returns_none() {
        let registry = ToolRegistry::new();
        assert!(build_full_tool_set(&registry).is_none());
    }

    #[test]
    fn full_tool_set_returns_all_registered() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StubTool {
            name: "tool_a".to_string(),
        }));
        registry.register(Box::new(StubTool {
            name: "tool_b".to_string(),
        }));
        registry.register(Box::new(StubTool {
            name: "tool_c".to_string(),
        }));

        let result = build_full_tool_set(&registry).expect("non-empty registry");
        assert_eq!(result.len(), 3);
        let mut names: Vec<&str> = result.iter().map(|d| d.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["tool_a", "tool_b", "tool_c"]);
    }

    #[test]
    fn full_tool_set_ignores_skill() {
        // Verify the function takes no skill parameter and returns ALL
        // registered tools regardless of any skill allowlist that might
        // exist elsewhere. This mimics the brain+other-tool scenario:
        // even though the brain skill in production only lists 4 tools,
        // build_full_tool_set should expose every tool in the registry.
        let mut registry = ToolRegistry::new();
        // Mimic a "brain skill" tool plus unrelated tools.
        registry.register(Box::new(StubTool {
            name: "brain_recall".to_string(),
        }));
        registry.register(Box::new(StubTool {
            name: "brain_remember".to_string(),
        }));
        registry.register(Box::new(StubTool {
            name: "write_file".to_string(),
        }));
        registry.register(Box::new(StubTool {
            name: "shell".to_string(),
        }));
        registry.register(Box::new(StubTool {
            name: "patch".to_string(),
        }));

        let result = build_full_tool_set(&registry).expect("non-empty registry");
        assert_eq!(result.len(), 5);
        let names: std::collections::HashSet<&str> =
            result.iter().map(|d| d.name.as_str()).collect();
        // All tools present, including ones a brain-only skill would have hidden.
        assert!(names.contains("write_file"));
        assert!(names.contains("shell"));
        assert!(names.contains("patch"));
        assert!(names.contains("brain_recall"));
        assert!(names.contains("brain_remember"));
    }
}
