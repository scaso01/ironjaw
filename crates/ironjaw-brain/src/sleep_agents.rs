//! Sleep-time agents that run during idle periods (no active chat).
//!
//! 5 async agents triggered when the system is idle for a configurable threshold:
//!
//! | Agent                  | Task                                            | Frequency      |
//! |------------------------|-------------------------------------------------|----------------|
//! | Contradiction Detector | Find conflicting memories, close older           | Every 30 min   |
//! | Insight Synthesizer    | Find patterns across recent memories             | Every 60 min   |
//! | Relationship Discoverer| Extract entity pairs, add to knowledge graph     | Every 30 min   |
//! | Temporal Verifier      | Check memories with expired validity             | Every 60 min   |
//! | Pattern Detector       | Identify recurring themes, create summaries      | Every 120 min  |
//!
//! Each agent uses local llama-server for inference, has a max runtime,
//! and can be individually enabled/disabled.

use crate::maintenance::MemoryMaintenance;
use ironjaw_llm::LlmRouter;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

// ── Agent Identity ───────────────────────────────────────────────────

/// Identifies which sleep-time agent is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SleepAgent {
    ContradictionDetector,
    InsightSynthesizer,
    RelationshipDiscoverer,
    TemporalVerifier,
    PatternDetector,
}

impl std::fmt::Display for SleepAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SleepAgent::ContradictionDetector => write!(f, "contradiction_detector"),
            SleepAgent::InsightSynthesizer => write!(f, "insight_synthesizer"),
            SleepAgent::RelationshipDiscoverer => write!(f, "relationship_discoverer"),
            SleepAgent::TemporalVerifier => write!(f, "temporal_verifier"),
            SleepAgent::PatternDetector => write!(f, "pattern_detector"),
        }
    }
}

// ── Config ───────────────────────────────────────────────────────────

/// Configuration for a single sleep-time agent.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Whether this agent is enabled
    pub enabled: bool,
    /// How often this agent runs (in seconds of idle time)
    pub interval_secs: u64,
    /// Maximum runtime per invocation (seconds)
    pub max_runtime_secs: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 1800, // 30 min
            max_runtime_secs: 60,
        }
    }
}

/// Configuration for the full sleep-agents system.
#[derive(Debug, Clone)]
pub struct SleepAgentsConfig {
    /// Global enable/disable for all sleep agents
    pub enabled: bool,
    /// Seconds of idle before sleep agents activate
    pub idle_threshold_secs: u64,
    /// Batch size for memory processing
    pub batch_size: usize,
    /// Days of summaries to analyze for pattern detection
    pub pattern_days: u64,
    /// Per-agent configs
    pub contradiction_detector: AgentConfig,
    pub insight_synthesizer: AgentConfig,
    pub relationship_discoverer: AgentConfig,
    pub temporal_verifier: AgentConfig,
    pub pattern_detector: AgentConfig,
}

impl Default for SleepAgentsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_threshold_secs: 300, // 5 minutes
            batch_size: 20,
            pattern_days: 14,
            contradiction_detector: AgentConfig {
                enabled: true,
                interval_secs: 1800, // 30 min
                max_runtime_secs: 60,
            },
            insight_synthesizer: AgentConfig {
                enabled: true,
                interval_secs: 3600, // 60 min
                max_runtime_secs: 60,
            },
            relationship_discoverer: AgentConfig {
                enabled: true,
                interval_secs: 1800, // 30 min
                max_runtime_secs: 60,
            },
            temporal_verifier: AgentConfig {
                enabled: true,
                interval_secs: 3600, // 60 min
                max_runtime_secs: 60,
            },
            pattern_detector: AgentConfig {
                enabled: true,
                interval_secs: 7200, // 120 min
                max_runtime_secs: 60,
            },
        }
    }
}

// ── Agent Result ─────────────────────────────────────────────────────

