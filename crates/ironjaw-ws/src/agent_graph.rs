//! Multi-Agent Communication Graph (Stream 8.5 + I5/I9)
//!
//! A directed graph tracking agent-to-agent messaging. Each agent is a
//! node, each message creates a directed edge. Provides graph queries
//! for neighbor discovery, message filtering, and history cleanup.
//!
//! Extended with:
//! - **Handoff Protocol** (I5): transfer session ownership between agents
//!   with context filtering (last_5, summary, full, none).
//! - **Scatter-Gather** (I9): fan out a message to multiple agents
//!   concurrently and aggregate their responses.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A node in the agent communication graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNode {
    pub id: String,
    pub name: String,
    /// One of: `"sub-agent"`, `"mcp-server"`, `"external"`, `"coordinator"`.
    pub agent_type: String,
    pub status: AgentStatus,
    pub created_at: DateTime<Utc>,
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Runtime status of an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Active,
    Idle,
    Busy,
    Disconnected,
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Idle => write!(f, "idle"),
            Self::Busy => write!(f, "busy"),
            Self::Disconnected => write!(f, "disconnected"),
        }
    }
}

/// A directed message (edge) between two agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub id: String,
    /// Source agent ID.
    pub from: String,
    /// Destination agent ID.
    pub to: String,
    pub content: String,
    pub message_type: MessageType,
    pub sent_at: DateTime<Utc>,
    pub acknowledged: bool,
}

/// The semantic type of an agent-to-agent message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageType {
    /// Task delegation.
    Task,
    /// Task result.
    Result,
    /// Information request.
    Query,
    /// Information response.
    Response,
    /// Broadcast to all connected agents.
    Broadcast,
    /// Agent status change notification.
    StatusUpdate,
}

// ---------------------------------------------------------------------------
// Handoff (I5)
// ---------------------------------------------------------------------------

/// How to filter conversation context during a handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffFilter {
    /// Send only the last 5 messages.
    Last5,
    /// Generate a summary of the conversation.
    Summary,
    /// Send the full conversation history.
    Full,
    /// Send no context at all.
    None,
}

/// Metadata attached to a handoff request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffMetadata {
    pub reason: String,
    #[serde(default)]
    pub priority: Option<String>,
}

/// A completed handoff record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffRecord {
    pub id: String,
    pub from: String,
    pub to: String,
    pub filter: HandoffFilter,
    pub metadata: HandoffMetadata,
    pub context_messages: Vec<AgentMessage>,
    pub completed_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Scatter-Gather (I9)
// ---------------------------------------------------------------------------

/// Aggregation strategy for scatter-gather responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AggregationStrategy {
    /// Concatenate all responses.
    Concat,
    /// Return the first response received.
    First,
    /// Return the best response (longest, as a simple heuristic).
    Best,
    /// Majority vote (most common response).
    Vote,
}

/// A single agent's response in a scatter-gather operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScatterResponse {
    pub agent_id: String,
    pub content: Option<String>,
    pub responded: bool,
    pub responded_at: Option<DateTime<Utc>>,
}

/// Result of a scatter-gather operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScatterResult {
    pub id: String,
    pub responses: Vec<ScatterResponse>,
    pub aggregated: String,
    pub strategy: AggregationStrategy,
    pub timed_out_agents: Vec<String>,
    pub completed_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur when sending a message through the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentGraphError {
    /// The specified agent ID was not found in the graph.
    AgentNotFound(String),
    /// An agent cannot send a message to itself.
    SelfMessage,
    /// Handoff target cannot be the same as source.
    HandoffSameAgent,
    /// No agents specified for scatter operation.
    EmptyScatterList,
    /// A scatter-gather operation timed out with zero responses.
    ScatterAllTimedOut,
}

impl fmt::Display for AgentGraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AgentNotFound(id) => {
                write!(f, "agent not found: {id}")
            }
            Self::SelfMessage => {
                write!(f, "agent cannot send a message to itself")
            }
            Self::HandoffSameAgent => {
                write!(f, "cannot handoff to the same agent")
            }
            Self::EmptyScatterList => {
                write!(f, "scatter requires at least one target agent")
            }
            Self::ScatterAllTimedOut => {
                write!(f, "all scatter targets timed out")
            }
        }
    }
}

