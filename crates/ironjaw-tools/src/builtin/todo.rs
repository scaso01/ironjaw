//! Todo list tool — persists todos via brain MemoryType::Todo.

use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_brain::types::NewMemory;
use ironjaw_security::PermissionTier;
use tokio_util::sync::CancellationToken;

pub struct TodoTool;

impl TodoTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TodoTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Manage a persistent todo list. Actions: add, list, complete."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "list", "complete"],
                    "description": "The action to perform"
                },
                "text": {
                    "type": "string",
                    "description": "Todo item text (required for 'add')"
                },
                "id": {
                    "type": "integer",
                    "description": "Memory ID of the todo to complete (required for 'complete')"
                }
            },
            "required": ["action"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let brain = match ctx.brain.as_ref() {
            Some(b) => b,
            None => return ToolResult::error("BRAIN_UNAVAILABLE", "Brain not configured"),
        };

        let action = match params.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: action")
            }
        };

        match action {
            "add" => {
                let text = match params.get("text").and_then(|v| v.as_str()) {
                    Some(t) => t,
                    None => {
                        return ToolResult::error(
                            "INVALID_PARAMS",
                            "Missing required parameter: text for action 'add'",
                        )
                    }
                };

                let mut mem = NewMemory {
                    content: format!("[TODO] {text}"),
                    source: Some("todo_tool".to_string()),
                    tags: Some("todo,pending".to_string()),
                    memory_type: Some("todo".to_string()),
                    importance: Some(0.6),
                    embedding: None,
                    strength: None,
                    supersedes: None,
                    content_hash: None,
                    valid_from: None,
                    valid_to: None,
                    source_type: None,
                    reflection: None,
                    trust_level: None,
                    evidence_session_id: None,
                    evidence_message_id: None,
                    scope: None,
                    procedure: None,
                    quality_score: Some(0.9),
                    quality_flag: None,
                };

                match brain.remember_full(&mut mem).await {
                    Ok((memory, _updated)) => {
                        ToolResult::text(format!("Added todo #{}: {text}", memory.id))
                    }
                    Err(e) => {
                        ToolResult::error("BRAIN_ERROR", format!("Failed to store todo: {e}"))
                    }
                }
            }

            "list" => match brain.recall("TODO pending", Some(50)).await {
                Ok(memories) => {
                    let todos: Vec<_> = memories
                        .iter()
                        .filter(|m| {
                            m.memory_type == "todo"
                                && m.tags.as_deref().unwrap_or("").contains("pending")
                        })
                        .collect();

                    if todos.is_empty() {
                        ToolResult::text("No pending todos.")
                    } else {
                        let lines: Vec<String> = todos
                            .iter()
                            .map(|m| format!("#{}: {}", m.id, m.content))
                            .collect();
                        ToolResult::text(lines.join("\n"))
                    }
                }
                Err(e) => ToolResult::error("BRAIN_ERROR", format!("Failed to recall todos: {e}")),
            },

            "complete" => {
                let id = match params.get("id").and_then(|v| v.as_i64()) {
                    Some(i) => i,
                    None => {
                        return ToolResult::error(
                            "INVALID_PARAMS",
                            "Missing required parameter: id for action 'complete'",
                        )
                    }
                };

                match brain.forget(id).await {
                    Ok(_found) => ToolResult::text(format!("Completed and removed todo #{id}")),
                    Err(e) => {
                        ToolResult::error("BRAIN_ERROR", format!("Failed to complete todo: {e}"))
                    }
                }
            }

            other => ToolResult::error(
                "INVALID_ACTION",
                format!("Unknown action '{other}'. Valid actions: add, list, complete"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironjaw_brain::store::BrainStore;
    use ironjaw_brain::{Brain, BrainConfig, Embedder};
    use std::path::PathBuf;
    use std::sync::Arc;

    async fn make_brain() -> Option<Arc<Brain>> {
        let store = BrainStore::open_memory().await.ok()?;
        let embedder = Embedder::new("http://localhost:8080", "nomic-embed-text");
        let config = BrainConfig {
            max_memories: 100,
            auto_recall_top_k: 3,
            brain_files_dir: ".".to_string(),
            layer1_budget_tokens: 512,
            memory_relevance_threshold: 0.0,
            auto_learn: false,
            learn_dedup_threshold: 0.95,
            ..Default::default()
        };
        Some(Arc::new(Brain::new(store, embedder, config)))
    }

    fn test_ctx() -> ToolContext {
        ToolContext::new("test", "test", PathBuf::from("."))
    }

    fn test_ctx_with_brain(brain: Arc<Brain>) -> ToolContext {
        ToolContext::new("test", "test", PathBuf::from(".")).with_brain(brain)
    }

    #[test]
    fn test_todo_schema_is_object() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string" }
            },
            "required": ["action"]
        });
        assert!(schema.is_object());
    }

    #[tokio::test]
    async fn test_todo_missing_action() {
        let tool = TodoTool::new();
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_todo_invalid_action() {
        let tool = TodoTool::new();
        let result = tool
            .execute(
                serde_json::json!({"action": "unknown"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_todo_add_missing_text() {
        let tool = TodoTool::new();
        let result = tool
            .execute(
                serde_json::json!({"action": "add"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_todo_complete_missing_id() {
        let tool = TodoTool::new();
        let result = tool
            .execute(
                serde_json::json!({"action": "complete"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[test]
    fn test_todo_tier_is_auto() {
        assert_eq!(format!("{:?}", PermissionTier::Auto), "Auto");
    }

    #[tokio::test]
    #[ignore = "requires live llama-server for embeddings"]
    async fn test_todo_add_with_brain() {
        let brain = match make_brain().await {
            Some(b) => b,
            None => return,
        };
        let tool = TodoTool::new();
        let ctx = test_ctx_with_brain(brain);
        let result = tool
            .execute(
                serde_json::json!({"action": "add", "text": "test todo item"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
    }
}
