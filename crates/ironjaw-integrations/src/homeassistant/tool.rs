use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Home Assistant integration tool.
/// Uses HA REST API with long-lived access token.
pub struct HomeAssistantTool {
    base_url: String,
    token: String,
    client: reqwest::Client,
}

impl HomeAssistantTool {
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for HomeAssistantTool {
    fn name(&self) -> &str {
        "home_assistant"
    }

    fn description(&self) -> &str {
        "Control Home Assistant — list entities, get state, call services"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list_entities", "get_state", "call_service"],
                    "description": "Action to perform"
                },
                "entity_id": {
                    "type": "string",
                    "description": "Entity ID (required for get_state and call_service)"
                },
                "service": {
                    "type": "string",
                    "description": "Service to call, e.g. 'light.turn_on' (required for call_service)"
                },
                "data": {
                    "type": "object",
                    "description": "Additional data for call_service"
                },
                "domain": {
                    "type": "string",
                    "description": "Filter list_entities by domain (e.g. 'light', 'switch')"
                }
            }
        })
    }

    fn tier(&self) -> PermissionTier {
        // Dynamic: read actions are Auto, call_service is Confirm
        PermissionTier::Confirm
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let action = match params.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => return ToolResult::error("INVALID_PARAMS", "action is required"),
        };

        match action {
            "list_entities" => self.list_entities(&params).await,
            "get_state" => self.get_state(&params).await,
            "call_service" => self.call_service(&params).await,
            _ => ToolResult::error("INVALID_PARAMS", format!("Unknown action: {action}")),
        }
    }
}

impl HomeAssistantTool {
    async fn list_entities(&self, params: &serde_json::Value) -> ToolResult {
        let url = format!("{}/api/states", self.base_url);
        let domain_filter = params.get("domain").and_then(|v| v.as_str());

        match self.ha_get(&url).await {
            Ok(body) => {
                let states: Vec<serde_json::Value> = match serde_json::from_str(&body) {
                    Ok(v) => v,
                    Err(e) => {
                        return ToolResult::error(
                            "PARSE_ERROR",
                            format!("Failed to parse HA response: {e}"),
                        )
                    }
                };

                let entities: Vec<serde_json::Value> = states
                    .into_iter()
                    .filter(|s| {
                        if let Some(domain) = domain_filter {
                            s.get("entity_id")
                                .and_then(|v| v.as_str())
                                .map(|id| id.starts_with(&format!("{domain}.")))
                                .unwrap_or(false)
                        } else {
                            true
                        }
                    })
                    .map(|s| {
                        serde_json::json!({
                            "entity_id": s.get("entity_id"),
                            "state": s.get("state"),
                            "friendly_name": s.get("attributes")
                                .and_then(|a| a.get("friendly_name")),
                        })
                    })
                    .collect();

                ToolResult::json(serde_json::json!({
                    "count": entities.len(),
                    "entities": entities,
                }))
            }
            Err(e) => ToolResult::error("HA_ERROR", e),
        }
    }

    async fn get_state(&self, params: &serde_json::Value) -> ToolResult {
        let entity_id = match params.get("entity_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return ToolResult::error("INVALID_PARAMS", "entity_id is required for get_state")
            }
        };

        let url = format!("{}/api/states/{}", self.base_url, entity_id);

