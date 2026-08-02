use crate::error::McpError;
use crate::types::{JsonRpcRequest, JsonRpcResponse};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tracing::{debug, error, warn};

// ── Transport trait ──────────────────────────────────────────────────────────

/// Trait implemented by all MCP transports (HTTP and stdio).
///
/// Every `McpClient` holds an `Arc<dyn Transport>` so the client can be
/// cloned and shared across tasks without duplicating the underlying
/// connection.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Send a single JSON-RPC request and return the response.
    async fn send(&self, request: &JsonRpcRequest) -> Result<JsonRpcResponse, McpError>;
}

// ── HTTP transport ───────────────────────────────────────────────────────────

/// HTTP transport for MCP JSON-RPC communication.
///
/// Sends JSON-RPC requests over HTTP POST and deserializes responses.
pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
    api_key: Option<String>,
}

impl HttpTransport {
    /// Create a new HTTP transport targeting the given URL.
    pub fn new(url: String, api_key: Option<String>, timeout_secs: u64) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            url,
            api_key,
        }
    }

    /// Returns the target URL.
    pub fn url(&self) -> &str {
        &self.url
    }
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn send(&self, request: &JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        debug!(
            method = %request.method,
            id = %request.id,
            url = %self.url,
            "sending MCP HTTP request"
        );

        let mut builder = self.client.post(&self.url).json(request);

        if let Some(ref key) = self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }

        let http_response = builder.send().await.map_err(|e| {
            if e.is_timeout() {
                McpError::Timeout
            } else {
                McpError::Transport(format!("HTTP request failed: {e}"))
            }
        })?;

        let status = http_response.status();
        if !status.is_success() {
            let body = http_response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(McpError::Transport(format!("HTTP {status}: {body}")));
        }

        let response: JsonRpcResponse = http_response
            .json()
            .await
            .map_err(|e| McpError::Parse(format!("failed to parse JSON-RPC response: {e}")))?;

        // Check for JSON-RPC level error
        if let Some(ref err) = response.error {
            return Err(McpError::JsonRpc {
                code: err.code,
                message: err.message.clone(),
            });
        }

        Ok(response)
    }
}

// ── Stdio transport ──────────────────────────────────────────────────────────

/// Message sent through the actor channel: a request + a oneshot for its response.
type ActorMsg = (
    JsonRpcRequest,
    oneshot::Sender<Result<JsonRpcResponse, McpError>>,
);

/// Stdio transport for MCP JSON-RPC communication.
///
/// Spawns the MCP server as a child process and communicates over its
/// stdin/stdout using newline-delimited JSON-RPC.
///
/// A single background actor task owns the child process and performs
/// request/response correlation via a `HashMap<String, oneshot::Sender>`.
/// Callers send `(request, oneshot_tx)` over an mpsc channel; the actor writes
/// the request to stdin, awaits the matching response line, and fulfils the
/// oneshot.
pub struct StdioTransport {
    server_name: String,
    request_tx: mpsc::Sender<ActorMsg>,
    timeout_secs: u64,
}

impl std::fmt::Debug for StdioTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioTransport")
            .field("server_name", &self.server_name)
            .field("timeout_secs", &self.timeout_secs)
            .finish_non_exhaustive()
    }
}

impl StdioTransport {
    /// Spawn the child process and start the background actor.
    ///
    /// `command` is the executable name; `args` are its arguments.
    /// `timeout_secs` is applied per request on the caller side.
    pub fn spawn(
        server_name: impl Into<String>,
        command: &str,
        args: &[String],
        timeout_secs: u64,
    ) -> Result<Self, McpError> {
        let server_name = server_name.into();

        let mut child = Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                McpError::Transport(format!("failed to spawn stdio MCP server `{command}`: {e}"))
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("child stdin not available".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("child stdout not available".to_string()))?;

        let (tx, rx) = mpsc::channel::<ActorMsg>(64);
        let name_for_actor = server_name.clone();

        tokio::spawn(async move {
            run_stdio_actor(name_for_actor, child, stdin, stdout, rx).await;
        });

        Ok(Self {
            server_name,
            request_tx: tx,
            timeout_secs,
        })
    }

