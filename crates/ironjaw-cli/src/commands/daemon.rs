use crate::{CliError, DaemonAction};

/// Build the HTTP method, path, and optional body for a daemon action.
pub fn build_request(action: &DaemonAction) -> (&'static str, String, Option<serde_json::Value>) {
    match action {
        DaemonAction::Status => ("GET", "/api/daemon/status".to_string(), None),
        DaemonAction::Log { lines } => ("GET", format!("/api/daemon/log?lines={lines}"), None),
    }
}

/// Execute a daemon subcommand against the server.
pub async fn execute(
    server: &str,
    token: Option<&str>,
    json_output: bool,
    action: &DaemonAction,
) -> Result<String, CliError> {
    let token = token.ok_or(CliError::AuthRequired)?;
    let (method, path, _body) = build_request(action);
    let url = format!("{server}{path}");

    let client = reqwest::Client::new();
    let mut req = match method {
        "GET" => client.get(&url),
        _ => return Err(CliError::Http(format!("unsupported method: {method}"))),
    };
    req = req.bearer_auth(token);

    let resp = req
        .send()
        .await
        .map_err(|e| CliError::Http(e.to_string()))?;
    let status_code = resp.status();
    let resp_body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| CliError::Parse(e.to_string()))?;

    if !status_code.is_success() {
        return Err(CliError::Server(format!(
            "{status_code}: {}",
            serde_json::to_string(&resp_body).unwrap_or_default()
        )));
    }

    if json_output {
        serde_json::to_string_pretty(&resp_body).map_err(|e| CliError::Parse(e.to_string()))
    } else {
        format_daemon_response(action, &resp_body)
    }
}

fn format_daemon_response(
    action: &DaemonAction,
    body: &serde_json::Value,
) -> Result<String, CliError> {
    match action {
        DaemonAction::Status => {
            let enabled = body
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let heartbeat = body
                .get("heartbeat_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let last_run = body
                .get("last_run")
                .and_then(|v| v.as_str())
                .unwrap_or("never");
            let state = if enabled { "running" } else { "stopped" };
            Ok(format!(
                "Daemon: {state}\nHeartbeat: {heartbeat}s\nLast run: {last_run}"
            ))
        }
        DaemonAction::Log { .. } => {
            let entries = body
                .get("entries")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if entries.is_empty() {
                return Ok("No daemon log entries.".to_string());
            }
            let mut lines = Vec::new();
            for entry in &entries {
                let ts = entry
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let msg = entry.get("message").and_then(|v| v.as_str()).unwrap_or("?");
                lines.push(format!("  [{ts}] {msg}"));
            }
            Ok(lines.join("\n"))
        }
    }
}

/// Describe what the daemon command would send.
pub fn describe(server: &str, action: &DaemonAction) -> String {
    let (method, path, _body) = build_request(action);
    format!("{method} {server}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_status_request() {
        let (method, path, body) = build_request(&DaemonAction::Status);
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/daemon/status");
        assert!(body.is_none());
    }

    #[test]
    fn build_log_request() {
        let (method, path, body) = build_request(&DaemonAction::Log { lines: 50 });
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/daemon/log?lines=50");
        assert!(body.is_none());
    }

    #[test]
    fn format_status_running() {
        let body = serde_json::json!({
            "enabled": true,
            "heartbeat_secs": 300,
            "last_run": "2026-04-06T10:00:00Z"
        });
        let result = format_daemon_response(&DaemonAction::Status, &body).unwrap();
        assert!(result.contains("Daemon: running"));
        assert!(result.contains("Heartbeat: 300s"));
        assert!(result.contains("Last run: 2026-04-06T10:00:00Z"));
    }

    #[test]
    fn format_status_stopped() {
        let body = serde_json::json!({"enabled": false, "heartbeat_secs": 0});
        let result = format_daemon_response(&DaemonAction::Status, &body).unwrap();
        assert!(result.contains("Daemon: stopped"));
    }

    #[test]
    fn format_log_empty() {
        let body = serde_json::json!({"entries": []});
        let result = format_daemon_response(&DaemonAction::Log { lines: 10 }, &body).unwrap();
        assert_eq!(result, "No daemon log entries.");
    }

    #[test]
    fn format_log_entries() {
        let body = serde_json::json!({"entries": [
            {"timestamp": "10:00", "message": "heartbeat"},
            {"timestamp": "10:05", "message": "checked schedules"},
        ]});
        let result = format_daemon_response(&DaemonAction::Log { lines: 10 }, &body).unwrap();
        assert!(result.contains("[10:00] heartbeat"));
        assert!(result.contains("[10:05] checked schedules"));
    }

    #[test]
    fn describe_status() {
        let desc = describe("http://localhost:18794", &DaemonAction::Status);
        assert_eq!(desc, "GET http://localhost:18794/api/daemon/status");
    }

    #[test]
    fn describe_log() {
        let desc = describe("http://localhost:18794", &DaemonAction::Log { lines: 30 });
        assert_eq!(desc, "GET http://localhost:18794/api/daemon/log?lines=30");
    }
}
