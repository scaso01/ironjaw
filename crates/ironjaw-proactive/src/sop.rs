use crate::sop_parser::{SopDefinition, SopError, SopStep, StepKind};
use std::collections::HashMap;

/// Execution status for a single SOP step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Completed,
    Failed(String),
    WaitingApproval,
}

/// Result of executing the next step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SopStepResult {
    /// Step completed successfully.
    Completed { step_id: String },
    /// Step requires HITL approval before continuing.
    NeedsApproval { step_id: String, prompt: String },
    /// All steps have been executed.
    Finished,
    /// Step failed with an error.
    Error { step_id: String, error: String },
}

/// Progress snapshot of an SOP execution.
#[derive(Debug, Clone)]
pub struct SopProgress {
    pub current_step: Option<String>,
    pub completed: usize,
    pub total: usize,
    pub percentage: f64,
}

/// Runtime state for an SOP execution.
#[derive(Debug)]
pub struct SopState {
    pub current_step_idx: usize,
    pub step_statuses: HashMap<String, StepStatus>,
    pub paused: bool,
}

impl SopState {
    fn new(definition: &SopDefinition) -> Self {
        let mut step_statuses = HashMap::new();
        for step in &definition.steps {
            step_statuses.insert(step.id.clone(), StepStatus::Pending);
        }
        Self {
            current_step_idx: 0,
            step_statuses,
            paused: false,
        }
    }
}

/// Engine that walks through an SOP definition step by step.
pub struct SopEngine {
    pub definition: SopDefinition,
    pub state: SopState,
}

impl SopEngine {
    /// Create a new SOP engine from a definition.
    pub fn new(definition: SopDefinition) -> Self {
        let state = SopState::new(&definition);
        Self { definition, state }
    }

    /// Execute the next pending step. Returns the result of that step.
    pub fn execute_next(&mut self) -> Result<SopStepResult, SopError> {
        if self.state.paused {
            return Err(SopError::Paused);
        }

        // Find the next pending step
        let next_idx = self.find_next_pending();
        let Some(idx) = next_idx else {
            return Ok(SopStepResult::Finished);
        };

        self.state.current_step_idx = idx;
        let step = self.definition.steps[idx].clone();

        // Mark as running
        self.state
            .step_statuses
            .insert(step.id.clone(), StepStatus::Running);

        match &step.kind {
            StepKind::Action => {
                // Auto-execute: mark completed
                self.state
                    .step_statuses
                    .insert(step.id.clone(), StepStatus::Completed);
                Ok(SopStepResult::Completed {
                    step_id: step.id.clone(),
                })
            }
            StepKind::Gate => {
                // Gate: pause for approval
                self.state
                    .step_statuses
                    .insert(step.id.clone(), StepStatus::WaitingApproval);
                self.state.paused = true;
                Ok(SopStepResult::NeedsApproval {
                    step_id: step.id.clone(),
                    prompt: step.action.clone(),
                })
            }
            StepKind::Condition { on_true, on_false } => {
                // Condition: for now, evaluate as "action" text.
                // The action field is treated as a condition description.
                // In a real system this would delegate to an evaluator.
                // Mark completed and let the caller decide routing.
                self.state
                    .step_statuses
                    .insert(step.id.clone(), StepStatus::Completed);
                // Store branch targets for the caller to inspect
                let _ = (on_true, on_false);
                Ok(SopStepResult::Completed {
                    step_id: step.id.clone(),
                })
            }
        }
    }

    /// Approve a gate step and resume execution.
    pub fn approve_gate(&mut self, step_id: &str) -> Result<(), SopError> {
        let status = self
            .state
            .step_statuses
            .get(step_id)
            .ok_or_else(|| SopError::StepNotFound(step_id.to_string()))?;

        if *status != StepStatus::WaitingApproval {
            return Err(SopError::GateNotPending(step_id.to_string()));
        }

        self.state
            .step_statuses
            .insert(step_id.to_string(), StepStatus::Completed);
        self.state.paused = false;
        Ok(())
    }

