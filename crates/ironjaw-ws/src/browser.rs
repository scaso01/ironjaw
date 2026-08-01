// Stream 8.9: Browser Automation
//
// Lightweight CDP (Chrome DevTools Protocol) client for browser automation.
// Uses reqwest (HTTP) + tokio-tungstenite (WebSocket) directly instead of
// heavy external crates like chromiumoxide or headless_chrome.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Mutex;

// ── Error Types ─────────────────────────────────────────────────────────────

/// All browser automation error cases.
#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("Browser not found at path: {0}")]
    BrowserNotFound(String),

    #[error("Failed to launch browser: {0}")]
    LaunchFailed(String),

    #[error("CDP connection failed: {0}")]
    ConnectionFailed(String),

    #[error("CDP command failed: {method} - {message}")]
    CommandFailed { method: String, message: String },

    #[error("Navigation failed: {0}")]
    NavigationFailed(String),

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Invalid selector: {0}")]
    InvalidSelector(String),

    #[error("Element not found: {0}")]
    ElementNotFound(String),

    #[error("JavaScript evaluation failed: {0}")]
    JsEvalFailed(String),

    #[error("Screenshot failed: {0}")]
    ScreenshotFailed(String),

    #[error("Timeout after {0}ms")]
    Timeout(u64),

    #[error("Browser not connected")]
    NotConnected,

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("JSON parse error: {0}")]
    JsonParse(String),

    #[error("Security violation: {0}")]
    SecurityViolation(String),
}

impl From<reqwest::Error> for BrowserError {
    fn from(e: reqwest::Error) -> Self {
        BrowserError::Http(e.to_string())
    }
}

impl From<serde_json::Error> for BrowserError {
    fn from(e: serde_json::Error) -> Self {
        BrowserError::JsonParse(e.to_string())
    }
}

// ── Configuration ───────────────────────────────────────────────────────────

/// Browser automation configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserConfig {
    /// Path to Chrome/Chromium executable.
    /// If None, attempts to find Chrome in standard locations.
    pub chrome_path: Option<String>,

    /// Run in headless mode (default: true).
    pub headless: bool,

    /// Default navigation timeout in milliseconds (default: 30000).
    pub timeout_ms: u64,

    /// Viewport width in pixels (default: 1280).
    pub viewport_width: u32,

    /// Viewport height in pixels (default: 720).
    pub viewport_height: u32,

    /// CDP debugging port (default: 9222).
    pub debug_port: u16,

    /// User agent string override. If None, uses Chrome default.
    pub user_agent: Option<String>,

    /// Maximum number of concurrent pages (default: 5).
    pub max_pages: usize,

    /// Allowed URL schemes for navigation (default: ["http", "https"]).
    pub allowed_schemes: Vec<String>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            chrome_path: None,
            headless: true,
            timeout_ms: 30_000,
            viewport_width: 1280,
            viewport_height: 720,
            debug_port: 9222,
            user_agent: None,
            max_pages: 5,
            allowed_schemes: vec!["http".to_string(), "https".to_string()],
        }
    }
}

impl BrowserConfig {
    /// Validate the configuration, returning errors for invalid values.
    pub fn validate(&self) -> Result<(), BrowserError> {
        if self.viewport_width == 0 || self.viewport_height == 0 {
            return Err(BrowserError::LaunchFailed(
                "Viewport dimensions must be non-zero".to_string(),
            ));
        }
        if self.viewport_width > 7680 || self.viewport_height > 4320 {
            return Err(BrowserError::LaunchFailed(
                "Viewport dimensions exceed 8K resolution limit".to_string(),
            ));
        }
        if self.timeout_ms == 0 {
            return Err(BrowserError::LaunchFailed(
                "Timeout must be greater than zero".to_string(),
            ));
        }
        if self.max_pages == 0 {
            return Err(BrowserError::LaunchFailed(
                "max_pages must be greater than zero".to_string(),
            ));
        }
        if self.allowed_schemes.is_empty() {
            return Err(BrowserError::SecurityViolation(
                "At least one URL scheme must be allowed".to_string(),
            ));
        }
        Ok(())
    }
}

// ── Page Info ───────────────────────────────────────────────────────────────

/// Information about a loaded page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageInfo {
    /// The current URL of the page.
    pub url: String,
    /// The page title.
    pub title: String,
    /// HTTP status code (0 if unknown).
    pub status: u16,
}

impl PageInfo {
    /// Create a new PageInfo.
    pub fn new(url: impl Into<String>, title: impl Into<String>, status: u16) -> Self {
        Self {
            url: url.into(),
            title: title.into(),
            status,
        }
    }
}

// ── CDP Message Types ───────────────────────────────────────────────────────

/// A CDP command to send over WebSocket.
#[derive(Debug, Serialize)]
struct CdpCommand {
    id: u64,
    method: String,
    params: serde_json::Value,
}

/// A CDP response received over WebSocket.
#[derive(Debug, Deserialize)]
struct CdpResponse {
    id: Option<u64>,
    result: Option<serde_json::Value>,
    error: Option<CdpErrorInfo>,
}

/// CDP error detail.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CdpErrorInfo {
    #[serde(default)]
    code: i64,
    message: String,
}

/// Target info from /json/list endpoint.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct CdpTarget {
    #[serde(default)]
    id: String,
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    web_socket_debugger_url: String,
}

// ── Input Validation ────────────────────────────────────────────────────────

/// Validate a URL for navigation, checking scheme and basic format.
fn validate_url(url: &str, allowed_schemes: &[String]) -> Result<(), BrowserError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(BrowserError::InvalidUrl("URL cannot be empty".to_string()));
    }

    // Extract scheme: try "://" first (standard URLs), then ":" (javascript:, data:, etc.)
    let (scheme, has_authority) = if let Some(pos) = url.find("://") {
        (&url[..pos], true)
    } else if let Some(pos) = url.find(':') {
        (&url[..pos], false)
    } else {
        return Err(BrowserError::InvalidUrl(format!(
            "URL must include a scheme (e.g., http:// or https://): {url}"
        )));
    };

    if !allowed_schemes.iter().any(|s| s.as_str() == scheme) {
        return Err(BrowserError::InvalidUrl(format!(
            "URL scheme '{scheme}' is not allowed. Allowed: {}",
            allowed_schemes.join(", ")
        )));
    }

    // Standard URLs (with ://) must have a host after the authority separator
    if has_authority {
        let after_scheme = &url[scheme.len() + 3..]; // skip "://"
        if after_scheme.is_empty() || after_scheme.starts_with('/') {
            return Err(BrowserError::InvalidUrl(format!(
                "URL must include a host: {url}"
            )));
        }
    }

    Ok(())
}

