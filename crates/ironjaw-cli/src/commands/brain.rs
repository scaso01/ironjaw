use crate::{BrainAction, CliError};
use serde_json::json;

/// Build the HTTP method, path, and optional body for a brain action.
pub fn build_request(action: &BrainAction) -> (&'static str, String, Option<serde_json::Value>) {
    match action {
        BrainAction::Remember { content } => (
            "POST",
            "/api/brain/memories".to_string(),
            Some(json!({ "content": content })),
        ),
        BrainAction::Recall { query } => (
            "GET",
            format!("/api/brain/memories?q={}", urlencoded(query)),
            None,
        ),
        BrainAction::Forget { id } => ("DELETE", format!("/api/brain/memories/{id}"), None),
    }
}

/// Minimal URL encoding for query parameters.
fn urlencoded(s: &str) -> String {
    s.replace(' ', "+")
        .replace('&', "%26")
        .replace('=', "%3D")
        .replace('?', "%3F")
        .replace('#', "%23")
}

/// Execute a brain subcommand against the server.
pub async fn execute(
    server: &str,
    token: Option<&str>,
    json_output: bool,
    action: &BrainAction,
) -> Result<String, CliError> {
    let token = token.ok_or(CliError::AuthRequired)?;
    let (method, path, body) = build_request(action);
    let url = format!("{server}{path}");

    let client = reqwest::Client::new();
    let mut req = match method {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "DELETE" => client.delete(&url),
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
        format_brain_response(action, &resp_body)
    }
}

/// Format a brain response for human-readable output.
fn format_brain_response(
    action: &BrainAction,
    body: &serde_json::Value,
) -> Result<String, CliError> {
    match action {
        BrainAction::Remember { .. } => {
            let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
            Ok(format!("Remembered (id: {id})"))
        }
        BrainAction::Recall { .. } => {
            let memories = body.as_array().unwrap_or(&Vec::new()).clone();
            if memories.is_empty() {
                return Ok("No memories found.".to_string());
            }
            let mut lines = Vec::new();
            for m in &memories {
                let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                let content = m
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect::<String>();
                lines.push(format!("  [{id}] {content}"));
            }
            Ok(format!(
                "Memories ({}):\n{}",
                memories.len(),
                lines.join("\n")
            ))
        }
        BrainAction::Forget { id } => Ok(format!("Forgot memory: {id}")),
    }
}

/// Describe what the brain command would send (for dry-run / testing).
pub fn describe(server: &str, action: &BrainAction) -> String {
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
    fn build_remember_request() {
        let (method, path, body) = build_request(&BrainAction::Remember {
            content: "hello world".to_string(),
        });
        assert_eq!(method, "POST");
        assert_eq!(path, "/api/brain/memories");
        assert_eq!(body.unwrap()["content"], "hello world");
    }

    #[test]
    fn build_recall_request() {
        let (method, path, body) = build_request(&BrainAction::Recall {
            query: "cat mat".to_string(),
        });
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/brain/memories?q=cat+mat");
        assert!(body.is_none());
    }

    #[test]
    fn build_forget_request() {
        let (method, path, body) = build_request(&BrainAction::Forget {
            id: "mem-7".to_string(),
        });
        assert_eq!(method, "DELETE");
        assert_eq!(path, "/api/brain/memories/mem-7");
        assert!(body.is_none());
    }

    #[test]
    fn urlencoded_spaces() {
        assert_eq!(urlencoded("hello world"), "hello+world");
    }

    #[test]
    fn urlencoded_special_chars() {
        assert_eq!(urlencoded("a&b=c?d#e"), "a%26b%3Dc%3Fd%23e");
    }

    #[test]
    fn format_remember() {
        let body = json!({"id": "mem-1"});
        let result = format_brain_response(
            &BrainAction::Remember {
                content: "x".to_string(),
            },
            &body,
        )
        .unwrap();
        assert_eq!(result, "Remembered (id: mem-1)");
    }

    #[test]
    fn format_recall_empty() {
        let body = json!([]);
        let result = format_brain_response(
            &BrainAction::Recall {
                query: "x".to_string(),
            },
            &body,
        )
        .unwrap();
        assert_eq!(result, "No memories found.");
    }

    #[test]
    fn format_recall_with_memories() {
        let body = json!([
            {"id": "m1", "content": "the cat"},
            {"id": "m2", "content": "the dog"},
        ]);
        let result = format_brain_response(
            &BrainAction::Recall {
                query: "x".to_string(),
            },
            &body,
        )
        .unwrap();
        assert!(result.contains("Memories (2):"));
        assert!(result.contains("[m1] the cat"));
        assert!(result.contains("[m2] the dog"));
    }

    #[test]
    fn format_forget() {
        let body = json!({});
        let result = format_brain_response(
            &BrainAction::Forget {
                id: "mem-9".to_string(),
            },
            &body,
        )
        .unwrap();
        assert_eq!(result, "Forgot memory: mem-9");
    }

    #[test]
    fn describe_remember() {
        let desc = describe(
            "http://localhost:18794",
            &BrainAction::Remember {
                content: "test".to_string(),
            },
        );
        assert!(desc.starts_with("POST http://localhost:18794/api/brain/memories"));
        assert!(desc.contains("test"));
    }

    #[test]
    fn describe_recall() {
        let desc = describe(
            "http://localhost:18794",
            &BrainAction::Recall {
                query: "cats".to_string(),
            },
        );
        assert_eq!(desc, "GET http://localhost:18794/api/brain/memories?q=cats");
    }

    #[test]
    fn format_recall_truncates_long_content() {
        let long = "a".repeat(200);
        let body = json!([{"id": "m1", "content": long}]);
        let result = format_brain_response(
            &BrainAction::Recall {
                query: "x".to_string(),
            },
            &body,
        )
        .unwrap();
        // The content should be truncated to 80 chars
        let line = result.lines().nth(1).unwrap();
        // [m1] + 80 chars = 86 chars total (with leading spaces)
        assert!(line.len() <= 90);
    }
}
