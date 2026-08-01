use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    /// Create a new JSON-RPC 2.0 request.
    pub fn new(
        id: serde_json::Value,
        method: impl Into<String>,
        params: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 2.0 response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// An MCP tool descriptor returned by `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

/// Result of calling an MCP tool via `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolResult {
    pub content: Vec<McpContent>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// A single content block inside an `McpToolResult`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

/// Configuration for a single MCP server.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub url: String,
    pub api_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_jsonrpc_request_serialization() {
        let req = JsonRpcRequest::new(json!(1), "tools/list", None);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 1);
        assert_eq!(json["method"], "tools/list");
        assert!(json.get("params").is_none());
    }

    #[test]
    fn test_jsonrpc_request_with_params() {
        let req = JsonRpcRequest::new(
            json!("abc-123"),
            "tools/call",
            Some(json!({"name": "echo", "arguments": {"text": "hello"}})),
        );
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["method"], "tools/call");
        assert_eq!(json["params"]["name"], "echo");
    }

    #[test]
    fn test_jsonrpc_request_roundtrip() {
        let req = JsonRpcRequest::new(json!(42), "initialize", Some(json!({})));
        let serialized = serde_json::to_string(&req).unwrap();
        let deserialized: JsonRpcRequest = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.jsonrpc, "2.0");
        assert_eq!(deserialized.id, json!(42));
        assert_eq!(deserialized.method, "initialize");
    }

    #[test]
    fn test_jsonrpc_response_success() {
        let json_str = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"tools": []},
            "error": null
        }"#;
        let resp: JsonRpcResponse = serde_json::from_str(json_str).unwrap();
        assert_eq!(resp.jsonrpc, "2.0");
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_jsonrpc_response_error() {
        let json_str = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "result": null,
            "error": {"code": -32601, "message": "Method not found"}
        }"#;
        let resp: JsonRpcResponse = serde_json::from_str(json_str).unwrap();
        assert!(resp.result.is_none());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "Method not found");
        assert!(err.data.is_none());
    }

    #[test]
    fn test_jsonrpc_error_with_data() {
        let json_str = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32000, "message": "Server error", "data": {"detail": "something broke"}}
        }"#;
        let resp: JsonRpcResponse = serde_json::from_str(json_str).unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32000);
        assert!(err.data.is_some());
        assert_eq!(err.data.unwrap()["detail"], "something broke");
    }

    #[test]
    fn test_mcp_tool_deserialization() {
        let json_str = r#"{
            "name": "read_file",
            "description": "Read a file from disk",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }
        }"#;
        let tool: McpTool = serde_json::from_str(json_str).unwrap();
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.description.as_deref(), Some("Read a file from disk"));
        assert_eq!(tool.input_schema["type"], "object");
        assert_eq!(tool.input_schema["properties"]["path"]["type"], "string");
    }

    #[test]
    fn test_mcp_tool_without_description() {
        let json_str = r#"{
            "name": "noop",
            "inputSchema": {"type": "object"}
        }"#;
        let tool: McpTool = serde_json::from_str(json_str).unwrap();
        assert_eq!(tool.name, "noop");
        assert!(tool.description.is_none());
    }

    #[test]
    fn test_mcp_tool_result_text() {
        let json_str = r#"{
            "content": [
                {"type": "text", "text": "Hello, world!"}
            ]
        }"#;
        let result: McpToolResult = serde_json::from_str(json_str).unwrap();
        assert_eq!(result.content.len(), 1);
        assert!(result.is_error.is_none());
        match &result.content[0] {
            McpContent::Text { text } => assert_eq!(text, "Hello, world!"),
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn test_mcp_tool_result_image() {
        let json_str = r#"{
            "content": [
                {"type": "image", "data": "iVBOR...", "mimeType": "image/png"}
            ]
        }"#;
        let result: McpToolResult = serde_json::from_str(json_str).unwrap();
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            McpContent::Image { data, mime_type } => {
                assert_eq!(data, "iVBOR...");
                assert_eq!(mime_type, "image/png");
            }
            _ => panic!("expected image content"),
        }
    }

    #[test]
    fn test_mcp_tool_result_mixed_content() {
        let json_str = r#"{
            "content": [
                {"type": "text", "text": "Here is the image:"},
                {"type": "image", "data": "base64data", "mimeType": "image/jpeg"}
            ],
            "isError": false
        }"#;
        let result: McpToolResult = serde_json::from_str(json_str).unwrap();
        assert_eq!(result.content.len(), 2);
        assert_eq!(result.is_error, Some(false));
    }

    #[test]
    fn test_mcp_tool_result_error() {
        let json_str = r#"{
            "content": [
                {"type": "text", "text": "File not found: /tmp/missing.txt"}
            ],
            "isError": true
        }"#;
        let result: McpToolResult = serde_json::from_str(json_str).unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn test_mcp_server_config() {
        let json_str = r#"{
            "name": "my-server",
            "url": "http://localhost:3000/mcp",
            "api_key": "sk-test"
        }"#;
        let config: McpServerConfig = serde_json::from_str(json_str).unwrap();
        assert_eq!(config.name, "my-server");
        assert_eq!(config.url, "http://localhost:3000/mcp");
        assert_eq!(config.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn test_mcp_server_config_no_key() {
        let json_str = r#"{
            "name": "local-server",
            "url": "http://localhost:8080/mcp"
        }"#;
        let config: McpServerConfig = serde_json::from_str(json_str).unwrap();
        assert!(config.api_key.is_none());
    }

    #[test]
    fn test_mcp_content_text_serialization() {
        let content = McpContent::Text {
            text: "test output".to_string(),
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "test output");
    }

    #[test]
    fn test_mcp_content_image_serialization() {
        let content = McpContent::Image {
            data: "base64==".to_string(),
            mime_type: "image/png".to_string(),
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["type"], "image");
        assert_eq!(json["data"], "base64==");
        assert_eq!(json["mimeType"], "image/png");
    }
}
