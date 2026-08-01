pub mod anthropic;
pub mod cache;
pub mod complexity;
pub mod decorators;
pub mod json_repair;
pub mod llama_server;
pub mod llama_status;
pub mod model_registry;
pub mod nudge;
pub mod openai;
pub mod perf_tracker;
pub mod prefetch;
pub mod prompt_cache;
pub mod provider;
pub mod router;
pub mod routing_mode;
pub mod stream_parser;
pub mod tokenizer;
pub mod types;
pub mod usage;
pub mod verification;

/// Absolute path to Git Bash. Bare `Command::new("bash")` resolves to
/// System32's WSL stub under the service PATH (no distro installed), whose
/// UTF-16 stderr logs as empty — always shell scripts through this instead.
pub const GIT_BASH: &str = r"C:\Program Files\Git\bin\bash.exe";

pub use anthropic::ApiKeySource;
pub use complexity::{ComplexityLevel, ComplexityScorer};
pub use decorators::{ErrorClass, FailoverProvider, RetryProvider};
pub use llama_status::{
    LlamaHealth, LlamaServerState, LlamaStatusMonitor, ModelChangeEvent, ModelChangeSource,
};
pub use model_registry::{CloudModel, CloudModelCatalog, ModelEntry, ModelRegistry};
pub use nudge::{wrong_tool_nudge, NudgeDecision, ToolNudger, WEB_SEARCH_TOOLS};
pub use perf_tracker::{ModelPerf, PerfTracker};
pub use provider::Provider;
pub use router::{LlmRouter, RoutingOverrides};
pub use routing_mode::{PlanOutput, RoutingMode, SkillRoutingConfig};
pub use stream_parser::{StreamParserError, ToolCallAccumulator};
pub use types::*;
pub use usage::{Budget, UsageError, UsageEvent, UsageSummary, UsageTracker};
pub use verification::{VerificationConfig, VerificationResult};
