use crate::types::{MemoryHint, Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const RING_BUFFER_SIZE: usize = 64 * 1024; // 64 KB

// ── Ring buffer for captured output ──────────────────────────────────────────

/// Fixed-size ring buffer that captures the most recent bytes of output.
#[derive(Debug)]
struct RingBuffer {
    data: Vec<u8>,
    pos: usize,
    full: bool,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            data: vec![0u8; capacity],
            pos: 0,
            full: false,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        let cap = self.data.len();
        for &b in bytes {
            self.data[self.pos] = b;
            self.pos = (self.pos + 1) % cap;
            if self.pos == 0 {
                self.full = true;
            }
        }
    }

    fn read(&self) -> Vec<u8> {
        if self.full {
            let mut out = Vec::with_capacity(self.data.len());
            out.extend_from_slice(&self.data[self.pos..]);
            out.extend_from_slice(&self.data[..self.pos]);
            out
        } else {
            self.data[..self.pos].to_vec()
        }
    }
}

// ── Process status ───────────────────────────────────────────────────────────

/// Status of a managed process.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProcessStatus {
    Running,
    Exited { code: i32 },
    Killed,
}

// ── Managed process ──────────────────────────────────────────────────────────

/// A child process managed by the ProcessManager.
struct ManagedProcess {
    child: Child,
    command: String,
    stdout_buf: Arc<Mutex<RingBuffer>>,
    stderr_buf: Arc<Mutex<RingBuffer>>,
    status: ProcessStatus,
}

// ── Process info (returned by list) ──────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProcessInfo {
    pub id: String,
    pub command: String,
    pub status: ProcessStatus,
}

// ── Process manager ──────────────────────────────────────────────────────────

