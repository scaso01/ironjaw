use crate::store::ProactiveStore;
use crate::types::*;
use tracing::{error, info, warn};

/// Maximum steps per workflow to prevent infinite loops.
const MAX_STEPS: usize = 100;

/// Maximum iterations of the main loop to prevent runaway execution.
const MAX_ITERATIONS: usize = 500;

/// Workflow DAG executor with checkpoints and approval gates.
pub struct WorkflowExecutor {
    store: ProactiveStore,
}

/// Callback trait for executing workflow step actions.
/// The engine provides an implementation that delegates to tools, LLM, etc.
#[async_trait::async_trait]
pub trait StepExecutor: Send + Sync {
    /// Execute a tool step. Returns JSON result.
    async fn execute_tool(&self, params: &serde_json::Value) -> anyhow::Result<serde_json::Value>;

    /// Execute an LLM step. Returns text response.
    async fn execute_llm(&self, params: &serde_json::Value) -> anyhow::Result<serde_json::Value>;

    /// Request approval for an approval gate. Returns true if approved.
    async fn request_approval(&self, step_id: &str, workflow_id: &str) -> anyhow::Result<bool>;

    /// Evaluate a condition. Returns true if condition is met.
    async fn evaluate_condition(
        &self,
        params: &serde_json::Value,
        previous_results: &serde_json::Value,
    ) -> anyhow::Result<bool>;
}

/// No-op executor for testing.
pub struct MockStepExecutor;

#[async_trait::async_trait]
impl StepExecutor for MockStepExecutor {
    async fn execute_tool(&self, _params: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({"mock": true}))
    }

    async fn execute_llm(&self, _params: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({"response": "mock response"}))
    }

    async fn request_approval(&self, _step_id: &str, _workflow_id: &str) -> anyhow::Result<bool> {
        Ok(true)
    }

    async fn evaluate_condition(
        &self,
        _params: &serde_json::Value,
        _previous_results: &serde_json::Value,
    ) -> anyhow::Result<bool> {
        Ok(true)
    }
}

impl WorkflowExecutor {
    pub fn new(store: ProactiveStore) -> Self {
        Self { store }
    }

