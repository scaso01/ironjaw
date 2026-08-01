//! Group Conversations with Speaker Selection (I8)
//!
//! Multi-agent group conversations where agents take turns speaking.
//! Supports round-robin, manual, and LLM-based speaker selection.
//! Each group conversation maintains its own message history and
//! terminates after `max_turns` or explicit termination.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Strategy for selecting the next speaker in a group conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerSelection {
    /// Rotate through agents in order.
    RoundRobin,
    /// Caller specifies who speaks next.
    Manual,
    /// An LLM selects the next speaker based on conversation context.
    Llm,
}

/// Status of a group conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupStatus {
    Active,
    Terminated,
}

/// A message in the group conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMessage {
    pub id: String,
    pub group_id: String,
    pub speaker: String,
    pub content: String,
    pub turn: usize,
    pub sent_at: DateTime<Utc>,
}

/// A group conversation session.
pub struct GroupConversation {
    pub id: String,
    pub agents: Vec<String>,
    pub selection: SpeakerSelection,
    pub max_turns: usize,
    pub current_turn: usize,
    pub status: GroupStatus,
    pub history: Vec<GroupMessage>,
    pub created_at: DateTime<Utc>,
    /// Index for round-robin selection.
    round_robin_idx: usize,
    pub metadata: HashMap<String, serde_json::Value>,
}

impl GroupConversation {
    fn new(agents: Vec<String>, selection: SpeakerSelection, max_turns: usize) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            agents,
            selection,
            max_turns,
            current_turn: 0,
            status: GroupStatus::Active,
            history: Vec::new(),
            created_at: Utc::now(),
            round_robin_idx: 0,
            metadata: HashMap::new(),
        }
    }

    /// Get the next speaker using the configured selection strategy.
    /// For `Manual` and `Llm` modes, `hint` provides the caller's choice
    /// or the LLM-selected agent.
    fn next_speaker(&mut self, hint: Option<&str>) -> Option<String> {
        if self.agents.is_empty() {
            return None;
        }

        match &self.selection {
            SpeakerSelection::RoundRobin => {
                let idx = self.round_robin_idx % self.agents.len();
                self.round_robin_idx += 1;
                Some(self.agents[idx].clone())
            }
            SpeakerSelection::Manual | SpeakerSelection::Llm => hint.map(|h| h.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from group conversation operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupError {
    /// The group conversation was not found.
    GroupNotFound(String),
    /// The group has been terminated.
    GroupTerminated(String),
    /// Max turns reached.
    MaxTurnsReached(String),
    /// The specified agent is not a member of the group.
    AgentNotInGroup(String),
    /// Need at least 2 agents for a group conversation.
    TooFewAgents,
    /// No speaker could be determined.
    NoSpeaker,
}

impl std::fmt::Display for GroupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GroupNotFound(id) => write!(f, "group not found: {id}"),
            Self::GroupTerminated(id) => write!(f, "group already terminated: {id}"),
            Self::MaxTurnsReached(id) => write!(f, "max turns reached for group: {id}"),
            Self::AgentNotInGroup(id) => write!(f, "agent not in group: {id}"),
            Self::TooFewAgents => write!(f, "need at least 2 agents for a group"),
            Self::NoSpeaker => write!(f, "no speaker could be determined"),
        }
    }
}

impl std::error::Error for GroupError {}

// ---------------------------------------------------------------------------
// GroupManager
// ---------------------------------------------------------------------------

/// Manages multiple group conversations.
pub struct GroupManager {
    groups: Arc<RwLock<HashMap<String, GroupConversation>>>,
}