/// Manages long-running child processes with output capture.
#[derive(Clone)]
pub struct ProcessManager {
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Spawn a new child process, returning its unique ID.
    pub async fn spawn(
        &self,
        command: &str,
        args: &[&str],
        working_dir: &std::path::Path,
    ) -> Result<String, String> {
        use std::process::Stdio;

        let mut cmd = Command::new(command);
        cmd.args(args)
            .current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn process: {e}"))?;

        let process_id = uuid::Uuid::new_v4().to_string();

        let stdout_buf = Arc::new(Mutex::new(RingBuffer::new(RING_BUFFER_SIZE)));
        let stderr_buf = Arc::new(Mutex::new(RingBuffer::new(RING_BUFFER_SIZE)));

        // Spawn background tasks to drain stdout/stderr into ring buffers
        if let Some(stdout) = child.stdout.take() {
            let buf = Arc::clone(&stdout_buf);
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut reader = stdout;
                let mut tmp = [0u8; 4096];
                loop {
                    match reader.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.lock().await.write(&tmp[..n]),
                    }
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            let buf = Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut reader = stderr;
                let mut tmp = [0u8; 4096];
                loop {
                    match reader.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.lock().await.write(&tmp[..n]),
                    }
                }
            });
        }

        let managed = ManagedProcess {
            child,
            command: format!("{command} {}", args.join(" ")),
            stdout_buf,
            stderr_buf,
            status: ProcessStatus::Running,
        };

        self.processes
            .lock()
            .await
            .insert(process_id.clone(), managed);
        Ok(process_id)
    }

    /// Poll a process for its current status and recent output.
    pub async fn poll(&self, process_id: &str) -> Result<(ProcessStatus, String, String), String> {
        let mut procs = self.processes.lock().await;
        let proc = procs
            .get_mut(process_id)
            .ok_or_else(|| format!("Process '{process_id}' not found"))?;

        // Try to reap the child without blocking
        if matches!(proc.status, ProcessStatus::Running) {
            match proc.child.try_wait() {
                Ok(Some(exit_status)) => {
                    let code = exit_status.code().unwrap_or(-1);
                    proc.status = ProcessStatus::Exited { code };
                }
                Ok(None) => {} // still running
                Err(e) => {
                    tracing::warn!("Failed to poll process {process_id}: {e}");
                }
            }
        }

        let status = proc.status.clone();
        let stdout = String::from_utf8_lossy(&proc.stdout_buf.lock().await.read()).to_string();
        let stderr = String::from_utf8_lossy(&proc.stderr_buf.lock().await.read()).to_string();

        Ok((status, stdout, stderr))
    }

    /// Write data to a process's stdin.
    pub async fn write_stdin(&self, process_id: &str, data: &str) -> Result<(), String> {
        let mut procs = self.processes.lock().await;
        let proc = procs
            .get_mut(process_id)
            .ok_or_else(|| format!("Process '{process_id}' not found"))?;

        if !matches!(proc.status, ProcessStatus::Running) {
            return Err("Process is not running".to_string());
        }

        let stdin = proc
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| "Process stdin not available".to_string())?;

        stdin
            .write_all(data.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to stdin: {e}"))?;

        stdin
            .flush()
            .await
            .map_err(|e| format!("Failed to flush stdin: {e}"))?;

        Ok(())
    }

    /// Kill a managed process.
    pub async fn kill(&self, process_id: &str) -> Result<(), String> {
        let mut procs = self.processes.lock().await;
        let proc = procs
            .get_mut(process_id)
            .ok_or_else(|| format!("Process '{process_id}' not found"))?;

        if !matches!(proc.status, ProcessStatus::Running) {
            return Err("Process is not running".to_string());
        }

        proc.child
            .kill()
            .await
            .map_err(|e| format!("Failed to kill process: {e}"))?;

        proc.status = ProcessStatus::Killed;
        Ok(())
    }

    /// List all managed processes.
    pub async fn list(&self) -> Vec<ProcessInfo> {
        let mut procs = self.processes.lock().await;
        let mut infos = Vec::new();

        for (id, proc) in procs.iter_mut() {
            // Reap exited processes while listing
            if matches!(proc.status, ProcessStatus::Running) {
                if let Ok(Some(exit_status)) = proc.child.try_wait() {
                    let code = exit_status.code().unwrap_or(-1);
                    proc.status = ProcessStatus::Exited { code };
                }
            }
            infos.push(ProcessInfo {
                id: id.clone(),
                command: proc.command.clone(),
                status: proc.status.clone(),
            });
        }

        infos.sort_by(|a, b| a.id.cmp(&b.id));
        infos
    }
}

// ── Tool implementations ─────────────────────────────────────────────────────

/// Start a long-running process.
pub struct ProcessStartTool {
    manager: ProcessManager,
}

impl ProcessStartTool {
    pub fn new(manager: ProcessManager) -> Self {
        Self { manager }
    }
}

#[async_trait::async_trait]
impl Tool for ProcessStartTool {
    fn name(&self) -> &str {
        "process_start"
    }

    fn description(&self) -> &str {
        "Start a long-running background process"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The executable to run"
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Arguments to pass to the command"
                },
                "working_dir": {
                    "type": "string",
                    "description": "Working directory (defaults to session working dir)"
                }
            },
            "required": ["command"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Confirm
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let command = match params.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: command")
            }
        };

        let args: Vec<&str> = params
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<&str>>())
            .unwrap_or_default();

        let working_dir = params
            .get("working_dir")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.working_dir.clone());

        match self.manager.spawn(command, &args, &working_dir).await {
            Ok(process_id) => ToolResult::json(serde_json::json!({
                "process_id": process_id,
                "command": command,
                "status": "running",
            })),
            Err(e) => ToolResult::error("SPAWN_FAILED", e),
        }
    }
}

/// Poll a managed process for status and recent output.
pub struct ProcessPollTool {
    manager: ProcessManager,
}

impl ProcessPollTool {
    pub fn new(manager: ProcessManager) -> Self {
        Self { manager }
    }
}

#[async_trait::async_trait]
impl Tool for ProcessPollTool {
    fn name(&self) -> &str {
        "process_poll"
    }

