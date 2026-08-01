use serde::{Deserialize, Serialize};

/// Configuration for the system tray icon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrayConfig {
    /// Whether to show the tray icon.
    #[serde(default = "default_show_tray")]
    pub show_tray: bool,
    /// Tooltip text shown on hover.
    #[serde(default = "default_tooltip")]
    pub tooltip: String,
}

fn default_show_tray() -> bool {
    true
}
fn default_tooltip() -> String {
    "Ironjaw Gateway".to_string()
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            show_tray: default_show_tray(),
            tooltip: default_tooltip(),
        }
    }
}

/// Menu items available in the system tray context menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrayMenuItem {
    /// Shows the current connection status.
    Status,
    /// Opens the dashboard in the main window.
    Dashboard,
    /// Visual separator line.
    Separator,
    /// Exits the application.
    Quit,
}

impl TrayMenuItem {
    /// Human-readable label for the menu item.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Dashboard => "Open Dashboard",
            Self::Separator => "---",
            Self::Quit => "Quit",
        }
    }

    /// Whether this item is a separator (non-interactive).
    pub fn is_separator(&self) -> bool {
        matches!(self, Self::Separator)
    }
}

/// Current tray icon state reflecting connection health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrayState {
    Connected,
    Disconnected,
    Error(String),
}

impl TrayState {
    /// Tooltip suffix for this state.
    pub fn tooltip_suffix(&self) -> String {
        match self {
            Self::Connected => " - Connected".to_string(),
            Self::Disconnected => " - Disconnected".to_string(),
            Self::Error(msg) => format!(" - Error: {msg}"),
        }
    }

    /// Whether the gateway is reachable.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// Build the default tray context menu.
pub fn build_tray_menu() -> Vec<TrayMenuItem> {
    vec![
        TrayMenuItem::Status,
        TrayMenuItem::Dashboard,
        TrayMenuItem::Separator,
        TrayMenuItem::Quit,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tray_config_defaults() {
        let config = TrayConfig::default();
        assert!(config.show_tray);
        assert_eq!(config.tooltip, "Ironjaw Gateway");
    }

    #[test]
    fn test_build_tray_menu() {
        let menu = build_tray_menu();
        assert_eq!(menu.len(), 4);
        assert_eq!(menu[0], TrayMenuItem::Status);
        assert_eq!(menu[1], TrayMenuItem::Dashboard);
        assert_eq!(menu[2], TrayMenuItem::Separator);
        assert_eq!(menu[3], TrayMenuItem::Quit);
    }

    #[test]
    fn test_menu_item_labels() {
        assert_eq!(TrayMenuItem::Status.label(), "Status");
        assert_eq!(TrayMenuItem::Dashboard.label(), "Open Dashboard");
        assert_eq!(TrayMenuItem::Separator.label(), "---");
        assert_eq!(TrayMenuItem::Quit.label(), "Quit");
    }

    #[test]
    fn test_separator_detection() {
        assert!(TrayMenuItem::Separator.is_separator());
        assert!(!TrayMenuItem::Status.is_separator());
        assert!(!TrayMenuItem::Quit.is_separator());
    }

    #[test]
    fn test_tray_state_connected() {
        let state = TrayState::Connected;
        assert!(state.is_connected());
        assert!(state.tooltip_suffix().contains("Connected"));
    }

    #[test]
    fn test_tray_state_disconnected() {
        let state = TrayState::Disconnected;
        assert!(!state.is_connected());
        assert!(state.tooltip_suffix().contains("Disconnected"));
    }

    #[test]
    fn test_tray_state_error() {
        let state = TrayState::Error("timeout".to_string());
        assert!(!state.is_connected());
        assert!(state.tooltip_suffix().contains("timeout"));
    }

    #[test]
    fn test_tray_config_serde() {
        let config = TrayConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: TrayConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.show_tray, config.show_tray);
        assert_eq!(parsed.tooltip, config.tooltip);
    }
}
