use crate::CliError;
use serde_json::json;

/// Build the JSON payload for a chat.send request.
pub fn build_payload(
    message: &str,
    session: Option<&str>,
    model: Option<&str>,
) -> serde_json::Value {
    let mut params = json!({
        "message": message,
    });
    if let Some(s) = session {
        params["session_id"] = json!(s);
    }
    if let Some(m) = model {
        params["model"] = json!(m);
    }
    json!({
        "type": "req",
        "method": "chat.send",
        "params": params,
    })
}

/// Execute the chat command via HTTP POST to /api/chat.
pub async fn execute(
    server: &str,
    token: Option<&str>,
    json_output: bool,
    message: &str,
    session: Option<&str>,
    model: Option<&str>,
) -> Result<String, CliError> {
    let token = token.ok_or(CliError::AuthRequired)?;
    let url = format!("{server}/api/chat");
    let payload = build_payload(message, session, model);

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .json(&payload)
        .send()
        .await
        .map_err(|e| CliError::Http(e.to_string()))?;

    let status_code = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| CliError::Parse(e.to_string()))?;

    if !status_code.is_success() {
        return Err(CliError::Server(format!(
            "{status_code}: {}",
            serde_json::to_string(&body).unwrap_or_default()
        )));
    }

    if json_output {
        serde_json::to_string_pretty(&body).map_err(|e| CliError::Parse(e.to_string()))
    } else {
        let reply = body
            .get("data")
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or_default();
        Ok(reply.to_string())
    }
}

/// Describe what the chat command would send (for dry-run / testing).
pub fn describe(server: &str, message: &str, session: Option<&str>, model: Option<&str>) -> String {
    let payload = build_payload(message, session, model);
    format!("POST {server}/api/chat\n{payload}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_minimal() {
        let p = build_payload("hello", None, None);
        assert_eq!(p["method"], "chat.send");
        assert_eq!(p["params"]["message"], "hello");
        assert!(p["params"].get("session_id").is_none());
        assert!(p["params"].get("model").is_none());
    }

    #[test]
    fn payload_with_session() {
        let p = build_payload("hi", Some("s-1"), None);
        assert_eq!(p["params"]["session_id"], "s-1");
    }

    #[test]
    fn payload_with_model() {
        let p = build_payload("hi", None, Some("qwen3:30b"));
        assert_eq!(p["params"]["model"], "qwen3:30b");
    }

    #[test]
    fn payload_with_all_options() {
        let p = build_payload("test", Some("sess"), Some("gpt-4"));
        assert_eq!(p["params"]["message"], "test");
        assert_eq!(p["params"]["session_id"], "sess");
        assert_eq!(p["params"]["model"], "gpt-4");
    }

    #[test]
    fn describe_output() {
        let desc = describe("http://localhost:18794", "hi", None, None);
        assert!(desc.starts_with("POST http://localhost:18794/api/chat"));
        assert!(desc.contains("chat.send"));
    }
}