impl std::error::Error for AgentGraphError {}

// ---------------------------------------------------------------------------
// AgentGraph
// ---------------------------------------------------------------------------

/// Directed communication graph tracking agent-to-agent messages.
///
/// When `max_history` edges are exceeded, the oldest messages are removed.
/// Supports handoff (I5) and scatter-gather (I9) operations.
pub struct AgentGraph {
    nodes: Arc<RwLock<HashMap<String, AgentNode>>>,
    edges: Arc<RwLock<Vec<AgentMessage>>>,
    max_history: usize,
    /// Audit trail of completed handoffs.
    handoff_records: Arc<RwLock<Vec<HandoffRecord>>>,
}

impl AgentGraph {
    /// Create a new agent graph with the given maximum message history.
    pub fn new(max_history: usize) -> Self {
        Self {
            nodes: Arc::new(RwLock::new(HashMap::new())),
            edges: Arc::new(RwLock::new(Vec::new())),
            max_history,
            handoff_records: Arc::new(RwLock::new(Vec::new())),
        }
    }

    // ── Node Management ────────────────────────────────────────────

    /// Register an agent in the graph.
    pub async fn register_agent(&self, agent: AgentNode) {
        let mut nodes = self.nodes.write().await;
        nodes.insert(agent.id.clone(), agent);
    }

    /// Unregister an agent. Returns `true` if the agent was found and removed.
    pub async fn unregister_agent(&self, id: &str) -> bool {
        let mut nodes = self.nodes.write().await;
        nodes.remove(id).is_some()
    }

    /// Get an agent by ID.
    pub async fn get_agent(&self, id: &str) -> Option<AgentNode> {
        let nodes = self.nodes.read().await;
        nodes.get(id).cloned()
    }

    /// Update an agent's status. Returns `true` if the agent was found.
    pub async fn update_status(&self, id: &str, status: AgentStatus) -> bool {
        let mut nodes = self.nodes.write().await;
        if let Some(agent) = nodes.get_mut(id) {
            agent.status = status;
            return true;
        }
        false
    }

    /// List all registered agents.
    pub async fn list_agents(&self) -> Vec<AgentNode> {
        let nodes = self.nodes.read().await;
        nodes.values().cloned().collect()
    }

    /// List agents filtered by status.
    pub async fn list_by_status(&self, status: &AgentStatus) -> Vec<AgentNode> {
        let nodes = self.nodes.read().await;
        nodes
            .values()
            .filter(|a| &a.status == status)
            .cloned()
            .collect()
    }

    // ── Messaging ──────────────────────────────────────────────────

    /// Send a message between two agents.
    ///
    /// Validates that both agents exist and that `from != to`.
    /// Assigns a unique ID to the message. If the message history
    /// exceeds `max_history`, the oldest message is removed.
    ///
    /// Returns the assigned message ID on success.
    pub async fn send_message(&self, mut message: AgentMessage) -> Result<String, AgentGraphError> {
        if message.from == message.to {
            return Err(AgentGraphError::SelfMessage);
        }

        let nodes = self.nodes.read().await;
        if !nodes.contains_key(&message.from) {
            return Err(AgentGraphError::AgentNotFound(message.from.clone()));
        }
        if !nodes.contains_key(&message.to) {
            return Err(AgentGraphError::AgentNotFound(message.to.clone()));
        }
        drop(nodes);

        let id = Uuid::new_v4().to_string();
        message.id = id.clone();

        let mut edges = self.edges.write().await;

        // Evict oldest messages if at capacity
        while edges.len() >= self.max_history {
            edges.remove(0);
        }

        edges.push(message);
        Ok(id)
    }

    /// Acknowledge a message. Returns `true` if the message was found.
    pub async fn acknowledge(&self, message_id: &str) -> bool {
        let mut edges = self.edges.write().await;
        if let Some(msg) = edges.iter_mut().find(|m| m.id == message_id) {
            msg.acknowledged = true;
            return true;
        }
        false
    }

