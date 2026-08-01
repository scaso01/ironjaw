use crate::error::McpError;
use crate::transport::{HttpTransport, StdioTransport, Transport};
use crate::types::{JsonRpcRequest, McpServerConfig, McpTool, McpToolResult};
use serde_json::json;
use std::sync::Arc;
use tracing::{debug, info};

/// MCP client that connects to a single MCP server.
///
/// Supports the core MCP lifecycle: initialize, discover tools, call tools.
/// The underlying transport (HTTP or stdio) is abstracted behind `Arc<dyn Transport>`.
pub struct McpClient {
    transport: Arc<dyn Transport>,
    server_name: String,
}

/// Default timeout for MCP requests in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

impl McpClient {
    // ── Constructors ─────────────────────────────────────────────────────────

    /// Create an HTTP-based MCP client from an `McpServerConfig`.
    ///
    /// This replaces the old `McpClient::new()` constructor.  The config must
    /// have a non-empty `url` field.
    pub fn http(config: &McpServerConfig, timeout_secs: u64) -> Self {
        let transport =
            HttpTransport::new(config.url.clone(), config.api_key.clone(), timeout_secs);
        Self {
            transport: Arc::new(transport),
            server_name: config.name.clone(),
        }
    }

    /// Create an HTTP-based MCP client with the default timeout.
    pub fn http_with_defaults(config: &McpServerConfig) -> Self {
        Self::http(config, DEFAULT_TIMEOUT_SECS)
    }

    /// Create a stdio-based MCP client by spawning the given command.
    ///
    /// `command` is the executable to launch; `args` are its arguments.
    /// `timeout_secs` applies to each individual JSON-RPC request.
    pub fn stdio(
        server_name: impl Into<String>,
        command: &str,
        args: &[String],
        timeout_secs: u64,
    ) -> Result<Self, McpError> {
        let server_name = server_name.into();
        let transport = StdioTransport::spawn(&server_name, command, args, timeout_secs)?;
        Ok(Self {
            transport: Arc::new(transport),
            server_name,
        })
    }

    /// Create a stdio-based MCP client with the default timeout.
    pub fn stdio_with_defaults(
        server_name: impl Into<String>,
        command: &str,
        args: &[String],
    ) -> Result<Self, McpError> {
        Self::stdio(server_name, command, args, DEFAULT_TIMEOUT_SECS)
    }

    /// Backward-compatible constructor: equivalent to `McpClient::http(config, timeout_secs)`.
    ///
    /// Kept so that existing call-sites in tests and library code continue
    /// to compile without modification.
    pub fn new(config: &McpServerConfig, timeout_secs: u64) -> Self {
        Self::http(config, timeout_secs)
    }

    /// Backward-compatible constructor: equivalent to `McpClient::http_with_defaults(config)`.
    pub fn with_defaults(config: &McpServerConfig) -> Self {
        Self::http_with_defaults(config)
    }

    // ── MCP lifecycle ─────────────────────────────────────────────────────────

    /// Send the `initialize` request to the MCP server.
    ///
    /// Returns the server's capabilities and metadata.
    pub async fn initialize(&self) -> Result<serde_json::Value, McpError> {
        info!(server = %self.server_name, "initializing MCP connection");

        let request = JsonRpcRequest::new(
            json!(1),
            "initialize",
            Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "ironjaw",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })),
        );

        let response = self.transport.send(&request).await?;

        let result = response.result.ok_or_else(|| {
            McpError::Parse("initialize response missing result field".to_string())
        })?;

        debug!(
            server = %self.server_name,
            "MCP server initialized"
        );

        Ok(result)
    }

    /// Discover available tools from the MCP server via `tools/list`.
    pub async fn discover_tools(&self) -> Result<Vec<McpTool>, McpError> {
        info!(server = %self.server_name, "discovering MCP tools");

        let request = JsonRpcRequest::new(json!(2), "tools/list", None);

        let response = self.transport.send(&request).await?;

        let result = response.result.ok_or_else(|| {
            McpError::Parse("tools/list response missing result field".to_string())
        })?;

        let tools: Vec<McpTool> =
            serde_json::from_value(result.get("tools").cloned().unwrap_or(json!([])))
                .map_err(|e| McpError::Parse(format!("failed to parse tools list: {e}")))?;

        info!(
            server = %self.server_name,
            count = tools.len(),
            "discovered MCP tools"
        );

        Ok(tools)
    }

    /// Call a tool on the MCP server via `tools/call`.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpToolResult, McpError> {
        info!(
            server = %self.server_name,
            tool = %name,
            "calling MCP tool"
        );

        let request_id = uuid::Uuid::new_v4().to_string();

        let request = JsonRpcRequest::new(
            json!(request_id),
            "tools/call",
            Some(json!({
                "name": name,
                "arguments": arguments
            })),
        );

        let response = self.transport.send(&request).await?;

        let result = response.result.ok_or_else(|| {
            McpError::Parse("tools/call response missing result field".to_string())
        })?;

        let tool_result: McpToolResult = serde_json::from_value(result)
            .map_err(|e| McpError::Parse(format!("failed to parse tool result: {e}")))?;

        debug!(
            server = %self.server_name,
            tool = %name,
            content_count = tool_result.content.len(),
            is_error = ?tool_result.is_error,
            "MCP tool call completed"
        );

        Ok(tool_result)
    }

    /// Returns the server name.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> McpServerConfig {
        McpServerConfig {
            name: "test-server".to_string(),
            url: "http://localhost:3000/mcp".to_string(),
            api_key: None,
        }
    }

    fn test_config_with_key() -> McpServerConfig {
        McpServerConfig {
            name: "auth-server".to_string(),
            url: "http://localhost:3001/mcp".to_string(),
            api_key: Some("sk-test-key".to_string()),
        }
    }

    #[test]
    fn test_client_construction() {
        let config = test_config();
        let client = McpClient::new(&config, 30);
        assert_eq!(client.server_name(), "test-server");
    }

    #[test]
    fn test_client_with_defaults() {
        let config = test_config();
        let client = McpClient::with_defaults(&config);
        assert_eq!(client.server_name(), "test-server");
    }

    #[test]
    fn test_client_with_api_key() {
        let config = test_config_with_key();
        let client = McpClient::new(&config, 60);
        assert_eq!(client.server_name(), "auth-server");
    }

    #[test]
    fn test_server_name() {
        let config = McpServerConfig {
            name: "my-custom-server".to_string(),
            url: "http://example.com/mcp".to_string(),
            api_key: None,
        };
        let client = McpClient::new(&config, 10);
        assert_eq!(client.server_name(), "my-custom-server");
    }

    #[test]
    fn test_multiple_clients() {
        let config1 = test_config();
        let config2 = test_config_with_key();
        let client1 = McpClient::new(&config1, 30);
        let client2 = McpClient::new(&config2, 30);
        assert_ne!(client1.server_name(), client2.server_name());
    }

    #[test]
    fn test_http_constructor() {
        let config = test_config();
        let client = McpClient::http(&config, 30);
        assert_eq!(client.server_name(), "test-server");
    }

    #[test]
    fn test_http_with_defaults_constructor() {
        let config = test_config();
        let client = McpClient::http_with_defaults(&config);
        assert_eq!(client.server_name(), "test-server");
    }
}
