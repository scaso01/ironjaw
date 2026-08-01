use crate::types::{Tool, ToolContext, ToolResult};
use ironjaw_security::PermissionTier;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

const DEFAULT_API_URL: &str = "http://ip-api.com/json";

/// Geographic location information for an IP address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationInfo {
    #[serde(alias = "query")]
    pub ip: String,
    pub city: String,
    #[serde(alias = "regionName")]
    pub region: String,
    pub country: String,
    pub lat: f64,
    pub lon: f64,
    pub timezone: String,
    pub isp: String,
}

/// IP geolocation tool using ip-api.com or compatible endpoint.
pub struct LocationTool {
    api_url: String,
}

impl Default for LocationTool {
    fn default() -> Self {
        Self {
            api_url: DEFAULT_API_URL.to_string(),
        }
    }
}

impl LocationTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_api_url(url: impl Into<String>) -> Self {
        Self {
            api_url: url.into(),
        }
    }

    /// Fetch location info. If `ip` is provided, look up that address; otherwise
    /// look up the caller's own IP.
    pub async fn get_location(&self, ip: Option<&str>) -> Result<LocationInfo, ToolError> {
        let url = match ip {
            Some(addr) => format!("{}/{addr}", self.api_url),
            None => self.api_url.clone(),
        };

        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| ToolError::Network(format!("Request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::Api(format!("API returned {status}: {body}")));
        }

        let info: LocationInfo = resp
            .json()
            .await
            .map_err(|e| ToolError::Parse(format!("Failed to parse response: {e}")))?;

        Ok(info)
    }
}

/// Errors specific to location lookup.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Network error: {0}")]
    Network(String),
    #[error("API error: {0}")]
    Api(String),
    #[error("Parse error: {0}")]
    Parse(String),
}

#[async_trait::async_trait]
impl Tool for LocationTool {
    fn name(&self) -> &str {
        "location"
    }

    fn description(&self) -> &str {
        "Look up geographic location for an IP address"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "ip": {
                    "type": "string",
                    "description": "IP address to look up (omit for own IP)"
                }
            }
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let ip = params.get("ip").and_then(|v| v.as_str());

        match self.get_location(ip).await {
            Ok(info) => match serde_json::to_value(&info) {
                Ok(v) => ToolResult::json(v),
                Err(e) => {
                    ToolResult::error("SERIALIZE_FAILED", format!("Failed to serialize: {e}"))
                }
            },
            Err(e) => ToolResult::error("LOCATION_FAILED", e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_ctx() -> ToolContext {
        ToolContext::new("test", "test", PathBuf::from("/tmp"))
    }

    #[test]
    fn test_response_parsing() {
        let json = serde_json::json!({
            "query": "8.8.8.8",
            "city": "Mountain View",
            "regionName": "California",
            "country": "United States",
            "lat": 37.3861,
            "lon": -122.0839,
            "timezone": "America/Los_Angeles",
            "isp": "Google LLC"
        });
        let info: LocationInfo = serde_json::from_value(json).unwrap();
        assert_eq!(info.ip, "8.8.8.8");
        assert_eq!(info.city, "Mountain View");
        assert_eq!(info.region, "California");
        assert_eq!(info.country, "United States");
        assert!((info.lat - 37.3861).abs() < 0.001);
        assert!((info.lon - (-122.0839)).abs() < 0.001);
        assert_eq!(info.timezone, "America/Los_Angeles");
        assert_eq!(info.isp, "Google LLC");
    }

    #[test]
    fn test_response_with_direct_field_names() {
        let json = serde_json::json!({
            "ip": "1.1.1.1",
            "city": "Sydney",
            "region": "NSW",
            "country": "Australia",
            "lat": -33.8688,
            "lon": 151.2093,
            "timezone": "Australia/Sydney",
            "isp": "Cloudflare"
        });
        let info: LocationInfo = serde_json::from_value(json).unwrap();
        assert_eq!(info.ip, "1.1.1.1");
        assert_eq!(info.region, "NSW");
    }

    #[test]
    fn test_serialization() {
        let info = LocationInfo {
            ip: "192.0.2.10".to_string(),
            city: "Local".to_string(),
            region: "LAN".to_string(),
            country: "Internal".to_string(),
            lat: 0.0,
            lon: 0.0,
            timezone: "UTC".to_string(),
            isp: "Private".to_string(),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["ip"], "192.0.2.10");
        assert_eq!(json["city"], "Local");
        assert_eq!(json["lat"], 0.0);
    }

    #[test]
    fn test_optional_ip_url_construction() {
        let tool = LocationTool::new();
        // Without IP — uses base URL
        assert_eq!(tool.api_url, DEFAULT_API_URL);

        // With custom URL
        let tool = LocationTool::with_api_url("http://custom:9999/json");
        assert_eq!(tool.api_url, "http://custom:9999/json");
    }

    #[tokio::test]
    async fn test_error_on_bad_endpoint() {
        let tool = LocationTool::with_api_url("http://127.0.0.1:1/bad");
        let result = tool.get_location(None).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::Network(_) => {}
            other => panic!("Expected Network error, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_error_on_bad_endpoint_with_ip() {
        let tool = LocationTool::with_api_url("http://127.0.0.1:1/bad");
        let result = tool.get_location(Some("8.8.8.8")).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_tool_metadata() {
        let tool = LocationTool::new();
        assert_eq!(tool.name(), "location");
        assert_eq!(tool.tier(), PermissionTier::Auto);
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn test_schema_structure() {
        let tool = LocationTool::new();
        let schema = tool.schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["ip"].is_object());
    }

    #[tokio::test]
    async fn test_execute_no_ip_bad_endpoint() {
        let tool = LocationTool::with_api_url("http://127.0.0.1:1/bad");
        let result = tool
            .execute(serde_json::json!({}), &test_ctx(), CancellationToken::new())
            .await;
        assert!(result.is_error());
        match result {
            ToolResult::Error { code, .. } => assert_eq!(code, "LOCATION_FAILED"),
            _ => panic!("Expected Error result"),
        }
    }

    #[tokio::test]
    async fn test_execute_with_ip_bad_endpoint() {
        let tool = LocationTool::with_api_url("http://127.0.0.1:1/bad");
        let result = tool
            .execute(
                serde_json::json!({"ip": "8.8.8.8"}),
                &test_ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[test]
    fn test_default_api_url() {
        let tool = LocationTool::new();
        assert_eq!(tool.api_url, "http://ip-api.com/json");
    }
}
