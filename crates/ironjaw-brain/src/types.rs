use serde::{Deserialize, Serialize};

/// Trust level for a memory — determines authority in conflict resolution and search ranking.
///
/// Higher values indicate more reliable sources. When contradictory memories are
/// detected, the one with the higher trust level wins. Equal trust → newer wins.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(i32)]
pub enum TrustLevel {
    /// Inferred by the model without explicit verification
    #[default]
    ModelInferred = 1,
    /// Retrieved from an external source (web search, API)
    Retrieved = 2,
    /// Explicitly stated by the user in conversation
    UserStated = 3,
    /// Verified by a tool execution (file read, command output)
    ToolVerified = 4,
    /// Hardcoded system policy (identity, core config)
    SystemPolicy = 5,
}

impl TrustLevel {
    /// Normalized score in [0.0, 1.0] for use in composite ranking.
    pub fn score(&self) -> f32 {
        (*self as i32) as f32 / 5.0
    }
}

impl std::fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TrustLevel::ModelInferred => "model_inferred",
            TrustLevel::Retrieved => "retrieved",
            TrustLevel::UserStated => "user_stated",
            TrustLevel::ToolVerified => "tool_verified",
            TrustLevel::SystemPolicy => "system_policy",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for TrustLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "model_inferred" => Ok(TrustLevel::ModelInferred),
            "retrieved" => Ok(TrustLevel::Retrieved),
            "user_stated" => Ok(TrustLevel::UserStated),
            "tool_verified" => Ok(TrustLevel::ToolVerified),
            "system_policy" => Ok(TrustLevel::SystemPolicy),
            _ => Err(format!("unknown trust level: {s}")),
        }
    }
}

impl From<i32> for TrustLevel {
    fn from(v: i32) -> Self {
        match v {
            5 => TrustLevel::SystemPolicy,
            4 => TrustLevel::ToolVerified,
            3 => TrustLevel::UserStated,
            2 => TrustLevel::Retrieved,
            _ => TrustLevel::ModelInferred,
        }
    }
}

/// A memory extracted by an automated pipeline, awaiting human approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingMemory {
    pub id: i64,
    pub content: String,
    pub source: String,
    pub memory_type: String,
    pub tags: Option<String>,
    pub importance: f64,
    pub confidence: f64,
    pub evidence_session_id: Option<String>,
    pub created_at: String,
}

/// Input for creating a pending memory.
#[derive(Debug, Clone)]
pub struct NewPendingMemory {
    pub content: String,
    pub source: String,
    pub memory_type: String,
    pub tags: Option<String>,
    pub importance: f64,
    pub confidence: f64,
    pub evidence_session_id: Option<String>,
    pub embedding: Option<Vec<f32>>,
}

/// Structured reflection metadata for Correction memories.
///
/// Captures the full context of a correction: what happened, why, and what to
/// do differently. The confidence score is used to boost high-confidence
/// corrections in search ranking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReflectionMeta {
    /// What happened — the trigger event or mistake
    pub trigger: String,
    /// Why it happened — root cause analysis
    pub analysis: String,
    /// What to do differently — the actionable lesson
    pub lesson: String,
    /// Confidence in this reflection (0.0–1.0)
    pub confidence: f32,
}

/// Where a memory originated from — used for deduplication and provenance tracking.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    /// Created directly via brain.remember
    #[default]
    Brain,
    /// Imported from cc-chronicle session transcripts
    CcChronicle,
    /// Imported from markdown brain files
    Markdown,
    /// Learned from a CC session conversation
    CcSession,
    /// Received via cc-bridge
    CcBridge,
}

impl std::fmt::Display for SourceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SourceType::Brain => "brain",
            SourceType::CcChronicle => "cc_chronicle",
            SourceType::Markdown => "markdown",
            SourceType::CcSession => "cc_session",
            SourceType::CcBridge => "cc_bridge",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for SourceType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "brain" => Ok(SourceType::Brain),
            "cc_chronicle" => Ok(SourceType::CcChronicle),
            "markdown" => Ok(SourceType::Markdown),
            "cc_session" => Ok(SourceType::CcSession),
            "cc_bridge" => Ok(SourceType::CcBridge),
            _ => Err(format!("unknown source type: {s}")),
        }
    }
}

