use serde::{Deserialize, Serialize};

/// Configuration for the Tauri desktop application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TauriAppConfig {
    /// Window title.
    #[serde(default = "default_title")]
    pub title: String,
    /// Window width in pixels.
    #[serde(default = "default_width")]
    pub width: u32,
    /// Window height in pixels.
    #[serde(default = "default_height")]
    pub height: u32,
    /// WebSocket URL to connect to.
    #[serde(default = "default_ws_url")]
    pub ws_url: String,
    /// Whether the window should stay on top.
    #[serde(default)]
    pub always_on_top: bool,
    /// Whether to start minimized to tray.
    #[serde(default)]
    pub start_minimized: bool,
}

fn default_title() -> String {
    "Ironjaw".to_string()
}
fn default_width() -> u32 {
    1200
}
fn default_height() -> u32 {
    800
}
fn default_ws_url() -> String {
    "ws://127.0.0.1:18794".to_string()
}

impl Default for TauriAppConfig {
    fn default() -> Self {
        Self {
            title: default_title(),
            width: default_width(),
            height: default_height(),
            ws_url: default_ws_url(),
            always_on_top: false,
            start_minimized: false,
        }
    }
}

/// Prepared window configuration for the Tauri webview window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowConfig {
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub url: String,
    pub decorations: bool,
    pub resizable: bool,
}

/// The Tauri application wrapper.
/// This is a scaffold — actual Tauri integration will be added when the tauri
/// dependency is enabled.
pub struct TauriApp {
    config: TauriAppConfig,
}

impl TauriApp {
    pub fn new(config: TauriAppConfig) -> Self {
        Self { config }
    }

    /// Get the app configuration.
    pub fn config(&self) -> &TauriAppConfig {
        &self.config
    }

    /// Build the window configuration from the app config.
    pub fn build_window_config(&self) -> WindowConfig {
        WindowConfig {
            title: self.config.title.clone(),
            width: self.config.width,
            height: self.config.height,
            url: self.config.ws_url.clone(),
            decorations: true,
            resizable: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = TauriAppConfig::default();
        assert_eq!(config.title, "Ironjaw");
        assert_eq!(config.width, 1200);
        assert_eq!(config.height, 800);
        assert_eq!(config.ws_url, "ws://127.0.0.1:18794");
        assert!(!config.always_on_top);
        assert!(!config.start_minimized);
    }

    #[test]
    fn test_app_new() {
        let config = TauriAppConfig::default();
        let app = TauriApp::new(config.clone());
        assert_eq!(app.config().title, config.title);
    }

    #[test]
    fn test_build_window_config() {
        let config = TauriAppConfig {
            title: "Test App".to_string(),
            width: 800,
            height: 600,
            ws_url: "ws://localhost:9999".to_string(),
            always_on_top: true,
            start_minimized: false,
        };
        let app = TauriApp::new(config);
        let wc = app.build_window_config();

        assert_eq!(wc.title, "Test App");
        assert_eq!(wc.width, 800);
        assert_eq!(wc.height, 600);
        assert_eq!(wc.url, "ws://localhost:9999");
        assert!(wc.decorations);
        assert!(wc.resizable);
    }

    #[test]
    fn test_config_serde_roundtrip() {
        let config = TauriAppConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: TauriAppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.title, config.title);
        assert_eq!(parsed.width, config.width);
        assert_eq!(parsed.height, config.height);
    }

    #[test]
    fn test_config_deserialize_partial() {
        let json = r#"{"title": "Custom"}"#;
        let config: TauriAppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.title, "Custom");
        // Other fields use defaults
        assert_eq!(config.width, 1200);
        assert_eq!(config.height, 800);
    }

    #[test]
    fn test_window_config_serde() {
        let wc = WindowConfig {
            title: "Ironjaw".to_string(),
            width: 1200,
            height: 800,
            url: "ws://127.0.0.1:18794".to_string(),
            decorations: true,
            resizable: true,
        };
        let json = serde_json::to_string(&wc).unwrap();
        assert!(json.contains("Ironjaw"));
        let parsed: WindowConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.title, wc.title);
    }
}