    /// Mark a step as failed with an error message.
    pub fn fail_step(&mut self, step_id: &str, error: String) -> Result<(), SopError> {
        if !self.state.step_statuses.contains_key(step_id) {
            return Err(SopError::StepNotFound(step_id.to_string()));
        }
        self.state
            .step_statuses
            .insert(step_id.to_string(), StepStatus::Failed(error));
        Ok(())
    }

    /// Get progress snapshot.
    pub fn get_progress(&self) -> SopProgress {
        let total = self.definition.steps.len();
        let completed = self
            .state
            .step_statuses
            .values()
            .filter(|s| matches!(s, StepStatus::Completed))
            .count();
        let percentage = if total == 0 {
            100.0
        } else {
            (completed as f64 / total as f64) * 100.0
        };
        let current_step = self
            .definition
            .steps
            .get(self.state.current_step_idx)
            .map(|s| s.id.clone());

        SopProgress {
            current_step,
            completed,
            total,
            percentage,
        }
    }

    /// Pause execution.
    pub fn pause(&mut self) {
        self.state.paused = true;
    }

    /// Resume execution.
    pub fn resume(&mut self) {
        self.state.paused = false;
    }

    /// Check if all steps are completed.
    pub fn is_finished(&self) -> bool {
        self.state
            .step_statuses
            .values()
            .all(|s| matches!(s, StepStatus::Completed | StepStatus::Failed(_)))
    }

    /// Find the next step index that is still pending.
    fn find_next_pending(&self) -> Option<usize> {
        self.definition.steps.iter().position(|step| {
            matches!(
                self.state.step_statuses.get(&step.id),
                Some(StepStatus::Pending)
            )
        })
    }

    /// Get a step by ID.
    pub fn get_step(&self, step_id: &str) -> Option<&SopStep> {
        self.definition.steps.iter().find(|s| s.id == step_id)
    }