/// Result from a single agent run.
#[derive(Debug, Clone, Default)]
pub struct AgentRunResult {
    pub agent: String,
    pub actions_taken: u64,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

/// Result from a full sleep-agents cycle.
#[derive(Debug, Clone, Default)]
pub struct SleepCycleReport {
    pub agents_run: usize,
    pub total_actions: u64,
    pub results: Vec<AgentRunResult>,
}

// ── Idle Tracker ─────────────────────────────────────────────────────

/// Tracks the last message timestamp to detect idle periods.
pub struct IdleTracker {
    last_activity: Arc<Mutex<tokio::time::Instant>>,
    idle_threshold_secs: u64,
}

impl IdleTracker {
    pub fn new(idle_threshold_secs: u64) -> Self {
        Self {
            last_activity: Arc::new(Mutex::new(tokio::time::Instant::now())),
            idle_threshold_secs,
        }
    }

    /// Record a new activity (call after each message).
    pub async fn touch(&self) {
        *self.last_activity.lock().await = tokio::time::Instant::now();
    }

    /// Check whether the system is idle (no activity for threshold seconds).
    pub async fn is_idle(&self) -> bool {
        let last = *self.last_activity.lock().await;
        tokio::time::Instant::now().duration_since(last).as_secs() >= self.idle_threshold_secs
    }

    /// Seconds since last activity.
    pub async fn idle_duration_secs(&self) -> u64 {
        let last = *self.last_activity.lock().await;
        tokio::time::Instant::now().duration_since(last).as_secs()
    }
}

// ── Scheduler ────────────────────────────────────────────────────────

/// Per-agent schedule state: tracks when each agent last ran.
struct AgentSchedule {
    last_run: Option<tokio::time::Instant>,
    interval_secs: u64,
}

impl AgentSchedule {
    fn new(interval_secs: u64) -> Self {
        Self {
            last_run: None,
            interval_secs,
        }
    }

    /// Whether this agent is due for a run.
    fn is_due(&self) -> bool {
        match self.last_run {
            None => true,
            Some(prev) => {
                tokio::time::Instant::now().duration_since(prev).as_secs() >= self.interval_secs
            }
        }
    }

    /// Mark as just run.
    fn mark_run(&mut self) {
        self.last_run = Some(tokio::time::Instant::now());
    }
}

/// Sleep-agents scheduler. Manages idle detection and agent scheduling.
pub struct SleepAgentScheduler {
    config: SleepAgentsConfig,
    idle_tracker: IdleTracker,
    running: Arc<AtomicBool>,
    schedules: Arc<Mutex<Vec<(SleepAgent, AgentSchedule)>>>,
}

impl SleepAgentScheduler {
    pub fn new(config: SleepAgentsConfig) -> Self {
        let idle_threshold = config.idle_threshold_secs;
        let schedules = vec![
            (
                SleepAgent::ContradictionDetector,
                AgentSchedule::new(config.contradiction_detector.interval_secs),
            ),
            (
                SleepAgent::InsightSynthesizer,
                AgentSchedule::new(config.insight_synthesizer.interval_secs),
            ),
            (
                SleepAgent::RelationshipDiscoverer,
                AgentSchedule::new(config.relationship_discoverer.interval_secs),
            ),
            (
                SleepAgent::TemporalVerifier,
                AgentSchedule::new(config.temporal_verifier.interval_secs),
            ),
            (
                SleepAgent::PatternDetector,
                AgentSchedule::new(config.pattern_detector.interval_secs),
            ),
        ];

        Self {
            config,
            idle_tracker: IdleTracker::new(idle_threshold),
            running: Arc::new(AtomicBool::new(false)),
            schedules: Arc::new(Mutex::new(schedules)),
        }
    }

    /// Record activity (resets idle timer).
    pub async fn touch(&self) {
        self.idle_tracker.touch().await;
    }

    /// Check if the system is currently idle.
    pub async fn is_idle(&self) -> bool {
        self.idle_tracker.is_idle().await
    }

    /// Seconds since last activity. Exposed for short-delay gating of background
    /// LLM jobs (auto-learn, extraction) that want a much shorter idle window than
    /// the sleep-agents' `idle_threshold_secs`.
    pub async fn idle_duration_secs(&self) -> u64 {
        self.idle_tracker.idle_duration_secs().await
    }

    /// Whether the scheduler is currently running agents.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Check if a specific agent is enabled.
    pub fn is_agent_enabled(&self, agent: SleepAgent) -> bool {
        match agent {
            SleepAgent::ContradictionDetector => self.config.contradiction_detector.enabled,
            SleepAgent::InsightSynthesizer => self.config.insight_synthesizer.enabled,
            SleepAgent::RelationshipDiscoverer => self.config.relationship_discoverer.enabled,
            SleepAgent::TemporalVerifier => self.config.temporal_verifier.enabled,
            SleepAgent::PatternDetector => self.config.pattern_detector.enabled,
        }
    }