/// Classification of a memory for filtering and importance scoring
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    Fact,
    Preference,
    Decision,
    Identity,
    Event,
    Observation,
    Goal,
    Todo,
    Correction,
    Command,
    Context,
    #[serde(rename = "session_summary")]
    SessionSummary,
    /// I19: Step-by-step procedures (how-to knowledge)
    Procedure,
}

impl MemoryType {
    /// Base importance score for each memory type
    pub fn base_importance(&self) -> f64 {
        match self {
            MemoryType::Identity => 0.9,
            MemoryType::Decision => 0.8,
            MemoryType::Goal => 0.8,
            MemoryType::Procedure => 0.8,
            MemoryType::Correction => 0.75,
            MemoryType::Preference => 0.7,
            MemoryType::Command => 0.6,
            MemoryType::Todo => 0.6,
            MemoryType::Fact => 0.5,
            MemoryType::Event => 0.5,
            MemoryType::SessionSummary => 0.65,
            MemoryType::Context => 0.45,
            MemoryType::Observation => 0.4,
        }
    }
}

impl std::fmt::Display for MemoryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            MemoryType::Fact => "fact",
            MemoryType::Preference => "preference",
            MemoryType::Decision => "decision",
            MemoryType::Identity => "identity",
            MemoryType::Event => "event",
            MemoryType::Observation => "observation",
            MemoryType::Goal => "goal",
            MemoryType::Todo => "todo",
            MemoryType::Correction => "correction",
            MemoryType::Command => "command",
            MemoryType::Context => "context",
            MemoryType::SessionSummary => "session_summary",
            MemoryType::Procedure => "procedure",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for MemoryType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fact" => Ok(MemoryType::Fact),
            "preference" => Ok(MemoryType::Preference),
            "decision" => Ok(MemoryType::Decision),
            "identity" => Ok(MemoryType::Identity),
            "event" => Ok(MemoryType::Event),
            "observation" => Ok(MemoryType::Observation),
            "goal" => Ok(MemoryType::Goal),
            "todo" => Ok(MemoryType::Todo),
            "correction" => Ok(MemoryType::Correction),
            "command" => Ok(MemoryType::Command),
            "context" => Ok(MemoryType::Context),
            "session_summary" => Ok(MemoryType::SessionSummary),
            "procedure" => Ok(MemoryType::Procedure),
            _ => Err(format!("unknown memory type: {s}")),
        }
    }
}

/// Structured metadata for Procedure memories (I19).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcedureMeta {
    /// When this procedure should be triggered
    pub trigger: String,
    /// Ordered list of steps to execute
    pub steps: Vec<String>,
    /// Prerequisites that must be met before execution
    pub prerequisites: Vec<String>,
    /// Expected output format
    pub output_format: Option<String>,
}

/// A stored memory with DB-generated fields
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: i64,
    pub content: String,
    pub source: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub tags: Option<String>,
    pub created_at: String,
    pub accessed_at: String,
    pub access_count: i64,
    pub memory_type: String,
    pub importance: f64,
    pub related_to: Option<String>,
    pub strength: f64,
    pub supersedes: Option<i64>,
    pub superseded_by: Option<i64>,
    pub content_hash: Option<String>,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub source_type: String,
    /// Structured reflection data for Correction memories
    pub reflection: Option<ReflectionMeta>,
    /// Trust level for authority ranking (default: ModelInferred)
    pub trust_level: TrustLevel,
    /// Session ID that established this memory (evidence pointer)
    pub evidence_session_id: Option<String>,
    /// Message ID that established this memory (evidence pointer)
    pub evidence_message_id: Option<String>,
    /// I20: Hierarchical scope (slash-delimited, e.g. `/project/ironjaw/decisions`)
    pub scope: Option<String>,
    /// I19: Structured procedure metadata for Procedure memories
    pub procedure: Option<ProcedureMeta>,
    /// Quality score (0.0–1.0) computed at store time by quality gates
    pub quality_score: f64,
    /// Quality flag — human-readable reason if quality is low, None if OK
    pub quality_flag: Option<String>,
}

