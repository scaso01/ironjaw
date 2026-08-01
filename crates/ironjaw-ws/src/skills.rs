// Skills system (Stream 8.1)
// A Skill is a named capability bundle: tools, prompts, and config that can be activated/deactivated as a unit.

use std::collections::HashMap;

/// Resolve a model name through aliases. Returns the alias target if found, otherwise the original name.
pub fn resolve_alias(aliases: &HashMap<String, String>, name: &str) -> String {
    aliases
        .get(name)
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

/// A named capability bundle containing related tools, prompts, and configuration.
///
/// Skills group related tools together and can be enabled/disabled as a unit.
/// When enabled, the skill's tools become available and its system prompt (if any)
/// is included in LLM context.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Skill {
    /// Unique name identifying this skill.
    pub name: String,
    /// Human-readable description of what this skill provides.
    pub description: String,
    /// Semantic version string (e.g. "1.0.0").
    pub version: String,
    /// Tool names this skill provides. These correspond to tool names in the ToolRegistry.
    pub tools: Vec<String>,
    /// Additional system prompt injected when this skill is active.
    pub system_prompt: Option<String>,
    /// Whether this skill is currently enabled.
    pub enabled: bool,
    /// Primary model alias for this skill (e.g. "coder").
    #[serde(default)]
    pub model: Option<String>,
    /// Ordered fallback model aliases if the primary is unavailable.
    #[serde(default)]
    pub fallback_chain: Vec<String>,
    /// Cloud model aliases available for switching while this skill is active.
    #[serde(default)]
    pub cloud_models: Vec<String>,
    /// LLM temperature override for this skill.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Original model value from config, used to restore after cloud preset override.
    #[serde(skip)]
    pub original_model: Option<String>,
}

/// Errors that can occur during skill operations.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SkillError {
    /// The requested skill was not found in the registry.
    #[error("skill not found: {0}")]
    NotFound(String),
    /// A skill with the given name already exists in the registry.
    #[error("skill already exists: {0}")]
    AlreadyExists(String),
    /// The skill definition is invalid (e.g. empty name).
    #[error("invalid skill: {0}")]
    InvalidSkill(String),
}

/// Registry of available skills. Manages registration, lookup, and enable/disable state.
#[derive(Debug)]
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
}

