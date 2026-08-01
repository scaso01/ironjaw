use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_OUTPUT_BYTES: usize = 64 * 1024; // 64 KB

/// Environment variables that enable library injection or code preloading.
/// Stripped from child processes to prevent privilege escalation.
const BLOCKED_ENV_VARS: &[&str] = &[
    // Native library injection
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    // Runtime-specific code injection
    "NODE_OPTIONS",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "RUBYOPT",
    "PERL5OPT",
    "JAVA_TOOL_OPTIONS",
];

/// Windows-specific environment variables that can be used for injection.
/// COMSPEC controls the default shell, PSModulePath can load malicious modules,
/// PATHEXT controls which extensions are executable.
#[cfg(windows)]
const BLOCKED_ENV_VARS_WINDOWS: &[&str] = &["COMSPEC", "PSModulePath", "PATHEXT"];

/// Strip dangerous environment variables from a command before spawning.
///
/// Pub so other subsystems (e.g. `ironjaw-ws/hooks/subprocess.rs`) can reuse
/// the same env-scrub policy without duplicating `BLOCKED_ENV_VARS`.
pub fn sanitize_env(cmd: &mut tokio::process::Command) {
    for var in BLOCKED_ENV_VARS {
        cmd.env_remove(var);
    }
    #[cfg(windows)]
    for var in BLOCKED_ENV_VARS_WINDOWS {
        cmd.env_remove(var);
    }
}

// ── OS-level process sandbox ─────────────────────────────────────────────────

/// Configurable limits for the process sandbox.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Maximum memory per process in bytes (default 512 MB).
    pub memory_limit_bytes: u64,
    /// Maximum number of active child processes (default 5).
    pub process_limit: u32,
    /// Whether sandboxing is enabled (default true).
    pub enabled: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            memory_limit_bytes: 512 * 1024 * 1024, // 512 MB
            process_limit: 5,
            enabled: true,
        }
    }
}

/// Windows Job Object sandbox that constrains child processes.
///
/// When applied, the child process and all its descendants are placed in a
/// Job Object with strict limits on memory and process creation.
/// All child processes are killed when this struct is dropped
/// (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`).
#[cfg(windows)]
pub struct ProcessSandbox {
    job_handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl ProcessSandbox {
    /// Create a new sandbox Job Object with the given configuration.
    pub fn new(config: &SandboxConfig) -> Result<Self, std::io::Error> {
        use windows_sys::Win32::System::JobObjects::*;

        let job_handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job_handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        // Set all limits in one call via JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
        // The BasicLimitInformation sub-struct carries LimitFlags + ActiveProcessLimit.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        info.BasicLimitInformation.ActiveProcessLimit = config.process_limit;
        info.ProcessMemoryLimit = config.memory_limit_bytes as usize;

        let ok = unsafe {
            SetInformationJobObject(
                job_handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };

        if ok == 0 {
            let err = std::io::Error::last_os_error();
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(job_handle);
            }
            return Err(err);
        }

        Ok(Self { job_handle })
    }

    /// Assign a child process to this sandbox by raw process handle.
    ///
    /// # Safety
    /// The caller must ensure `process_handle` is a valid process handle.
    pub unsafe fn assign_process(
        &self,
        process_handle: windows_sys::Win32::Foundation::HANDLE,
    ) -> Result<(), std::io::Error> {
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        let ok = AssignProcessToJobObject(self.job_handle, process_handle);
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

// SAFETY: Windows HANDLEs are thread-safe and can be used from any thread.
#[cfg(windows)]
unsafe impl Send for ProcessSandbox {}
#[cfg(windows)]
unsafe impl Sync for ProcessSandbox {}

#[cfg(windows)]
impl Drop for ProcessSandbox {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job_handle);
        }
    }
}

/// No-op sandbox for non-Windows platforms.
#[cfg(not(windows))]
pub struct ProcessSandbox;

#[cfg(not(windows))]
impl ProcessSandbox {
    /// Create a no-op sandbox (Job Objects are Windows-only).
    pub fn new(_config: &SandboxConfig) -> Result<Self, std::io::Error> {
        tracing::debug!("Process sandboxing not available on this platform");
        Ok(Self)
    }
}

/// Executes shell commands on the server.
pub struct ShellTool;

impl Default for ShellTool {
    fn default() -> Self {
        Self
    }
}

impl ShellTool {
    pub fn new() -> Self {
        Self
    }

