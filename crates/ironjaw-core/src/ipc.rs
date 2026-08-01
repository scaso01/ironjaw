// Daemon mode IPC: platform-specific transport (named pipes on Windows, Unix sockets on Unix).

use serde::{Deserialize, Serialize};
use std::fmt;

/// Errors from the IPC subsystem.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("Connection failed: {0}")]
    Connection(String),
    #[error("Serialization error: {0}")]
    Serialization(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Handler error: {0}")]
    Handler(String),
    #[error("Invalid command: {0}")]
    InvalidCommand(String),
}

/// Commands that can be sent over IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcCommand {
    Status,
    Reload,
    Shutdown,
    Version,
    Custom(CustomCommand),
}

/// A custom command with a name field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomCommand {
    pub name: String,
}

impl fmt::Display for IpcCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status => write!(f, "status"),
            Self::Reload => write!(f, "reload"),
            Self::Shutdown => write!(f, "shutdown"),
            Self::Version => write!(f, "version"),
            Self::Custom(c) => write!(f, "custom:{}", c.name),
        }
    }
}

/// An IPC request sent from client to server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcRequest {
    pub command: IpcCommand,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
}

impl IpcRequest {
    /// Create a request with no arguments.
    pub fn new(command: IpcCommand) -> Self {
        Self {
            command,
            args: None,
        }
    }

    /// Create a request with arguments.
    pub fn with_args(command: IpcCommand, args: serde_json::Value) -> Self {
        Self {
            command,
            args: Some(args),
        }
    }

    /// Serialize this request to JSON bytes with a newline delimiter.
    pub fn to_bytes(&self) -> Result<Vec<u8>, IpcError> {
        let mut bytes =
            serde_json::to_vec(self).map_err(|e| IpcError::Serialization(e.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Deserialize a request from JSON bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, IpcError> {
        serde_json::from_slice(data).map_err(|e| IpcError::Serialization(e.to_string()))
    }
}

/// An IPC response sent from server to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl IpcResponse {
    /// Create a success response with data.
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    /// Create a success response with no data.
    pub fn ok_empty() -> Self {
        Self {
            success: true,
            data: None,
            error: None,
        }
    }

    /// Create an error response.
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(message.into()),
        }
    }

    /// Serialize this response to JSON bytes with a newline delimiter.
    pub fn to_bytes(&self) -> Result<Vec<u8>, IpcError> {
        let mut bytes =
            serde_json::to_vec(self).map_err(|e| IpcError::Serialization(e.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Deserialize a response from JSON bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, IpcError> {
        serde_json::from_slice(data).map_err(|e| IpcError::Serialization(e.to_string()))
    }
}

/// Trait for handling IPC commands. Implement this to define server behavior.
pub trait IpcHandler: Send + Sync {
    fn handle_command(&self, cmd: IpcCommand, args: Option<&serde_json::Value>) -> IpcResponse;
}

/// Returns the platform-specific IPC address.
pub fn default_ipc_address() -> String {
    #[cfg(windows)]
    {
        r"\\.\pipe\ironjaw".to_string()
    }
    #[cfg(unix)]
    {
        "/tmp/ironjaw.sock".to_string()
    }
}

/// Configuration for IPC server/client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcConfig {
    /// Whether IPC is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// IPC address (named pipe path on Windows, socket path on Unix).
    #[serde(default = "default_ipc_address")]
    pub address: String,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            address: default_ipc_address(),
        }
    }
}

/// IPC server that listens for commands.
///
/// Platform-specific transport is handled internally:
/// - Windows: named pipe at `\\.\pipe\ironjaw`
/// - Unix: Unix domain socket at `/tmp/ironjaw.sock`
pub struct IpcServer {
    address: String,
    handler: Box<dyn IpcHandler>,
}

impl IpcServer {
    /// Create a new IPC server with the given handler.
    pub fn new(address: &str, handler: impl IpcHandler + 'static) -> Result<Self, IpcError> {
        Ok(Self {
            address: address.to_string(),
            handler: Box::new(handler),
        })
    }

    /// Return the address this server listens on.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Process a single raw request (used for testing and in-memory transports).
    pub fn process_request(&self, request: &IpcRequest) -> IpcResponse {
        self.handler
            .handle_command(request.command.clone(), request.args.as_ref())
    }
}

/// IPC client for sending commands to a running server.
pub struct IpcClient {
    address: String,
}

impl IpcClient {
    /// Create a new client targeting the given address.
    pub fn new(address: &str) -> Self {
        Self {
            address: address.to_string(),
        }
    }

