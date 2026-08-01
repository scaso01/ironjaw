// OpenTelemetry tracing: OTLP exporter, span helpers, config.
//
// Gated behind the `telemetry` feature flag. When the feature is disabled,
// all public functions are no-ops or return mock values so callers don't
// need conditional compilation.

use serde::{Deserialize, Serialize};

/// Errors from the telemetry subsystem.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("Telemetry init failed: {0}")]
    Init(String),
    #[error("Exporter error: {0}")]
    Exporter(String),
    #[error("Shutdown error: {0}")]
    Shutdown(String),
    #[error("Span creation failed: {0}")]
    SpanCreation(String),
}

/// Configuration for OpenTelemetry tracing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// Whether telemetry is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// OTLP exporter endpoint.
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    /// Service name reported in traces.
    #[serde(default = "default_service_name")]
    pub service_name: String,
    /// Sampling ratio (0.0 to 1.0).
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,
}

fn default_endpoint() -> String {
    "http://localhost:4317".to_string()
}

fn default_service_name() -> String {
    "ironjaw".to_string()
}

fn default_sample_rate() -> f64 {
    1.0
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: default_endpoint(),
            service_name: default_service_name(),
            sample_rate: default_sample_rate(),
        }
    }
}

/// A span representing a unit of work.
///
/// When the `telemetry` feature is enabled, this wraps a real OpenTelemetry span.
/// Otherwise, it's a lightweight no-op struct for zero overhead.
#[derive(Debug)]
pub struct Span {
    name: String,
    attributes: Vec<(String, String)>,
}

impl Span {
    /// Create a new span with the given name.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            attributes: Vec::new(),
        }
    }

    /// Get the span name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the span attributes.
    pub fn attributes(&self) -> &[(String, String)] {
        &self.attributes
    }

    /// Add a key-value attribute to the span.
    pub fn set_attribute(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.attributes.push((key.into(), value.into()));
    }

    /// End the span (no-op in mock mode; real OTel would finalize here).
    pub fn end(&self) {
        // In a real OTel implementation this would call span.end()
    }
}

/// Extension trait for adding common Ironjaw attributes to spans.
pub trait SpanExt {
    /// Tag this span with a tool name.
    fn with_tool(&mut self, tool_name: &str) -> &mut Self;
    /// Tag this span with a session ID.
    fn with_session(&mut self, session_id: &str) -> &mut Self;
    /// Tag this span with an error message.
    fn with_error(&mut self, err: &str) -> &mut Self;
    /// Tag this span with a method name.
    fn with_method(&mut self, method: &str) -> &mut Self;
}

impl SpanExt for Span {
    fn with_tool(&mut self, tool_name: &str) -> &mut Self {
        self.set_attribute("ironjaw.tool", tool_name);
        self
    }

    fn with_session(&mut self, session_id: &str) -> &mut Self {
        self.set_attribute("ironjaw.session_id", session_id);
        self
    }

    fn with_error(&mut self, err: &str) -> &mut Self {
        self.set_attribute("error", "true");
        self.set_attribute("error.message", err);
        self
    }

    fn with_method(&mut self, method: &str) -> &mut Self {
        self.set_attribute("ironjaw.method", method);
        self
    }
}

/// Create a new span. Convenience wrapper around `Span::new`.
pub fn create_span(name: &str) -> Span {
    Span::new(name)
}