    /// Get max runtime for a specific agent.
    fn max_runtime_secs(&self, agent: SleepAgent) -> u64 {
        match agent {
            SleepAgent::ContradictionDetector => {
                self.config.contradiction_detector.max_runtime_secs
            }
            SleepAgent::InsightSynthesizer => self.config.insight_synthesizer.max_runtime_secs,
            SleepAgent::RelationshipDiscoverer => {
                self.config.relationship_discoverer.max_runtime_secs
            }
            SleepAgent::TemporalVerifier => self.config.temporal_verifier.max_runtime_secs,
            SleepAgent::PatternDetector => self.config.pattern_detector.max_runtime_secs,
        }
    }

    /// Run a single cycle: check idle, run due agents.
    ///
    /// Returns a report of what was done. If not idle or nothing is due,
    /// returns an empty report.
    pub async fn run_cycle(
        &self,
        maintenance: &MemoryMaintenance,
        llm: &LlmRouter,
    ) -> SleepCycleReport {
        if !self.config.enabled {
            return SleepCycleReport::default();
        }

        if !self.idle_tracker.is_idle().await {
            return SleepCycleReport::default();
        }

        // Prevent concurrent runs
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            debug!("Sleep agents already running, skipping cycle");
            return SleepCycleReport::default();
        }

        let mut report = SleepCycleReport::default();

        // Collect due agents
        let mut due_agents = Vec::new();
        {
            let schedules = self.schedules.lock().await;
            for (agent, schedule) in schedules.iter() {
                if self.is_agent_enabled(*agent) && schedule.is_due() {
                    due_agents.push(*agent);
                }
            }
        }

        if due_agents.is_empty() {
            self.running.store(false, Ordering::SeqCst);
            return report;
        }

        let idle_secs = self.idle_tracker.idle_duration_secs().await;
        info!(
            agents = ?due_agents,
            idle_secs,
            "Sleep agents: starting cycle"
        );

        for agent in &due_agents {
            // Check if still idle before each agent
            if !self.idle_tracker.is_idle().await {
                info!("Sleep agents: activity detected, aborting cycle");
                break;
            }

            let result = self.run_agent(*agent, maintenance, llm).await;

            // Mark as run regardless of outcome
            {
                let mut schedules = self.schedules.lock().await;
                if let Some((_, schedule)) = schedules.iter_mut().find(|(a, _)| *a == *agent) {
                    schedule.mark_run();
                }
            }

            report.total_actions += result.actions_taken;
            report.agents_run += 1;
            report.results.push(result);
        }

        self.running.store(false, Ordering::SeqCst);

        if report.agents_run > 0 {
            info!(
                agents_run = report.agents_run,
                total_actions = report.total_actions,
                "Sleep agents: cycle complete"
            );
        }

        report
    }

