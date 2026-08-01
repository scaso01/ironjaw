use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SopError {
    #[error("failed to parse TOML: {0}")]
    TomlParseError(#[from] toml::de::Error),
    #[error("invalid SOP definition: {0}")]
    InvalidDefinition(String),
    #[error("step not found: {0}")]
    StepNotFound(String),
    #[error("execution error: {0}")]
    ExecutionError(String),
    #[error("gate not pending approval: {0}")]
    GateNotPending(String),
    #[error("SOP is paused")]
    Paused,
    #[error("SOP already finished")]
    Finished,
}

/// A complete SOP definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SopDefinition {
    pub name: String,
    pub description: String,
    pub steps: Vec<SopStep>,
}

/// A single step in an SOP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SopStep {
    pub id: String,
    pub name: String,
    pub kind: StepKind,
    pub action: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// The kind of SOP step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StepKind {
    /// Auto-execute step, no human approval needed.
    Action,
    /// Requires human-in-the-loop approval before proceeding.
    Gate,
    /// Conditional branch: evaluates action, routes to on_true or on_false step.
    Condition { on_true: String, on_false: String },
}

// ── TOML format ──────────────────────────────────────────

/// Intermediate TOML representation for deserialization.
#[derive(Debug, Deserialize)]
struct TomlSop {
    name: String,
    description: String,
    #[serde(default)]
    steps: Vec<TomlStep>,
}

#[derive(Debug, Deserialize)]
struct TomlStep {
    id: String,
    name: String,
    #[serde(rename = "type", default = "default_step_type")]
    kind: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    on_true: Option<String>,
    #[serde(default)]
    on_false: Option<String>,
}

fn default_step_type() -> String {
    "action".to_string()
}

/// Parse an SOP definition from TOML content.
pub fn parse_toml(content: &str) -> Result<SopDefinition, SopError> {
    let raw: TomlSop = toml::from_str(content)?;

    let mut steps = Vec::with_capacity(raw.steps.len());
    for step in raw.steps {
        let kind = match step.kind.as_str() {
            "action" => StepKind::Action,
            "gate" => StepKind::Gate,
            "condition" => {
                let on_true = step.on_true.ok_or_else(|| {
                    SopError::InvalidDefinition(format!(
                        "Condition step '{}' missing on_true",
                        step.id
                    ))
                })?;
                let on_false = step.on_false.ok_or_else(|| {
                    SopError::InvalidDefinition(format!(
                        "Condition step '{}' missing on_false",
                        step.id
                    ))
                })?;
                StepKind::Condition { on_true, on_false }
            }
            other => {
                return Err(SopError::InvalidDefinition(format!(
                    "Unknown step type: '{other}'"
                )));
            }
        };

        steps.push(SopStep {
            id: step.id,
            name: step.name,
            kind,
            action: step.action,
            depends_on: step.depends_on,
        });
    }

    Ok(SopDefinition {
        name: raw.name,
        description: raw.description,
        steps,
    })
}

// ── Markdown format ──────────────────────────────────────

/// Parse an SOP definition from Markdown content.
///
/// Expected format:
/// ```markdown
/// # SOP Name
/// Description text here.
///
/// ## step-id: Step Name
/// - action: Do something
/// - depends_on: other-step-id
///
/// ## gate-id: Gate Name [GATE]
/// - action: Approve deployment
///
/// ## cond-id: Condition Name [CONDITION on_true=step-a on_false=step-b]
/// - action: Check if ready
/// ```
pub fn parse_markdown(content: &str) -> Result<SopDefinition, SopError> {
    let mut name = String::new();
    let mut description = String::new();
    let mut steps: Vec<SopStep> = Vec::new();
    let mut current_step: Option<PartialStep> = None;
    let mut in_description = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Top-level heading = SOP name
        if trimmed.starts_with("# ") && !trimmed.starts_with("## ") {
            name = trimmed.trim_start_matches("# ").trim().to_string();
            in_description = true;
            continue;
        }

        // Step heading
        if trimmed.starts_with("## ") {
            // Finalize previous step
            if let Some(step) = current_step.take() {
                steps.push(step.finalize());
            }
            in_description = false;

            let heading = trimmed.trim_start_matches("## ").trim();
            current_step = Some(parse_step_heading(heading)?);
            continue;
        }

        // Description lines (between # and first ##)
        if in_description && !trimmed.is_empty() {
            if !description.is_empty() {
                description.push(' ');
            }
            description.push_str(trimmed);
            continue;
        }

        // Bullet point lines under a step
        if let Some(ref mut step) = current_step {
            if let Some(rest) = trimmed.strip_prefix("- ") {
                if let Some(action) = rest.strip_prefix("action: ") {
                    step.action = action.trim().to_string();
                } else if let Some(deps) = rest.strip_prefix("depends_on: ") {
                    step.depends_on = deps.split(',').map(|s| s.trim().to_string()).collect();
                }
            }
        }
    }

    // Finalize last step
    if let Some(step) = current_step.take() {
        steps.push(step.finalize());
    }

    if name.is_empty() {
        return Err(SopError::InvalidDefinition(
            "Missing SOP name (# heading)".to_string(),
        ));
    }

    Ok(SopDefinition {
        name,
        description,
        steps,
    })
}

