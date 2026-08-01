use axum::extract::ws::Message;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};

use crate::protocol::Frame;

/// Information about a connected device
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub device_type: String,
    pub name: String,
    pub capabilities: Vec<String>,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub last_seen: chrono::DateTime<chrono::Utc>,
}

/// Internal entry for a connected device
struct DeviceEntry {
    info: DeviceInfo,
    tx: mpsc::UnboundedSender<Message>,
}

/// Registry of connected devices. Thread-safe via RwLock.
/// Each device has a message sender for pushing frames.
#[derive(Clone)]
pub struct DeviceRegistry {
    devices: Arc<RwLock<HashMap<String, DeviceEntry>>>,
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            devices: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a new device connection. Returns the device ID.
    pub async fn register(
        &self,
        connection_id: &str,
        device_type: &str,
        name: &str,
        capabilities: Vec<String>,
        tx: mpsc::UnboundedSender<Message>,
    ) -> String {
        let device_id = connection_id.to_string();
        let now = chrono::Utc::now();
        let info = DeviceInfo {
            id: device_id.clone(),
            device_type: device_type.to_string(),
            name: name.to_string(),
            capabilities,
            connected_at: now,
            last_seen: now,
        };
        let entry = DeviceEntry { info, tx };
        self.devices.write().await.insert(device_id.clone(), entry);
        info!(device_id = %device_id, device_type = %device_type, "Device registered");
        // Broadcast presence event after write lock is released
        self.broadcast_presence().await;
        device_id
    }

    /// Unregister a device (on disconnect).
    pub async fn unregister(&self, connection_id: &str) {
        let removed = self.devices.write().await.remove(connection_id).is_some();
        if removed {
            info!(device_id = %connection_id, "Device unregistered");
            // Broadcast presence event after write lock is released
            self.broadcast_presence().await;
        }
    }

    /// Broadcast a `device.presence` event with the current device list to all clients.
    async fn broadcast_presence(&self) {
        let devices: Vec<serde_json::Value> = self
            .devices
            .read()
            .await
            .values()
            .map(|e| {
                serde_json::json!({
                    "id": e.info.id,
                    "device_type": e.info.device_type,
                    "name": e.info.name,
                    "connected_at": e.info.connected_at,
                })
            })
            .collect();
        let frame = Frame::event("device.presence", serde_json::json!({ "devices": devices }));
        if let Ok(text) = serde_json::to_string(&frame) {
            self.broadcast_text(text).await;
        }
    }

    /// List all connected devices.
    pub async fn list(&self) -> Vec<DeviceInfo> {
        self.devices
            .read()
            .await
            .values()
            .map(|e| e.info.clone())
            .collect()
    }

    /// Send a JSON text message to a specific device.
    pub async fn send_to(&self, device_id: &str, message: Message) -> Result<(), String> {
        let devices = self.devices.read().await;
        match devices.get(device_id) {
            Some(entry) => entry
                .tx
                .send(message)
                .map_err(|_| format!("Device '{device_id}' channel closed")),
            None => Err(format!("Device '{device_id}' not found")),
        }
    }

    /// Send a text message to a specific device.
    pub async fn send_text(&self, device_id: &str, text: String) -> Result<(), String> {
        self.send_to(device_id, Message::Text(text.into())).await
    }

    /// Broadcast a message to ALL connected devices.
    pub async fn broadcast(&self, message: Message) {
        let devices = self.devices.read().await;
        for (id, entry) in devices.iter() {
            if entry.tx.send(message.clone()).is_err() {
                warn!(device_id = %id, "Failed to broadcast to device (channel closed)");
            }
        }
    }

    /// Broadcast text to all connected devices.
    pub async fn broadcast_text(&self, text: String) {
        self.broadcast(Message::Text(text.into())).await;
    }

    /// Get count of connected devices.
    pub async fn count(&self) -> usize {
        self.devices.read().await.len()
    }

    /// Update last_seen timestamp for a device.
    pub async fn touch(&self, device_id: &str) {
        let mut devices = self.devices.write().await;
        if let Some(entry) = devices.get_mut(device_id) {
            entry.info.last_seen = chrono::Utc::now();
        }
    }

    /// Check if a device is connected.
    pub async fn is_connected(&self, device_id: &str) -> bool {
        self.devices.read().await.contains_key(device_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_and_list() {
        let registry = DeviceRegistry::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let id = registry
            .register(
                "conn-1",
                "android",
                "Dr. CLAW",
                vec!["voice".to_string()],
                tx,
            )
            .await;
        assert_eq!(id, "conn-1");
        let devices = registry.list().await;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device_type, "android");
        assert_eq!(devices[0].name, "Dr. CLAW");
    }

    #[tokio::test]
    async fn test_unregister() {
        let registry = DeviceRegistry::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        registry
            .register("conn-1", "android", "Phone", vec![], tx)
            .await;
        assert_eq!(registry.count().await, 1);
        registry.unregister("conn-1").await;
        assert_eq!(registry.count().await, 0);
    }

    #[tokio::test]
    async fn test_send_to() {
        let registry = DeviceRegistry::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        registry
            .register("conn-1", "android", "Phone", vec![], tx)
            .await;
        // Drain presence event from register
        let _ = rx.recv().await.unwrap();
        registry
            .send_text("conn-1", "hello".to_string())
            .await
            .unwrap();
        let msg = rx.recv().await.unwrap();
        match msg {
            Message::Text(t) => assert_eq!(t.to_string(), "hello"),
            _ => panic!("Expected text message"),
        }
    }

    #[tokio::test]
    async fn test_send_to_unknown() {
        let registry = DeviceRegistry::new();
        let result = registry.send_text("unknown", "hello".to_string()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_broadcast() {
        let registry = DeviceRegistry::new();
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        registry
            .register("conn-1", "android", "Phone", vec![], tx1)
            .await;
        // Drain presence event from conn-1 register (only conn-1 exists)
        let _ = rx1.recv().await.unwrap();
        registry
            .register("conn-2", "desktop", "workstation", vec![], tx2)
            .await;
        // Drain presence events from conn-2 register (broadcast to both)
        let _ = rx1.recv().await.unwrap();
        let _ = rx2.recv().await.unwrap();
        registry.broadcast_text("broadcast msg".to_string()).await;
        let msg1 = rx1.recv().await.unwrap();
        let msg2 = rx2.recv().await.unwrap();
        match (msg1, msg2) {
            (Message::Text(t1), Message::Text(t2)) => {
                assert_eq!(t1.to_string(), "broadcast msg");
                assert_eq!(t2.to_string(), "broadcast msg");
            }
            _ => panic!("Expected text messages"),
        }
    }

    #[tokio::test]
    async fn test_touch() {
        let registry = DeviceRegistry::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        registry
            .register("conn-1", "android", "Phone", vec![], tx)
            .await;
        let before = registry.list().await[0].last_seen;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        registry.touch("conn-1").await;
        let after = registry.list().await[0].last_seen;
        assert!(after > before);
    }

    #[tokio::test]
    async fn test_is_connected() {
        let registry = DeviceRegistry::new();
        assert!(!registry.is_connected("conn-1").await);
        let (tx, _rx) = mpsc::unbounded_channel();
        registry
            .register("conn-1", "android", "Phone", vec![], tx)
            .await;
        assert!(registry.is_connected("conn-1").await);
    }
}
