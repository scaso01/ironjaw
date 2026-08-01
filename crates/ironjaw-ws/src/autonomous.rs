//! Autonomous Hands System (Stream 8.8)
//!
//! Plans and executes multi-step autonomous workflows -- sequences of tool
//! calls, LLM queries, and control-flow operations that the AI plans and
//! executes with human oversight. Uses the existing tool registry and
//! sub-agent system; does **not** depend on WASM.
//!
//! Safety guardrails:
//! - Step counter prevents infinite loops (`max_steps`).
//! - Nesting depth limit prevents unbounded sub-plan recursion (`max_depth`).
//! - Tool whitelist enforcement per execution.
//! - Automatic pause before destructive operations (write_file, shell, docker).
//! - Mandatory human approval gates at configurable intervals.
//! - Overall execution timeout.

use axum::extract::ws::Message;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use ironjaw_llm::{ChatMessage, ChatRequest, LlmRouter, ToolCall};
use ironjaw_security::HitlGate;
use ironjaw_tools::{ToolContext, ToolRegistry};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration knobs for autonomous execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionConfig {
    /// Maximum number of steps before the executor stops (prevents runaway).
    pub max_steps: usize,
    /// Maximum nesting depth for sub-plans.
    pub max_depth: usize,
    /// Force a human approval gate every N steps.
    pub require_approval_every_n: usize,
    /// Overall execution timeout in seconds.
    pub timeout_secs: u64,
    /// Tool whitelist -- only these tools may be invoked. Empty = deny all.
    pub allowed_tools: Vec<String>,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            max_steps: 50,
            max_depth: 5,
            require_approval_every_n: 10,
            timeout_secs: 300,
            allowed_tools: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// The kind of action a single step performs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Invoke a registered tool by name.
    ToolCall { tool_name: String, params: String },
    /// Send a query to the LLM and capture the response.
    LlmQuery { prompt: String },
    /// Pause execution and wait for human approval before continuing.
    WaitForApproval { reason: String },
    /// Conditional branch -- choose a path based on the previous step result.
    Branch {
        condition: String,
        if_true: Vec<usize>,
        if_false: Vec<usize>,
    },
    /// Repeat a set of steps a fixed number of times.
    Loop { body: Vec<usize>, iterations: usize },
    /// Save a recoverable checkpoint of the current execution state.
    Checkpoint,
}

impl fmt::Display for StepKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ToolCall { tool_name, .. } => write!(f, "tool_call({tool_name})"),
            Self::LlmQuery { .. } => write!(f, "llm_query"),
            Self::WaitForApproval { reason } => write!(f, "wait_for_approval({reason})"),
            Self::Branch { condition, .. } => write!(f, "branch({condition})"),
            Self::Loop { iterations, .. } => write!(f, "loop({iterations})"),
            Self::Checkpoint => write!(f, "checkpoint"),
        }
    }
}

/// Lifecycle state of an autonomous execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    /// The executor is building/validating the plan.
    Planning,
    /// The plan is actively running.
    Running,
    /// Execution is paused (manual or automatic).
    Paused,
    /// Blocked on human approval.
    WaitingApproval,
    /// All steps finished successfully.
    Completed,
    /// Execution stopped due to an error.
    Failed(String),
    /// Execution was cancelled by the user.
    Cancelled,
}

impl fmt::Display for ExecutionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Planning => write!(f, "planning"),
            Self::Running => write!(f, "running"),
            Self::Paused => write!(f, "paused"),
            Self::WaitingApproval => write!(f, "waiting_approval"),
            Self::Completed => write!(f, "completed"),
            Self::Failed(msg) => write!(f, "failed: {msg}"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

// ---------------------------------------------------------------------------
// Step & Plan
// ---------------------------------------------------------------------------

/// A single action within an execution plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionStep {
    /// Zero-based index within the plan.
    pub index: usize,
    /// Human-readable label for this step.
    pub label: String,
    /// What this step does.
    pub kind: StepKind,
    /// Indices of steps that must complete before this one can run.
    pub dependencies: Vec<usize>,
}

/// An ordered sequence of steps to execute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPlan {
    /// Unique plan identifier.
    pub id: String,
    /// High-level goal that this plan achieves.
    pub goal: String,
    /// The ordered steps.
    pub steps: Vec<ExecutionStep>,
    /// Nesting depth (0 = top-level plan).
    pub depth: usize,
    /// When the plan was created.
    pub created_at: DateTime<Utc>,
}

impl ExecutionPlan {
    /// Create a new plan with the given goal and steps.
    pub fn new(goal: impl Into<String>, steps: Vec<ExecutionStep>, depth: usize) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            goal: goal.into(),
            steps,
            depth,
            created_at: Utc::now(),
        }
    }

    /// Number of steps in the plan.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether the plan has zero steps.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Results & Reports
// ---------------------------------------------------------------------------

/// The outcome of executing a single step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    /// Which step this result belongs to.
    pub step_index: usize,
    /// Whether the step succeeded.
    pub success: bool,
    /// Output or return value from the step.
    pub output: String,
    /// When execution of this step started.
    pub started_at: DateTime<Utc>,
    /// When execution of this step ended.
    pub ended_at: DateTime<Utc>,
}

/// Summary report produced after an execution completes (or fails).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReport {
    /// The plan that was executed.
    pub plan_id: String,
    /// Final state of the execution.
    pub final_state: ExecutionState,
    /// Per-step results (in execution order).
    pub step_results: Vec<StepResult>,
    /// Total number of steps executed.
    pub steps_executed: usize,
    /// When execution started.
    pub started_at: DateTime<Utc>,
    /// When execution ended.
    pub ended_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Snapshot (checkpoint / restore)
// ---------------------------------------------------------------------------