impl SkillRegistry {
    /// Create an empty skill registry.
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
        }
    }

    /// Register a new skill. Returns an error if the name is empty or already taken.
    pub fn register(&mut self, skill: Skill) -> Result<(), SkillError> {
        if skill.name.is_empty() {
            return Err(SkillError::InvalidSkill(
                "skill name cannot be empty".to_string(),
            ));
        }
        use std::collections::hash_map::Entry;
        match self.skills.entry(skill.name.clone()) {
            Entry::Occupied(e) => Err(SkillError::AlreadyExists(e.key().clone())),
            Entry::Vacant(e) => {
                e.insert(skill);
                Ok(())
            }
        }
    }

    /// Remove a skill by name. Returns an error if it does not exist.
    pub fn unregister(&mut self, name: &str) -> Result<(), SkillError> {
        if self.skills.remove(name).is_none() {
            return Err(SkillError::NotFound(name.to_string()));
        }
        Ok(())
    }

    /// Get a reference to a skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// List all registered skills, sorted by name.
    pub fn list(&self) -> Vec<&Skill> {
        let mut skills: Vec<&Skill> = self.skills.values().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// Enable a skill by name. Returns an error if the skill does not exist.
    pub fn enable(&mut self, name: &str) -> Result<(), SkillError> {
        match self.skills.get_mut(name) {
            Some(skill) => {
                skill.enabled = true;
                Ok(())
            }
            None => Err(SkillError::NotFound(name.to_string())),
        }
    }

    /// Disable a skill by name. Returns an error if the skill does not exist.
    pub fn disable(&mut self, name: &str) -> Result<(), SkillError> {
        match self.skills.get_mut(name) {
            Some(skill) => {
                skill.enabled = false;
                Ok(())
            }
            None => Err(SkillError::NotFound(name.to_string())),
        }
    }

    /// Collect all tool names from enabled skills, sorted and deduplicated.
    pub fn active_tools(&self) -> Vec<&str> {
        let mut tools: Vec<&str> = self
            .skills
            .values()
            .filter(|s| s.enabled)
            .flat_map(|s| s.tools.iter().map(|t| t.as_str()))
            .collect();
        tools.sort();
        tools.dedup();
        tools
    }

    /// Collect all system prompts from enabled skills that have one, sorted by skill name.
    pub fn active_prompts(&self) -> Vec<&str> {
        let mut enabled: Vec<&Skill> = self.skills.values().filter(|s| s.enabled).collect();
        enabled.sort_by(|a, b| a.name.cmp(&b.name));
        enabled
            .into_iter()
            .filter_map(|s| s.system_prompt.as_deref())
            .collect()
    }

    /// Switch active skill -- disable all others, enable the target.
    /// Returns error if skill name not found.
    pub fn switch(&mut self, name: &str) -> Result<(), SkillError> {
        if !self.skills.contains_key(name) {
            return Err(SkillError::NotFound(name.to_string()));
        }
        for skill in self.skills.values_mut() {
            skill.enabled = false;
        }
        self.skills.get_mut(name).unwrap().enabled = true;
        Ok(())
    }

    /// Get the currently active (enabled) skill. Returns None if no skill is enabled
    /// or if multiple are enabled (ambiguous).
    pub fn active_skill(&self) -> Option<&Skill> {
        let enabled: Vec<&Skill> = self.skills.values().filter(|s| s.enabled).collect();
        if enabled.len() == 1 {
            Some(enabled[0])
        } else {
            None
        }
    }

    /// Resolve the effective model for the active skill using alias lookup.
    /// Tries primary model first, then walks fallback_chain.
    /// Returns None if no active skill or no model configured.
    pub fn resolve_model(&self, aliases: &HashMap<String, String>) -> Option<String> {
        let skill = self.active_skill()?;
        if let Some(ref model) = skill.model {
            return Some(resolve_alias(aliases, model));
        }
        skill
            .fallback_chain
            .first()
            .map(|fallback| resolve_alias(aliases, fallback))
    }

    /// Set the model for a specific skill.
    pub fn set_model(&mut self, name: &str, model: &str) -> Result<(), SkillError> {
        match self.skills.get_mut(name) {
            Some(skill) => {
                skill.model = Some(model.to_string());
                Ok(())
            }
            None => Err(SkillError::NotFound(name.to_string())),
        }
    }

    /// Apply preset to all skills. "cloud" uses cloud_models[0], "local" restores original_model.
    pub fn apply_preset(&mut self, preset: &str) -> Vec<(String, Option<String>)> {
        self.skills
            .values_mut()
            .map(|skill| {
                match preset {
                    "cloud" => {
                        if let Some(cloud) = skill.cloud_models.first() {
                            skill.model = Some(cloud.clone());
                        }
                    }
                    "local" => {
                        skill.model = skill.original_model.clone();
                    }
                    _ => {}
                }
                (skill.name.clone(), skill.model.clone())
            })
            .collect()
    }

    /// Number of registered skills.
    pub fn count(&self) -> usize {
        self.skills.len()
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a basic skill for testing.
    fn make_skill(name: &str, enabled: bool) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("{name} skill"),
            version: "1.0.0".to_string(),
            tools: vec![format!("{name}_tool_a"), format!("{name}_tool_b")],
            system_prompt: Some(format!("You have {name} capabilities.")),
            enabled,
            model: None,
            fallback_chain: vec![],
            cloud_models: vec![],
            temperature: None,
            original_model: None,
        }
    }

    /// Helper to create a skill without a system prompt.
    fn make_skill_no_prompt(name: &str, enabled: bool) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("{name} skill"),
            version: "1.0.0".to_string(),
            tools: vec![format!("{name}_tool")],
            system_prompt: None,
            enabled,
            model: None,
            fallback_chain: vec![],
            cloud_models: vec![],
            temperature: None,
            original_model: None,
        }
    }

    #[test]
    fn test_register_and_get() {
        let mut reg = SkillRegistry::new();
        let skill = make_skill("search", true);
        reg.register(skill).unwrap();

        let found = reg.get("search").unwrap();
        assert_eq!(found.name, "search");
        assert_eq!(found.version, "1.0.0");
        assert!(found.enabled);
    }

    #[test]
    fn test_register_duplicate_fails() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("search", true)).unwrap();

        let err = reg.register(make_skill("search", false)).unwrap_err();
        assert_eq!(err, SkillError::AlreadyExists("search".to_string()));
    }

    #[test]
    fn test_register_empty_name_fails() {
        let mut reg = SkillRegistry::new();
        let skill = Skill {
            name: String::new(),
            description: "bad".to_string(),
            version: "1.0.0".to_string(),
            tools: vec![],
            system_prompt: None,
            enabled: false,
            model: None,
            fallback_chain: vec![],
            cloud_models: vec![],
            temperature: None,
            original_model: None,
        };
        let err = reg.register(skill).unwrap_err();
        assert!(matches!(err, SkillError::InvalidSkill(_)));
    }

    #[test]
    fn test_unregister() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("search", true)).unwrap();
        assert_eq!(reg.count(), 1);

        reg.unregister("search").unwrap();
        assert_eq!(reg.count(), 0);
        assert!(reg.get("search").is_none());
    }

    #[test]
    fn test_unregister_nonexistent_fails() {
        let mut reg = SkillRegistry::new();
        let err = reg.unregister("ghost").unwrap_err();
        assert_eq!(err, SkillError::NotFound("ghost".to_string()));
    }

    #[test]
    fn test_list_sorted() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("zeta", true)).unwrap();
        reg.register(make_skill("alpha", false)).unwrap();
        reg.register(make_skill("mid", true)).unwrap();

        let list = reg.list();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].name, "alpha");
        assert_eq!(list[1].name, "mid");
        assert_eq!(list[2].name, "zeta");
    }

    #[test]
    fn test_enable_and_disable() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("search", false)).unwrap();

        assert!(!reg.get("search").unwrap().enabled);
        reg.enable("search").unwrap();
        assert!(reg.get("search").unwrap().enabled);
        reg.disable("search").unwrap();
        assert!(!reg.get("search").unwrap().enabled);
    }

    #[test]
    fn test_enable_nonexistent_fails() {
        let mut reg = SkillRegistry::new();
        let err = reg.enable("ghost").unwrap_err();
        assert_eq!(err, SkillError::NotFound("ghost".to_string()));
    }

    #[test]
    fn test_disable_nonexistent_fails() {
        let mut reg = SkillRegistry::new();
        let err = reg.disable("ghost").unwrap_err();
        assert_eq!(err, SkillError::NotFound("ghost".to_string()));
    }

    #[test]
    fn test_active_tools_only_from_enabled() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("search", true)).unwrap();
        reg.register(make_skill("git", false)).unwrap();

        let tools = reg.active_tools();
        assert_eq!(tools, vec!["search_tool_a", "search_tool_b"]);
    }

    #[test]
    fn test_active_tools_deduplication() {
        let mut reg = SkillRegistry::new();
        let mut skill_a = make_skill("alpha", true);
        skill_a.tools = vec!["shared_tool".to_string(), "alpha_only".to_string()];
        let mut skill_b = make_skill("beta", true);
        skill_b.tools = vec!["shared_tool".to_string(), "beta_only".to_string()];

        reg.register(skill_a).unwrap();
        reg.register(skill_b).unwrap();

        let tools = reg.active_tools();
        assert_eq!(
            tools,
            vec!["alpha_only", "beta_only", "shared_tool"] // sorted, deduplicated
        );
    }

    #[test]
    fn test_active_prompts_only_from_enabled() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("search", true)).unwrap();
        reg.register(make_skill("git", false)).unwrap();
        reg.register(make_skill_no_prompt("monitor", true)).unwrap();

        let prompts = reg.active_prompts();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0], "You have search capabilities.");
    }

    #[test]
    fn test_active_prompts_sorted_by_skill_name() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("zeta", true)).unwrap();
        reg.register(make_skill("alpha", true)).unwrap();

        let prompts = reg.active_prompts();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0], "You have alpha capabilities.");
        assert_eq!(prompts[1], "You have zeta capabilities.");
    }

    #[test]
    fn test_empty_registry() {
        let reg = SkillRegistry::new();
        assert_eq!(reg.count(), 0);
        assert!(reg.list().is_empty());
        assert!(reg.active_tools().is_empty());
        assert!(reg.active_prompts().is_empty());
        assert!(reg.get("anything").is_none());
    }

    #[test]
    fn test_default_trait() {
        let reg = SkillRegistry::default();
        assert_eq!(reg.count(), 0);
    }

    #[test]
    fn test_skill_error_display() {
        let not_found = SkillError::NotFound("search".to_string());
        assert_eq!(not_found.to_string(), "skill not found: search");

        let exists = SkillError::AlreadyExists("search".to_string());
        assert_eq!(exists.to_string(), "skill already exists: search");

        let invalid = SkillError::InvalidSkill("bad name".to_string());
        assert_eq!(invalid.to_string(), "invalid skill: bad name");
    }

    #[test]
    fn test_switch_activates_single_skill() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("alpha", true)).unwrap();
        reg.register(make_skill("beta", false)).unwrap();

        reg.switch("beta").unwrap();

        assert!(!reg.get("alpha").unwrap().enabled);
        assert!(reg.get("beta").unwrap().enabled);
    }

    #[test]
    fn test_switch_nonexistent_fails() {
        let mut reg = SkillRegistry::new();
        let err = reg.switch("ghost").unwrap_err();
        assert_eq!(err, SkillError::NotFound("ghost".to_string()));
    }

    #[test]
    fn test_active_skill_returns_single_enabled() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("alpha", false)).unwrap();
        reg.register(make_skill("beta", true)).unwrap();

        let active = reg.active_skill().unwrap();
        assert_eq!(active.name, "beta");
    }

    #[test]
    fn test_active_skill_none_when_multiple_enabled() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("alpha", true)).unwrap();
        reg.register(make_skill("beta", true)).unwrap();

        // Multiple enabled = ambiguous, returns None
        assert!(reg.active_skill().is_none());
    }

    #[test]
    fn test_active_skill_none_when_none_enabled() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill("alpha", false)).unwrap();
        assert!(reg.active_skill().is_none());
    }

    fn make_skill_with_model(name: &str, enabled: bool, model: Option<&str>) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("{name} skill"),
            version: "1.0.0".to_string(),
            tools: vec![],
            system_prompt: None,
            enabled,
            model: model.map(|s| s.to_string()),
            fallback_chain: vec![],
            cloud_models: vec![],
            temperature: None,
            original_model: model.map(|s| s.to_string()),
        }
    }

    #[test]
    fn test_resolve_model_from_primary() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill_with_model("coder", true, Some("coder")))
            .unwrap();

        let mut aliases = HashMap::new();
        aliases.insert("coder".to_string(), "llama-server/qwen3-coder".to_string());

        let model = reg.resolve_model(&aliases).unwrap();
        assert_eq!(model, "llama-server/qwen3-coder");
    }

    #[test]
    fn test_resolve_model_from_fallback() {
        let mut reg = SkillRegistry::new();
        let mut skill = make_skill_with_model("coder", true, None);
        skill.fallback_chain = vec!["fast".to_string()];
        reg.register(skill).unwrap();

        let mut aliases = HashMap::new();
        aliases.insert("fast".to_string(), "llama-server/qwen3:8b".to_string());

        let model = reg.resolve_model(&aliases).unwrap();
        assert_eq!(model, "llama-server/qwen3:8b");
    }

    #[test]
    fn test_resolve_model_no_model_configured() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill_with_model("coder", true, None))
            .unwrap();

        let aliases = HashMap::new();
        assert!(reg.resolve_model(&aliases).is_none());
    }

    #[test]
    fn test_resolve_model_passthrough_when_no_alias() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill_with_model(
            "coder",
            true,
            Some("literal-model-name"),
        ))
        .unwrap();

        let aliases = HashMap::new();
        let model = reg.resolve_model(&aliases).unwrap();
        assert_eq!(model, "literal-model-name");
    }

    #[test]
    fn test_resolve_alias_fn() {
        let mut aliases = HashMap::new();
        aliases.insert("coder".to_string(), "llama-server/qwen3-coder".to_string());

        assert_eq!(resolve_alias(&aliases, "coder"), "llama-server/qwen3-coder");
        assert_eq!(resolve_alias(&aliases, "unknown"), "unknown");
        assert_eq!(resolve_alias(&HashMap::new(), "anything"), "anything");
    }

    #[test]
    fn test_set_model() {
        let mut reg = SkillRegistry::new();
        reg.register(make_skill_with_model("coder", true, Some("coder")))
            .unwrap();

        reg.set_model("coder", "cloud").unwrap();
        assert_eq!(reg.get("coder").unwrap().model, Some("cloud".to_string()));
    }

    #[test]
    fn test_set_model_nonexistent_fails() {
        let mut reg = SkillRegistry::new();
        let err = reg.set_model("ghost", "cloud").unwrap_err();
        assert_eq!(err, SkillError::NotFound("ghost".to_string()));
    }

    #[test]
    fn test_apply_preset_cloud() {
        let mut reg = SkillRegistry::new();
        let mut brain = make_skill_with_model("brain", true, Some("brain"));
        brain.cloud_models = vec!["cloud".to_string()];
        let mut coder = make_skill_with_model("coder", false, Some("coder"));
        coder.cloud_models = vec!["opus".to_string()];
        reg.register(brain).unwrap();
        reg.register(coder).unwrap();

        let results = reg.apply_preset("cloud");
        assert_eq!(results.len(), 2);

        assert_eq!(reg.get("brain").unwrap().model, Some("cloud".to_string()));
        assert_eq!(reg.get("coder").unwrap().model, Some("opus".to_string()));
    }

    #[test]
    fn test_apply_preset_local_restores_original() {
        let mut reg = SkillRegistry::new();
        let mut skill = make_skill_with_model("brain", true, Some("brain"));
        skill.cloud_models = vec!["cloud".to_string()];
        reg.register(skill).unwrap();

        // Switch to cloud
        reg.apply_preset("cloud");
        assert_eq!(reg.get("brain").unwrap().model, Some("cloud".to_string()));

        // Restore to local
        reg.apply_preset("local");
        assert_eq!(reg.get("brain").unwrap().model, Some("brain".to_string()));
    }

    #[test]
    fn test_apply_preset_roundtrip() {
        let mut reg = SkillRegistry::new();
        let mut brain = make_skill_with_model("brain", true, Some("brain"));
        brain.cloud_models = vec!["cloud".to_string()];
        let mut coder = make_skill_with_model("coder", false, Some("coder"));
        coder.cloud_models = vec!["opus".to_string()];
        reg.register(brain).unwrap();
        reg.register(coder).unwrap();

        // local -> cloud -> local should restore original models
        let original_brain = reg.get("brain").unwrap().model.clone();
        let original_coder = reg.get("coder").unwrap().model.clone();

        reg.apply_preset("cloud");
        assert_ne!(reg.get("brain").unwrap().model, original_brain);
        assert_ne!(reg.get("coder").unwrap().model, original_coder);

        reg.apply_preset("local");
        assert_eq!(reg.get("brain").unwrap().model, original_brain);
        assert_eq!(reg.get("coder").unwrap().model, original_coder);
    }

    #[test]
    fn test_apply_preset_unknown_is_noop() {
        let mut reg = SkillRegistry::new();
        let mut skill = make_skill_with_model("brain", true, Some("brain"));
        skill.cloud_models = vec!["cloud".to_string()];
        reg.register(skill).unwrap();

        reg.apply_preset("unknown_preset");
        assert_eq!(reg.get("brain").unwrap().model, Some("brain".to_string()));
    }
}