    /// Get all messages sent to the given agent.
    pub async fn get_messages_for(&self, agent_id: &str) -> Vec<AgentMessage> {
        let edges = self.edges.read().await;
        edges.iter().filter(|m| m.to == agent_id).cloned().collect()
    }

    /// Get all messages sent from the given agent.
    pub async fn get_messages_from(&self, agent_id: &str) -> Vec<AgentMessage> {
        let edges = self.edges.read().await;
        edges
            .iter()
            .filter(|m| m.from == agent_id)
            .cloned()
            .collect()
    }

    /// Get all unacknowledged messages for the given agent.
    pub async fn get_unacknowledged(&self, agent_id: &str) -> Vec<AgentMessage> {
        let edges = self.edges.read().await;
        edges
            .iter()
            .filter(|m| m.to == agent_id && !m.acknowledged)
            .cloned()
            .collect()
    }

    // ── Graph Queries ──────────────────────────────────────────────

    /// Get all agent IDs that the given agent has communicated with
    /// (either sent to or received from).
    pub async fn get_neighbors(&self, agent_id: &str) -> Vec<String> {
        let edges = self.edges.read().await;
        let mut neighbors = std::collections::HashSet::new();

        for msg in edges.iter() {
            if msg.from == agent_id {
                neighbors.insert(msg.to.clone());
            }
            if msg.to == agent_id {
                neighbors.insert(msg.from.clone());
            }
        }

        neighbors.into_iter().collect()
    }

    /// Get the total number of messages in the graph.
    pub async fn message_count(&self) -> usize {
        let edges = self.edges.read().await;
        edges.len()
    }

    /// Get the total number of registered agents.
    pub async fn agent_count(&self) -> usize {
        let nodes = self.nodes.read().await;
        nodes.len()
    }

    /// Remove all messages sent before the given timestamp.
    /// Returns the number of messages removed.
    pub async fn cleanup_old_messages(&self, before: DateTime<Utc>) -> usize {
        let mut edges = self.edges.write().await;
        let len_before = edges.len();
        edges.retain(|m| m.sent_at >= before);
        len_before - edges.len()
    }

    // ── Handoff Protocol (I5) ─────────────────────────────────────

    /// Hand off a session from one agent to another with context filtering.
    ///
    /// The gateway rewrites conversation context based on the filter:
    /// - `Last5`: only the most recent 5 messages between the agents
    /// - `Summary`: placeholder for LLM summary (returns last 5 as stand-in)
    /// - `Full`: all messages to/from the source agent
    /// - `None`: no context forwarded
    ///
    /// Returns the handoff record on success.
    pub async fn handoff(
        &self,
        from: &str,
        to: &str,
        filter: HandoffFilter,
        metadata: HandoffMetadata,
    ) -> Result<HandoffRecord, AgentGraphError> {
        if from == to {
            return Err(AgentGraphError::HandoffSameAgent);
        }

        // Validate agents exist
        let nodes = self.nodes.read().await;
        if !nodes.contains_key(from) {
            return Err(AgentGraphError::AgentNotFound(from.to_string()));
        }
        if !nodes.contains_key(to) {
            return Err(AgentGraphError::AgentNotFound(to.to_string()));
        }
        drop(nodes);

        // Build filtered context
        let context_messages = self.filter_context(from, &filter).await;

        let record = HandoffRecord {
            id: Uuid::new_v4().to_string(),
            from: from.to_string(),
            to: to.to_string(),
            filter,
            metadata,
            context_messages,
            completed_at: Utc::now(),
        };

        // Store in audit trail
        let mut records = self.handoff_records.write().await;
        records.push(record.clone());

        info!(
            from = %from,
            to = %to,
            handoff_id = %record.id,
            "Agent handoff completed"
        );

        Ok(record)
    }

