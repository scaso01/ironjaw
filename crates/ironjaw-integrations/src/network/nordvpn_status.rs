use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// NordVPN local-CLI status (workstation).
pub struct NordvpnStatusTool;

impl NordvpnStatusTool {
    pub fn new() -> Self {
        Self
    }
}
impl Default for NordvpnStatusTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for NordvpnStatusTool {
    fn name(&self) -> &str {
        "nordvpn_status"
    }
    fn description(&self) -> &str {
        "Get current NordVPN connection state on this machine (workstation): server, country, kill-switch state, mesh status. Read-only."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{}})
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
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new("nordvpn")
                .args(["status"])
                .output(),
        )
        .await;
        match result {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                ToolResult::json(
                    serde_json::json!({"raw": stdout, "exit_code": out.status.code().unwrap_or(-1)}),
                )
            }
            Ok(Err(e)) => ToolResult::error("EXEC_ERROR", format!("nordvpn: {e}")),
            Err(_) => ToolResult::error("TIMEOUT", "nordvpn status timed out"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meta() {
        assert_eq!(NordvpnStatusTool::new().name(), "nordvpn_status");
    }
}
