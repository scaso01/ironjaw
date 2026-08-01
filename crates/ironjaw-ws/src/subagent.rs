use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tracing::{info, warn};

use axum::extract::ws::Message;
use ironjaw_llm::types::ToolDefinition;
use ironjaw_security::HitlGate;
use ironjaw_tools::{ToolContext, ToolRegistry};

/// Status of a sub-agent task.
#[derive(Debug, Clone, PartialEq)]
pub enum SubAgentStatus {
    /// Waiting to be picked up.
    Pending,
    /// Actively executing.
    Running,
    /// Finished successfully.
    Completed,
    /// Failed with an error message.
    Failed(String),
}

impl std::fmt::Display for SubAgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed(msg) => write!(f, "failed: {msg}"),
        }
    }
}

/// How the sub-agent was spawned.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SpawnMode {
    /// Standard spawn with empty context.
    #[default]
    Standard,
    /// Fork: inherits parent session context (message history cloned).
    Fork,
    /// Worktree: runs in an isolated git worktree.
    Worktree,
}

/// Configuration for fork-spawning: clones parent context into child agent.
#[derive(Debug, Clone)]
pub struct ForkConfig {
    pub parent_session_id: String,
    pub inherit_tools: bool,
    pub inherit_brain_context: bool,
}

/// Errors that can occur during sub-agent operations.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SubAgentError {
    #[error("sub-agent not found")]
    NotFound,
    #[error("max depth {0} exceeded")]
    DepthExceeded(usize),
    #[error("max concurrency {0} exceeded")]
    ConcurrencyExceeded(usize),
    #[error("sub-agent already completed")]
    AlreadyCompleted,
    #[error("spawn failed: {0}")]
    SpawnFailed(String),
}

/// A spawned sub-agent that executes a delegated task.
#[derive(Debug, Clone)]
pub struct SubAgent {
    /// Unique identifier for this sub-agent.
    pub id: String,
    /// Description of what this sub-agent is doing.
    pub task: String,
    /// Current execution status.
    pub status: SubAgentStatus,
    /// Nesting depth (0 = top-level, 1 = child of top-level, etc.).
    pub depth: usize,
    /// ID of the parent sub-agent, if any.
    pub parent_id: Option<String>,
    /// Result payload once completed.
    pub result: Option<String>,
    /// When this sub-agent was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// How this agent was spawned (Standard, Fork, or Worktree).
    pub spawn_mode: SpawnMode,
}

/// Manages sub-agent lifecycle for parallel task delegation.
///
/// The main agent can spawn sub-agents to handle independent pieces of work.
/// Sub-agents can themselves spawn children, up to `max_depth` levels deep.
/// At most `max_concurrent` agents may be in Pending or Running state at once.
#[derive(Clone)]
pub struct SubAgentManager {
    agents: Arc<Mutex<HashMap<String, SubAgent>>>,
    max_depth: usize,
    max_concurrent: usize,
}

