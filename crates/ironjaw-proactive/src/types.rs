use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

// ── Schedule ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub name: String,
    pub cron_expr: String,
    pub action_type: ActionType,
    pub action_data: serde_json::Value,
    pub enabled: bool,
    pub last_run: Option<NaiveDateTime>,
    pub next_run: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionType {
    Tool,
    Llm,
    Workflow,
    Event,
}

impl ActionType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Llm => "llm",
            Self::Workflow => "workflow",
            Self::Event => "event",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "tool" => Some(Self::Tool),
            "llm" => Some(Self::Llm),
            "workflow" => Some(Self::Workflow),
            "event" => Some(Self::Event),
            _ => None,
        }
    }
}

// ── Workflow ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub id: String,
    pub name: String,
    pub status: WorkflowStatus,
    pub state: serde_json::Value,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowStatus {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
}

impl WorkflowStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "paused" => Some(Self::Paused),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

// ── Workflow Step ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub id: String,
    pub workflow_id: String,
    pub name: String,
    pub step_type: StepType,
    pub params: serde_json::Value,
    pub depends_on: Vec<String>,
    pub status: StepStatus,
    pub result: Option<serde_json::Value>,
    pub started_at: Option<NaiveDateTime>,
    pub completed_at: Option<NaiveDateTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepType {
    Tool,
    Llm,
    Approval,
    Condition,
}

impl StepType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Llm => "llm",
            Self::Approval => "approval",
            Self::Condition => "condition",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "tool" => Some(Self::Tool),
            "llm" => Some(Self::Llm),
            "approval" => Some(Self::Approval),
            "condition" => Some(Self::Condition),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Skipped,
}

impl StepStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

// ── Schedule Action (channel message) ────────────────────

#[derive(Debug, Clone)]
pub struct ScheduleAction {
    pub schedule_id: String,
    pub action_type: ActionType,
    pub action_data: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_type_roundtrip() {
        for at in [
            ActionType::Tool,
            ActionType::Llm,
            ActionType::Workflow,
            ActionType::Event,
        ] {
            assert_eq!(ActionType::from_str(at.as_str()), Some(at));
        }
    }

    #[test]
    fn test_workflow_status_roundtrip() {
        for ws in [
            WorkflowStatus::Pending,
            WorkflowStatus::Running,
            WorkflowStatus::Paused,
            WorkflowStatus::Completed,
            WorkflowStatus::Failed,
        ] {
            assert_eq!(WorkflowStatus::from_str(ws.as_str()), Some(ws));
        }
    }

    #[test]
    fn test_step_type_roundtrip() {
        for st in [
            StepType::Tool,
            StepType::Llm,
            StepType::Approval,
            StepType::Condition,
        ] {
            assert_eq!(StepType::from_str(st.as_str()), Some(st));
        }
    }

    #[test]
    fn test_step_status_roundtrip() {
        for ss in [
            StepStatus::Pending,
            StepStatus::Running,
            StepStatus::Completed,
            StepStatus::Failed,
            StepStatus::Skipped,
        ] {
            assert_eq!(StepStatus::from_str(ss.as_str()), Some(ss));
        }
    }

    #[test]
    fn test_action_type_serde() {
        let at = ActionType::Tool;
        let json = serde_json::to_string(&at).unwrap();
        assert_eq!(json, "\"tool\"");
        let parsed: ActionType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, at);
    }

    #[test]
    fn test_unknown_returns_none() {
        assert_eq!(ActionType::from_str("invalid"), None);
        assert_eq!(WorkflowStatus::from_str("unknown"), None);
        assert_eq!(StepType::from_str("bad"), None);
        assert_eq!(StepStatus::from_str("nope"), None);
    }
}
