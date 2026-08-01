//! Participant awareness for Ironjaw sessions.
//!
//! Tracks who/what is participating in a conversation -- humans, AI agents,
//! tools, MCP servers, and external services. Each session maintains its own
//! roster of participants with join/leave semantics and metadata.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The kind of entity participating in a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ParticipantKind {
    /// A human user interacting via a client.
    Human,
    /// An AI agent (e.g., LLM, sub-agent).
    Agent,
    /// A tool providing capabilities (e.g., read_file, shell).
    Tool,
    /// An MCP server exposing tools or resources.
    McpServer,
    /// An external service (e.g., SearXNG, CC Bridge).
    ExternalService,
}

/// A single participant in a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Participant {
    /// Unique identifier for this participant.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// What kind of entity this participant is.
    pub kind: ParticipantKind,
    /// When this participant joined the session.
    pub joined_at: chrono::DateTime<chrono::Utc>,
    /// Arbitrary key-value metadata (capabilities, versions, etc.).
    pub metadata: HashMap<String, serde_json::Value>,
    /// Whether this participant is currently active.
    pub active: bool,
}

impl Participant {
    /// Create a new active participant with the given id, name, and kind.
    pub fn new(id: impl Into<String>, name: impl Into<String>, kind: ParticipantKind) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind,
            joined_at: chrono::Utc::now(),
            metadata: HashMap::new(),
            active: true,
        }
    }

    /// Create a new participant with custom metadata.
    pub fn with_metadata(
        id: impl Into<String>,
        name: impl Into<String>,
        kind: ParticipantKind,
        metadata: HashMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind,
            joined_at: chrono::Utc::now(),
            metadata,
            active: true,
        }
    }
}

/// Registry tracking participants across all sessions.
///
/// Each session has its own independent list of participants. Participants
/// can join, leave (marked inactive), and be queried by kind or status.
pub struct ParticipantRegistry {
    /// session_id -> list of participants
    sessions: HashMap<String, Vec<Participant>>,
}

