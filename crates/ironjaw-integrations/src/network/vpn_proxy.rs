use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// VPN-proxy probe — test the 10 nordlynx SOCKS proxies on docker-host (1081-1090).
pub struct VpnProxyTool;

impl VpnProxyTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for VpnProxyTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for VpnProxyTool {
    fn name(&self) -> &str {
        "vpn_proxy"
    }
    fn description(&self) -> &str {
        "Test or query the 10 NordLynx SOCKS5 proxies (slots 1-10 on ports 1081-1090) on docker-host. Action 'check' tests reachability + reports exit IP per slot. Use for 'is VPN slot 5 up', 'what's slot 3's exit country', 'rotate to a fresh exit'."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "action":{"type":"string","enum":["check","exit_ip"],"default":"check"},
                "slot":{"type":"integer","minimum":1,"maximum":10}
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
        let slot = params.get("slot").and_then(|v| v.as_u64());
        let slots: Vec<u16> = match slot {
            Some(s) if (1..=10).contains(&s) => vec![1080 + s as u16],
            None => (1081u16..=1090).collect(),
            _ => return ToolResult::error("INVALID_PARAMS", "slot must be 1-10"),
        };
        let mut results = Vec::new();
        for port in slots {
            let proxy_url = format!("socks5://192.0.2.20:{port}");
            let proxy = match reqwest::Proxy::all(&proxy_url) {
                Ok(p) => p,
                Err(e) => {
                    results.push(
                        serde_json::json!({"slot": port-1080, "ok": false, "err": e.to_string()}),
                    );
                    continue;
                }
            };
            let client = match reqwest::Client::builder()
                .proxy(proxy)
                .timeout(Duration::from_secs(8))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    results.push(
                        serde_json::json!({"slot": port-1080, "ok": false, "err": e.to_string()}),
                    );
                    continue;
                }
            };
            // Both `check` and `exit_ip` actions report the slot's exit IP via ipify.
            let target = "https://api.ipify.org";
            match client.get(target).send().await {
                Ok(r) => {
                    let ip = r.text().await.unwrap_or_default();
                    results.push(serde_json::json!({"slot": port-1080, "ok": true, "exit_ip": ip}));
                }
                Err(e) => results.push(
                    serde_json::json!({"slot": port-1080, "ok": false, "err": e.to_string()}),
                ),
            }
        }
        ToolResult::json(serde_json::json!({"count": results.len(), "results": results}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meta() {
        assert_eq!(VpnProxyTool::new().name(), "vpn_proxy");
    }
}
