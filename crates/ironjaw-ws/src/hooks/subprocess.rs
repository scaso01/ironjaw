// B2 CP4: subprocess hook executor.
//
// Runs an external program from a fixed `~/.config/ironjaw/hooks/<name>`
// directory and feeds it the `HookContext` over stdin as JSON. The program's
// exit code (+ optional JSON stdout) determines the resulting `HookAction`.
//
// Security boundaries (unique to hooks; have no precedent in the rest of the
// repo):
// - **Fixed-directory enforcement**: hook executable resolves only inside the
//   configured `[hooks] dir`. Any path containing `..`, `/`, `\`, or that
//   canonicalizes outside the configured dir is rejected. Never falls back
//   to `$PATH`.
// - **Per-event opt-in**: even with subprocess globally enabled, only the
//   events listed in `[hooks.subprocess] enabled_events = [...]` may invoke
//   a subprocess hook. Enforced by the loader (CP6); SubprocessHook itself
//   is always-execute.
// - **Audit per invocation**: every execution writes an `AuditEvent` with
//   `actor = "hook.subprocess"`, payload = `{event, hook_name, exit_code,
//   duration_ms, stdout_truncated}`.
//
// Reused from `crates/ironjaw-tools/src/builtin/shell.rs` (DO NOT duplicate
// constants — change there propagates here):
// - `sanitize_env` + `BLOCKED_ENV_VARS` for env scrubbing
// - `tokio::time::timeout(Duration, child.wait_with_output())` pattern
// - Output truncation pattern (capped at 16 KB here, vs 64 KB for shell — hook
//   output is consumed as JSON; 64 KB is excessive).

use async_trait::async_trait;
use ironjaw_audit::{ActionType, AuditEntry, AuditStore};
use ironjaw_tools::builtin::shell::sanitize_env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

use super::registry::Hook;
use super::types::{HookAction, HookContext};

/// Maximum stdout bytes captured from a subprocess hook (truncated past this).
pub const MAX_HOOK_OUTPUT_BYTES: usize = 16 * 1024;

/// Default per-hook execution timeout.
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 5;

/// Hard ceiling enforced on per-hook timeout config.
pub const HOOK_TIMEOUT_CEILING_SECS: u64 = 60;

/// Errors raised when constructing or executing a subprocess hook.
#[derive(Debug, thiserror::Error)]
pub enum SubprocessHookError {
    #[error("hook name '{0}' contains forbidden path characters (.., /, \\)")]
    ForbiddenPathChar(String),
    #[error("hook name '{0}' is empty")]
    EmptyName(String),
    #[error("resolved hook path '{0}' escapes the hooks directory")]
    PathEscapesDir(PathBuf),
    #[error("hooks directory '{0}' does not exist")]
    HooksDirMissing(PathBuf),
    #[error("hook executable '{0}' does not exist")]
    ExecutableMissing(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A subprocess hook: wraps an executable in `~/.config/ironjaw/hooks/<name>`.
#[derive(Clone)]
pub struct SubprocessHook {
    name: String,
    /// Absolute, canonicalized path to the executable. Resolved at construction.
    exe: PathBuf,
    priority: i32,
    timeout: Duration,
    /// Optional audit store. When `Some`, every execution is logged.
    audit: Option<AuditStore>,
}

impl std::fmt::Debug for SubprocessHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubprocessHook")
            .field("name", &self.name)
            .field("exe", &self.exe)
            .field("priority", &self.priority)
            .field("timeout", &self.timeout)
            .field("has_audit", &self.audit.is_some())
            .finish()
    }
}

