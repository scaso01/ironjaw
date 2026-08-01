//! Channel adapter abstraction for transport-agnostic communication.
//!
//! A [`Channel`] abstracts the communication transport. Instead of tying logic
//! to WebSocket, this module provides a trait that WebSocket, HTTP, stdio, MCP,
//! and future transports can implement.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Identifies the type of channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChannelKind {
    WebSocket,
    Http,
    Stdio,
    Mcp,
    Custom(String),
}

impl fmt::Display for ChannelKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WebSocket => write!(f, "WebSocket"),
            Self::Http => write!(f, "Http"),
            Self::Stdio => write!(f, "Stdio"),
            Self::Mcp => write!(f, "Mcp"),
            Self::Custom(name) => write!(f, "Custom({name})"),
        }
    }
}

/// Metadata about a channel connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: String,
    pub kind: ChannelKind,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub authenticated: bool,
}

/// A message that flows through channels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMessage {
    pub id: String,
    pub channel_id: String,
    pub content: String,
    pub direction: MessageDirection,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Direction of a channel message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageDirection {
    /// From client to server.
    Inbound,
    /// From server to client.
    Outbound,
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors that can occur during channel operations.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("channel is not connected")]
    NotConnected,
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("channel is already closed")]
    AlreadyClosed,
    #[error("invalid message: {0}")]
    InvalidMessage(String),
}

// ---------------------------------------------------------------------------
// Channel trait
// ---------------------------------------------------------------------------

/// Core channel trait -- all transports implement this.
#[async_trait]
pub trait Channel: Send + Sync {
    /// Returns the channel metadata.
    fn info(&self) -> &ChannelInfo;

    /// Returns the channel kind.
    fn kind(&self) -> &ChannelKind;

    /// Returns `true` if the channel is still connected.
    fn is_connected(&self) -> bool;

    /// Send a message through the channel.
    async fn send(&self, message: ChannelMessage) -> Result<(), ChannelError>;

    /// Close the channel.
    async fn close(&self) -> Result<(), ChannelError>;
}

// ---------------------------------------------------------------------------
// ChannelRegistry
// ---------------------------------------------------------------------------

/// Registry that manages multiple channels.
pub struct ChannelRegistry {
    channels: HashMap<String, Box<dyn Channel>>,
}

impl Default for ChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelRegistry {
    /// Create a new, empty registry.
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }

    /// Register a channel. Returns the channel ID.
    pub fn register(&mut self, channel: Box<dyn Channel>) -> String {
        let id = channel.info().id.clone();
        self.channels.insert(id.clone(), channel);
        id
    }

    /// Remove a channel by ID. Returns the channel if it existed.
    pub fn unregister(&mut self, id: &str) -> Option<Box<dyn Channel>> {
        self.channels.remove(id)
    }

    /// Look up a channel by ID.
    pub fn get(&self, id: &str) -> Option<&dyn Channel> {
        self.channels.get(id).map(|c| c.as_ref())
    }

    /// List metadata for all registered channels.
    pub fn list(&self) -> Vec<&ChannelInfo> {
        self.channels.values().map(|c| c.info()).collect()
    }

    /// List metadata for channels matching a given kind.
    pub fn list_by_kind(&self, kind: &ChannelKind) -> Vec<&ChannelInfo> {
        self.channels
            .values()
            .filter(|c| c.kind() == kind)
            .map(|c| c.info())
            .collect()
    }

    /// Total number of registered channels (connected or not).
    pub fn count(&self) -> usize {
        self.channels.len()
    }

    /// Number of registered channels of a specific kind.
    pub fn count_by_kind(&self, kind: &ChannelKind) -> usize {
        self.channels.values().filter(|c| c.kind() == kind).count()
    }

    /// Number of channels that report themselves as connected.
    pub fn connected_count(&self) -> usize {
        self.channels.values().filter(|c| c.is_connected()).count()
    }
}

// ---------------------------------------------------------------------------
// MockChannel
// ---------------------------------------------------------------------------

