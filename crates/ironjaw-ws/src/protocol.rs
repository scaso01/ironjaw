use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// All frame types in Protocol v4.
/// Each variant corresponds to a JSON frame with a "type" field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// Client -> Server request
    Req {
        id: Uuid,
        method: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    /// Server -> Client response
    Res {
        id: Uuid,
        ok: bool,
        #[serde(rename = "payload", skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<ErrorPayload>,
    },
    /// Server -> Client streaming token
    Stream {
        id: Uuid,
        #[serde(skip_serializing_if = "Option::is_none")]
        delta: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        done: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<serde_json::Value>,
    },
    /// Server -> Client event
    Event {
        event: String,
        #[serde(rename = "payload", default)]
        data: serde_json::Value,
    },
    /// Bidirectional device command
    Cmd {
        id: Uuid,
        tool: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    /// Command response (bidirectional)
    #[serde(rename = "cmd.res")]
    CmdRes {
        id: Uuid,
        ok: bool,
        #[serde(rename = "payload", skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<ErrorPayload>,
        /// Approval scope for HITL permission responses.
        /// `"once"` (default) approves this single invocation.
        /// `"session"` approves the tool for the remainder of the session.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        scope: Option<String>,
    },
    /// Server -> Client: generative UI component (Phase 11)
    Ui {
        id: Uuid,
        component: String,
        #[serde(default)]
        schema: serde_json::Value,
    },
    /// Client -> Server: UI interaction response (Phase 11)
    #[serde(rename = "ui.res")]
    UiRes {
        id: Uuid,
        action: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

impl Frame {
    /// Create a successful response frame
    pub fn ok(id: Uuid, data: serde_json::Value) -> Self {
        Frame::Res {
            id,
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    /// Create an error response frame
    pub fn err(id: Uuid, code: impl Into<String>, message: impl Into<String>) -> Self {
        Frame::Res {
            id,
            ok: false,
            data: None,
            error: Some(ErrorPayload {
                code: code.into(),
                message: message.into(),
                retryable: false,
            }),
        }
    }

    /// Create a retryable error response frame
    pub fn retryable_err(id: Uuid, code: impl Into<String>, message: impl Into<String>) -> Self {
        Frame::Res {
            id,
            ok: false,
            data: None,
            error: Some(ErrorPayload {
                code: code.into(),
                message: message.into(),
                retryable: true,
            }),
        }
    }

    /// Create an event frame
    pub fn event(event: impl Into<String>, data: serde_json::Value) -> Self {
        Frame::Event {
            event: event.into(),
            data,
        }
    }

    /// Create a UI component frame
    pub fn ui(id: Uuid, component: impl Into<String>, schema: serde_json::Value) -> Self {
        Frame::Ui {
            id,
            component: component.into(),
            schema,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_req_frame() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let frame = Frame::Req {
            id,
            method: "auth.connect".to_string(),
            params: serde_json::json!({"token": "abc"}),
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""type":"req""#));
        assert!(json.contains(r#""method":"auth.connect""#));
    }

    #[test]
    fn test_deserialize_req_frame() {
        let json = r#"{
            "type": "req",
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "method": "chat.send",
            "params": {"message": "hello"}
        }"#;
        let frame: Frame = serde_json::from_str(json).unwrap();
        match frame {
            Frame::Req { method, params, .. } => {
                assert_eq!(method, "chat.send");
                assert_eq!(params["message"], "hello");
            }
            _ => panic!("Expected Req frame"),
        }
    }

    #[test]
    fn test_serialize_res_ok() {
        let id = Uuid::new_v4();
        let frame = Frame::ok(id, serde_json::json!({"status": "connected"}));
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""ok":true"#));
        assert!(!json.contains(r#""error""#)); // error field skipped
    }

    #[test]
    fn test_serialize_res_error() {
        let id = Uuid::new_v4();
        let frame = Frame::err(id, "AUTH_FAILED", "Invalid token");
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""ok":false"#));
        assert!(json.contains("AUTH_FAILED"));
        // Non-retryable errors should serialize retryable as false
        assert!(json.contains(r#""retryable":false"#));
    }

    #[test]
    fn test_serialize_retryable_error() {
        let id = Uuid::new_v4();
        let frame = Frame::retryable_err(id, "RATE_LIMITED", "Too many requests");
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""ok":false"#));
        assert!(json.contains("RATE_LIMITED"));
        assert!(json.contains(r#""retryable":true"#));
    }

    #[test]
    fn test_deserialize_error_without_retryable() {
        // Backward compat: old payloads without retryable field should default to false
        let json = r#"{
            "type": "res",
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "ok": false,
            "error": {"code": "OLD_ERROR", "message": "legacy"}
        }"#;
        let frame: Frame = serde_json::from_str(json).unwrap();
        match frame {
            Frame::Res { error, .. } => {
                let err = error.unwrap();
                assert_eq!(err.code, "OLD_ERROR");
                assert!(!err.retryable);
            }
            _ => panic!("Expected Res frame"),
        }
    }

    #[test]
    fn test_deserialize_stream_frame() {
        let json = r#"{
            "type": "stream",
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "delta": "Hello "
        }"#;
        let frame: Frame = serde_json::from_str(json).unwrap();
        match frame {
            Frame::Stream { delta, done, .. } => {
                assert_eq!(delta.unwrap(), "Hello ");
                assert!(done.is_none());
            }
            _ => panic!("Expected Stream frame"),
        }
    }

    #[test]
    fn test_deserialize_event_frame() {
        let json = r#"{
            "type": "event",
            "event": "tool.start",
            "payload": {"tool": "shell"}
        }"#;
        let frame: Frame = serde_json::from_str(json).unwrap();
        match frame {
            Frame::Event { event, data } => {
                assert_eq!(event, "tool.start");
                assert_eq!(data["tool"], "shell");
            }
            _ => panic!("Expected Event frame"),
        }
    }

    #[test]
    fn test_roundtrip_cmd_res() {
        let id = Uuid::new_v4();
        let frame = Frame::CmdRes {
            id,
            ok: true,
            data: Some(serde_json::json!({"result": "ok"})),
            error: None,
            scope: None,
        };
        let json = serde_json::to_string(&frame).unwrap();
        let parsed: Frame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, parsed);
    }

    #[test]
    fn test_cmd_res_scope_session_roundtrip() {
        let id = Uuid::new_v4();
        let frame = Frame::CmdRes {
            id,
            ok: true,
            data: None,
            error: None,
            scope: Some("session".to_string()),
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""scope":"session""#));
        let parsed: Frame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, parsed);
    }

    #[test]
    fn test_cmd_res_without_scope_defaults_to_none() {
        // Old clients that don't send scope should deserialize cleanly.
        let json = r#"{
            "type": "cmd.res",
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "ok": true
        }"#;
        let frame: Frame = serde_json::from_str(json).unwrap();
        match frame {
            Frame::CmdRes { scope, .. } => assert!(scope.is_none()),
            _ => panic!("Expected CmdRes frame"),
        }
    }

    #[test]
    fn test_unknown_type_fails() {
        let json = r#"{"type": "bogus", "id": "550e8400-e29b-41d4-a716-446655440000"}"#;
        let result: Result<Frame, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_ui_frame_roundtrip() {
        let id = Uuid::new_v4();
        let frame = Frame::ui(id, "confirm_card", serde_json::json!({"title": "Delete?"}));
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""type":"ui""#));
        assert!(json.contains(r#""component":"confirm_card""#));
        let parsed: Frame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, parsed);
    }

    #[test]
    fn test_ui_res_frame_roundtrip() {
        let json = r#"{
            "type": "ui.res",
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "action": "confirmed",
            "data": {"choice": "yes"}
        }"#;
        let frame: Frame = serde_json::from_str(json).unwrap();
        match frame {
            Frame::UiRes { action, data, .. } => {
                assert_eq!(action, "confirmed");
                assert_eq!(data.unwrap()["choice"], "yes");
            }
            _ => panic!("Expected UiRes frame"),
        }
    }
}