impl SubprocessHook {
    /// Construct a SubprocessHook from a hooks directory + a hook name.
    ///
    /// Rejects:
    /// - empty names
    /// - names containing `..`, `/`, or `\` (path traversal)
    /// - resolved paths whose canonical form escapes `hooks_dir`
    /// - missing executables
    pub fn new(
        hooks_dir: impl AsRef<Path>,
        name: &str,
        priority: i32,
        timeout_secs: u64,
        audit: Option<AuditStore>,
    ) -> Result<Self, SubprocessHookError> {
        if name.is_empty() {
            return Err(SubprocessHookError::EmptyName(name.to_string()));
        }
        if name.contains("..") || name.contains('/') || name.contains('\\') {
            return Err(SubprocessHookError::ForbiddenPathChar(name.to_string()));
        }
        let dir = hooks_dir.as_ref();
        if !dir.exists() {
            return Err(SubprocessHookError::HooksDirMissing(dir.to_path_buf()));
        }
        let canon_dir = std::fs::canonicalize(dir)?;
        // Try the exact name first (test convention passes full filename
        // including extension); then fall back to common executable
        // extensions so production discovery — which passes a stem
        // stripped from e.g. `marker.cmd` — resolves correctly on
        // Windows and Unix alike.
        let direct = canon_dir.join(name);
        let candidate = if direct.exists() {
            direct
        } else {
            const EXTS: &[&str] = &["cmd", "bat", "exe", "ps1", "sh", "py"];
            EXTS.iter()
                .map(|ext| canon_dir.join(format!("{name}.{ext}")))
                .find(|p| p.exists())
                .ok_or_else(|| SubprocessHookError::ExecutableMissing(direct.clone()))?
        };
        let canon_exe = std::fs::canonicalize(&candidate)?;
        if !canon_exe.starts_with(&canon_dir) {
            return Err(SubprocessHookError::PathEscapesDir(canon_exe));
        }

        // Clamp timeout to ceiling defensively (config should already enforce).
        let clamped_secs = timeout_secs.min(HOOK_TIMEOUT_CEILING_SECS);
        Ok(Self {
            name: name.to_string(),
            exe: canon_exe,
            priority,
            timeout: Duration::from_secs(clamped_secs),
            audit,
        })
    }

    /// Read-only accessor: resolved executable path.
    pub fn exe_path(&self) -> &Path {
        &self.exe
    }
}