/// Sanitize a CSS selector to prevent injection.
/// Rejects selectors containing potentially dangerous patterns.
fn sanitize_selector(selector: &str) -> Result<String, BrowserError> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Err(BrowserError::InvalidSelector(
            "Selector cannot be empty".to_string(),
        ));
    }

    // Reject extremely long selectors (potential DoS)
    if selector.len() > 1024 {
        return Err(BrowserError::InvalidSelector(
            "Selector exceeds maximum length of 1024 characters".to_string(),
        ));
    }

    // Reject selectors containing JavaScript-executable content
    let dangerous_patterns = [
        "javascript:",
        "expression(",
        "eval(",
        "url(",
        "import(",
        "\\00",
        "\\x00",
    ];
    let lower = selector.to_lowercase();
    for pattern in &dangerous_patterns {
        if lower.contains(pattern) {
            return Err(BrowserError::InvalidSelector(format!(
                "Selector contains forbidden pattern: {pattern}"
            )));
        }
    }

    Ok(selector.to_string())
}

/// Sanitize JavaScript code to prevent dangerous operations.
/// This is a defense-in-depth measure; the real security boundary is
/// the Confirm permission tier.
fn sanitize_js(script: &str) -> Result<String, BrowserError> {
    let script = script.trim();
    if script.is_empty() {
        return Err(BrowserError::JsEvalFailed(
            "Script cannot be empty".to_string(),
        ));
    }

    // Reject extremely long scripts (potential DoS)
    if script.len() > 64 * 1024 {
        return Err(BrowserError::JsEvalFailed(
            "Script exceeds maximum length of 64KB".to_string(),
        ));
    }

    // Block certain dangerous patterns
    let blocked_patterns = [
        // File system access
        "require('fs')",
        "require(\"fs\")",
        "require('child_process')",
        "require(\"child_process\")",
        // Network exfiltration via fetch to file://
        "file://",
    ];
    let lower = script.to_lowercase();
    for pattern in &blocked_patterns {
        if lower.contains(&pattern.to_lowercase()) {
            return Err(BrowserError::SecurityViolation(format!(
                "Script contains blocked pattern: {pattern}"
            )));
        }
    }

    Ok(script.to_string())
}

// ── CDP Connection ──────────────────────────────────────────────────────────

/// Internal state for a CDP WebSocket connection.
/// In production, this holds a real WebSocket; in tests, it can be mocked.
#[allow(dead_code)]
struct CdpConnectionInner {
    /// WebSocket sender (Option to allow disconnect).
    ws_sender: Option<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tokio_tungstenite::tungstenite::Message,
        >,
    >,
    /// WebSocket receiver.
    ws_receiver: Option<
        futures_util::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    >,
    /// Command ID counter.
    next_id: AtomicU64,
    /// Current page URL.
    current_url: String,
    /// Current page title.
    current_title: String,
}

/// A CDP connection that sends commands and receives responses.
pub struct CdpConnection {
    inner: Mutex<CdpConnectionInner>,
    config: BrowserConfig,
}

impl CdpConnection {
    /// Create a new CDP connection (not yet connected).
    fn new(config: BrowserConfig) -> Self {
        Self {
            inner: Mutex::new(CdpConnectionInner {
                ws_sender: None,
                ws_receiver: None,
                next_id: AtomicU64::new(1),
                current_url: String::new(),
                current_title: String::new(),
            }),
            config,
        }
    }

    /// Connect to the browser's CDP WebSocket endpoint.
    async fn connect(&self, ws_url: &str) -> Result<(), BrowserError> {
        use futures_util::StreamExt;

        let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| BrowserError::ConnectionFailed(e.to_string()))?;

        let (sender, receiver) = ws_stream.split();
        let mut inner = self.inner.lock().await;
        inner.ws_sender = Some(sender);
        inner.ws_receiver = Some(receiver);

        Ok(())
    }

    /// Send a CDP command and wait for its response.
    async fn send_command(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, BrowserError> {
        use futures_util::{SinkExt, StreamExt};

        let mut inner = self.inner.lock().await;
        let id = inner.next_id.fetch_add(1, Ordering::SeqCst);

        let cmd = CdpCommand {
            id,
            method: method.to_string(),
            params,
        };
        let msg_text =
            serde_json::to_string(&cmd).map_err(|e| BrowserError::JsonParse(e.to_string()))?;

        let sender = inner.ws_sender.as_mut().ok_or(BrowserError::NotConnected)?;
        sender
            .send(tokio_tungstenite::tungstenite::Message::Text(
                msg_text.into(),
            ))
            .await
            .map_err(|e| BrowserError::WebSocket(e.to_string()))?;

        // Read messages until we find our response
        let receiver = inner
            .ws_receiver
            .as_mut()
            .ok_or(BrowserError::NotConnected)?;

        let timeout = Duration::from_millis(self.config.timeout_ms);
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let msg = tokio::time::timeout_at(deadline, receiver.next())
                .await
                .map_err(|_| BrowserError::Timeout(self.config.timeout_ms))?
                .ok_or_else(|| BrowserError::WebSocket("Connection closed".to_string()))?
                .map_err(|e| BrowserError::WebSocket(e.to_string()))?;

            if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                let resp: CdpResponse = serde_json::from_str(&text)
                    .map_err(|e| BrowserError::JsonParse(e.to_string()))?;

                if resp.id == Some(id) {
                    if let Some(error) = resp.error {
                        return Err(BrowserError::CommandFailed {
                            method: method.to_string(),
                            message: error.message,
                        });
                    }
                    return Ok(resp.result.unwrap_or(serde_json::Value::Null));
                }
                // Not our response -- skip events and other responses
            }
        }
    }

    /// Check if connected.
    async fn is_connected(&self) -> bool {
        let inner = self.inner.lock().await;
        inner.ws_sender.is_some()
    }
}

// ── Browser Automation ──────────────────────────────────────────────────────

/// Main browser automation struct.
/// Manages a Chrome/Chromium browser instance via CDP.
pub struct BrowserAutomation {
    config: BrowserConfig,
    connection: Option<Arc<CdpConnection>>,
    /// Browser child process handle.
    #[allow(dead_code)]
    process: Option<tokio::process::Child>,
}

