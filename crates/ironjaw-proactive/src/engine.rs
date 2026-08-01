use crate::scheduler::{CronExpr, Scheduler};
use crate::store::ProactiveStore;
use crate::types::*;
use crate::workflow::{StepExecutor, WorkflowExecutor};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{error, info};

/// Configuration for the proactive engine.
#[derive(Debug, Clone, Deserialize)]
pub struct ProactiveConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_proactive_db")]
    pub database_path: String,
    #[serde(default = "default_check_interval")]
    pub check_interval_secs: u64,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_workflows: usize,
}

fn default_proactive_db() -> String {
    "data/proactive.db".to_string()
}

fn default_check_interval() -> u64 {
    60
}

fn default_max_concurrent() -> usize {
    3
}

impl Default for ProactiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            database_path: default_proactive_db(),
            check_interval_secs: default_check_interval(),
            max_concurrent_workflows: default_max_concurrent(),
        }
    }
}

/// The proactive engine orchestrates scheduling and workflow execution.
pub struct ProactiveEngine {
    store: ProactiveStore,
    config: ProactiveConfig,
    workflow_executor: WorkflowExecutor,
}

impl ProactiveEngine {
    pub fn new(store: ProactiveStore, config: ProactiveConfig) -> Self {
        let workflow_executor = WorkflowExecutor::new(store.clone());
        Self {
            store,
            config,
            workflow_executor,
        }
    }

    pub fn store(&self) -> &ProactiveStore {
        &self.store
    }

    pub fn config(&self) -> &ProactiveConfig {
        &self.config
    }

    pub fn workflow_executor(&self) -> &WorkflowExecutor {
        &self.workflow_executor
    }

    /// Start the scheduler background task. Returns the action receiver.
    pub fn start_scheduler(&self) -> mpsc::Receiver<ScheduleAction> {
        let (tx, rx) = mpsc::channel(64);
        let scheduler = Scheduler::new(self.store.clone(), self.config.check_interval_secs);

        tokio::spawn(async move {
            scheduler.run(tx).await;
        });

        info!(
            interval_secs = self.config.check_interval_secs,
            "Proactive scheduler spawned"
        );

        rx
    }

    // ── Schedule management ──────────────────────────────────

    pub async fn create_schedule(
        &self,
        name: &str,
        cron_expr: &str,
        action_type: ActionType,
        action_data: &serde_json::Value,
    ) -> anyhow::Result<Schedule> {
        // Validate cron expression
        if CronExpr::parse(cron_expr).is_none() {
            return Err(anyhow::anyhow!("Invalid cron expression: {cron_expr}"));
        }

        let id = uuid::Uuid::new_v4().to_string();
        self.store
            .create_schedule(&id, name, cron_expr, action_type, action_data)
            .await
    }

    pub async fn list_schedules(&self, enabled_only: bool) -> anyhow::Result<Vec<Schedule>> {
        self.store.list_schedules(enabled_only).await
    }

    pub async fn delete_schedule(&self, id: &str) -> anyhow::Result<bool> {
        self.store.delete_schedule(id).await
    }

    pub async fn toggle_schedule(
        &self,
        id: &str,
        enabled: bool,
    ) -> anyhow::Result<Option<Schedule>> {
        self.store.toggle_schedule(id, enabled).await?;
        self.store.get_schedule(id).await
    }

    // ── Workflow management ──────────────────────────────────

    pub async fn create_workflow(
        &self,
        name: &str,
        steps: Vec<WorkflowStep>,
    ) -> anyhow::Result<Workflow> {
        let id = uuid::Uuid::new_v4().to_string();
        self.store.create_workflow(&id, name, &steps).await
    }

    pub async fn get_workflow(&self, id: &str) -> anyhow::Result<Option<Workflow>> {
        self.store.get_workflow(id).await
    }

    pub async fn get_workflow_steps(&self, workflow_id: &str) -> anyhow::Result<Vec<WorkflowStep>> {
        self.store.get_workflow_steps(workflow_id).await
    }

    pub async fn list_workflows(&self) -> anyhow::Result<Vec<Workflow>> {
        self.store.list_workflows().await
    }

    pub async fn cancel_workflow(&self, id: &str) -> anyhow::Result<bool> {
        self.workflow_executor.cancel(id).await
    }

    pub async fn approve_workflow_step(
        &self,
        workflow_id: &str,
        step_id: &str,
    ) -> anyhow::Result<()> {
        self.workflow_executor
            .approve_step(workflow_id, step_id)
            .await
    }

    pub async fn deny_workflow_step(&self, workflow_id: &str, step_id: &str) -> anyhow::Result<()> {
        self.workflow_executor.deny_step(workflow_id, step_id).await
    }

    /// Execute a workflow using the provided step executor.
    pub async fn run_workflow(
        &self,
        workflow_id: &str,
        executor: &dyn StepExecutor,
    ) -> anyhow::Result<WorkflowStatus> {
        self.workflow_executor.execute(workflow_id, executor).await
    }