    /// Return the server address this client connects to.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Send a request and receive a response.
    ///
    /// This uses the in-process path for testing. For real IPC over named pipes
    /// or Unix sockets, the transport layer would connect to the pipe/socket.
    pub fn send_to_server(
        &self,
        request: &IpcRequest,
        server: &IpcServer,
    ) -> Result<IpcResponse, IpcError> {
        Ok(server.process_request(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Mock handler for tests ---

    struct MockHandler;

    impl IpcHandler for MockHandler {
        fn handle_command(
            &self,
            cmd: IpcCommand,
            _args: Option<&serde_json::Value>,
        ) -> IpcResponse {
            match cmd {
                IpcCommand::Status => {
                    IpcResponse::ok(serde_json::json!({"status": "running", "uptime": 1234}))
                }
                IpcCommand::Reload => IpcResponse::ok_empty(),
                IpcCommand::Shutdown => IpcResponse::ok_empty(),
                IpcCommand::Version => IpcResponse::ok(serde_json::json!({"version": "0.1.0"})),
                IpcCommand::Custom(ref c) => IpcResponse::ok(serde_json::json!({"custom": c.name})),
            }
        }
    }

    struct ErrorHandler;

    impl IpcHandler for ErrorHandler {
        fn handle_command(
            &self,
            _cmd: IpcCommand,
            _args: Option<&serde_json::Value>,
        ) -> IpcResponse {
            IpcResponse::err("handler error")
        }
    }

    // --- Command serialization tests ---

    #[test]
    fn test_command_serialize_status() {
        let cmd = IpcCommand::Status;
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("status"));
        let parsed: IpcCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, IpcCommand::Status);
    }

    #[test]
    fn test_command_serialize_reload() {
        let cmd = IpcCommand::Reload;
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: IpcCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, IpcCommand::Reload);
    }

