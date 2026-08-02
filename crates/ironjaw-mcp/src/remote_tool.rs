use crate::client::McpClient;
use crate::types::{McpContent, McpTool};
use ironjaw_security::PermissionTier;
use ironjaw_tools::types::{MemoryHint, Tool, ToolContext, ToolResult};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A remote MCP tool surfaced as a local `Tool` in the `ToolRegistry`.
///
/// Each `McpRemoteTool` wraps one tool discovered via `client.discover_tools()`.
/// When executed it forwards the call to the MCP server over the configured transport
/// (stdio or HTTP) and converts the `McpToolResult` back into a `ToolResult`.
///
/// **Naming**: The tool name exposed to the agentic loop is
/// `mcp__<server_name>__<original_tool_name>` (double underscore separators),
/// matching the convention used by Claude Code.
///
/// **Tier**: Always `Notify` — remote tool invocations are non-destructive from
/// ironjaw's perspective (the remote server owns its own permission model), but we
/// still want a notification event emitted so the UI can show activity.
pub struct McpRemoteTool {
    client: Arc<McpClient>,
    tool: McpTool,
    /// Prefixed tool name: `mcp__<server_name>__<tool_name>`
    full_name: String,
    /// Description cached at construction time.
    description: String,
}

impl McpRemoteTool {
    /// Wrap a discovered `McpTool` with a client reference.
    ///
    /// `server_name` must match the name the client was constructed with (i.e.
    /// `client.server_name()`).  Passing a different name would produce a
    /// misleading prefix but is otherwise safe.
    pub fn new(client: Arc<McpClient>, tool: McpTool) -> Self {
        let server_name = client.server_name();
        let full_name = format!("mcp__{}__{}", server_name, tool.name);
        let description = tool
            .description
            .clone()
            .unwrap_or_else(|| format!("MCP tool {} from server {}", tool.name, server_name));
        Self {
            client,
            tool,
            full_name,
            description,
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpRemoteTool {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        self.tool.input_schema.clone()
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Notify
    }

    fn memory_hints(
        &self,
        _input: &serde_json::Value,
        _output: &serde_json::Value,
    ) -> Vec<MemoryHint> {
        // Remote tools don't provide memory hints by default.
        vec![]
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult {
        // Race the MCP call against the cancellation token.
        tokio::select! {
            _ = cancel.cancelled() => {
                ToolResult::cancelled(format!(
                    "MCP tool {} was cancelled before completion",
                    self.full_name
                ))
            }
            result = self.client.call_tool(&self.tool.name, params) => {
                match result {
                    Err(e) => ToolResult::error("MCP_ERROR", e.to_string()),
                    Ok(tool_result) => {
                        // If the remote server signalled an error, surface it.
                        if tool_result.is_error.unwrap_or(false) {
                            let msg = collect_text_content(&tool_result.content);
                            return ToolResult::error("MCP_TOOL_ERROR", msg);
                        }

                        convert_mcp_content(tool_result.content)
                    }
                }
            }
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Concatenate all `Text` content chunks into a single string.
fn collect_text_content(content: &[McpContent]) -> String {
    content
        .iter()
        .filter_map(|c| {
            if let McpContent::Text { text } = c {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Convert a `Vec<McpContent>` into a `ToolResult`.
///
/// - All-text content → `ToolResult::Text` (concatenated).
/// - Any image content → `ToolResult::Json` (structured array so the agentic loop
///   can forward the data to the LLM vision path if needed).
/// - Empty → `ToolResult::Text { content: "" }`.
fn convert_mcp_content(content: Vec<McpContent>) -> ToolResult {
    let has_image = content
        .iter()
        .any(|c| matches!(c, McpContent::Image { .. }));

    if has_image {
        // Emit a structured JSON array so vision-capable models can consume images.
        let parts: Vec<serde_json::Value> = content
            .into_iter()
            .map(|c| match c {
                McpContent::Text { text } => json!({ "type": "text", "text": text }),
                McpContent::Image { data, mime_type } => {
                    json!({ "type": "image", "data": data, "mime_type": mime_type })
                }
            })
            .collect();
        ToolResult::json(json!(parts))
    } else {
        // Pure text — concatenate all chunks.
        let text = collect_text_content(&content);
        ToolResult::text(text)
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{McpServerConfig, McpToolResult};

    fn make_tool(name: &str, description: Option<&str>) -> McpTool {
        McpTool {
            name: name.to_string(),
            description: description.map(|s| s.to_string()),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn make_client(server_name: &str) -> Arc<McpClient> {
        let config = McpServerConfig {
            name: server_name.to_string(),
            url: "http://localhost:9999/mcp".to_string(),
            api_key: None,
        };
        Arc::new(McpClient::new(&config, 5))
    }

    #[test]
    fn test_name_prefix() {
        let client = make_client("filesystem");
        let tool = make_tool("read_file", Some("Read a file"));
        let remote = McpRemoteTool::new(client, tool);
        assert_eq!(remote.name(), "mcp__filesystem__read_file");
    }

    #[test]
    fn test_description_present() {
        let client = make_client("automation");
        let tool = make_tool("tap_screen", Some("Tap at coordinates"));
        let remote = McpRemoteTool::new(client, tool);
        assert_eq!(remote.description(), "Tap at coordinates");
    }

    #[test]
    fn test_description_fallback() {
        let client = make_client("automation");
        let tool = make_tool("tap_screen", None);
        let remote = McpRemoteTool::new(client, tool);
        // Falls back to generated description containing server + tool name.
        assert!(remote.description().contains("tap_screen"));
        assert!(remote.description().contains("automation"));
    }

    #[test]
    fn test_tier_is_notify() {
        let client = make_client("test-server");
        let tool = make_tool("some_tool", None);
        let remote = McpRemoteTool::new(client, tool);
        assert_eq!(remote.tier(), PermissionTier::Notify);
    }

    #[test]
    fn test_schema_passthrough() {
        let client = make_client("test-server");
        let schema = json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        });
        let tool = McpTool {
            name: "search".to_string(),
            description: None,
            input_schema: schema.clone(),
        };
        let remote = McpRemoteTool::new(client, tool);
        assert_eq!(remote.schema(), schema);
    }

    #[test]
    fn test_convert_text_content() {
        let content = vec![
            McpContent::Text {
                text: "hello ".to_string(),
            },
            McpContent::Text {
                text: "world".to_string(),
            },
        ];
        let result = convert_mcp_content(content);
        assert!(matches!(result, ToolResult::Text { content } if content == "hello world"));
    }

    #[test]
    fn test_convert_empty_content() {
        let result = convert_mcp_content(vec![]);
        assert!(matches!(result, ToolResult::Text { content } if content.is_empty()));
    }

    #[test]
    fn test_convert_image_content() {
        let content = vec![
            McpContent::Text {
                text: "caption".to_string(),
            },
            McpContent::Image {
                data: "base64data".to_string(),
                mime_type: "image/png".to_string(),
            },
        ];
        let result = convert_mcp_content(content);
        match result {
            ToolResult::Json { content: v } => {
                let arr = v.as_array().unwrap();
                assert_eq!(arr.len(), 2);
                assert_eq!(arr[0]["type"], "text");
                assert_eq!(arr[1]["type"], "image");
                assert_eq!(arr[1]["mime_type"], "image/png");
            }
            other => panic!("expected Json, got {other:?}"),
        }
    }

    #[test]
    fn test_mcp_error_flag() {
        let tool_result = McpToolResult {
            content: vec![McpContent::Text {
                text: "Something went wrong".to_string(),
            }],
            is_error: Some(true),
        };

        // Simulate what execute() would do after receiving this result.
        let result = if tool_result.is_error.unwrap_or(false) {
            let msg = collect_text_content(&tool_result.content);
            ToolResult::error("MCP_TOOL_ERROR", msg)
        } else {
            convert_mcp_content(tool_result.content)
        };

        assert!(result.is_error());
        match result {
            ToolResult::Error { code, message } => {
                assert_eq!(code, "MCP_TOOL_ERROR");
                assert_eq!(message, "Something went wrong");
            }
            _ => panic!("expected Error variant"),
        }
    }

    #[tokio::test]
    async fn test_cancellation_returns_cancelled() {
        let client = make_client("test-server");
        let tool = make_tool("slow_tool", None);
        let remote = McpRemoteTool::new(client, tool);
        let cancel = CancellationToken::new();

        // Pre-cancel the token so the select fires immediately.
        cancel.cancel();

        let result = remote
            .execute(json!({}), &ToolContext::default(), cancel)
            .await;

        assert!(result.is_cancelled());
    }
}