    /// Handle a schedule action (called from the action receiver loop).
    pub async fn handle_action(
        &self,
        action: ScheduleAction,
        executor: &dyn StepExecutor,
    ) -> anyhow::Result<()> {
        info!(
            schedule_id = %action.schedule_id,
            action_type = ?action.action_type,
            "Handling schedule action"
        );

        match action.action_type {
            ActionType::Tool => {
                let result = executor.execute_tool(&action.action_data).await;
                match result {
                    Ok(r) => {
                        info!(schedule_id = %action.schedule_id, result = %r, "Tool action completed")
                    }
                    Err(e) => {
                        error!(schedule_id = %action.schedule_id, error = %e, "Tool action failed")
                    }
                }
            }
            ActionType::Llm => {
                let result = executor.execute_llm(&action.action_data).await;
                match result {
                    Ok(r) => {
                        info!(schedule_id = %action.schedule_id, result = %r, "LLM action completed")
                    }
                    Err(e) => {
                        error!(schedule_id = %action.schedule_id, error = %e, "LLM action failed")
                    }
                }
            }
            ActionType::Workflow => {
                if let Some(wf_id) = action
                    .action_data
                    .get("workflow_id")
                    .and_then(|v| v.as_str())
                {
                    match self.run_workflow(wf_id, executor).await {
                        Ok(status) => {
                            info!(schedule_id = %action.schedule_id, ?status, "Workflow completed")
                        }
                        Err(e) => {
                            error!(schedule_id = %action.schedule_id, error = %e, "Workflow failed")
                        }
                    }
                } else {
                    error!(schedule_id = %action.schedule_id, "Workflow action missing workflow_id");
                }
            }
            ActionType::Event => {
                // Event actions are delivered to connected clients via the event channel
                // The handler in ironjaw-ws is responsible for pushing these
                info!(schedule_id = %action.schedule_id, "Event action — delegating to client push");
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::MockStepExecutor;

    async fn test_engine() -> ProactiveEngine {
        let store = ProactiveStore::open_memory().await.unwrap();
        ProactiveEngine::new(store, ProactiveConfig::default())
    }

    #[tokio::test]
    async fn test_create_schedule_valid() {
        let engine = test_engine().await;
        let sched = engine
            .create_schedule(
                "Morning",
                "0 8 * * *",
                ActionType::Llm,
                &serde_json::json!({"prompt": "briefing"}),
            )
            .await
            .unwrap();

        assert_eq!(sched.name, "Morning");
        assert!(sched.enabled);
    }

    #[tokio::test]
    async fn test_create_schedule_invalid_cron() {
        let engine = test_engine().await;
        let result = engine
            .create_schedule(
                "Bad",
                "not a cron",
                ActionType::Event,
                &serde_json::json!({}),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_and_delete_schedules() {
        let engine = test_engine().await;
        engine
            .create_schedule(
                "A",
                "*/5 * * * *",
                ActionType::Event,
                &serde_json::json!({}),
            )
            .await
            .unwrap();
        engine
            .create_schedule("B", "0 * * * *", ActionType::Tool, &serde_json::json!({}))
            .await
            .unwrap();

        let all = engine.list_schedules(false).await.unwrap();
        assert_eq!(all.len(), 2);

        engine.delete_schedule(&all[0].id).await.unwrap();
        let remaining = engine.list_schedules(false).await.unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[tokio::test]
    async fn test_toggle_schedule() {
        let engine = test_engine().await;
        let sched = engine
            .create_schedule(
                "Toggle",
                "* * * * *",
                ActionType::Event,
                &serde_json::json!({}),
            )
            .await
            .unwrap();

        let toggled = engine
            .toggle_schedule(&sched.id, false)
            .await
            .unwrap()
            .unwrap();
        assert!(!toggled.enabled);
    }

    #[tokio::test]
    async fn test_create_and_run_workflow() {
        let engine = test_engine().await;
        let steps = vec![WorkflowStep {
            id: "s1".to_string(),
            workflow_id: String::new(), // will be set by store
            name: "Do thing".to_string(),
            step_type: StepType::Tool,
            params: serde_json::json!({}),
            depends_on: vec![],
            status: StepStatus::Pending,
            result: None,
            started_at: None,
            completed_at: None,
        }];

        let wf = engine
            .create_workflow("Test workflow", steps)
            .await
            .unwrap();
        assert_eq!(wf.status, WorkflowStatus::Pending);

        let status = engine
            .run_workflow(&wf.id, &MockStepExecutor)
            .await
            .unwrap();
        assert_eq!(status, WorkflowStatus::Completed);
    }

    #[tokio::test]
    async fn test_cancel_workflow() {
        let engine = test_engine().await;
        let wf = engine.create_workflow("Cancel", vec![]).await.unwrap();
        engine
            .store()
            .update_workflow_status(&wf.id, WorkflowStatus::Running)
            .await
            .unwrap();

        assert!(engine.cancel_workflow(&wf.id).await.unwrap());
        let wf = engine.get_workflow(&wf.id).await.unwrap().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Failed);
    }

    #[tokio::test]
    async fn test_handle_tool_action() {
        let engine = test_engine().await;
        let action = ScheduleAction {
            schedule_id: "test".to_string(),
            action_type: ActionType::Tool,
            action_data: serde_json::json!({"tool": "host_status"}),
        };
        // Should not error
        engine
            .handle_action(action, &MockStepExecutor)
            .await
            .unwrap();
    }
}
