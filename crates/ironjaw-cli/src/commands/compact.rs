use crate::{CliError, CompactAction};
use serde_json::json;

/// Build the HTTP method, path, and optional body for a compact action.
pub fn build_request(action: &CompactAction) -> (&'static str, String, Option<serde_json::Value>) {
    match action {
        CompactAction::Status => ("GET", "/api/context/status".to_string(), None),
        CompactAction::Micro => (
            "POST",
            "/api/context/compact".to_string(),
            Some(json!({ "strategy": "micro" })),
        ),
        CompactAction::Full => (
            "POST",
            "/api/context/compact".to_string(),
            Some(json!({ "strategy": "full" })),
        ),
    }
}

/// Execute a compact subcommand against the server.
pub async fn execute(
    server: &str,
    token: Option<&str>,
    json_output: bool,
    action: &CompactAction,
) -> Result<String, CliError> {
    let token = token.ok_or(CliError::AuthRequired)?;
    let (method, path, body) = build_request(action);
    let url = format!("{server}{path}");

    let client = reqwest::Client::new();
    let mut req = match method {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        _ => return Err(CliError::Http(format!("unsupported method: {method}"))),
    };
    req = req.bearer_auth(token);
    if let Some(b) = &body {
        req = req.json(b);
    }

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
        format_compact_response(action, &resp_body)
    }
}

fn format_compact_response(
    action: &CompactAction,
    body: &serde_json::Value,
) -> Result<String, CliError> {
    match action {
        CompactAction::Status => {
            let tokens = body
                .get("context_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let limit = body
                .get("context_limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let pct = if limit > 0 {
                (tokens as f64 / limit as f64) * 100.0
            } else {
                0.0
            };
            Ok(format!("Context: {tokens}/{limit} tokens ({pct:.1}%)"))
        }
        CompactAction::Micro | CompactAction::Full => {
            let strategy = match action {
                CompactAction::Micro => "micro",
                CompactAction::Full => "full",
                _ => "unknown",
            };
            let removed = body
                .get("tokens_removed")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Ok(format!("Compaction ({strategy}): removed {removed} tokens"))
        }
    }
}

/// Describe what the compact command would send.
pub fn describe(server: &str, action: &CompactAction) -> String {
    let (method, path, body) = build_request(action);
    match body {
        Some(b) => format!("{method} {server}{path}\n{b}"),
        None => format!("{method} {server}{path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_status_request() {
        let (method, path, body) = build_request(&CompactAction::Status);
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/context/status");
        assert!(body.is_none());
    }

    #[test]
    fn build_micro_request() {
        let (method, path, body) = build_request(&CompactAction::Micro);
        assert_eq!(method, "POST");
        assert_eq!(path, "/api/context/compact");
        assert_eq!(body.unwrap()["strategy"], "micro");
    }

    #[test]
    fn build_full_request() {
        let (method, path, body) = build_request(&CompactAction::Full);
        assert_eq!(method, "POST");
        assert_eq!(path, "/api/context/compact");
        assert_eq!(body.unwrap()["strategy"], "full");
    }

    #[test]
    fn format_status() {
        let body = json!({"context_tokens": 5000, "context_limit": 32768});
        let result = format_compact_response(&CompactAction::Status, &body).unwrap();
        assert!(result.contains("5000/32768"));
        assert!(result.contains("15.3%"));
    }

    #[test]
    fn format_micro_compact() {
        let body = json!({"tokens_removed": 1200});
        let result = format_compact_response(&CompactAction::Micro, &body).unwrap();
        assert!(result.contains("micro"));
        assert!(result.contains("1200"));
    }

    #[test]
    fn describe_status() {
        let desc = describe("http://localhost:18794", &CompactAction::Status);
        assert_eq!(desc, "GET http://localhost:18794/api/context/status");
    }
}
