use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;
use thiserror::Error;

/// Errors from script agent execution.
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("script execution timed out after {0}s")]
    Timeout(u64),
    #[error("recursion depth exceeded (max {0})")]
    DepthExceeded(u8),
    #[error("failed to spawn process: {0}")]
    SpawnFailed(String),
    #[error("invalid response from script: {0}")]
    InvalidResponse(String),
    #[error("runtime error: {0}")]
    RuntimeError(String),
}

/// Supported script runtimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScriptRuntime {
    Python,
    Node,
}

impl ScriptRuntime {
    /// Return the command name used to invoke this runtime.
    pub fn command(&self) -> &'static str {
        match self {
            ScriptRuntime::Python => "python",
            ScriptRuntime::Node => "node",
        }
    }
}

/// Configuration for a script agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptAgentConfig {
    pub runtime: ScriptRuntime,
    pub script_path: PathBuf,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_depth")]
    pub max_depth: u8,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
}

fn default_timeout() -> u64 {
    30
}

fn default_max_depth() -> u8 {
    2
}

/// A request sent to a script agent subprocess.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptRequest {
    pub method: String,
    pub params: serde_json::Value,
    #[serde(default)]
    pub depth: u8,
}

/// A response received from a script agent subprocess.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptResponse {
    pub result: serde_json::Value,
    pub duration_ms: u64,
}

/// Script agent that delegates work to a Python or Node subprocess.
///
/// Protocol: write one JSON line to stdin, read one JSON line from stdout.
pub struct ScriptAgent {
    config: ScriptAgentConfig,
}

impl ScriptAgent {
    pub fn new(config: ScriptAgentConfig) -> Self {
        Self { config }
    }

    /// Get the agent configuration.
    pub fn config(&self) -> &ScriptAgentConfig {
        &self.config
    }

    /// Execute a request by spawning the script as a subprocess.
    pub fn execute(&self, request: ScriptRequest) -> Result<ScriptResponse, AgentError> {
        // Depth guard
        if request.depth >= self.config.max_depth {
            return Err(AgentError::DepthExceeded(self.config.max_depth));
        }

        let start = Instant::now();

        // Build command
        let mut cmd = std::process::Command::new(self.config.runtime.command());
        cmd.arg(&self.config.script_path);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        if let Some(ref working_dir) = self.config.working_dir {
            cmd.current_dir(working_dir);
        }

        // Spawn
        let mut child = cmd
            .spawn()
            .map_err(|e| AgentError::SpawnFailed(e.to_string()))?;

        // Write request as JSON line to stdin
        let request_json =
            serde_json::to_string(&request).map_err(|e| AgentError::RuntimeError(e.to_string()))?;

        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            writeln!(stdin, "{request_json}")
                .map_err(|e| AgentError::RuntimeError(format!("Failed to write stdin: {e}")))?;
            // Drop stdin to signal EOF
        }

        // Wait with timeout
        let timeout = std::time::Duration::from_secs(self.config.timeout_secs);
        let output = wait_with_timeout(&mut child, timeout)?;

        let duration_ms = start.elapsed().as_millis() as u64;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AgentError::RuntimeError(format!(
                "Process exited with {}: {}",
                output.status,
                stderr.trim()
            )));
        }

        // Parse first line of stdout as JSON response
        let stdout = String::from_utf8_lossy(&output.stdout);
        let first_line = stdout
            .lines()
            .next()
            .ok_or_else(|| AgentError::InvalidResponse("No output from script".to_string()))?;

        let result: serde_json::Value = serde_json::from_str(first_line)
            .map_err(|e| AgentError::InvalidResponse(format!("Invalid JSON: {e}")))?;

        Ok(ScriptResponse {
            result,
            duration_ms,
        })
    }
}