/// A new memory to be inserted
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMemory {
    pub content: String,
    pub source: Option<String>,
    pub tags: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub memory_type: Option<String>,
    pub importance: Option<f64>,
    pub strength: Option<f64>,
    pub supersedes: Option<i64>,
    pub content_hash: Option<String>,
    pub valid_from: Option<String>,
    /// Optional expiry timestamp (ISO-8601).  When set the memory is treated
    /// as stale after this instant.  Used by tool-memory writeback (Phase 11)
    /// to mark live-state facts with a `now + volatility` expiry.
    pub valid_to: Option<String>,
    pub source_type: Option<String>,
    /// Structured reflection data (for Correction memories)
    pub reflection: Option<ReflectionMeta>,
    /// Trust level (default: ModelInferred if None)
    pub trust_level: Option<TrustLevel>,
    /// Session ID evidence pointer
    pub evidence_session_id: Option<String>,
    /// Message ID evidence pointer
    pub evidence_message_id: Option<String>,
    /// I20: Hierarchical scope (slash-delimited)
    pub scope: Option<String>,
    /// I19: Structured procedure metadata
    pub procedure: Option<ProcedureMeta>,
    /// Quality score (0.0–1.0, computed by quality gates; None = auto-compute)
    pub quality_score: Option<f64>,
    /// Quality flag (reason for low quality; None = auto-compute)
    pub quality_flag: Option<String>,
}

/// A recorded action in a workflow sequence (I15).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowAction {
    /// RPC method or tool name
    pub method: String,
    /// Key parameter fingerprint (for sequence matching)
    pub param_fingerprint: Option<String>,
}

/// A learned workflow (I15): a repeating sequence of user actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearnedWorkflow {
    pub id: i64,
    pub name: String,
    pub actions: Vec<WorkflowAction>,
    pub occurrence_count: i64,
    pub is_candidate: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// New workflow to insert (I15).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewWorkflow {
    pub name: String,
    pub actions: Vec<WorkflowAction>,
}

/// Identity key-value entry (Layer 1 — always-loaded content)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub key: String,
    pub content: String,
    pub updated_at: String,
}

/// Comprehensive brain health metrics snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainHealthMetrics {
    /// Memory counts: active, retired (deleted), total
    pub active_memories: i64,
    pub retired_memories: i64,
    pub total_memories: i64,

    /// Count per memory_type (e.g. "fact" -> 120)
    pub type_distribution: Vec<TypeCount>,

    /// Count per trust_level
    pub trust_distribution: Vec<TrustCount>,

    /// Age buckets: last 24h, last week, last month, older
    pub age_distribution: AgeBuckets,

    /// Count per source_type
    pub source_distribution: Vec<SourceCount>,

    /// Embedding coverage
    pub with_embedding: i64,
    pub without_embedding: i64,

    /// Average importance and access_count across active memories
    pub avg_importance: f64,
    pub avg_access_count: f64,

    /// Top 10 most accessed memories (id, content preview, access_count)
    pub top_accessed: Vec<TopMemory>,
}

/// Count for a memory type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeCount {
    pub memory_type: String,
    pub count: i64,
}

/// Count for a trust level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustCount {
    pub trust_level: String,
    pub count: i64,
}

/// Count for a source type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceCount {
    pub source_type: String,
    pub count: i64,
}

/// Age distribution buckets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgeBuckets {
    pub last_24h: i64,
    pub last_week: i64,
    pub last_month: i64,
    pub older: i64,
}

/// A frequently accessed memory summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopMemory {
    pub id: i64,
    pub content_preview: String,
    pub access_count: i64,
    pub memory_type: String,
}

/// A named reference document loadable/unloadable from LLM context on demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextVariable {
    pub id: i64,
    pub name: String,
    pub content: String,
    pub token_count: i64,
    pub source_path: Option<String>,
    pub loaded: bool,
    pub created_at: String,
    pub updated_at: String,
}