    /// Filter conversation context for a handoff.
    async fn filter_context(&self, agent_id: &str, filter: &HandoffFilter) -> Vec<AgentMessage> {
        match filter {
            HandoffFilter::None => Vec::new(),
            HandoffFilter::Full => {
                let edges = self.edges.read().await;
                edges
                    .iter()
                    .filter(|m| m.from == agent_id || m.to == agent_id)
                    .cloned()
                    .collect()
            }
            HandoffFilter::Last5 | HandoffFilter::Summary => {
                // Summary would use LLM in production; for now behaves like Last5
                let edges = self.edges.read().await;
                let relevant: Vec<_> = edges
                    .iter()
                    .filter(|m| m.from == agent_id || m.to == agent_id)
                    .cloned()
                    .collect();
                let start = relevant.len().saturating_sub(5);
                relevant[start..].to_vec()
            }
        }
    }

    /// Get handoff records for a specific agent (as source or target).
    pub async fn get_handoff_records(&self, agent_id: &str) -> Vec<HandoffRecord> {
        let records = self.handoff_records.read().await;
        records
            .iter()
            .filter(|r| r.from == agent_id || r.to == agent_id)
            .cloned()
            .collect()
    }

    /// Get all handoff records.
    pub async fn all_handoff_records(&self) -> Vec<HandoffRecord> {
        let records = self.handoff_records.read().await;
        records.clone()
    }

    // ── Scatter-Gather / Fan-Out (I9) ─────────────────────────────

    /// Fan out a message to multiple agents and aggregate responses.
    ///
    /// Each target agent receives the message. Their "response" is simulated
    /// by collecting their most recent message back to the sender. The
    /// `timeout` controls how long to wait for responses.
    ///
    /// In production, the gateway would forward the message over WebSocket
    /// and collect actual responses. This implementation provides the
    /// data-structure and aggregation logic.
    pub async fn scatter(
        &self,
        from: &str,
        agents: &[String],
        message: &str,
        strategy: AggregationStrategy,
        timeout_secs: u64,
    ) -> Result<ScatterResult, AgentGraphError> {
        if agents.is_empty() {
            return Err(AgentGraphError::EmptyScatterList);
        }

        // Validate sender
        {
            let nodes = self.nodes.read().await;
            if !nodes.contains_key(from) {
                return Err(AgentGraphError::AgentNotFound(from.to_string()));
            }
        }

        // Validate all target agents exist
        {
            let nodes = self.nodes.read().await;
            for agent_id in agents {
                if !nodes.contains_key(agent_id) {
                    return Err(AgentGraphError::AgentNotFound(agent_id.clone()));
                }
            }
        }

        // Send message to all agents concurrently
        let mut send_handles = Vec::new();
        for agent_id in agents {
            let msg = AgentMessage {
                id: String::new(),
                from: from.to_string(),
                to: agent_id.clone(),
                content: message.to_string(),
                message_type: MessageType::Query,
                sent_at: Utc::now(),
                acknowledged: false,
            };
            send_handles.push(self.send_message(msg));
        }

        // Await all sends
        let mut responses = Vec::new();
        let mut timed_out = Vec::new();

        for (i, handle) in send_handles.into_iter().enumerate() {
            let agent_id = &agents[i];
            let deadline = tokio::time::Duration::from_secs(timeout_secs);
            match tokio::time::timeout(deadline, handle).await {
                Ok(Ok(_msg_id)) => {
                    // In a real gateway, we'd wait for the agent's response.
                    // Here we check if the agent has any response messages to us.
                    let agent_msgs = self.get_messages_from(agent_id).await;
                    let latest = agent_msgs.iter().rfind(|m| m.to == from).cloned();

                    responses.push(ScatterResponse {
                        agent_id: agent_id.clone(),
                        content: latest.as_ref().map(|m| m.content.clone()),
                        responded: latest.is_some(),
                        responded_at: latest.map(|m| m.sent_at),
                    });
                }
                Ok(Err(e)) => {
                    warn!(agent_id = %agent_id, error = %e, "Scatter send failed");
                    responses.push(ScatterResponse {
                        agent_id: agent_id.clone(),
                        content: None,
                        responded: false,
                        responded_at: None,
                    });
                    timed_out.push(agent_id.clone());
                }
                Err(_) => {
                    debug!(agent_id = %agent_id, "Scatter target timed out");
                    responses.push(ScatterResponse {
                        agent_id: agent_id.clone(),
                        content: None,
                        responded: false,
                        responded_at: None,
                    });
                    timed_out.push(agent_id.clone());
                }
            }
        }

        // Aggregate responses
        let responded: Vec<&str> = responses
            .iter()
            .filter(|r| r.responded)
            .filter_map(|r| r.content.as_deref())
            .collect();

        let aggregated = aggregate_responses(&responded, &strategy);

        Ok(ScatterResult {
            id: Uuid::new_v4().to_string(),
            responses,
            aggregated,
            strategy,
            timed_out_agents: timed_out,
            completed_at: Utc::now(),
        })
    }
}

