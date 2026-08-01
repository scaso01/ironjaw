/// Errors that can occur during MCP client operations.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// HTTP or network-level transport failure.
    #[error("transport error: {0}")]
    Transport(String),

    /// JSON-RPC error returned by the server.
    #[error("JSON-RPC error {code}: {message}")]
    JsonRpc { code: i64, message: String },

    /// Failed to parse the response body.
    #[error("parse error: {0}")]
    Parse(String),

    /// Request timed out.
    #[error("request timed out")]
    Timeout,
}

impl McpError {
    /// Returns `true` if this is a timeout error.
    pub fn is_timeout(&self) -> bool {
        matches!(self, McpError::Timeout)
    }

    /// Returns `true` if this is a JSON-RPC error.
    pub fn is_jsonrpc(&self) -> bool {
        matches!(self, McpError::JsonRpc { .. })
    }
}

/// Compact display for logging — delegates to the `Display` from `thiserror`.
impl McpError {
    /// Returns a short error kind label for metrics/logging.
    pub fn kind(&self) -> &'static str {
        match self {
            McpError::Transport(_) => "transport",
            McpError::JsonRpc { .. } => "jsonrpc",
            McpError::Parse(_) => "parse",
            McpError::Timeout => "timeout",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transport_error() {
        let err = McpError::Transport("connection refused".to_string());
        assert!(!err.is_timeout());
        assert!(!err.is_jsonrpc());
        assert_eq!(err.kind(), "transport");
        assert!(err.to_string().contains("connection refused"));
    }

    #[test]
    fn test_jsonrpc_error() {
        let err = McpError::JsonRpc {
            code: -32601,
            message: "Method not found".to_string(),
        };
        assert!(err.is_jsonrpc());
        assert!(!err.is_timeout());
        assert_eq!(err.kind(), "jsonrpc");
        assert!(err.to_string().contains("-32601"));
        assert!(err.to_string().contains("Method not found"));
    }

    #[test]
    fn test_parse_error() {
        let err = McpError::Parse("unexpected token".to_string());
        assert_eq!(err.kind(), "parse");
        assert!(err.to_string().contains("unexpected token"));
    }

    #[test]
    fn test_timeout_error() {
        let err = McpError::Timeout;
        assert!(err.is_timeout());
        assert_eq!(err.kind(), "timeout");
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn test_error_display() {
        let err = McpError::JsonRpc {
            code: -32600,
            message: "Invalid Request".to_string(),
        };
        let display = format!("{err}");
        assert_eq!(display, "JSON-RPC error -32600: Invalid Request");
    }

    #[test]
    fn test_error_debug() {
        let err = McpError::Timeout;
        let debug = format!("{err:?}");
        assert!(debug.contains("Timeout"));
    }
}