impl SubAgentManager {
    /// Create a new manager with the given depth and concurrency limits.
    pub fn new(max_depth: usize, max_concurrent: usize) -> Self {
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
            max_depth,
            max_concurrent,
        }
    }

    /// Spawn a new sub-agent for the given task.
    ///
    /// Returns the agent's unique ID on success, or an error if depth or
    /// concurrency limits would be violated.
    pub async fn spawn(
        &self,
        task: String,
        parent_id: Option<String>,
        depth: usize,
    ) -> Result<String, SubAgentError> {
        if depth >= self.max_depth {
            return Err(SubAgentError::DepthExceeded(self.max_depth));
        }

        let mut agents = self.agents.lock().await;

        // Count active (pending + running) agents
        let active_count = agents
            .values()
            .filter(|a| matches!(a.status, SubAgentStatus::Pending | SubAgentStatus::Running))
            .count();

        if active_count >= self.max_concurrent {
            return Err(SubAgentError::ConcurrencyExceeded(self.max_concurrent));
        }

        let id = uuid::Uuid::new_v4().to_string();
        let agent = SubAgent {
            id: id.clone(),
            task: task.clone(),
            status: SubAgentStatus::Pending,
            depth,
            parent_id,
            result: None,
            created_at: chrono::Utc::now(),
            spawn_mode: SpawnMode::Standard,
        };

        agents.insert(id.clone(), agent);
        info!(agent_id = %id, depth, task = %task, "Sub-agent spawned");
        Ok(id)
    }

    /// Get a snapshot of a sub-agent by ID.
    pub async fn get(&self, id: &str) -> Option<SubAgent> {
        self.agents.lock().await.get(id).cloned()
    }

    /// Update the status of a sub-agent.
    ///
    /// Returns an error if the agent doesn't exist or is already completed/failed.
    pub async fn update_status(
        &self,
        id: &str,
        status: SubAgentStatus,
    ) -> Result<(), SubAgentError> {
        let mut agents = self.agents.lock().await;
        let agent = agents.get_mut(id).ok_or(SubAgentError::NotFound)?;

        if matches!(
            agent.status,
            SubAgentStatus::Completed | SubAgentStatus::Failed(_)
        ) {
            return Err(SubAgentError::AlreadyCompleted);
        }

        info!(agent_id = %id, old = %agent.status, new = %status, "Sub-agent status updated");
        agent.status = status;
        Ok(())
    }

    /// Mark a sub-agent as completed with the given result.
    ///
    /// This sets the status to `Completed` and stores the result string.
    pub async fn set_result(&self, id: &str, result: String) -> Result<(), SubAgentError> {
        let mut agents = self.agents.lock().await;
        let agent = agents.get_mut(id).ok_or(SubAgentError::NotFound)?;

        if matches!(
            agent.status,
            SubAgentStatus::Completed | SubAgentStatus::Failed(_)
        ) {
            return Err(SubAgentError::AlreadyCompleted);
        }

        info!(agent_id = %id, "Sub-agent completed with result");
        agent.status = SubAgentStatus::Completed;
        agent.result = Some(result);
        Ok(())
    }

    /// List all sub-agents (snapshot).
    pub async fn list(&self) -> Vec<SubAgent> {
        self.agents.lock().await.values().cloned().collect()
    }

    /// Cancel a pending or running sub-agent.
    ///
    /// Returns an error if the agent doesn't exist or is already in a terminal state.
    pub async fn cancel(&self, id: &str) -> Result<(), SubAgentError> {
        let mut agents = self.agents.lock().await;
        let agent = agents.get_mut(id).ok_or(SubAgentError::NotFound)?;

        if matches!(
            agent.status,
            SubAgentStatus::Completed | SubAgentStatus::Failed(_)
        ) {
            return Err(SubAgentError::AlreadyCompleted);
        }

        warn!(agent_id = %id, "Sub-agent cancelled");
        agent.status = SubAgentStatus::Failed("cancelled".to_string());
        Ok(())
    }

    /// Remove completed or failed agents older than the given duration.
    pub async fn cleanup_completed(&self, older_than: chrono::Duration) {
        let cutoff = chrono::Utc::now() - older_than;
        let mut agents = self.agents.lock().await;
        let before = agents.len();

        agents.retain(|_, a| {
            // Keep if still active or newer than cutoff
            matches!(a.status, SubAgentStatus::Pending | SubAgentStatus::Running)
                || a.created_at > cutoff
        });

        let removed = before - agents.len();
        if removed > 0 {
            info!(removed, "Cleaned up completed sub-agents");
        }
    }

    /// Spawn a forked agent that inherits parent session context.
    /// The parent's message history should be cloned into the child session
    /// by the caller (via SessionStore::clone_session_messages).
    pub async fn spawn_fork(
        &self,
        task: &str,
        fork_config: ForkConfig,
    ) -> Result<String, SubAgentError> {
        let id = self
            .spawn(
                task.to_string(),
                Some(fork_config.parent_session_id.clone()),
                0,
            )
            .await?;

        // Mark as fork spawn mode
        let mut agents = self.agents.lock().await;
        if let Some(agent) = agents.get_mut(&id) {
            agent.spawn_mode = SpawnMode::Fork;
        }

        Ok(id)
    }
}

impl Default for SubAgentManager {
    fn default() -> Self {
        Self::new(3, 5)
    }
}

