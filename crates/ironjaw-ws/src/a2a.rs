//! Google A2A (Agent-to-Agent) protocol implementation.
//!
//! Provides inter-agent communication endpoints following the A2A spec:
//! - `GET /.well-known/agent.json` — agent card (capabilities discovery)
//! - `POST /a2a/tasks/send` — submit a task
//! - `GET /a2a/tasks/:task_id` — get task state
//! - `GET /a2a/tasks` — list all tasks

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// A2A Types
// ---------------------------------------------------------------------------

/// Agent card describing this agent's identity and capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub url: String,
    pub version: String,
    pub capabilities: AgentCapabilities,
    pub skills: Vec<AgentSkill>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authentication: Option<AgentAuth>,
}

/// Capabilities advertised by this agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub streaming: bool,
    pub push_notifications: bool,
    pub state_transition_history: bool,
}

/// A skill (capability) that this agent can perform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
}

/// Authentication schemes supported by this agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAuth {
    pub schemes: Vec<String>,
}

/// A task managed by the A2A protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2ATask {
    pub id: String,
    pub status: A2ATaskStatus,
    pub messages: Vec<A2AMessage>,
    pub created_at: String,
    pub updated_at: String,
}

/// Task lifecycle status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum A2ATaskStatus {
    Submitted,
    Working,
    InputRequired,
    Completed,
    Canceled,
    Failed,
}

/// A message exchanged within an A2A task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2AMessage {
    pub role: String,
    pub parts: Vec<A2APart>,
}

/// A single content part within an A2A message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum A2APart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "data")]
    Data { data: serde_json::Value },
}

/// Request body for creating/sending a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendTaskRequest {
    pub id: Option<String>,
    pub message: A2AMessage,
    pub skill_id: Option<String>,
}

// ---------------------------------------------------------------------------
// A2A Task Store
// ---------------------------------------------------------------------------

/// In-memory store for A2A tasks.
#[derive(Debug, Clone)]
pub struct A2ATaskStore {
    tasks: Arc<Mutex<HashMap<String, A2ATask>>>,
}

impl Default for A2ATaskStore {
    fn default() -> Self {
        Self::new()
    }
}

impl A2ATaskStore {
    /// Create a new empty task store.
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a new task from a send request.
    pub fn create_task(&self, request: &SendTaskRequest) -> A2ATask {
        let id = request
            .id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let now = chrono::Utc::now().to_rfc3339();
        let task = A2ATask {
            id: id.clone(),
            status: A2ATaskStatus::Submitted,
            messages: vec![request.message.clone()],
            created_at: now.clone(),
            updated_at: now,
        };
        let mut tasks = self.tasks.lock().unwrap_or_else(|poisoned| {
            tracing::error!("A2A task store lock poisoned, recovering");
            poisoned.into_inner()
        });
        tasks.insert(id, task.clone());
        task
    }

    /// Get a task by ID.
    pub fn get_task(&self, id: &str) -> Option<A2ATask> {
        let tasks = self.tasks.lock().unwrap_or_else(|poisoned| {
            tracing::error!("A2A task store lock poisoned, recovering");
            poisoned.into_inner()
        });
        tasks.get(id).cloned()
    }

    /// Update a task's status. Returns the updated task or None if not found.
    pub fn update_status(&self, id: &str, status: A2ATaskStatus) -> Option<A2ATask> {
        let mut tasks = self.tasks.lock().unwrap_or_else(|poisoned| {
            tracing::error!("A2A task store lock poisoned, recovering");
            poisoned.into_inner()
        });
        if let Some(task) = tasks.get_mut(id) {
            task.status = status;
            task.updated_at = chrono::Utc::now().to_rfc3339();
            Some(task.clone())
        } else {
            None
        }
    }

