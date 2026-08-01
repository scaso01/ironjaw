use crate::types::{Tool, ToolContext, ToolInfo, ToolResult, ToolSummary};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

/// Registry of available tools. Supports register, list, get, and execute.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool. Replaces any existing tool with the same name.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    /// List metadata for all registered tools.
    pub fn list(&self) -> Vec<ToolInfo> {
        let mut infos: Vec<ToolInfo> = self
            .tools
            .values()
            .map(|t| ToolInfo {
                name: t.name().to_string(),
                description: t.description().to_string(),
                schema: t.schema(),
                tier: t.tier() as u8,
            })
            .collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    /// Returns name + description only (no schema). For LLM context efficiency.
    pub fn list_summaries(&self) -> Vec<ToolSummary> {
        let mut summaries: Vec<ToolSummary> = self
            .tools
            .values()
            .map(|t| ToolSummary {
                name: t.name().to_string(),
                description: t.description().to_string(),
                tier: t.tier() as u8,
            })
            .collect();
        summaries.sort_by(|a, b| a.name.cmp(&b.name));
        summaries
    }

    /// Returns full schema for a specific tool by name.
    pub fn get_schema(&self, name: &str) -> Option<serde_json::Value> {
        self.tools.get(name).map(|t| t.schema())
    }

    /// Get a reference to a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// Execute a tool by name with the given params, context, and cancellation token.
    pub async fn execute(
        &self,
        name: &str,
        params: serde_json::Value,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult {
        match self.tools.get(name) {
            Some(tool) => tool.execute(params, ctx, cancel).await,
            None => ToolResult::error("TOOL_NOT_FOUND", format!("Tool '{name}' is not registered")),
        }
    }

    /// Number of registered tools.
    pub fn count(&self) -> usize {
        self.tools.len()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("count", &self.tools.len())
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironjaw_security::PermissionTier;
    use std::path::PathBuf;
    use tokio_util::sync::CancellationToken;

    /// Simple mock tool for testing
    struct MockTool {
        name: String,
        response: String,
    }

    #[async_trait::async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "A mock tool for testing"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn tier(&self) -> PermissionTier {
            PermissionTier::Auto
        }
        async fn execute(
            &self,
            _params: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            ToolResult::text(&self.response)
        }
    }

    fn test_ctx() -> ToolContext {
        ToolContext::new("test-session", "test-conn", PathBuf::from("/tmp"))
    }

    #[test]
    fn test_register_and_list() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "alpha".to_string(),
            response: "ok".to_string(),
        }));
        reg.register(Box::new(MockTool {
            name: "beta".to_string(),
            response: "ok".to_string(),
        }));

        let list = reg.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "alpha"); // sorted
        assert_eq!(list[1].name, "beta");
        assert_eq!(reg.count(), 2);
    }

    #[test]
    fn test_get() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "test_tool".to_string(),
            response: "ok".to_string(),
        }));

        assert!(reg.get("test_tool").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[tokio::test]
    async fn test_execute() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "echo".to_string(),
            response: "hello world".to_string(),
        }));

        let ctx = test_ctx();
        let result = reg
            .execute(
                "echo",
                serde_json::json!({}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        match result {
            ToolResult::Text { content } => assert_eq!(content, "hello world"),
            _ => panic!("Expected Text result"),
        }
    }

    #[tokio::test]
    async fn test_execute_unknown_tool() {
        let reg = ToolRegistry::new();
        let ctx = test_ctx();
        let result = reg
            .execute(
                "nonexistent",
                serde_json::json!({}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, .. } => assert_eq!(code, "TOOL_NOT_FOUND"),
            _ => panic!("Expected Error result"),
        }
    }

    #[test]
    fn test_list_summaries_returns_name_and_description_only() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "alpha".to_string(),
            response: "ok".to_string(),
        }));
        reg.register(Box::new(MockTool {
            name: "beta".to_string(),
            response: "ok".to_string(),
        }));

        let summaries = reg.list_summaries();
        assert_eq!(summaries.len(), 2);
        assert!(!summaries[0].name.is_empty());
        assert!(!summaries[0].description.is_empty());
        assert_eq!(summaries[0].name, "alpha"); // sorted
        assert_eq!(summaries[1].name, "beta");
    }

    #[test]
    fn test_get_schema_returns_full_schema() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "test_tool".to_string(),
            response: "ok".to_string(),
        }));

        let schema = reg.get_schema("test_tool");
        assert!(schema.is_some());
        assert!(schema.unwrap().is_object());

        assert!(reg.get_schema("nonexistent").is_none());
    }

    #[test]
    fn test_register_replaces_existing() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "dup".to_string(),
            response: "first".to_string(),
        }));
        reg.register(Box::new(MockTool {
            name: "dup".to_string(),
            response: "second".to_string(),
        }));
        assert_eq!(reg.count(), 1);
    }
}
