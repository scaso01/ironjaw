use crate::{CliError, ToolsAction};
use serde_json::json;

/// Build the HTTP method, path, and optional body for a tools action.
pub fn build_request(action: &ToolsAction) -> (&'static str, String, Option<serde_json::Value>) {
    match action {
        ToolsAction::List => ("GET", "/api/tools".to_string(), None),
        ToolsAction::Execute { name, args } => {
            let parsed_args: serde_json::Value =
                serde_json::from_str(args).unwrap_or_else(|_| json!({}));
            (
                "POST",
                format!("/api/tools/{name}/execute"),
                Some(json!({
                    "tool": name,
                    "args": parsed_args,
                })),
            )
        }
    }
}

/// Execute a tools subcommand against the server.
pub async fn execute(
    server: &str,
    token: Option<&str>,
    json_output: bool,
    action: &ToolsAction,
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
        format_tools_response(action, &resp_body)
    }
}

/// Format a tools response for human-readable output.
fn format_tools_response(
    action: &ToolsAction,
    body: &serde_json::Value,
) -> Result<String, CliError> {
    match action {
        ToolsAction::List => {
            let tools = body.as_array().unwrap_or(&Vec::new()).clone();
            if tools.is_empty() {
                return Ok("No tools registered.".to_string());
            }
            let mut lines = Vec::new();
            for t in &tools {
                let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let desc = t.get("description").and_then(|v| v.as_str()).unwrap_or("");
                lines.push(format!("  {name:20} {desc}"));
            }
            Ok(format!("Tools ({}):\n{}", tools.len(), lines.join("\n")))
        }
        ToolsAction::Execute { name, .. } => {
            let result = body
                .get("result")
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()))
                .unwrap_or_else(|| body.to_string());
            Ok(format!("Tool `{name}` result:\n{result}"))
        }
    }
}

/// Describe what the tools command would send (for dry-run / testing).
pub fn describe(server: &str, action: &ToolsAction) -> String {
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
        let (method, path, body) = build_request(&ToolsAction::List);
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/tools");
        assert!(body.is_none());
    }

    #[test]
    fn build_execute_request() {
        let (method, path, body) = build_request(&ToolsAction::Execute {
            name: "shell".to_string(),
            args: r#"{"cmd":"ls"}"#.to_string(),
        });
        assert_eq!(method, "POST");
        assert_eq!(path, "/api/tools/shell/execute");
        let b = body.unwrap();
        assert_eq!(b["tool"], "shell");
        assert_eq!(b["args"]["cmd"], "ls");
    }

    #[test]
    fn build_execute_invalid_json_args_defaults_empty() {
        let (_, _, body) = build_request(&ToolsAction::Execute {
            name: "test".to_string(),
            args: "not-json".to_string(),
        });
        let b = body.unwrap();
        assert_eq!(b["args"], json!({}));
    }

    #[test]
    fn format_list_empty() {
        let body = json!([]);
        let result = format_tools_response(&ToolsAction::List, &body).unwrap();
        assert_eq!(result, "No tools registered.");
    }

    #[test]
    fn format_list_with_tools() {
        let body = json!([
            {"name": "read_file", "description": "Read a file"},
            {"name": "shell", "description": "Run a command"},
        ]);
        let result = format_tools_response(&ToolsAction::List, &body).unwrap();
        assert!(result.contains("Tools (2):"));
        assert!(result.contains("read_file"));
        assert!(result.contains("shell"));
    }

    #[test]
    fn format_execute() {
        let body = json!({"result": {"output": "hello"}});
        let result = format_tools_response(
            &ToolsAction::Execute {
                name: "shell".to_string(),
                args: "{}".to_string(),
            },
            &body,
        )
        .unwrap();
        assert!(result.contains("Tool `shell` result:"));
        assert!(result.contains("hello"));
    }

    #[test]
    fn describe_list() {
        let desc = describe("http://localhost:18794", &ToolsAction::List);
        assert_eq!(desc, "GET http://localhost:18794/api/tools");
    }

    #[test]
    fn describe_execute() {
        let desc = describe(
            "http://localhost:18794",
            &ToolsAction::Execute {
                name: "shell".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            },
        );
        assert!(desc.starts_with("POST http://localhost:18794/api/tools/shell/execute"));
    }
}
