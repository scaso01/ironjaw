use crate::{CliError, SessionAction};
use serde_json::json;

/// Build the HTTP method, path suffix, and optional JSON body for a session action.
pub fn build_request(action: &SessionAction) -> (&'static str, String, Option<serde_json::Value>) {
    match action {
        SessionAction::List => ("GET", "/api/sessions".to_string(), None),
        SessionAction::Get { id } => ("GET", format!("/api/sessions/{id}"), None),
        SessionAction::Create { name } => (
            "POST",
            "/api/sessions".to_string(),
            Some(json!({ "name": name })),
        ),
        SessionAction::Delete { id } => ("DELETE", format!("/api/sessions/{id}"), None),
        SessionAction::Rename { id, name } => (
            "PATCH",
            format!("/api/sessions/{id}"),
            Some(json!({ "name": name })),
        ),
    }
}

/// Execute a session subcommand against the server.
pub async fn execute(
    server: &str,
    token: Option<&str>,
    json_output: bool,
    action: &SessionAction,
) -> Result<String, CliError> {
    let token = token.ok_or(CliError::AuthRequired)?;
    let (method, path, body) = build_request(action);
    let url = format!("{server}{path}");

    let client = reqwest::Client::new();
    let mut req = match method {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "DELETE" => client.delete(&url),
        "PATCH" => client.patch(&url),
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
        format_session_response(action, &resp_body)
    }
}

/// Format a session response for human-readable output.
fn format_session_response(
    action: &SessionAction,
    body: &serde_json::Value,
) -> Result<String, CliError> {
    match action {
        SessionAction::List => {
            let sessions = body.as_array().unwrap_or(&Vec::new()).clone();
            if sessions.is_empty() {
                return Ok("No sessions found.".to_string());
            }
            let mut lines = Vec::new();
            for s in &sessions {
                let id = s.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed");
                lines.push(format!("  {id}  {name}"));
            }
            Ok(format!(
                "Sessions ({}):\n{}",
                sessions.len(),
                lines.join("\n")
            ))
        }
        SessionAction::Get { .. } => {
            let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            let name = body
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unnamed");
            Ok(format!("Session: {id}\nName: {name}"))
        }
        SessionAction::Create { .. } => {
            let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            Ok(format!("Created session: {id}"))
        }
        SessionAction::Delete { id } => Ok(format!("Deleted session: {id}")),
        SessionAction::Rename { id, name } => Ok(format!("Renamed session {id} to \"{name}\"")),
    }
}

/// Describe what the session command would send (for dry-run / testing).
pub fn describe(server: &str, action: &SessionAction) -> String {
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
    fn build_list_request() {
        let (method, path, body) = build_request(&SessionAction::List);
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/sessions");
        assert!(body.is_none());
    }

    #[test]
    fn build_get_request() {
        let (method, path, body) = build_request(&SessionAction::Get {
            id: "abc".to_string(),
        });
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/sessions/abc");
        assert!(body.is_none());
    }

    #[test]
    fn build_create_request() {
        let (method, path, body) = build_request(&SessionAction::Create {
            name: "test".to_string(),
        });
        assert_eq!(method, "POST");
        assert_eq!(path, "/api/sessions");
        assert_eq!(body.unwrap()["name"], "test");
    }

    #[test]
    fn build_delete_request() {
        let (method, path, body) = build_request(&SessionAction::Delete {
            id: "x".to_string(),
        });
        assert_eq!(method, "DELETE");
        assert_eq!(path, "/api/sessions/x");
        assert!(body.is_none());
    }

    #[test]
    fn build_rename_request() {
        let (method, path, body) = build_request(&SessionAction::Rename {
            id: "s1".to_string(),
            name: "new".to_string(),
        });
        assert_eq!(method, "PATCH");
        assert_eq!(path, "/api/sessions/s1");
        assert_eq!(body.unwrap()["name"], "new");
    }

    #[test]
    fn format_list_empty() {
        let body = json!([]);
        let result = format_session_response(&SessionAction::List, &body).unwrap();
        assert_eq!(result, "No sessions found.");
    }

    #[test]
    fn format_list_with_sessions() {
        let body = json!([
            {"id": "a", "name": "alpha"},
            {"id": "b", "name": "beta"},
        ]);
        let result = format_session_response(&SessionAction::List, &body).unwrap();
        assert!(result.contains("Sessions (2):"));
        assert!(result.contains("a  alpha"));
        assert!(result.contains("b  beta"));
    }

    #[test]
    fn format_get() {
        let body = json!({"id": "s1", "name": "my-session"});
        let result = format_session_response(
            &SessionAction::Get {
                id: "s1".to_string(),
            },
            &body,
        )
        .unwrap();
        assert!(result.contains("Session: s1"));
        assert!(result.contains("Name: my-session"));
    }

    #[test]
    fn format_create() {
        let body = json!({"id": "new-id"});
        let result = format_session_response(
            &SessionAction::Create {
                name: "test".to_string(),
            },
            &body,
        )
        .unwrap();
        assert_eq!(result, "Created session: new-id");
    }

    #[test]
    fn format_delete() {
        let body = json!({});
        let result = format_session_response(
            &SessionAction::Delete {
                id: "rm-1".to_string(),
            },
            &body,
        )
        .unwrap();
        assert_eq!(result, "Deleted session: rm-1");
    }

    #[test]
    fn format_rename() {
        let body = json!({});
        let result = format_session_response(
            &SessionAction::Rename {
                id: "s1".to_string(),
                name: "renamed".to_string(),
            },
            &body,
        )
        .unwrap();
        assert_eq!(result, "Renamed session s1 to \"renamed\"");
    }

    #[test]
    fn describe_list() {
        let desc = describe("http://localhost:18794", &SessionAction::List);
        assert_eq!(desc, "GET http://localhost:18794/api/sessions");
    }

    #[test]
    fn describe_create() {
        let desc = describe(
            "http://localhost:18794",
            &SessionAction::Create {
                name: "foo".to_string(),
            },
        );
        assert!(desc.starts_with("POST http://localhost:18794/api/sessions"));
        assert!(desc.contains("foo"));
    }
}