impl Default for GroupManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GroupManager {
    pub fn new() -> Self {
        Self {
            groups: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new group conversation.
    pub async fn create(
        &self,
        agents: Vec<String>,
        selection: SpeakerSelection,
        max_turns: usize,
    ) -> Result<String, GroupError> {
        if agents.len() < 2 {
            return Err(GroupError::TooFewAgents);
        }

        let group = GroupConversation::new(agents.clone(), selection, max_turns);
        let id = group.id.clone();

        let mut groups = self.groups.write().await;
        groups.insert(id.clone(), group);

        info!(
            group_id = %id,
            agents = ?agents,
            "Group conversation created"
        );
        Ok(id)
    }

    /// Post a message to a group conversation.
    /// Returns the group message and the next speaker (if determinable).
    pub async fn message(
        &self,
        group_id: &str,
        speaker: &str,
        content: &str,
        next_speaker_hint: Option<&str>,
    ) -> Result<(GroupMessage, Option<String>), GroupError> {
        let mut groups = self.groups.write().await;
        let group = groups
            .get_mut(group_id)
            .ok_or_else(|| GroupError::GroupNotFound(group_id.to_string()))?;

        if group.status == GroupStatus::Terminated {
            return Err(GroupError::GroupTerminated(group_id.to_string()));
        }

        if group.current_turn >= group.max_turns {
            return Err(GroupError::MaxTurnsReached(group_id.to_string()));
        }

        if !group.agents.contains(&speaker.to_string()) {
            return Err(GroupError::AgentNotInGroup(speaker.to_string()));
        }

        group.current_turn += 1;
        let msg = GroupMessage {
            id: Uuid::new_v4().to_string(),
            group_id: group_id.to_string(),
            speaker: speaker.to_string(),
            content: content.to_string(),
            turn: group.current_turn,
            sent_at: Utc::now(),
        };
        group.history.push(msg.clone());

        // Auto-terminate if we hit max turns
        if group.current_turn >= group.max_turns {
            group.status = GroupStatus::Terminated;
            info!(
                group_id = %group_id,
                "Group conversation auto-terminated at max turns"
            );
            return Ok((msg, None));
        }

        let next = group.next_speaker(next_speaker_hint);
        debug!(
            group_id = %group_id,
            speaker = %speaker,
            turn = group.current_turn,
            "Group message posted"
        );

        Ok((msg, next))
    }

    /// Terminate a group conversation. Returns a summary.
    pub async fn terminate(&self, group_id: &str) -> Result<GroupSummary, GroupError> {
        let mut groups = self.groups.write().await;
        let group = groups
            .get_mut(group_id)
            .ok_or_else(|| GroupError::GroupNotFound(group_id.to_string()))?;

        group.status = GroupStatus::Terminated;

        let summary = GroupSummary {
            group_id: group_id.to_string(),
            agents: group.agents.clone(),
            total_turns: group.current_turn,
            message_count: group.history.len(),
            created_at: group.created_at,
            terminated_at: Utc::now(),
        };

        info!(
            group_id = %group_id,
            turns = summary.total_turns,
            "Group conversation terminated"
        );

        Ok(summary)
    }

    /// Get the message history for a group.
    pub async fn history(&self, group_id: &str) -> Result<Vec<GroupMessage>, GroupError> {
        let groups = self.groups.read().await;
        let group = groups
            .get(group_id)
            .ok_or_else(|| GroupError::GroupNotFound(group_id.to_string()))?;
        Ok(group.history.clone())
    }

    /// Get group info (without full history).
    pub async fn get_info(&self, group_id: &str) -> Result<GroupInfo, GroupError> {
        let groups = self.groups.read().await;
        let group = groups
            .get(group_id)
            .ok_or_else(|| GroupError::GroupNotFound(group_id.to_string()))?;

        Ok(GroupInfo {
            id: group.id.clone(),
            agents: group.agents.clone(),
            selection: group.selection.clone(),
            max_turns: group.max_turns,
            current_turn: group.current_turn,
            status: group.status.clone(),
            created_at: group.created_at,
        })
    }

    /// List all group conversation IDs.
    pub async fn list(&self) -> Vec<GroupInfo> {
        let groups = self.groups.read().await;
        groups
            .values()
            .map(|g| GroupInfo {
                id: g.id.clone(),
                agents: g.agents.clone(),
                selection: g.selection.clone(),
                max_turns: g.max_turns,
                current_turn: g.current_turn,
                status: g.status.clone(),
                created_at: g.created_at,
            })
            .collect()
    }

    /// Remove terminated groups older than the given timestamp.
    pub async fn cleanup_terminated(&self, before: DateTime<Utc>) -> usize {
        let mut groups = self.groups.write().await;
        let before_len = groups.len();
        groups.retain(|_, g| !(g.status == GroupStatus::Terminated && g.created_at < before));
        before_len - groups.len()
    }

    /// Get the number of active groups.
    pub async fn active_count(&self) -> usize {
        let groups = self.groups.read().await;
        groups
            .values()
            .filter(|g| g.status == GroupStatus::Active)
            .count()
    }

    /// Get total group count.
    pub async fn total_count(&self) -> usize {
        let groups = self.groups.read().await;
        groups.len()
    }
}

/// Summary produced when a group conversation is terminated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupSummary {
    pub group_id: String,
    pub agents: Vec<String>,
    pub total_turns: usize,
    pub message_count: usize,
    pub created_at: DateTime<Utc>,
    pub terminated_at: DateTime<Utc>,
}

/// Lightweight group info (without history).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupInfo {
    pub id: String,
    pub agents: Vec<String>,
    pub selection: SpeakerSelection,
    pub max_turns: usize,
    pub current_turn: usize,
    pub status: GroupStatus,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_group() {
        let gm = GroupManager::new();
        let id = gm
            .create(
                vec!["a1".into(), "a2".into()],
                SpeakerSelection::RoundRobin,
                10,
            )
            .await
            .unwrap();
        assert!(!id.is_empty());
        assert_eq!(gm.total_count().await, 1);
        assert_eq!(gm.active_count().await, 1);
    }