#[derive(Debug)]
struct PartialStep {
    id: String,
    name: String,
    kind: StepKind,
    action: String,
    depends_on: Vec<String>,
}

impl PartialStep {
    fn finalize(self) -> SopStep {
        SopStep {
            id: self.id,
            name: self.name,
            kind: self.kind,
            action: self.action,
            depends_on: self.depends_on,
        }
    }
}

fn parse_step_heading(heading: &str) -> Result<PartialStep, SopError> {
    // Extract kind markers: [GATE], [CONDITION on_true=x on_false=y]
    let kind;
    let clean_heading;

    if let Some(gate_start) = heading.find("[GATE]") {
        kind = StepKind::Gate;
        clean_heading = heading[..gate_start].trim().to_string();
    } else if let Some(cond_start) = heading.find("[CONDITION") {
        let cond_end = heading.find(']').unwrap_or(heading.len());
        let attrs = &heading[cond_start + "[CONDITION".len()..cond_end].trim();

        let on_true = extract_attr(attrs, "on_true").ok_or_else(|| {
            SopError::InvalidDefinition("CONDITION missing on_true attribute".to_string())
        })?;
        let on_false = extract_attr(attrs, "on_false").ok_or_else(|| {
            SopError::InvalidDefinition("CONDITION missing on_false attribute".to_string())
        })?;

        kind = StepKind::Condition { on_true, on_false };
        clean_heading = heading[..cond_start].trim().to_string();
    } else {
        kind = StepKind::Action;
        clean_heading = heading.to_string();
    }

    // Parse "id: Name"
    let (id, name) = if let Some(colon_pos) = clean_heading.find(':') {
        let id = clean_heading[..colon_pos].trim().to_string();
        let name = clean_heading[colon_pos + 1..].trim().to_string();
        (id, name)
    } else {
        let id = clean_heading.to_lowercase().replace(' ', "-");
        (id, clean_heading)
    };

    Ok(PartialStep {
        id,
        name,
        kind,
        action: String::new(),
        depends_on: Vec::new(),
    })
}

