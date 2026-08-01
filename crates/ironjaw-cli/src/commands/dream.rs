use crate::{CliError, DreamAction};

/// Build the HTTP method, path, and optional body for a dream action.
pub fn build_request(action: &DreamAction) -> (&'static str, String, Option<serde_json::Value>) {
    match action {
        DreamAction::Status => ("GET", "/api/brain/dream/status".to_string(), None),
        DreamAction::Trigger => ("POST", "/api/brain/dream/trigger".to_string(), None),
    }
}

/// Execute a dream subcommand against the server.
pub async fn execute(
    server: &str,
    token: Option<&str>,
    json_output: bool,
    action: &DreamAction,
) -> Result<String, CliError> {
    let token = token.ok_or(CliError::AuthRequired)?;
    let (method, path, _body) = build_request(action);
    let url = format!("{server}{path}");

    let client = reqwest::Client::new();
    let mut req = match method {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
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
        format_dream_response(action, &resp_body)
    }
}

fn format_dream_response(
    action: &DreamAction,
    body: &serde_json::Value,
) -> Result<String, CliError> {
    match action {
        DreamAction::Status => {
            let phase = body.get("phase").and_then(|v| v.as_str()).unwrap_or("idle");
            let enabled = body
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let last_run = body
                .get("last_run")
                .and_then(|v| v.as_str())
                .unwrap_or("never");
            let memories = body
                .get("memories_processed")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let state = if enabled { "enabled" } else { "disabled" };
            Ok(format!(
                "Dream gate: {state}\nPhase: {phase}\nLast run: {last_run}\nMemories processed: {memories}"
            ))
        }
        DreamAction::Trigger => {
            let status = body
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            Ok(format!("Dream consolidation: {status}"))
        }
    }
}

/// Describe what the dream command would send.
pub fn describe(server: &str, action: &DreamAction) -> String {
    let (method, path, _body) = build_request(action);
    format!("{method} {server}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_status_request() {
        let (method, path, body) = build_request(&DreamAction::Status);
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/brain/dream/status");
        assert!(body.is_none());
    }

    #[test]
    fn build_trigger_request() {
        let (method, path, body) = build_request(&DreamAction::Trigger);
        assert_eq!(method, "POST");
        assert_eq!(path, "/api/brain/dream/trigger");
        assert!(body.is_none());
    }

    #[test]
    fn format_status_enabled() {
        let body = serde_json::json!({
            "enabled": true,
            "phase": "consolidating",
            "last_run": "2026-04-06T09:00:00Z",
            "memories_processed": 42
        });
        let result = format_dream_response(&DreamAction::Status, &body).unwrap();
        assert!(result.contains("Dream gate: enabled"));
        assert!(result.contains("Phase: consolidating"));
        assert!(result.contains("Memories processed: 42"));
    }

    #[test]
    fn format_status_disabled() {
        let body = serde_json::json!({"enabled": false, "phase": "idle"});
        let result = format_dream_response(&DreamAction::Status, &body).unwrap();
        assert!(result.contains("Dream gate: disabled"));
        assert!(result.contains("Phase: idle"));
    }

    #[test]
    fn format_trigger() {
        let body = serde_json::json!({"status": "triggered"});
        let result = format_dream_response(&DreamAction::Trigger, &body).unwrap();
        assert_eq!(result, "Dream consolidation: triggered");
    }

    #[test]
    fn describe_status() {
        let desc = describe("http://localhost:18794", &DreamAction::Status);
        assert_eq!(desc, "GET http://localhost:18794/api/brain/dream/status");
    }

    #[test]
    fn describe_trigger() {
        let desc = describe("http://localhost:18794", &DreamAction::Trigger);
        assert_eq!(desc, "POST http://localhost:18794/api/brain/dream/trigger");
    }
}