impl BrowserAutomation {
    /// Create a new browser automation instance with the given config.
    pub fn new(config: BrowserConfig) -> Result<Self, BrowserError> {
        config.validate()?;
        Ok(Self {
            config,
            connection: None,
            process: None,
        })
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Result<Self, BrowserError> {
        Self::new(BrowserConfig::default())
    }

    /// Get the current configuration.
    pub fn config(&self) -> &BrowserConfig {
        &self.config
    }

    /// Whether the browser is currently connected.
    pub async fn is_connected(&self) -> bool {
        match &self.connection {
            Some(conn) => conn.is_connected().await,
            None => false,
        }
    }

    /// Launch Chrome and connect via CDP.
    pub async fn launch(&mut self) -> Result<(), BrowserError> {
        self.config.validate()?;

        let chrome_path = self
            .config
            .chrome_path
            .clone()
            .or_else(find_chrome)
            .ok_or_else(|| {
                BrowserError::BrowserNotFound(
                    "Chrome not found. Set chrome_path in config.".to_string(),
                )
            })?;

        // Check that the executable exists
        if !std::path::Path::new(&chrome_path).exists() {
            return Err(BrowserError::BrowserNotFound(chrome_path));
        }

        let mut cmd = tokio::process::Command::new(&chrome_path);
        cmd.arg(format!(
            "--remote-debugging-port={}",
            self.config.debug_port
        ));

        if self.config.headless {
            cmd.arg("--headless=new");
        }

        cmd.arg(format!(
            "--window-size={},{}",
            self.config.viewport_width, self.config.viewport_height
        ));
        cmd.arg("--disable-gpu");
        cmd.arg("--no-first-run");
        cmd.arg("--no-default-browser-check");
        cmd.arg("--disable-extensions");

        if let Some(ref ua) = self.config.user_agent {
            cmd.arg(format!("--user-agent={ua}"));
        }

        let process = cmd.spawn().map_err(|e| {
            BrowserError::LaunchFailed(format!("Failed to launch Chrome at {chrome_path}: {e}"))
        })?;
        self.process = Some(process);

        // Wait for CDP to become available
        let base_url = format!("http://127.0.0.1:{}", self.config.debug_port);
        let client = reqwest::Client::new();
        let mut attempts = 0;
        let max_attempts = 30; // 3 seconds max

        let ws_url = loop {
            attempts += 1;
            tokio::time::sleep(Duration::from_millis(100)).await;

            match client.get(format!("{base_url}/json/list")).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let targets: Vec<CdpTarget> = resp.json().await.map_err(|e| {
                        BrowserError::ConnectionFailed(format!("Failed to parse target list: {e}"))
                    })?;
                    let page_target = targets
                        .into_iter()
                        .find(|t| t.r#type == "page")
                        .ok_or_else(|| {
                            BrowserError::ConnectionFailed("No page target found".to_string())
                        })?;
                    break page_target.web_socket_debugger_url;
                }
                _ if attempts >= max_attempts => {
                    return Err(BrowserError::ConnectionFailed(
                        "CDP endpoint did not become available within 3 seconds".to_string(),
                    ));
                }
                _ => continue,
            }
        };

        let conn = Arc::new(CdpConnection::new(self.config.clone()));
        conn.connect(&ws_url).await?;
        self.connection = Some(conn);

        tracing::info!(
            "Browser launched and connected via CDP on port {}",
            self.config.debug_port
        );
        Ok(())
    }

