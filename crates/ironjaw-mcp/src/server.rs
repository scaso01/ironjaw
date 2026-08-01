//! MCP server mode for Ironjaw.
//!
//! Allows Ironjaw to act AS an MCP server (stdio transport) so that
//! Claude Desktop, Cursor, or other MCP clients can use Ironjaw's tools.
//!
//! The server reads line-delimited JSON-RPC 2.0 messages from stdin, processes
//! them, and writes responses to stdout. Notifications (methods without an id
//! or notification methods like `notifications/initialized`) receive no response.
//!
//! # Supported methods
//!
//! - `initialize` -- returns server info and capabilities
//! - `notifications/initialized` -- acknowledged silently (no response)
//! - `tools/list` -- returns the registered tool descriptors
//! - `tools/call` -- delegates to a tool handler (placeholder for now)

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use serde_json::json;
use tracing::{debug, info, warn};

use crate::error::McpError;
use crate::types::{McpContent, McpToolResult};

/// Callback type for handling tool invocations.
///
/// Receives the tool name and a JSON object of arguments, returns a JSON result
/// or an error. The callback must be `Send + Sync` for use across threads.
pub type ToolHandler =
    Arc<dyn Fn(&str, serde_json::Value) -> anyhow::Result<serde_json::Value> + Send + Sync>;

/// Descriptor for a tool that the MCP server exposes to clients.
#[derive(Debug, Clone)]
pub struct McpServerTool {
    /// Tool name (must be unique).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema describing the tool's input parameters.
    pub input_schema: serde_json::Value,
}

/// An MCP server that exposes Ironjaw's tools over stdio transport.
///
/// Create an instance, register tools, then either:
/// - Call [`McpServer::run_stdio`] for the blocking event loop, or
/// - Call [`McpServer::handle_request`] for single-request processing (useful
///   for testing and embedding in other transports).
pub struct McpServer {
    name: String,
    version: String,
    tools: Vec<McpServerTool>,
    tool_handler: Option<ToolHandler>,
}

/// The protocol version this server speaks.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Tools that only read state. Advertised with `readOnlyHint: true` so clients can
/// run them without a confirmation prompt — Gemini Spark asks before every tool call
/// otherwise, which makes the brain unusable as an ambient lookup.
///
/// Anything absent from this list is advertised as destructive. That default is
/// deliberate: a new tool has to be added here consciously, so forgetting one costs
/// an extra prompt rather than an unprompted mutation.
const READ_ONLY_TOOLS: &[&str] = &[
    "brain_recall",
    "brain_recent",
    "brain_status",
    "brain_timeline",
    "brain_summaries",
    "brain_context_list",
    "brain_pending_list",
    "brain_snapshot_list",
    "brain_dream_status",
];

/// Returned in `initialize` as `instructions`. MCP clients (Claude, Gemini Spark)
/// surface this to the model as standing guidance, so it is what makes the brain
/// fire on its own instead of only when the user names it explicitly.
const SERVER_INSTRUCTIONS: &str = "\
This server is the user's personal long-term memory — their work, projects, home \
infrastructure, machines, decisions, preferences and past conversations. It is the \
authoritative source for anything about them.

Call `brain_recall` BEFORE answering whenever the question touches the user \
personally: their career, resume, employers or contacts; their projects, code, \
servers, home network or devices; anything they have decided, planned, preferred or \
been told before; or any question using \"I\", \"my\", \"we\" or \"our\" where the \
answer depends on their situation. Do not wait to be told to check memory and do not \
ask permission first — recall is a read-only lookup.

If recall returns nothing useful, say so plainly and answer from general knowledge \
rather than guessing at his specifics. Use `brain_remember` to store new durable facts \
he tells you about himself.";