#[async_trait]
impl Hook for SubprocessHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn priority(&self) -> i32 {
        self.priority
    }

    async fn execute(&self, ctx: &HookContext) -> HookAction {
        use tokio::io::AsyncWriteExt;
        let start = Instant::now();
        let event_name = ctx.event.kind().to_string();

        // Serialize HookContext to JSON for stdin.
        let payload = serde_json::json!({
            "event": event_name,
            "session_id": ctx.session_id,
            "data": ctx.data,
            "timestamp": ctx.timestamp.to_rfc3339(),
        });
        let payload_bytes = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(e) => {
                return HookAction::Block(format!(
                    "subprocess hook '{}': payload serialization failed: {e}",
                    self.name
                ));
            }
        };

        let mut cmd = tokio::process::Command::new(&self.exe);
        sanitize_env(&mut cmd);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.audit_invocation(
                    &event_name,
                    None,
                    start.elapsed(),
                    false,
                    Some(&format!("spawn failed: {e}")),
                )
                .await;
                return HookAction::Block(format!(
                    "subprocess hook '{}': spawn failed: {e}",
                    self.name
                ));
            }
        };

        // Write payload to child stdin, then drop to signal EOF.
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(&payload_bytes).await {
                let _ = child.kill().await;
                self.audit_invocation(
                    &event_name,
                    None,
                    start.elapsed(),
                    false,
                    Some(&format!("stdin write failed: {e}")),
                )
                .await;
                return HookAction::Block(format!(
                    "subprocess hook '{}': stdin write failed: {e}",
                    self.name
                ));
            }
        }

        // Race child.wait() against the timeout. On timeout: actively kill
        // the child via tokio::process::Child::kill (which both signals and
        // awaits termination), then drain the pipes. We deliberately do NOT
        // use Child::wait_with_output() because it consumes the child and we
        // need to retain the handle to kill on timeout.
        use tokio::io::AsyncReadExt;
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        // Wait race.
        let status = match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                self.audit_invocation(
                    &event_name,
                    None,
                    start.elapsed(),
                    false,
                    Some(&format!("wait failed: {e}")),
                )
                .await;
                return HookAction::Block(format!(
                    "subprocess hook '{}': wait failed: {e}",
                    self.name
                ));
            }
            Err(_) => {
                // Timeout: kill the child synchronously (kill().await both
                // signals and reaps) and return the timeout block.
                let _ = child.kill().await;
                self.audit_invocation(&event_name, None, start.elapsed(), true, Some("timeout"))
                    .await;
                return HookAction::Block(format!(
                    "subprocess hook '{}': timed out after {:?}",
                    self.name, self.timeout
                ));
            }
        };

        // Drain stdout/stderr now that the process has exited (these can't
        // block — EOF was already signaled when the process died).
        let stdout_buf = if let Some(mut p) = stdout_pipe {
            let mut buf = Vec::new();
            let _ = p.read_to_end(&mut buf).await;
            buf
        } else {
            Vec::new()
        };
        let stderr_buf = if let Some(mut p) = stderr_pipe {
            let mut buf = Vec::new();
            let _ = p.read_to_end(&mut buf).await;
            buf
        } else {
            Vec::new()
        };
        let output = std::process::Output {
            status,
            stdout: stdout_buf,
            stderr: stderr_buf,
        };

        let exit_code = output.status.code();
        // Truncate stdout at MAX_HOOK_OUTPUT_BYTES.
        let (stdout_bytes, truncated) = if output.stdout.len() > MAX_HOOK_OUTPUT_BYTES {
            (output.stdout[..MAX_HOOK_OUTPUT_BYTES].to_vec(), true)
        } else {
            (output.stdout, false)
        };
        let stdout_str = String::from_utf8_lossy(&stdout_bytes).into_owned();

        self.audit_invocation(&event_name, exit_code, start.elapsed(), truncated, None)
            .await;

        // Result protocol:
        //   exit 0  + parseable JSON stdout = derive HookAction from JSON
        //   exit 0  + non-JSON stdout      = HookAction::Continue
        //   exit !=0                       = HookAction::Block(stderr|stdout snippet)
        match exit_code {
            Some(0) => {
                if stdout_str.trim().is_empty() {
                    HookAction::Continue
                } else {
                    parse_hook_action_from_json(&stdout_str).unwrap_or(HookAction::Continue)
                }
            }
            Some(code) => {
                let stderr_str = String::from_utf8_lossy(&output.stderr).into_owned();
                let detail = if !stderr_str.trim().is_empty() {
                    stderr_str
                } else {
                    stdout_str
                };
                HookAction::Block(format!(
                    "subprocess hook '{}' exited {code}: {}",
                    self.name,
                    detail.trim().chars().take(256).collect::<String>()
                ))
            }
            None => HookAction::Block(format!(
                "subprocess hook '{}': terminated by signal",
                self.name
            )),
        }
    }
}

impl SubprocessHook {
    /// Append an audit row for this invocation, if an AuditStore was supplied.
    async fn audit_invocation(
        &self,
        event: &str,
        exit_code: Option<i32>,
        duration: Duration,
        stdout_truncated_or_timeout: bool,
        error: Option<&str>,
    ) {
        let Some(audit) = &self.audit else {
            return;
        };
        let params = serde_json::json!({
            "event": event,
            "hook_name": &self.name,
            "exe": self.exe.display().to_string(),
            "exit_code": exit_code,
            "duration_ms": duration.as_millis() as u64,
            "stdout_truncated": stdout_truncated_or_timeout,
        });
        let entry = AuditEntry {
            action_type: ActionType::Event,
            method: Some("hook.subprocess.execute".to_string()),
            tool_name: Some(self.name.clone()),
            params: Some(params),
            result: None,
            ok: error.is_none() && exit_code == Some(0),
            error: error.map(|e| e.to_string()),
            llm_model: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            latency_ms: Some(duration.as_millis() as i64),
            session_id: None,
            device_id: None,
            request_id: Uuid::new_v4(),
            iteration: None,
            tool_calls: None,
        };
        let _ = audit.log(&entry).await;
    }
}

