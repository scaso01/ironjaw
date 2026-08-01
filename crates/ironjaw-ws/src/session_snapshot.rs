use ironjaw_session::types::{Session, SessionMessage};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub id: String,
    pub session_id: String,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub label: Option<String>,
    pub session: Session,
    pub messages: Vec<SessionMessage>,
}

pub struct SessionSnapshotStore {
    inner: RwLock<HashMap<String, VecDeque<SessionSnapshot>>>,
    max_per_session: usize,
}

impl SessionSnapshotStore {
    pub fn new(max_per_session: usize) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            max_per_session,
        }
    }

    pub async fn create(
        &self,
        label: Option<String>,
        session: Session,
        messages: Vec<SessionMessage>,
    ) -> SessionSnapshot {
        let snapshot = SessionSnapshot {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session.id.clone(),
            captured_at: chrono::Utc::now(),
            label,
            session,
            messages,
        };

        let mut inner = self.inner.write().await;
        let bucket = inner.entry(snapshot.session_id.clone()).or_default();
        if bucket.len() >= self.max_per_session {
            bucket.pop_front();
        }
        bucket.push_back(snapshot.clone());

        snapshot
    }

    pub async fn list(&self, session_id: &str) -> Vec<SessionSnapshot> {
        let inner = self.inner.read().await;
        inner
            .get(session_id)
            .map(|bucket| bucket.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub async fn get(&self, snapshot_id: &str) -> Option<SessionSnapshot> {
        let inner = self.inner.read().await;
        for bucket in inner.values() {
            for snap in bucket {
                if snap.id == snapshot_id {
                    return Some(snap.clone());
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironjaw_session::types::SessionMode;

    fn make_session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            user_id: "user-1".to_string(),
            title: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            context_tokens: 0,
            compaction_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            message_count: None,
            pinned: false,
            archived: false,
            mode: SessionMode::Default,
            working_dir: None,
        }
    }

    #[tokio::test]
    async fn create_returns_snapshot_with_generated_id() {
        let store = SessionSnapshotStore::new(5);
        let session = make_session("sess-1");
        let snap = store
            .create(Some("before compact".to_string()), session, vec![])
            .await;

        assert!(!snap.id.is_empty());
        assert_eq!(snap.session_id, "sess-1");
        assert_eq!(snap.label.as_deref(), Some("before compact"));
    }

    #[tokio::test]
    async fn list_returns_snapshots_for_session() {
        let store = SessionSnapshotStore::new(5);
        let session = make_session("sess-2");
        store.create(None, session.clone(), vec![]).await;
        store
            .create(Some("second".to_string()), session, vec![])
            .await;

        let snaps = store.list("sess-2").await;
        assert_eq!(snaps.len(), 2);
    }

    #[tokio::test]
    async fn list_empty_for_unknown_session() {
        let store = SessionSnapshotStore::new(5);
        let snaps = store.list("nobody").await;
        assert!(snaps.is_empty());
    }

    #[tokio::test]
    async fn get_finds_by_id() {
        let store = SessionSnapshotStore::new(5);
        let session = make_session("sess-3");
        let created = store.create(None, session, vec![]).await;

        let found = store.get(&created.id).await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, created.id);
    }

    #[tokio::test]
    async fn get_returns_none_for_missing_id() {
        let store = SessionSnapshotStore::new(5);
        assert!(store.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn evicts_oldest_when_full() {
        let store = SessionSnapshotStore::new(2);
        let session = make_session("sess-4");

        let first = store
            .create(Some("first".to_string()), session.clone(), vec![])
            .await;
        store
            .create(Some("second".to_string()), session.clone(), vec![])
            .await;
        store
            .create(Some("third".to_string()), session, vec![])
            .await;

        let snaps = store.list("sess-4").await;
        assert_eq!(snaps.len(), 2);
        // first should have been evicted
        assert!(snaps.iter().all(|s| s.id != first.id));
        assert_eq!(snaps[0].label.as_deref(), Some("second"));
        assert_eq!(snaps[1].label.as_deref(), Some("third"));
    }

    #[tokio::test]
    async fn snapshots_across_sessions_are_isolated() {
        let store = SessionSnapshotStore::new(5);
        store.create(None, make_session("sess-a"), vec![]).await;
        store.create(None, make_session("sess-b"), vec![]).await;

        assert_eq!(store.list("sess-a").await.len(), 1);
        assert_eq!(store.list("sess-b").await.len(), 1);
        assert_eq!(store.list("sess-c").await.len(), 0);
    }
}
