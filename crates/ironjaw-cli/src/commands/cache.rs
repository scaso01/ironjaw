use crate::{CacheAction, CliError};

/// Build the HTTP method, path, and optional body for a cache action.
pub fn build_request(action: &CacheAction) -> (&'static str, String, Option<serde_json::Value>) {
    match action {
        CacheAction::Stats => ("GET", "/api/cache/stats".to_string(), None),
    }
}

/// Execute a cache subcommand against the server.
pub async fn execute(
    server: &str,
    token: Option<&str>,
    json_output: bool,
    action: &CacheAction,
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
        format_cache_response(action, &resp_body)
    }
}

fn format_cache_response(
    action: &CacheAction,
    body: &serde_json::Value,
) -> Result<String, CliError> {
    match action {
        CacheAction::Stats => {
            let entries = body.get("entries").and_then(|v| v.as_u64()).unwrap_or(0);
            let max_entries = body
                .get("max_entries")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let hits = body.get("hits").and_then(|v| v.as_u64()).unwrap_or(0);
            let misses = body.get("misses").and_then(|v| v.as_u64()).unwrap_or(0);
            let total = hits + misses;
            let hit_rate = if total > 0 {
                (hits as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            Ok(format!(
                "Prompt cache: {entries}/{max_entries} entries\nHits: {hits}, Misses: {misses} ({hit_rate:.1}% hit rate)"
            ))
        }
    }
}

/// Describe what the cache command would send.
pub fn describe(server: &str, action: &CacheAction) -> String {
    let (method, path, _body) = build_request(action);
    format!("{method} {server}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_stats_request() {
        let (method, path, body) = build_request(&CacheAction::Stats);
        assert_eq!(method, "GET");
        assert_eq!(path, "/api/cache/stats");
        assert!(body.is_none());
    }

    #[test]
    fn format_stats() {
        let body = serde_json::json!({
            "entries": 5,
            "max_entries": 10,
            "hits": 80,
            "misses": 20
        });
        let result = format_cache_response(&CacheAction::Stats, &body).unwrap();
        assert!(result.contains("5/10 entries"));
        assert!(result.contains("Hits: 80"));
        assert!(result.contains("80.0% hit rate"));
    }

    #[test]
    fn format_stats_no_requests() {
        let body = serde_json::json!({
            "entries": 0,
            "max_entries": 10,
            "hits": 0,
            "misses": 0
        });
        let result = format_cache_response(&CacheAction::Stats, &body).unwrap();
        assert!(result.contains("0/10 entries"));
        assert!(result.contains("0.0% hit rate"));
    }

    #[test]
    fn describe_stats() {
        let desc = describe("http://localhost:18794", &CacheAction::Stats);
        assert_eq!(desc, "GET http://localhost:18794/api/cache/stats");
    }
}