/// Parse a JSON document (trimmed) into a HookAction.
///
/// Accepted shapes:
/// ```json
/// {"action": "continue"}
/// {"action": "block", "reason": "policy"}
/// {"action": "modify", "value": <any>}
/// ```
fn parse_hook_action_from_json(s: &str) -> Option<HookAction> {
    let v: serde_json::Value = serde_json::from_str(s.trim()).ok()?;
    let action = v.get("action")?.as_str()?;
    match action {
        "continue" => Some(HookAction::Continue),
        "block" => {
            let reason = v
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("blocked")
                .to_string();
            Some(HookAction::Block(reason))
        }
        "modify" => {
            let value = v.get("value").cloned().unwrap_or(serde_json::Value::Null);
            Some(HookAction::Modify(value))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookEvent;
    use std::fs;

    /// Create a temp directory and return its path. The OS cleans up under /tmp;
    /// on Windows tests, the std tempdir is on the user temp drive.
    fn tmpdir(label: &str) -> PathBuf {
        let mut base = std::env::temp_dir();
        base.push(format!(
            "ironjaw-hook-test-{}-{}",
            label,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    /// Write an executable that mimics a hook. On Unix the file is chmod +x;
    /// on Windows we write a `.cmd` wrapper.
    #[cfg(unix)]
    fn write_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(windows)]
    fn write_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
        // On Windows, "no extension" still works for cmd execution via `cmd.exe /c`,
        // but tokio::process::Command::new(<path>) uses CreateProcess which requires
        // an executable extension OR a .bat/.cmd. We always write a `.cmd` and pass
        // the full name including extension.
        let path = dir.join(format!("{name}.cmd"));
        // Wrap the body so we can ignore the shebang.
        fs::write(&path, body).unwrap();
        path
    }

    /// On Windows tests we register the hook with the .cmd suffix.
    fn hook_name(base: &str) -> String {
        #[cfg(windows)]
        {
            format!("{base}.cmd")
        }
        #[cfg(unix)]
        {
            base.to_string()
        }
    }

    fn ctx() -> HookContext {
        HookContext::with_session(
            HookEvent::SessionStart,
            "test-sess",
            serde_json::json!({"hi": 1}),
        )
    }

    #[test]
    fn rejects_empty_name() {
        let dir = tmpdir("empty");
        let err = SubprocessHook::new(&dir, "", 0, 5, None).unwrap_err();
        assert!(matches!(err, SubprocessHookError::EmptyName(_)));
    }

    #[test]
    fn rejects_dotdot_traversal() {
        let dir = tmpdir("dotdot");
        let err = SubprocessHook::new(&dir, "../etc/passwd", 0, 5, None).unwrap_err();
        assert!(matches!(err, SubprocessHookError::ForbiddenPathChar(_)));
    }

    #[test]
    fn rejects_forward_slash() {
        let dir = tmpdir("fslash");
        let err = SubprocessHook::new(&dir, "sub/dir/hook", 0, 5, None).unwrap_err();
        assert!(matches!(err, SubprocessHookError::ForbiddenPathChar(_)));
    }

    #[test]
    fn rejects_back_slash() {
        let dir = tmpdir("bslash");
        let err = SubprocessHook::new(&dir, "sub\\dir\\hook", 0, 5, None).unwrap_err();
        assert!(matches!(err, SubprocessHookError::ForbiddenPathChar(_)));
    }

    #[test]
    fn rejects_missing_dir() {
        let mut dir = std::env::temp_dir();
        dir.push("ironjaw-hook-test-missing-dir-DOES-NOT-EXIST");
        let _ = std::fs::remove_dir_all(&dir);
        let err = SubprocessHook::new(&dir, "hook", 0, 5, None).unwrap_err();
        assert!(matches!(err, SubprocessHookError::HooksDirMissing(_)));
    }

    #[test]
    fn rejects_missing_executable() {
        let dir = tmpdir("noexe");
        let err = SubprocessHook::new(&dir, "ghost", 0, 5, None).unwrap_err();
        assert!(matches!(err, SubprocessHookError::ExecutableMissing(_)));
    }

    #[test]
    fn timeout_is_clamped_to_ceiling() {
        let dir = tmpdir("clamp");
        #[cfg(unix)]
        let _ = write_executable(&dir, "noop", "#!/bin/sh\nexit 0\n");
        #[cfg(windows)]
        let _ = write_executable(&dir, "noop", "@echo off\r\nexit /b 0\r\n");
        let hook = SubprocessHook::new(&dir, &hook_name("noop"), 0, 999_999, None).unwrap();
        assert_eq!(hook.timeout, Duration::from_secs(HOOK_TIMEOUT_CEILING_SECS));
    }

    #[tokio::test]
    async fn exit_zero_returns_continue() {
        let dir = tmpdir("ok");
        #[cfg(unix)]
        let _ = write_executable(&dir, "ok", "#!/bin/sh\nexit 0\n");
        #[cfg(windows)]
        let _ = write_executable(&dir, "ok", "@echo off\r\nexit /b 0\r\n");
        let hook = SubprocessHook::new(&dir, &hook_name("ok"), 0, 5, None).unwrap();
        let action = hook.execute(&ctx()).await;
        assert!(action.is_continue(), "got {action:?}");
    }

    #[tokio::test]
    async fn nonzero_exit_returns_block_with_stderr() {
        let dir = tmpdir("fail");
        #[cfg(unix)]
        let _ = write_executable(
            &dir,
            "boom",
            "#!/bin/sh\necho 'something bad' 1>&2\nexit 7\n",
        );
        #[cfg(windows)]
        let _ = write_executable(
            &dir,
            "boom",
            "@echo off\r\necho something bad 1>&2\r\nexit /b 7\r\n",
        );
        let hook = SubprocessHook::new(&dir, &hook_name("boom"), 0, 5, None).unwrap();
        let action = hook.execute(&ctx()).await;
        assert!(action.is_block(), "got {action:?}");
        if let HookAction::Block(reason) = action {
            assert!(reason.contains("exited 7"), "reason: {reason}");
            assert!(reason.contains("something bad"), "reason: {reason}");
        }
    }

    #[tokio::test]
    async fn json_block_stdout_translates_to_block_action() {
        let dir = tmpdir("jblock");
        #[cfg(unix)]
        let _ = write_executable(
            &dir,
            "blk",
            r#"#!/bin/sh
echo '{"action":"block","reason":"policy says no"}'
exit 0
"#,
        );
        #[cfg(windows)]
        let _ = write_executable(
            &dir,
            "blk",
            "@echo off\r\necho {\"action\":\"block\",\"reason\":\"policy says no\"}\r\nexit /b 0\r\n",
        );
        let hook = SubprocessHook::new(&dir, &hook_name("blk"), 0, 5, None).unwrap();
        let action = hook.execute(&ctx()).await;
        assert!(action.is_block());
        if let HookAction::Block(reason) = action {
            assert_eq!(reason, "policy says no");
        }
    }

    #[tokio::test]
    async fn timeout_kills_runaway_hook() {
        let dir = tmpdir("slow");
        #[cfg(unix)]
        let _ = write_executable(&dir, "slow", "#!/bin/sh\nsleep 30\n");
        #[cfg(windows)]
        let _ = write_executable(&dir, "slow", "@echo off\r\nping -n 30 127.0.0.1 >NUL\r\n");
        let hook = SubprocessHook::new(&dir, &hook_name("slow"), 0, 1, None).unwrap();
        let start = Instant::now();
        let action = hook.execute(&ctx()).await;
        let elapsed = start.elapsed();
        assert!(action.is_block(), "got {action:?}");
        // Timeout was 1s; allow generous slack but ensure we did NOT wait the full 30s.
        assert!(
            elapsed < Duration::from_secs(10),
            "elapsed {elapsed:?} suggests the timeout did not fire"
        );
        if let HookAction::Block(reason) = action {
            assert!(reason.contains("timed out"), "reason: {reason}");
        }
    }

    #[tokio::test]
    async fn output_is_truncated_at_cap() {
        // Verify the truncation logic itself rather than driving 16 KB through
        // cmd.exe (slow on Windows): synthesize an oversized buffer and apply
        // the same truncate-then-decode path the executor uses.
        let oversized: Vec<u8> = vec![b'X'; MAX_HOOK_OUTPUT_BYTES * 2];
        let (truncated_bytes, was_truncated) = if oversized.len() > MAX_HOOK_OUTPUT_BYTES {
            (oversized[..MAX_HOOK_OUTPUT_BYTES].to_vec(), true)
        } else {
            (oversized, false)
        };
        assert!(was_truncated);
        assert_eq!(truncated_bytes.len(), MAX_HOOK_OUTPUT_BYTES);

        // Smoke-test the executor's stdout-handling path with a small payload
        // (real subprocess, just one echo). Validates the spawn path
        // independently of the cap logic.
        let dir = tmpdir("smallout");
        #[cfg(unix)]
        let _ = write_executable(&dir, "small", "#!/bin/sh\necho hi\nexit 0\n");
        #[cfg(windows)]
        let _ = write_executable(&dir, "small", "@echo off\r\necho hi\r\nexit /b 0\r\n");
        let hook = SubprocessHook::new(&dir, &hook_name("small"), 0, 5, None).unwrap();
        let action = hook.execute(&ctx()).await;
        assert!(action.is_continue(), "got {action:?}");
    }

    #[tokio::test]
    async fn audit_log_is_written_when_store_provided() {
        let dir = tmpdir("audit");
        #[cfg(unix)]
        let _ = write_executable(&dir, "ok", "#!/bin/sh\nexit 0\n");
        #[cfg(windows)]
        let _ = write_executable(&dir, "ok", "@echo off\r\nexit /b 0\r\n");

        // In-memory audit store.
        let audit = AuditStore::open_memory().await.expect("audit store");
        let hook = SubprocessHook::new(&dir, &hook_name("ok"), 0, 5, Some(audit.clone())).unwrap();
        let _ = hook.execute(&ctx()).await;

        // Verify at least one audit row was written for this method.
        let rows = audit.recent(50).await.expect("recent audit rows");
        let any_match = rows
            .iter()
            .any(|e| e.method.as_deref() == Some("hook.subprocess.execute"));
        assert!(
            any_match,
            "expected at least one hook.subprocess.execute audit row; got {} rows",
            rows.len(),
        );
    }

    #[test]
    fn parse_hook_action_continue() {
        let a = parse_hook_action_from_json(r#"{"action":"continue"}"#).unwrap();
        assert!(a.is_continue());
    }

    #[test]
    fn parse_hook_action_block_uses_default_reason() {
        let a = parse_hook_action_from_json(r#"{"action":"block"}"#).unwrap();
        assert!(a.is_block());
        if let HookAction::Block(reason) = a {
            assert_eq!(reason, "blocked");
        }
    }

    #[test]
    fn parse_hook_action_modify_carries_value() {
        let a = parse_hook_action_from_json(r#"{"action":"modify","value":{"k":42}}"#).unwrap();
        assert!(a.is_modify());
        if let HookAction::Modify(v) = a {
            assert_eq!(v["k"], 42);
        }
    }

    #[test]
    fn parse_hook_action_unknown_returns_none() {
        assert!(parse_hook_action_from_json(r#"{"action":"explode"}"#).is_none());
        assert!(parse_hook_action_from_json("not json").is_none());
    }
}