    /// Get the status of a step.
    pub fn get_step_status(&self, step_id: &str) -> Option<&StepStatus> {
        self.state.step_statuses.get(step_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sop_parser;

    fn simple_sop() -> SopDefinition {
        SopDefinition {
            name: "Test SOP".to_string(),
            description: "A test procedure".to_string(),
            steps: vec![
                SopStep {
                    id: "s1".to_string(),
                    name: "Step 1".to_string(),
                    kind: StepKind::Action,
                    action: "Do thing 1".to_string(),
                    depends_on: vec![],
                },
                SopStep {
                    id: "s2".to_string(),
                    name: "Step 2".to_string(),
                    kind: StepKind::Action,
                    action: "Do thing 2".to_string(),
                    depends_on: vec!["s1".to_string()],
                },
                SopStep {
                    id: "s3".to_string(),
                    name: "Step 3".to_string(),
                    kind: StepKind::Action,
                    action: "Do thing 3".to_string(),
                    depends_on: vec!["s2".to_string()],
                },
            ],
        }
    }

    fn gate_sop() -> SopDefinition {
        SopDefinition {
            name: "Gate SOP".to_string(),
            description: "SOP with a gate".to_string(),
            steps: vec![
                SopStep {
                    id: "build".to_string(),
                    name: "Build".to_string(),
                    kind: StepKind::Action,
                    action: "cargo build".to_string(),
                    depends_on: vec![],
                },
                SopStep {
                    id: "approve".to_string(),
                    name: "Approve".to_string(),
                    kind: StepKind::Gate,
                    action: "Please approve the deployment".to_string(),
                    depends_on: vec!["build".to_string()],
                },
                SopStep {
                    id: "deploy".to_string(),
                    name: "Deploy".to_string(),
                    kind: StepKind::Action,
                    action: "kubectl apply".to_string(),
                    depends_on: vec!["approve".to_string()],
                },
            ],
        }
    }

    fn condition_sop() -> SopDefinition {
        SopDefinition {
            name: "Condition SOP".to_string(),
            description: "SOP with a condition".to_string(),
            steps: vec![
                SopStep {
                    id: "check".to_string(),
                    name: "Check".to_string(),
                    kind: StepKind::Condition {
                        on_true: "proceed".to_string(),
                        on_false: "abort".to_string(),
                    },
                    action: "Is environment ready?".to_string(),
                    depends_on: vec![],
                },
                SopStep {
                    id: "proceed".to_string(),
                    name: "Proceed".to_string(),
                    kind: StepKind::Action,
                    action: "Continue work".to_string(),
                    depends_on: vec!["check".to_string()],
                },
            ],
        }
    }

    #[test]
    fn test_execute_linear_steps() {
        let mut engine = SopEngine::new(simple_sop());

        let r1 = engine.execute_next().unwrap();
        assert_eq!(
            r1,
            SopStepResult::Completed {
                step_id: "s1".to_string()
            }
        );

        let r2 = engine.execute_next().unwrap();
        assert_eq!(
            r2,
            SopStepResult::Completed {
                step_id: "s2".to_string()
            }
        );

        let r3 = engine.execute_next().unwrap();
        assert_eq!(
            r3,
            SopStepResult::Completed {
                step_id: "s3".to_string()
            }
        );

        let r4 = engine.execute_next().unwrap();
        assert_eq!(r4, SopStepResult::Finished);
    }

    #[test]
    fn test_gate_pauses_execution() {
        let mut engine = SopEngine::new(gate_sop());

        // Execute build
        let r1 = engine.execute_next().unwrap();
        assert_eq!(
            r1,
            SopStepResult::Completed {
                step_id: "build".to_string()
            }
        );

        // Execute gate — should pause
        let r2 = engine.execute_next().unwrap();
        assert_eq!(
            r2,
            SopStepResult::NeedsApproval {
                step_id: "approve".to_string(),
                prompt: "Please approve the deployment".to_string(),
            }
        );

        // Trying to execute while paused should error
        let err = engine.execute_next().unwrap_err();
        assert!(matches!(err, SopError::Paused));
    }

    #[test]
    fn test_approve_gate_and_resume() {
        let mut engine = SopEngine::new(gate_sop());

        engine.execute_next().unwrap(); // build
        engine.execute_next().unwrap(); // gate (pauses)

        // Approve
        engine.approve_gate("approve").unwrap();
        assert!(!engine.state.paused);
        assert_eq!(
            engine.get_step_status("approve"),
            Some(&StepStatus::Completed)
        );

        // Continue to deploy
        let r3 = engine.execute_next().unwrap();
        assert_eq!(
            r3,
            SopStepResult::Completed {
                step_id: "deploy".to_string()
            }
        );
    }

    #[test]
    fn test_approve_nonexistent_step() {
        let mut engine = SopEngine::new(simple_sop());
        let err = engine.approve_gate("nonexistent").unwrap_err();
        assert!(matches!(err, SopError::StepNotFound(_)));
    }

    #[test]
    fn test_approve_non_gate_step() {
        let mut engine = SopEngine::new(simple_sop());
        engine.execute_next().unwrap(); // s1 completed
        let err = engine.approve_gate("s1").unwrap_err();
        assert!(matches!(err, SopError::GateNotPending(_)));
    }

    #[test]
    fn test_condition_step_completes() {
        let mut engine = SopEngine::new(condition_sop());
        let r1 = engine.execute_next().unwrap();
        assert_eq!(
            r1,
            SopStepResult::Completed {
                step_id: "check".to_string()
            }
        );
    }

    #[test]
    fn test_progress_tracking() {
        let mut engine = SopEngine::new(simple_sop());

        let p0 = engine.get_progress();
        assert_eq!(p0.completed, 0);
        assert_eq!(p0.total, 3);
        assert!((p0.percentage - 0.0).abs() < f64::EPSILON);

        engine.execute_next().unwrap();
        let p1 = engine.get_progress();
        assert_eq!(p1.completed, 1);
        assert!((p1.percentage - 100.0 / 3.0).abs() < 0.1);

        engine.execute_next().unwrap();
        engine.execute_next().unwrap();
        let p3 = engine.get_progress();
        assert_eq!(p3.completed, 3);
        assert!((p3.percentage - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_progress_empty_sop() {
        let engine = SopEngine::new(SopDefinition {
            name: "Empty".to_string(),
            description: "Nothing".to_string(),
            steps: vec![],
        });
        let progress = engine.get_progress();
        assert_eq!(progress.total, 0);
        assert!((progress.percentage - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_pause_and_resume() {
        let mut engine = SopEngine::new(simple_sop());
        engine.execute_next().unwrap();

        engine.pause();
        assert!(engine.state.paused);
        let err = engine.execute_next().unwrap_err();
        assert!(matches!(err, SopError::Paused));

        engine.resume();
        assert!(!engine.state.paused);
        let r = engine.execute_next().unwrap();
        assert_eq!(
            r,
            SopStepResult::Completed {
                step_id: "s2".to_string()
            }
        );
    }

    #[test]
    fn test_fail_step() {
        let mut engine = SopEngine::new(simple_sop());
        engine
            .fail_step("s1", "Something went wrong".to_string())
            .unwrap();
        assert!(matches!(
            engine.get_step_status("s1"),
            Some(StepStatus::Failed(_))
        ));
    }

    #[test]
    fn test_fail_nonexistent_step() {
        let mut engine = SopEngine::new(simple_sop());
        let err = engine.fail_step("nope", "error".to_string()).unwrap_err();
        assert!(matches!(err, SopError::StepNotFound(_)));
    }

    #[test]
    fn test_is_finished() {
        let mut engine = SopEngine::new(simple_sop());
        assert!(!engine.is_finished());

        engine.execute_next().unwrap();
        engine.execute_next().unwrap();
        engine.execute_next().unwrap();
        assert!(engine.is_finished());
    }

    #[test]
    fn test_get_step() {
        let engine = SopEngine::new(simple_sop());
        let step = engine.get_step("s2").unwrap();
        assert_eq!(step.name, "Step 2");
        assert!(engine.get_step("nonexistent").is_none());
    }

    #[test]
    fn test_initial_state() {
        let engine = SopEngine::new(simple_sop());
        assert_eq!(engine.state.current_step_idx, 0);
        assert!(!engine.state.paused);
        for step in &engine.definition.steps {
            assert_eq!(engine.get_step_status(&step.id), Some(&StepStatus::Pending));
        }
    }

    #[test]
    fn test_from_toml_parse_and_execute() {
        let toml_content = r#"
name = "Quick SOP"
description = "Fast test"
[[steps]]
id = "a"
name = "Alpha"
type = "action"
action = "first"
[[steps]]
id = "b"
name = "Beta"
type = "action"
action = "second"
depends_on = ["a"]
"#;
        let def = sop_parser::parse_toml(toml_content).unwrap();
        let mut engine = SopEngine::new(def);
        assert_eq!(
            engine.execute_next().unwrap(),
            SopStepResult::Completed {
                step_id: "a".to_string()
            }
        );
        assert_eq!(
            engine.execute_next().unwrap(),
            SopStepResult::Completed {
                step_id: "b".to_string()
            }
        );
        assert_eq!(engine.execute_next().unwrap(), SopStepResult::Finished);
    }

    #[test]
    fn test_from_markdown_parse_and_execute() {
        let md = "# Quick\nFast\n\n## a: Alpha\n- action: first\n\n## b: Beta\n- action: second\n- depends_on: a\n";
        let def = sop_parser::parse_markdown(md).unwrap();
        let mut engine = SopEngine::new(def);
        assert_eq!(
            engine.execute_next().unwrap(),
            SopStepResult::Completed {
                step_id: "a".to_string()
            }
        );
        assert_eq!(
            engine.execute_next().unwrap(),
            SopStepResult::Completed {
                step_id: "b".to_string()
            }
        );
    }

    #[test]
    fn test_error_variant_display() {
        let err = SopError::StepNotFound("xyz".to_string());
        assert_eq!(err.to_string(), "step not found: xyz");

        let err = SopError::GateNotPending("abc".to_string());
        assert_eq!(err.to_string(), "gate not pending approval: abc");

        let err = SopError::Paused;
        assert_eq!(err.to_string(), "SOP is paused");

        let err = SopError::Finished;
        assert_eq!(err.to_string(), "SOP already finished");
    }
}