    fn description(&self) -> &str {
        "Check status and get recent output from a managed process"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "process_id": {
                    "type": "string",
                    "description": "The process ID returned by process_start"
                }
            },
            "required": ["process_id"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let process_id = match params.get("process_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return ToolResult::error(
                    "INVALID_PARAMS",
                    "Missing required parameter: process_id",
                )
            }
        };

        match self.manager.poll(process_id).await {
            Ok((status, stdout, stderr)) => ToolResult::json(serde_json::json!({
                "process_id": process_id,
                "status": status,
                "stdout": stdout,
                "stderr": stderr,
            })),
            Err(e) => ToolResult::error("POLL_FAILED", e),
        }
    }
}

/// Write to a managed process's stdin.
pub struct ProcessWriteTool {
    manager: ProcessManager,
}

impl ProcessWriteTool {
    pub fn new(manager: ProcessManager) -> Self {
        Self { manager }
    }
}

#[async_trait::async_trait]
impl Tool for ProcessWriteTool {
    fn name(&self) -> &str {
        "process_write"
    }

    fn description(&self) -> &str {
        "Write data to a managed process's stdin"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "process_id": {
                    "type": "string",
                    "description": "The process ID returned by process_start"
                },
                "data": {
                    "type": "string",
                    "description": "Data to write to the process's stdin"
                }
            },
            "required": ["process_id", "data"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Notify
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let process_id = match params.get("process_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return ToolResult::error(
                    "INVALID_PARAMS",
                    "Missing required parameter: process_id",
                )
            }
        };

        let data = match params.get("data").and_then(|v| v.as_str()) {
            Some(d) => d,
            None => return ToolResult::error("INVALID_PARAMS", "Missing required parameter: data"),
        };

        match self.manager.write_stdin(process_id, data).await {
            Ok(()) => ToolResult::json(serde_json::json!({
                "process_id": process_id,
                "bytes_written": data.len(),
            })),
            Err(e) => ToolResult::error("WRITE_FAILED", e),
        }
    }
}

/// Kill a managed process.
pub struct ProcessKillTool {
    manager: ProcessManager,
}

impl ProcessKillTool {
    pub fn new(manager: ProcessManager) -> Self {
        Self { manager }
    }
}

#[async_trait::async_trait]
impl Tool for ProcessKillTool {
    fn name(&self) -> &str {
        "process_kill"
    }

    fn description(&self) -> &str {
        "Kill a managed background process"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "process_id": {
                    "type": "string",
                    "description": "The process ID returned by process_start"
                }
            },
            "required": ["process_id"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Confirm
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let process_id = match params.get("process_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return ToolResult::error(
                    "INVALID_PARAMS",
                    "Missing required parameter: process_id",
                )
            }
        };

        match self.manager.kill(process_id).await {
            Ok(()) => ToolResult::json(serde_json::json!({
                "process_id": process_id,
                "status": "killed",
            })),
            Err(e) => ToolResult::error("KILL_FAILED", e),
        }
    }
}

/// List all managed processes.
pub struct ProcessListTool {
    manager: ProcessManager,
}

impl ProcessListTool {
    pub fn new(manager: ProcessManager) -> Self {
        Self { manager }
    }
}

#[async_trait::async_trait]
impl Tool for ProcessListTool {
    fn name(&self) -> &str {
        "process_list"
    }

    fn description(&self) -> &str {
        "List all managed background processes"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
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
        let processes = self.manager.list().await;
        ToolResult::json(serde_json::json!({
            "processes": processes,
            "count": processes.len(),
        }))
    }