    #[tokio::test]
    async fn test_create_too_few_agents() {
        let gm = GroupManager::new();
        let err = gm
            .create(vec!["a1".into()], SpeakerSelection::RoundRobin, 10)
            .await
            .unwrap_err();
        assert_eq!(err, GroupError::TooFewAgents);
    }

    #[tokio::test]
    async fn test_round_robin_messaging() {
        let gm = GroupManager::new();
        let gid = gm
            .create(
                vec!["a1".into(), "a2".into(), "a3".into()],
                SpeakerSelection::RoundRobin,
                10,
            )
            .await
            .unwrap();

        // First message from a1
        let (msg, next) = gm.message(&gid, "a1", "hello", None).await.unwrap();
        assert_eq!(msg.speaker, "a1");
        assert_eq!(msg.turn, 1);
        // Round robin should yield a1 first (idx=0), then advance
        assert_eq!(next, Some("a1".into()));

        // Second message from a2
        let (msg2, next2) = gm.message(&gid, "a2", "hi back", None).await.unwrap();
        assert_eq!(msg2.turn, 2);
        assert_eq!(next2, Some("a2".into()));
    }

    #[tokio::test]
    async fn test_manual_speaker_selection() {
        let gm = GroupManager::new();
        let gid = gm
            .create(vec!["a1".into(), "a2".into()], SpeakerSelection::Manual, 10)
            .await
            .unwrap();

        let (_, next) = gm.message(&gid, "a1", "hello", Some("a2")).await.unwrap();
        assert_eq!(next, Some("a2".into()));

        let (_, next2) = gm.message(&gid, "a2", "reply", Some("a1")).await.unwrap();
        assert_eq!(next2, Some("a1".into()));
    }

    #[tokio::test]
    async fn test_max_turns_auto_terminate() {
        let gm = GroupManager::new();
        let gid = gm
            .create(
                vec!["a1".into(), "a2".into()],
                SpeakerSelection::RoundRobin,
                2,
            )
            .await
            .unwrap();

        gm.message(&gid, "a1", "msg1", None).await.unwrap();
        let (_, next) = gm.message(&gid, "a2", "msg2", None).await.unwrap();
        // Should be terminated, no next speaker
        assert_eq!(next, None);

        // Further messages should fail
        let err = gm.message(&gid, "a1", "msg3", None).await.unwrap_err();
        assert_eq!(err, GroupError::GroupTerminated(gid));
    }

    #[tokio::test]
    async fn test_agent_not_in_group() {
        let gm = GroupManager::new();
        let gid = gm
            .create(
                vec!["a1".into(), "a2".into()],
                SpeakerSelection::RoundRobin,
                10,
            )
            .await
            .unwrap();

        let err = gm.message(&gid, "a3", "hello", None).await.unwrap_err();
        assert_eq!(err, GroupError::AgentNotInGroup("a3".into()));
    }