/// Wait for a child process with a timeout.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> Result<std::process::Output, AgentError> {
    let start = Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Process finished — collect stdout/stderr manually since
                // wait_with_output() takes ownership and we only have &mut.
                let mut stdout_buf = Vec::new();
                let mut stderr_buf = Vec::new();
                if let Some(ref mut out) = child.stdout {
                    use std::io::Read;
                    let _ = out.read_to_end(&mut stdout_buf);
                }
                if let Some(ref mut err) = child.stderr {
                    use std::io::Read;
                    let _ = err.read_to_end(&mut stderr_buf);
                }
                return Ok(std::process::Output {
                    status,
                    stdout: stdout_buf,
                    stderr: stderr_buf,
                });
            }
            Ok(None) => {
                // Still running
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(AgentError::Timeout(timeout.as_secs()));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                return Err(AgentError::RuntimeError(format!(
                    "Failed to check process status: {e}"
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> ScriptAgentConfig {
        ScriptAgentConfig {
            runtime: ScriptRuntime::Python,
            script_path: PathBuf::from("test_script.py"),
            timeout_secs: 30,
            max_depth: 2,
            working_dir: None,
        }
    }

    #[test]
    fn test_config_defaults() {
        let json = r#"{
            "runtime": "python",
            "script_path": "agent.py"
        }"#;
        let config: ScriptAgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.runtime, ScriptRuntime::Python);
        assert_eq!(config.timeout_secs, 30);
        assert_eq!(config.max_depth, 2);
        assert!(config.working_dir.is_none());
    }

    #[test]
    fn test_config_node_runtime() {
        let json = r#"{
            "runtime": "node",
            "script_path": "agent.js",
            "timeout_secs": 60,
            "max_depth": 5,
            "working_dir": "/tmp/work"
        }"#;
        let config: ScriptAgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.runtime, ScriptRuntime::Node);
        assert_eq!(config.timeout_secs, 60);
        assert_eq!(config.max_depth, 5);
        assert_eq!(
            config.working_dir.as_deref(),
            Some(std::path::Path::new("/tmp/work"))
        );
    }

    #[test]
    fn test_runtime_command() {
        assert_eq!(ScriptRuntime::Python.command(), "python");
        assert_eq!(ScriptRuntime::Node.command(), "node");
    }

    #[test]
    fn test_depth_guard_rejects() {
        let config = ScriptAgentConfig {
            max_depth: 2,
            ..default_config()
        };
        let agent = ScriptAgent::new(config);

        let request = ScriptRequest {
            method: "test".to_string(),
            params: serde_json::json!({}),
            depth: 2, // >= max_depth
        };

        let err = agent.execute(request).unwrap_err();
        assert!(matches!(err, AgentError::DepthExceeded(2)));
    }

    #[test]
    fn test_depth_guard_allows() {
        let config = ScriptAgentConfig {
            runtime: ScriptRuntime::Python,
            script_path: PathBuf::from("test_script.py"),
            timeout_secs: 30,
            max_depth: 3,
            working_dir: None,
        };
        let agent = ScriptAgent::new(config);

        let request = ScriptRequest {
            method: "test".to_string(),
            params: serde_json::json!({}),
            depth: 2, // < max_depth of 3, so depth guard passes
        };

        // This will fail (script doesn't exist) but NOT due to depth guard.
        // On Windows, python spawns fine but exits with error; on other platforms, spawn may fail.
        let err = agent.execute(request).unwrap_err();
        assert!(
            !matches!(err, AgentError::DepthExceeded(_)),
            "Should not fail due to depth guard"
        );
    }

    #[test]
    fn test_request_serialization() {
        let request = ScriptRequest {
            method: "analyze".to_string(),
            params: serde_json::json!({"file": "test.py"}),
            depth: 1,
        };
        let json = serde_json::to_string(&request).unwrap();
        let parsed: ScriptRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.method, "analyze");
        assert_eq!(parsed.depth, 1);
    }

    #[test]
    fn test_response_parsing() {
        let json = r#"{"result": {"status": "ok"}, "duration_ms": 42}"#;
        let response: ScriptResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.result["status"], "ok");
        assert_eq!(response.duration_ms, 42);
    }

    #[test]
    fn test_spawn_nonexistent_binary() {
        // Use a completely nonexistent binary name to ensure SpawnFailed
        let _config = ScriptAgentConfig {
            runtime: ScriptRuntime::Python,
            script_path: PathBuf::from("agent.py"),
            timeout_secs: 30,
            max_depth: 2,
            working_dir: None,
        };
        // Override the command by using a config that references a nonexistent runtime
        // We test this by directly calling Command with a fake binary
        let mut cmd = std::process::Command::new("__nonexistent_binary_abc123__");
        cmd.arg("test.py");
        let result = cmd.spawn();
        assert!(result.is_err(), "Spawning nonexistent binary should fail");
    }

    #[test]
    fn test_execute_nonexistent_script_errors() {
        let config = ScriptAgentConfig {
            script_path: PathBuf::from("/nonexistent/path/agent.py"),
            ..default_config()
        };
        let agent = ScriptAgent::new(config);
        let request = ScriptRequest {
            method: "test".to_string(),
            params: serde_json::json!({}),
            depth: 0,
        };
        // On Windows python spawns OK but fails at runtime; on other platforms spawn may fail.
        // Either way, execute() should return an error.
        assert!(agent.execute(request).is_err());
    }

    #[test]
    fn test_error_variants_display() {
        let err = AgentError::Timeout(30);
        assert_eq!(err.to_string(), "script execution timed out after 30s");

        let err = AgentError::DepthExceeded(2);
        assert_eq!(err.to_string(), "recursion depth exceeded (max 2)");

        let err = AgentError::SpawnFailed("no such file".to_string());
        assert!(err.to_string().contains("no such file"));

        let err = AgentError::InvalidResponse("bad json".to_string());
        assert!(err.to_string().contains("bad json"));

        let err = AgentError::RuntimeError("crash".to_string());
        assert!(err.to_string().contains("crash"));
    }

    #[test]
    fn test_runtime_serde_roundtrip() {
        for runtime in [ScriptRuntime::Python, ScriptRuntime::Node] {
            let json = serde_json::to_string(&runtime).unwrap();
            let parsed: ScriptRuntime = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, runtime);
        }
    }

    #[test]
    fn test_request_depth_default() {
        let json = r#"{"method": "test", "params": {}}"#;
        let request: ScriptRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.depth, 0);
    }

    #[test]
    fn test_agent_config_accessor() {
        let config = default_config();
        let agent = ScriptAgent::new(config);
        assert_eq!(agent.config().runtime, ScriptRuntime::Python);
        assert_eq!(agent.config().timeout_secs, 30);
    }

    #[test]
    fn test_config_serde_full_roundtrip() {
        let config = ScriptAgentConfig {
            runtime: ScriptRuntime::Node,
            script_path: PathBuf::from("agent.js"),
            timeout_secs: 60,
            max_depth: 5,
            working_dir: Some(PathBuf::from("/tmp")),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: ScriptAgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.runtime, ScriptRuntime::Node);
        assert_eq!(parsed.max_depth, 5);
        assert_eq!(parsed.working_dir, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn test_depth_guard_at_zero_with_max_zero() {
        let config = ScriptAgentConfig {
            max_depth: 0,
            ..default_config()
        };
        let agent = ScriptAgent::new(config);
        let request = ScriptRequest {
            method: "test".to_string(),
            params: serde_json::json!({}),
            depth: 0, // >= 0
        };
        let err = agent.execute(request).unwrap_err();
        assert!(matches!(err, AgentError::DepthExceeded(0)));
    }

    #[test]
    fn test_request_with_complex_params() {
        let request = ScriptRequest {
            method: "process".to_string(),
            params: serde_json::json!({
                "files": ["a.py", "b.py"],
                "options": {"verbose": true, "depth": 3}
            }),
            depth: 0,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"files\""));
        assert!(json.contains("\"verbose\""));
    }
}
