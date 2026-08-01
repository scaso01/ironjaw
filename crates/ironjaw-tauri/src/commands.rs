use crate::app::TauriAppConfig;

/// Get the current gateway status.
/// Stub for `#[tauri::command]` — will be decorated when tauri dep is added.
pub fn get_status() -> Result<serde_json::Value, String> {
    Ok(serde_json::json!({
        "status": "ok",
        "connected": false,
        "message": "Stub: not connected to gateway"
    }))
}

/// Connect to the Ironjaw WebSocket gateway.
/// Stub for `#[tauri::command]`.
pub fn connect_ws(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("WebSocket URL cannot be empty".to_string());
    }
    if !url.starts_with("ws://") && !url.starts_with("wss://") {
        return Err(format!("Invalid WebSocket URL scheme: {url}"));
    }
    // Stub: actual connection will be implemented with tauri.
    Ok(())
}

/// Send a chat message to the gateway.
/// Stub for `#[tauri::command]`.
pub fn send_message(msg: &str) -> Result<serde_json::Value, String> {
    if msg.is_empty() {
        return Err("Message cannot be empty".to_string());
    }
    Ok(serde_json::json!({
        "sent": true,
        "echo": msg,
        "message": "Stub: message not actually delivered"
    }))
}

/// Get the current application configuration.
/// Stub for `#[tauri::command]`.
pub fn get_config() -> Result<TauriAppConfig, String> {
    Ok(TauriAppConfig::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_status() {
        let result = get_status().unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(result["connected"], false);
    }

    #[test]
    fn test_connect_ws_valid() {
        assert!(connect_ws("ws://127.0.0.1:18794").is_ok());
        assert!(connect_ws("wss://example.com/ws").is_ok());
    }

    #[test]
    fn test_connect_ws_empty() {
        let err = connect_ws("").unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn test_connect_ws_invalid_scheme() {
        let err = connect_ws("http://localhost").unwrap_err();
        assert!(err.contains("Invalid WebSocket URL"));
    }

    #[test]
    fn test_send_message() {
        let result = send_message("hello").unwrap();
        assert_eq!(result["sent"], true);
        assert_eq!(result["echo"], "hello");
    }

    #[test]
    fn test_send_message_empty() {
        let err = send_message("").unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn test_get_config() {
        let config = get_config().unwrap();
        assert_eq!(config.title, "Ironjaw");
        assert_eq!(config.width, 1200);
        assert_eq!(config.height, 800);
    }
}