        match self.ha_get(&url).await {
            Ok(body) => {
                let state: serde_json::Value = match serde_json::from_str(&body) {
                    Ok(v) => v,
                    Err(e) => {
                        return ToolResult::error(
                            "PARSE_ERROR",
                            format!("Failed to parse HA response: {e}"),
                        )
                    }
                };
                ToolResult::json(state)
            }
            Err(e) => ToolResult::error("HA_ERROR", e),
        }
    }

    async fn call_service(&self, params: &serde_json::Value) -> ToolResult {
        let service = match params.get("service").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return ToolResult::error(
                    "INVALID_PARAMS",
                    "service is required for call_service (e.g. 'light.turn_on')",
                )
            }
        };

        // Parse "domain.service" format
        let parts: Vec<&str> = service.splitn(2, '.').collect();
        if parts.len() != 2 {
            return ToolResult::error(
                "INVALID_PARAMS",
                "service must be in 'domain.action' format (e.g. 'light.turn_on')",
            );
        }
        let (domain, action) = (parts[0], parts[1]);

        let entity_id = params.get("entity_id").and_then(|v| v.as_str());
        let extra_data = params.get("data").cloned().unwrap_or(serde_json::json!({}));

        let mut body = extra_data.as_object().cloned().unwrap_or_default();
        if let Some(eid) = entity_id {
            body.insert("entity_id".to_string(), serde_json::json!(eid));
        }

        let url = format!("{}/api/services/{}/{}", self.base_url, domain, action);

        match self.ha_post(&url, &serde_json::Value::Object(body)).await {
            Ok(resp_body) => {
                let result: serde_json::Value =
                    serde_json::from_str(&resp_body).unwrap_or(serde_json::json!({"status": "ok"}));
                ToolResult::json(serde_json::json!({
                    "service": service,
                    "entity_id": entity_id,
                    "result": result,
                }))
            }
            Err(e) => ToolResult::error("HA_ERROR", e),
        }
    }

    async fn ha_get(&self, url: &str) -> Result<String, String> {
        let resp = self
            .client
            .get(url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("HA request failed: {e}"))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read HA response: {e}"))?;

        if !status.is_success() {
            return Err(format!("HA returned {status} — {body}"));
        }

        Ok(body)
    }

    async fn ha_post(&self, url: &str, body: &serde_json::Value) -> Result<String, String> {
        let resp = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .json(body)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("HA request failed: {e}"))?;

        let status = resp.status();
        let resp_body = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read HA response: {e}"))?;

        if !status.is_success() {
            return Err(format!("HA returned {status} — {resp_body}"));
        }

        Ok(resp_body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ha_tool_metadata() {
        let tool = HomeAssistantTool::new("http://ha.local:8123", "test-token");
        assert_eq!(tool.name(), "home_assistant");
        assert_eq!(tool.tier(), PermissionTier::Confirm);
    }

    #[test]
    fn test_ha_tool_schema() {
        let tool = HomeAssistantTool::new("http://ha.local:8123", "test-token");
        let schema = tool.schema();
        assert_eq!(schema["properties"]["action"]["enum"][0], "list_entities");
        assert_eq!(schema["properties"]["action"]["enum"][1], "get_state");
        assert_eq!(schema["properties"]["action"]["enum"][2], "call_service");
    }

    #[tokio::test]
    async fn test_ha_missing_action() {
        let tool = HomeAssistantTool::new("http://ha.local:8123", "test-token");
        let ctx = ToolContext::new("test", "test", std::path::PathBuf::from("."));
        let result = tool
            .execute(serde_json::json!({}), &ctx, CancellationToken::new())
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_ha_unknown_action() {
        let tool = HomeAssistantTool::new("http://ha.local:8123", "test-token");
        let ctx = ToolContext::new("test", "test", std::path::PathBuf::from("."));
        let result = tool
            .execute(
                serde_json::json!({"action": "reboot"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_ha_get_state_missing_entity() {
        let tool = HomeAssistantTool::new("http://ha.local:8123", "test-token");
        let ctx = ToolContext::new("test", "test", std::path::PathBuf::from("."));
        let result = tool
            .execute(
                serde_json::json!({"action": "get_state"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_ha_call_service_missing_service() {
        let tool = HomeAssistantTool::new("http://ha.local:8123", "test-token");
        let ctx = ToolContext::new("test", "test", std::path::PathBuf::from("."));
        let result = tool
            .execute(
                serde_json::json!({"action": "call_service"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_ha_call_service_bad_format() {
        let tool = HomeAssistantTool::new("http://ha.local:8123", "test-token");
        let ctx = ToolContext::new("test", "test", std::path::PathBuf::from("."));
        let result = tool
            .execute(
                serde_json::json!({"action": "call_service", "service": "nodot"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }
}