    /// Add a message to an existing task. Returns the updated task or None if not found.
    pub fn add_message(&self, id: &str, message: A2AMessage) -> Option<A2ATask> {
        let mut tasks = self.tasks.lock().unwrap_or_else(|poisoned| {
            tracing::error!("A2A task store lock poisoned, recovering");
            poisoned.into_inner()
        });
        if let Some(task) = tasks.get_mut(id) {
            task.messages.push(message);
            task.updated_at = chrono::Utc::now().to_rfc3339();
            Some(task.clone())
        } else {
            None
        }
    }

    /// List all tasks, ordered by creation time (oldest first).
    pub fn list_tasks(&self) -> Vec<A2ATask> {
        let tasks = self.tasks.lock().unwrap_or_else(|poisoned| {
            tracing::error!("A2A task store lock poisoned, recovering");
            poisoned.into_inner()
        });
        let mut list: Vec<A2ATask> = tasks.values().cloned().collect();
        list.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        list
    }
}

// ---------------------------------------------------------------------------
// Agent Card builder
// ---------------------------------------------------------------------------

/// Build the default Ironjaw agent card.
pub fn build_agent_card(base_url: &str) -> AgentCard {
    AgentCard {
        name: "Ironjaw".to_string(),
        description: "Dr. CLAW AI assistant gateway — WebSocket + REST + A2A".to_string(),
        url: base_url.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        capabilities: AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: true,
        },
        skills: vec![
            AgentSkill {
                id: "chat".to_string(),
                name: "Chat".to_string(),
                description: "General-purpose LLM chat with tool use".to_string(),
                tags: vec!["llm".to_string(), "chat".to_string()],
            },
            AgentSkill {
                id: "search".to_string(),
                name: "Web Search".to_string(),
                description: "Search the web via SearXNG and Perplexica".to_string(),
                tags: vec!["search".to_string(), "web".to_string()],
            },
            AgentSkill {
                id: "code".to_string(),
                name: "Code Tools".to_string(),
                description: "Read, write, and execute code on the host".to_string(),
                tags: vec!["code".to_string(), "tools".to_string()],
            },
            AgentSkill {
                id: "memory".to_string(),
                name: "RAG Memory".to_string(),
                description: "Store and recall long-term memories with semantic search".to_string(),
                tags: vec!["memory".to_string(), "rag".to_string()],
            },
        ],
        authentication: Some(AgentAuth {
            schemes: vec!["bearer".to_string()],
        }),
    }
}

// ---------------------------------------------------------------------------
// A2A State (separate from AppState for simplicity)
// ---------------------------------------------------------------------------

/// Shared state for A2A handlers, holding the task store and agent card.
#[derive(Debug, Clone)]
pub struct A2AState {
    pub task_store: A2ATaskStore,
    pub agent_card: AgentCard,
}

// ---------------------------------------------------------------------------
// Axum Handlers
// ---------------------------------------------------------------------------

/// GET /.well-known/agent.json — return the agent card.
pub async fn agent_card_handler(State(state): State<A2AState>) -> Json<AgentCard> {
    Json(state.agent_card)
}

/// POST /a2a/tasks/send — create a new task.
pub async fn send_task_handler(
    State(state): State<A2AState>,
    Json(request): Json<SendTaskRequest>,
) -> (StatusCode, Json<A2ATask>) {
    let task = state.task_store.create_task(&request);
    (StatusCode::CREATED, Json(task))
}

/// GET /a2a/tasks/:task_id — get a task by ID.
pub async fn get_task_handler(
    State(state): State<A2AState>,
    Path(task_id): Path<String>,
) -> Result<Json<A2ATask>, (StatusCode, Json<serde_json::Value>)> {
    match state.task_store.get_task(&task_id) {
        Some(task) => Ok(Json(task)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": {
                    "message": format!("Task '{task_id}' not found"),
                    "type": "not_found"
                }
            })),
        )),
    }
}