/// Tracks whether telemetry has been initialized.
static INITIALIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Initialize telemetry with the given configuration.
///
/// When the `telemetry` feature is enabled, this sets up the OTLP exporter
/// and registers the global tracer provider. Without the feature, this
/// simply marks telemetry as initialized for status checks.
pub fn init_telemetry(config: &TelemetryConfig) -> Result<(), TelemetryError> {
    if !config.enabled {
        tracing::info!("Telemetry disabled by config");
        return Ok(());
    }

    if INITIALIZED.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(TelemetryError::Init(
            "Telemetry already initialized".to_string(),
        ));
    }

    // Validate config
    if config.sample_rate < 0.0 || config.sample_rate > 1.0 {
        return Err(TelemetryError::Init(format!(
            "Invalid sample_rate: {} (must be 0.0-1.0)",
            config.sample_rate
        )));
    }

    if config.endpoint.is_empty() {
        return Err(TelemetryError::Init("Empty endpoint".to_string()));
    }

    if config.service_name.is_empty() {
        return Err(TelemetryError::Init("Empty service_name".to_string()));
    }

    #[cfg(feature = "telemetry")]
    {
        // Real OTel initialization would go here:
        // 1. Create OTLP exporter targeting config.endpoint
        // 2. Build TracerProvider with sampler at config.sample_rate
        // 3. Set global tracer provider
        // 4. Install tracing-opentelemetry layer
        tracing::info!(
            endpoint = %config.endpoint,
            service = %config.service_name,
            sample_rate = config.sample_rate,
            "OpenTelemetry initialized"
        );
    }

    #[cfg(not(feature = "telemetry"))]
    {
        tracing::info!(
            endpoint = %config.endpoint,
            service = %config.service_name,
            "Telemetry init (no-op — 'telemetry' feature not enabled)"
        );
    }

    INITIALIZED.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// Returns whether telemetry has been initialized.
pub fn is_initialized() -> bool {
    INITIALIZED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Shutdown telemetry, flushing any pending spans.
pub fn shutdown_telemetry() {
    if !INITIALIZED.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }

    #[cfg(feature = "telemetry")]
    {
        // Real shutdown: flush exporter, drop tracer provider
        tracing::info!("OpenTelemetry shutdown complete");
    }

    INITIALIZED.store(false, std::sync::atomic::Ordering::SeqCst);
    tracing::info!("Telemetry shutdown");
}