// ---------------------------------------------------------------------------
// Standalone helpers (Phase 9)
// ---------------------------------------------------------------------------

/// Tier order used by [`cap_tools_by_tier`].
///
/// Matches the `Debug` representation of [`ironjaw_security::PermissionTier`]:
/// `"Auto"` < `"Notify"` < `"Confirm"` < `"MultiConfirm"`.
fn tier_rank(tier: &str) -> u8 {
    match tier {
        "Auto" => 0,
        "Notify" => 1,
        "Confirm" => 2,
        "MultiConfirm" => 3,
        _ => 2, // unknown → Confirm (safe default)
    }
}

/// Return only the tools whose tier is ≤ `ceiling`.
///
/// The `ceiling` string must match the `Debug` variant of
/// [`ironjaw_security::PermissionTier`] (e.g., `"Auto"`, `"Notify"`,
/// `"Confirm"`, `"MultiConfirm"`).
pub fn cap_tools_by_tier(tools: Vec<ToolDefinition>, ceiling: &str) -> Vec<ToolDefinition> {
    let ceiling_rank = tier_rank(ceiling);
    tools
        .into_iter()
        .filter(|t| tier_rank(&t.tier) <= ceiling_rank)
        .collect()
}

/// Execute a subagent task using a mini-agentic loop (≤ 3 iterations).
///
/// Each iteration calls [`crate::agentic::execute_tool_batch`] with all
/// provided tool calls.  The results from every iteration are concatenated
/// into a single result string.  Only tools at or below `ceiling_tier` are
/// passed through (others are silently dropped).
///
/// # Arguments
/// - `task_description` – human-readable description (logged only).
/// - `tool_calls` – the batch of [`ironjaw_llm::ToolCall`]s to execute per
///   iteration.  An empty slice is valid (loop runs once, produces empty result).
/// - `registry` – the tool registry for dispatch.
/// - `hitl` – the HITL gate (must be in auto-approve mode for tests without UI).
/// - `tx` – WebSocket channel for `tool.start`/`tool.result` events.
/// - `ceiling_tier` – maximum allowed tier (Debug variant string, see
///   [`cap_tools_by_tier`]).
/// - `timeout_secs` – per-tool timeout.
///
/// Returns an aggregated result string, or an error description if all
/// iterations produced errors.
///
/// # Phase 18 note
/// // PHASE 18: subagent full refactor — replace this mini-loop with the full
/// //           SubAgentManager lifecycle, skill inheritance, and fork/worktree
/// //           spawn modes once the autonomous executor is fully wired.
#[allow(clippy::too_many_arguments)]
pub async fn execute_subagent_task(
    task_description: &str,
    tool_calls: &[ironjaw_llm::ToolCall],
    registry: &Arc<ToolRegistry>,
    hitl: &Arc<HitlGate>,
    tx: &mpsc::UnboundedSender<Message>,
    tool_context: &ToolContext,
    ceiling_tier: &str,
    timeout_secs: u64,
    session_id: &str,
    hitl_session_cache_enabled: bool,
) -> Result<String, String> {
    const MAX_ITERATIONS: usize = 3;

    info!(task = %task_description, ceiling = %ceiling_tier, "Subagent task started");

    // Filter tool calls to those whose tool is allowed at or below ceiling.
    // We can only filter by name — we don't have the tier for a ToolCall
    // without a registry lookup.  Do a best-effort lookup.
    let allowed_calls: Vec<ironjaw_llm::ToolCall> = tool_calls
        .iter()
        .filter(|call| {
            let tool_tier = registry
                .get(&call.function.name)
                .map(|t| format!("{:?}", t.tier()))
                .unwrap_or_else(|| "Confirm".to_string());
            tier_rank(&tool_tier) <= tier_rank(ceiling_tier)
        })
        .cloned()
        .collect();

    if allowed_calls.len() < tool_calls.len() {
        warn!(
            task = %task_description,
            dropped = tool_calls.len() - allowed_calls.len(),
            "Subagent dropped tool calls above ceiling tier",
        );
    }

    let mut aggregated = String::new();
    let mut last_error: Option<String> = None;

    for iteration in 0..MAX_ITERATIONS {
        if allowed_calls.is_empty() {
            break;
        }

        let results = crate::agentic::execute_tool_batch(
            &allowed_calls,
            registry,
            hitl,
            tx,
            tool_context,
            timeout_secs,
            session_id,
            hitl_session_cache_enabled,
            None,
            false,
            ironjaw_session::types::SessionMode::Default,
            None,
            // C2-A: subagent doesn't need the shared in-flight registry;
            // use a local ephemeral one.
            &Arc::new(DashMap::<String, HashMap<String, String>>::new()),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        let all_ok = results.iter().all(|r| r.ok);

        for r in &results {
            if r.ok {
                let content =
                    serde_json::to_string(&r.result).unwrap_or_else(|_| "result".to_string());
                if !aggregated.is_empty() {
                    aggregated.push('\n');
                }
                aggregated.push_str(&content);
            } else {
                let err = r.error.clone().unwrap_or_else(|| "tool error".to_string());
                warn!(
                    task = %task_description,
                    tool = %r.tool_name,
                    iteration,
                    error = %err,
                    "Subagent tool call failed",
                );
                last_error = Some(err);
            }
        }

        if all_ok {
            break;
        }
    }

    if aggregated.is_empty() {
        Err(last_error.unwrap_or_else(|| "subagent produced no output".to_string()))
    } else {
        Ok(aggregated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spawn_and_get() {
        let mgr = SubAgentManager::new(3, 5);
        let id = mgr
            .spawn("research topic".to_string(), None, 0)
            .await
            .unwrap();

        let agent = mgr.get(&id).await.unwrap();
        assert_eq!(agent.task, "research topic");
        assert_eq!(agent.status, SubAgentStatus::Pending);
        assert_eq!(agent.depth, 0);
        assert!(agent.parent_id.is_none());
        assert!(agent.result.is_none());
    }

    #[tokio::test]
    async fn test_spawn_with_parent() {
        let mgr = SubAgentManager::new(3, 5);
        let parent_id = mgr.spawn("parent task".to_string(), None, 0).await.unwrap();
        let child_id = mgr
            .spawn("child task".to_string(), Some(parent_id.clone()), 1)
            .await
            .unwrap();

        let child = mgr.get(&child_id).await.unwrap();
        assert_eq!(child.parent_id, Some(parent_id));
        assert_eq!(child.depth, 1);
    }

    #[tokio::test]
    async fn test_depth_limit() {
        let mgr = SubAgentManager::new(2, 10);

        // depth 0 and 1 should work
        mgr.spawn("depth 0".to_string(), None, 0).await.unwrap();
        mgr.spawn("depth 1".to_string(), None, 1).await.unwrap();

        // depth 2 should fail (max_depth is 2, so 0 and 1 are valid)
        let err = mgr.spawn("depth 2".to_string(), None, 2).await.unwrap_err();
        assert_eq!(err, SubAgentError::DepthExceeded(2));
    }

    #[tokio::test]
    async fn test_concurrency_limit() {
        let mgr = SubAgentManager::new(5, 2);

        mgr.spawn("task 1".to_string(), None, 0).await.unwrap();
        mgr.spawn("task 2".to_string(), None, 0).await.unwrap();

        // Third should fail — 2 pending agents already
        let err = mgr.spawn("task 3".to_string(), None, 0).await.unwrap_err();
        assert_eq!(err, SubAgentError::ConcurrencyExceeded(2));
    }

    #[tokio::test]
    async fn test_concurrency_freed_after_completion() {
        let mgr = SubAgentManager::new(5, 2);

        let id1 = mgr.spawn("task 1".to_string(), None, 0).await.unwrap();
        mgr.spawn("task 2".to_string(), None, 0).await.unwrap();

        // At limit — cannot spawn
        assert!(mgr.spawn("task 3".to_string(), None, 0).await.is_err());

        // Complete one agent — frees a slot
        mgr.set_result(&id1, "done".to_string()).await.unwrap();

        // Now we can spawn again
        mgr.spawn("task 3".to_string(), None, 0).await.unwrap();
    }

    #[tokio::test]
    async fn test_status_transitions() {
        let mgr = SubAgentManager::new(3, 5);
        let id = mgr.spawn("task".to_string(), None, 0).await.unwrap();

        // Pending -> Running
        mgr.update_status(&id, SubAgentStatus::Running)
            .await
            .unwrap();
        assert_eq!(mgr.get(&id).await.unwrap().status, SubAgentStatus::Running);

        // Running -> Completed
        mgr.update_status(&id, SubAgentStatus::Completed)
            .await
            .unwrap();
        assert_eq!(
            mgr.get(&id).await.unwrap().status,
            SubAgentStatus::Completed
        );

        // Completed -> anything should fail
        let err = mgr
            .update_status(&id, SubAgentStatus::Running)
            .await
            .unwrap_err();
        assert_eq!(err, SubAgentError::AlreadyCompleted);
    }

    #[tokio::test]
    async fn test_set_result() {
        let mgr = SubAgentManager::new(3, 5);
        let id = mgr.spawn("task".to_string(), None, 0).await.unwrap();

        mgr.update_status(&id, SubAgentStatus::Running)
            .await
            .unwrap();
        mgr.set_result(&id, "answer is 42".to_string())
            .await
            .unwrap();

        let agent = mgr.get(&id).await.unwrap();
        assert_eq!(agent.status, SubAgentStatus::Completed);
        assert_eq!(agent.result, Some("answer is 42".to_string()));
    }

    #[tokio::test]
    async fn test_set_result_already_completed() {
        let mgr = SubAgentManager::new(3, 5);
        let id = mgr.spawn("task".to_string(), None, 0).await.unwrap();

        mgr.set_result(&id, "first".to_string()).await.unwrap();

        let err = mgr.set_result(&id, "second".to_string()).await.unwrap_err();
        assert_eq!(err, SubAgentError::AlreadyCompleted);
    }

    #[tokio::test]
    async fn test_list() {
        let mgr = SubAgentManager::new(3, 5);

        mgr.spawn("task 1".to_string(), None, 0).await.unwrap();
        mgr.spawn("task 2".to_string(), None, 0).await.unwrap();
        mgr.spawn("task 3".to_string(), None, 0).await.unwrap();

        let agents = mgr.list().await;
        assert_eq!(agents.len(), 3);
    }

    #[tokio::test]
    async fn test_cancel_pending() {
        let mgr = SubAgentManager::new(3, 5);
        let id = mgr.spawn("task".to_string(), None, 0).await.unwrap();

        mgr.cancel(&id).await.unwrap();

        let agent = mgr.get(&id).await.unwrap();
        assert_eq!(
            agent.status,
            SubAgentStatus::Failed("cancelled".to_string())
        );
    }

    #[tokio::test]
    async fn test_cancel_running() {
        let mgr = SubAgentManager::new(3, 5);
        let id = mgr.spawn("task".to_string(), None, 0).await.unwrap();

        mgr.update_status(&id, SubAgentStatus::Running)
            .await
            .unwrap();
        mgr.cancel(&id).await.unwrap();

        let agent = mgr.get(&id).await.unwrap();
        assert_eq!(
            agent.status,
            SubAgentStatus::Failed("cancelled".to_string())
        );
    }

    #[tokio::test]
    async fn test_cancel_completed_fails() {
        let mgr = SubAgentManager::new(3, 5);
        let id = mgr.spawn("task".to_string(), None, 0).await.unwrap();

        mgr.set_result(&id, "done".to_string()).await.unwrap();

        let err = mgr.cancel(&id).await.unwrap_err();
        assert_eq!(err, SubAgentError::AlreadyCompleted);
    }

    #[tokio::test]
    async fn test_cancel_not_found() {
        let mgr = SubAgentManager::new(3, 5);
        let err = mgr.cancel("nonexistent").await.unwrap_err();
        assert_eq!(err, SubAgentError::NotFound);
    }

    #[tokio::test]
    async fn test_get_not_found() {
        let mgr = SubAgentManager::new(3, 5);
        assert!(mgr.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_update_status_not_found() {
        let mgr = SubAgentManager::new(3, 5);
        let err = mgr
            .update_status("nonexistent", SubAgentStatus::Running)
            .await
            .unwrap_err();
        assert_eq!(err, SubAgentError::NotFound);
    }

    #[tokio::test]
    async fn test_cleanup_completed() {
        let mgr = SubAgentManager::new(3, 5);

        let id1 = mgr.spawn("task 1".to_string(), None, 0).await.unwrap();
        let id2 = mgr.spawn("task 2".to_string(), None, 0).await.unwrap();
        let _id3 = mgr.spawn("task 3".to_string(), None, 0).await.unwrap();

        // Complete agent 1 and fail agent 2
        mgr.set_result(&id1, "done".to_string()).await.unwrap();
        mgr.cancel(&id2).await.unwrap();

        // Cleanup with zero duration — removes all completed/failed
        mgr.cleanup_completed(chrono::Duration::zero()).await;

        let agents = mgr.list().await;
        // Only the pending agent 3 should remain
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].status, SubAgentStatus::Pending);
    }

    #[tokio::test]
    async fn test_cleanup_respects_age() {
        let mgr = SubAgentManager::new(3, 5);

        let id = mgr.spawn("task".to_string(), None, 0).await.unwrap();
        mgr.set_result(&id, "done".to_string()).await.unwrap();

        // Cleanup with 1 hour threshold — agent was just created, should NOT be removed
        mgr.cleanup_completed(chrono::Duration::hours(1)).await;

        let agents = mgr.list().await;
        assert_eq!(agents.len(), 1);
    }

    #[tokio::test]
    async fn test_default() {
        let mgr = SubAgentManager::default();

        // Should allow depth 0, 1, 2 (max_depth=3)
        mgr.spawn("d0".to_string(), None, 0).await.unwrap();
        mgr.spawn("d1".to_string(), None, 1).await.unwrap();
        mgr.spawn("d2".to_string(), None, 2).await.unwrap();

        // depth 3 should fail
        assert!(mgr.spawn("d3".to_string(), None, 3).await.is_err());

        // Should allow up to 5 concurrent (3 already active)
        mgr.spawn("extra1".to_string(), None, 0).await.unwrap();
        mgr.spawn("extra2".to_string(), None, 0).await.unwrap();

        // 6th should fail
        assert!(mgr.spawn("extra3".to_string(), None, 0).await.is_err());
    }

    #[tokio::test]
    async fn test_cancel_frees_concurrency_slot() {
        let mgr = SubAgentManager::new(3, 2);

        let id1 = mgr.spawn("task 1".to_string(), None, 0).await.unwrap();
        mgr.spawn("task 2".to_string(), None, 0).await.unwrap();

        // At limit
        assert!(mgr.spawn("task 3".to_string(), None, 0).await.is_err());

        // Cancel frees a slot (status becomes Failed, not Pending/Running)
        mgr.cancel(&id1).await.unwrap();

        // Now we can spawn
        mgr.spawn("task 3".to_string(), None, 0).await.unwrap();
    }

    #[test]
    fn test_fork_config_clones_parent_context() {
        let config = ForkConfig {
            parent_session_id: "session-abc".to_string(),
            inherit_tools: true,
            inherit_brain_context: true,
        };
        assert!(config.inherit_tools);
        assert_eq!(config.parent_session_id, "session-abc");
    }

    #[tokio::test]
    async fn test_spawn_fork_sets_parent_id() {
        let mgr = SubAgentManager::default();
        let fork_config = ForkConfig {
            parent_session_id: "parent-1".to_string(),
            inherit_tools: true,
            inherit_brain_context: false,
        };
        let id = mgr.spawn_fork("research task", fork_config).await.unwrap();
        let agent = mgr.get(&id).await.unwrap();
        assert_eq!(agent.parent_id, Some("parent-1".to_string()));
        assert_eq!(agent.spawn_mode, SpawnMode::Fork);
    }

    #[tokio::test]
    async fn test_spawn_standard_has_standard_mode() {
        let mgr = SubAgentManager::default();
        let id = mgr.spawn("task".to_string(), None, 0).await.unwrap();
        let agent = mgr.get(&id).await.unwrap();
        assert_eq!(agent.spawn_mode, SpawnMode::Standard);
    }

    #[test]
    fn test_spawn_mode_default() {
        assert_eq!(SpawnMode::default(), SpawnMode::Standard);
    }
}