impl Default for ParticipantRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ParticipantRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    /// Add a participant to a session. If the session does not exist yet, it
    /// is created automatically.
    pub fn join(&mut self, session_id: &str, participant: Participant) {
        self.sessions
            .entry(session_id.to_string())
            .or_default()
            .push(participant);
    }

    /// Mark a participant as inactive (left). Returns `true` if the participant
    /// was found and marked inactive, `false` if not found.
    pub fn leave(&mut self, session_id: &str, participant_id: &str) -> bool {
        if let Some(participants) = self.sessions.get_mut(session_id) {
            for p in participants.iter_mut() {
                if p.id == participant_id && p.active {
                    p.active = false;
                    return true;
                }
            }
        }
        false
    }

    /// Look up a specific participant in a session by id.
    pub fn get(&self, session_id: &str, participant_id: &str) -> Option<&Participant> {
        self.sessions
            .get(session_id)?
            .iter()
            .find(|p| p.id == participant_id)
    }

    /// List all participants (active and inactive) in a session.
    pub fn list(&self, session_id: &str) -> Vec<&Participant> {
        self.sessions
            .get(session_id)
            .map(|ps| ps.iter().collect())
            .unwrap_or_default()
    }

    /// List only active participants in a session.
    pub fn list_active(&self, session_id: &str) -> Vec<&Participant> {
        self.sessions
            .get(session_id)
            .map(|ps| ps.iter().filter(|p| p.active).collect())
            .unwrap_or_default()
    }

    /// List participants of a specific kind in a session (active and inactive).
    pub fn list_by_kind(&self, session_id: &str, kind: &ParticipantKind) -> Vec<&Participant> {
        self.sessions
            .get(session_id)
            .map(|ps| ps.iter().filter(|p| &p.kind == kind).collect())
            .unwrap_or_default()
    }

    /// Remove all participant data for a session. Called when a session is
    /// destroyed or cleaned up.
    pub fn cleanup_session(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
    }

    /// Returns the number of sessions currently tracked.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_human(id: &str) -> Participant {
        Participant::new(id, format!("Human {id}"), ParticipantKind::Human)
    }

    fn make_agent(id: &str) -> Participant {
        Participant::new(id, format!("Agent {id}"), ParticipantKind::Agent)
    }

    fn make_tool(id: &str) -> Participant {
        Participant::new(id, format!("Tool {id}"), ParticipantKind::Tool)
    }

    #[test]
    fn test_join_and_list() {
        let mut registry = ParticipantRegistry::new();
        registry.join("s1", make_human("h1"));
        registry.join("s1", make_agent("a1"));

        let all = registry.list("s1");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "h1");
        assert_eq!(all[1].id, "a1");
    }

    #[test]
    fn test_leave_marks_inactive() {
        let mut registry = ParticipantRegistry::new();
        registry.join("s1", make_human("h1"));

        assert!(registry.leave("s1", "h1"));

        let p = registry.get("s1", "h1").unwrap();
        assert!(!p.active);
    }

    #[test]
    fn test_leave_nonexistent_returns_false() {
        let mut registry = ParticipantRegistry::new();
        assert!(!registry.leave("s1", "nope"));

        registry.join("s1", make_human("h1"));
        assert!(!registry.leave("s1", "nope"));
    }

    #[test]
    fn test_leave_already_inactive_returns_false() {
        let mut registry = ParticipantRegistry::new();
        registry.join("s1", make_human("h1"));

        assert!(registry.leave("s1", "h1"));
        // Second leave should return false (already inactive).
        assert!(!registry.leave("s1", "h1"));
    }

    #[test]
    fn test_get_existing() {
        let mut registry = ParticipantRegistry::new();
        registry.join("s1", make_human("h1"));

        let p = registry.get("s1", "h1").unwrap();
        assert_eq!(p.name, "Human h1");
        assert_eq!(p.kind, ParticipantKind::Human);
        assert!(p.active);
    }

    #[test]
    fn test_get_nonexistent() {
        let registry = ParticipantRegistry::new();
        assert!(registry.get("s1", "h1").is_none());
    }

    #[test]
    fn test_list_active() {
        let mut registry = ParticipantRegistry::new();
        registry.join("s1", make_human("h1"));
        registry.join("s1", make_agent("a1"));
        registry.leave("s1", "a1");

        let active = registry.list_active("s1");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "h1");
    }

    #[test]
    fn test_list_by_kind() {
        let mut registry = ParticipantRegistry::new();
        registry.join("s1", make_human("h1"));
        registry.join("s1", make_agent("a1"));
        registry.join("s1", make_agent("a2"));
        registry.join("s1", make_tool("t1"));

        let agents = registry.list_by_kind("s1", &ParticipantKind::Agent);
        assert_eq!(agents.len(), 2);

        let tools = registry.list_by_kind("s1", &ParticipantKind::Tool);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].id, "t1");

        let mcps = registry.list_by_kind("s1", &ParticipantKind::McpServer);
        assert!(mcps.is_empty());
    }

    #[test]
    fn test_cleanup_session() {
        let mut registry = ParticipantRegistry::new();
        registry.join("s1", make_human("h1"));
        registry.join("s2", make_human("h2"));
        assert_eq!(registry.session_count(), 2);

        registry.cleanup_session("s1");
        assert_eq!(registry.session_count(), 1);
        assert!(registry.list("s1").is_empty());
        assert_eq!(registry.list("s2").len(), 1);
    }

    #[test]
    fn test_session_count() {
        let mut registry = ParticipantRegistry::new();
        assert_eq!(registry.session_count(), 0);

        registry.join("s1", make_human("h1"));
        assert_eq!(registry.session_count(), 1);

        registry.join("s2", make_agent("a1"));
        assert_eq!(registry.session_count(), 2);

        registry.cleanup_session("s1");
        assert_eq!(registry.session_count(), 1);
    }

    #[test]
    fn test_list_empty_session() {
        let registry = ParticipantRegistry::new();
        assert!(registry.list("nonexistent").is_empty());
        assert!(registry.list_active("nonexistent").is_empty());
        assert!(registry
            .list_by_kind("nonexistent", &ParticipantKind::Human)
            .is_empty());
    }

    #[test]
    fn test_participant_serialization() {
        let p = make_human("h1");
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["id"], "h1");
        assert_eq!(json["name"], "Human h1");
        assert_eq!(json["kind"], "Human");
        assert_eq!(json["active"], true);
        assert!(json["joined_at"].is_string());
    }

    #[test]
    fn test_participant_deserialization() {
        let json_str = r#"{
            "id": "a1",
            "name": "Claude",
            "kind": "Agent",
            "joined_at": "2026-03-11T10:00:00Z",
            "metadata": {"model": "opus"},
            "active": true
        }"#;
        let p: Participant = serde_json::from_str(json_str).unwrap();
        assert_eq!(p.id, "a1");
        assert_eq!(p.name, "Claude");
        assert_eq!(p.kind, ParticipantKind::Agent);
        assert!(p.active);
        assert_eq!(p.metadata["model"], json!("opus"));
    }

    #[test]
    fn test_participant_with_metadata() {
        let mut meta = HashMap::new();
        meta.insert("version".to_string(), json!("1.0"));
        meta.insert("capabilities".to_string(), json!(["search", "code"]));

        let p = Participant::with_metadata("mcp1", "SearXNG", ParticipantKind::McpServer, meta);
        assert_eq!(p.id, "mcp1");
        assert_eq!(p.kind, ParticipantKind::McpServer);
        assert_eq!(p.metadata["version"], json!("1.0"));
        assert_eq!(p.metadata["capabilities"], json!(["search", "code"]));
    }

    #[test]
    fn test_participant_kind_variants() {
        // Verify all variants serialize correctly.
        let kinds = vec![
            (ParticipantKind::Human, "Human"),
            (ParticipantKind::Agent, "Agent"),
            (ParticipantKind::Tool, "Tool"),
            (ParticipantKind::McpServer, "McpServer"),
            (ParticipantKind::ExternalService, "ExternalService"),
        ];
        for (kind, expected) in kinds {
            let json = serde_json::to_value(&kind).unwrap();
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn test_multiple_sessions_isolated() {
        let mut registry = ParticipantRegistry::new();
        registry.join("s1", make_human("h1"));
        registry.join("s2", make_agent("a1"));

        assert_eq!(registry.list("s1").len(), 1);
        assert_eq!(registry.list("s1")[0].kind, ParticipantKind::Human);

        assert_eq!(registry.list("s2").len(), 1);
        assert_eq!(registry.list("s2")[0].kind, ParticipantKind::Agent);

        // Leaving in s1 does not affect s2.
        registry.leave("s1", "h1");
        assert_eq!(registry.list_active("s1").len(), 0);
        assert_eq!(registry.list_active("s2").len(), 1);
    }

    #[test]
    fn test_default_trait() {
        let registry = ParticipantRegistry::default();
        assert_eq!(registry.session_count(), 0);
    }
}