/// Aggregate a set of response strings according to the chosen strategy.
fn aggregate_responses(responses: &[&str], strategy: &AggregationStrategy) -> String {
    if responses.is_empty() {
        return String::new();
    }

    match strategy {
        AggregationStrategy::Concat => responses.join("\n---\n"),
        AggregationStrategy::First => responses[0].to_string(),
        AggregationStrategy::Best => {
            // Simple heuristic: longest response is "best"
            responses
                .iter()
                .max_by_key(|r| r.len())
                .unwrap_or(&"")
                .to_string()
        }
        AggregationStrategy::Vote => {
            // Most common response
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for r in responses {
                *counts.entry(r).or_insert(0) += 1;
            }
            counts
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(text, _)| text.to_string())
                .unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_agent(id: &str, name: &str, agent_type: &str) -> AgentNode {
        AgentNode {
            id: id.to_string(),
            name: name.to_string(),
            agent_type: agent_type.to_string(),
            status: AgentStatus::Active,
            created_at: Utc::now(),
            metadata: HashMap::new(),
        }
    }

    fn make_message(from: &str, to: &str, content: &str, msg_type: MessageType) -> AgentMessage {
        AgentMessage {
            id: String::new(), // assigned by send_message()
            from: from.to_string(),
            to: to.to_string(),
            content: content.to_string(),
            message_type: msg_type,
            sent_at: Utc::now(),
            acknowledged: false,
        }
    }

    #[tokio::test]
    async fn test_register_and_get_agent() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "coordinator"))
            .await;

        let agent = graph.get_agent("a1").await.unwrap();
        assert_eq!(agent.name, "Alpha");
        assert_eq!(agent.agent_type, "coordinator");
        assert_eq!(agent.status, AgentStatus::Active);
    }

    #[tokio::test]
    async fn test_get_agent_not_found() {
        let graph = AgentGraph::new(100);
        assert!(graph.get_agent("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_unregister_agent() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;

        assert!(graph.unregister_agent("a1").await);
        assert!(graph.get_agent("a1").await.is_none());
        assert!(!graph.unregister_agent("a1").await); // already gone
    }

    #[tokio::test]
    async fn test_update_status() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;

        assert!(graph.update_status("a1", AgentStatus::Busy).await);
        assert_eq!(
            graph.get_agent("a1").await.unwrap().status,
            AgentStatus::Busy
        );

        assert!(!graph.update_status("missing", AgentStatus::Idle).await);
    }

    #[tokio::test]
    async fn test_list_agents() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "external"))
            .await;

        let agents = graph.list_agents().await;
        assert_eq!(agents.len(), 2);
    }

    #[tokio::test]
    async fn test_list_by_status() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "external"))
            .await;
        graph.update_status("a2", AgentStatus::Idle).await;

        let active = graph.list_by_status(&AgentStatus::Active).await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "a1");

        let idle = graph.list_by_status(&AgentStatus::Idle).await;
        assert_eq!(idle.len(), 1);
        assert_eq!(idle[0].id, "a2");
    }

    #[tokio::test]
    async fn test_send_message() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "sub-agent"))
            .await;

        let id = graph
            .send_message(make_message("a1", "a2", "do task X", MessageType::Task))
            .await
            .unwrap();

        assert!(!id.is_empty());
        assert_eq!(graph.message_count().await, 1);
    }

    #[tokio::test]
    async fn test_send_message_self_error() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;

        let result = graph
            .send_message(make_message("a1", "a1", "hello me", MessageType::Query))
            .await;

        assert_eq!(result.unwrap_err(), AgentGraphError::SelfMessage);
    }

    #[tokio::test]
    async fn test_send_message_agent_not_found() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;

        // "to" agent not found
        let result = graph
            .send_message(make_message("a1", "missing", "hello", MessageType::Query))
            .await;
        assert_eq!(
            result.unwrap_err(),
            AgentGraphError::AgentNotFound("missing".to_string())
        );

        // "from" agent not found
        let result = graph
            .send_message(make_message("missing", "a1", "hello", MessageType::Query))
            .await;
        assert_eq!(
            result.unwrap_err(),
            AgentGraphError::AgentNotFound("missing".to_string())
        );
    }

    #[tokio::test]
    async fn test_acknowledge() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "sub-agent"))
            .await;

        let id = graph
            .send_message(make_message("a1", "a2", "task", MessageType::Task))
            .await
            .unwrap();

        assert!(graph.acknowledge(&id).await);
        assert!(!graph.acknowledge("nonexistent").await);

        let unacked = graph.get_unacknowledged("a2").await;
        assert!(unacked.is_empty());
    }

    #[tokio::test]
    async fn test_get_messages_for_and_from() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a3", "Gamma", "sub-agent"))
            .await;

        graph
            .send_message(make_message("a1", "a2", "msg1", MessageType::Task))
            .await
            .unwrap();
        graph
            .send_message(make_message("a1", "a3", "msg2", MessageType::Query))
            .await
            .unwrap();
        graph
            .send_message(make_message("a3", "a2", "msg3", MessageType::Response))
            .await
            .unwrap();

        // Messages sent FROM a1
        let from_a1 = graph.get_messages_from("a1").await;
        assert_eq!(from_a1.len(), 2);

        // Messages sent TO a2
        let to_a2 = graph.get_messages_for("a2").await;
        assert_eq!(to_a2.len(), 2); // from a1 and from a3
    }

    #[tokio::test]
    async fn test_get_unacknowledged() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "sub-agent"))
            .await;

        let id1 = graph
            .send_message(make_message("a1", "a2", "first", MessageType::Task))
            .await
            .unwrap();
        graph
            .send_message(make_message("a1", "a2", "second", MessageType::Task))
            .await
            .unwrap();

        // Acknowledge only the first
        graph.acknowledge(&id1).await;

        let unacked = graph.get_unacknowledged("a2").await;
        assert_eq!(unacked.len(), 1);
        assert_eq!(unacked[0].content, "second");
    }

    #[tokio::test]
    async fn test_get_neighbors() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a3", "Gamma", "sub-agent"))
            .await;

        graph
            .send_message(make_message("a1", "a2", "hi", MessageType::Query))
            .await
            .unwrap();
        graph
            .send_message(make_message("a3", "a1", "hello", MessageType::Query))
            .await
            .unwrap();

        let mut neighbors = graph.get_neighbors("a1").await;
        neighbors.sort();
        assert_eq!(neighbors, vec!["a2", "a3"]);
    }

    #[tokio::test]
    async fn test_agent_and_message_count() {
        let graph = AgentGraph::new(100);
        assert_eq!(graph.agent_count().await, 0);
        assert_eq!(graph.message_count().await, 0);

        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "sub-agent"))
            .await;

        assert_eq!(graph.agent_count().await, 2);

        graph
            .send_message(make_message("a1", "a2", "msg", MessageType::Task))
            .await
            .unwrap();

        assert_eq!(graph.message_count().await, 1);
    }

    #[tokio::test]
    async fn test_max_history_eviction() {
        let graph = AgentGraph::new(3);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "sub-agent"))
            .await;

        for i in 0..5 {
            graph
                .send_message(make_message(
                    "a1",
                    "a2",
                    &format!("msg-{i}"),
                    MessageType::Task,
                ))
                .await
                .unwrap();
        }

        assert_eq!(graph.message_count().await, 3);
        // Oldest messages (msg-0, msg-1) should be evicted
        let msgs = graph.get_messages_for("a2").await;
        assert!(msgs.iter().all(|m| m.content != "msg-0"));
        assert!(msgs.iter().all(|m| m.content != "msg-1"));
        assert!(msgs.iter().any(|m| m.content == "msg-4"));
    }

    #[tokio::test]
    async fn test_cleanup_old_messages() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "sub-agent"))
            .await;

        graph
            .send_message(make_message("a1", "a2", "old", MessageType::Task))
            .await
            .unwrap();

        // Sleep briefly so the next message has a later timestamp
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let cutoff = Utc::now();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        graph
            .send_message(make_message("a1", "a2", "new", MessageType::Task))
            .await
            .unwrap();

        let removed = graph.cleanup_old_messages(cutoff).await;
        assert_eq!(removed, 1);
        assert_eq!(graph.message_count().await, 1);

        let msgs = graph.get_messages_for("a2").await;
        assert_eq!(msgs[0].content, "new");
    }

    #[tokio::test]
    async fn test_agent_graph_error_display() {
        let e1 = AgentGraphError::AgentNotFound("abc".to_string());
        assert_eq!(e1.to_string(), "agent not found: abc");

        let e2 = AgentGraphError::SelfMessage;
        assert_eq!(e2.to_string(), "agent cannot send a message to itself");

        let e3 = AgentGraphError::HandoffSameAgent;
        assert_eq!(e3.to_string(), "cannot handoff to the same agent");

        let e4 = AgentGraphError::EmptyScatterList;
        assert_eq!(e4.to_string(), "scatter requires at least one target agent");

        let e5 = AgentGraphError::ScatterAllTimedOut;
        assert_eq!(e5.to_string(), "all scatter targets timed out");
    }

    // ── Handoff Tests (I5) ────────────────────────────────────────

    #[tokio::test]
    async fn test_handoff_basic() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "coordinator"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "sub-agent"))
            .await;

        // Create some messages so context is available
        graph
            .send_message(make_message("a1", "a2", "task-1", MessageType::Task))
            .await
            .unwrap();

        let record = graph
            .handoff(
                "a1",
                "a2",
                HandoffFilter::Full,
                HandoffMetadata {
                    reason: "escalation".to_string(),
                    priority: Some("high".to_string()),
                },
            )
            .await
            .unwrap();

        assert_eq!(record.from, "a1");
        assert_eq!(record.to, "a2");
        assert_eq!(record.filter, HandoffFilter::Full);
        assert_eq!(record.context_messages.len(), 1);
        assert_eq!(record.metadata.reason, "escalation");
    }

    #[tokio::test]
    async fn test_handoff_same_agent() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;

        let err = graph
            .handoff(
                "a1",
                "a1",
                HandoffFilter::None,
                HandoffMetadata {
                    reason: "test".to_string(),
                    priority: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err, AgentGraphError::HandoffSameAgent);
    }

    #[tokio::test]
    async fn test_handoff_agent_not_found() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;

        let err = graph
            .handoff(
                "a1",
                "missing",
                HandoffFilter::None,
                HandoffMetadata {
                    reason: "test".to_string(),
                    priority: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err, AgentGraphError::AgentNotFound("missing".to_string()));
    }

    #[tokio::test]
    async fn test_handoff_filter_none() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "sub-agent"))
            .await;

        graph
            .send_message(make_message("a1", "a2", "msg", MessageType::Task))
            .await
            .unwrap();

        let record = graph
            .handoff(
                "a1",
                "a2",
                HandoffFilter::None,
                HandoffMetadata {
                    reason: "test".to_string(),
                    priority: None,
                },
            )
            .await
            .unwrap();
        assert!(record.context_messages.is_empty());
    }

    #[tokio::test]
    async fn test_handoff_filter_last5() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "sub-agent"))
            .await;

        // Send 10 messages
        for i in 0..10 {
            graph
                .send_message(make_message(
                    "a1",
                    "a2",
                    &format!("msg-{i}"),
                    MessageType::Task,
                ))
                .await
                .unwrap();
        }

        let record = graph
            .handoff(
                "a1",
                "a2",
                HandoffFilter::Last5,
                HandoffMetadata {
                    reason: "test".to_string(),
                    priority: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(record.context_messages.len(), 5);
        assert_eq!(record.context_messages[0].content, "msg-5");
        assert_eq!(record.context_messages[4].content, "msg-9");
    }

    #[tokio::test]
    async fn test_handoff_records() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "sub-agent"))
            .await;

        graph
            .handoff(
                "a1",
                "a2",
                HandoffFilter::None,
                HandoffMetadata {
                    reason: "test".to_string(),
                    priority: None,
                },
            )
            .await
            .unwrap();

        let records = graph.get_handoff_records("a1").await;
        assert_eq!(records.len(), 1);

        let all = graph.all_handoff_records().await;
        assert_eq!(all.len(), 1);
    }

    // ── Scatter-Gather Tests (I9) ─────────────────────────────────

    #[tokio::test]
    async fn test_scatter_basic() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("coord", "Coordinator", "coordinator"))
            .await;
        graph
            .register_agent(make_agent("a1", "Alpha", "sub-agent"))
            .await;
        graph
            .register_agent(make_agent("a2", "Beta", "sub-agent"))
            .await;

        let result = graph
            .scatter(
                "coord",
                &["a1".into(), "a2".into()],
                "analyze this",
                AggregationStrategy::Concat,
                5,
            )
            .await
            .unwrap();

        assert_eq!(result.responses.len(), 2);
        assert_eq!(result.strategy, AggregationStrategy::Concat);
    }

    #[tokio::test]
    async fn test_scatter_empty_agents() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("coord", "Coordinator", "coordinator"))
            .await;

        let err = graph
            .scatter("coord", &[], "hello", AggregationStrategy::First, 5)
            .await
            .unwrap_err();
        assert_eq!(err, AgentGraphError::EmptyScatterList);
    }

    #[tokio::test]
    async fn test_scatter_sender_not_found() {
        let graph = AgentGraph::new(100);
        let err = graph
            .scatter(
                "missing",
                &["a1".into()],
                "hello",
                AggregationStrategy::First,
                5,
            )
            .await
            .unwrap_err();
        assert_eq!(err, AgentGraphError::AgentNotFound("missing".to_string()));
    }

    #[tokio::test]
    async fn test_scatter_target_not_found() {
        let graph = AgentGraph::new(100);
        graph
            .register_agent(make_agent("coord", "Coordinator", "coordinator"))
            .await;

        let err = graph
            .scatter(
                "coord",
                &["missing".into()],
                "hello",
                AggregationStrategy::First,
                5,
            )
            .await
            .unwrap_err();
        assert_eq!(err, AgentGraphError::AgentNotFound("missing".to_string()));
    }

    // ── Aggregation Tests ─────────────────────────────────────────

    #[test]
    fn test_aggregate_concat() {
        let responses = vec!["hello", "world"];
        let result = aggregate_responses(&responses, &AggregationStrategy::Concat);
        assert_eq!(result, "hello\n---\nworld");
    }

    #[test]
    fn test_aggregate_first() {
        let responses = vec!["first", "second"];
        let result = aggregate_responses(&responses, &AggregationStrategy::First);
        assert_eq!(result, "first");
    }

    #[test]
    fn test_aggregate_best() {
        let responses = vec!["short", "much longer response"];
        let result = aggregate_responses(&responses, &AggregationStrategy::Best);
        assert_eq!(result, "much longer response");
    }

    #[test]
    fn test_aggregate_vote() {
        let responses = vec!["yes", "no", "yes", "yes"];
        let result = aggregate_responses(&responses, &AggregationStrategy::Vote);
        assert_eq!(result, "yes");
    }

    #[test]
    fn test_aggregate_empty() {
        let responses: Vec<&str> = vec![];
        let result = aggregate_responses(&responses, &AggregationStrategy::Concat);
        assert_eq!(result, "");
    }
}