    /// Connect to an already-running Chrome instance at the given debug port.
    pub async fn connect(&mut self, debug_port: u16) -> Result<(), BrowserError> {
        let base_url = format!("http://127.0.0.1:{debug_port}");
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{base_url}/json/list"))
            .send()
            .await
            .map_err(|e| {
                BrowserError::ConnectionFailed(format!(
                    "Cannot reach CDP at port {debug_port}: {e}"
                ))
            })?;

        let targets: Vec<CdpTarget> = resp.json().await.map_err(|e| {
            BrowserError::ConnectionFailed(format!("Failed to parse target list: {e}"))
        })?;

        let page_target = targets
            .into_iter()
            .find(|t| t.r#type == "page")
            .ok_or_else(|| BrowserError::ConnectionFailed("No page target found".to_string()))?;

        let conn = Arc::new(CdpConnection::new(self.config.clone()));
        conn.connect(&page_target.web_socket_debugger_url).await?;
        self.connection = Some(conn);

        Ok(())
    }

    /// Get the active CDP connection.
    fn conn(&self) -> Result<&Arc<CdpConnection>, BrowserError> {
        self.connection.as_ref().ok_or(BrowserError::NotConnected)
    }

    /// Navigate to a URL.
    pub async fn navigate(&self, url: &str) -> Result<PageInfo, BrowserError> {
        validate_url(url, &self.config.allowed_schemes)?;
        let conn = self.conn()?;

        let result = conn
            .send_command("Page.navigate", serde_json::json!({ "url": url }))
            .await
            .map_err(|e| BrowserError::NavigationFailed(e.to_string()))?;

        // Check for navigation errors
        if let Some(error_text) = result.get("errorText").and_then(|v| v.as_str()) {
            if !error_text.is_empty() {
                return Err(BrowserError::NavigationFailed(error_text.to_string()));
            }
        }

        // Wait for page to be ready
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Get page info
        let title_result = conn
            .send_command(
                "Runtime.evaluate",
                serde_json::json!({ "expression": "document.title" }),
            )
            .await
            .unwrap_or(serde_json::Value::Null);

        let title = title_result
            .pointer("/result/value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        Ok(PageInfo::new(url, title, 200))
    }

    /// Get the full HTML content of the current page.
    pub async fn get_content(&self) -> Result<String, BrowserError> {
        let conn = self.conn()?;
        let result = conn
            .send_command(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": "document.documentElement.outerHTML"
                }),
            )
            .await?;

        result
            .pointer("/result/value")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| BrowserError::CommandFailed {
                method: "get_content".to_string(),
                message: "No HTML content returned".to_string(),
            })
    }

    /// Get the visible text content of the current page.
    pub async fn get_text(&self) -> Result<String, BrowserError> {
        let conn = self.conn()?;
        let result = conn
            .send_command(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": "document.body ? document.body.innerText : ''"
                }),
            )
            .await?;

        result
            .pointer("/result/value")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| BrowserError::CommandFailed {
                method: "get_text".to_string(),
                message: "No text content returned".to_string(),
            })
    }

    /// Take a PNG screenshot of the current page.
    pub async fn screenshot(&self) -> Result<Vec<u8>, BrowserError> {
        let conn = self.conn()?;
        let result = conn
            .send_command(
                "Page.captureScreenshot",
                serde_json::json!({ "format": "png" }),
            )
            .await
            .map_err(|e| BrowserError::ScreenshotFailed(e.to_string()))?;

        let data_b64 = result.get("data").and_then(|v| v.as_str()).ok_or_else(|| {
            BrowserError::ScreenshotFailed("No screenshot data returned".to_string())
        })?;

        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64)
            .map_err(|e| BrowserError::ScreenshotFailed(format!("Base64 decode failed: {e}")))
    }

    /// Click an element matching the CSS selector.
    pub async fn click(&self, selector: &str) -> Result<(), BrowserError> {
        let selector = sanitize_selector(selector)?;
        let conn = self.conn()?;

        // Find element and get its center coordinates
        let js = format!(
            r#"(() => {{
                const el = document.querySelector({sel});
                if (!el) return null;
                const rect = el.getBoundingClientRect();
                return {{ x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 }};
            }})()"#,
            sel = serde_json::to_string(&selector)
                .map_err(|e| BrowserError::InvalidSelector(e.to_string()))?
        );

        let result = conn
            .send_command(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": js,
                    "returnByValue": true
                }),
            )
            .await?;

        let coords = result
            .pointer("/result/value")
            .ok_or_else(|| BrowserError::ElementNotFound(selector.clone()))?;

        if coords.is_null() {
            return Err(BrowserError::ElementNotFound(selector));
        }

        let x = coords
            .get("x")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| BrowserError::ElementNotFound(selector.clone()))?;
        let y = coords
            .get("y")
            .and_then(|v| v.as_f64())
            .ok_or(BrowserError::ElementNotFound(selector))?;

        // Dispatch mouse events
        for event_type in &["mousePressed", "mouseReleased"] {
            conn.send_command(
                "Input.dispatchMouseEvent",
                serde_json::json!({
                    "type": event_type,
                    "x": x,
                    "y": y,
                    "button": "left",
                    "clickCount": 1
                }),
            )
            .await?;
        }

        Ok(())
    }

    /// Type text into an element matching the CSS selector.
    pub async fn type_text(&self, selector: &str, text: &str) -> Result<(), BrowserError> {
        let selector = sanitize_selector(selector)?;

        // Focus the element first
        let conn = self.conn()?;
        let focus_js = format!(
            r#"(() => {{
                const el = document.querySelector({sel});
                if (!el) return false;
                el.focus();
                return true;
            }})()"#,
            sel = serde_json::to_string(&selector)
                .map_err(|e| BrowserError::InvalidSelector(e.to_string()))?
        );

        let result = conn
            .send_command(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": focus_js,
                    "returnByValue": true
                }),
            )
            .await?;

        let focused = result
            .pointer("/result/value")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !focused {
            return Err(BrowserError::ElementNotFound(selector));
        }

        // Type each character via CDP
        for ch in text.chars() {
            conn.send_command(
                "Input.dispatchKeyEvent",
                serde_json::json!({
                    "type": "keyDown",
                    "text": ch.to_string()
                }),
            )
            .await?;
            conn.send_command(
                "Input.dispatchKeyEvent",
                serde_json::json!({
                    "type": "keyUp",
                    "text": ch.to_string()
                }),
            )
            .await?;
        }

        Ok(())
    }

    /// Execute JavaScript in the browser and return the result.
    pub async fn evaluate_js(&self, script: &str) -> Result<serde_json::Value, BrowserError> {
        let script = sanitize_js(script)?;
        let conn = self.conn()?;

        let result = conn
            .send_command(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": script,
                    "returnByValue": true,
                    "awaitPromise": true
                }),
            )
            .await?;

        // Check for exceptions
        if let Some(exception) = result.get("exceptionDetails") {
            let msg = exception
                .pointer("/exception/description")
                .or_else(|| exception.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown JS error");
            return Err(BrowserError::JsEvalFailed(msg.to_string()));
        }

        Ok(result
            .pointer("/result/value")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    /// Wait for an element matching the CSS selector to appear.
    pub async fn wait_for_selector(
        &self,
        selector: &str,
        timeout_ms: u64,
    ) -> Result<(), BrowserError> {
        let selector = sanitize_selector(selector)?;
        let conn = self.conn()?;

        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let poll_interval = Duration::from_millis(100);

        let check_js = format!(
            "document.querySelector({}) !== null",
            serde_json::to_string(&selector)
                .map_err(|e| BrowserError::InvalidSelector(e.to_string()))?
        );

        loop {
            let result = conn
                .send_command(
                    "Runtime.evaluate",
                    serde_json::json!({
                        "expression": &check_js,
                        "returnByValue": true
                    }),
                )
                .await?;

            if result
                .pointer("/result/value")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return Ok(());
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(BrowserError::Timeout(timeout_ms));
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Close the browser and clean up resources.
    pub async fn close(&mut self) -> Result<(), BrowserError> {
        if let Some(conn) = &self.connection {
            // Try to close browser gracefully via CDP
            let _ = conn
                .send_command("Browser.close", serde_json::json!({}))
                .await;
        }
        self.connection = None;

        if let Some(mut process) = self.process.take() {
            let _ = process.kill().await;
        }

        Ok(())
    }
}

impl Drop for BrowserAutomation {
    fn drop(&mut self) {
        // Best-effort sync cleanup
        if let Some(mut process) = self.process.take() {
            let _ = process.start_kill();
        }
    }
}

// ── Chrome Discovery ────────────────────────────────────────────────────────

/// Attempt to find Chrome/Chromium in standard installation paths.
fn find_chrome() -> Option<String> {
    let candidates = if cfg!(target_os = "windows") {
        vec![
            r"C:\Program Files\Google\Chrome\Application\chrome.exe".to_string(),
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe".to_string(),
            format!(
                r"{}\AppData\Local\Google\Chrome\Application\chrome.exe",
                std::env::var("USERPROFILE").unwrap_or_default()
            ),
            r"C:\Program Files\Chromium\Application\chrome.exe".to_string(),
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".to_string(),
            "/Applications/Chromium.app/Contents/MacOS/Chromium".to_string(),
        ]
    } else {
        vec![
            "/usr/bin/google-chrome".to_string(),
            "/usr/bin/google-chrome-stable".to_string(),
            "/usr/bin/chromium".to_string(),
            "/usr/bin/chromium-browser".to_string(),
            "/snap/bin/chromium".to_string(),
        ]
    };

    candidates
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}

// ── Browser Tool (Tool trait) ───────────────────────────────────────────────

/// Browser automation tool implementing the Tool trait for the tool registry.
/// Supports actions: navigate, get_content, get_text, screenshot, click,
/// type_text, evaluate_js, wait_for_selector.
pub struct BrowserTool;

impl Default for BrowserTool {
    fn default() -> Self {
        Self
    }
}

impl BrowserTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl ironjaw_tools::Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "Browser automation via Chrome DevTools Protocol. Navigate pages, extract content, \
         interact with elements, execute JavaScript, and take screenshots."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "The browser action to perform",
                    "enum": [
                        "navigate",
                        "get_content",
                        "get_text",
                        "screenshot",
                        "click",
                        "type_text",
                        "evaluate_js",
                        "wait_for_selector"
                    ]
                },
                "url": {
                    "type": "string",
                    "description": "URL to navigate to (required for 'navigate' action)"
                },
                "selector": {
                    "type": "string",
                    "description": "CSS selector (required for 'click', 'type_text', 'wait_for_selector')"
                },
                "text": {
                    "type": "string",
                    "description": "Text to type (required for 'type_text' action)"
                },
                "script": {
                    "type": "string",
                    "description": "JavaScript code (required for 'evaluate_js' action)"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (for 'wait_for_selector', default 30000)",
                    "minimum": 100,
                    "maximum": 120000
                }
            },
            "required": ["action"]
        })
    }

    fn tier(&self) -> ironjaw_security::PermissionTier {
        ironjaw_security::PermissionTier::Confirm
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ironjaw_tools::ToolContext,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> ironjaw_tools::ToolResult {
        let action = match params.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => {
                return ironjaw_tools::ToolResult::error(
                    "INVALID_PARAMS",
                    "Missing required parameter: action",
                );
            }
        };

        // For now, the tool validates inputs and returns structured info.
        // Actual browser interaction requires a running BrowserAutomation instance
        // which would be managed by AppState. This tool acts as the RPC entry point.
        match action {
            "navigate" => {
                let url = match params.get("url").and_then(|v| v.as_str()) {
                    Some(u) => u,
                    None => {
                        return ironjaw_tools::ToolResult::error(
                            "INVALID_PARAMS",
                            "Missing required parameter: url",
                        );
                    }
                };
                let config = BrowserConfig::default();
                if let Err(e) = validate_url(url, &config.allowed_schemes) {
                    return ironjaw_tools::ToolResult::error("INVALID_URL", e.to_string());
                }
                ironjaw_tools::ToolResult::json(serde_json::json!({
                    "action": "navigate",
                    "url": url,
                    "status": "browser_not_running",
                    "message": "Browser automation requires a running Chrome instance. \
                               Use browser.launch first."
                }))
            }
            "get_content" | "get_text" | "screenshot" => {
                ironjaw_tools::ToolResult::json(serde_json::json!({
                    "action": action,
                    "status": "browser_not_running",
                    "message": "Browser automation requires a running Chrome instance."
                }))
            }
            "click" => {
                let selector = match params.get("selector").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None => {
                        return ironjaw_tools::ToolResult::error(
                            "INVALID_PARAMS",
                            "Missing required parameter: selector",
                        );
                    }
                };
                if let Err(e) = sanitize_selector(selector) {
                    return ironjaw_tools::ToolResult::error("INVALID_SELECTOR", e.to_string());
                }
                ironjaw_tools::ToolResult::json(serde_json::json!({
                    "action": "click",
                    "selector": selector,
                    "status": "browser_not_running"
                }))
            }
            "type_text" => {
                let selector = match params.get("selector").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None => {
                        return ironjaw_tools::ToolResult::error(
                            "INVALID_PARAMS",
                            "Missing required parameter: selector",
                        );
                    }
                };
                let text = match params.get("text").and_then(|v| v.as_str()) {
                    Some(t) => t,
                    None => {
                        return ironjaw_tools::ToolResult::error(
                            "INVALID_PARAMS",
                            "Missing required parameter: text",
                        );
                    }
                };
                if let Err(e) = sanitize_selector(selector) {
                    return ironjaw_tools::ToolResult::error("INVALID_SELECTOR", e.to_string());
                }
                ironjaw_tools::ToolResult::json(serde_json::json!({
                    "action": "type_text",
                    "selector": selector,
                    "text": text,
                    "status": "browser_not_running"
                }))
            }
            "evaluate_js" => {
                let script = match params.get("script").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None => {
                        return ironjaw_tools::ToolResult::error(
                            "INVALID_PARAMS",
                            "Missing required parameter: script",
                        );
                    }
                };
                if let Err(e) = sanitize_js(script) {
                    return ironjaw_tools::ToolResult::error("SECURITY_VIOLATION", e.to_string());
                }
                ironjaw_tools::ToolResult::json(serde_json::json!({
                    "action": "evaluate_js",
                    "status": "browser_not_running"
                }))
            }
            "wait_for_selector" => {
                let selector = match params.get("selector").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None => {
                        return ironjaw_tools::ToolResult::error(
                            "INVALID_PARAMS",
                            "Missing required parameter: selector",
                        );
                    }
                };
                if let Err(e) = sanitize_selector(selector) {
                    return ironjaw_tools::ToolResult::error("INVALID_SELECTOR", e.to_string());
                }
                let timeout = params
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30_000);
                ironjaw_tools::ToolResult::json(serde_json::json!({
                    "action": "wait_for_selector",
                    "selector": selector,
                    "timeout_ms": timeout,
                    "status": "browser_not_running"
                }))
            }
            _ => ironjaw_tools::ToolResult::error(
                "INVALID_ACTION",
                format!("Unknown browser action: {action}"),
            ),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── BrowserConfig tests ─────────────────────────────────────────────

    #[test]
    fn test_config_default() {
        let config = BrowserConfig::default();
        assert!(config.headless);
        assert_eq!(config.timeout_ms, 30_000);
        assert_eq!(config.viewport_width, 1280);
        assert_eq!(config.viewport_height, 720);
        assert_eq!(config.debug_port, 9222);
        assert!(config.user_agent.is_none());
        assert!(config.chrome_path.is_none());
        assert_eq!(config.max_pages, 5);
        assert_eq!(
            config.allowed_schemes,
            vec!["http".to_string(), "https".to_string()]
        );
    }

    #[test]
    fn test_config_validate_ok() {
        let config = BrowserConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validate_zero_viewport() {
        let config = BrowserConfig {
            viewport_width: 0,
            ..BrowserConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validate_huge_viewport() {
        let config = BrowserConfig {
            viewport_width: 10000,
            ..BrowserConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validate_zero_timeout() {
        let config = BrowserConfig {
            timeout_ms: 0,
            ..BrowserConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validate_zero_max_pages() {
        let config = BrowserConfig {
            max_pages: 0,
            ..BrowserConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validate_no_schemes() {
        let config = BrowserConfig {
            allowed_schemes: vec![],
            ..BrowserConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("scheme"));
    }

    #[test]
    fn test_config_serde_roundtrip() {
        let config = BrowserConfig {
            chrome_path: Some("/usr/bin/chromium".to_string()),
            headless: false,
            timeout_ms: 60_000,
            viewport_width: 1920,
            viewport_height: 1080,
            debug_port: 9333,
            user_agent: Some("TestBot/1.0".to_string()),
            max_pages: 10,
            allowed_schemes: vec!["http".to_string(), "https".to_string()],
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: BrowserConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.chrome_path, config.chrome_path);
        assert!(!parsed.headless);
        assert_eq!(parsed.timeout_ms, 60_000);
        assert_eq!(parsed.viewport_width, 1920);
        assert_eq!(parsed.debug_port, 9333);
        assert_eq!(parsed.user_agent, config.user_agent);
    }

    // ── PageInfo tests ──────────────────────────────────────────────────

    #[test]
    fn test_page_info_new() {
        let info = PageInfo::new("https://example.com", "Example", 200);
        assert_eq!(info.url, "https://example.com");
        assert_eq!(info.title, "Example");
        assert_eq!(info.status, 200);
    }

    #[test]
    fn test_page_info_serde_roundtrip() {
        let info = PageInfo::new("https://test.com/path?q=1", "Test Page", 301);
        let json = serde_json::to_string(&info).unwrap();
        let parsed: PageInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.url, info.url);
        assert_eq!(parsed.title, info.title);
        assert_eq!(parsed.status, 301);
    }

    #[test]
    fn test_page_info_empty_title() {
        let info = PageInfo::new("https://example.com", "", 200);
        assert_eq!(info.title, "");
    }

    // ── URL validation tests ────────────────────────────────────────────

    #[test]
    fn test_validate_url_http() {
        let schemes = vec!["http".to_string(), "https".to_string()];
        assert!(validate_url("http://example.com", &schemes).is_ok());
    }

    #[test]
    fn test_validate_url_https() {
        let schemes = vec!["http".to_string(), "https".to_string()];
        assert!(validate_url("https://example.com/path?q=1", &schemes).is_ok());
    }

    #[test]
    fn test_validate_url_empty() {
        let schemes = vec!["http".to_string(), "https".to_string()];
        let err = validate_url("", &schemes).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn test_validate_url_no_scheme() {
        let schemes = vec!["http".to_string(), "https".to_string()];
        let err = validate_url("example.com", &schemes).unwrap_err();
        assert!(err.to_string().contains("scheme"));
    }

    #[test]
    fn test_validate_url_file_scheme_rejected() {
        let schemes = vec!["http".to_string(), "https".to_string()];
        let err = validate_url("file:///etc/passwd", &schemes).unwrap_err();
        assert!(err.to_string().contains("not allowed"));
    }

    #[test]
    fn test_validate_url_javascript_scheme_rejected() {
        let schemes = vec!["http".to_string(), "https".to_string()];
        let err = validate_url("javascript:alert(1)", &schemes).unwrap_err();
        assert!(err.to_string().contains("not allowed"));
    }

    #[test]
    fn test_validate_url_data_scheme_rejected() {
        let schemes = vec!["http".to_string(), "https".to_string()];
        let err = validate_url("data:text/html,<h1>hello</h1>", &schemes).unwrap_err();
        assert!(err.to_string().contains("not allowed"));
    }

    #[test]
    fn test_validate_url_no_host() {
        let schemes = vec!["http".to_string(), "https".to_string()];
        let err = validate_url("http://", &schemes).unwrap_err();
        assert!(err.to_string().contains("host"));
    }

    // ── Selector sanitization tests ─────────────────────────────────────

    #[test]
    fn test_sanitize_selector_valid() {
        assert!(sanitize_selector("#my-id").is_ok());
        assert!(sanitize_selector(".class-name").is_ok());
        assert!(sanitize_selector("div > p:nth-child(2)").is_ok());
        assert!(sanitize_selector("[data-testid='submit']").is_ok());
        assert!(sanitize_selector("input[type=text]").is_ok());
    }

    #[test]
    fn test_sanitize_selector_empty() {
        let err = sanitize_selector("").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn test_sanitize_selector_whitespace_only() {
        let err = sanitize_selector("   ").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn test_sanitize_selector_too_long() {
        let long = "a".repeat(1025);
        let err = sanitize_selector(&long).unwrap_err();
        assert!(err.to_string().contains("maximum length"));
    }

    #[test]
    fn test_sanitize_selector_javascript_injection() {
        let err = sanitize_selector("div[onclick=javascript:alert(1)]").unwrap_err();
        assert!(err.to_string().contains("forbidden"));
    }

    #[test]
    fn test_sanitize_selector_expression_injection() {
        let err = sanitize_selector("div{background:expression(alert(1))}").unwrap_err();
        assert!(err.to_string().contains("forbidden"));
    }

    #[test]
    fn test_sanitize_selector_eval_injection() {
        let err = sanitize_selector("*{eval(document.cookie)}").unwrap_err();
        assert!(err.to_string().contains("forbidden"));
    }

    #[test]
    fn test_sanitize_selector_null_byte() {
        let err = sanitize_selector(r"div\x00").unwrap_err();
        assert!(err.to_string().contains("forbidden"));
    }

    // ── JavaScript sanitization tests ───────────────────────────────────

    #[test]
    fn test_sanitize_js_valid() {
        assert!(sanitize_js("document.title").is_ok());
        assert!(sanitize_js("1 + 1").is_ok());
        assert!(sanitize_js("fetch('https://api.example.com')").is_ok());
        assert!(sanitize_js("JSON.stringify({key: 'value'})").is_ok());
    }

    #[test]
    fn test_sanitize_js_empty() {
        let err = sanitize_js("").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn test_sanitize_js_too_long() {
        let long = "x".repeat(65 * 1024);
        let err = sanitize_js(&long).unwrap_err();
        assert!(err.to_string().contains("64KB"));
    }

    #[test]
    fn test_sanitize_js_require_fs_blocked() {
        let err = sanitize_js("require('fs').readFileSync('/etc/passwd')").unwrap_err();
        assert!(err.to_string().contains("blocked"));
    }

    #[test]
    fn test_sanitize_js_require_child_process_blocked() {
        let err = sanitize_js("require('child_process').execSync('whoami')").unwrap_err();
        assert!(err.to_string().contains("blocked"));
    }

    #[test]
    fn test_sanitize_js_file_protocol_blocked() {
        let err = sanitize_js("fetch('file:///etc/passwd')").unwrap_err();
        assert!(err.to_string().contains("blocked"));
    }

    // ── BrowserError tests ──────────────────────────────────────────────

    #[test]
    fn test_error_display() {
        let err = BrowserError::InvalidUrl("bad url".to_string());
        assert!(err.to_string().contains("bad url"));

        let err = BrowserError::Timeout(5000);
        assert!(err.to_string().contains("5000ms"));

        let err = BrowserError::NotConnected;
        assert!(err.to_string().contains("not connected"));

        let err = BrowserError::CommandFailed {
            method: "Page.navigate".to_string(),
            message: "net::ERR_NAME_NOT_RESOLVED".to_string(),
        };
        assert!(err.to_string().contains("Page.navigate"));
        assert!(err.to_string().contains("ERR_NAME_NOT_RESOLVED"));
    }

    #[test]
    fn test_error_from_reqwest() {
        // Verify the From impl exists by constructing via a parse error
        let result: Result<reqwest::Url, _> = "not a url".parse();
        if let Err(_e) = result {
            // We can't easily construct a reqwest::Error, but the From impl compiles.
        }
    }

    #[test]
    fn test_error_from_serde_json() {
        let e: Result<serde_json::Value, _> = serde_json::from_str("invalid json");
        let browser_err = BrowserError::from(e.unwrap_err());
        assert!(browser_err.to_string().contains("parse"));
    }

    // ── BrowserAutomation tests ─────────────────────────────────────────

    #[test]
    fn test_browser_automation_new() {
        let ba = BrowserAutomation::new(BrowserConfig::default());
        assert!(ba.is_ok());
    }

    #[test]
    fn test_browser_automation_invalid_config() {
        let config = BrowserConfig {
            viewport_width: 0,
            ..BrowserConfig::default()
        };
        let ba = BrowserAutomation::new(config);
        assert!(ba.is_err());
    }

    #[test]
    fn test_browser_automation_with_defaults() {
        let ba = BrowserAutomation::with_defaults();
        assert!(ba.is_ok());
        let ba = ba.unwrap();
        assert!(ba.config().headless);
        assert_eq!(ba.config().timeout_ms, 30_000);
    }

    #[tokio::test]
    async fn test_browser_not_connected() {
        let ba = BrowserAutomation::with_defaults().unwrap();
        assert!(!ba.is_connected().await);
    }

    #[tokio::test]
    async fn test_navigate_requires_connection() {
        let ba = BrowserAutomation::with_defaults().unwrap();
        let result = ba.navigate("https://example.com").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            BrowserError::NotConnected => {}
            other => panic!("Expected NotConnected, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_get_content_requires_connection() {
        let ba = BrowserAutomation::with_defaults().unwrap();
        let result = ba.get_content().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_text_requires_connection() {
        let ba = BrowserAutomation::with_defaults().unwrap();
        let result = ba.get_text().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_screenshot_requires_connection() {
        let ba = BrowserAutomation::with_defaults().unwrap();
        let result = ba.screenshot().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_click_validates_selector_before_connect() {
        let ba = BrowserAutomation::with_defaults().unwrap();
        // Empty selector should fail with InvalidSelector, not NotConnected
        let result = ba.click("").await;
        match result.unwrap_err() {
            BrowserError::InvalidSelector(_) => {}
            other => panic!("Expected InvalidSelector, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_type_text_validates_selector_before_connect() {
        let ba = BrowserAutomation::with_defaults().unwrap();
        let result = ba.type_text("", "hello").await;
        match result.unwrap_err() {
            BrowserError::InvalidSelector(_) => {}
            other => panic!("Expected InvalidSelector, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_evaluate_js_validates_before_connect() {
        let ba = BrowserAutomation::with_defaults().unwrap();
        // Empty script fails with JsEvalFailed before connection check
        let result = ba.evaluate_js("").await;
        match result.unwrap_err() {
            BrowserError::JsEvalFailed(_) => {}
            other => panic!("Expected JsEvalFailed, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_evaluate_js_blocked_script() {
        let ba = BrowserAutomation::with_defaults().unwrap();
        let result = ba
            .evaluate_js("require('fs').readFileSync('/etc/passwd')")
            .await;
        match result.unwrap_err() {
            BrowserError::SecurityViolation(_) => {}
            other => panic!("Expected SecurityViolation, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_wait_for_selector_validates_before_connect() {
        let ba = BrowserAutomation::with_defaults().unwrap();
        let result = ba.wait_for_selector("", 1000).await;
        match result.unwrap_err() {
            BrowserError::InvalidSelector(_) => {}
            other => panic!("Expected InvalidSelector, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_navigate_validates_url_before_connect() {
        let ba = BrowserAutomation::with_defaults().unwrap();
        // file:// URL fails before connection check
        let result = ba.navigate("file:///etc/passwd").await;
        match result.unwrap_err() {
            BrowserError::InvalidUrl(_) => {}
            other => panic!("Expected InvalidUrl, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_close_when_not_connected() {
        let mut ba = BrowserAutomation::with_defaults().unwrap();
        // Close should succeed even when not connected
        assert!(ba.close().await.is_ok());
    }

    // ── BrowserTool (Tool trait) tests ──────────────────────────────────

    #[test]
    fn test_browser_tool_name() {
        let tool = BrowserTool::new();
        assert_eq!(ironjaw_tools::Tool::name(&tool), "browser");
    }

    #[test]
    fn test_browser_tool_description() {
        let tool = BrowserTool::new();
        let desc = ironjaw_tools::Tool::description(&tool);
        assert!(desc.contains("Chrome DevTools Protocol"));
    }

    #[test]
    fn test_browser_tool_tier() {
        let tool = BrowserTool::new();
        assert_eq!(
            ironjaw_tools::Tool::tier(&tool),
            ironjaw_security::PermissionTier::Confirm
        );
    }

    #[test]
    fn test_browser_tool_schema() {
        let tool = BrowserTool::new();
        let schema = ironjaw_tools::Tool::schema(&tool);
        assert_eq!(schema["type"], "object");
        assert!(
            schema["properties"]["action"]["enum"]
                .as_array()
                .unwrap()
                .len()
                >= 8
        );
    }

    #[tokio::test]
    async fn test_browser_tool_missing_action() {
        let tool = BrowserTool::new();
        let ctx = ironjaw_tools::ToolContext {
            session_id: "test".to_string(),
            connection_id: "test".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            brain: None,
            stream_tx: None,
        };
        let result = ironjaw_tools::Tool::execute(
            &tool,
            serde_json::json!({}),
            &ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_browser_tool_unknown_action() {
        let tool = BrowserTool::new();
        let ctx = ironjaw_tools::ToolContext {
            session_id: "test".to_string(),
            connection_id: "test".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            brain: None,
            stream_tx: None,
        };
        let result = ironjaw_tools::Tool::execute(
            &tool,
            serde_json::json!({"action": "destroy"}),
            &ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_browser_tool_navigate_invalid_url() {
        let tool = BrowserTool::new();
        let ctx = ironjaw_tools::ToolContext {
            session_id: "test".to_string(),
            connection_id: "test".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            brain: None,
            stream_tx: None,
        };
        let result = ironjaw_tools::Tool::execute(
            &tool,
            serde_json::json!({"action": "navigate", "url": "file:///etc/passwd"}),
            &ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_browser_tool_navigate_missing_url() {
        let tool = BrowserTool::new();
        let ctx = ironjaw_tools::ToolContext {
            session_id: "test".to_string(),
            connection_id: "test".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            brain: None,
            stream_tx: None,
        };
        let result = ironjaw_tools::Tool::execute(
            &tool,
            serde_json::json!({"action": "navigate"}),
            &ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_browser_tool_click_missing_selector() {
        let tool = BrowserTool::new();
        let ctx = ironjaw_tools::ToolContext {
            session_id: "test".to_string(),
            connection_id: "test".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            brain: None,
            stream_tx: None,
        };
        let result = ironjaw_tools::Tool::execute(
            &tool,
            serde_json::json!({"action": "click"}),
            &ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_browser_tool_type_text_missing_params() {
        let tool = BrowserTool::new();
        let ctx = ironjaw_tools::ToolContext {
            session_id: "test".to_string(),
            connection_id: "test".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            brain: None,
            stream_tx: None,
        };
        // Missing selector
        let result = ironjaw_tools::Tool::execute(
            &tool,
            serde_json::json!({"action": "type_text", "text": "hello"}),
            &ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(result.is_error());

        // Missing text
        let result = ironjaw_tools::Tool::execute(
            &tool,
            serde_json::json!({"action": "type_text", "selector": "#input"}),
            &ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_browser_tool_evaluate_js_missing_script() {
        let tool = BrowserTool::new();
        let ctx = ironjaw_tools::ToolContext {
            session_id: "test".to_string(),
            connection_id: "test".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            brain: None,
            stream_tx: None,
        };
        let result = ironjaw_tools::Tool::execute(
            &tool,
            serde_json::json!({"action": "evaluate_js"}),
            &ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_browser_tool_evaluate_js_blocked() {
        let tool = BrowserTool::new();
        let ctx = ironjaw_tools::ToolContext {
            session_id: "test".to_string(),
            connection_id: "test".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            brain: None,
            stream_tx: None,
        };
        let result = ironjaw_tools::Tool::execute(
            &tool,
            serde_json::json!({
                "action": "evaluate_js",
                "script": "require('child_process').execSync('id')"
            }),
            &ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_browser_tool_wait_for_selector_missing() {
        let tool = BrowserTool::new();
        let ctx = ironjaw_tools::ToolContext {
            session_id: "test".to_string(),
            connection_id: "test".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            brain: None,
            stream_tx: None,
        };
        let result = ironjaw_tools::Tool::execute(
            &tool,
            serde_json::json!({"action": "wait_for_selector"}),
            &ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn test_browser_tool_navigate_valid_url_returns_json() {
        let tool = BrowserTool::new();
        let ctx = ironjaw_tools::ToolContext {
            session_id: "test".to_string(),
            connection_id: "test".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            brain: None,
            stream_tx: None,
        };
        let result = ironjaw_tools::Tool::execute(
            &tool,
            serde_json::json!({"action": "navigate", "url": "https://example.com"}),
            &ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        // Should return JSON (not error) since URL is valid, even though browser isn't running
        assert!(!result.is_error());
        match result {
            ironjaw_tools::ToolResult::Json { content } => {
                assert_eq!(content["action"], "navigate");
                assert_eq!(content["url"], "https://example.com");
                assert_eq!(content["status"], "browser_not_running");
            }
            _ => panic!("Expected Json result"),
        }
    }

    #[tokio::test]
    async fn test_browser_tool_get_content_returns_json() {
        let tool = BrowserTool::new();
        let ctx = ironjaw_tools::ToolContext {
            session_id: "test".to_string(),
            connection_id: "test".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            brain: None,
            stream_tx: None,
        };
        let result = ironjaw_tools::Tool::execute(
            &tool,
            serde_json::json!({"action": "get_content"}),
            &ctx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(!result.is_error());
    }

    // ── CDP message type tests ──────────────────────────────────────────

    #[test]
    fn test_cdp_command_serialize() {
        let cmd = CdpCommand {
            id: 1,
            method: "Page.navigate".to_string(),
            params: serde_json::json!({"url": "https://example.com"}),
        };
        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["id"], 1);
        assert_eq!(json["method"], "Page.navigate");
        assert_eq!(json["params"]["url"], "https://example.com");
    }

    #[test]
    fn test_cdp_response_deserialize_success() {
        let json = r#"{"id": 1, "result": {"frameId": "abc"}}"#;
        let resp: CdpResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.id, Some(1));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_cdp_response_deserialize_error() {
        let json = r#"{"id": 1, "error": {"code": -32000, "message": "Not found"}}"#;
        let resp: CdpResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.id, Some(1));
        assert!(resp.result.is_none());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32000);
        assert_eq!(err.message, "Not found");
    }

    #[test]
    fn test_cdp_response_deserialize_event() {
        // CDP events don't have an id
        let json = r#"{"method": "Page.loadEventFired", "params": {"timestamp": 123.456}}"#;
        let resp: CdpResponse = serde_json::from_str(json).unwrap();
        assert!(resp.id.is_none());
        assert!(resp.result.is_none());
    }

    #[test]
    fn test_cdp_target_deserialize() {
        let json = r#"{
            "id": "abc123",
            "type": "page",
            "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/abc123"
        }"#;
        let target: CdpTarget = serde_json::from_str(json).unwrap();
        assert_eq!(target.id, "abc123");
        assert_eq!(target.r#type, "page");
        assert!(target.web_socket_debugger_url.contains("devtools/page"));
    }

    // ── Chrome discovery test ───────────────────────────────────────────

    #[test]
    fn test_find_chrome_returns_option() {
        // Just verify it doesn't panic -- result depends on whether Chrome is installed
        let _result = find_chrome();
    }
}