    /// Execute a workflow from scratch or resume from last checkpoint.
    pub async fn execute(
        &self,
        workflow_id: &str,
        executor: &dyn StepExecutor,
    ) -> anyhow::Result<WorkflowStatus> {
        let workflow = self
            .store
            .get_workflow(workflow_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Workflow '{workflow_id}' not found"))?;

        match workflow.status {
            WorkflowStatus::Completed | WorkflowStatus::Failed => {
                return Ok(workflow.status);
            }
            _ => {}
        }

        // Mark as running
        self.store
            .update_workflow_status(workflow_id, WorkflowStatus::Running)
            .await?;

        let steps = self.store.get_workflow_steps(workflow_id).await?;

        if steps.len() > MAX_STEPS {
            self.store
                .update_workflow_status(workflow_id, WorkflowStatus::Failed)
                .await?;
            return Err(anyhow::anyhow!(
                "Workflow has {} steps, exceeding max of {}",
                steps.len(),
                MAX_STEPS
            ));
        }

        let mut iterations = 0;

        loop {
            iterations += 1;
            if iterations > MAX_ITERATIONS {
                warn!(
                    workflow_id = workflow_id,
                    "Workflow exceeded max iterations, marking failed"
                );
                self.store
                    .update_workflow_status(workflow_id, WorkflowStatus::Failed)
                    .await?;
                return Ok(WorkflowStatus::Failed);
            }

            // Reload steps to get current state
            let steps = self.store.get_workflow_steps(workflow_id).await?;

            // Find ready steps (all dependencies completed, status = pending)
            let ready: Vec<&WorkflowStep> = steps
                .iter()
                .filter(|s| s.status == StepStatus::Pending)
                .filter(|s| {
                    s.depends_on.iter().all(|dep_id| {
                        steps
                            .iter()
                            .find(|d| d.id == *dep_id)
                            .map(|d| d.status == StepStatus::Completed)
                            .unwrap_or(false)
                    })
                })
                .collect();

            if ready.is_empty() {
                // Check if all steps are done
                let all_done = steps.iter().all(|s| {
                    matches!(
                        s.status,
                        StepStatus::Completed | StepStatus::Skipped | StepStatus::Failed
                    )
                });

                if all_done {
                    let has_failures = steps.iter().any(|s| s.status == StepStatus::Failed);
                    let final_status = if has_failures {
                        WorkflowStatus::Failed
                    } else {
                        WorkflowStatus::Completed
                    };
                    self.store
                        .update_workflow_status(workflow_id, final_status)
                        .await?;
                    info!(
                        workflow_id = workflow_id,
                        status = ?final_status,
                        "Workflow finished"
                    );
                    return Ok(final_status);
                }

                // Check for approval-paused workflow
                let has_waiting = steps
                    .iter()
                    .any(|s| s.status == StepStatus::Running && s.step_type == StepType::Approval);

                if has_waiting {
                    self.store
                        .update_workflow_status(workflow_id, WorkflowStatus::Paused)
                        .await?;
                    return Ok(WorkflowStatus::Paused);
                }

                // Deadlock: pending steps exist but none are ready
                warn!(
                    workflow_id = workflow_id,
                    "Workflow deadlocked — pending steps with unresolved dependencies"
                );
                self.store
                    .update_workflow_status(workflow_id, WorkflowStatus::Failed)
                    .await?;
                return Ok(WorkflowStatus::Failed);
            }

            // Execute ready steps (one at a time for simplicity and checkpoint safety)
            for step in ready {
                let step_id = step.id.clone();

                // Mark running
                self.store
                    .update_step(&step_id, StepStatus::Running, None)
                    .await?;

                info!(
                    workflow_id = workflow_id,
                    step_id = %step_id,
                    step_name = %step.name,
                    step_type = ?step.step_type,
                    "Executing workflow step"
                );

                // Collect previous step results for condition evaluation
                let previous_results = self.collect_results(workflow_id).await?;

                let result = match step.step_type {
                    StepType::Tool => executor.execute_tool(&step.params).await,
                    StepType::Llm => executor.execute_llm(&step.params).await,
                    StepType::Approval => {
                        match executor.request_approval(&step_id, workflow_id).await {
                            Ok(true) => Ok(serde_json::json!({"approved": true})),
                            Ok(false) => {
                                // Not approved — pause workflow
                                self.store
                                    .update_step(
                                        &step_id,
                                        StepStatus::Running,
                                        Some(&serde_json::json!({"waiting": true})),
                                    )
                                    .await?;
                                self.store
                                    .update_workflow_status(workflow_id, WorkflowStatus::Paused)
                                    .await?;
                                return Ok(WorkflowStatus::Paused);
                            }
                            Err(e) => Err(e),
                        }
                    }
                    StepType::Condition => {
                        match executor
                            .evaluate_condition(&step.params, &previous_results)
                            .await
                        {
                            Ok(true) => Ok(serde_json::json!({"condition_met": true})),
                            Ok(false) => {
                                // Condition not met — skip dependent steps
                                self.store
                                    .update_step(
                                        &step_id,
                                        StepStatus::Skipped,
                                        Some(&serde_json::json!({"condition_met": false})),
                                    )
                                    .await?;
                                self.skip_dependents(workflow_id, &step_id).await?;
                                continue;
                            }
                            Err(e) => Err(e),
                        }
                    }
                };

                match result {
                    Ok(result_val) => {
                        self.store
                            .update_step(&step_id, StepStatus::Completed, Some(&result_val))
                            .await?;
                        info!(
                            step_id = %step_id,
                            "Step completed successfully"
                        );
                    }
                    Err(e) => {
                        error!(
                            step_id = %step_id,
                            error = %e,
                            "Step failed"
                        );
                        self.store
                            .update_step(
                                &step_id,
                                StepStatus::Failed,
                                Some(&serde_json::json!({"error": e.to_string()})),
                            )
                            .await?;
                    }
                }
            }
        }
    }

    /// Approve a paused approval step and resume the workflow.
    pub async fn approve_step(&self, workflow_id: &str, step_id: &str) -> anyhow::Result<()> {
        self.store
            .update_step(
                step_id,
                StepStatus::Completed,
                Some(&serde_json::json!({"approved": true})),
            )
            .await?;
        self.store
            .update_workflow_status(workflow_id, WorkflowStatus::Running)
            .await?;
        Ok(())
    }

    /// Deny a paused approval step and fail the workflow.
    pub async fn deny_step(&self, workflow_id: &str, step_id: &str) -> anyhow::Result<()> {
        self.store
            .update_step(
                step_id,
                StepStatus::Failed,
                Some(&serde_json::json!({"denied": true})),
            )
            .await?;
        self.store
            .update_workflow_status(workflow_id, WorkflowStatus::Failed)
            .await?;
        Ok(())
    }

    /// Cancel a running or paused workflow.
    pub async fn cancel(&self, workflow_id: &str) -> anyhow::Result<bool> {
        let wf = self.store.get_workflow(workflow_id).await?;
        match wf {
            Some(wf)
                if matches!(
                    wf.status,
                    WorkflowStatus::Running | WorkflowStatus::Paused | WorkflowStatus::Pending
                ) =>
            {
                self.store
                    .update_workflow_status(workflow_id, WorkflowStatus::Failed)
                    .await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn collect_results(&self, workflow_id: &str) -> anyhow::Result<serde_json::Value> {
        let steps = self.store.get_workflow_steps(workflow_id).await?;
        let mut results = serde_json::Map::new();
        for step in steps {
            if let Some(result) = step.result {
                results.insert(step.id, result);
            }
        }
        Ok(serde_json::Value::Object(results))
    }

    async fn skip_dependents(
        &self,
        workflow_id: &str,
        skipped_step_id: &str,
    ) -> anyhow::Result<()> {
        let steps = self.store.get_workflow_steps(workflow_id).await?;
        for step in &steps {
            if step.depends_on.contains(&skipped_step_id.to_string())
                && step.status == StepStatus::Pending
            {
                self.store
                    .update_step(
                        &step.id,
                        StepStatus::Skipped,
                        Some(&serde_json::json!({"reason": "dependency skipped"})),
                    )
                    .await?;
                // Recursively skip
                Box::pin(self.skip_dependents(workflow_id, &step.id)).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_step(id: &str, name: &str, step_type: StepType, deps: Vec<&str>) -> WorkflowStep {
        WorkflowStep {
            id: id.to_string(),
            workflow_id: "w1".to_string(),
            name: name.to_string(),
            step_type,
            params: serde_json::json!({}),
            depends_on: deps.into_iter().map(|s| s.to_string()).collect(),
            status: StepStatus::Pending,
            result: None,
            started_at: None,
            completed_at: None,
        }
    }

    #[tokio::test]
    async fn test_linear_workflow() {
        let store = ProactiveStore::open_memory().await.unwrap();
        let steps = vec![
            make_step("s1", "First", StepType::Tool, vec![]),
            make_step("s2", "Second", StepType::Tool, vec!["s1"]),
            make_step("s3", "Third", StepType::Tool, vec!["s2"]),
        ];
        store.create_workflow("w1", "Linear", &steps).await.unwrap();

        let exec = WorkflowExecutor::new(store.clone());
        let status = exec.execute("w1", &MockStepExecutor).await.unwrap();
        assert_eq!(status, WorkflowStatus::Completed);

        let final_steps = store.get_workflow_steps("w1").await.unwrap();
        assert!(final_steps
            .iter()
            .all(|s| s.status == StepStatus::Completed));
    }

    #[tokio::test]
    async fn test_parallel_steps() {
        let store = ProactiveStore::open_memory().await.unwrap();
        // s1 and s2 have no deps (can run in parallel), s3 depends on both
        let steps = vec![
            make_step("s1", "A", StepType::Tool, vec![]),
            make_step("s2", "B", StepType::Tool, vec![]),
            make_step("s3", "C", StepType::Tool, vec!["s1", "s2"]),
        ];
        store
            .create_workflow("w1", "Parallel", &steps)
            .await
            .unwrap();

        let exec = WorkflowExecutor::new(store.clone());
        let status = exec.execute("w1", &MockStepExecutor).await.unwrap();
        assert_eq!(status, WorkflowStatus::Completed);
    }

    #[tokio::test]
    async fn test_condition_skip() {
        let store = ProactiveStore::open_memory().await.unwrap();
        let steps = vec![
            make_step("s1", "Check", StepType::Condition, vec![]),
            make_step("s2", "Skippable", StepType::Tool, vec!["s1"]),
        ];
        store
            .create_workflow("w1", "Condition", &steps)
            .await
            .unwrap();

        // Use a special executor that returns false for conditions
        struct FailCondition;

        #[async_trait::async_trait]
        impl StepExecutor for FailCondition {
            async fn execute_tool(
                &self,
                _p: &serde_json::Value,
            ) -> anyhow::Result<serde_json::Value> {
                Ok(serde_json::json!({}))
            }
            async fn execute_llm(
                &self,
                _p: &serde_json::Value,
            ) -> anyhow::Result<serde_json::Value> {
                Ok(serde_json::json!({}))
            }
            async fn request_approval(&self, _s: &str, _w: &str) -> anyhow::Result<bool> {
                Ok(true)
            }
            async fn evaluate_condition(
                &self,
                _p: &serde_json::Value,
                _r: &serde_json::Value,
            ) -> anyhow::Result<bool> {
                Ok(false)
            }
        }

        let exec = WorkflowExecutor::new(store.clone());
        let status = exec.execute("w1", &FailCondition).await.unwrap();
        assert_eq!(status, WorkflowStatus::Completed);

        let final_steps = store.get_workflow_steps("w1").await.unwrap();
        assert_eq!(final_steps[0].status, StepStatus::Skipped);
        assert_eq!(final_steps[1].status, StepStatus::Skipped);
    }

    #[tokio::test]
    async fn test_already_completed() {
        let store = ProactiveStore::open_memory().await.unwrap();
        store.create_workflow("w1", "Done", &[]).await.unwrap();
        store
            .update_workflow_status("w1", WorkflowStatus::Completed)
            .await
            .unwrap();

        let exec = WorkflowExecutor::new(store.clone());
        let status = exec.execute("w1", &MockStepExecutor).await.unwrap();
        assert_eq!(status, WorkflowStatus::Completed);
    }

    #[tokio::test]
    async fn test_cancel_workflow() {
        let store = ProactiveStore::open_memory().await.unwrap();
        store.create_workflow("w1", "Cancel me", &[]).await.unwrap();
        store
            .update_workflow_status("w1", WorkflowStatus::Running)
            .await
            .unwrap();

        let exec = WorkflowExecutor::new(store.clone());
        assert!(exec.cancel("w1").await.unwrap());

        let wf = store.get_workflow("w1").await.unwrap().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Failed);
    }

    #[tokio::test]
    async fn test_failed_step() {
        let store = ProactiveStore::open_memory().await.unwrap();
        let steps = vec![make_step("s1", "Fail", StepType::Tool, vec![])];
        store
            .create_workflow("w1", "Failing", &steps)
            .await
            .unwrap();

        struct FailExecutor;

        #[async_trait::async_trait]
        impl StepExecutor for FailExecutor {
            async fn execute_tool(
                &self,
                _p: &serde_json::Value,
            ) -> anyhow::Result<serde_json::Value> {
                Err(anyhow::anyhow!("Tool failed"))
            }
            async fn execute_llm(
                &self,
                _p: &serde_json::Value,
            ) -> anyhow::Result<serde_json::Value> {
                Ok(serde_json::json!({}))
            }
            async fn request_approval(&self, _s: &str, _w: &str) -> anyhow::Result<bool> {
                Ok(true)
            }
            async fn evaluate_condition(
                &self,
                _p: &serde_json::Value,
                _r: &serde_json::Value,
            ) -> anyhow::Result<bool> {
                Ok(true)
            }
        }

        let exec = WorkflowExecutor::new(store.clone());
        let status = exec.execute("w1", &FailExecutor).await.unwrap();
        assert_eq!(status, WorkflowStatus::Failed);
    }
}