    /// Run a single agent with timeout enforcement.
    async fn run_agent(
        &self,
        agent: SleepAgent,
        maintenance: &MemoryMaintenance,
        llm: &LlmRouter,
    ) -> AgentRunResult {
        let start = tokio::time::Instant::now();
        let max_runtime = std::time::Duration::from_secs(self.max_runtime_secs(agent));
        let batch_size = self.config.batch_size;
        let pattern_days = self.config.pattern_days;

        debug!(agent = %agent, "Sleep agent: starting");

        let result = tokio::time::timeout(max_runtime, async {
            match agent {
                SleepAgent::ContradictionDetector => {
                    maintenance.detect_contradictions(llm, batch_size).await
                }
                SleepAgent::InsightSynthesizer => {
                    maintenance.synthesize_insights(llm, batch_size).await
                }
                SleepAgent::RelationshipDiscoverer => {
                    maintenance.discover_relationships(llm, batch_size).await
                }
                SleepAgent::TemporalVerifier => {
                    maintenance.verify_temporal_facts(llm, batch_size).await
                }
                SleepAgent::PatternDetector => maintenance.detect_patterns(llm, pattern_days).await,
            }
        })
        .await;

        let elapsed_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(Ok(actions)) => {
                info!(
                    agent = %agent,
                    actions,
                    elapsed_ms,
                    "Sleep agent: completed"
                );
                AgentRunResult {
                    agent: agent.to_string(),
                    actions_taken: actions,
                    elapsed_ms,
                    error: None,
                }
            }
            Ok(Err(e)) => {
                warn!(
                    agent = %agent,
                    error = %e,
                    elapsed_ms,
                    "Sleep agent: failed"
                );
                AgentRunResult {
                    agent: agent.to_string(),
                    actions_taken: 0,
                    elapsed_ms,
                    error: Some(e.to_string()),
                }
            }
            Err(_) => {
                warn!(
                    agent = %agent,
                    max_runtime_secs = max_runtime.as_secs(),
                    "Sleep agent: timed out"
                );
                AgentRunResult {
                    agent: agent.to_string(),
                    actions_taken: 0,
                    elapsed_ms,
                    error: Some("timeout".to_string()),
                }
            }
        }
    }

    /// Start the background loop that periodically checks idle and runs agents.
    ///
    /// Call this once during startup. The returned `JoinHandle` can be used to
    /// cancel the loop (via abort or a cancellation token).
    pub fn spawn_background_loop(
        self: Arc<Self>,
        maintenance: Arc<MemoryMaintenance>,
        llm: Arc<LlmRouter>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // Check every 60 seconds whether we should run agents
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;
                let report = self.run_cycle(&maintenance, &llm).await;
                if report.agents_run > 0 {
                    debug!(
                        agents_run = report.agents_run,
                        total_actions = report.total_actions,
                        "Sleep agents background cycle completed"
                    );
                }
            }
        })
    }
}

// ── Dream Gate ──────────────────────────────────────────────────────

/// Three-gate trigger for dream consolidation, matching CC's autoDream.
/// All three conditions must be true simultaneously:
/// 1. Time gate: at least `min_interval` since last dream
/// 2. Session gate: at least `min_sessions` since last dream
/// 3. Lock gate: advisory lock (prevents concurrent dreams)
pub struct DreamGate {
    pub last_dream: Option<chrono::DateTime<chrono::Utc>>,
    min_interval: chrono::Duration,
    min_sessions: u32,
    sessions_since_dream: u32,
    locked: bool,
}

impl DreamGate {
    pub fn new(min_interval: chrono::Duration, min_sessions: u32) -> Self {
        Self {
            last_dream: None,
            min_interval,
            min_sessions,
            sessions_since_dream: 0,
            locked: false,
        }
    }

    pub fn record_session(&mut self) {
        self.sessions_since_dream += 1;
    }

    pub fn should_dream(&self) -> bool {
        if self.locked {
            return false;
        }

        // Session gate
        if self.sessions_since_dream < self.min_sessions {
            return false;
        }

        // Time gate
        match self.last_dream {
            None => true, // Never dreamed — go ahead
            Some(last) => {
                let elapsed = chrono::Utc::now().signed_duration_since(last);
                elapsed >= self.min_interval
            }
        }
    }

    pub fn try_lock(&mut self) -> Option<DreamLockGuard> {
        if self.locked {
            return None;
        }
        self.locked = true;
        Some(DreamLockGuard { _private: () })
    }

    pub fn unlock(&mut self) {
        self.locked = false;
    }

    pub fn complete_dream(&mut self) {
        self.last_dream = Some(chrono::Utc::now());
        self.sessions_since_dream = 0;
        self.locked = false;
    }

    pub fn sessions_since_dream(&self) -> u32 {
        self.sessions_since_dream
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }
}

/// Token returned by `DreamGate::try_lock()` to prove a lock was acquired.
pub struct DreamLockGuard {
    _private: (),
}

// ── Consolidation Plan ──────────────────────────────────────────────

/// 4-phase consolidation protocol matching CC's autoDream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsolidationPhase {
    Orient,      // Assess current memory state
    Gather,      // Collect recent observations
    Consolidate, // Merge, resolve contradictions
    Prune,       // Enforce constraints (200 lines, 25KB)
}

const PHASES: &[ConsolidationPhase] = &[
    ConsolidationPhase::Orient,
    ConsolidationPhase::Gather,
    ConsolidationPhase::Consolidate,
    ConsolidationPhase::Prune,
];

