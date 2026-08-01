use serde::{Deserialize, Serialize};

/// How the LLM router selects models for a given request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RoutingMode {
    /// Route based on request complexity (existing behavior).
    #[default]
    Complexity,
    /// Two-phase pipeline: planner model produces a plan, executor model
    /// generates the final response from that plan + conversation context.
    PhaseBased { planner: String, executor: String },
}

/// The structured plan produced by the planner phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanOutput {
    /// The raw plan text from the planner model.
    pub plan: String,
    /// The model that produced the plan.
    pub planner_model: String,
}

/// Configuration for per-skill routing overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillRoutingConfig {
    /// Skill names that should use phase-based routing.
    /// If empty, phase-based routing is disabled for all skills.
    pub phase_based_skills: Vec<String>,
    /// The default planner model when phase-based routing is active.
    pub default_planner: Option<String>,
    /// The default executor model when phase-based routing is active.
    pub default_executor: Option<String>,
}

impl SkillRoutingConfig {
    /// Check if a skill should use phase-based routing.
    pub fn is_phase_based(&self, skill_name: &str) -> bool {
        self.phase_based_skills.iter().any(|s| s == skill_name)
    }

    /// Get the routing mode for a skill, falling back to Complexity if not configured.
    pub fn routing_mode_for(&self, skill_name: &str) -> RoutingMode {
        if self.is_phase_based(skill_name) {
            match (&self.default_planner, &self.default_executor) {
                (Some(planner), Some(executor)) => RoutingMode::PhaseBased {
                    planner: planner.clone(),
                    executor: executor.clone(),
                },
                _ => RoutingMode::Complexity,
            }
        } else {
            RoutingMode::Complexity
        }
    }
}

/// System prompt used to instruct the planner model.
pub const PLANNER_SYSTEM_PROMPT: &str = "\
You are a planning assistant. Given the user's request and conversation context, \
produce a structured plan for how to respond. Your plan should:\n\
1. Identify the key points to address\n\
2. Outline the response structure\n\
3. Note any constraints or considerations\n\
4. Be concise and actionable\n\n\
Return ONLY the plan, no preamble or explanation.";