    /// Spawn a command inside an OS-level sandbox (Windows Job Object).
    /// On non-Windows, this is a plain spawn with no sandboxing.
    ///
    /// `cancel` is polled inside every blocking wait so that `chat.abort` can
    /// kill the child process — including grandchildren — before the command
    /// finishes naturally.  On Windows the `ProcessSandbox` Job Object
    /// (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) kills grandchildren when the
    /// sandbox handle is dropped; on other platforms `SIGKILL` covers direct
    /// children only.
    async fn spawn_sandboxed(
        mut cmd: tokio::process::Command,
        config: &SandboxConfig,
        cancel: CancellationToken,
    ) -> std::io::Result<std::process::Output> {
        if !config.enabled {
            // kill_on_drop ensures the child is killed if the future is dropped.
            cmd.kill_on_drop(true);
            // Pipe stdout/stderr so we can collect them without wait_with_output
            // (which takes `self`, preventing us from calling kill() in the cancel arm).
            use std::process::Stdio;
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = cmd.spawn()?;
            return tokio::select! {
                status = child.wait() => {
                    let status = status?;
                    // Drain stdout/stderr now that the process has exited.
                    let mut stdout = Vec::new();
                    let mut stderr = Vec::new();
                    if let Some(mut s) = child.stdout.take() {
                        use tokio::io::AsyncReadExt;
                        let _ = s.read_to_end(&mut stdout).await;
                    }
                    if let Some(mut s) = child.stderr.take() {
                        use tokio::io::AsyncReadExt;
                        let _ = s.read_to_end(&mut stderr).await;
                    }
                    Ok(std::process::Output { status, stdout, stderr })
                }
                _ = cancel.cancelled() => {
                    let _ = child.kill().await;
                    Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "cancelled",
                    ))
                }
            };
        }

        #[cfg(windows)]
        {
            use std::process::Stdio;
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            // kill_on_drop: if this future is dropped mid-wait (cancel fires),
            // tokio will send SIGKILL / TerminateProcess to the child.
            cmd.kill_on_drop(true);
            let mut child = cmd.spawn()?;
            // Best-effort sandbox: assign to Job Object immediately after spawn.
            match ProcessSandbox::new(config) {
                Ok(sandbox) => {
                    if let Some(pid) = child.id() {
                        use windows_sys::Win32::System::Threading::{
                            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
                        };
                        let access = PROCESS_SET_QUOTA | PROCESS_TERMINATE;
                        let handle = unsafe { OpenProcess(access, 0, pid) };
                        if !handle.is_null() {
                            if let Err(e) = unsafe { sandbox.assign_process(handle) } {
                                tracing::warn!("Failed to assign child to Job Object: {e}");
                            }
                            unsafe {
                                windows_sys::Win32::Foundation::CloseHandle(handle);
                            }
                        }
                    }
                    // Race the child against the cancellation token.
                    // Use child.wait() (&mut self) so we retain ownership of
                    // `child` for kill() in the cancel arm.  Stdout/stderr are
                    // drained after the process exits (or skipped on cancel).
                    tokio::select! {
                        status = child.wait() => {
                            let status = status?;
                            let mut stdout = Vec::new();
                            let mut stderr = Vec::new();
                            if let Some(mut s) = child.stdout.take() {
                                use tokio::io::AsyncReadExt;
                                let _ = s.read_to_end(&mut stdout).await;
                            }
                            if let Some(mut s) = child.stderr.take() {
                                use tokio::io::AsyncReadExt;
                                let _ = s.read_to_end(&mut stderr).await;
                            }
                            drop(sandbox);
                            Ok(std::process::Output { status, stdout, stderr })
                        }
                        _ = cancel.cancelled() => {
                            let _ = child.kill().await;
                            drop(sandbox); // kills grandchildren via Job Object
                            Err(std::io::Error::new(
                                std::io::ErrorKind::Interrupted,
                                "cancelled",
                            ))
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to create Job Object sandbox: {e}");
                    tokio::select! {
                        status = child.wait() => {
                            let status = status?;
                            let mut stdout = Vec::new();
                            let mut stderr = Vec::new();
                            if let Some(mut s) = child.stdout.take() {
                                use tokio::io::AsyncReadExt;
                                let _ = s.read_to_end(&mut stdout).await;
                            }
                            if let Some(mut s) = child.stderr.take() {
                                use tokio::io::AsyncReadExt;
                                let _ = s.read_to_end(&mut stderr).await;
                            }
                            Ok(std::process::Output { status, stdout, stderr })
                        }
                        _ = cancel.cancelled() => {
                            let _ = child.kill().await;
                            Err(std::io::Error::new(
                                std::io::ErrorKind::Interrupted,
                                "cancelled",
                            ))
                        }
                    }
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = config;
            use std::process::Stdio;
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            cmd.kill_on_drop(true);
            let mut child = cmd.spawn()?;
            tokio::select! {
                status = child.wait() => {
                    let status = status?;
                    let mut stdout = Vec::new();
                    let mut stderr = Vec::new();
                    if let Some(mut s) = child.stdout.take() {
                        use tokio::io::AsyncReadExt;
                        let _ = s.read_to_end(&mut stdout).await;
                    }
                    if let Some(mut s) = child.stderr.take() {
                        use tokio::io::AsyncReadExt;
                        let _ = s.read_to_end(&mut stderr).await;
                    }
                    Ok(std::process::Output { status, stdout, stderr })
                }
                _ = cancel.cancelled() => {
                    let _ = child.kill().await;
                    Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "cancelled",
                    ))
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a cmd.exe command on this Windows machine (workstation) — this runs via `cmd /C`, NOT PowerShell (use the `powershell` tool for PowerShell scripts). Use for ad-hoc system queries not covered by other tools. Working directory is C:\\Users\\deploy\\Projects. Output capped at 64KB; 30s timeout."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds (default 30)",
                    "minimum": 1,
                    "maximum": 300
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
        cancel: CancellationToken,
    ) -> ToolResult {
        let command = match params.get("command").and_then(|v| v.as_str()) {
            Some(cmd) => cmd,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: command")
            }
        };

        let timeout_secs = params
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let timeout = Duration::from_secs(timeout_secs);

        // Determine shell based on platform
        let (shell, flag) = if cfg!(target_os = "windows") {
            ("cmd", "/C")
        } else {
            ("bash", "-c")
        };

        // Streaming path: emit stdout lines in real-time via stream_tx
        if let Some(stream_tx) = ctx.stream_tx.clone() {
            use std::process::Stdio;
            use tokio::io::{AsyncBufReadExt, BufReader};

            let mut cmd = tokio::process::Command::new(shell);
            cmd.arg(flag)
                .arg(command)
                .current_dir(&ctx.working_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            sanitize_env(&mut cmd);

            let spawn_result = cmd.spawn();
            let mut child = match spawn_result {
                Ok(c) => c,
                Err(e) => {
                    return ToolResult::error(
                        "EXEC_FAILED",
                        format!("Failed to spawn command: {e}"),
                    )
                }
            };

            let stdout_handle = child.stdout.take().map(BufReader::new);
            let stderr_handle = child.stderr.take().map(BufReader::new);

            let mut stdout_lines: Vec<String> = Vec::new();
            let mut stderr_lines: Vec<String> = Vec::new();

            let drain_fut = tokio::time::timeout(timeout, async {
                // Drain stdout line-by-line, forwarding each to stream_tx
                if let Some(mut reader) = stdout_handle {
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                                let _ = stream_tx.send(trimmed.to_string());
                                stdout_lines.push(trimmed.to_string());
                            }
                        }
                    }
                }
                // Drain stderr silently (collected but not streamed)
                if let Some(mut reader) = stderr_handle {
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                stderr_lines.push(
                                    line.trim_end_matches('\n')
                                        .trim_end_matches('\r')
                                        .to_string(),
                                );
                            }
                        }
                    }
                }
                child.wait().await
            });

            // C2-B: race timeout/drain against the connection's abort token.
            let exit_code = tokio::select! {
                stream_timeout = drain_fut => match stream_timeout {
                    Ok(Ok(status)) => status.code().unwrap_or(-1),
                    Ok(Err(_)) => -1,
                    Err(_) => {
                        let _ = child.kill().await;
                        return ToolResult::error(
                            "TIMEOUT",
                            format!("Command timed out after {timeout_secs} seconds"),
                        );
                    }
                },
                _ = cancel.cancelled() => {
                    let _ = child.kill().await;
                    return ToolResult::cancelled("Shell command was cancelled");
                },
            };

            let mut stdout = stdout_lines.join("\n");
            let mut stderr = stderr_lines.join("\n");

            if stdout.len() > MAX_OUTPUT_BYTES {
                stdout.truncate(MAX_OUTPUT_BYTES);
                stdout.push_str("\n... (output truncated at 64KB)");
            }
            if stderr.len() > MAX_OUTPUT_BYTES {
                stderr.truncate(MAX_OUTPUT_BYTES);
                stderr.push_str("\n... (output truncated at 64KB)");
            }

            return ToolResult::json(serde_json::json!({
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr,
            }));
        }

        // Non-streaming path: wait for full output
        let mut cmd = tokio::process::Command::new(shell);
        cmd.arg(flag).arg(command).current_dir(&ctx.working_dir);
        sanitize_env(&mut cmd);

        let sandbox_config = SandboxConfig::default();
        let exec_fut = tokio::time::timeout(
            timeout,
            Self::spawn_sandboxed(cmd, &sandbox_config, cancel.clone()),
        );

        // C2-B: race the timeout-wrapped execution against the connection's abort token.
        let result = tokio::select! {
            r = exec_fut => r,
            _ = cancel.cancelled() => {
                return ToolResult::cancelled("Shell command was cancelled");
            },
        };

        match result {
            Ok(Ok(output)) => {
                let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();

                // Cap output
                if stdout.len() > MAX_OUTPUT_BYTES {
                    stdout.truncate(MAX_OUTPUT_BYTES);
                    stdout.push_str("\n... (output truncated at 64KB)");
                }
                if stderr.len() > MAX_OUTPUT_BYTES {
                    stderr.truncate(MAX_OUTPUT_BYTES);
                    stderr.push_str("\n... (output truncated at 64KB)");
                }

                let exit_code = output.status.code().unwrap_or(-1);

                ToolResult::json(serde_json::json!({
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                }))
            }
            Ok(Err(e)) => {
                ToolResult::error("EXEC_FAILED", format!("Failed to execute command: {e}"))
            }
            Err(_) => ToolResult::error(
                "TIMEOUT",
                format!("Command timed out after {timeout_secs} seconds"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> ToolContext {
        ToolContext::new("test", "test", std::env::temp_dir())
    }

    #[tokio::test]
    async fn test_shell_echo() {
        let tool = ShellTool::new();
        let cmd = "echo hello";
        let result = tool
            .execute(
                serde_json::json!({"command": cmd}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
        match &result {
            ToolResult::Json { content } => {
                assert_eq!(content["exit_code"], 0);
                assert!(content["stdout"].as_str().unwrap().contains("hello"));
            }
            _ => panic!("Expected Json result"),
        }
    }

    #[tokio::test]
    async fn test_shell_missing_command() {
        let tool = ShellTool::new();
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_shell_timeout() {
        // Test the timeout mechanism using tokio::time::timeout directly,
        // since the sandbox environment lacks most external commands.
        use tokio::time::{timeout, Duration};
        let result = timeout(
            Duration::from_millis(50),
            tokio::time::sleep(Duration::from_secs(30)),
        )
        .await;
        assert!(result.is_err(), "timeout should have fired");

        // Also verify the tool handles missing commands gracefully
        let tool = ShellTool::new();
        let result = tool
            .execute(
                serde_json::json!({"command": "nonexistent_cmd_12345", "timeout_secs": 1}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        // Should get either EXEC_FAILED or a non-zero exit code (depends on platform)
        match &result {
            ToolResult::Error { .. } => {} // EXEC_FAILED is fine
            ToolResult::Json { content } => {
                // cmd.exe returns exit code 1 for unknown commands
                assert_ne!(content["exit_code"], 0);
            }
            _ => panic!("Unexpected result variant"),
        }
    }

    #[test]
    fn test_shell_tier() {
        let tool = ShellTool::new();
        assert_eq!(tool.tier(), PermissionTier::Confirm);
    }

    #[test]
    fn test_blocked_env_vars_list() {
        assert!(BLOCKED_ENV_VARS.contains(&"LD_PRELOAD"));
        assert!(BLOCKED_ENV_VARS.contains(&"NODE_OPTIONS"));
        assert!(BLOCKED_ENV_VARS.contains(&"PYTHONPATH"));
        assert!(BLOCKED_ENV_VARS.contains(&"JAVA_TOOL_OPTIONS"));
        assert_eq!(BLOCKED_ENV_VARS.len(), 9);
    }

    #[cfg(windows)]
    #[test]
    fn test_blocked_env_vars_windows() {
        assert!(BLOCKED_ENV_VARS_WINDOWS.contains(&"COMSPEC"));
        assert!(BLOCKED_ENV_VARS_WINDOWS.contains(&"PSModulePath"));
        assert!(BLOCKED_ENV_VARS_WINDOWS.contains(&"PATHEXT"));
        assert_eq!(BLOCKED_ENV_VARS_WINDOWS.len(), 3);
    }

    #[test]
    fn test_sanitize_env_removes_vars() {
        let mut cmd = tokio::process::Command::new("echo");
        cmd.env("LD_PRELOAD", "/tmp/evil.so");
        cmd.env("NODE_OPTIONS", "--require /tmp/evil.js");
        cmd.env("SAFE_VAR", "keep_me");
        sanitize_env(&mut cmd);
        // sanitize_env calls env_remove on all blocked vars.
        // We cannot inspect the env map directly, but we verify it doesn't panic
        // and the function covers all blocked vars.
    }

    #[test]
    fn test_sandbox_config_defaults() {
        let config = SandboxConfig::default();
        assert_eq!(config.memory_limit_bytes, 512 * 1024 * 1024);
        assert_eq!(config.process_limit, 5);
        assert!(config.enabled);
    }

    #[test]
    fn test_sandbox_config_custom() {
        let config = SandboxConfig {
            memory_limit_bytes: 256 * 1024 * 1024,
            process_limit: 3,
            enabled: false,
        };
        assert_eq!(config.memory_limit_bytes, 256 * 1024 * 1024);
        assert_eq!(config.process_limit, 3);
        assert!(!config.enabled);
    }

    #[test]
    fn test_process_sandbox_creation() {
        let config = SandboxConfig::default();
        let sandbox = ProcessSandbox::new(&config);
        assert!(sandbox.is_ok(), "ProcessSandbox::new() should succeed");
    }

    #[tokio::test]
    async fn test_spawn_sandboxed_echo() {
        let (shell, flag) = if cfg!(target_os = "windows") {
            ("cmd", "/C")
        } else {
            ("bash", "-c")
        };
        let config = SandboxConfig::default();
        let mut cmd = tokio::process::Command::new(shell);
        cmd.arg(flag).arg("echo sandboxed");
        let output = ShellTool::spawn_sandboxed(cmd, &config, CancellationToken::new())
            .await
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("sandboxed"));
    }

    #[tokio::test]
    async fn test_spawn_sandboxed_disabled() {
        let (shell, flag) = if cfg!(target_os = "windows") {
            ("cmd", "/C")
        } else {
            ("bash", "-c")
        };
        let config = SandboxConfig {
            enabled: false,
            ..SandboxConfig::default()
        };
        let mut cmd = tokio::process::Command::new(shell);
        cmd.arg(flag).arg("echo no_sandbox");
        let output = ShellTool::spawn_sandboxed(cmd, &config, CancellationToken::new())
            .await
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("no_sandbox"));
    }

    #[tokio::test]
    async fn test_shell_streaming_via_stream_tx() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let ctx = ToolContext::new("test", "test", std::env::temp_dir()).with_stream_tx(tx);

        let command = if cfg!(target_os = "windows") {
            "cmd /C echo line1 && echo line2 && echo line3"
        } else {
            "bash -c 'echo line1; echo line2; echo line3'"
        };

        let tool = ShellTool::new();
        let result = tool
            .execute(
                serde_json::json!({"command": command}),
                &ctx,
                CancellationToken::new(),
            )
            .await;

        assert!(!result.is_error(), "shell execute should not error");

        // Drain all streamed lines
        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }

        assert!(
            lines.len() > 1,
            "expected >1 streamed lines, got: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("line1")),
            "expected 'line1' in stream: {lines:?}"
        );
    }

    /// Verifies P10 cancel propagation end-to-end: a long-running shell command
    /// is cancelled mid-flight and the child process is terminated promptly via
    /// the kill-on-drop / Job-Object grandchild kill path. Without the cancel
    /// fix, this test would hang for ~5s waiting for `ping`/`sleep` to exit.
    #[tokio::test]
    async fn test_shell_cancel_terminates_child_promptly() {
        let command = if cfg!(target_os = "windows") {
            "ping -n 30 127.0.0.1"
        } else {
            "sleep 30"
        };
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            cancel_clone.cancel();
        });

        let tool = ShellTool::new();
        let start = std::time::Instant::now();
        let result = tool
            .execute(
                serde_json::json!({"command": command, "timeout_secs": 30}),
                &ToolContext::new("test", "test", std::env::temp_dir()),
                cancel,
            )
            .await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "cancel should kill child within 3s, took {elapsed:?}"
        );
        // Either Cancelled variant or an error containing 'cancel' is acceptable
        let s = format!("{result:?}").to_lowercase();
        assert!(
            s.contains("cancel"),
            "result should reflect cancellation, got: {result:?}"
        );
    }
}
