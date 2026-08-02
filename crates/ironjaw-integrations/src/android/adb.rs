use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Default target when no serial is given: the phone over Meshnet. We match on the
/// IP prefix (not the full serial) because the wireless-debug port rotates per boot
/// while the Meshnet IP is stable.
// ponytail: single hardcoded phone IP; add an [integrations] config field if a
// second phone / different IP ever needs to be the default.
const DEFAULT_PHONE_PREFIX: &str = "198.51.100.206";

/// ADB tool — interact with Android devices via the `adb` binary.
pub struct AdbTool;

impl AdbTool {
    pub fn new() -> Self {
        Self
    }

    /// Determine the effective permission tier for a given action.
    /// Varies by action since some are read-only (Auto) and others are destructive (Confirm).
    pub fn tier_for_action(action: &str) -> PermissionTier {
        match action {
            "devices" | "logcat" => PermissionTier::Auto,
            "pull" => PermissionTier::Notify,
            "shell" | "install" | "push" => PermissionTier::Confirm,
            _ => PermissionTier::Confirm,
        }
    }

    /// Build the adb command arguments for a given action and parameters.
    /// Returns the list of arguments to pass to the `adb` binary, or an error message.
    pub fn build_args(action: &str, params: &serde_json::Value) -> Result<Vec<String>, String> {
        let args = params.get("args").unwrap_or(&serde_json::Value::Null);

        match action {
            "devices" => Ok(vec!["devices".to_string(), "-l".to_string()]),

            "shell" => {
                let cmd = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or("args.command is required for shell action")?;
                Ok(vec!["shell".to_string(), cmd.to_string()])
            }

            "install" => {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or("args.path is required for install action")?;
                Ok(vec![
                    "install".to_string(),
                    "-r".to_string(),
                    path.to_string(),
                ])
            }

            "push" => {
                let local = args
                    .get("local")
                    .and_then(|v| v.as_str())
                    .ok_or("args.local is required for push action")?;
                let remote = args
                    .get("remote")
                    .and_then(|v| v.as_str())
                    .ok_or("args.remote is required for push action")?;
                Ok(vec![
                    "push".to_string(),
                    local.to_string(),
                    remote.to_string(),
                ])
            }

            "pull" => {
                let remote = args
                    .get("remote")
                    .and_then(|v| v.as_str())
                    .ok_or("args.remote is required for pull action")?;
                let local = args
                    .get("local")
                    .and_then(|v| v.as_str())
                    .ok_or("args.local is required for pull action")?;
                Ok(vec![
                    "pull".to_string(),
                    remote.to_string(),
                    local.to_string(),
                ])
            }

            "logcat" => {
                let lines = args.get("lines").and_then(|v| v.as_u64()).unwrap_or(100);
                Ok(vec![
                    "logcat".to_string(),
                    "-d".to_string(),
                    "-t".to_string(),
                    lines.to_string(),
                ])
            }

            _ => Err(format!("Unknown action: {action}")),
        }
    }

    /// Resolve which device serial to target for a single-device action.
    ///
    /// - An explicit (non-empty) serial is trusted and used as-is.
    /// - Otherwise parse `adb devices` output: prefer the phone (serial whose host
    ///   matches `DEFAULT_PHONE_PREFIX`); if exactly one device is connected, use it;
    ///   if several non-phone devices are connected, error and list them so the
    ///   caller can pass `args.serial`.
    pub fn resolve_serial(
        explicit: Option<&str>,
        devices_output: &str,
    ) -> Result<Option<String>, String> {
        if let Some(s) = explicit {
            if !s.is_empty() {
                return Ok(Some(s.to_string()));
            }
        }

        let serials: Vec<String> = devices_output
            .lines()
            .filter_map(|line| {
                let mut it = line.split_whitespace();
                let serial = it.next()?;
                let state = it.next()?;
                // skips the "List of devices attached" header and offline/unauthorized
                if state == "device" {
                    Some(serial.to_string())
                } else {
                    None
                }
            })
            .collect();

        if serials.is_empty() {
            return Err("no adb devices connected".to_string());
        }
        if let Some(phone) = serials.iter().find(|s| s.starts_with(DEFAULT_PHONE_PREFIX)) {
            return Ok(Some(phone.clone()));
        }
        if let [only] = serials.as_slice() {
            return Ok(Some(only.clone()));
        }
        Err(format!(
            "multiple devices connected and no serial given; pass args.serial. Connected: {}",
            serials.join(", ")
        ))
    }
}