/// System prompt prefix used to instruct the executor model.
pub const EXECUTOR_SYSTEM_PREFIX: &str = "\
You are executing a response based on the following plan. \
Follow the plan closely while generating a natural, complete response.\n\n\
## Plan\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_routing_mode_default_is_complexity() {
        let mode = RoutingMode::default();
        assert!(matches!(mode, RoutingMode::Complexity));
    }

    #[test]
    fn test_routing_mode_phase_based() {
        let mode = RoutingMode::PhaseBased {
            planner: "qwen3:8b".to_string(),
            executor: "qwen3-coder".to_string(),
        };
        match mode {
            RoutingMode::PhaseBased { planner, executor } => {
                assert_eq!(planner, "qwen3:8b");
                assert_eq!(executor, "qwen3-coder");
            }
            _ => panic!("Expected PhaseBased"),
        }
    }

    #[test]
    fn test_routing_mode_serialization_complexity() {
        let mode = RoutingMode::Complexity;
        let json = serde_json::to_string(&mode).unwrap();
        assert!(json.contains("\"mode\":\"complexity\""));
        let parsed: RoutingMode = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, RoutingMode::Complexity));
    }

    #[test]
    fn test_routing_mode_serialization_phase_based() {
        let mode = RoutingMode::PhaseBased {
            planner: "small-model".to_string(),
            executor: "big-model".to_string(),
        };
        let json = serde_json::to_string(&mode).unwrap();
        assert!(json.contains("\"mode\":\"phase_based\""));
        assert!(json.contains("\"planner\":\"small-model\""));
        assert!(json.contains("\"executor\":\"big-model\""));
        let parsed: RoutingMode = serde_json::from_str(&json).unwrap();
        match parsed {
            RoutingMode::PhaseBased { planner, executor } => {
                assert_eq!(planner, "small-model");
                assert_eq!(executor, "big-model");
            }
            _ => panic!("Expected PhaseBased"),
        }
    }

    #[test]
    fn test_plan_output() {
        let plan = PlanOutput {
            plan: "1. Greet user\n2. Answer question".to_string(),
            planner_model: "qwen3:8b".to_string(),
        };
        assert_eq!(plan.planner_model, "qwen3:8b");
        assert!(plan.plan.contains("Greet user"));
    }

    #[test]
    fn test_skill_routing_config_default() {
        let config = SkillRoutingConfig::default();
        assert!(config.phase_based_skills.is_empty());
        assert!(config.default_planner.is_none());
        assert!(config.default_executor.is_none());
    }

    #[test]
    fn test_skill_routing_is_phase_based() {
        let config = SkillRoutingConfig {
            phase_based_skills: vec!["coding".to_string(), "analysis".to_string()],
            default_planner: Some("qwen3:8b".to_string()),
            default_executor: Some("qwen3-coder".to_string()),
        };
        assert!(config.is_phase_based("coding"));
        assert!(config.is_phase_based("analysis"));
        assert!(!config.is_phase_based("chat"));
    }

    #[test]
    fn test_skill_routing_mode_for_phase_based() {
        let config = SkillRoutingConfig {
            phase_based_skills: vec!["coding".to_string()],
            default_planner: Some("qwen3:8b".to_string()),
            default_executor: Some("qwen3-coder".to_string()),
        };
        let mode = config.routing_mode_for("coding");
        match mode {
            RoutingMode::PhaseBased { planner, executor } => {
                assert_eq!(planner, "qwen3:8b");
                assert_eq!(executor, "qwen3-coder");
            }
            _ => panic!("Expected PhaseBased for coding skill"),
        }
    }

    #[test]
    fn test_skill_routing_mode_for_non_phase_based() {
        let config = SkillRoutingConfig {
            phase_based_skills: vec!["coding".to_string()],
            default_planner: Some("qwen3:8b".to_string()),
            default_executor: Some("qwen3-coder".to_string()),
        };
        let mode = config.routing_mode_for("chat");
        assert!(matches!(mode, RoutingMode::Complexity));
    }

    #[test]
    fn test_skill_routing_mode_falls_back_without_models() {
        let config = SkillRoutingConfig {
            phase_based_skills: vec!["coding".to_string()],
            default_planner: None,
            default_executor: None,
        };
        // Even though coding is in phase_based_skills, missing models => Complexity fallback
        let mode = config.routing_mode_for("coding");
        assert!(matches!(mode, RoutingMode::Complexity));
    }

    #[test]
    fn test_skill_routing_mode_partial_models_fallback() {
        let config = SkillRoutingConfig {
            phase_based_skills: vec!["coding".to_string()],
            default_planner: Some("qwen3:8b".to_string()),
            default_executor: None, // missing executor
        };
        let mode = config.routing_mode_for("coding");
        assert!(matches!(mode, RoutingMode::Complexity));
    }

    #[test]
    fn test_planner_system_prompt_not_empty() {
        assert!(!PLANNER_SYSTEM_PROMPT.is_empty());
        assert!(PLANNER_SYSTEM_PROMPT.contains("planning"));
    }

    #[test]
    fn test_executor_system_prefix_not_empty() {
        assert!(!EXECUTOR_SYSTEM_PREFIX.is_empty());
        assert!(EXECUTOR_SYSTEM_PREFIX.contains("Plan"));
    }

    #[test]
    fn test_plan_output_serialization() {
        let plan = PlanOutput {
            plan: "Step 1: do X\nStep 2: do Y".to_string(),
            planner_model: "test-model".to_string(),
        };
        let json = serde_json::to_string(&plan).unwrap();
        let parsed: PlanOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.plan, plan.plan);
        assert_eq!(parsed.planner_model, plan.planner_model);
    }

    #[test]
    fn test_skill_routing_config_serialization() {
        let config = SkillRoutingConfig {
            phase_based_skills: vec!["coding".to_string()],
            default_planner: Some("planner".to_string()),
            default_executor: Some("executor".to_string()),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: SkillRoutingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.phase_based_skills, vec!["coding"]);
        assert_eq!(parsed.default_planner.as_deref(), Some("planner"));
        assert_eq!(parsed.default_executor.as_deref(), Some("executor"));
    }
}