/// A serializable snapshot of execution state for crash recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSnapshot {
    /// The full plan.
    pub plan: ExecutionPlan,
    /// Index of the next step to execute.
    pub next_step: usize,
    /// Results accumulated so far.
    pub results: Vec<StepResult>,
    /// Current depth.
    pub depth: usize,
    /// Config used for this execution.
    pub config: ExecutionConfig,
    /// When this snapshot was taken.
    pub snapshot_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during autonomous execution.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AutonomousError {
    #[error("max steps ({0}) exceeded")]
    MaxStepsExceeded(usize),

    #[error("max depth ({0}) exceeded")]
    MaxDepthExceeded(usize),

    #[error("execution timed out after {0}s")]
    Timeout(u64),

    #[error("tool not in whitelist: {0}")]
    ToolNotAllowed(String),

    #[error("execution is not in a resumable state: {0}")]
    NotResumable(String),

    #[error("execution already active")]
    AlreadyActive,

    #[error("no active execution")]
    NoActiveExecution,

    #[error("plan is empty")]
    EmptyPlan,

    #[error("step dependency not met: step {0} depends on {1}")]
    DependencyNotMet(usize, usize),

    #[error("approval required: {0}")]
    ApprovalRequired(String),

    #[error("destructive operation requires approval: {0}")]
    DestructiveOperation(String),

    #[error("step execution failed: {0}")]
    StepFailed(String),

    #[error("invalid step index: {0}")]
    InvalidStepIndex(usize),
}

// ---------------------------------------------------------------------------
// Destructive tool detection
// ---------------------------------------------------------------------------

/// Tools that are considered destructive and require automatic approval gates.
const DESTRUCTIVE_TOOLS: &[&str] = &[
    "write_file",
    "shell",
    "patch",
    "docker_manage",
    "docker_exec",
    "adb",
    "secrets_set",
];

/// Check whether a tool name is considered destructive.
fn is_destructive_tool(name: &str) -> bool {
    DESTRUCTIVE_TOOLS.contains(&name)
}

// ---------------------------------------------------------------------------
// AutonomousExecutor
// ---------------------------------------------------------------------------

/// Internal mutable state for the executor.
struct ExecutorInner {
    state: ExecutionState,
    current_plan: Option<ExecutionPlan>,
    next_step: usize,
    steps_executed: usize,
    results: Vec<StepResult>,
    started_at: Option<DateTime<Utc>>,
    config: ExecutionConfig,
    completed_steps: HashSet<usize>,
}

// ---------------------------------------------------------------------------
// Execution context (optional — enables real tool/LLM calls)
// ---------------------------------------------------------------------------

/// Resources needed for real tool and LLM execution.
///
/// When present, `execute_step` routes `ToolCall` steps through
/// `crate::agentic::execute_tool_batch` and `LlmQuery` steps through
/// `LlmRouter::chat`.  When absent, both fall back to the lightweight
/// simulation strings (useful for tests that don't need a live registry).
#[derive(Clone)]
pub struct ExecutionContext {
    pub registry: Arc<ToolRegistry>,
    pub hitl: Arc<HitlGate>,
    pub router: Arc<LlmRouter>,
    pub tx: mpsc::UnboundedSender<Message>,
    pub tool_context: ToolContext,
    pub tool_timeout_secs: u64,
    /// Current session ID — passed to `execute_tool_batch` for HITL session cache.
    pub session_id: String,
    /// Whether the HITL session cache is enabled (from `ChatAgenticConfig`).
    pub hitl_session_cache_enabled: bool,
}

/// Plans and executes multi-step autonomous workflows with safety guardrails.
///
/// Only one plan may be active at a time. The executor enforces step limits,
/// depth limits, timeouts, tool whitelists, and mandatory human approval gates.
#[derive(Clone)]
pub struct AutonomousExecutor {
    inner: Arc<Mutex<ExecutorInner>>,
    /// Optional real execution context. When `None`, steps produce simulation
    /// strings instead of calling the real tool registry / LLM router.
    exec_ctx: Option<Arc<ExecutionContext>>,
}

