//! MCP (Model Context Protocol) client for Ironjaw.
//!
//! Provides an HTTP-based client that can connect to MCP servers to:
//! - Initialize the MCP protocol handshake
//! - Discover available tools via `tools/list`
//! - Call tools via `tools/call`
//!
//! # Example
//!
//! ```no_run
//! use ironjaw_mcp::{McpClient, McpServerConfig};
//! use serde_json::json;
//!
//! # async fn example() -> Result<(), ironjaw_mcp::McpError> {
//! let config = McpServerConfig {
//!     name: "my-server".to_string(),
//!     url: "http://localhost:3000/mcp".to_string(),
//!     api_key: None,
//! };
//!
//! let client = McpClient::new(&config, 30);
//! let _caps = client.initialize().await?;
//! let tools = client.discover_tools().await?;
//! for tool in &tools {
//!     println!("Tool: {} - {:?}", tool.name, tool.description);
//! }
//! # Ok(())
//! # }
//! ```

pub mod client;
pub mod error;
pub mod remote_tool;
pub mod server;
pub mod transport;
pub mod types;

// Re-export primary API surface
pub use client::McpClient;
pub use error::McpError;
pub use remote_tool::McpRemoteTool;
pub use server::{register_brain_tools, McpServer, McpServerTool, ToolHandler};
pub use types::{
    JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpContent, McpServerConfig, McpTool,
    McpToolResult,
};