impl McpServer {
    /// Create a new MCP server with the given name and version.
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.to_string(),
            version: version.to_string(),
            tools: Vec::new(),
            tool_handler: None,
        }
    }

    /// Register a tool that clients can discover and call.
    pub fn add_tool(&mut self, tool: McpServerTool) {
        self.tools.push(tool);
    }

    /// Set a callback that handles all tool invocations.
    ///
    /// The handler receives `(tool_name, arguments_json)` and returns a JSON
    /// value on success or an error on failure.
    pub fn set_tool_handler(&mut self, handler: ToolHandler) {
        self.tool_handler = Some(handler);
    }

    /// Process a single JSON-RPC request string and return an optional response.
    ///
    /// Returns `None` for notifications (which require no response).
    /// Returns `Some(json_string)` for requests that need a reply.
    pub fn handle_request(&self, request: &str) -> Option<String> {
        let parsed: serde_json::Value = match serde_json::from_str(request) {
            Ok(v) => v,
            Err(e) => {
                warn!("failed to parse JSON-RPC request: {e}");
                let error_response = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {
                        "code": -32700,
                        "message": format!("Parse error: {e}")
                    }
                });
                return Some(serde_json::to_string(&error_response).unwrap_or_default());
            }
        };

        let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = parsed.get("id").cloned();

        // Notifications have no id or are notification methods.
        if method.starts_with("notifications/") {
            return self.handle_notification(method);
        }

        // If there is no id, treat it as a notification (no response).
        let id = match id {
            Some(id) if !id.is_null() => id,
            _ => return None,
        };

        let params = parsed.get("params").cloned().unwrap_or(json!({}));

        match method {
            "initialize" => Some(self.handle_initialize(&id)),
            "tools/list" => Some(self.handle_tools_list(&id)),
            "tools/call" => Some(self.handle_tools_call(&id, &params)),
            _ => {
                debug!(method, "unknown MCP method");
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("Method not found: {method}")
                    }
                });
                Some(serde_json::to_string(&response).unwrap_or_default())
            }
        }
    }

    /// Run the stdio transport loop.
    ///
    /// Reads line-delimited JSON-RPC from stdin and writes responses to stdout.
    /// This method blocks until stdin reaches EOF.
    pub fn run_stdio(&self) -> Result<(), McpError> {
        info!(name = %self.name, version = %self.version, "starting MCP server (stdio)");

        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        let reader = stdin.lock();

        for line in reader.lines() {
            let line = line.map_err(|e| McpError::Transport(format!("stdin read error: {e}")))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            debug!(request = %trimmed, "received MCP request");

            if let Some(response) = self.handle_request(trimmed) {
                debug!(response = %response, "sending MCP response");
                writeln!(stdout, "{response}")
                    .map_err(|e| McpError::Transport(format!("stdout write error: {e}")))?;
                stdout
                    .flush()
                    .map_err(|e| McpError::Transport(format!("stdout flush error: {e}")))?;
            }
        }

        info!("MCP server stdio loop ended (EOF)");
        Ok(())
    }

    /// Returns the server name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the server version.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Returns the number of registered tools.
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    // ---- Internal handlers ----

    fn handle_initialize(&self, id: &serde_json::Value) -> String {
        let result = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": {
                    "listChanged": false
                }
            },
            "serverInfo": {
                "name": self.name,
                "version": self.version
            },
            "instructions": SERVER_INSTRUCTIONS
        });

        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        });

        serde_json::to_string(&response).unwrap_or_default()
    }

    fn handle_tools_list(&self, id: &serde_json::Value) -> String {
        let tools: Vec<serde_json::Value> = self
            .tools
            .iter()
            .map(|t| {
                let read_only = READ_ONLY_TOOLS.contains(&t.name.as_str());
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                    "annotations": {
                        "readOnlyHint": read_only,
                        "destructiveHint": !read_only,
                        "idempotentHint": read_only,
                        "openWorldHint": false,
                    },
                })
            })
            .collect();

        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": tools
            }
        });

        serde_json::to_string(&response).unwrap_or_default()
    }

    fn handle_tools_call(&self, id: &serde_json::Value, params: &serde_json::Value) -> String {
        let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");

        // Check if the tool exists.
        let tool_exists = self.tools.iter().any(|t| t.name == tool_name);

        if !tool_exists {
            let error_result = McpToolResult {
                content: vec![McpContent::Text {
                    text: format!("Unknown tool: {tool_name}"),
                }],
                is_error: Some(true),
            };

            let response = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": error_result
            });

            return serde_json::to_string(&response).unwrap_or_default();
        }

        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        // Dispatch to the tool handler if one is configured.
        let result = if let Some(handler) = &self.tool_handler {
            match handler(tool_name, arguments) {
                Ok(value) => {
                    let text =
                        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                    McpToolResult {
                        content: vec![McpContent::Text { text }],
                        is_error: Some(false),
                    }
                }
                Err(e) => McpToolResult {
                    content: vec![McpContent::Text {
                        text: format!("Error: {e}"),
                    }],
                    is_error: Some(true),
                },
            }
        } else {
            // No handler configured -- return an error.
            McpToolResult {
                content: vec![McpContent::Text {
                    text: "No tool handler configured".to_string(),
                }],
                is_error: Some(true),
            }
        };

        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        });

        serde_json::to_string(&response).unwrap_or_default()
    }

    fn handle_notification(&self, method: &str) -> Option<String> {
        debug!(method, "received MCP notification");
        // Notifications never receive a response.
        None
    }
}