impl Default for AdbTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve the `adb` executable path.
///
/// The gateway runs under Task Scheduler with a minimal PATH that does NOT
/// include the Android SDK's `platform-tools` dir, so a bare `adb` spawn fails
/// with "program not found". Resolve it explicitly: ANDROID_HOME /
/// ANDROID_SDK_ROOT first, then the default Windows SDK location under the user
/// profile, falling back to PATH lookup.
///
/// ponytail: hardcoded Windows SDK default. Add a config field if a non-default
/// SDK path or a non-Windows host ever needs it.
fn adb_program() -> std::path::PathBuf {
    use std::path::PathBuf;
    let exe = if cfg!(target_os = "windows") {
        "adb.exe"
    } else {
        "adb"
    };

    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Ok(root) = std::env::var(var) {
            let p = PathBuf::from(root).join("platform-tools").join(exe);
            if p.exists() {
                return p;
            }
        }
    }

    if cfg!(target_os = "windows") {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let p = PathBuf::from(local)
                .join("Android")
                .join("Sdk")
                .join("platform-tools")
                .join(exe);
            if p.exists() {
                return p;
            }
        }
    }

    PathBuf::from("adb")
}

/// Resolve the `ssh` executable for the rish-over-ssh phone fallback.
///
/// Like `adb_program`, the gateway's service PATH may not include it, so prefer the
/// Windows OpenSSH location and fall back to a bare `ssh` lookup.
/// ponytail: Windows default path; add config if a non-default ssh client is needed.
fn ssh_program() -> std::path::PathBuf {
    use std::path::PathBuf;
    if cfg!(target_os = "windows") {
        if let Ok(root) = std::env::var("SystemRoot") {
            let p = PathBuf::from(root)
                .join("System32")
                .join("OpenSSH")
                .join("ssh.exe");
            if p.exists() {
                return p;
            }
        }
        return PathBuf::from("ssh.exe");
    }
    PathBuf::from("ssh")
}

/// Build the remote command for rish-over-ssh: run the user's command on the phone
/// through rish (Shizuku shell, uid 2000). The command is single-quoted for the
/// remote `sh`, with embedded single quotes escaped (`'` -> `'\''`).
fn build_rish_remote(command: &str) -> String {
    let escaped = command.replace('\'', "'\\''");
    format!("~/.shizuku/rish -c '{escaped}'")
}

