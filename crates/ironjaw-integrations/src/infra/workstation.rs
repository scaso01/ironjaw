use ironjaw_security::PermissionTier;
use ironjaw_tools::{MemoryHint, Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// workstation status tool — local system metrics (CPU, RAM, disk, services).
pub struct HostStatusTool;

impl Default for HostStatusTool {
    fn default() -> Self {
        Self
    }
}

impl HostStatusTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Tool for HostStatusTool {
    fn name(&self) -> &str {
        "host_status"
    }

    fn description(&self) -> &str {
        "Get THIS machine's status (workstation: Ryzen AI 9 HX 370, 64GB RAM, Windows 11): CPU, memory, disk, services, uptime. Use for 'is X running', 'how much RAM is free', 'what's eating CPU' questions about the local machine."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "detail": {
                    "type": "string",
                    "description": "What to check: cpu, memory, disk, services, or all (default: all)",
                    "enum": ["cpu", "memory", "disk", "services", "all"]
                }
            }
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
        let detail = params
            .get("detail")
            .and_then(|v| v.as_str())
            .unwrap_or("all");
        let mut result = serde_json::Map::new();

        if detail == "all" || detail == "cpu" {
            // Average load % across cores. wmic was removed in Win11 24H2+, so
            // use the CIM cmdlet instead.
            if let Ok(output) = run_ps(
                "[int]((Get-CimInstance Win32_Processor | \
                 Measure-Object -Property LoadPercentage -Average).Average)",
            )
            .await
            {
                result.insert(
                    "cpu_load_pct".to_string(),
                    serde_json::Value::String(output),
                );
            }
        }

        if detail == "all" || detail == "memory" {
            if let Ok(output) = run_ps(
                "$os = Get-CimInstance Win32_OperatingSystem; \
                 '{0} MB free / {1} MB total' -f \
                 [int]($os.FreePhysicalMemory/1024),[int]($os.TotalVisibleMemorySize/1024)",
            )
            .await
            {
                result.insert("memory".to_string(), serde_json::Value::String(output));
            }
        }

        if detail == "all" || detail == "disk" {
            if let Ok(output) = run_ps(
                "Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=3' | \
                 ForEach-Object { '{0} {1} GB free / {2} GB' -f \
                 $_.DeviceID,[int]($_.FreeSpace/1GB),[int]($_.Size/1GB) }",
            )
            .await
            {
                result.insert("disk".to_string(), serde_json::Value::String(output));
            }
        }

        if detail == "all" || detail == "services" {
            // Task Scheduler state per service. State is Running/Ready/Disabled;
            // "Ready" = registered + idle (the task launched and detached), NOT a
            // liveness probe.
            // ponytail: task state, not a port/process check. Upgrade to a port
            // probe if "is it actually serving" ever matters.
            let services = [
                "Llama Server",
                "CC Bridge",
                "CC Chronicle",
                "Caddy",
                "Ironjaw Gateway",
            ];
            let mut svc_status = Vec::new();
            for svc in &services {
                // svc names are hardcoded constants (no injection surface).
                let script = format!(
                    "try {{ (Get-ScheduledTask -TaskName '{svc}' -ErrorAction Stop).State }} \
                     catch {{ 'not_found' }}"
                );
                let status = match run_ps(&script).await {
                    Ok(output) => output.trim().to_lowercase(),
                    Err(_) => "error".to_string(),
                };
                svc_status.push(serde_json::json!({ "name": svc, "status": status }));
            }
            result.insert("services".to_string(), serde_json::json!(svc_status));
        }

        ToolResult::json(serde_json::Value::Object(result))
    }

    fn memory_hints(
        &self,
        _input: &serde_json::Value,
        _output: &serde_json::Value,
    ) -> Vec<MemoryHint> {
        vec![MemoryHint {
            entity: "workstation".to_string(),
            key: "services".to_string(),
            volatility: Duration::from_secs(300),
            content: None,
        }]
    }
}

/// Run a PowerShell script and return trimmed stdout.
///
/// The script is passed as a single argv element (not flattened through
/// `cmd /C`), so arguments containing spaces — e.g. the "Llama Server" task
/// name — survive intact. The old `cmd /C` + space-join path silently broke
/// every multi-word task name into "unknown".
///
/// ponytail: Windows-only by design (this tool reports workstation's status). Compiles
/// everywhere; only meaningful where `powershell` exists.
async fn run_ps(script: &str) -> Result<String, String> {
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => Ok(String::from_utf8_lossy(&output.stdout).trim().to_string()),
        Ok(Err(e)) => Err(format!("Command failed: {e}")),
        Err(_) => Err("Command timed out".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_status_metadata() {
        let tool = HostStatusTool::new();
        assert_eq!(tool.name(), "host_status");
        assert_eq!(tool.tier(), PermissionTier::Auto);
    }

    #[test]
    fn test_host_status_schema() {
        let tool = HostStatusTool::new();
        let schema = tool.schema();
        assert!(schema["properties"]["detail"].is_object());
    }

    // Proves the quoting fix: the old `cmd /C` + space-join path mangled any
    // arg with a space (the schtasks "Llama Server" -> "unknown" bug). run_ps
    // passes the script as one arg, so a space-containing value round-trips.
    #[cfg(windows)]
    #[tokio::test]
    async fn test_run_ps_preserves_spaces() {
        let out = run_ps("'Llama Server'").await.unwrap();
        assert_eq!(out, "Llama Server");
    }
}