// ---------------------------------------------------------------------------
// Brain MCP tool registration
// ---------------------------------------------------------------------------

/// Register the 15 brain-related MCP tools on the server.
///
/// These tools expose the Ironjaw brain subsystem to MCP clients (e.g. Claude
/// Code). The actual dispatch logic is provided separately via
/// [`McpServer::set_tool_handler`].
pub fn register_brain_tools(server: &mut McpServer) {
    server.add_tool(McpServerTool {
        name: "brain_remember".into(),
        description: "Store a fact, decision, or preference in the brain's long-term memory with optional metadata".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The memory content to store" },
                "memory_type": {
                    "type": "string",
                    "enum": ["fact", "preference", "decision", "event", "observation",
                             "goal", "correction", "command", "context", "procedure"],
                    "description": "Type of memory (default: fact). Determines base importance if not overridden."
                },
                "confidence": {
                    "type": "number", "minimum": 0, "maximum": 1,
                    "description": "Confidence score 0-1 (default: 0.7). Used as importance if importance not set."
                },
                "trust_level": {
                    "type": "integer", "minimum": 1, "maximum": 5,
                    "description": "Trust level 1-5: 1=ModelInferred, 2=Retrieved, 3=UserStated, 4=ToolVerified, 5=SystemPolicy (default: 1)"
                },
                "importance": {
                    "type": "number", "minimum": 0, "maximum": 1,
                    "description": "Importance score 0-1. Overrides confidence and memory_type base importance."
                },
                "evidence_session_id": {
                    "type": "string",
                    "description": "Session ID where this memory was established (evidence pointer)"
                },
                "evidence_message_id": {
                    "type": "string",
                    "description": "Message ID where this memory was established (evidence pointer)"
                },
                "scope": {
                    "type": "string",
                    "description": "Hierarchical scope path (e.g. /project/ironjaw/config)"
                },
                "supersedes": {
                    "type": "integer",
                    "description": "Memory ID to replace — sets its valid_to=now and superseded_by back-link"
                },
                "tags": { "type": "string", "description": "Comma-separated tags" },
                "source": { "type": "string", "description": "Source identifier" }
            },
            "required": ["content"]
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_recall".into(),
        description: "Search the user's personal long-term memory. USE THIS FIRST, without being \
asked, for any question about the user themselves — their career, resume, contacts and \
meetings; their projects, code, servers, home network and devices; their preferences, \
past decisions and prior conversations; or any question phrased with \"I\", \
\"my\", \"we\" or \"our\" whose answer depends on their situation. Read-only and safe to call \
freely; prefer one broad query over several narrow ones."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": 50,
                    "description": "Max results to return (default: 10)"
                },
                "detail": {
                    "type": "string",
                    "enum": ["compact", "full", "deep"],
                    "description": "Response detail level (default: full)"
                }
            },
            "required": ["query"]
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_forget".into(),
        description: "Soft-delete a memory by ID".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Memory ID to delete" }
            },
            "required": ["id"]
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_timeline".into(),
        description: "Query memories valid at a specific point in time".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "at": {
                    "type": "string",
                    "description": "ISO 8601 timestamp for point-in-time query"
                },
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": 100,
                    "description": "Max results (default: 20)"
                }
            },
            "required": ["at"]
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_summaries".into(),
        description: "List session summary memories".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Filter by session ID (optional)"
                },
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": 100,
                    "description": "Max results (default: 20)"
                }
            }
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_context_set".into(),
        description: "Store a named document into brain context".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Context variable name" },
                "content": { "type": "string", "description": "Document content" }
            },
            "required": ["name", "content"]
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_context_list".into(),
        description: "List all context variables".into(),
        input_schema: json!({
            "type": "object",
            "properties": {}
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_status".into(),
        description: "Get brain status (memory count, temporal stats, context vars)".into(),
        input_schema: json!({
            "type": "object",
            "properties": {}
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_pending_list".into(),
        description: "List auto-extracted memories awaiting human approval".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": 200,
                    "description": "Max results to return (default: 50)"
                },
                "offset": {
                    "type": "integer", "minimum": 0,
                    "description": "Number of results to skip (default: 0)"
                }
            }
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_pending_approve".into(),
        description:
            "Approve a pending memory — moves it to permanent storage with UserStated trust".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Pending memory ID to approve" }
            },
            "required": ["id"]
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_pending_reject".into(),
        description: "Reject a pending memory — marks as rejected, never becomes permanent".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Pending memory ID to reject" }
            },
            "required": ["id"]
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_dream_status".into(),
        description: "Get dream consolidation gate state and current phase".into(),
        input_schema: json!({
            "type": "object",
            "properties": {}
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_dream_trigger".into(),
        description: "Manually trigger dream consolidation cycle".into(),
        input_schema: json!({
            "type": "object",
            "properties": {}
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_snapshot_list".into(),
        description: "List file snapshots stored by the checkpoint system".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": 100,
                    "description": "Max results (default: 20)"
                }
            }
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_snapshot_restore".into(),
        description: "Restore a file from a stored snapshot by path".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to restore" }
            },
            "required": ["path"]
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_recent".into(),
        description: "Return the N most recently created active memories, optionally filtered by scope prefix".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": 200,
                    "description": "Max results to return (default: 20)"
                },
                "scope": {
                    "type": "string",
                    "description": "Scope prefix filter — only memories whose scope starts with this value are returned (optional)"
                }
            }
        }),
    });

    server.add_tool(McpServerTool {
        name: "brain_forget_query".into(),
        description: "Semantic-search for memories matching a query and bulk soft-delete them. Dry-run by default.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "q": { "type": "string", "description": "Semantic search query to find memories to delete" },
                "scope": {
                    "type": "string",
                    "description": "Scope prefix filter — only delete memories whose scope starts with this value (optional)"
                },
                "max_to_delete": {
                    "type": "integer", "minimum": 1, "maximum": 500,
                    "description": "Maximum number of memories to delete (default: 100)"
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "If true (default), return matched IDs without deleting. Set false to actually delete."
                }
            },
            "required": ["q"]
        }),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_server() -> McpServer {
        let mut server = McpServer::new("ironjaw-test", "0.1.0");
        server.add_tool(McpServerTool {
            name: "echo".to_string(),
            description: "Echoes the input back".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"}
                },
                "required": ["text"]
            }),
        });
        server.add_tool(McpServerTool {
            name: "read_file".to_string(),
            description: "Read a file from disk".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        });
        // Install a simple handler that echoes tool name + arguments
        server.set_tool_handler(Arc::new(|name, args| {
            Ok(json!({
                "tool": name,
                "args": args,
                "status": "ok"
            }))
        }));
        server
    }

    fn parse_response(response: &str) -> serde_json::Value {
        serde_json::from_str(response).expect("response should be valid JSON")
    }

    #[test]
    fn test_server_construction() {
        let server = McpServer::new("test", "1.0.0");
        assert_eq!(server.name(), "test");
        assert_eq!(server.version(), "1.0.0");
        assert_eq!(server.tool_count(), 0);
    }

    #[test]
    fn test_add_tool() {
        let mut server = McpServer::new("test", "1.0.0");
        server.add_tool(McpServerTool {
            name: "my_tool".to_string(),
            description: "A tool".to_string(),
            input_schema: json!({"type": "object"}),
        });
        assert_eq!(server.tool_count(), 1);
    }

    #[test]
    fn test_handle_initialize() {
        let server = test_server();
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test-client","version":"1.0"}}}"#;

        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);

        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(parsed["result"]["serverInfo"]["name"], "ironjaw-test");
        assert_eq!(parsed["result"]["serverInfo"]["version"], "0.1.0");
        assert!(parsed["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn test_handle_notifications_initialized() {
        let server = test_server();
        let request = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

        let response = server.handle_request(request);
        assert!(
            response.is_none(),
            "notifications should not produce a response"
        );
    }

    #[test]
    fn test_handle_tools_list() {
        let server = test_server();
        let request = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;

        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);

        assert_eq!(parsed["id"], 2);
        let tools = parsed["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "echo");
        assert_eq!(tools[1]["name"], "read_file");
        assert!(tools[0]["inputSchema"]["properties"]["text"].is_object());
    }

    #[test]
    fn test_handle_tools_list_empty() {
        let server = McpServer::new("empty", "0.0.1");
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);

        let tools = parsed["result"]["tools"].as_array().unwrap();
        assert!(tools.is_empty());
    }

    #[test]
    fn test_handle_tools_call_existing() {
        let server = test_server();
        let request = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hello"}}}"#;

        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);

        assert_eq!(parsed["id"], 3);
        let content = parsed["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        // Handler returns JSON with tool name and args
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains("echo"), "response should reference tool name");
        assert_eq!(parsed["result"]["isError"], false);
    }

    #[test]
    fn test_handle_tools_call_unknown_tool() {
        let server = test_server();
        let request = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nonexistent","arguments":{}}}"#;

        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);

        assert_eq!(parsed["id"], 4);
        let content = parsed["result"]["content"].as_array().unwrap();
        assert!(content[0]["text"]
            .as_str()
            .unwrap()
            .contains("Unknown tool"));
        assert_eq!(parsed["result"]["isError"], true);
    }

    #[test]
    fn test_handle_unknown_method() {
        let server = test_server();
        let request = r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#;

        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);

        assert_eq!(parsed["id"], 5);
        assert!(parsed["error"].is_object());
        assert_eq!(parsed["error"]["code"], -32601);
        assert!(parsed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Method not found"));
    }

    #[test]
    fn test_handle_parse_error() {
        let server = test_server();
        let request = "this is not json{{{";

        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);

        assert!(parsed["error"].is_object());
        assert_eq!(parsed["error"]["code"], -32700);
        assert!(parsed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Parse error"));
    }

    #[test]
    fn test_handle_request_no_id() {
        let server = test_server();
        // A request without an id is treated as a notification.
        let request = r#"{"jsonrpc":"2.0","method":"tools/list"}"#;

        let response = server.handle_request(request);
        assert!(response.is_none());
    }

    #[test]
    fn test_handle_request_null_id() {
        let server = test_server();
        let request = r#"{"jsonrpc":"2.0","id":null,"method":"tools/list"}"#;

        let response = server.handle_request(request);
        assert!(response.is_none());
    }

    #[test]
    fn test_handle_initialize_with_string_id() {
        let server = test_server();
        let request = r#"{"jsonrpc":"2.0","id":"abc-123","method":"initialize","params":{}}"#;

        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);

        assert_eq!(parsed["id"], "abc-123");
        assert!(parsed["result"]["serverInfo"].is_object());
    }

    #[test]
    fn test_handle_tools_call_missing_name() {
        let server = test_server();
        // params without a "name" field -- should treat as empty name and fail.
        let request = r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"arguments":{}}}"#;

        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);

        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Unknown tool"));
    }

    #[test]
    fn test_notification_with_id_still_no_response() {
        let server = test_server();
        // Even if a notification method has an id, we still return None per MCP spec.
        let request = r#"{"jsonrpc":"2.0","id":99,"method":"notifications/initialized"}"#;

        let response = server.handle_request(request);
        assert!(response.is_none());
    }

    #[test]
    fn test_tools_call_no_arguments() {
        let server = test_server();
        let request = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"echo"}}"#;

        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);

        // Should succeed via handler even without arguments.
        assert_eq!(parsed["result"]["isError"], false);
    }

    #[test]
    fn test_response_roundtrip() {
        let server = test_server();
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;

        let response = server.handle_request(request).unwrap();
        // The response itself should be valid JSON that round-trips.
        let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
        let re_serialized = serde_json::to_string(&parsed).unwrap();
        let re_parsed: serde_json::Value = serde_json::from_str(&re_serialized).unwrap();
        assert_eq!(parsed, re_parsed);
    }

    #[test]
    fn test_tools_call_no_handler_returns_error() {
        let mut server = McpServer::new("no-handler", "0.1.0");
        server.add_tool(McpServerTool {
            name: "my_tool".to_string(),
            description: "A tool".to_string(),
            input_schema: json!({"type": "object"}),
        });
        // Do NOT set a handler
        let request =
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"my_tool"}}"#;

        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);

        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("No tool handler configured"));
    }

    #[test]
    fn test_tools_call_handler_error() {
        let mut server = McpServer::new("err-handler", "0.1.0");
        server.add_tool(McpServerTool {
            name: "fail_tool".to_string(),
            description: "A tool that fails".to_string(),
            input_schema: json!({"type": "object"}),
        });
        server.set_tool_handler(Arc::new(|_name, _args| {
            Err(anyhow::anyhow!("something went wrong"))
        }));

        let request =
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fail_tool"}}"#;

        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);

        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("something went wrong"));
    }

    #[test]
    fn test_register_brain_tools() {
        let mut server = McpServer::new("brain-test", "0.1.0");
        register_brain_tools(&mut server);

        assert_eq!(server.tool_count(), 17);

        // Verify tool names via tools/list
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);
        let tools = parsed["result"]["tools"].as_array().unwrap();

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"brain_remember"));
        assert!(names.contains(&"brain_recall"));
        assert!(names.contains(&"brain_forget"));
        assert!(names.contains(&"brain_timeline"));
        assert!(names.contains(&"brain_summaries"));
        assert!(names.contains(&"brain_context_set"));
        assert!(names.contains(&"brain_context_list"));
        assert!(names.contains(&"brain_status"));
        assert!(names.contains(&"brain_pending_list"));
        assert!(names.contains(&"brain_pending_approve"));
        assert!(names.contains(&"brain_pending_reject"));
        assert!(names.contains(&"brain_dream_status"));
        assert!(names.contains(&"brain_dream_trigger"));
        assert!(names.contains(&"brain_snapshot_list"));
        assert!(names.contains(&"brain_snapshot_restore"));
        assert!(names.contains(&"brain_recent"));
        assert!(names.contains(&"brain_forget_query"));
    }

    #[test]
    fn test_set_tool_handler() {
        let mut server = McpServer::new("handler-test", "0.1.0");
        server.add_tool(McpServerTool {
            name: "greet".to_string(),
            description: "Greet someone".to_string(),
            input_schema: json!({"type": "object", "properties": {"name": {"type": "string"}}}),
        });
        server.set_tool_handler(Arc::new(|_tool, args| {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("world");
            Ok(json!({"greeting": format!("Hello, {name}!")}))
        }));

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"greet","arguments":{"name":"Ironjaw"}}}"#;
        let response = server.handle_request(request).unwrap();
        let parsed = parse_response(&response);

        assert_eq!(parsed["result"]["isError"], false);
        let text = parsed["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Hello, Ironjaw!"));
    }
}