/// Phone `shell` fallback when adb can't reach the phone: run the command on the
/// phone via rish (Shizuku, uid 2000) over Termux's ssh server on a fixed port.
/// adb's wireless-debug port rotates per boot and turns itself off; this ssh path
/// stays up, so phone shell commands keep working.
///
/// ponytail: phone ssh endpoint + key are hardcoded to match DEFAULT_PHONE_PREFIX.
/// Promote to an [integrations] config block if a second phone / different login appears.
async fn run_rish_shell(command: &str) -> ToolResult {
    const SSH_ENDPOINT: &str = "u0_a711@198.51.100.206";
    const SSH_PORT: &str = "8022";

    let key = match std::env::var("USERPROFILE") {
        Ok(home) => format!("{home}\\.ssh\\id_ed25519"),
        Err(_) => {
            return ToolResult::error(
                "RISH_NO_KEY",
                "USERPROFILE not set; cannot locate the ssh key for the rish fallback",
            );
        }
    };

    let remote = build_rish_remote(command);

    // BatchMode + no host-key prompts so a service context never hangs waiting for input.
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(ssh_program())
            .args([
                "-i",
                &key,
                "-p",
                SSH_PORT,
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=NUL",
                "-o",
                "ConnectTimeout=8",
                SSH_ENDPOINT,
                &remote,
            ])
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let exit_code = output.status.code().unwrap_or(-1);
            if exit_code != 0 {
                return ToolResult::error(
                    "RISH_ERROR",
                    format!("rish shell failed (exit {exit_code}): {stderr}"),
                );
            }
            ToolResult::json(serde_json::json!({
                "action": "shell",
                "output": stdout,
                "stderr": stderr,
                "via": "rish-over-ssh",
            }))
        }
        Ok(Err(e)) => ToolResult::error("RISH_ERROR", format!("Failed to run ssh for rish: {e}")),
        Err(_) => ToolResult::error("TIMEOUT", "rish shell timed out after 30s"),
    }
}

#[async_trait::async_trait]
impl Tool for AdbTool {
    fn name(&self) -> &str {
        "adb"
    }

