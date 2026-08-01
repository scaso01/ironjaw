use serde::{Deserialize, Serialize};

/// Notification priority levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Immediate push to all devices (vibrate + sound)
    Critical,
    /// Standard notification
    Normal,
    /// Silent / low-priority
    Low,
    /// Batched — accumulated and sent as summary
    Digest,
}

impl Priority {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "critical" => Some(Priority::Critical),
            "normal" => Some(Priority::Normal),
            "low" => Some(Priority::Low),
            "digest" => Some(Priority::Digest),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Priority::Critical => "critical",
            Priority::Normal => "normal",
            Priority::Low => "low",
            Priority::Digest => "digest",
        }
    }

    /// Numeric priority for queue ordering (higher = more important).
    pub fn numeric(&self) -> u8 {
        match self {
            Priority::Critical => 3,
            Priority::Normal => 2,
            Priority::Low => 1,
            Priority::Digest => 0,
        }
    }
}

/// A notification to be delivered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: String,
    pub priority: Priority,
    pub title: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl Notification {
    pub fn new(priority: Priority, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            priority,
            title: title.into(),
            body: body.into(),
            data: None,
            created_at: chrono::Utc::now(),
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    /// Convert to a JSON event frame payload.
    pub fn to_event_data(&self) -> serde_json::Value {
        serde_json::json!({
            "notification_id": self.id,
            "priority": self.priority.as_str(),
            "title": self.title,
            "body": self.body,
            "data": self.data,
            "created_at": self.created_at.to_rfc3339(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_priority_from_str() {
        assert_eq!(Priority::from_str("critical"), Some(Priority::Critical));
        assert_eq!(Priority::from_str("normal"), Some(Priority::Normal));
        assert_eq!(Priority::from_str("low"), Some(Priority::Low));
        assert_eq!(Priority::from_str("digest"), Some(Priority::Digest));
        assert_eq!(Priority::from_str("unknown"), None);
    }

    #[test]
    fn test_priority_numeric_ordering() {
        assert!(Priority::Critical.numeric() > Priority::Normal.numeric());
        assert!(Priority::Normal.numeric() > Priority::Low.numeric());
        assert!(Priority::Low.numeric() > Priority::Digest.numeric());
    }

    #[test]
    fn test_notification_new() {
        let n = Notification::new(Priority::Normal, "Test", "Body text");
        assert_eq!(n.title, "Test");
        assert_eq!(n.body, "Body text");
        assert!(n.data.is_none());
        assert!(!n.id.is_empty());
    }

    #[test]
    fn test_notification_with_data() {
        let n = Notification::new(Priority::Critical, "Alert", "Server down")
            .with_data(serde_json::json!({"server": "docker-host"}));
        assert!(n.data.is_some());
        assert_eq!(n.data.unwrap()["server"], "docker-host");
    }

    #[test]
    fn test_to_event_data() {
        let n = Notification::new(Priority::Normal, "Test", "Body");
        let data = n.to_event_data();
        assert_eq!(data["priority"], "normal");
        assert_eq!(data["title"], "Test");
        assert_eq!(data["body"], "Body");
    }

    #[test]
    fn test_priority_roundtrip() {
        for p in [
            Priority::Critical,
            Priority::Normal,
            Priority::Low,
            Priority::Digest,
        ] {
            let s = p.as_str();
            assert_eq!(Priority::from_str(s), Some(p));
        }
    }
}