/// GET /a2a/tasks — list all tasks.
pub async fn list_tasks_handler(State(state): State<A2AState>) -> Json<Vec<A2ATask>> {
    Json(state.task_store.list_tasks())
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build an axum Router for A2A endpoints with its own state.
///
/// Returns a Router<()> that can be merged into the main app router.
pub fn a2a_router(base_url: &str) -> axum::Router {
    let state = A2AState {
        task_store: A2ATaskStore::new(),
        agent_card: build_agent_card(base_url),
    };

    axum::Router::new()
        .route(
            "/.well-known/agent.json",
            axum::routing::get(agent_card_handler),
        )
        .route("/a2a/tasks/send", axum::routing::post(send_task_handler))
        .route("/a2a/tasks/{task_id}", axum::routing::get(get_task_handler))
        .route("/a2a/tasks", axum::routing::get(list_tasks_handler))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- AgentCard tests --

    #[test]
    fn test_agent_card_serialization() {
        let card = build_agent_card("http://localhost:18794");
        let json = serde_json::to_string(&card).unwrap();
        assert!(json.contains("Ironjaw"));
        assert!(json.contains("http://localhost:18794"));
        assert!(json.contains("\"streaming\":true"));
        assert!(json.contains("\"bearer\""));
    }

    #[test]
    fn test_agent_card_deserialization_roundtrip() {
        let card = build_agent_card("http://localhost:18794");
        let json = serde_json::to_string(&card).unwrap();
        let deserialized: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "Ironjaw");
        assert_eq!(deserialized.url, "http://localhost:18794");
        assert!(deserialized.capabilities.streaming);
        assert!(!deserialized.capabilities.push_notifications);
        assert!(deserialized.capabilities.state_transition_history);
        assert_eq!(deserialized.skills.len(), 4);
        assert_eq!(deserialized.skills[0].id, "chat");
    }

    #[test]
    fn test_agent_card_without_auth() {
        let mut card = build_agent_card("http://localhost:18794");
        card.authentication = None;
        let json = serde_json::to_string(&card).unwrap();
        assert!(!json.contains("authentication"));
        let deserialized: AgentCard = serde_json::from_str(&json).unwrap();
        assert!(deserialized.authentication.is_none());
    }

    // -- A2ATaskStatus tests --

    #[test]
    fn test_task_status_serialization() {
        assert_eq!(
            serde_json::to_string(&A2ATaskStatus::Submitted).unwrap(),
            "\"submitted\""
        );
        assert_eq!(
            serde_json::to_string(&A2ATaskStatus::Working).unwrap(),
            "\"working\""
        );
        assert_eq!(
            serde_json::to_string(&A2ATaskStatus::InputRequired).unwrap(),
            "\"inputrequired\""
        );
        assert_eq!(
            serde_json::to_string(&A2ATaskStatus::Completed).unwrap(),
            "\"completed\""
        );
        assert_eq!(
            serde_json::to_string(&A2ATaskStatus::Canceled).unwrap(),
            "\"canceled\""
        );
        assert_eq!(
            serde_json::to_string(&A2ATaskStatus::Failed).unwrap(),
            "\"failed\""
        );
    }

    #[test]
    fn test_task_status_deserialization() {
        let status: A2ATaskStatus = serde_json::from_str("\"submitted\"").unwrap();
        assert_eq!(status, A2ATaskStatus::Submitted);
        let status: A2ATaskStatus = serde_json::from_str("\"working\"").unwrap();
        assert_eq!(status, A2ATaskStatus::Working);
        let status: A2ATaskStatus = serde_json::from_str("\"completed\"").unwrap();
        assert_eq!(status, A2ATaskStatus::Completed);
    }

    // -- A2APart tests --

    #[test]
    fn test_text_part_serialization() {
        let part = A2APart::Text {
            text: "Hello, agent!".to_string(),
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        assert!(json.contains("Hello, agent!"));
    }

    #[test]
    fn test_data_part_serialization() {
        let part = A2APart::Data {
            data: serde_json::json!({"key": "value", "count": 42}),
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains("\"type\":\"data\""));
        assert!(json.contains("\"key\":\"value\""));
        assert!(json.contains("\"count\":42"));
    }

    #[test]
    fn test_part_deserialization_roundtrip() {
        let text_part = A2APart::Text {
            text: "test".to_string(),
        };
        let json = serde_json::to_string(&text_part).unwrap();
        let deserialized: A2APart = serde_json::from_str(&json).unwrap();
        match deserialized {
            A2APart::Text { text } => assert_eq!(text, "test"),
            _ => panic!("Expected Text variant"),
        }

        let data_part = A2APart::Data {
            data: serde_json::json!({"nested": true}),
        };
        let json = serde_json::to_string(&data_part).unwrap();
        let deserialized: A2APart = serde_json::from_str(&json).unwrap();
        match deserialized {
            A2APart::Data { data } => assert_eq!(data["nested"], true),
            _ => panic!("Expected Data variant"),
        }
    }

    // -- A2AMessage tests --

    #[test]
    fn test_message_serialization() {
        let msg = A2AMessage {
            role: "user".to_string(),
            parts: vec![
                A2APart::Text {
                    text: "Search for Rust tutorials".to_string(),
                },
                A2APart::Data {
                    data: serde_json::json!({"max_results": 10}),
                },
            ],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("Search for Rust tutorials"));
        assert!(json.contains("\"max_results\":10"));
    }

    #[test]
    fn test_message_deserialization() {
        let json = r#"{
            "role": "agent",
            "parts": [
                {"type": "text", "text": "Here are the results"},
                {"type": "data", "data": {"results": [1, 2, 3]}}
            ]
        }"#;
        let msg: A2AMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.role, "agent");
        assert_eq!(msg.parts.len(), 2);
    }

    // -- SendTaskRequest tests --

    #[test]
    fn test_send_task_request_full() {
        let json = r#"{
            "id": "custom-id-123",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "Do something"}]
            },
            "skill_id": "chat"
        }"#;
        let req: SendTaskRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, Some("custom-id-123".to_string()));
        assert_eq!(req.message.role, "user");
        assert_eq!(req.skill_id, Some("chat".to_string()));
    }

    #[test]
    fn test_send_task_request_minimal() {
        let json = r#"{
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "Hello"}]
            }
        }"#;
        let req: SendTaskRequest = serde_json::from_str(json).unwrap();
        assert!(req.id.is_none());
        assert!(req.skill_id.is_none());
    }

    // -- A2ATask tests --

    #[test]
    fn test_task_serialization_roundtrip() {
        let task = A2ATask {
            id: "task-001".to_string(),
            status: A2ATaskStatus::Working,
            messages: vec![A2AMessage {
                role: "user".to_string(),
                parts: vec![A2APart::Text {
                    text: "Hello".to_string(),
                }],
            }],
            created_at: "2026-03-11T00:00:00Z".to_string(),
            updated_at: "2026-03-11T00:01:00Z".to_string(),
        };
        let json = serde_json::to_string(&task).unwrap();
        let deserialized: A2ATask = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "task-001");
        assert_eq!(deserialized.status, A2ATaskStatus::Working);
        assert_eq!(deserialized.messages.len(), 1);
    }

    // -- A2ATaskStore tests --

    #[test]
    fn test_store_create_task_with_auto_id() {
        let store = A2ATaskStore::new();
        let request = SendTaskRequest {
            id: None,
            message: A2AMessage {
                role: "user".to_string(),
                parts: vec![A2APart::Text {
                    text: "Hello".to_string(),
                }],
            },
            skill_id: None,
        };
        let task = store.create_task(&request);
        assert!(!task.id.is_empty());
        assert_eq!(task.status, A2ATaskStatus::Submitted);
        assert_eq!(task.messages.len(), 1);
        // Verify the auto-generated ID is a valid UUID
        Uuid::parse_str(&task.id).expect("auto-generated ID should be a valid UUID");
    }

    #[test]
    fn test_store_create_task_with_custom_id() {
        let store = A2ATaskStore::new();
        let request = SendTaskRequest {
            id: Some("my-task-42".to_string()),
            message: A2AMessage {
                role: "user".to_string(),
                parts: vec![A2APart::Text {
                    text: "Custom".to_string(),
                }],
            },
            skill_id: Some("chat".to_string()),
        };
        let task = store.create_task(&request);
        assert_eq!(task.id, "my-task-42");
    }

    #[test]
    fn test_store_get_task() {
        let store = A2ATaskStore::new();
        let request = SendTaskRequest {
            id: Some("lookup-me".to_string()),
            message: A2AMessage {
                role: "user".to_string(),
                parts: vec![A2APart::Text {
                    text: "Find me".to_string(),
                }],
            },
            skill_id: None,
        };
        store.create_task(&request);

        let found = store.get_task("lookup-me");
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "lookup-me");

        let not_found = store.get_task("nonexistent");
        assert!(not_found.is_none());
    }

    #[test]
    fn test_store_update_status() {
        let store = A2ATaskStore::new();
        let request = SendTaskRequest {
            id: Some("status-test".to_string()),
            message: A2AMessage {
                role: "user".to_string(),
                parts: vec![A2APart::Text {
                    text: "Test".to_string(),
                }],
            },
            skill_id: None,
        };
        store.create_task(&request);

        let updated = store.update_status("status-test", A2ATaskStatus::Working);
        assert!(updated.is_some());
        assert_eq!(updated.unwrap().status, A2ATaskStatus::Working);

        let updated = store.update_status("status-test", A2ATaskStatus::Completed);
        assert!(updated.is_some());
        assert_eq!(updated.unwrap().status, A2ATaskStatus::Completed);

        let not_found = store.update_status("nonexistent", A2ATaskStatus::Failed);
        assert!(not_found.is_none());
    }

    #[test]
    fn test_store_add_message() {
        let store = A2ATaskStore::new();
        let request = SendTaskRequest {
            id: Some("msg-test".to_string()),
            message: A2AMessage {
                role: "user".to_string(),
                parts: vec![A2APart::Text {
                    text: "Initial".to_string(),
                }],
            },
            skill_id: None,
        };
        store.create_task(&request);

        let agent_msg = A2AMessage {
            role: "agent".to_string(),
            parts: vec![A2APart::Text {
                text: "Response".to_string(),
            }],
        };
        let updated = store.add_message("msg-test", agent_msg);
        assert!(updated.is_some());
        let task = updated.unwrap();
        assert_eq!(task.messages.len(), 2);
        assert_eq!(task.messages[0].role, "user");
        assert_eq!(task.messages[1].role, "agent");

        let not_found = store.add_message(
            "nonexistent",
            A2AMessage {
                role: "user".to_string(),
                parts: vec![],
            },
        );
        assert!(not_found.is_none());
    }

    #[test]
    fn test_store_list_tasks() {
        let store = A2ATaskStore::new();
        assert!(store.list_tasks().is_empty());

        for i in 0..3 {
            let request = SendTaskRequest {
                id: Some(format!("task-{i}")),
                message: A2AMessage {
                    role: "user".to_string(),
                    parts: vec![A2APart::Text {
                        text: format!("Task {i}"),
                    }],
                },
                skill_id: None,
            };
            store.create_task(&request);
        }

        let tasks = store.list_tasks();
        assert_eq!(tasks.len(), 3);
    }

    #[test]
    fn test_store_default_impl() {
        // Verify Default trait works
        let store = A2ATaskStore::default();
        assert!(store.list_tasks().is_empty());
    }

    // -- Handler response format tests --

    #[tokio::test]
    async fn test_agent_card_handler_response() {
        let state = A2AState {
            task_store: A2ATaskStore::new(),
            agent_card: build_agent_card("http://localhost:18794"),
        };
        let Json(card) = agent_card_handler(State(state)).await;
        assert_eq!(card.name, "Ironjaw");
        assert_eq!(card.skills.len(), 4);
        assert!(card.capabilities.streaming);
    }

    #[tokio::test]
    async fn test_send_task_handler_response() {
        let state = A2AState {
            task_store: A2ATaskStore::new(),
            agent_card: build_agent_card("http://localhost:18794"),
        };
        let request = SendTaskRequest {
            id: None,
            message: A2AMessage {
                role: "user".to_string(),
                parts: vec![A2APart::Text {
                    text: "Hello A2A".to_string(),
                }],
            },
            skill_id: Some("chat".to_string()),
        };
        let (status, Json(task)) = send_task_handler(State(state), Json(request)).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(task.status, A2ATaskStatus::Submitted);
        assert_eq!(task.messages.len(), 1);
    }

    #[tokio::test]
    async fn test_get_task_handler_found() {
        let store = A2ATaskStore::new();
        let request = SendTaskRequest {
            id: Some("find-me".to_string()),
            message: A2AMessage {
                role: "user".to_string(),
                parts: vec![A2APart::Text {
                    text: "Test".to_string(),
                }],
            },
            skill_id: None,
        };
        store.create_task(&request);

        let state = A2AState {
            task_store: store,
            agent_card: build_agent_card("http://localhost:18794"),
        };
        let result = get_task_handler(State(state), Path("find-me".to_string())).await;
        assert!(result.is_ok());
        let Json(task) = result.unwrap();
        assert_eq!(task.id, "find-me");
    }

    #[tokio::test]
    async fn test_get_task_handler_not_found() {
        let state = A2AState {
            task_store: A2ATaskStore::new(),
            agent_card: build_agent_card("http://localhost:18794"),
        };
        let result = get_task_handler(State(state), Path("nonexistent".to_string())).await;
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_tasks_handler_empty() {
        let state = A2AState {
            task_store: A2ATaskStore::new(),
            agent_card: build_agent_card("http://localhost:18794"),
        };
        let Json(tasks) = list_tasks_handler(State(state)).await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn test_list_tasks_handler_with_tasks() {
        let store = A2ATaskStore::new();
        for i in 0..2 {
            store.create_task(&SendTaskRequest {
                id: Some(format!("t-{i}")),
                message: A2AMessage {
                    role: "user".to_string(),
                    parts: vec![A2APart::Text {
                        text: format!("msg {i}"),
                    }],
                },
                skill_id: None,
            });
        }
        let state = A2AState {
            task_store: store,
            agent_card: build_agent_card("http://localhost:18794"),
        };
        let Json(tasks) = list_tasks_handler(State(state)).await;
        assert_eq!(tasks.len(), 2);
    }

    // -- AgentSkill tests --

    #[test]
    fn test_agent_skill_serialization() {
        let skill = AgentSkill {
            id: "test-skill".to_string(),
            name: "Test Skill".to_string(),
            description: "A test skill for testing".to_string(),
            tags: vec!["test".to_string(), "example".to_string()],
        };
        let json = serde_json::to_string(&skill).unwrap();
        assert!(json.contains("\"id\":\"test-skill\""));
        assert!(json.contains("\"tags\":[\"test\",\"example\"]"));
    }

    // -- AgentCapabilities tests --

    #[test]
    fn test_capabilities_serialization() {
        let caps = AgentCapabilities {
            streaming: false,
            push_notifications: true,
            state_transition_history: false,
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(json.contains("\"streaming\":false"));
        assert!(json.contains("\"push_notifications\":true"));
        assert!(json.contains("\"state_transition_history\":false"));
    }

    // -- AgentAuth tests --

    #[test]
    fn test_agent_auth_serialization() {
        let auth = AgentAuth {
            schemes: vec!["bearer".to_string(), "api_key".to_string()],
        };
        let json = serde_json::to_string(&auth).unwrap();
        let deserialized: AgentAuth = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.schemes.len(), 2);
        assert_eq!(deserialized.schemes[0], "bearer");
        assert_eq!(deserialized.schemes[1], "api_key");
    }

    // -- A2A Router integration --

    #[test]
    fn test_a2a_router_builds() {
        // Verify the router builds without panicking
        let _router = a2a_router("http://localhost:18794");
    }
}
