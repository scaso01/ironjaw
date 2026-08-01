use crate::types::*;
use chrono::NaiveDateTime;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;
use std::path::Path;
use tracing::info;

/// SQLite-backed store for schedules and workflows.
#[derive(Clone)]
pub struct ProactiveStore {
    pool: SqlitePool,
}

impl ProactiveStore {
    pub async fn open(db_path: &str) -> anyhow::Result<Self> {
        if let Some(parent) = Path::new(db_path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        let url = format!("sqlite:{db_path}?mode=rwc");
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await?;

        sqlx::query(include_str!("../migrations/001_create_proactive.sql"))
            .execute(&pool)
            .await?;

        info!(path = db_path, "Proactive store opened");
        Ok(Self { pool })
    }

    pub async fn open_memory() -> anyhow::Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;

        sqlx::query(include_str!("../migrations/001_create_proactive.sql"))
            .execute(&pool)
            .await?;

        Ok(Self { pool })
    }

    // ── Schedule CRUD ────────────────────────────────────────

    pub async fn create_schedule(
        &self,
        id: &str,
        name: &str,
        cron_expr: &str,
        action_type: ActionType,
        action_data: &serde_json::Value,
    ) -> anyhow::Result<Schedule> {
        let action_type_str = action_type.as_str();
        let action_data_str = serde_json::to_string(action_data)?;

        sqlx::query(
            "INSERT INTO schedules (id, name, cron_expr, action_type, action_data)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(name)
        .bind(cron_expr)
        .bind(action_type_str)
        .bind(&action_data_str)
        .execute(&self.pool)
        .await?;

        self.get_schedule(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Schedule not found after insert"))
    }

    pub async fn get_schedule(&self, id: &str) -> anyhow::Result<Option<Schedule>> {
        #[allow(clippy::type_complexity)]
        let row: Option<(
            String,
            String,
            String,
            String,
            String,
            bool,
            Option<String>,
            Option<String>,
            String,
        )> = sqlx::query_as(
            "SELECT id, name, cron_expr, action_type, action_data, enabled, last_run, next_run, created_at
             FROM schedules WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some((
                id,
                name,
                cron_expr,
                action_type,
                action_data,
                enabled,
                last_run,
                next_run,
                created_at,
            )) => Ok(Some(Schedule {
                id,
                name,
                cron_expr,
                action_type: ActionType::from_str(&action_type).unwrap_or(ActionType::Event),
                action_data: serde_json::from_str(&action_data)?,
                enabled,
                last_run: last_run.and_then(|s| parse_datetime(&s)),
                next_run: next_run.and_then(|s| parse_datetime(&s)),
                created_at: parse_datetime(&created_at)
                    .unwrap_or_else(|| chrono::Utc::now().naive_utc()),
            })),
            None => Ok(None),
        }
    }

    pub async fn list_schedules(&self, enabled_only: bool) -> anyhow::Result<Vec<Schedule>> {
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            String,
            String,
            String,
            String,
            String,
            bool,
            Option<String>,
            Option<String>,
            String,
        )> = if enabled_only {
            sqlx::query_as(
                "SELECT id, name, cron_expr, action_type, action_data, enabled, last_run, next_run, created_at
                 FROM schedules WHERE enabled = 1 ORDER BY name",
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as(
                "SELECT id, name, cron_expr, action_type, action_data, enabled, last_run, next_run, created_at
                 FROM schedules ORDER BY name",
            )
            .fetch_all(&self.pool)
            .await?
        };

        let mut schedules = Vec::new();
        for (
            id,
            name,
            cron_expr,
            action_type,
            action_data,
            enabled,
            last_run,
            next_run,
            created_at,
        ) in rows
        {
            schedules.push(Schedule {
                id,
                name,
                cron_expr,
                action_type: ActionType::from_str(&action_type).unwrap_or(ActionType::Event),
                action_data: serde_json::from_str(&action_data)?,
                enabled,
                last_run: last_run.and_then(|s| parse_datetime(&s)),
                next_run: next_run.and_then(|s| parse_datetime(&s)),
                created_at: parse_datetime(&created_at)
                    .unwrap_or_else(|| chrono::Utc::now().naive_utc()),
            });
        }
        Ok(schedules)
    }

    pub async fn delete_schedule(&self, id: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM schedules WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn toggle_schedule(&self, id: &str, enabled: bool) -> anyhow::Result<bool> {
        let result = sqlx::query("UPDATE schedules SET enabled = ? WHERE id = ?")
            .bind(enabled)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn update_last_run(
        &self,
        id: &str,
        last_run: NaiveDateTime,
        next_run: Option<NaiveDateTime>,
    ) -> anyhow::Result<()> {
        let last_str = last_run.format("%Y-%m-%dT%H:%M:%S%.3f").to_string();
        let next_str = next_run.map(|n| n.format("%Y-%m-%dT%H:%M:%S%.3f").to_string());

        sqlx::query("UPDATE schedules SET last_run = ?, next_run = ? WHERE id = ?")
            .bind(&last_str)
            .bind(&next_str)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Workflow CRUD ────────────────────────────────────────

    pub async fn create_workflow(
        &self,
        id: &str,
        name: &str,
        steps: &[WorkflowStep],
    ) -> anyhow::Result<Workflow> {
        let steps_json = serde_json::to_string(steps)?;

        sqlx::query("INSERT INTO workflows (id, name, steps_json) VALUES (?, ?, ?)")
            .bind(id)
            .bind(name)
            .bind(&steps_json)
            .execute(&self.pool)
            .await?;

        // Insert individual steps
        for step in steps {
            let deps = if step.depends_on.is_empty() {
                None
            } else {
                Some(step.depends_on.join(","))
            };

            sqlx::query(
                "INSERT INTO workflow_steps (id, workflow_id, name, step_type, params_json, depends_on)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&step.id)
            .bind(id)
            .bind(&step.name)
            .bind(step.step_type.as_str())
            .bind(serde_json::to_string(&step.params)?)
            .bind(&deps)
            .execute(&self.pool)
            .await?;
        }

        self.get_workflow(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Workflow not found after insert"))
    }

    pub async fn get_workflow(&self, id: &str) -> anyhow::Result<Option<Workflow>> {
        let row: Option<(String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT id, name, status, state_json, created_at, updated_at
             FROM workflows WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some((id, name, status, state_json, created_at, updated_at)) => Ok(Some(Workflow {
                id,
                name,
                status: WorkflowStatus::from_str(&status).unwrap_or(WorkflowStatus::Pending),
                state: serde_json::from_str(&state_json)?,
                created_at: parse_datetime(&created_at)
                    .unwrap_or_else(|| chrono::Utc::now().naive_utc()),
                updated_at: parse_datetime(&updated_at)
                    .unwrap_or_else(|| chrono::Utc::now().naive_utc()),
            })),
            None => Ok(None),
        }
    }

    pub async fn update_workflow_status(
        &self,
        id: &str,
        status: WorkflowStatus,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE workflows SET status = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now')
             WHERE id = ?",
        )
        .bind(status.as_str())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn get_workflow_steps(&self, workflow_id: &str) -> anyhow::Result<Vec<WorkflowStep>> {
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT id, workflow_id, name, step_type, params_json, depends_on,
                    status, result_json, started_at, completed_at
             FROM workflow_steps WHERE workflow_id = ?",
        )
        .bind(workflow_id)
        .fetch_all(&self.pool)
        .await?;

        let mut steps = Vec::new();
        for (
            id,
            wf_id,
            name,
            step_type,
            params_json,
            depends_on,
            status,
            result_json,
            started_at,
            completed_at,
        ) in rows
        {
            steps.push(WorkflowStep {
                id,
                workflow_id: wf_id,
                name,
                step_type: StepType::from_str(&step_type).unwrap_or(StepType::Tool),
                params: serde_json::from_str(&params_json)?,
                depends_on: depends_on
                    .map(|d| d.split(',').map(|s| s.trim().to_string()).collect())
                    .unwrap_or_default(),
                status: StepStatus::from_str(&status).unwrap_or(StepStatus::Pending),
                result: result_json.and_then(|r| serde_json::from_str(&r).ok()),
                started_at: started_at.and_then(|s| parse_datetime(&s)),
                completed_at: completed_at.and_then(|s| parse_datetime(&s)),
            });
        }
        Ok(steps)
    }

    pub async fn update_step(
        &self,
        step_id: &str,
        status: StepStatus,
        result: Option<&serde_json::Value>,
    ) -> anyhow::Result<bool> {
        let result_json = result.map(|r| serde_json::to_string(r).unwrap_or_default());

        let now = chrono::Utc::now()
            .naive_utc()
            .format("%Y-%m-%dT%H:%M:%S%.3f")
            .to_string();

        let query = match status {
            StepStatus::Running => {
                sqlx::query(
                    "UPDATE workflow_steps SET status = ?, started_at = ?, result_json = ? WHERE id = ?",
                )
                .bind(status.as_str())
                .bind(&now)
                .bind(&result_json)
                .bind(step_id)
                .execute(&self.pool)
                .await?
            }
            StepStatus::Completed | StepStatus::Failed | StepStatus::Skipped => {
                sqlx::query(
                    "UPDATE workflow_steps SET status = ?, completed_at = ?, result_json = ? WHERE id = ?",
                )
                .bind(status.as_str())
                .bind(&now)
                .bind(&result_json)
                .bind(step_id)
                .execute(&self.pool)
                .await?
            }
            _ => {
                sqlx::query(
                    "UPDATE workflow_steps SET status = ?, result_json = ? WHERE id = ?",
                )
                .bind(status.as_str())
                .bind(&result_json)
                .bind(step_id)
                .execute(&self.pool)
                .await?
            }
        };
        Ok(query.rows_affected() > 0)
    }

    pub async fn list_workflows(&self) -> anyhow::Result<Vec<Workflow>> {
        let rows: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT id, name, status, state_json, created_at, updated_at
             FROM workflows ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut workflows = Vec::new();
        for (id, name, status, state_json, created_at, updated_at) in rows {
            workflows.push(Workflow {
                id,
                name,
                status: WorkflowStatus::from_str(&status).unwrap_or(WorkflowStatus::Pending),
                state: serde_json::from_str(&state_json)?,
                created_at: parse_datetime(&created_at)
                    .unwrap_or_else(|| chrono::Utc::now().naive_utc()),
                updated_at: parse_datetime(&updated_at)
                    .unwrap_or_else(|| chrono::Utc::now().naive_utc()),
            });
        }
        Ok(workflows)
    }
}

fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| {
            s.parse::<chrono::DateTime<chrono::Utc>>()
                .map(|dt| dt.naive_utc())
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> ProactiveStore {
        ProactiveStore::open_memory().await.unwrap()
    }

    #[tokio::test]
    async fn test_create_and_get_schedule() {
        let store = test_store().await;
        let sched = store
            .create_schedule(
                "s1",
                "Morning briefing",
                "0 8 * * *",
                ActionType::Llm,
                &serde_json::json!({"prompt": "Give me a morning briefing"}),
            )
            .await
            .unwrap();

        assert_eq!(sched.id, "s1");
        assert_eq!(sched.name, "Morning briefing");
        assert_eq!(sched.cron_expr, "0 8 * * *");
        assert_eq!(sched.action_type, ActionType::Llm);
        assert!(sched.enabled);

        let fetched = store.get_schedule("s1").await.unwrap().unwrap();
        assert_eq!(fetched.name, "Morning briefing");
    }

    #[tokio::test]
    async fn test_list_schedules() {
        let store = test_store().await;
        store
            .create_schedule(
                "s1",
                "Alpha",
                "*/5 * * * *",
                ActionType::Event,
                &serde_json::json!({}),
            )
            .await
            .unwrap();
        store
            .create_schedule(
                "s2",
                "Beta",
                "0 * * * *",
                ActionType::Tool,
                &serde_json::json!({}),
            )
            .await
            .unwrap();

        let all = store.list_schedules(false).await.unwrap();
        assert_eq!(all.len(), 2);

        // Disable one
        store.toggle_schedule("s2", false).await.unwrap();
        let enabled = store.list_schedules(true).await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].id, "s1");
    }

    #[tokio::test]
    async fn test_delete_schedule() {
        let store = test_store().await;
        store
            .create_schedule(
                "s1",
                "Temp",
                "* * * * *",
                ActionType::Event,
                &serde_json::json!({}),
            )
            .await
            .unwrap();

        assert!(store.delete_schedule("s1").await.unwrap());
        assert!(!store.delete_schedule("s1").await.unwrap()); // already gone
        assert!(store.get_schedule("s1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_toggle_schedule() {
        let store = test_store().await;
        store
            .create_schedule(
                "s1",
                "Toggle",
                "* * * * *",
                ActionType::Event,
                &serde_json::json!({}),
            )
            .await
            .unwrap();

        store.toggle_schedule("s1", false).await.unwrap();
        let s = store.get_schedule("s1").await.unwrap().unwrap();
        assert!(!s.enabled);

        store.toggle_schedule("s1", true).await.unwrap();
        let s = store.get_schedule("s1").await.unwrap().unwrap();
        assert!(s.enabled);
    }

    #[tokio::test]
    async fn test_update_last_run() {
        let store = test_store().await;
        store
            .create_schedule(
                "s1",
                "Run",
                "* * * * *",
                ActionType::Event,
                &serde_json::json!({}),
            )
            .await
            .unwrap();

        let now = chrono::Utc::now().naive_utc();
        store.update_last_run("s1", now, Some(now)).await.unwrap();

        let s = store.get_schedule("s1").await.unwrap().unwrap();
        assert!(s.last_run.is_some());
        assert!(s.next_run.is_some());
    }

    #[tokio::test]
    async fn test_create_and_get_workflow() {
        let store = test_store().await;
        let steps = vec![
            WorkflowStep {
                id: "step1".to_string(),
                workflow_id: "w1".to_string(),
                name: "Check status".to_string(),
                step_type: StepType::Tool,
                params: serde_json::json!({"tool": "host_status"}),
                depends_on: vec![],
                status: StepStatus::Pending,
                result: None,
                started_at: None,
                completed_at: None,
            },
            WorkflowStep {
                id: "step2".to_string(),
                workflow_id: "w1".to_string(),
                name: "Analyze".to_string(),
                step_type: StepType::Llm,
                params: serde_json::json!({"prompt": "Analyze the status"}),
                depends_on: vec!["step1".to_string()],
                status: StepStatus::Pending,
                result: None,
                started_at: None,
                completed_at: None,
            },
        ];

        let wf = store
            .create_workflow("w1", "Status check", &steps)
            .await
            .unwrap();
        assert_eq!(wf.id, "w1");
        assert_eq!(wf.name, "Status check");
        assert_eq!(wf.status, WorkflowStatus::Pending);

        let fetched_steps = store.get_workflow_steps("w1").await.unwrap();
        assert_eq!(fetched_steps.len(), 2);
        assert_eq!(fetched_steps[0].name, "Check status");
        assert_eq!(fetched_steps[1].depends_on, vec!["step1"]);
    }

    #[tokio::test]
    async fn test_update_workflow_status() {
        let store = test_store().await;
        store.create_workflow("w1", "Test", &[]).await.unwrap();

        store
            .update_workflow_status("w1", WorkflowStatus::Running)
            .await
            .unwrap();
        let wf = store.get_workflow("w1").await.unwrap().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Running);

        store
            .update_workflow_status("w1", WorkflowStatus::Completed)
            .await
            .unwrap();
        let wf = store.get_workflow("w1").await.unwrap().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Completed);
    }

    #[tokio::test]
    async fn test_update_step() {
        let store = test_store().await;
        let steps = vec![WorkflowStep {
            id: "step1".to_string(),
            workflow_id: "w1".to_string(),
            name: "Do thing".to_string(),
            step_type: StepType::Tool,
            params: serde_json::json!({}),
            depends_on: vec![],
            status: StepStatus::Pending,
            result: None,
            started_at: None,
            completed_at: None,
        }];
        store.create_workflow("w1", "Test", &steps).await.unwrap();

        // Mark running
        store
            .update_step("step1", StepStatus::Running, None)
            .await
            .unwrap();
        let fetched = store.get_workflow_steps("w1").await.unwrap();
        assert_eq!(fetched[0].status, StepStatus::Running);
        assert!(fetched[0].started_at.is_some());

        // Mark completed with result
        let result = serde_json::json!({"output": "done"});
        store
            .update_step("step1", StepStatus::Completed, Some(&result))
            .await
            .unwrap();
        let fetched = store.get_workflow_steps("w1").await.unwrap();
        assert_eq!(fetched[0].status, StepStatus::Completed);
        assert!(fetched[0].completed_at.is_some());
        assert_eq!(fetched[0].result.as_ref().unwrap()["output"], "done");
    }

    #[tokio::test]
    async fn test_list_workflows() {
        let store = test_store().await;
        store.create_workflow("w1", "First", &[]).await.unwrap();
        store.create_workflow("w2", "Second", &[]).await.unwrap();

        let workflows = store.list_workflows().await.unwrap();
        assert_eq!(workflows.len(), 2);
    }

    #[tokio::test]
    async fn test_get_nonexistent() {
        let store = test_store().await;
        assert!(store.get_schedule("nope").await.unwrap().is_none());
        assert!(store.get_workflow("nope").await.unwrap().is_none());
    }
}