    fn memory_hints(
        &self,
        _input: &serde_json::Value,
        _output: &serde_json::Value,
    ) -> Vec<MemoryHint> {
        vec![MemoryHint {
            entity: "system".to_string(),
            key: "processes".to_string(),
            volatility: std::time::Duration::from_secs(30),
            content: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> ToolContext {
        ToolContext::new("test", "test", std::env::temp_dir())
    }

    // ── Ring buffer tests ────────────────────────────────────────────────────

    #[test]
    fn test_ring_buffer_basic() {
        let mut buf = RingBuffer::new(16);
        buf.write(b"hello");
        assert_eq!(buf.read(), b"hello");
    }

    #[test]
    fn test_ring_buffer_wrap_around() {
        let mut buf = RingBuffer::new(8);
        buf.write(b"12345678"); // fills exactly
        buf.write(b"AB"); // wraps: overwrites positions 0,1
        let out = buf.read();
        // Buffer should contain the last 8 bytes: "345678AB"
        assert_eq!(String::from_utf8_lossy(&out), "345678AB");
    }

    #[test]
    fn test_ring_buffer_empty() {
        let buf = RingBuffer::new(16);
        assert!(buf.read().is_empty());
    }

    #[test]
    fn test_ring_buffer_overflow() {
        let mut buf = RingBuffer::new(4);
        buf.write(b"abcdefgh"); // 8 bytes into 4-byte buffer
        let out = buf.read();
        assert_eq!(String::from_utf8_lossy(&out), "efgh");
    }

    // ── ProcessManager tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_spawn_and_poll() {
        let mgr = ProcessManager::new();

        let (cmd, args) = if cfg!(target_os = "windows") {
            ("cmd", vec!["/C", "echo hello"])
        } else {
            ("echo", vec!["hello"])
        };

        let id = mgr.spawn(cmd, &args, &std::env::temp_dir()).await.unwrap();

        // Give the process time to finish and output to be captured
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let (status, stdout, _stderr) = mgr.poll(&id).await.unwrap();
        match status {
            ProcessStatus::Exited { code } => assert_eq!(code, 0),
            _ => panic!("Expected Exited status, got {status:?}"),
        }
        assert!(stdout.contains("hello"), "stdout was: {stdout:?}");
    }

    #[tokio::test]
    async fn test_spawn_and_kill() {
        let mgr = ProcessManager::new();

        // Start a long-running process
        let (cmd, args) = if cfg!(target_os = "windows") {
            ("cmd", vec!["/C", "ping -n 60 127.0.0.1"])
        } else {
            ("sleep", vec!["60"])
        };

        let id = mgr.spawn(cmd, &args, &std::env::temp_dir()).await.unwrap();

        // Verify it's running
        let (status, _, _) = mgr.poll(&id).await.unwrap();
        assert!(matches!(status, ProcessStatus::Running));

        // Kill it
        mgr.kill(&id).await.unwrap();

        // Verify it's killed
        let (status, _, _) = mgr.poll(&id).await.unwrap();
        assert!(matches!(status, ProcessStatus::Killed));
    }

    #[tokio::test]
    async fn test_kill_nonexistent() {
        let mgr = ProcessManager::new();
        let result = mgr.kill("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_processes() {
        let mgr = ProcessManager::new();

        // Initially empty
        let list = mgr.list().await;
        assert!(list.is_empty());

        // Spawn a process
        let (cmd, args) = if cfg!(target_os = "windows") {
            ("cmd", vec!["/C", "echo list_test"])
        } else {
            ("echo", vec!["list_test"])
        };

        let _id = mgr.spawn(cmd, &args, &std::env::temp_dir()).await.unwrap();

        let list = mgr.list().await;
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn test_poll_nonexistent() {
        let mgr = ProcessManager::new();
        let result = mgr.poll("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_write_stdin_nonexistent() {
        let mgr = ProcessManager::new();
        let result = mgr.write_stdin("nonexistent", "data").await;
        assert!(result.is_err());
    }

    // ── Tool trait tests ─────────────────────────────────────────────────────

    #[test]
    fn test_tool_tiers() {
        let mgr = ProcessManager::new();
        assert_eq!(
            ProcessStartTool::new(mgr.clone()).tier(),
            PermissionTier::Confirm
        );
        assert_eq!(
            ProcessPollTool::new(mgr.clone()).tier(),
            PermissionTier::Auto
        );
        assert_eq!(
            ProcessWriteTool::new(mgr.clone()).tier(),
            PermissionTier::Notify
        );
        assert_eq!(
            ProcessKillTool::new(mgr.clone()).tier(),
            PermissionTier::Confirm
        );
        assert_eq!(ProcessListTool::new(mgr).tier(), PermissionTier::Auto);
    }

    #[test]
    fn test_tool_names() {
        let mgr = ProcessManager::new();
        assert_eq!(ProcessStartTool::new(mgr.clone()).name(), "process_start");
        assert_eq!(ProcessPollTool::new(mgr.clone()).name(), "process_poll");
        assert_eq!(ProcessWriteTool::new(mgr.clone()).name(), "process_write");
        assert_eq!(ProcessKillTool::new(mgr.clone()).name(), "process_kill");
        assert_eq!(ProcessListTool::new(mgr).name(), "process_list");
    }

    #[tokio::test]
    async fn test_process_start_tool_missing_command() {
        let mgr = ProcessManager::new();
        let tool = ProcessStartTool::new(mgr);
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_process_poll_tool_missing_id() {
        let mgr = ProcessManager::new();
        let tool = ProcessPollTool::new(mgr);
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_process_kill_tool_missing_id() {
        let mgr = ProcessManager::new();
        let tool = ProcessKillTool::new(mgr);
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_process_write_tool_missing_params() {
        let mgr = ProcessManager::new();
        let tool = ProcessWriteTool::new(mgr);
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());

        let mgr2 = ProcessManager::new();
        let tool2 = ProcessWriteTool::new(mgr2);
        let result2 = tool2
            .execute(
                serde_json::json!({"process_id": "abc"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result2.is_error());
    }

    #[tokio::test]
    async fn test_process_list_tool_empty() {
        let mgr = ProcessManager::new();
        let tool = ProcessListTool::new(mgr);
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(!result.is_error());
        match &result {
            ToolResult::Json { content } => {
                assert_eq!(content["count"], 0);
            }
            _ => panic!("Expected Json result"),
        }
    }

    #[tokio::test]
    async fn test_process_start_tool_spawn() {
        let mgr = ProcessManager::new();
        let tool = ProcessStartTool::new(mgr);

        let (cmd, args_json) = if cfg!(target_os = "windows") {
            ("cmd", serde_json::json!(["/C", "echo tool_test"]))
        } else {
            ("echo", serde_json::json!(["tool_test"]))
        };

        let result = tool
            .execute(
                serde_json::json!({"command": cmd, "args": args_json}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
        match &result {
            ToolResult::Json { content } => {
                assert!(content["process_id"].is_string());
                assert_eq!(content["status"], "running");
            }
            _ => panic!("Expected Json result"),
        }
    }

    #[tokio::test]
    async fn test_process_manager_clone_shared_state() {
        let mgr = ProcessManager::new();
        let mgr2 = mgr.clone();

        let (cmd, args) = if cfg!(target_os = "windows") {
            ("cmd", vec!["/C", "echo shared"])
        } else {
            ("echo", vec!["shared"])
        };

        let id = mgr.spawn(cmd, &args, &std::env::temp_dir()).await.unwrap();

        // The clone should see the same process
        let list = mgr2.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
    }

    #[test]
    fn test_process_manager_default() {
        let mgr = ProcessManager::default();
        // Just verify it constructs without panic
        let _ = mgr;
    }

    #[test]
    fn test_process_status_serialization() {
        let running = ProcessStatus::Running;
        let json = serde_json::to_value(&running).unwrap();
        assert_eq!(json["status"], "running");

        let exited = ProcessStatus::Exited { code: 42 };
        let json = serde_json::to_value(&exited).unwrap();
        assert_eq!(json["status"], "exited");
        assert_eq!(json["code"], 42);

        let killed = ProcessStatus::Killed;
        let json = serde_json::to_value(&killed).unwrap();
        assert_eq!(json["status"], "killed");
    }
}