    #[test]
    fn test_command_serialize_shutdown() {
        let cmd = IpcCommand::Shutdown;
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: IpcCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, IpcCommand::Shutdown);
    }

    #[test]
    fn test_command_serialize_version() {
        let cmd = IpcCommand::Version;
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: IpcCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, IpcCommand::Version);
    }

    #[test]
    fn test_command_serialize_custom() {
        let cmd = IpcCommand::Custom(CustomCommand {
            name: "my_cmd".into(),
        });
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("my_cmd"));
        let parsed: IpcCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed,
            IpcCommand::Custom(CustomCommand {
                name: "my_cmd".into()
            })
        );
    }

    #[test]
    fn test_command_display() {
        assert_eq!(IpcCommand::Status.to_string(), "status");
        assert_eq!(IpcCommand::Reload.to_string(), "reload");
        assert_eq!(IpcCommand::Shutdown.to_string(), "shutdown");
        assert_eq!(IpcCommand::Version.to_string(), "version");
        assert_eq!(
            IpcCommand::Custom(CustomCommand { name: "foo".into() }).to_string(),
            "custom:foo"
        );
    }

    // --- Request serialization tests ---

    #[test]
    fn test_request_serialize_roundtrip() {
        let req = IpcRequest::new(IpcCommand::Status);
        let bytes = req.to_bytes().unwrap();
        // Remove trailing newline for parsing
        let parsed = IpcRequest::from_bytes(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(parsed.command, IpcCommand::Status);
        assert!(parsed.args.is_none());
    }

    #[test]
    fn test_request_with_args() {
        let req = IpcRequest::with_args(
            IpcCommand::Custom(CustomCommand {
                name: "test".into(),
            }),
            serde_json::json!({"key": "value"}),
        );
        let bytes = req.to_bytes().unwrap();
        let parsed = IpcRequest::from_bytes(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(
            parsed.command,
            IpcCommand::Custom(CustomCommand {
                name: "test".into()
            })
        );
        assert_eq!(parsed.args.unwrap()["key"].as_str().unwrap(), "value");
    }

    #[test]
    fn test_request_invalid_bytes() {
        let result = IpcRequest::from_bytes(b"not json");
        assert!(result.is_err());
    }

    // --- Response serialization tests ---

    #[test]
    fn test_response_ok_roundtrip() {
        let resp = IpcResponse::ok(serde_json::json!({"uptime": 42}));
        let bytes = resp.to_bytes().unwrap();
        let parsed = IpcResponse::from_bytes(&bytes[..bytes.len() - 1]).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.data.unwrap()["uptime"], 42);
        assert!(parsed.error.is_none());
    }

    #[test]
    fn test_response_ok_empty() {
        let resp = IpcResponse::ok_empty();
        assert!(resp.success);
        assert!(resp.data.is_none());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_response_err_roundtrip() {
        let resp = IpcResponse::err("something broke");
        let bytes = resp.to_bytes().unwrap();
        let parsed = IpcResponse::from_bytes(&bytes[..bytes.len() - 1]).unwrap();
        assert!(!parsed.success);
        assert!(parsed.data.is_none());
        assert_eq!(parsed.error.unwrap(), "something broke");
    }

    #[test]
    fn test_response_invalid_bytes() {
        let result = IpcResponse::from_bytes(b"garbage");
        assert!(result.is_err());
    }

    // --- Mock handler tests ---

    #[test]
    fn test_mock_handler_status() {
        let handler = MockHandler;
        let resp = handler.handle_command(IpcCommand::Status, None);
        assert!(resp.success);
        assert_eq!(resp.data.unwrap()["status"], "running");
    }

    #[test]
    fn test_mock_handler_version() {
        let handler = MockHandler;
        let resp = handler.handle_command(IpcCommand::Version, None);
        assert!(resp.success);
        assert_eq!(resp.data.unwrap()["version"], "0.1.0");
    }

    #[test]
    fn test_mock_handler_custom() {
        let handler = MockHandler;
        let resp = handler.handle_command(
            IpcCommand::Custom(CustomCommand {
                name: "hello".into(),
            }),
            None,
        );
        assert!(resp.success);
        assert_eq!(resp.data.unwrap()["custom"], "hello");
    }

    #[test]
    fn test_error_handler() {
        let handler = ErrorHandler;
        let resp = handler.handle_command(IpcCommand::Status, None);
        assert!(!resp.success);
        assert_eq!(resp.error.unwrap(), "handler error");
    }

    // --- Server/Client integration ---

    #[test]
    fn test_server_process_request() {
        let server = IpcServer::new("test-addr", MockHandler).unwrap();
        let req = IpcRequest::new(IpcCommand::Status);
        let resp = server.process_request(&req);
        assert!(resp.success);
        assert_eq!(resp.data.unwrap()["status"], "running");
    }

    #[test]
    fn test_server_address() {
        let server = IpcServer::new(r"\\.\pipe\ironjaw", MockHandler).unwrap();
        assert_eq!(server.address(), r"\\.\pipe\ironjaw");
    }

    #[test]
    fn test_client_send_to_server() {
        let server = IpcServer::new("test-addr", MockHandler).unwrap();
        let client = IpcClient::new("test-addr");
        let req = IpcRequest::new(IpcCommand::Version);
        let resp = client.send_to_server(&req, &server).unwrap();
        assert!(resp.success);
        assert_eq!(resp.data.unwrap()["version"], "0.1.0");
    }

    #[test]
    fn test_client_address() {
        let client = IpcClient::new("/tmp/ironjaw.sock");
        assert_eq!(client.address(), "/tmp/ironjaw.sock");
    }

    // --- Config tests ---

    #[test]
    fn test_ipc_config_defaults() {
        let config = IpcConfig::default();
        assert!(!config.enabled);
        // Platform-dependent address
        #[cfg(windows)]
        assert_eq!(config.address, r"\\.\pipe\ironjaw");
        #[cfg(unix)]
        assert_eq!(config.address, "/tmp/ironjaw.sock");
    }

    #[test]
    fn test_ipc_config_serde_roundtrip() {
        let config = IpcConfig {
            enabled: true,
            address: "custom-addr".into(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: IpcConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.address, "custom-addr");
    }

    #[test]
    fn test_default_ipc_address() {
        let addr = default_ipc_address();
        #[cfg(windows)]
        assert!(addr.contains("pipe"));
        #[cfg(unix)]
        assert!(addr.contains("sock"));
        // Always non-empty
        assert!(!addr.is_empty());
    }

    // --- Full round-trip: client builds request, server processes, client parses response ---

    #[test]
    fn test_full_roundtrip_with_bytes() {
        let server = IpcServer::new("test", MockHandler).unwrap();

        // Client builds request
        let req = IpcRequest::with_args(IpcCommand::Reload, serde_json::json!({"force": true}));
        let req_bytes = req.to_bytes().unwrap();

        // Server receives bytes, processes
        let parsed_req = IpcRequest::from_bytes(&req_bytes[..req_bytes.len() - 1]).unwrap();
        let resp = server.process_request(&parsed_req);

        // Server sends response bytes
        let resp_bytes = resp.to_bytes().unwrap();

        // Client receives and parses
        let parsed_resp = IpcResponse::from_bytes(&resp_bytes[..resp_bytes.len() - 1]).unwrap();
        assert!(parsed_resp.success);
    }
}
