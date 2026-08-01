use crate::CliError;

/// Build the URL for the health endpoint.
fn health_url(server: &str) -> String {
    format!("{server}/health")
}

/// Execute the status command: hit /health and format the response.
pub async fn execute(server: &str, _token: Option<&str>, json: bool) -> Result<String, CliError> {
    let url = health_url(server);

    let resp = reqwest::get(&url)
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

    if json {
        serde_json::to_string_pretty(&body).map_err(|e| CliError::Parse(e.to_string()))
    } else {
        let status_field = body
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let uptime = body.get("uptime").and_then(|v| v.as_u64()).unwrap_or(0);
        Ok(format!("Status: {status_field}\nUptime: {uptime}s"))
    }
}

/// Build the request description (for dry-run / testing).
pub fn describe(server: &str) -> String {
    format!("GET {server}/health")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_url_default() {
        assert_eq!(
            health_url("http://localhost:18794"),
            "http://localhost:18794/health"
        );
    }

    #[test]
    fn health_url_custom() {
        assert_eq!(
            health_url("http://192.0.2.10:9999"),
            "http://192.0.2.10:9999/health"
        );
    }

    #[test]
    fn describe_output() {
        let desc = describe("http://localhost:18794");
        assert_eq!(desc, "GET http://localhost:18794/health");
    }
}