/// A mock channel for testing.
pub struct MockChannel {
    info: ChannelInfo,
    connected: std::sync::atomic::AtomicBool,
    messages: std::sync::Arc<tokio::sync::Mutex<Vec<ChannelMessage>>>,
}

impl MockChannel {
    /// Create a new mock channel of the given kind.
    pub fn new(kind: ChannelKind) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        Self {
            info: ChannelInfo {
                id,
                kind,
                connected_at: chrono::Utc::now(),
                metadata: HashMap::new(),
                authenticated: false,
            },
            connected: std::sync::atomic::AtomicBool::new(true),
            messages: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    /// Create a mock channel with a specific ID (useful in tests).
    pub fn with_id(kind: ChannelKind, id: impl Into<String>) -> Self {
        Self {
            info: ChannelInfo {
                id: id.into(),
                kind,
                connected_at: chrono::Utc::now(),
                metadata: HashMap::new(),
                authenticated: false,
            },
            connected: std::sync::atomic::AtomicBool::new(true),
            messages: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    /// Retrieve all messages that were sent through this channel.
    pub async fn sent_messages(&self) -> Vec<ChannelMessage> {
        self.messages.lock().await.clone()
    }
}

#[async_trait]
impl Channel for MockChannel {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    fn kind(&self) -> &ChannelKind {
        &self.info.kind
    }

    fn is_connected(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn send(&self, message: ChannelMessage) -> Result<(), ChannelError> {
        if !self.is_connected() {
            return Err(ChannelError::NotConnected);
        }
        if message.content.is_empty() {
            return Err(ChannelError::InvalidMessage(
                "content must not be empty".into(),
            ));
        }
        self.messages.lock().await.push(message);
        Ok(())
    }

    async fn close(&self) -> Result<(), ChannelError> {
        if !self
            .connected
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(ChannelError::AlreadyClosed);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helper: build a ChannelMessage for tests
// ---------------------------------------------------------------------------

/// Build an outbound [`ChannelMessage`] with sensible defaults.
pub fn outbound_message(channel_id: &str, content: &str) -> ChannelMessage {
    ChannelMessage {
        id: uuid::Uuid::new_v4().to_string(),
        channel_id: channel_id.to_owned(),
        content: content.to_owned(),
        direction: MessageDirection::Outbound,
        timestamp: chrono::Utc::now(),
    }
}

/// Build an inbound [`ChannelMessage`] with sensible defaults.
pub fn inbound_message(channel_id: &str, content: &str) -> ChannelMessage {
    ChannelMessage {
        id: uuid::Uuid::new_v4().to_string(),
        channel_id: channel_id.to_owned(),
        content: content.to_owned(),
        direction: MessageDirection::Inbound,
        timestamp: chrono::Utc::now(),
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- MockChannel basics -------------------------------------------------

    #[tokio::test]
    async fn mock_channel_starts_connected() {
        let ch = MockChannel::new(ChannelKind::WebSocket);
        assert!(ch.is_connected());
    }

    #[tokio::test]
    async fn mock_channel_kind_matches() {
        let ch = MockChannel::new(ChannelKind::Stdio);
        assert_eq!(ch.kind(), &ChannelKind::Stdio);
    }

    #[tokio::test]
    async fn mock_channel_info_populated() {
        let ch = MockChannel::new(ChannelKind::Http);
        let info = ch.info();
        assert_eq!(info.kind, ChannelKind::Http);
        assert!(!info.id.is_empty());
        assert!(!info.authenticated);
    }

    #[tokio::test]
    async fn mock_channel_with_custom_id() {
        let ch = MockChannel::with_id(ChannelKind::Mcp, "test-id-42");
        assert_eq!(ch.info().id, "test-id-42");
    }

    // -- Send / receive -----------------------------------------------------

    #[tokio::test]
    async fn send_stores_message() {
        let ch = MockChannel::new(ChannelKind::WebSocket);
        let msg = outbound_message(&ch.info().id, "hello");
        ch.send(msg).await.unwrap();

        let sent = ch.sent_messages().await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].content, "hello");
    }

    #[tokio::test]
    async fn send_multiple_messages() {
        let ch = MockChannel::new(ChannelKind::WebSocket);
        let id = ch.info().id.clone();
        ch.send(outbound_message(&id, "one")).await.unwrap();
        ch.send(outbound_message(&id, "two")).await.unwrap();
        ch.send(outbound_message(&id, "three")).await.unwrap();

        let sent = ch.sent_messages().await;
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[2].content, "three");
    }

    #[tokio::test]
    async fn send_preserves_direction() {
        let ch = MockChannel::new(ChannelKind::WebSocket);
        let id = ch.info().id.clone();
        ch.send(inbound_message(&id, "in")).await.unwrap();
        ch.send(outbound_message(&id, "out")).await.unwrap();

        let sent = ch.sent_messages().await;
        assert_eq!(sent[0].direction, MessageDirection::Inbound);
        assert_eq!(sent[1].direction, MessageDirection::Outbound);
    }

    // -- Close --------------------------------------------------------------

    #[tokio::test]
    async fn close_disconnects_channel() {
        let ch = MockChannel::new(ChannelKind::WebSocket);
        assert!(ch.is_connected());
        ch.close().await.unwrap();
        assert!(!ch.is_connected());
    }

    #[tokio::test]
    async fn double_close_returns_error() {
        let ch = MockChannel::new(ChannelKind::WebSocket);
        ch.close().await.unwrap();
        let err = ch.close().await.unwrap_err();
        assert!(matches!(err, ChannelError::AlreadyClosed));
    }

    // -- Error cases --------------------------------------------------------

    #[tokio::test]
    async fn send_after_close_returns_not_connected() {
        let ch = MockChannel::new(ChannelKind::WebSocket);
        ch.close().await.unwrap();
        let err = ch.send(outbound_message("x", "payload")).await.unwrap_err();
        assert!(matches!(err, ChannelError::NotConnected));
    }

    #[tokio::test]
    async fn send_empty_content_returns_invalid_message() {
        let ch = MockChannel::new(ChannelKind::WebSocket);
        let msg = ChannelMessage {
            id: "m1".into(),
            channel_id: ch.info().id.clone(),
            content: String::new(),
            direction: MessageDirection::Outbound,
            timestamp: chrono::Utc::now(),
        };
        let err = ch.send(msg).await.unwrap_err();
        assert!(matches!(err, ChannelError::InvalidMessage(_)));
    }

    // -- ChannelRegistry basics ---------------------------------------------

    #[tokio::test]
    async fn registry_starts_empty() {
        let reg = ChannelRegistry::new();
        assert_eq!(reg.count(), 0);
        assert_eq!(reg.connected_count(), 0);
        assert!(reg.list().is_empty());
    }

    #[tokio::test]
    async fn registry_default_is_empty() {
        let reg = ChannelRegistry::default();
        assert_eq!(reg.count(), 0);
    }

    #[tokio::test]
    async fn register_and_get() {
        let mut reg = ChannelRegistry::new();
        let ch = MockChannel::with_id(ChannelKind::WebSocket, "ws-1");
        let id = reg.register(Box::new(ch));
        assert_eq!(id, "ws-1");
        assert!(reg.get("ws-1").is_some());
        assert_eq!(reg.count(), 1);
    }

    #[tokio::test]
    async fn unregister_removes_channel() {
        let mut reg = ChannelRegistry::new();
        let ch = MockChannel::with_id(ChannelKind::Http, "http-1");
        reg.register(Box::new(ch));

        let removed = reg.unregister("http-1");
        assert!(removed.is_some());
        assert!(reg.get("http-1").is_none());
        assert_eq!(reg.count(), 0);
    }

    #[tokio::test]
    async fn unregister_nonexistent_returns_none() {
        let mut reg = ChannelRegistry::new();
        assert!(reg.unregister("ghost").is_none());
    }

    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let reg = ChannelRegistry::new();
        assert!(reg.get("nope").is_none());
    }

    // -- Filtering by kind --------------------------------------------------

    #[tokio::test]
    async fn list_by_kind_filters_correctly() {
        let mut reg = ChannelRegistry::new();
        reg.register(Box::new(MockChannel::with_id(
            ChannelKind::WebSocket,
            "ws-1",
        )));
        reg.register(Box::new(MockChannel::with_id(ChannelKind::Http, "http-1")));
        reg.register(Box::new(MockChannel::with_id(
            ChannelKind::WebSocket,
            "ws-2",
        )));

        let ws_list = reg.list_by_kind(&ChannelKind::WebSocket);
        assert_eq!(ws_list.len(), 2);

        let http_list = reg.list_by_kind(&ChannelKind::Http);
        assert_eq!(http_list.len(), 1);

        let stdio_list = reg.list_by_kind(&ChannelKind::Stdio);
        assert!(stdio_list.is_empty());
    }

    #[tokio::test]
    async fn count_by_kind() {
        let mut reg = ChannelRegistry::new();
        reg.register(Box::new(MockChannel::with_id(ChannelKind::Mcp, "mcp-1")));
        reg.register(Box::new(MockChannel::with_id(ChannelKind::Mcp, "mcp-2")));
        reg.register(Box::new(MockChannel::with_id(
            ChannelKind::WebSocket,
            "ws-1",
        )));

        assert_eq!(reg.count_by_kind(&ChannelKind::Mcp), 2);
        assert_eq!(reg.count_by_kind(&ChannelKind::WebSocket), 1);
        assert_eq!(reg.count_by_kind(&ChannelKind::Http), 0);
    }

    // -- Connected count tracking -------------------------------------------

    #[tokio::test]
    async fn connected_count_reflects_live_channels() {
        let mut reg = ChannelRegistry::new();
        let ch1 = MockChannel::with_id(ChannelKind::WebSocket, "ws-1");
        let ch2 = MockChannel::with_id(ChannelKind::WebSocket, "ws-2");
        // Close ch2 before registering
        ch2.close().await.unwrap();

        reg.register(Box::new(ch1));
        reg.register(Box::new(ch2));

        assert_eq!(reg.count(), 2);
        assert_eq!(reg.connected_count(), 1);
    }

    // -- Multiple channels of same kind -------------------------------------

    #[tokio::test]
    async fn multiple_channels_same_kind() {
        let mut reg = ChannelRegistry::new();
        for i in 0..5 {
            reg.register(Box::new(MockChannel::with_id(
                ChannelKind::WebSocket,
                format!("ws-{i}"),
            )));
        }
        assert_eq!(reg.count(), 5);
        assert_eq!(reg.count_by_kind(&ChannelKind::WebSocket), 5);
        assert_eq!(reg.connected_count(), 5);
    }

    // -- Custom channel kind ------------------------------------------------

    #[tokio::test]
    async fn custom_channel_kind() {
        let kind = ChannelKind::Custom("grpc".into());
        let ch = MockChannel::with_id(kind.clone(), "grpc-1");
        assert_eq!(ch.kind(), &kind);

        let mut reg = ChannelRegistry::new();
        reg.register(Box::new(ch));
        assert_eq!(reg.count_by_kind(&ChannelKind::Custom("grpc".into())), 1);
        assert_eq!(reg.count_by_kind(&ChannelKind::Custom("other".into())), 0);
    }

    // -- Serialization ------------------------------------------------------

    #[tokio::test]
    async fn channel_info_serializes_to_json() {
        let info = ChannelInfo {
            id: "test-1".into(),
            kind: ChannelKind::WebSocket,
            connected_at: chrono::Utc::now(),
            metadata: HashMap::new(),
            authenticated: true,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"id\":\"test-1\""));
        assert!(json.contains("\"kind\":\"WebSocket\""));
        assert!(json.contains("\"authenticated\":true"));
    }

    #[tokio::test]
    async fn channel_info_roundtrips_json() {
        let mut meta = HashMap::new();
        meta.insert(
            "user_agent".into(),
            serde_json::Value::String("test-agent".into()),
        );
        let info = ChannelInfo {
            id: "rt-1".into(),
            kind: ChannelKind::Http,
            connected_at: chrono::Utc::now(),
            metadata: meta,
            authenticated: false,
        };
        let json = serde_json::to_string(&info).unwrap();
        let restored: ChannelInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, "rt-1");
        assert_eq!(restored.kind, ChannelKind::Http);
        assert!(!restored.authenticated);
        assert_eq!(
            restored.metadata.get("user_agent").unwrap(),
            &serde_json::Value::String("test-agent".into())
        );
    }

    #[tokio::test]
    async fn channel_message_roundtrips_json() {
        let msg = ChannelMessage {
            id: "msg-1".into(),
            channel_id: "ch-1".into(),
            content: "payload".into(),
            direction: MessageDirection::Inbound,
            timestamp: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let restored: ChannelMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, "msg-1");
        assert_eq!(restored.direction, MessageDirection::Inbound);
    }

    #[tokio::test]
    async fn channel_kind_custom_roundtrips_json() {
        let kind = ChannelKind::Custom("mqtt".into());
        let json = serde_json::to_string(&kind).unwrap();
        let restored: ChannelKind = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, ChannelKind::Custom("mqtt".into()));
    }

