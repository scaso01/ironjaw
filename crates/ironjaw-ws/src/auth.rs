use crate::protocol::Frame;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Constant-time string comparison to prevent timing attacks on token validation.
/// Always compares all bytes regardless of where a mismatch occurs.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    if a_bytes.len() != b_bytes.len() {
        // Still do a dummy comparison to avoid leaking length info via timing
        let mut _acc: u8 = 0;
        for &byte in a_bytes {
            _acc |= byte;
        }
        return false;
    }
    let mut result: u8 = 0;
    for (x, y) in a_bytes.iter().zip(b_bytes.iter()) {
        result |= x ^ y;
    }
    result == 0
}

#[derive(Debug, Deserialize)]
pub struct AuthConnectParams {
    pub token: String,
    #[serde(default)]
    pub device: Option<DeviceInfo>,
    /// Optional session ID to resume an existing session
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub name: Option<String>,
    pub platform: Option<String>,
    pub app_version: Option<String>,
}

/// Result of authentication attempt
#[derive(Debug)]
pub struct AuthResult {
    pub authenticated: bool,
    pub session_id: String,
    pub device_id: Option<String>,
}

/// Validate an auth.connect request, create or resume session, return response frame
pub async fn handle_auth_connect(
    request_id: Uuid,
    params: serde_json::Value,
    state: &AppState,
) -> (Frame, AuthResult) {
    // Parse params -- try direct format first, then OpenClaw nested format (params.auth.token)
    let normalized = if params.get("token").is_some() {
        params.clone()
    } else if let Some(token) = params
        .get("auth")
        .and_then(|a| a.get("token"))
        .and_then(|t| t.as_str())
    {
        // OpenClaw compat: extract token from nested auth object
        let mut p = params.clone();
        p["token"] = serde_json::Value::String(token.to_string());
        // Carry over device and session_id if present at top level
        p
    } else {
        params.clone()
    };

    let auth_params: AuthConnectParams = match serde_json::from_value(normalized) {
        Ok(p) => p,
        Err(e) => {
            warn!("Auth params parse error: {}", e);
            let frame = Frame::err(
                request_id,
                "INVALID_PARAMS",
                format!("Invalid auth parameters: {e}"),
            );
            let result = AuthResult {
                authenticated: false,
                session_id: String::new(),
                device_id: None,
            };
            return (frame, result);
        }
    };

    if !constant_time_eq(&auth_params.token, &state.auth_token) {
        warn!("Auth failed: invalid token");
        let frame = Frame::err(request_id, "AUTH_FAILED", "Invalid token");
        let result = AuthResult {
            authenticated: false,
            session_id: String::new(),
            device_id: None,
        };
        return (frame, result);
    }

    let device_name = auth_params
        .device
        .as_ref()
        .and_then(|d| d.name.clone())
        .unwrap_or_else(|| "unknown".to_string());

    // Create or resume session
    let (session_id, resumed) = if let Some(ref requested_id) = auth_params.session_id {
        // Try to resume existing session
        match state.session.store.get_session(requested_id).await {
            Ok(Some(session)) => {
                info!(
                    session_id = %session.id,
                    device = %device_name,
                    "Resuming existing session"
                );
                let _ = state.session.store.touch_session(&session.id).await;
                (session.id, true)
            }
            _ => {
                // Session not found — create new one
                let new_id = Uuid::new_v4().to_string();
                match state
                    .session
                    .store
                    .create_session(&new_id, "default", None)
                    .await
                {
                    Ok(_) => {
                        info!(
                            session_id = %new_id,
                            requested = %requested_id,
                            device = %device_name,
                            "Requested session not found, created new session"
                        );
                        (new_id, false)
                    }
                    Err(e) => {
                        warn!("Failed to create session: {}", e);
                        let frame =
                            Frame::err(request_id, "STORE_ERROR", "Failed to create session");
                        let result = AuthResult {
                            authenticated: false,
                            session_id: String::new(),
                            device_id: None,
                        };
                        return (frame, result);
                    }
                }
            }
        }
    } else {
        // No session_id requested — try to resume the most recent session first,
        // only create a new one if none exists. This prevents session proliferation
        // when clients reconnect without sending session_id.
        let existing = state
            .session
            .store
            .list_sessions(1, 0)
            .await
            .unwrap_or_default();
        if let Some(latest) = existing.first() {
            info!(
                session_id = %latest.id,
                device = %device_name,
                "Resuming most recent session (no session_id provided)"
            );
            let _ = state.session.store.touch_session(&latest.id).await;
            (latest.id.clone(), true)
        } else {
            let new_id = Uuid::new_v4().to_string();
            match state
                .session
                .store
                .create_session(&new_id, "default", None)
                .await
            {
                Ok(_) => {
                    info!(
                        session_id = %new_id,
                        device = %device_name,
                        "Client authenticated, new session created (no prior sessions)"
                    );
                    (new_id, false)
                }
                Err(e) => {
                    warn!("Failed to create session: {}", e);
                    let frame = Frame::err(request_id, "STORE_ERROR", "Failed to create session");
                    let result = AuthResult {
                        authenticated: false,
                        session_id: String::new(),
                        device_id: None,
                    };
                    return (frame, result);
                }
            }
        }
    };

    // Refresh identity context on reconnect so brain picks up any changes
    // to model config, brain files, or identity entries since the session started.
    if resumed {
        match state.brain.engine.bootstrap_from_files().await {
            Ok(report) => {
                debug!(
                    session_id = %session_id,
                    layers = report.layers_loaded,
                    tokens = report.total_tokens,
                    "Identity refreshed on reconnect for session {}", session_id
                );
            }
            Err(e) => {
                warn!(
                    session_id = %session_id,
                    error = %e,
                    "Failed to refresh identity on reconnect"
                );
            }
        }
    }

    let frame = Frame::ok(
        request_id,
        serde_json::json!({
            "protocol": 4,
            "server": {
                "name": "Ironjaw",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "features": {},
            "snapshot": {
                "sessionDefaults": {
                    "mainSessionKey": session_id,
                    "defaultAgentId": null,
                }
            },
            "policy": {
                "maxPayload": 65536,
                "maxBufferedBytes": 1048576,
                "tickIntervalMs": 15000,
            },
            "auth": {},
            // Ironjaw extras (backward compat)
            "session_id": session_id,
            "resumed": resumed,
            "device_acknowledged": device_name,
        }),
    );

    let result = AuthResult {
        authenticated: true,
        session_id,
        device_id: auth_params.device.and_then(|d| d.name),
    };

    (frame, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_auth_params() {
        let json = serde_json::json!({
            "token": "my-token",
            "device": {
                "name": "s22-ultra",
                "platform": "android",
                "app_version": "0.9.3"
            }
        });
        let params: AuthConnectParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.token, "my-token");
        assert_eq!(params.device.unwrap().name.unwrap(), "s22-ultra");
    }

    #[test]
    fn test_parse_auth_params_minimal() {
        let json = serde_json::json!({
            "token": "my-token"
        });
        let params: AuthConnectParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.token, "my-token");
        assert!(params.device.is_none());
    }

    #[test]
    fn test_parse_auth_params_missing_token() {
        let json = serde_json::json!({});
        let result: Result<AuthConnectParams, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_auth_params_with_session_id() {
        let json = serde_json::json!({
            "token": "my-token",
            "session_id": "existing-session-123"
        });
        let params: AuthConnectParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.session_id, Some("existing-session-123".to_string()));
    }
}