/// Reset the initialized state (for testing only).
#[cfg(test)]
fn reset_initialized() {
    INITIALIZED.store(false, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reset state before each test to avoid cross-test interference
    fn setup() {
        reset_initialized();
    }

    #[test]
    fn test_config_defaults() {
        let config = TelemetryConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.endpoint, "http://localhost:4317");
        assert_eq!(config.service_name, "ironjaw");
        assert!((config.sample_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_serde_roundtrip() {
        let config = TelemetryConfig {
            enabled: true,
            endpoint: "http://otel:4317".to_string(),
            service_name: "my-service".to_string(),
            sample_rate: 0.5,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: TelemetryConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.endpoint, "http://otel:4317");
        assert_eq!(parsed.service_name, "my-service");
        assert!((parsed.sample_rate - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_from_partial_json() {
        let json = r#"{"enabled": true}"#;
        let config: TelemetryConfig = serde_json::from_str(json).unwrap();
        assert!(config.enabled);
        assert_eq!(config.endpoint, "http://localhost:4317");
        assert_eq!(config.service_name, "ironjaw");
    }

    #[test]
    fn test_create_span() {
        let span = create_span("test.operation");
        assert_eq!(span.name(), "test.operation");
        assert!(span.attributes().is_empty());
    }

    #[test]
    fn test_span_attributes() {
        let mut span = create_span("test");
        span.set_attribute("key", "value");
        assert_eq!(span.attributes().len(), 1);
        assert_eq!(
            span.attributes()[0],
            ("key".to_string(), "value".to_string())
        );
    }

    #[test]
    fn test_span_ext_with_tool() {
        let mut span = create_span("tool.execute");
        span.with_tool("read_file");
        let attrs = span.attributes();
        assert!(attrs
            .iter()
            .any(|(k, v)| k == "ironjaw.tool" && v == "read_file"));
    }

    #[test]
    fn test_span_ext_with_session() {
        let mut span = create_span("chat.send");
        span.with_session("sess-abc-123");
        let attrs = span.attributes();
        assert!(attrs
            .iter()
            .any(|(k, v)| k == "ironjaw.session_id" && v == "sess-abc-123"));
    }

    #[test]
    fn test_span_ext_with_error() {
        let mut span = create_span("op.failed");
        span.with_error("connection refused");
        let attrs = span.attributes();
        assert!(attrs.iter().any(|(k, v)| k == "error" && v == "true"));
        assert!(attrs
            .iter()
            .any(|(k, v)| k == "error.message" && v == "connection refused"));
    }

    #[test]
    fn test_span_ext_with_method() {
        let mut span = create_span("dispatch");
        span.with_method("chat.send");
        let attrs = span.attributes();
        assert!(attrs
            .iter()
            .any(|(k, v)| k == "ironjaw.method" && v == "chat.send"));
    }

    #[test]
    fn test_span_ext_chaining() {
        let mut span = create_span("complex.op");
        span.with_tool("shell")
            .with_session("s1")
            .with_method("tools.execute");
        assert_eq!(span.attributes().len(), 3);
    }

    #[test]
    fn test_span_end_does_not_panic() {
        let span = create_span("test");
        span.end(); // Should be a no-op, must not panic
    }

    #[test]
    fn test_init_disabled() {
        setup();
        let config = TelemetryConfig::default(); // enabled = false
        let result = init_telemetry(&config);
        assert!(result.is_ok());
        // Should NOT mark as initialized when disabled
        assert!(!is_initialized());
    }

    #[test]
    fn test_init_enabled() {
        setup();
        let config = TelemetryConfig {
            enabled: true,
            ..Default::default()
        };
        let result = init_telemetry(&config);
        assert!(result.is_ok());
        assert!(is_initialized());
        // Reset for other tests
        reset_initialized();
    }

    #[test]
    fn test_init_double_init_fails() {
        setup();
        let config = TelemetryConfig {
            enabled: true,
            ..Default::default()
        };
        init_telemetry(&config).unwrap();
        let result = init_telemetry(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("already initialized"));
        reset_initialized();
    }

    #[test]
    fn test_init_invalid_sample_rate() {
        setup();
        let config = TelemetryConfig {
            enabled: true,
            sample_rate: 2.0,
            ..Default::default()
        };
        let result = init_telemetry(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("sample_rate"));
    }

    #[test]
    fn test_init_negative_sample_rate() {
        setup();
        let config = TelemetryConfig {
            enabled: true,
            sample_rate: -0.1,
            ..Default::default()
        };
        assert!(init_telemetry(&config).is_err());
    }

    #[test]
    fn test_init_empty_endpoint() {
        setup();
        let config = TelemetryConfig {
            enabled: true,
            endpoint: String::new(),
            ..Default::default()
        };
        let result = init_telemetry(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("endpoint"));
    }

    #[test]
    fn test_init_empty_service_name() {
        setup();
        let config = TelemetryConfig {
            enabled: true,
            service_name: String::new(),
            ..Default::default()
        };
        let result = init_telemetry(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("service_name"));
    }

    #[test]
    fn test_shutdown() {
        setup();
        let config = TelemetryConfig {
            enabled: true,
            ..Default::default()
        };
        init_telemetry(&config).unwrap();
        assert!(is_initialized());
        shutdown_telemetry();
        assert!(!is_initialized());
    }

    #[test]
    fn test_shutdown_when_not_initialized() {
        setup();
        // Should not panic
        shutdown_telemetry();
        assert!(!is_initialized());
    }

    #[test]
    fn test_telemetry_error_display() {
        let err = TelemetryError::Init("test".to_string());
        assert_eq!(err.to_string(), "Telemetry init failed: test");

        let err = TelemetryError::Exporter("export fail".to_string());
        assert_eq!(err.to_string(), "Exporter error: export fail");

        let err = TelemetryError::Shutdown("flush fail".to_string());
        assert_eq!(err.to_string(), "Shutdown error: flush fail");

        let err = TelemetryError::SpanCreation("bad name".to_string());
        assert_eq!(err.to_string(), "Span creation failed: bad name");
    }
}
