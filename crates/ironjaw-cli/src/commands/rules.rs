use crate::{CliError, RulesAction};
use serde_json::json;

/// Build the HTTP method, path, and optional body for a rules action.
pub fn build_request(action: &RulesAction) -> (&'static str, String, Option<serde_json::Value>) {
    match action {
        RulesAction::List => ("GET", "/api/security/rules".to_string(), None),
        RulesAction::Add {
            scope,
            pattern,
            decision,
        } => (
            "POST",
            "/api/security/rules".to_string(),
            Some(json!({
                "scope": scope,
                "pattern": pattern,
                "decision": decision,
            })),
        ),
        RulesAction::Remove { index } => ("DELETE", format!("/api/security/rules/{index}"), None),
        RulesAction::Test { command } => (
            "POST",
            "/api/security/rules/test".to_string(),
            Some(json!({ "command": command })),
        ),
    }
}

/// Execute a rules subcommand against the server.
pub async fn execute(
    server: &str,
    token: Option<&str>,
    json_output: bool,
    action: &RulesAction,
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
        format_rules_response(action, &resp_body)
    }
}

fn format_rules_response(
    action: &RulesAction,
    body: &serde_json::Value,
) -> Result<String, CliError> {
    match action {
        RulesAction::List => {
            let rules = body
                .get("rules")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if rules.is_empty() {
                return Ok("No permission rules configured.".to_string());
            }
            let mut lines = vec![format!("Permission rules ({}):", rules.len())];
            for (i, rule) in rules.iter().enumerate() {
                let scope = rule.get("scope").and_then(|v| v.as_str()).unwrap_or("?");
                let pattern = rule.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
                let decision = rule.get("decision").and_then(|v| v.as_str()).unwrap_or("?");
                lines.push(format!("  [{i}] {decision} {scope}:{pattern}"));
            }
            Ok(lines.join("\n"))
        }
        RulesAction::Add { pattern, .. } => Ok(format!("Rule added: {pattern}")),
        RulesAction::Remove { index } => Ok(format!("Rule removed: index {index}")),
        RulesAction::Test { command } => {
            let decision = body
                .get("decision")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let matched = body
                .get("matched_rule")
                .and_then(|v| v.as_str())
                .unwrap_or("none");
            Ok(format!(
                "Command: {command}\nDecision: {decision}\nMatched rule: {matched}"
            ))
        }
    }
}

/// Describe what the rules command would send.
pub fn describe(server: &str, action: &RulesAction) -> String {
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
        let (method, path, body) = build_request(&RulesAction::List);
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/security/rules");
        assert!(body.is_none());
    }

    #[test]
    fn build_add_request() {
        let (method, path, body) = build_request(&RulesAction::Add {
            scope: "project".to_string(),
            pattern: "shell(git *)".to_string(),
            decision: "allow".to_string(),
        });
        assert_eq!(method, "POST");
        assert_eq!(path, "/api/security/rules");
        let b = body.unwrap();
        assert_eq!(b["scope"], "project");
        assert_eq!(b["pattern"], "shell(git *)");
        assert_eq!(b["decision"], "allow");
    }

    #[test]
    fn build_remove_request() {
        let (method, path, body) = build_request(&RulesAction::Remove { index: 3 });
        assert_eq!(method, "DELETE");
        assert_eq!(path, "/api/security/rules/3");
        assert!(body.is_none());
    }

    #[test]
    fn build_test_request() {
        let (method, path, body) = build_request(&RulesAction::Test {
            command: "shell(rm -rf /)".to_string(),
        });
        assert_eq!(method, "POST");
        assert_eq!(path, "/api/security/rules/test");
        assert_eq!(body.unwrap()["command"], "shell(rm -rf /)");
    }

    #[test]
    fn format_list_empty() {
        let body = json!({"rules": []});
        let result = format_rules_response(&RulesAction::List, &body).unwrap();
        assert_eq!(result, "No permission rules configured.");
    }

    #[test]
    fn format_list_with_rules() {
        let body = json!({"rules": [
            {"scope": "project", "pattern": "shell(git *)", "decision": "allow"},
            {"scope": "global", "pattern": "shell(rm *)", "decision": "deny"},
        ]});
        let result = format_rules_response(&RulesAction::List, &body).unwrap();
        assert!(result.contains("Permission rules (2):"));
        assert!(result.contains("[0] allow project:shell(git *)"));
        assert!(result.contains("[1] deny global:shell(rm *)"));
    }

    #[test]
    fn format_add() {
        let body = json!({});
        let result = format_rules_response(
            &RulesAction::Add {
                scope: "project".to_string(),
                pattern: "shell(git *)".to_string(),
                decision: "allow".to_string(),
            },
            &body,
        )
        .unwrap();
        assert_eq!(result, "Rule added: shell(git *)");
    }

    #[test]
    fn format_remove() {
        let body = json!({});
        let result = format_rules_response(&RulesAction::Remove { index: 2 }, &body).unwrap();
        assert_eq!(result, "Rule removed: index 2");
    }

    #[test]
    fn format_test() {
        let body = json!({"decision": "deny", "matched_rule": "shell(rm *)"});
        let result = format_rules_response(
            &RulesAction::Test {
                command: "shell(rm -rf /)".to_string(),
            },
            &body,
        )
        .unwrap();
        assert!(result.contains("Decision: deny"));
        assert!(result.contains("Matched rule: shell(rm *)"));
    }

    #[test]
    fn describe_list() {
        let desc = describe("http://localhost:18794", &RulesAction::List);
        assert_eq!(desc, "GET http://localhost:18794/api/security/rules");
    }
}