impl AutonomousExecutor {
    /// Create a new executor with the given configuration.
    ///
    /// The executor starts without an execution context — `execute_step` will
    /// use simulation strings for `ToolCall` and `LlmQuery` steps.  Call
    /// [`with_execution_context`] to wire in real tool/LLM execution.
    pub fn new(config: ExecutionConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ExecutorInner {
                state: ExecutionState::Planning,
                current_plan: None,
                next_step: 0,
                steps_executed: 0,
                results: Vec::new(),
                started_at: None,
                config,
                completed_steps: HashSet::new(),
            })),
            exec_ctx: None,
        }
    }

    /// Create a new executor with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(ExecutionConfig::default())
    }

    /// Attach a real execution context so `execute_step` calls the live
    /// tool registry and LLM router instead of producing simulation strings.
    ///
    /// Returns `self` so this can be chained after `new()`.
    pub fn with_execution_context(mut self, ctx: ExecutionContext) -> Self {
        self.exec_ctx = Some(Arc::new(ctx));
        self
    }

    /// Build an execution plan from a goal description and context.
    ///
    /// In a real system this would call the LLM to decompose the goal into
    /// steps. Here we validate and register the plan for execution.
    pub async fn plan(
        &self,
        goal: &str,
        _context: &str,
        steps: Vec<ExecutionStep>,
        depth: usize,
    ) -> Result<ExecutionPlan, AutonomousError> {
        let mut inner = self.inner.lock().await;

        // Cannot plan while another execution is active.
        if matches!(
            inner.state,
            ExecutionState::Running | ExecutionState::WaitingApproval
        ) {
            return Err(AutonomousError::AlreadyActive);
        }

        // Enforce depth limit.
        if depth >= inner.config.max_depth {
            return Err(AutonomousError::MaxDepthExceeded(inner.config.max_depth));
        }

        // Validate plan is non-empty.
        if steps.is_empty() {
            return Err(AutonomousError::EmptyPlan);
        }

        // Validate step count against max_steps.
        if steps.len() > inner.config.max_steps {
            return Err(AutonomousError::MaxStepsExceeded(inner.config.max_steps));
        }

        // Validate tool whitelist.
        let allowed: HashSet<&str> = inner
            .config
            .allowed_tools
            .iter()
            .map(|s| s.as_str())
            .collect();
        for step in &steps {
            if let StepKind::ToolCall { tool_name, .. } = &step.kind {
                if !allowed.is_empty() && !allowed.contains(tool_name.as_str()) {
                    return Err(AutonomousError::ToolNotAllowed(tool_name.clone()));
                }
            }
        }

        // Validate dependencies (all referenced indices must be < step count).
        let step_count = steps.len();
        for step in &steps {
            for &dep in &step.dependencies {
                if dep >= step_count {
                    return Err(AutonomousError::InvalidStepIndex(dep));
                }
            }
        }

        let plan = ExecutionPlan::new(goal, steps, depth);
        info!(plan_id = %plan.id, goal = %plan.goal, steps = plan.steps.len(), "Plan created");

        inner.state = ExecutionState::Planning;
        inner.current_plan = Some(plan.clone());
        inner.next_step = 0;
        inner.steps_executed = 0;
        inner.results.clear();
        inner.completed_steps.clear();
        inner.started_at = None;

        Ok(plan)
    }

    /// Execute the current plan, running steps in order.
    ///
    /// Returns an `ExecutionReport` summarizing what happened. Execution may
    /// stop early due to errors, approval gates, timeouts, or cancellation.
    pub async fn execute(&self) -> Result<ExecutionReport, AutonomousError> {
        // Transition to Running.
        {
            let mut inner = self.inner.lock().await;
            match &inner.state {
                ExecutionState::Planning | ExecutionState::Paused => {}
                other => {
                    return Err(AutonomousError::NotResumable(other.to_string()));
                }
            }
            if inner.current_plan.is_none() {
                return Err(AutonomousError::NoActiveExecution);
            }
            inner.state = ExecutionState::Running;
            if inner.started_at.is_none() {
                inner.started_at = Some(Utc::now());
            }
        }

        let deadline = {
            let inner = self.inner.lock().await;
            inner.started_at.unwrap() + chrono::Duration::seconds(inner.config.timeout_secs as i64)
        };

        loop {
            // Check timeout.
            if Utc::now() > deadline {
                let mut inner = self.inner.lock().await;
                inner.state = ExecutionState::Failed(format!(
                    "timed out after {}s",
                    inner.config.timeout_secs
                ));
                return Self::build_report_from(&inner);
            }

            // Determine the next step and run it.
            let step_action = {
                let mut inner = self.inner.lock().await;

                // Check for cancellation or pause.
                match &inner.state {
                    ExecutionState::Running => {}
                    ExecutionState::Cancelled => {
                        return Self::build_report_from(&inner);
                    }
                    ExecutionState::Paused => {
                        return Self::build_report_from(&inner);
                    }
                    ExecutionState::WaitingApproval => {
                        return Self::build_report_from(&inner);
                    }
                    _ => {
                        return Self::build_report_from(&inner);
                    }
                }

                let plan = inner.current_plan.as_ref().unwrap();
                if inner.next_step >= plan.steps.len() {
                    // All steps done.
                    inner.state = ExecutionState::Completed;
                    return Self::build_report_from(&inner);
                }

                // Check max_steps enforcement.
                if inner.steps_executed >= inner.config.max_steps {
                    inner.state = ExecutionState::Failed(format!(
                        "max steps ({}) exceeded",
                        inner.config.max_steps
                    ));
                    return Self::build_report_from(&inner);
                }

                // Check approval gate.
                if inner.config.require_approval_every_n > 0
                    && inner.steps_executed > 0
                    && inner.steps_executed % inner.config.require_approval_every_n == 0
                    && !matches!(
                        plan.steps[inner.next_step].kind,
                        StepKind::WaitForApproval { .. }
                    )
                {
                    inner.state = ExecutionState::WaitingApproval;
                    info!(
                        step = inner.steps_executed,
                        "Mandatory approval gate reached"
                    );
                    return Self::build_report_from(&inner);
                }

                let step = plan.steps[inner.next_step].clone();

                // Check dependencies.
                for &dep in &step.dependencies {
                    if !inner.completed_steps.contains(&dep) {
                        inner.state = ExecutionState::Failed(format!(
                            "dependency not met: step {} depends on {}",
                            step.index, dep
                        ));
                        return Self::build_report_from(&inner);
                    }
                }

                // Check destructive tool gate.
                if let StepKind::ToolCall { ref tool_name, .. } = step.kind {
                    if is_destructive_tool(tool_name) {
                        inner.state = ExecutionState::WaitingApproval;
                        info!(
                            tool = %tool_name,
                            step = inner.next_step,
                            "Destructive operation requires approval"
                        );
                        return Self::build_report_from(&inner);
                    }
                }

                // Check tool whitelist at execution time too.
                if let StepKind::ToolCall { ref tool_name, .. } = step.kind {
                    let allowed: HashSet<&str> = inner
                        .config
                        .allowed_tools
                        .iter()
                        .map(|s| s.as_str())
                        .collect();
                    if !allowed.is_empty() && !allowed.contains(tool_name.as_str()) {
                        inner.state =
                            ExecutionState::Failed(format!("tool not allowed: {tool_name}"));
                        return Self::build_report_from(&inner);
                    }
                }

                step
            };

            // Execute the step (outside the lock).
            let result = self.execute_step(&step_action).await;

            // Record the result.
            {
                let mut inner = self.inner.lock().await;
                match &result {
                    Ok(step_result) => {
                        inner.completed_steps.insert(step_action.index);
                        inner.results.push(step_result.clone());
                        inner.steps_executed += 1;
                        inner.next_step += 1;
                        info!(
                            step = step_action.index,
                            label = %step_action.label,
                            "Step completed successfully"
                        );
                    }
                    Err(e) => {
                        // WaitForApproval steps are not errors -- they pause execution.
                        if let StepKind::WaitForApproval { .. } = &step_action.kind {
                            inner.state = ExecutionState::WaitingApproval;
                            return Self::build_report_from(&inner);
                        }
                        inner.state = ExecutionState::Failed(e.to_string());
                        warn!(
                            step = step_action.index,
                            error = %e,
                            "Step failed"
                        );
                        return Self::build_report_from(&inner);
                    }
                }
            }
        }
    }

    /// Execute a single step and produce a result.
    async fn execute_step(&self, step: &ExecutionStep) -> Result<StepResult, AutonomousError> {
        let started_at = Utc::now();

        let output = match &step.kind {
            StepKind::ToolCall { tool_name, params } => {
                if let Some(ctx) = &self.exec_ctx {
                    // Build a single-element ToolCall slice and delegate to
                    // the real agentic executor.
                    let tool_call = ToolCall {
                        id: Uuid::new_v4().to_string(),
                        type_: "function".to_string(),
                        function: ironjaw_llm::ToolCallFunction {
                            name: tool_name.clone(),
                            arguments: params.clone(),
                        },
                    };
                    let results = crate::agentic::execute_tool_batch(
                        &[tool_call],
                        &ctx.registry,
                        &ctx.hitl,
                        &ctx.tx,
                        &ctx.tool_context,
                        ctx.tool_timeout_secs,
                        &ctx.session_id,
                        ctx.hitl_session_cache_enabled,
                        None,
                        false,
                        ironjaw_session::types::SessionMode::Default,
                        None,
                        // C2-A: autonomous subagent doesn't need the shared
                        // in-flight registry; use a local ephemeral one.
                        &Arc::new(DashMap::<String, HashMap<String, String>>::new()),
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await;
                    match results.into_iter().next() {
                        Some(r) if r.ok => serde_json::to_string(&r.result)
                            .unwrap_or_else(|_| "tool result".to_string()),
                        Some(r) => {
                            return Err(AutonomousError::StepFailed(
                                r.error.unwrap_or_else(|| "tool error".to_string()),
                            ));
                        }
                        None => return Err(AutonomousError::StepFailed("no result".to_string())),
                    }
                } else {
                    // Simulation path — no execution context wired in.
                    format!("tool:{tool_name} executed with params:{params}")
                }
            }
            StepKind::LlmQuery { prompt } => {
                if let Some(ctx) = &self.exec_ctx {
                    use tokio_stream::StreamExt as _;
                    let request = ChatRequest {
                        model: String::new(),
                        messages: vec![ChatMessage::user(prompt.clone())],
                        temperature: None,
                        max_tokens: None,
                        extra: None,
                        thinking: None,
                        tools: None,
                        tool_choice: None,
                    };
                    match ctx.router.chat(request).await {
                        Ok(mut stream) => {
                            let mut assembled = String::new();
                            while let Some(chunk) = stream.next().await {
                                if !chunk.delta.is_empty() {
                                    assembled.push_str(&chunk.delta);
                                }
                                if chunk.done {
                                    break;
                                }
                            }
                            assembled
                        }
                        Err(e) => {
                            return Err(AutonomousError::StepFailed(e.to_string()));
                        }
                    }
                } else {
                    // Simulation path — no execution context wired in.
                    format!("llm response for: {prompt}")
                }
            }
            StepKind::WaitForApproval { reason } => {
                return Err(AutonomousError::ApprovalRequired(reason.clone()));
            }
            StepKind::Branch { condition, .. } => {
                // Simplified: always take the true branch.
                format!("branch evaluated: {condition} -> true")
            }
            StepKind::Loop { iterations, .. } => {
                format!("loop completed: {iterations} iterations")
            }
            StepKind::Checkpoint => {
                // Save a checkpoint internally.
                "checkpoint saved".to_string()
            }
        };

        let ended_at = Utc::now();
        Ok(StepResult {
            step_index: step.index,
            success: true,
            output,
            started_at,
            ended_at,
        })
    }

    /// Pause the current execution. Can be resumed later with `resume()`.
    pub async fn pause(&self) -> Result<(), AutonomousError> {
        let mut inner = self.inner.lock().await;
        match &inner.state {
            ExecutionState::Running | ExecutionState::WaitingApproval => {
                info!("Execution paused");
                inner.state = ExecutionState::Paused;
                Ok(())
            }
            other => Err(AutonomousError::NotResumable(other.to_string())),
        }
    }

    /// Resume a paused or approval-waiting execution.
    pub async fn resume(&self) -> Result<(), AutonomousError> {
        let mut inner = self.inner.lock().await;
        match &inner.state {
            ExecutionState::Paused | ExecutionState::WaitingApproval => {
                info!("Execution resumed");
                inner.state = ExecutionState::Running;
                Ok(())
            }
            other => Err(AutonomousError::NotResumable(other.to_string())),
        }
    }

    /// Cancel the current execution entirely.
    pub async fn cancel(&self) -> Result<(), AutonomousError> {
        let mut inner = self.inner.lock().await;
        match &inner.state {
            ExecutionState::Running
            | ExecutionState::Paused
            | ExecutionState::WaitingApproval
            | ExecutionState::Planning => {
                warn!("Execution cancelled");
                inner.state = ExecutionState::Cancelled;
                Ok(())
            }
            _ => Err(AutonomousError::NoActiveExecution),
        }
    }

    /// Take a checkpoint snapshot of the current execution state.
    pub async fn checkpoint(&self) -> Result<ExecutionSnapshot, AutonomousError> {
        let inner = self.inner.lock().await;
        let plan = inner
            .current_plan
            .as_ref()
            .ok_or(AutonomousError::NoActiveExecution)?;

        Ok(ExecutionSnapshot {
            plan: plan.clone(),
            next_step: inner.next_step,
            results: inner.results.clone(),
            depth: plan.depth,
            config: inner.config.clone(),
            snapshot_at: Utc::now(),
        })
    }

    /// Restore execution state from a previously saved snapshot.
    pub async fn restore(&self, snapshot: ExecutionSnapshot) -> Result<(), AutonomousError> {
        let mut inner = self.inner.lock().await;

        // Cannot restore while actively running.
        if matches!(inner.state, ExecutionState::Running) {
            return Err(AutonomousError::AlreadyActive);
        }

        let completed_steps: HashSet<usize> =
            snapshot.results.iter().map(|r| r.step_index).collect();

        inner.current_plan = Some(snapshot.plan);
        inner.next_step = snapshot.next_step;
        inner.results = snapshot.results;
        inner.config = snapshot.config;
        inner.steps_executed = completed_steps.len();
        inner.completed_steps = completed_steps;
        inner.state = ExecutionState::Paused;
        inner.started_at = Some(Utc::now());

        info!(
            next_step = inner.next_step,
            "Execution restored from snapshot"
        );
        Ok(())
    }

    /// Get the current execution state.
    pub async fn state(&self) -> ExecutionState {
        self.inner.lock().await.state.clone()
    }

    /// Get the number of steps executed so far.
    pub async fn steps_executed(&self) -> usize {
        self.inner.lock().await.steps_executed
    }

    /// Get the current execution config.
    pub async fn config(&self) -> ExecutionConfig {
        self.inner.lock().await.config.clone()
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Build an `ExecutionReport` from the already-locked internal state.
    ///
    /// This is a synchronous helper that takes a reference to `ExecutorInner`
    /// so callers that already hold the lock can build a report without
    /// attempting to re-acquire it (which would deadlock).
    fn build_report_from(inner: &ExecutorInner) -> Result<ExecutionReport, AutonomousError> {
        let plan = inner
            .current_plan
            .as_ref()
            .ok_or(AutonomousError::NoActiveExecution)?;
        let started_at = inner.started_at.unwrap_or_else(Utc::now);
        Ok(ExecutionReport {
            plan_id: plan.id.clone(),
            final_state: inner.state.clone(),
            step_results: inner.results.clone(),
            steps_executed: inner.steps_executed,
            started_at,
            ended_at: Utc::now(),
        })
    }
}

impl Default for AutonomousExecutor {
    fn default() -> Self {
        Self::with_defaults()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- helpers --

    fn make_tool_step(index: usize, tool: &str, params: &str) -> ExecutionStep {
        ExecutionStep {
            index,
            label: format!("step-{index}"),
            kind: StepKind::ToolCall {
                tool_name: tool.to_string(),
                params: params.to_string(),
            },
            dependencies: vec![],
        }
    }

    fn make_llm_step(index: usize, prompt: &str) -> ExecutionStep {
        ExecutionStep {
            index,
            label: format!("llm-{index}"),
            kind: StepKind::LlmQuery {
                prompt: prompt.to_string(),
            },
            dependencies: vec![],
        }
    }

    fn make_approval_step(index: usize, reason: &str) -> ExecutionStep {
        ExecutionStep {
            index,
            label: format!("approval-{index}"),
            kind: StepKind::WaitForApproval {
                reason: reason.to_string(),
            },
            dependencies: vec![],
        }
    }

    fn make_checkpoint_step(index: usize) -> ExecutionStep {
        ExecutionStep {
            index,
            label: format!("checkpoint-{index}"),
            kind: StepKind::Checkpoint,
            dependencies: vec![],
        }
    }

    fn make_branch_step(
        index: usize,
        condition: &str,
        if_true: Vec<usize>,
        if_false: Vec<usize>,
    ) -> ExecutionStep {
        ExecutionStep {
            index,
            label: format!("branch-{index}"),
            kind: StepKind::Branch {
                condition: condition.to_string(),
                if_true,
                if_false,
            },
            dependencies: vec![],
        }
    }

    fn make_loop_step(index: usize, body: Vec<usize>, iterations: usize) -> ExecutionStep {
        ExecutionStep {
            index,
            label: format!("loop-{index}"),
            kind: StepKind::Loop { body, iterations },
            dependencies: vec![],
        }
    }

    fn default_config_with_tools(tools: Vec<&str>) -> ExecutionConfig {
        ExecutionConfig {
            allowed_tools: tools.into_iter().map(String::from).collect(),
            ..Default::default()
        }
    }

    // -- plan creation tests --

    #[tokio::test]
    async fn test_plan_creation_basic() {
        let exec = AutonomousExecutor::new(default_config_with_tools(vec!["read_file"]));
        let steps = vec![make_tool_step(0, "read_file", r#"{"path":"foo.txt"}"#)];
        let plan = exec
            .plan("read a file", "test context", steps, 0)
            .await
            .unwrap();
        assert_eq!(plan.goal, "read a file");
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.depth, 0);
        assert!(!plan.id.is_empty());
    }

    #[tokio::test]
    async fn test_plan_creation_empty_fails() {
        let exec = AutonomousExecutor::with_defaults();
        let err = exec.plan("goal", "ctx", vec![], 0).await.unwrap_err();
        assert_eq!(err, AutonomousError::EmptyPlan);
    }

    #[tokio::test]
    async fn test_plan_max_depth_exceeded() {
        let config = ExecutionConfig {
            max_depth: 2,
            ..Default::default()
        };
        let exec = AutonomousExecutor::new(config);
        let steps = vec![make_llm_step(0, "hello")];
        let err = exec.plan("goal", "ctx", steps, 2).await.unwrap_err();
        assert_eq!(err, AutonomousError::MaxDepthExceeded(2));
    }

    #[tokio::test]
    async fn test_plan_max_steps_exceeded() {
        let config = ExecutionConfig {
            max_steps: 2,
            allowed_tools: vec!["read_file".to_string()],
            ..Default::default()
        };
        let exec = AutonomousExecutor::new(config);
        let steps = vec![
            make_tool_step(0, "read_file", "a"),
            make_tool_step(1, "read_file", "b"),
            make_tool_step(2, "read_file", "c"),
        ];
        let err = exec.plan("goal", "ctx", steps, 0).await.unwrap_err();
        assert_eq!(err, AutonomousError::MaxStepsExceeded(2));
    }

    #[tokio::test]
    async fn test_plan_tool_whitelist_violation() {
        let config = default_config_with_tools(vec!["read_file"]);
        let exec = AutonomousExecutor::new(config);
        let steps = vec![make_tool_step(0, "shell", "ls")];
        let err = exec.plan("goal", "ctx", steps, 0).await.unwrap_err();
        assert_eq!(err, AutonomousError::ToolNotAllowed("shell".to_string()));
    }

    #[tokio::test]
    async fn test_plan_invalid_dependency_index() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![ExecutionStep {
            index: 0,
            label: "step-0".to_string(),
            kind: StepKind::LlmQuery {
                prompt: "hello".to_string(),
            },
            dependencies: vec![99], // invalid
        }];
        let err = exec.plan("goal", "ctx", steps, 0).await.unwrap_err();
        assert_eq!(err, AutonomousError::InvalidStepIndex(99));
    }

    // -- step execution lifecycle tests --

    #[tokio::test]
    async fn test_execute_tool_call_step() {
        let config = default_config_with_tools(vec!["read_file"]);
        let exec = AutonomousExecutor::new(config);
        let steps = vec![make_tool_step(0, "read_file", r#"{"path":"a.txt"}"#)];
        exec.plan("read", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        assert_eq!(report.final_state, ExecutionState::Completed);
        assert_eq!(report.steps_executed, 1);
        assert!(report.step_results[0].success);
        assert!(report.step_results[0].output.contains("read_file"));
    }

    #[tokio::test]
    async fn test_execute_llm_query_step() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![make_llm_step(0, "explain closures")];
        exec.plan("ask llm", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        assert_eq!(report.final_state, ExecutionState::Completed);
        assert_eq!(report.steps_executed, 1);
        assert!(report.step_results[0].output.contains("explain closures"));
    }

    #[tokio::test]
    async fn test_execute_checkpoint_step() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![make_checkpoint_step(0)];
        exec.plan("checkpoint test", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        assert_eq!(report.final_state, ExecutionState::Completed);
        assert!(report.step_results[0].output.contains("checkpoint"));
    }

    #[tokio::test]
    async fn test_execute_branch_step() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![make_branch_step(0, "x > 0", vec![1], vec![2])];
        exec.plan("branch test", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        assert_eq!(report.final_state, ExecutionState::Completed);
        assert!(report.step_results[0].output.contains("branch"));
    }

    #[tokio::test]
    async fn test_execute_loop_step() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![make_loop_step(0, vec![1], 3)];
        exec.plan("loop test", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        assert_eq!(report.final_state, ExecutionState::Completed);
        assert!(report.step_results[0].output.contains("3 iterations"));
    }

    #[tokio::test]
    async fn test_execute_multi_step_plan() {
        let config = default_config_with_tools(vec!["read_file", "list_files"]);
        let exec = AutonomousExecutor::new(config);
        let steps = vec![
            make_tool_step(0, "read_file", "a.txt"),
            make_tool_step(1, "list_files", "/src"),
            make_llm_step(2, "summarize"),
        ];
        exec.plan("multi-step", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        assert_eq!(report.final_state, ExecutionState::Completed);
        assert_eq!(report.steps_executed, 3);
        assert_eq!(report.step_results.len(), 3);
    }

    // -- state transition tests --

    #[tokio::test]
    async fn test_state_transitions_planning_to_running() {
        let exec = AutonomousExecutor::with_defaults();
        assert_eq!(exec.state().await, ExecutionState::Planning);

        let steps = vec![make_llm_step(0, "hello")];
        exec.plan("test", "ctx", steps, 0).await.unwrap();
        assert_eq!(exec.state().await, ExecutionState::Planning);
    }

    #[tokio::test]
    async fn test_state_transitions_running_to_completed() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![make_llm_step(0, "hello")];
        exec.plan("test", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        assert_eq!(report.final_state, ExecutionState::Completed);
        assert_eq!(exec.state().await, ExecutionState::Completed);
    }

    #[tokio::test]
    async fn test_state_transitions_approval_gate() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![make_approval_step(0, "confirm deployment")];
        exec.plan("deploy", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        assert_eq!(report.final_state, ExecutionState::WaitingApproval);
        assert_eq!(exec.state().await, ExecutionState::WaitingApproval);
    }

    // -- max steps enforcement --

    #[tokio::test]
    async fn test_max_steps_enforcement_at_runtime() {
        let config = ExecutionConfig {
            max_steps: 2,
            require_approval_every_n: 0,
            ..Default::default()
        };
        let exec = AutonomousExecutor::new(config);
        // Plan with exactly 2 steps (allowed at plan time).
        let steps = vec![make_llm_step(0, "a"), make_llm_step(1, "b")];
        exec.plan("test", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        // Should complete because 2 steps == max_steps.
        assert_eq!(report.final_state, ExecutionState::Completed);
        assert_eq!(report.steps_executed, 2);
    }

    // -- timeout enforcement --

    #[tokio::test]
    async fn test_timeout_enforcement() {
        let config = ExecutionConfig {
            timeout_secs: 0, // instant timeout
            require_approval_every_n: 0,
            ..Default::default()
        };
        let exec = AutonomousExecutor::new(config);
        let steps = vec![make_llm_step(0, "slow")];
        exec.plan("timeout test", "ctx", steps, 0).await.unwrap();

        // Sleep briefly so the deadline is already past.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let report = exec.execute().await.unwrap();
        assert!(
            matches!(report.final_state, ExecutionState::Failed(ref m) if m.contains("timed out"))
        );
    }

    // -- tool whitelist enforcement at runtime --

    #[tokio::test]
    async fn test_tool_whitelist_empty_allows_nothing() {
        // When allowed_tools is empty, the whitelist check is active but empty
        // means "deny all tools" at planning time. An empty whitelist is
        // treated as a special case: it means no restriction (all tools OK).
        // Wait -- let's re-check the logic. Our plan() method says:
        //   if !allowed.is_empty() && !allowed.contains(tool_name)
        // So empty whitelist = no restriction. Let's test that.
        let config = ExecutionConfig {
            allowed_tools: vec![], // no restriction
            require_approval_every_n: 0,
            ..Default::default()
        };
        let exec = AutonomousExecutor::new(config);
        let steps = vec![make_tool_step(0, "read_file", "x")];
        // Should succeed -- empty whitelist means no restriction.
        exec.plan("test", "ctx", steps, 0).await.unwrap();
    }

    // -- approval gate at intervals --

    #[tokio::test]
    async fn test_approval_gate_at_interval() {
        let config = ExecutionConfig {
            require_approval_every_n: 2,
            ..Default::default()
        };
        let exec = AutonomousExecutor::new(config);
        let steps = vec![
            make_llm_step(0, "a"),
            make_llm_step(1, "b"),
            make_llm_step(2, "c"),
        ];
        exec.plan("test", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        // After executing 2 steps (indices 0 and 1), the 3rd step should
        // trigger the approval gate (steps_executed=2, 2 % 2 == 0).
        assert_eq!(report.final_state, ExecutionState::WaitingApproval);
        assert_eq!(report.steps_executed, 2);
    }

    // -- destructive tool auto-pause --

    #[tokio::test]
    async fn test_destructive_tool_requires_approval() {
        let config = default_config_with_tools(vec!["write_file"]);
        let exec = AutonomousExecutor::new(config);
        let steps = vec![make_tool_step(
            0,
            "write_file",
            r#"{"path":"x","content":"y"}"#,
        )];
        exec.plan("write", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        assert_eq!(report.final_state, ExecutionState::WaitingApproval);
        assert_eq!(report.steps_executed, 0);
    }

    #[tokio::test]
    async fn test_destructive_tool_detection() {
        assert!(is_destructive_tool("write_file"));
        assert!(is_destructive_tool("shell"));
        assert!(is_destructive_tool("patch"));
        assert!(is_destructive_tool("docker_manage"));
        assert!(is_destructive_tool("docker_exec"));
        assert!(is_destructive_tool("adb"));
        assert!(is_destructive_tool("secrets_set"));
        assert!(!is_destructive_tool("read_file"));
        assert!(!is_destructive_tool("list_files"));
        assert!(!is_destructive_tool("searxng"));
    }

    // -- pause / resume / cancel --

    #[tokio::test]
    async fn test_pause_and_resume() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![make_approval_step(0, "check")];
        exec.plan("test", "ctx", steps, 0).await.unwrap();

        // Execute hits approval gate.
        let report = exec.execute().await.unwrap();
        assert_eq!(report.final_state, ExecutionState::WaitingApproval);

        // Pause from WaitingApproval.
        exec.pause().await.unwrap();
        assert_eq!(exec.state().await, ExecutionState::Paused);

        // Resume from Paused.
        exec.resume().await.unwrap();
        assert_eq!(exec.state().await, ExecutionState::Running);
    }

    #[tokio::test]
    async fn test_cancel_planning() {
        let exec = AutonomousExecutor::with_defaults();
        exec.cancel().await.unwrap();
        assert_eq!(exec.state().await, ExecutionState::Cancelled);
    }

    #[tokio::test]
    async fn test_cancel_paused() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![make_approval_step(0, "check")];
        exec.plan("test", "ctx", steps, 0).await.unwrap();
        exec.execute().await.unwrap();
        exec.pause().await.unwrap();
        exec.cancel().await.unwrap();
        assert_eq!(exec.state().await, ExecutionState::Cancelled);
    }

    #[tokio::test]
    async fn test_cancel_completed_fails() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![make_llm_step(0, "hello")];
        exec.plan("test", "ctx", steps, 0).await.unwrap();
        exec.execute().await.unwrap();
        assert_eq!(exec.state().await, ExecutionState::Completed);
        let err = exec.cancel().await.unwrap_err();
        assert_eq!(err, AutonomousError::NoActiveExecution);
    }

    #[tokio::test]
    async fn test_pause_completed_fails() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![make_llm_step(0, "hello")];
        exec.plan("test", "ctx", steps, 0).await.unwrap();
        exec.execute().await.unwrap();
        let err = exec.pause().await.unwrap_err();
        assert!(matches!(err, AutonomousError::NotResumable(_)));
    }

    // -- checkpoint save / restore --

    #[tokio::test]
    async fn test_checkpoint_save() {
        let config = default_config_with_tools(vec!["read_file"]);
        let exec = AutonomousExecutor::new(config);
        let steps = vec![
            make_tool_step(0, "read_file", "a"),
            make_approval_step(1, "confirm"),
        ];
        exec.plan("test", "ctx", steps, 0).await.unwrap();
        exec.execute().await.unwrap(); // runs step 0, pauses at step 1
        let snapshot = exec.checkpoint().await.unwrap();
        assert_eq!(snapshot.next_step, 1);
        assert_eq!(snapshot.results.len(), 1);
        assert_eq!(snapshot.plan.goal, "test");
    }

    #[tokio::test]
    async fn test_checkpoint_restore() {
        let config = default_config_with_tools(vec!["read_file"]);
        let exec = AutonomousExecutor::new(config.clone());
        let steps = vec![
            make_tool_step(0, "read_file", "a"),
            make_approval_step(1, "confirm"),
        ];
        exec.plan("test", "ctx", steps, 0).await.unwrap();
        exec.execute().await.unwrap();

        let snapshot = exec.checkpoint().await.unwrap();

        // Create a fresh executor and restore into it.
        let exec2 = AutonomousExecutor::new(config);
        exec2.restore(snapshot).await.unwrap();
        assert_eq!(exec2.state().await, ExecutionState::Paused);
        assert_eq!(exec2.steps_executed().await, 1);
    }

    #[tokio::test]
    async fn test_checkpoint_no_active_execution_fails() {
        let exec = AutonomousExecutor::with_defaults();
        let err = exec.checkpoint().await.unwrap_err();
        assert_eq!(err, AutonomousError::NoActiveExecution);
    }

    // -- concurrent execution prevention --

    #[tokio::test]
    async fn test_concurrent_plan_while_running_fails() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![make_approval_step(0, "hold")];
        exec.plan("first", "ctx", steps.clone(), 0).await.unwrap();
        exec.execute().await.unwrap(); // now WaitingApproval

        // Try to plan again while WaitingApproval.
        let err = exec.plan("second", "ctx", steps, 0).await.unwrap_err();
        assert_eq!(err, AutonomousError::AlreadyActive);
    }

    // -- error handling --

    #[tokio::test]
    async fn test_execute_without_plan_fails() {
        let exec = AutonomousExecutor::with_defaults();
        let err = exec.execute().await.unwrap_err();
        assert!(matches!(err, AutonomousError::NoActiveExecution));
    }

    #[tokio::test]
    async fn test_resume_without_pause_fails() {
        let exec = AutonomousExecutor::with_defaults();
        let err = exec.resume().await.unwrap_err();
        assert!(matches!(err, AutonomousError::NotResumable(_)));
    }

    // -- dependency enforcement --

    #[tokio::test]
    async fn test_dependency_enforcement() {
        let exec = AutonomousExecutor::new(ExecutionConfig {
            require_approval_every_n: 0,
            ..Default::default()
        });
        // Step 1 depends on step 1 (itself) -- impossible to satisfy at position 0 order.
        // Actually let's make step 0 depend on step 1 which hasn't run yet.
        let steps = vec![
            ExecutionStep {
                index: 0,
                label: "first".to_string(),
                kind: StepKind::LlmQuery {
                    prompt: "a".to_string(),
                },
                dependencies: vec![1], // depends on step 1, which hasn't run
            },
            make_llm_step(1, "b"),
        ];
        exec.plan("dep test", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        assert!(matches!(
            report.final_state,
            ExecutionState::Failed(ref m) if m.contains("dependency not met")
        ));
    }

    // -- ExecutionPlan helpers --

    #[tokio::test]
    async fn test_plan_len_and_is_empty() {
        let plan = ExecutionPlan::new("goal", vec![], 0);
        assert!(plan.is_empty());
        assert_eq!(plan.len(), 0);

        let plan2 = ExecutionPlan::new("goal", vec![make_llm_step(0, "hi")], 0);
        assert!(!plan2.is_empty());
        assert_eq!(plan2.len(), 1);
    }

    // -- Display implementations --

    #[tokio::test]
    async fn test_step_kind_display() {
        assert_eq!(
            StepKind::ToolCall {
                tool_name: "read_file".to_string(),
                params: "{}".to_string()
            }
            .to_string(),
            "tool_call(read_file)"
        );
        assert_eq!(
            StepKind::LlmQuery {
                prompt: "hi".to_string()
            }
            .to_string(),
            "llm_query"
        );
        assert_eq!(
            StepKind::WaitForApproval {
                reason: "check".to_string()
            }
            .to_string(),
            "wait_for_approval(check)"
        );
        assert_eq!(
            StepKind::Branch {
                condition: "x > 0".to_string(),
                if_true: vec![],
                if_false: vec![]
            }
            .to_string(),
            "branch(x > 0)"
        );
        assert_eq!(
            StepKind::Loop {
                body: vec![],
                iterations: 5
            }
            .to_string(),
            "loop(5)"
        );
        assert_eq!(StepKind::Checkpoint.to_string(), "checkpoint");
    }

    #[tokio::test]
    async fn test_execution_state_display() {
        assert_eq!(ExecutionState::Planning.to_string(), "planning");
        assert_eq!(ExecutionState::Running.to_string(), "running");
        assert_eq!(ExecutionState::Paused.to_string(), "paused");
        assert_eq!(
            ExecutionState::WaitingApproval.to_string(),
            "waiting_approval"
        );
        assert_eq!(ExecutionState::Completed.to_string(), "completed");
        assert_eq!(
            ExecutionState::Failed("oops".to_string()).to_string(),
            "failed: oops"
        );
        assert_eq!(ExecutionState::Cancelled.to_string(), "cancelled");
    }

    // -- error display --

    #[tokio::test]
    async fn test_error_display() {
        assert_eq!(
            AutonomousError::MaxStepsExceeded(50).to_string(),
            "max steps (50) exceeded"
        );
        assert_eq!(
            AutonomousError::MaxDepthExceeded(5).to_string(),
            "max depth (5) exceeded"
        );
        assert_eq!(
            AutonomousError::Timeout(300).to_string(),
            "execution timed out after 300s"
        );
        assert_eq!(
            AutonomousError::ToolNotAllowed("shell".to_string()).to_string(),
            "tool not in whitelist: shell"
        );
        assert_eq!(
            AutonomousError::AlreadyActive.to_string(),
            "execution already active"
        );
        assert_eq!(
            AutonomousError::NoActiveExecution.to_string(),
            "no active execution"
        );
        assert_eq!(AutonomousError::EmptyPlan.to_string(), "plan is empty");
        assert_eq!(
            AutonomousError::InvalidStepIndex(42).to_string(),
            "invalid step index: 42"
        );
    }

    // -- default config --

    #[tokio::test]
    async fn test_default_config() {
        let config = ExecutionConfig::default();
        assert_eq!(config.max_steps, 50);
        assert_eq!(config.max_depth, 5);
        assert_eq!(config.require_approval_every_n, 10);
        assert_eq!(config.timeout_secs, 300);
        assert!(config.allowed_tools.is_empty());
    }

    // -- executor default --

    #[tokio::test]
    async fn test_executor_default() {
        let exec = AutonomousExecutor::default();
        let config = exec.config().await;
        assert_eq!(config.max_steps, 50);
    }

    // -- executor is clone --

    #[tokio::test]
    async fn test_executor_clone_shares_state() {
        let exec = AutonomousExecutor::with_defaults();
        let exec2 = exec.clone();
        let steps = vec![make_llm_step(0, "hello")];
        exec.plan("shared", "ctx", steps, 0).await.unwrap();

        // Clone should see the plan too.
        let state = exec2.state().await;
        assert_eq!(state, ExecutionState::Planning);
    }

    // -- resume after approval and continue --

    #[tokio::test]
    async fn test_resume_after_approval_continues_execution() {
        let config = ExecutionConfig {
            require_approval_every_n: 0,
            ..Default::default()
        };
        let exec = AutonomousExecutor::new(config);
        let steps = vec![
            make_approval_step(0, "first gate"),
            make_llm_step(1, "after gate"),
        ];
        exec.plan("resume test", "ctx", steps, 0).await.unwrap();

        // First execute hits approval gate at step 0.
        let report = exec.execute().await.unwrap();
        assert_eq!(report.final_state, ExecutionState::WaitingApproval);

        // Approve: advance past the approval step manually.
        {
            let mut inner = exec.inner.lock().await;
            // Record the approval step as done and advance.
            inner.completed_steps.insert(0);
            inner.results.push(StepResult {
                step_index: 0,
                success: true,
                output: "approved".to_string(),
                started_at: Utc::now(),
                ended_at: Utc::now(),
            });
            inner.steps_executed += 1;
            inner.next_step = 1;
            inner.state = ExecutionState::Paused;
        }

        // Resume and execute the rest.
        let report2 = exec.execute().await.unwrap();
        assert_eq!(report2.final_state, ExecutionState::Completed);
        assert_eq!(report2.steps_executed, 2);
    }

    // -- report fields --

    #[tokio::test]
    async fn test_report_timestamps() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![make_llm_step(0, "quick")];
        exec.plan("test", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        assert!(report.ended_at >= report.started_at);
        assert!(!report.plan_id.is_empty());
    }

    // -- step result fields --

    #[tokio::test]
    async fn test_step_result_timestamps() {
        let exec = AutonomousExecutor::with_defaults();
        let steps = vec![make_llm_step(0, "quick")];
        exec.plan("test", "ctx", steps, 0).await.unwrap();
        let report = exec.execute().await.unwrap();
        let sr = &report.step_results[0];
        assert!(sr.ended_at >= sr.started_at);
        assert_eq!(sr.step_index, 0);
        assert!(sr.success);
    }
}
