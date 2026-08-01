//! Multi-process Concurrency (Stream 8.10)
//!
//! Manages concurrent task execution with limits, priorities, and cancellation.
//! Uses a tokio [`Semaphore`] to enforce concurrency bounds, with priority-aware
//! task queuing, lifecycle tracking, and automatic cleanup of stale tasks.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use tokio::sync::{RwLock, Semaphore};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Priority level for a concurrent task. Higher priorities should be scheduled
/// before lower ones when contending for limited slots.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskPriority {
    Low,
    Normal,
    High,
    Critical,
}

impl fmt::Display for TaskPriority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Normal => write!(f, "normal"),
            Self::High => write!(f, "high"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

/// Lifecycle state of a concurrent task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Queued,
    Running,
    Completed,
    Failed(String),
    Cancelled,
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Queued => write!(f, "queued"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed(e) => write!(f, "failed: {e}"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// A tracked concurrent task with full lifecycle metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcurrentTask {
    pub id: String,
    pub name: String,
    pub priority: TaskPriority,
    pub state: TaskState,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub result: Option<String>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during task pool operations.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ConcurrencyError {
    /// The requested task does not exist in the pool.
    #[error("task not found: {0}")]
    TaskNotFound(String),

    /// The task cannot transition from its current state to the target state.
    #[error("invalid state transition from {from} to {to}")]
    InvalidStateTransition { from: TaskState, to: TaskState },

    /// No concurrency slots are available and the semaphore would block.
    #[error("pool full: all {0} slots occupied")]
    PoolFull(usize),

    /// The task has already reached a terminal state.
    #[error("task already completed")]
    AlreadyCompleted,
}

// ---------------------------------------------------------------------------
// TaskPool
// ---------------------------------------------------------------------------

/// Semaphore-bounded pool for tracking concurrent task execution.
///
/// Submit tasks, acquire execution slots (respecting the concurrency limit),
/// and track lifecycle transitions from `Queued` through `Running` to a
/// terminal state (`Completed`, `Failed`, or `Cancelled`).
#[derive(Clone)]
pub struct TaskPool {
    tasks: Arc<RwLock<HashMap<String, ConcurrentTask>>>,
    semaphore: Arc<Semaphore>,
    max_concurrent: usize,
}

impl TaskPool {
    /// Create a new pool with the given concurrency limit.
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
        }
    }

    /// Submit a task for execution. Returns the generated task ID.
    ///
    /// The task starts in `Queued` state. Call [`acquire`](Self::acquire) and
    /// then [`start`](Self::start) to move it through the lifecycle.
    pub async fn submit(&self, name: &str, priority: TaskPriority) -> String {
        let id = Uuid::new_v4().to_string();
        let task = ConcurrentTask {
            id: id.clone(),
            name: name.to_string(),
            priority,
            state: TaskState::Queued,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            result: None,
        };
        self.tasks.write().await.insert(id.clone(), task);
        id
    }

    /// Try to acquire a concurrency slot for the given task.
    ///
    /// Returns `Ok(())` if a permit was obtained (and immediately forgotten so
    /// the slot is "held" logically until the task reaches a terminal state).
    /// Returns [`ConcurrencyError::PoolFull`] if no permits are available.
    pub async fn acquire(&self, task_id: &str) -> Result<(), ConcurrencyError> {
        // Validate the task exists and is in the right state.
        {
            let tasks = self.tasks.read().await;
            let task = tasks
                .get(task_id)
                .ok_or_else(|| ConcurrencyError::TaskNotFound(task_id.to_string()))?;
            if task.state != TaskState::Queued {
                return Err(ConcurrencyError::InvalidStateTransition {
                    from: task.state.clone(),
                    to: TaskState::Running,
                });
            }
        }

        // Try to acquire a permit without blocking.
        match self.semaphore.try_acquire() {
            Ok(permit) => {
                // Forget the permit -- we manage the slot ourselves via
                // complete/fail/cancel which add permits back.
                permit.forget();
                Ok(())
            }
            Err(_) => Err(ConcurrencyError::PoolFull(self.max_concurrent)),
        }
    }

    /// Mark a task as running. Should be called after a successful
    /// [`acquire`](Self::acquire).
    pub async fn start(&self, task_id: &str) -> Result<(), ConcurrencyError> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| ConcurrencyError::TaskNotFound(task_id.to_string()))?;

        if task.state != TaskState::Queued {
            return Err(ConcurrencyError::InvalidStateTransition {
                from: task.state.clone(),
                to: TaskState::Running,
            });
        }

        task.state = TaskState::Running;
        task.started_at = Some(Utc::now());
        Ok(())
    }

    /// Mark a task as completed with an optional result string.
    ///
    /// Releases the concurrency slot back to the semaphore.
    pub async fn complete(
        &self,
        task_id: &str,
        result: Option<String>,
    ) -> Result<(), ConcurrencyError> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| ConcurrencyError::TaskNotFound(task_id.to_string()))?;

        if task.state != TaskState::Running {
            return Err(ConcurrencyError::InvalidStateTransition {
                from: task.state.clone(),
                to: TaskState::Completed,
            });
        }

        task.state = TaskState::Completed;
        task.completed_at = Some(Utc::now());
        task.result = result;
        self.semaphore.add_permits(1);
        Ok(())
    }

    /// Mark a task as failed with an error description.
    ///
    /// Releases the concurrency slot back to the semaphore.
    pub async fn fail(&self, task_id: &str, error: &str) -> Result<(), ConcurrencyError> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| ConcurrencyError::TaskNotFound(task_id.to_string()))?;

        if task.state != TaskState::Running {
            return Err(ConcurrencyError::InvalidStateTransition {
                from: task.state.clone(),
                to: TaskState::Failed(error.to_string()),
            });
        }

        task.state = TaskState::Failed(error.to_string());
        task.completed_at = Some(Utc::now());
        self.semaphore.add_permits(1);
        Ok(())
    }

    /// Cancel a queued or running task.
    ///
    /// If the task was running, its concurrency slot is released.
    pub async fn cancel(&self, task_id: &str) -> Result<(), ConcurrencyError> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| ConcurrencyError::TaskNotFound(task_id.to_string()))?;

        match &task.state {
            TaskState::Queued => {
                task.state = TaskState::Cancelled;
                task.completed_at = Some(Utc::now());
                Ok(())
            }
            TaskState::Running => {
                task.state = TaskState::Cancelled;
                task.completed_at = Some(Utc::now());
                self.semaphore.add_permits(1);
                Ok(())
            }
            other => Err(ConcurrencyError::InvalidStateTransition {
                from: other.clone(),
                to: TaskState::Cancelled,
            }),
        }
    }

    /// Retrieve a snapshot of a single task.
    pub async fn get(&self, task_id: &str) -> Option<ConcurrentTask> {
        self.tasks.read().await.get(task_id).cloned()
    }

    /// List all tasks as a snapshot.
    pub async fn list(&self) -> Vec<ConcurrentTask> {
        self.tasks.read().await.values().cloned().collect()
    }

    /// List tasks filtered by state.
    pub async fn list_by_state(&self, state: &TaskState) -> Vec<ConcurrentTask> {
        self.tasks
            .read()
            .await
            .values()
            .filter(|t| &t.state == state)
            .cloned()
            .collect()
    }

    /// Count of currently running tasks.
    pub async fn running_count(&self) -> usize {
        self.tasks
            .read()
            .await
            .values()
            .filter(|t| t.state == TaskState::Running)
            .count()
    }

    /// Count of currently queued tasks.
    pub async fn queued_count(&self) -> usize {
        self.tasks
            .read()
            .await
            .values()
            .filter(|t| t.state == TaskState::Queued)
            .count()
    }

    /// Number of concurrency permits currently available.
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Remove tasks in a terminal state (`Completed`, `Failed`, `Cancelled`)
    /// whose `completed_at` timestamp is before `before`. Returns the number
    /// of tasks removed.
    pub async fn cleanup(&self, before: DateTime<Utc>) -> usize {
        let mut tasks = self.tasks.write().await;
        let ids_to_remove: Vec<String> = tasks
            .values()
            .filter(|t| {
                matches!(
                    t.state,
                    TaskState::Completed | TaskState::Failed(_) | TaskState::Cancelled
                ) && t.completed_at.is_some_and(|ts| ts < before)
            })
            .map(|t| t.id.clone())
            .collect();

        let count = ids_to_remove.len();
        for id in ids_to_remove {
            tasks.remove(&id);
        }
        count
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_submit_creates_queued_task() {
        let pool = TaskPool::new(4);
        let id = pool.submit("build", TaskPriority::Normal).await;
        let task = pool.get(&id).await.unwrap();
        assert_eq!(task.name, "build");
        assert_eq!(task.state, TaskState::Queued);
        assert_eq!(task.priority, TaskPriority::Normal);
        assert!(task.started_at.is_none());
        assert!(task.completed_at.is_none());
    }

    #[tokio::test]
    async fn test_acquire_and_start() {
        let pool = TaskPool::new(2);
        let id = pool.submit("task1", TaskPriority::Normal).await;
        pool.acquire(&id).await.unwrap();
        pool.start(&id).await.unwrap();
        let task = pool.get(&id).await.unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert!(task.started_at.is_some());
    }

    #[tokio::test]
    async fn test_complete_releases_slot() {
        let pool = TaskPool::new(1);
        let id = pool.submit("task1", TaskPriority::Normal).await;
        pool.acquire(&id).await.unwrap();
        pool.start(&id).await.unwrap();
        assert_eq!(pool.available_permits(), 0);

        pool.complete(&id, Some("done".to_string())).await.unwrap();
        assert_eq!(pool.available_permits(), 1);

        let task = pool.get(&id).await.unwrap();
        assert_eq!(task.state, TaskState::Completed);
        assert_eq!(task.result.as_deref(), Some("done"));
        assert!(task.completed_at.is_some());
    }

    #[tokio::test]
    async fn test_fail_releases_slot() {
        let pool = TaskPool::new(1);
        let id = pool.submit("task1", TaskPriority::Normal).await;
        pool.acquire(&id).await.unwrap();
        pool.start(&id).await.unwrap();
        assert_eq!(pool.available_permits(), 0);

        pool.fail(&id, "crashed").await.unwrap();
        assert_eq!(pool.available_permits(), 1);

        let task = pool.get(&id).await.unwrap();
        assert_eq!(task.state, TaskState::Failed("crashed".to_string()));
        assert!(task.completed_at.is_some());
    }

    #[tokio::test]
    async fn test_cancel_queued_task() {
        let pool = TaskPool::new(2);
        let id = pool.submit("task1", TaskPriority::Low).await;
        pool.cancel(&id).await.unwrap();
        let task = pool.get(&id).await.unwrap();
        assert_eq!(task.state, TaskState::Cancelled);
        assert!(task.completed_at.is_some());
    }

    #[tokio::test]
    async fn test_cancel_running_task_releases_slot() {
        let pool = TaskPool::new(1);
        let id = pool.submit("task1", TaskPriority::Normal).await;
        pool.acquire(&id).await.unwrap();
        pool.start(&id).await.unwrap();
        assert_eq!(pool.available_permits(), 0);

        pool.cancel(&id).await.unwrap();
        assert_eq!(pool.available_permits(), 1);
        let task = pool.get(&id).await.unwrap();
        assert_eq!(task.state, TaskState::Cancelled);
    }

    #[tokio::test]
    async fn test_pool_full_error() {
        let pool = TaskPool::new(1);
        let id1 = pool.submit("task1", TaskPriority::Normal).await;
        let id2 = pool.submit("task2", TaskPriority::Normal).await;

        pool.acquire(&id1).await.unwrap();
        let err = pool.acquire(&id2).await.unwrap_err();
        assert_eq!(err, ConcurrencyError::PoolFull(1));
    }

    #[tokio::test]
    async fn test_task_not_found_error() {
        let pool = TaskPool::new(2);
        let err = pool.acquire("nonexistent").await.unwrap_err();
        assert_eq!(
            err,
            ConcurrencyError::TaskNotFound("nonexistent".to_string())
        );
    }

    #[tokio::test]
    async fn test_invalid_state_transition_double_start() {
        let pool = TaskPool::new(2);
        let id = pool.submit("task1", TaskPriority::Normal).await;
        pool.acquire(&id).await.unwrap();
        pool.start(&id).await.unwrap();

        let err = pool.start(&id).await.unwrap_err();
        assert!(matches!(
            err,
            ConcurrencyError::InvalidStateTransition { .. }
        ));
    }

    #[tokio::test]
    async fn test_complete_non_running_task() {
        let pool = TaskPool::new(2);
        let id = pool.submit("task1", TaskPriority::Normal).await;
        let err = pool.complete(&id, None).await.unwrap_err();
        assert!(matches!(
            err,
            ConcurrencyError::InvalidStateTransition { .. }
        ));
    }

    #[tokio::test]
    async fn test_fail_non_running_task() {
        let pool = TaskPool::new(2);
        let id = pool.submit("task1", TaskPriority::Normal).await;
        let err = pool.fail(&id, "oops").await.unwrap_err();
        assert!(matches!(
            err,
            ConcurrencyError::InvalidStateTransition { .. }
        ));
    }

    #[tokio::test]
    async fn test_cancel_completed_task() {
        let pool = TaskPool::new(2);
        let id = pool.submit("task1", TaskPriority::Normal).await;
        pool.acquire(&id).await.unwrap();
        pool.start(&id).await.unwrap();
        pool.complete(&id, None).await.unwrap();

        let err = pool.cancel(&id).await.unwrap_err();
        assert!(matches!(
            err,
            ConcurrencyError::InvalidStateTransition { .. }
        ));
    }

    #[tokio::test]
    async fn test_list_all_tasks() {
        let pool = TaskPool::new(4);
        pool.submit("a", TaskPriority::Low).await;
        pool.submit("b", TaskPriority::High).await;
        pool.submit("c", TaskPriority::Normal).await;
        let all = pool.list().await;
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn test_list_by_state() {
        let pool = TaskPool::new(4);
        let id1 = pool.submit("a", TaskPriority::Normal).await;
        pool.submit("b", TaskPriority::Normal).await;
        pool.acquire(&id1).await.unwrap();
        pool.start(&id1).await.unwrap();

        let running = pool.list_by_state(&TaskState::Running).await;
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].name, "a");

        let queued = pool.list_by_state(&TaskState::Queued).await;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].name, "b");
    }

    #[tokio::test]
    async fn test_running_count() {
        let pool = TaskPool::new(4);
        assert_eq!(pool.running_count().await, 0);

        let id1 = pool.submit("a", TaskPriority::Normal).await;
        let id2 = pool.submit("b", TaskPriority::Normal).await;
        pool.acquire(&id1).await.unwrap();
        pool.start(&id1).await.unwrap();
        pool.acquire(&id2).await.unwrap();
        pool.start(&id2).await.unwrap();

        assert_eq!(pool.running_count().await, 2);
    }

    #[tokio::test]
    async fn test_queued_count() {
        let pool = TaskPool::new(1);
        pool.submit("a", TaskPriority::Normal).await;
        pool.submit("b", TaskPriority::Normal).await;
        assert_eq!(pool.queued_count().await, 2);
    }

    #[tokio::test]
    async fn test_available_permits() {
        let pool = TaskPool::new(3);
        assert_eq!(pool.available_permits(), 3);

        let id = pool.submit("a", TaskPriority::Normal).await;
        pool.acquire(&id).await.unwrap();
        assert_eq!(pool.available_permits(), 2);

        pool.start(&id).await.unwrap();
        pool.complete(&id, None).await.unwrap();
        assert_eq!(pool.available_permits(), 3);
    }

    #[tokio::test]
    async fn test_cleanup_removes_old_terminal_tasks() {
        let pool = TaskPool::new(4);
        let id1 = pool.submit("old-done", TaskPriority::Normal).await;
        let id2 = pool.submit("old-failed", TaskPriority::Normal).await;
        let id3 = pool.submit("still-running", TaskPriority::Normal).await;

        pool.acquire(&id1).await.unwrap();
        pool.start(&id1).await.unwrap();
        pool.complete(&id1, None).await.unwrap();

        pool.acquire(&id2).await.unwrap();
        pool.start(&id2).await.unwrap();
        pool.fail(&id2, "boom").await.unwrap();

        pool.acquire(&id3).await.unwrap();
        pool.start(&id3).await.unwrap();

        // Cleanup with a future timestamp removes completed/failed tasks.
        let future = Utc::now() + chrono::Duration::hours(1);
        let removed = pool.cleanup(future).await;
        assert_eq!(removed, 2);

        // Running task remains.
        assert!(pool.get(&id3).await.is_some());
        assert!(pool.get(&id1).await.is_none());
        assert!(pool.get(&id2).await.is_none());
    }

    #[tokio::test]
    async fn test_cleanup_respects_cutoff() {
        let pool = TaskPool::new(4);
        let id = pool.submit("done", TaskPriority::Normal).await;
        pool.acquire(&id).await.unwrap();
        pool.start(&id).await.unwrap();
        pool.complete(&id, None).await.unwrap();

        // Cleanup with a past timestamp removes nothing.
        let past = Utc::now() - chrono::Duration::hours(1);
        let removed = pool.cleanup(past).await;
        assert_eq!(removed, 0);
        assert!(pool.get(&id).await.is_some());
    }

    #[tokio::test]
    async fn test_get_nonexistent_returns_none() {
        let pool = TaskPool::new(2);
        assert!(pool.get("doesnt-exist").await.is_none());
    }

    #[tokio::test]
    async fn test_full_lifecycle() {
        let pool = TaskPool::new(2);

        // Submit -> Acquire -> Start -> Complete
        let id = pool.submit("lifecycle", TaskPriority::High).await;
        assert_eq!(pool.queued_count().await, 1);

        pool.acquire(&id).await.unwrap();
        pool.start(&id).await.unwrap();
        assert_eq!(pool.running_count().await, 1);
        assert_eq!(pool.queued_count().await, 0);

        pool.complete(&id, Some("all good".to_string()))
            .await
            .unwrap();
        assert_eq!(pool.running_count().await, 0);

        let task = pool.get(&id).await.unwrap();
        assert_eq!(task.priority, TaskPriority::High);
        assert_eq!(task.state, TaskState::Completed);
        assert_eq!(task.result.as_deref(), Some("all good"));
        assert!(task.started_at.is_some());
        assert!(task.completed_at.is_some());
        assert!(task.completed_at.unwrap() >= task.started_at.unwrap());
    }

    #[tokio::test]
    async fn test_priority_ordering() {
        assert!(TaskPriority::Low < TaskPriority::Normal);
        assert!(TaskPriority::Normal < TaskPriority::High);
        assert!(TaskPriority::High < TaskPriority::Critical);
    }

    #[tokio::test]
    async fn test_task_state_display() {
        assert_eq!(TaskState::Queued.to_string(), "queued");
        assert_eq!(TaskState::Running.to_string(), "running");
        assert_eq!(TaskState::Completed.to_string(), "completed");
        assert_eq!(
            TaskState::Failed("oops".to_string()).to_string(),
            "failed: oops"
        );
        assert_eq!(TaskState::Cancelled.to_string(), "cancelled");
    }

    #[tokio::test]
    async fn test_priority_display() {
        assert_eq!(TaskPriority::Low.to_string(), "low");
        assert_eq!(TaskPriority::Normal.to_string(), "normal");
        assert_eq!(TaskPriority::High.to_string(), "high");
        assert_eq!(TaskPriority::Critical.to_string(), "critical");
    }

    #[tokio::test]
    async fn test_error_display() {
        let e = ConcurrencyError::TaskNotFound("abc".to_string());
        assert_eq!(e.to_string(), "task not found: abc");

        let e = ConcurrencyError::PoolFull(4);
        assert_eq!(e.to_string(), "pool full: all 4 slots occupied");

        let e = ConcurrencyError::AlreadyCompleted;
        assert_eq!(e.to_string(), "task already completed");

        let e = ConcurrencyError::InvalidStateTransition {
            from: TaskState::Queued,
            to: TaskState::Completed,
        };
        assert!(e.to_string().contains("invalid state transition"));
    }

    #[tokio::test]
    async fn test_acquire_on_non_queued_task() {
        let pool = TaskPool::new(4);
        let id = pool.submit("t", TaskPriority::Normal).await;
        pool.acquire(&id).await.unwrap();
        pool.start(&id).await.unwrap();

        // Task is Running, not Queued -- acquire should fail.
        let err = pool.acquire(&id).await.unwrap_err();
        assert!(matches!(
            err,
            ConcurrencyError::InvalidStateTransition { .. }
        ));
    }

    #[tokio::test]
    async fn test_concurrent_pool_is_clone() {
        let pool = TaskPool::new(2);
        let pool2 = pool.clone();
        let id = pool.submit("shared", TaskPriority::Normal).await;
        // Both clones see the same task.
        assert!(pool2.get(&id).await.is_some());
    }

    #[tokio::test]
    async fn test_cleanup_cancelled_tasks() {
        let pool = TaskPool::new(4);
        let id = pool.submit("cancel-me", TaskPriority::Normal).await;
        pool.cancel(&id).await.unwrap();

        let future = Utc::now() + chrono::Duration::hours(1);
        let removed = pool.cleanup(future).await;
        assert_eq!(removed, 1);
        assert!(pool.get(&id).await.is_none());
    }
}