fn extract_attr(attrs: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    for token in attrs.split_whitespace() {
        if let Some(value) = token.strip_prefix(&prefix) {
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_toml() -> &'static str {
        r#"
name = "Deploy Service"
description = "Standard deployment procedure"

[[steps]]
id = "build"
name = "Build Artifacts"
type = "action"
action = "cargo build --release"

[[steps]]
id = "test"
name = "Run Tests"
type = "action"
action = "cargo test"
depends_on = ["build"]

[[steps]]
id = "approve"
name = "Approve Deployment"
type = "gate"
action = "Manual approval required"
depends_on = ["test"]

[[steps]]
id = "check-env"
name = "Check Environment"
type = "condition"
action = "Evaluate environment readiness"
on_true = "deploy"
on_false = "rollback"
depends_on = ["approve"]

[[steps]]
id = "deploy"
name = "Deploy to Production"
type = "action"
action = "kubectl apply -f deploy.yaml"
depends_on = ["check-env"]

[[steps]]
id = "rollback"
name = "Rollback Changes"
type = "action"
action = "kubectl rollout undo"
depends_on = ["check-env"]
"#
    }

    fn sample_markdown() -> &'static str {
        r#"# Deploy Service
Standard deployment procedure.

## build: Build Artifacts
- action: cargo build --release

## test: Run Tests
- action: cargo test
- depends_on: build

## approve: Approve Deployment [GATE]
- action: Manual approval required
- depends_on: test

## check-env: Check Environment [CONDITION on_true=deploy on_false=rollback]
- action: Evaluate environment readiness
- depends_on: approve

## deploy: Deploy to Production
- action: kubectl apply -f deploy.yaml
- depends_on: check-env

## rollback: Rollback Changes
- action: kubectl rollout undo
- depends_on: check-env
"#
    }

    #[test]
    fn test_parse_toml_full() {
        let sop = parse_toml(sample_toml()).unwrap();
        assert_eq!(sop.name, "Deploy Service");
        assert_eq!(sop.description, "Standard deployment procedure");
        assert_eq!(sop.steps.len(), 6);
    }

    #[test]
    fn test_parse_toml_step_types() {
        let sop = parse_toml(sample_toml()).unwrap();
        assert_eq!(sop.steps[0].kind, StepKind::Action);
        assert_eq!(sop.steps[2].kind, StepKind::Gate);
        assert!(matches!(
            &sop.steps[3].kind,
            StepKind::Condition {
                on_true,
                on_false
            } if on_true == "deploy" && on_false == "rollback"
        ));
    }

    #[test]
    fn test_parse_toml_dependencies() {
        let sop = parse_toml(sample_toml()).unwrap();
        assert!(sop.steps[0].depends_on.is_empty());
        assert_eq!(sop.steps[1].depends_on, vec!["build"]);
        assert_eq!(sop.steps[2].depends_on, vec!["test"]);
    }

    #[test]
    fn test_parse_toml_actions() {
        let sop = parse_toml(sample_toml()).unwrap();
        assert_eq!(sop.steps[0].action, "cargo build --release");
        assert_eq!(sop.steps[4].action, "kubectl apply -f deploy.yaml");
    }

    #[test]
    fn test_parse_toml_invalid() {
        assert!(parse_toml("not valid toml {{[").is_err());
    }

    #[test]
    fn test_parse_toml_missing_condition_on_true() {
        let toml_content = r#"
name = "Bad"
description = "Missing on_true"
[[steps]]
id = "s1"
name = "Check"
type = "condition"
action = "check"
on_false = "fallback"
"#;
        let err = parse_toml(toml_content).unwrap_err();
        assert!(err.to_string().contains("on_true"));
    }

    #[test]
    fn test_parse_toml_unknown_step_type() {
        let toml_content = r#"
name = "Bad"
description = "Unknown type"
[[steps]]
id = "s1"
name = "Mystery"
type = "mystery"
action = "do something"
"#;
        let err = parse_toml(toml_content).unwrap_err();
        assert!(err.to_string().contains("mystery"));
    }

    #[test]
    fn test_parse_markdown_full() {
        let sop = parse_markdown(sample_markdown()).unwrap();
        assert_eq!(sop.name, "Deploy Service");
        assert_eq!(sop.description, "Standard deployment procedure.");
        assert_eq!(sop.steps.len(), 6);
    }

    #[test]
    fn test_parse_markdown_step_types() {
        let sop = parse_markdown(sample_markdown()).unwrap();
        assert_eq!(sop.steps[0].kind, StepKind::Action);
        assert_eq!(sop.steps[2].kind, StepKind::Gate);
        assert!(matches!(
            &sop.steps[3].kind,
            StepKind::Condition {
                on_true,
                on_false
            } if on_true == "deploy" && on_false == "rollback"
        ));
    }

    #[test]
    fn test_parse_markdown_dependencies() {
        let sop = parse_markdown(sample_markdown()).unwrap();
        assert!(sop.steps[0].depends_on.is_empty());
        assert_eq!(sop.steps[1].depends_on, vec!["build"]);
    }

    #[test]
    fn test_parse_markdown_actions() {
        let sop = parse_markdown(sample_markdown()).unwrap();
        assert_eq!(sop.steps[0].action, "cargo build --release");
        assert_eq!(sop.steps[4].action, "kubectl apply -f deploy.yaml");
    }

    #[test]
    fn test_parse_markdown_no_name() {
        let md = "## step: Something\n- action: do it\n";
        let err = parse_markdown(md).unwrap_err();
        assert!(err.to_string().contains("Missing SOP name"));
    }

    #[test]
    fn test_parse_markdown_missing_condition_attr() {
        let md = "# Bad SOP\nDesc\n\n## s1: Check [CONDITION on_true=a]\n- action: check\n";
        let err = parse_markdown(md).unwrap_err();
        assert!(err.to_string().contains("on_false"));
    }

    #[test]
    fn test_step_kind_serde_roundtrip() {
        let action = StepKind::Action;
        let json = serde_json::to_string(&action).unwrap();
        let parsed: StepKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, action);

        let gate = StepKind::Gate;
        let json = serde_json::to_string(&gate).unwrap();
        let parsed: StepKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, gate);
    }

    #[test]
    fn test_condition_kind_serde() {
        let cond = StepKind::Condition {
            on_true: "yes".to_string(),
            on_false: "no".to_string(),
        };
        let json = serde_json::to_string(&cond).unwrap();
        let parsed: StepKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
    }

    #[test]
    fn test_sop_definition_serde() {
        let def = SopDefinition {
            name: "Test".to_string(),
            description: "A test SOP".to_string(),
            steps: vec![SopStep {
                id: "s1".to_string(),
                name: "Step 1".to_string(),
                kind: StepKind::Action,
                action: "do it".to_string(),
                depends_on: vec![],
            }],
        };
        let json = serde_json::to_string(&def).unwrap();
        let parsed: SopDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "Test");
        assert_eq!(parsed.steps.len(), 1);
    }

    #[test]
    fn test_parse_toml_minimal() {
        let toml_content = r#"
name = "Minimal"
description = "No steps"
"#;
        let sop = parse_toml(toml_content).unwrap();
        assert_eq!(sop.name, "Minimal");
        assert!(sop.steps.is_empty());
    }

    #[test]
    fn test_parse_toml_default_step_type() {
        let toml_content = r#"
name = "Default"
description = "Default type"
[[steps]]
id = "s1"
name = "Auto"
action = "do it"
"#;
        let sop = parse_toml(toml_content).unwrap();
        assert_eq!(sop.steps[0].kind, StepKind::Action);
    }

    #[test]
    fn test_parse_markdown_multiple_depends() {
        let md = "# Test SOP\nDesc\n\n## s3: Merge\n- action: merge\n- depends_on: s1, s2\n";
        let sop = parse_markdown(md).unwrap();
        assert_eq!(sop.steps[0].depends_on, vec!["s1", "s2"]);
    }
}