    /// Returns the server name.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

#[async_trait::async_trait]
impl Transport for StdioTransport {
    async fn send(&self, request: &JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let (resp_tx, resp_rx) = oneshot::channel();

        self.request_tx
            .send((request.clone(), resp_tx))
            .await
            .map_err(|_| McpError::Transport("stdio actor has shut down".to_string()))?;

        timeout(Duration::from_secs(self.timeout_secs), resp_rx)
            .await
            .map_err(|_| McpError::Timeout)?
            .map_err(|_| McpError::Transport("stdio actor dropped oneshot sender".to_string()))?
    }
}

// ── Background actor ─────────────────────────────────────────────────────────

/// Background task that owns the child process and manages request/response
/// correlation for stdio-based MCP servers.
///
/// The actor:
/// 1. Receives `(request, oneshot_tx)` from callers over `rx`.
/// 2. Writes the request as a JSON line to the child's stdin.
/// 3. Reads response lines from the child's stdout.
/// 4. Correlates responses to waiting callers by JSON-RPC `id`.
async fn run_stdio_actor(
    server_name: String,
    mut child: tokio::process::Child,
    mut stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    mut rx: mpsc::Receiver<ActorMsg>,
) {
    let mut reader = BufReader::new(stdout).lines();
    // Map from request id (as string) → oneshot sender
    let mut pending: HashMap<String, oneshot::Sender<Result<JsonRpcResponse, McpError>>> =
        HashMap::new();

    loop {
        tokio::select! {
            // New outgoing request from a caller
            maybe_msg = rx.recv() => {
                match maybe_msg {
                    None => {
                        // Channel closed — all callers dropped; shut down
                        debug!(server = %server_name, "stdio actor: request channel closed, shutting down");
                        break;
                    }
                    Some((request, resp_tx)) => {
                        let id_key = request.id.to_string();
                        match serde_json::to_string(&request) {
                            Ok(mut line) => {
                                line.push('\n');
                                if let Err(e) = stdin.write_all(line.as_bytes()).await {
                                    error!(
                                        server = %server_name,
                                        error = %e,
                                        "stdio actor: failed to write to child stdin"
                                    );
                                    let _ = resp_tx.send(Err(McpError::Transport(
                                        format!("stdin write failed: {e}")
                                    )));
                                    continue;
                                }
                                // Flush so the child sees the request immediately
                                if let Err(e) = stdin.flush().await {
                                    warn!(server = %server_name, error = %e, "stdin flush error");
                                }
                                pending.insert(id_key, resp_tx);
                            }
                            Err(e) => {
                                let _ = resp_tx.send(Err(McpError::Parse(
                                    format!("failed to serialize request: {e}")
                                )));
                            }
                        }
                    }
                }
            }

            // Incoming response line from the child's stdout
            line_result = reader.next_line() => {
                match line_result {
                    Err(e) => {
                        error!(server = %server_name, error = %e, "stdio actor: stdout read error");
                        // Fail all pending requests
                        for (_, tx) in pending.drain() {
                            let _ = tx.send(Err(McpError::Transport(
                                format!("stdout read error: {e}")
                            )));
                        }
                        break;
                    }
                    Ok(None) => {
                        // Child closed stdout (exited)
                        debug!(server = %server_name, "stdio actor: child stdout EOF");
                        for (_, tx) in pending.drain() {
                            let _ = tx.send(Err(McpError::Transport(
                                "child process exited".to_string()
                            )));
                        }
                        break;
                    }
                    Ok(Some(line)) => {
                        if line.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<JsonRpcResponse>(&line) {
                            Err(e) => {
                                warn!(
                                    server = %server_name,
                                    line = %line,
                                    error = %e,
                                    "stdio actor: failed to parse response line"
                                );
                            }
                            Ok(response) => {
                                let id_key = response.id.to_string();
                                if let Some(tx) = pending.remove(&id_key) {
                                    // Check JSON-RPC level error before forwarding
                                    if let Some(ref err) = response.error {
                                        let _ = tx.send(Err(McpError::JsonRpc {
                                            code: err.code,
                                            message: err.message.clone(),
                                        }));
                                    } else {
                                        let _ = tx.send(Ok(response));
                                    }
                                } else {
                                    warn!(
                                        server = %server_name,
                                        id = %id_key,
                                        "stdio actor: no pending request for response id"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Wait for child to exit (best-effort cleanup)
    if let Err(e) = child.wait().await {
        warn!(server = %server_name, error = %e, "stdio actor: child wait error");
    }
    debug!(server = %server_name, "stdio actor exited");
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transport_construction() {
        let transport = HttpTransport::new(
            "http://localhost:3000/mcp".to_string(),
            Some("sk-test".to_string()),
            30,
        );
        assert_eq!(transport.url(), "http://localhost:3000/mcp");
        assert_eq!(transport.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn test_transport_construction_no_key() {
        let transport = HttpTransport::new("http://localhost:8080/mcp".to_string(), None, 60);
        assert_eq!(transport.url(), "http://localhost:8080/mcp");
        assert!(transport.api_key.is_none());
    }

    /// Verify that `StdioTransport::spawn` fails gracefully when given a
    /// non-existent command — the actor should return a `McpError::Transport`.
    #[tokio::test]
    async fn test_stdio_transport_spawn_bad_command() {
        let result = StdioTransport::spawn(
            "test-server",
            "this_command_definitely_does_not_exist_ironjaw_test",
            &[],
            5,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            McpError::Transport(_) => {}
            other => panic!("expected McpError::Transport, got {other:?}"),
        }
    }

    /// Verify that a `StdioTransport` backed by a trivial echo-style process
    /// can round-trip a JSON-RPC response.
    ///
    /// We use `node --eval` with an inline script that reads one JSON line from
    /// stdin, parses it, and writes back a minimal `{"jsonrpc":"2.0","id":<id>,"result":{}}`
    /// line to stdout.  Node.js is required for this test to run; it is skipped
    /// on machines without Node.
    // Not gated to unix: this test was, which meant it never ran on a Windows
    // developer machine and its deadlock went unnoticed until CI. It skips itself
    // when node is missing, so running it everywhere costs nothing.
    #[tokio::test]
    async fn test_stdio_transport_round_trip_node() {
        // Skip if node is not available
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        // Reply per newline-delimited line, the way a real MCP stdio server does.
        // Waiting for stdin 'end' instead would deadlock: `send` keeps the pipe open
        // for the life of the transport, so 'end' never fires and every request
        // times out.
        let script = r#"
process.stdin.resume();
let buf = '';
process.stdin.on('data', d => {
  buf += d;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (!line) continue;
    const req = JSON.parse(line);
    process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:req.id,result:{}}) + '\n');
  }
});
"#;
        let transport = StdioTransport::spawn(
            "node-echo",
            "node",
            &["--eval".to_string(), script.to_string()],
            5,
        );
        // Node may not be on PATH; graceful skip
        if transport.is_err() {
            return;
        }
        let transport = std::sync::Arc::new(transport.unwrap());
        use crate::types::JsonRpcRequest;
        let req = JsonRpcRequest::new(serde_json::json!("test-1"), "ping", None);
        let resp = transport.send(&req).await;
        assert!(resp.is_ok(), "round-trip failed: {resp:?}");
    }
}