    // -- Display impl -------------------------------------------------------

    #[tokio::test]
    async fn channel_kind_display() {
        assert_eq!(ChannelKind::WebSocket.to_string(), "WebSocket");
        assert_eq!(ChannelKind::Http.to_string(), "Http");
        assert_eq!(ChannelKind::Stdio.to_string(), "Stdio");
        assert_eq!(ChannelKind::Mcp.to_string(), "Mcp");
        assert_eq!(
            ChannelKind::Custom("grpc".into()).to_string(),
            "Custom(grpc)"
        );
    }

    // -- Error Display impl -------------------------------------------------

    #[tokio::test]
    async fn channel_error_display() {
        assert_eq!(
            ChannelError::NotConnected.to_string(),
            "channel is not connected"
        );
        assert_eq!(
            ChannelError::AlreadyClosed.to_string(),
            "channel is already closed"
        );
        assert_eq!(
            ChannelError::SendFailed("timeout".into()).to_string(),
            "send failed: timeout"
        );
        assert_eq!(
            ChannelError::InvalidMessage("bad".into()).to_string(),
            "invalid message: bad"
        );
    }

    // -- Registry list returns all ------------------------------------------

    #[tokio::test]
    async fn registry_list_returns_all_infos() {
        let mut reg = ChannelRegistry::new();
        reg.register(Box::new(MockChannel::with_id(ChannelKind::WebSocket, "a")));
        reg.register(Box::new(MockChannel::with_id(ChannelKind::Http, "b")));
        reg.register(Box::new(MockChannel::with_id(ChannelKind::Stdio, "c")));

        let infos = reg.list();
        assert_eq!(infos.len(), 3);
        let ids: Vec<&str> = infos.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
        assert!(ids.contains(&"c"));
    }

    // -- Re-registration replaces existing ----------------------------------

    #[tokio::test]
    async fn register_same_id_replaces() {
        let mut reg = ChannelRegistry::new();
        let ch1 = MockChannel::with_id(ChannelKind::WebSocket, "dup");
        let ch2 = MockChannel::with_id(ChannelKind::Http, "dup");

        reg.register(Box::new(ch1));
        assert_eq!(reg.get("dup").unwrap().kind(), &ChannelKind::WebSocket);

        reg.register(Box::new(ch2));
        assert_eq!(reg.count(), 1);
        assert_eq!(reg.get("dup").unwrap().kind(), &ChannelKind::Http);
    }
}