    #[tokio::test]
    async fn test_terminate() {
        let gm = GroupManager::new();
        let gid = gm
            .create(
                vec!["a1".into(), "a2".into()],
                SpeakerSelection::RoundRobin,
                10,
            )
            .await
            .unwrap();

        gm.message(&gid, "a1", "hello", None).await.unwrap();
        let summary = gm.terminate(&gid).await.unwrap();
        assert_eq!(summary.total_turns, 1);
        assert_eq!(summary.message_count, 1);
        assert_eq!(gm.active_count().await, 0);
    }

    #[tokio::test]
    async fn test_terminate_not_found() {
        let gm = GroupManager::new();
        let err = gm.terminate("nope").await.unwrap_err();
        assert_eq!(err, GroupError::GroupNotFound("nope".into()));
    }

    #[tokio::test]
    async fn test_history() {
        let gm = GroupManager::new();
        let gid = gm
            .create(
                vec!["a1".into(), "a2".into()],
                SpeakerSelection::RoundRobin,
                10,
            )
            .await
            .unwrap();

        gm.message(&gid, "a1", "first", None).await.unwrap();
        gm.message(&gid, "a2", "second", None).await.unwrap();

        let hist = gm.history(&gid).await.unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].content, "first");
        assert_eq!(hist[1].content, "second");
    }

    #[tokio::test]
    async fn test_get_info() {
        let gm = GroupManager::new();
        let gid = gm
            .create(vec!["a1".into(), "a2".into()], SpeakerSelection::Manual, 5)
            .await
            .unwrap();

        let info = gm.get_info(&gid).await.unwrap();
        assert_eq!(info.agents, vec!["a1", "a2"]);
        assert_eq!(info.selection, SpeakerSelection::Manual);
        assert_eq!(info.max_turns, 5);
        assert_eq!(info.status, GroupStatus::Active);
    }

    #[tokio::test]
    async fn test_list_groups() {
        let gm = GroupManager::new();
        gm.create(
            vec!["a1".into(), "a2".into()],
            SpeakerSelection::RoundRobin,
            10,
        )
        .await
        .unwrap();
        gm.create(vec!["a3".into(), "a4".into()], SpeakerSelection::Manual, 5)
            .await
            .unwrap();

        let list = gm.list().await;
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn test_group_not_found() {
        let gm = GroupManager::new();
        let err = gm.message("nope", "a1", "hi", None).await.unwrap_err();
        assert_eq!(err, GroupError::GroupNotFound("nope".into()));
    }

    #[tokio::test]
    async fn test_error_display() {
        assert_eq!(
            GroupError::GroupNotFound("g1".into()).to_string(),
            "group not found: g1"
        );
        assert_eq!(
            GroupError::GroupTerminated("g1".into()).to_string(),
            "group already terminated: g1"
        );
        assert_eq!(
            GroupError::MaxTurnsReached("g1".into()).to_string(),
            "max turns reached for group: g1"
        );
        assert_eq!(
            GroupError::AgentNotInGroup("a1".into()).to_string(),
            "agent not in group: a1"
        );
        assert_eq!(
            GroupError::TooFewAgents.to_string(),
            "need at least 2 agents for a group"
        );
        assert_eq!(
            GroupError::NoSpeaker.to_string(),
            "no speaker could be determined"
        );
    }

    #[tokio::test]
    async fn test_cleanup_terminated() {
        let gm = GroupManager::new();
        let gid = gm
            .create(
                vec!["a1".into(), "a2".into()],
                SpeakerSelection::RoundRobin,
                10,
            )
            .await
            .unwrap();
        gm.terminate(&gid).await.unwrap();

        // All groups created before "now+1s" should be cleaned up
        let cutoff = Utc::now() + chrono::Duration::seconds(1);
        let removed = gm.cleanup_terminated(cutoff).await;
        assert_eq!(removed, 1);
        assert_eq!(gm.total_count().await, 0);
    }
}