    fn description(&self) -> &str {
        "Interact with Android devices via ADB: list devices, run shell commands, \
         install APKs, push/pull files, read logcat"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "ADB action to perform",
                    "enum": ["devices", "shell", "install", "push", "pull", "logcat"]
                },
                "args": {
                    "type": "object",
                    "description": "Action-specific arguments",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Shell command (for shell action)"
                        },
                        "path": {
                            "type": "string",
                            "description": "APK path (for install action)"
                        },
                        "local": {
                            "type": "string",
                            "description": "Local file path (for push/pull actions)"
                        },
                        "remote": {
                            "type": "string",
                            "description": "Remote device path (for push/pull actions)"
                        },
                        "lines": {
                            "type": "integer",
                            "description": "Number of logcat lines (default 100, for logcat action)",
                            "minimum": 1,
                            "maximum": 5000
                        },
                        "serial": {
                            "type": "string",
                            "description": "Target device serial (e.g. '198.51.100.206:43433' for the phone, '192.0.2.30:5555' for a Fire TV). Omit to default to the phone. Use action 'devices' to list connected serials."
                        }
                    }
                }
            },
            "required": ["action"]
        })
    }

    fn tier(&self) -> PermissionTier {
        // Base tier is Confirm since the most dangerous actions require it.
        // Actual enforcement per action is handled by the caller via tier_for_action().
        PermissionTier::Confirm
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let action = match params.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: action");
            }
        };

        // Advisory: log if the per-action tier differs from the method-level tier.
        // Per-action tiers are advisory until the handler supports per-invocation tier checks.
        let action_tier = Self::tier_for_action(action);
        let method_tier = self.tier();
        if action_tier != method_tier {
            tracing::debug!(
                action = action,
                action_tier = ?action_tier,
                method_tier = ?method_tier,
                "ADB action tier differs from method-level tier (advisory)"
            );
        }

        let action_args = match Self::build_args(action, &params) {
            Ok(args) => args,
            Err(msg) => return ToolResult::error("INVALID_PARAMS", msg),
        };

        // Resolve the target device. `devices` lists everything and needs no target;
        // every other action runs against a single device, so with >1 connected we
        // must pass `-s <serial>` or adb fails "more than one device/emulator".
        let explicit_serial = params
            .get("args")
            .and_then(|a| a.get("serial"))
            .and_then(|v| v.as_str());
        let serial = if action == "devices" {
            None
        } else {
            let listing = match tokio::time::timeout(
                Duration::from_secs(10),
                tokio::process::Command::new(adb_program())
                    .arg("devices")
                    .output(),
            )
            .await
            {
                Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).to_string(),
                _ => String::new(),
            };
            match Self::resolve_serial(explicit_serial, &listing) {
                Ok(s) => s,
                Err(msg) => {
                    // adb has no usable target. The phone is the default target and is
                    // still reachable out-of-band via rish-over-ssh (Shizuku, uid 2000)
                    // on a fixed ssh port, so phone shell commands work even when adb's
                    // wireless-debug port has rotated or turned itself off.
                    // ponytail: only covers the default/omitted serial. If a caller passes
                    // a STALE explicit phone serial, resolve_serial accepts it and adb
                    // fails at runtime instead (no rish fallback). The schema tells callers
                    // to omit the serial for the phone, so this is the documented path.
                    let wants_phone = explicit_serial
                        .map(|s| s.is_empty() || s.starts_with(DEFAULT_PHONE_PREFIX))
                        .unwrap_or(true);
                    if action == "shell" && wants_phone {
                        let cmd = action_args.get(1).map(|s| s.as_str()).unwrap_or_default();
                        return run_rish_shell(cmd).await;
                    }
                    return ToolResult::error("ADB_NO_DEVICE", msg);
                }
            }
        };

        let mut adb_args: Vec<String> = Vec::new();
        if let Some(s) = &serial {
            adb_args.push("-s".to_string());
            adb_args.push(s.clone());
        }
        adb_args.extend(action_args);

        let result = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new(adb_program())
                .args(&adb_args)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let exit_code = output.status.code().unwrap_or(-1);

                if exit_code != 0 {
                    return ToolResult::error(
                        "ADB_ERROR",
                        format!("adb {action} failed (exit {exit_code}): {stderr}"),
                    );
                }

                ToolResult::json(serde_json::json!({
                    "action": action,
                    "output": stdout,
                    "stderr": stderr,
                }))
            }
            Ok(Err(e)) => ToolResult::error("ADB_ERROR", format!("Failed to run adb: {e}")),
            Err(_) => ToolResult::error("TIMEOUT", format!("adb {action} timed out after 30s")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_ctx() -> ToolContext {
        ToolContext::new("test", "test", PathBuf::from("/tmp"))
    }

    #[test]
    fn test_adb_metadata() {
        let tool = AdbTool::new();
        assert_eq!(tool.name(), "adb");
        assert_eq!(tool.tier(), PermissionTier::Confirm);
    }

    #[test]
    fn test_adb_schema() {
        let tool = AdbTool::new();
        let schema = tool.schema();
        assert!(schema["properties"]["action"].is_object());
        assert!(schema["properties"]["args"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("action")));
    }

    #[test]
    fn test_adb_default() {
        let tool = AdbTool;
        assert_eq!(tool.name(), "adb");
    }

    #[test]
    fn test_adb_program_resolves_to_adb_binary() {
        // Whether resolved to a full SDK path or the bare PATH fallback, the
        // file stem must be "adb" (never empty / never a directory).
        let p = adb_program();
        assert_eq!(p.file_stem().and_then(|s| s.to_str()), Some("adb"));
    }

    // -- tier_for_action tests --

    #[test]
    fn test_tier_devices_auto() {
        assert_eq!(AdbTool::tier_for_action("devices"), PermissionTier::Auto);
    }

    #[test]
    fn test_tier_logcat_auto() {
        assert_eq!(AdbTool::tier_for_action("logcat"), PermissionTier::Auto);
    }

    #[test]
    fn test_tier_pull_notify() {
        assert_eq!(AdbTool::tier_for_action("pull"), PermissionTier::Notify);
    }

    #[test]
    fn test_tier_shell_confirm() {
        assert_eq!(AdbTool::tier_for_action("shell"), PermissionTier::Confirm);
    }

    #[test]
    fn test_tier_install_confirm() {
        assert_eq!(AdbTool::tier_for_action("install"), PermissionTier::Confirm);
    }

    #[test]
    fn test_tier_push_confirm() {
        assert_eq!(AdbTool::tier_for_action("push"), PermissionTier::Confirm);
    }

    #[test]
    fn test_tier_unknown_confirm() {
        assert_eq!(AdbTool::tier_for_action("unknown"), PermissionTier::Confirm);
    }

    // -- build_args tests --

    #[test]
    fn test_build_args_devices() {
        let args = AdbTool::build_args("devices", &serde_json::json!({})).unwrap();
        assert_eq!(args, vec!["devices", "-l"]);
    }

    #[test]
    fn test_build_args_shell() {
        let params = serde_json::json!({"args": {"command": "pm list packages"}});
        let args = AdbTool::build_args("shell", &params).unwrap();
        assert_eq!(args, vec!["shell", "pm list packages"]);
    }

    #[test]
    fn test_build_args_shell_missing_command() {
        let params = serde_json::json!({"args": {}});
        let err = AdbTool::build_args("shell", &params).unwrap_err();
        assert!(err.contains("command"));
    }

    #[test]
    fn test_build_args_install() {
        let params = serde_json::json!({"args": {"path": "/tmp/app.apk"}});
        let args = AdbTool::build_args("install", &params).unwrap();
        assert_eq!(args, vec!["install", "-r", "/tmp/app.apk"]);
    }

    #[test]
    fn test_build_args_install_missing_path() {
        let params = serde_json::json!({"args": {}});
        let err = AdbTool::build_args("install", &params).unwrap_err();
        assert!(err.contains("path"));
    }

    #[test]
    fn test_build_args_push() {
        let params =
            serde_json::json!({"args": {"local": "/tmp/file.txt", "remote": "/sdcard/file.txt"}});
        let args = AdbTool::build_args("push", &params).unwrap();
        assert_eq!(args, vec!["push", "/tmp/file.txt", "/sdcard/file.txt"]);
    }

    #[test]
    fn test_build_args_push_missing_local() {
        let params = serde_json::json!({"args": {"remote": "/sdcard/file.txt"}});
        let err = AdbTool::build_args("push", &params).unwrap_err();
        assert!(err.contains("local"));
    }

    #[test]
    fn test_build_args_push_missing_remote() {
        let params = serde_json::json!({"args": {"local": "/tmp/file.txt"}});
        let err = AdbTool::build_args("push", &params).unwrap_err();
        assert!(err.contains("remote"));
    }

    #[test]
    fn test_build_args_pull() {
        let params =
            serde_json::json!({"args": {"remote": "/sdcard/photo.jpg", "local": "/tmp/photo.jpg"}});
        let args = AdbTool::build_args("pull", &params).unwrap();
        assert_eq!(args, vec!["pull", "/sdcard/photo.jpg", "/tmp/photo.jpg"]);
    }

    #[test]
    fn test_build_args_pull_missing_remote() {
        let params = serde_json::json!({"args": {"local": "/tmp/photo.jpg"}});
        let err = AdbTool::build_args("pull", &params).unwrap_err();
        assert!(err.contains("remote"));
    }

    #[test]
    fn test_build_args_pull_missing_local() {
        let params = serde_json::json!({"args": {"remote": "/sdcard/photo.jpg"}});
        let err = AdbTool::build_args("pull", &params).unwrap_err();
        assert!(err.contains("local"));
    }

    #[test]
    fn test_build_args_logcat_default() {
        let params = serde_json::json!({});
        let args = AdbTool::build_args("logcat", &params).unwrap();
        assert_eq!(args, vec!["logcat", "-d", "-t", "100"]);
    }

    #[test]
    fn test_build_args_logcat_custom_lines() {
        let params = serde_json::json!({"args": {"lines": 50}});
        let args = AdbTool::build_args("logcat", &params).unwrap();
        assert_eq!(args, vec!["logcat", "-d", "-t", "50"]);
    }

    #[test]
    fn test_build_args_unknown_action() {
        let err = AdbTool::build_args("reboot", &serde_json::json!({})).unwrap_err();
        assert!(err.contains("Unknown action"));
    }

    // -- execute parameter validation tests --

    #[tokio::test]
    async fn test_adb_missing_action() {
        let tool = AdbTool::new();
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, .. } => assert_eq!(code, "INVALID_PARAMS"),
            _ => panic!("Expected error"),
        }
    }

    #[tokio::test]
    async fn test_adb_invalid_action() {
        let tool = AdbTool::new();
        let result = tool
            .execute(
                serde_json::json!({"action": "reboot"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, message } => {
                assert_eq!(code, "INVALID_PARAMS");
                assert!(message.contains("Unknown action"));
            }
            _ => panic!("Expected error"),
        }
    }

    #[tokio::test]
    async fn test_adb_shell_missing_args() {
        let tool = AdbTool::new();
        let result = tool
            .execute(
                serde_json::json!({"action": "shell"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, message } => {
                assert_eq!(code, "INVALID_PARAMS");
                assert!(message.contains("command"));
            }
            _ => panic!("Expected error"),
        }
    }

    #[tokio::test]
    async fn test_adb_install_missing_path() {
        let tool = AdbTool::new();
        let result = tool
            .execute(
                serde_json::json!({"action": "install", "args": {}}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, message } => {
                assert_eq!(code, "INVALID_PARAMS");
                assert!(message.contains("path"));
            }
            _ => panic!("Expected error"),
        }
    }

    // -- resolve_serial tests --

    #[test]
    fn test_resolve_serial_explicit_passthrough() {
        let out = "List of devices attached\n198.51.100.206:43433\tdevice\n";
        let r = AdbTool::resolve_serial(Some("192.0.2.30:5555"), out).unwrap();
        assert_eq!(r, Some("192.0.2.30:5555".to_string()));
    }

    #[test]
    fn test_resolve_serial_defaults_to_phone() {
        let out = "List of devices attached\n\
                   192.0.2.30:5555\tdevice\n\
                   198.51.100.206:43433\tdevice\n\
                   192.0.2.37:5555\tdevice\n";
        let r = AdbTool::resolve_serial(None, out).unwrap();
        assert_eq!(r, Some("198.51.100.206:43433".to_string()));
    }

    #[test]
    fn test_resolve_serial_single_device() {
        let out = "List of devices attached\n192.0.2.30:5555\tdevice\n";
        let r = AdbTool::resolve_serial(None, out).unwrap();
        assert_eq!(r, Some("192.0.2.30:5555".to_string()));
    }

    #[test]
    fn test_resolve_serial_none_connected() {
        let out = "List of devices attached\n192.168.1.15:5555\toffline\n";
        let err = AdbTool::resolve_serial(None, out).unwrap_err();
        assert!(err.contains("no adb devices"));
    }

    #[test]
    fn test_resolve_serial_ambiguous_errors() {
        let out = "List of devices attached\n\
                   192.0.2.30:5555\tdevice\n\
                   192.0.2.37:5555\tdevice\n";
        let err = AdbTool::resolve_serial(None, out).unwrap_err();
        assert!(err.contains("multiple devices"));
        assert!(err.contains("192.0.2.30:5555"));
    }

    #[test]
    fn test_resolve_serial_empty_explicit_falls_through() {
        let out = "List of devices attached\n198.51.100.206:43433\tdevice\n";
        let r = AdbTool::resolve_serial(Some(""), out).unwrap();
        assert_eq!(r, Some("198.51.100.206:43433".to_string()));
    }

    // -- build_rish_remote tests --

    #[test]
    fn test_build_rish_remote_plain() {
        assert_eq!(
            build_rish_remote("getprop ro.product.model"),
            "~/.shizuku/rish -c 'getprop ro.product.model'"
        );
    }

    #[test]
    fn test_build_rish_remote_escapes_single_quotes() {
        // embedded single quotes must be escaped so the remote sh keeps them literal
        assert_eq!(
            build_rish_remote("echo 'hi'"),
            "~/.shizuku/rish -c 'echo '\\''hi'\\'''"
        );
    }
}