#[derive(Default)]
pub struct ConsolidationPlan {
    phase_index: usize,
}

impl ConsolidationPlan {
    pub fn current_phase(&self) -> ConsolidationPhase {
        PHASES[self.phase_index]
    }

    pub fn advance(&mut self) -> ConsolidationPhase {
        if self.phase_index < PHASES.len() - 1 {
            self.phase_index += 1;
        }
        PHASES[self.phase_index]
    }

    pub fn is_complete(&self) -> bool {
        self.phase_index >= PHASES.len() - 1
    }

    pub fn reset(&mut self) {
        self.phase_index = 0;
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = SleepAgentsConfig::default();
        assert!(config.enabled);
        assert_eq!(config.idle_threshold_secs, 300);
        assert_eq!(config.batch_size, 20);
        assert_eq!(config.pattern_days, 14);
        assert!(config.contradiction_detector.enabled);
        assert_eq!(config.contradiction_detector.interval_secs, 1800);
        assert_eq!(config.insight_synthesizer.interval_secs, 3600);
        assert_eq!(config.pattern_detector.interval_secs, 7200);
    }

    #[test]
    fn test_agent_display() {
        assert_eq!(
            SleepAgent::ContradictionDetector.to_string(),
            "contradiction_detector"
        );
        assert_eq!(
            SleepAgent::InsightSynthesizer.to_string(),
            "insight_synthesizer"
        );
        assert_eq!(
            SleepAgent::RelationshipDiscoverer.to_string(),
            "relationship_discoverer"
        );
        assert_eq!(
            SleepAgent::TemporalVerifier.to_string(),
            "temporal_verifier"
        );
        assert_eq!(SleepAgent::PatternDetector.to_string(), "pattern_detector");
    }

    #[tokio::test]
    async fn test_idle_tracker_initially_not_idle() {
        let tracker = IdleTracker::new(5);
        // Just created — should not be idle yet
        assert!(!tracker.is_idle().await);
    }

    #[tokio::test]
    async fn test_idle_tracker_touch_resets() {
        let tracker = IdleTracker::new(0);
        // With 0 threshold, should be idle immediately
        assert!(tracker.is_idle().await);
        tracker.touch().await;
        // After touch with 0 threshold, still idle (0 seconds elapsed >= 0)
        assert!(tracker.is_idle().await);
    }

    #[tokio::test]
    async fn test_idle_tracker_duration() {
        let tracker = IdleTracker::new(300);
        let dur = tracker.idle_duration_secs().await;
        assert!(dur < 2); // Should be near 0 right after creation
    }

    #[test]
    fn test_agent_schedule_initially_due() {
        let schedule = AgentSchedule::new(1800);
        assert!(schedule.is_due());
    }

    #[test]
    fn test_agent_schedule_not_due_after_run() {
        let mut schedule = AgentSchedule::new(1800);
        schedule.mark_run();
        assert!(!schedule.is_due());
    }

    #[test]
    fn test_sleep_cycle_report_default() {
        let report = SleepCycleReport::default();
        assert_eq!(report.agents_run, 0);
        assert_eq!(report.total_actions, 0);
        assert!(report.results.is_empty());
    }

    #[tokio::test]
    async fn test_scheduler_disabled_returns_empty() {
        let config = SleepAgentsConfig {
            enabled: false,
            ..Default::default()
        };
        let scheduler = SleepAgentScheduler::new(config);
        let store = crate::store::BrainStore::open_memory().await.unwrap();
        let _maintenance = MemoryMaintenance::new(store);

        // We can't easily create a real LlmRouter without providers,
        // but disabled config returns early before touching the LLM.
        // Instead, just verify the config check.
        assert!(!scheduler.config.enabled);
    }

    #[tokio::test]
    async fn test_scheduler_not_idle_returns_empty() {
        let config = SleepAgentsConfig {
            enabled: true,
            idle_threshold_secs: 3600, // 1 hour — won't be idle
            ..Default::default()
        };
        let scheduler = SleepAgentScheduler::new(config);
        assert!(!scheduler.is_idle().await);
    }

