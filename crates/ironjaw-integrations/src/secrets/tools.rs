use crate::secrets::vault::{SecretCategory, SecretsVault};
use async_trait::async_trait;
use ironjaw_security::PermissionTier;
use ironjaw_tools::{Tool, ToolContext, ToolResult};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Retrieve a secret from the encrypted vault.
pub struct SecretsGetTool {
    vault: Arc<SecretsVault>,
}

impl SecretsGetTool {
    pub fn new(vault: Arc<SecretsVault>) -> Self {
        Self { vault }
    }
}

#[async_trait]
impl Tool for SecretsGetTool {
    fn name(&self) -> &str {
        "secrets_get"
    }

    fn description(&self) -> &str {
        "Retrieve a secret by key from the encrypted vault"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The secret key to retrieve"
                }
            },
            "required": ["key"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::Confirm
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let key = match params.get("key").and_then(|v| v.as_str()) {
            Some(k) => k,
            None => return ToolResult::error("INVALID_PARAMS", "Missing required parameter: key"),
        };

        match self.vault.get(key).await {
            Ok(Some((value, category))) => ToolResult::json(serde_json::json!({
                "key": key,
                "value": value,
                "category": category,
            })),
            Ok(None) => ToolResult::error("NOT_FOUND", format!("Secret '{key}' not found")),
            Err(e) => ToolResult::error("VAULT_ERROR", format!("Failed to retrieve secret: {e}")),
        }
    }
}

/// Store or update an encrypted secret in the vault.
pub struct SecretsSetTool {
    vault: Arc<SecretsVault>,
}

impl SecretsSetTool {
    pub fn new(vault: Arc<SecretsVault>) -> Self {
        Self { vault }
    }
}

#[async_trait]
impl Tool for SecretsSetTool {
    fn name(&self) -> &str {
        "secrets_set"
    }

    fn description(&self) -> &str {
        "Store or update an encrypted secret in the vault"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The secret key name"
                },
                "value": {
                    "type": "string",
                    "description": "The secret value to store (will be encrypted)"
                },
                "category": {
                    "type": "string",
                    "enum": ["system", "tool", "user"],
                    "description": "Secret category (default: user)"
                }
            },
            "required": ["key", "value"]
        })
    }

    fn tier(&self) -> PermissionTier {
        PermissionTier::MultiConfirm
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let key = match params.get("key").and_then(|v| v.as_str()) {
            Some(k) => k,
            None => return ToolResult::error("INVALID_PARAMS", "Missing required parameter: key"),
        };
        let value = match params.get("value").and_then(|v| v.as_str()) {
            Some(v) => v,
            None => {
                return ToolResult::error("INVALID_PARAMS", "Missing required parameter: value")
            }
        };
        let category = params
            .get("category")
            .and_then(|v| v.as_str())
            .and_then(SecretCategory::from_str_value)
            .unwrap_or(SecretCategory::User);

        match self.vault.set(key, value, category).await {
            Ok(()) => ToolResult::json(serde_json::json!({
                "key": key,
                "category": category,
                "stored": true,
            })),
            Err(e) => ToolResult::error("VAULT_ERROR", format!("Failed to store secret: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_secrets_get_tool() {
        let vault = Arc::new(SecretsVault::open(":memory:").await.unwrap());
        vault
            .set("test_key", "test_value", SecretCategory::User)
            .await
            .unwrap();

        let tool = SecretsGetTool::new(vault);
        assert_eq!(tool.name(), "secrets_get");
        assert_eq!(tool.tier(), PermissionTier::Confirm);

        let ctx = ToolContext::new("test", "test", std::path::PathBuf::from("."));

        let result = tool
            .execute(
                serde_json::json!({"key": "test_key"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());
    }

    #[tokio::test]
    async fn test_secrets_set_tool() {
        let vault = Arc::new(SecretsVault::open(":memory:").await.unwrap());

        let tool = SecretsSetTool::new(vault.clone());
        assert_eq!(tool.name(), "secrets_set");
        assert_eq!(tool.tier(), PermissionTier::MultiConfirm);

        let ctx = ToolContext::new("test", "test", std::path::PathBuf::from("."));

        let result = tool
            .execute(
                serde_json::json!({"key": "new_key", "value": "new_value"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error());

        let result = vault.get("new_key").await.unwrap();
        assert_eq!(
            result,
            Some(("new_value".to_string(), SecretCategory::User))
        );
    }

    #[tokio::test]
    async fn test_secrets_get_not_found() {
        let vault = Arc::new(SecretsVault::open(":memory:").await.unwrap());
        let tool = SecretsGetTool::new(vault);

        let ctx = ToolContext::new("test", "test", std::path::PathBuf::from("."));

        let result = tool
            .execute(
                serde_json::json!({"key": "missing"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error());
    }
}