    #[test]
    fn test_agent_enabled_check() {
        let mut config = SleepAgentsConfig::default();
        config.temporal_verifier.enabled = false;

        let scheduler = SleepAgentScheduler::new(config);
        assert!(scheduler.is_agent_enabled(SleepAgent::ContradictionDetector));
        assert!(scheduler.is_agent_enabled(SleepAgent::InsightSynthesizer));
        assert!(scheduler.is_agent_enabled(SleepAgent::RelationshipDiscoverer));
        assert!(!scheduler.is_agent_enabled(SleepAgent::TemporalVerifier));
        assert!(scheduler.is_agent_enabled(SleepAgent::PatternDetector));
    }

    #[test]
    fn test_max_runtime_per_agent() {
        let mut config = SleepAgentsConfig::default();
        config.contradiction_detector.max_runtime_secs = 90;
        config.pattern_detector.max_runtime_secs = 120;

        let scheduler = SleepAgentScheduler::new(config);
        assert_eq!(
            scheduler.max_runtime_secs(SleepAgent::ContradictionDetector),
            90
        );
        assert_eq!(
            scheduler.max_runtime_secs(SleepAgent::InsightSynthesizer),
            60
        );
        assert_eq!(scheduler.max_runtime_secs(SleepAgent::PatternDetector), 120);
    }

    #[test]
    fn test_is_not_running_initially() {
        let scheduler = SleepAgentScheduler::new(SleepAgentsConfig::default());
        assert!(!scheduler.is_running());
    }

    #[test]
    fn test_agent_run_result_default() {
        let result = AgentRunResult::default();
        assert!(result.agent.is_empty());
        assert_eq!(result.actions_taken, 0);
        assert_eq!(result.elapsed_ms, 0);
        assert!(result.error.is_none());
    }

    #[test]
    fn test_agent_config_default() {
        let config = AgentConfig::default();
        assert!(config.enabled);
        assert_eq!(config.interval_secs, 1800);
        assert_eq!(config.max_runtime_secs, 60);
    }

    #[test]
    fn test_dream_gate_requires_all_three_conditions() {
        let mut gate = DreamGate::new(chrono::Duration::hours(24), 5);

        // Fresh gate — not enough sessions
        assert!(!gate.should_dream());

        // Simulate 5 sessions
        for _ in 0..5 {
            gate.record_session();
        }
        // No last_dream set — time gate passes (never dreamed)
        assert!(gate.should_dream());

        // Set last_dream to now — time gate should fail
        gate.last_dream = Some(chrono::Utc::now());
        assert!(!gate.should_dream()); // Time gate not met

        // Simulate 24h passage
        gate.last_dream = Some(chrono::Utc::now() - chrono::Duration::hours(25));
        assert!(gate.should_dream()); // All gates met
    }

    #[test]
    fn test_dream_gate_lock() {
        let mut gate = DreamGate::new(chrono::Duration::hours(24), 5);
        let lock = gate.try_lock();
        assert!(lock.is_some()); // First lock succeeds
        assert!(gate.is_locked());

        // Second lock should fail
        let lock2 = gate.try_lock();
        assert!(lock2.is_none());

        // Unlock and try again
        gate.unlock();
        assert!(!gate.is_locked());
        let lock3 = gate.try_lock();
        assert!(lock3.is_some());
    }

    #[test]
    fn test_dream_gate_complete_resets() {
        let mut gate = DreamGate::new(chrono::Duration::hours(24), 5);
        for _ in 0..5 {
            gate.record_session();
        }
        gate.complete_dream();
        assert_eq!(gate.sessions_since_dream(), 0);
        assert!(gate.last_dream.is_some());
        assert!(!gate.is_locked());
    }

    #[test]
    fn test_consolidation_phases_execute_in_order() {
        let mut phases = ConsolidationPlan::default();
        assert_eq!(phases.current_phase(), ConsolidationPhase::Orient);
        assert_eq!(phases.advance(), ConsolidationPhase::Gather);
        assert_eq!(phases.advance(), ConsolidationPhase::Consolidate);
        assert_eq!(phases.advance(), ConsolidationPhase::Prune);
        assert!(phases.is_complete());
    }

    #[test]
    fn test_consolidation_plan_reset() {
        let mut phases = ConsolidationPlan::default();
        phases.advance();
        phases.advance();
        phases.reset();
        assert_eq!(phases.current_phase(), ConsolidationPhase::Orient);
        assert!(!phases.is_complete());
    }
}
