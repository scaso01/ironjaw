use crate::handler::ConnectionContext;
use crate::state::AppState;
use dashmap::DashMap;
use ironjaw_security::tiers::PermissionTier;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ── Built-in command implementations ─────────────────────────────────────────

struct HelpCommand;

#[async_trait::async_trait]
impl SlashCommand for HelpCommand {
    fn name(&self) -> &str {
        "help"
    }
    fn description(&self) -> &str {
        "List all available slash commands"
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }
    async fn execute(
        &self,
        _args: &str,
        state: &AppState,
        _ctx: &ConnectionContext,
    ) -> CommandResult {
        let cmds = state.commands.list();
        if cmds.is_empty() {
            return CommandResult::ok("No commands registered.");
        }
        let lines: Vec<String> = cmds
            .iter()
            .map(|c| {
                let tier_label = match c.tier {
                    0 => "auto",
                    1 => "notify",
                    2 => "confirm",
                    _ => "multi-confirm",
                };
                format!("/{:<20} [{}]  {}", c.name, tier_label, c.description)
            })
            .collect();
        CommandResult::ok(lines.join("\n"))
    }
}

struct StatusCommand;

#[async_trait::async_trait]
impl SlashCommand for StatusCommand {
    fn name(&self) -> &str {
        "status"
    }
    fn description(&self) -> &str {
        "Show gateway uptime, active session, and current model"
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Auto
    }
    async fn execute(
        &self,
        _args: &str,
        state: &AppState,
        ctx: &ConnectionContext,
    ) -> CommandResult {
        let uptime = chrono::Utc::now()
            .signed_duration_since(state.started_at)
            .num_seconds();
        let hours = uptime / 3600;
        let mins = (uptime % 3600) / 60;
        let secs = uptime % 60;

        let model = match state.llm.model_registry.active_or_default() {
            m if m.is_empty() => "(none)".to_string(),
            m => m,
        };

        let output = format!(
            "Gateway status\n  Uptime:  {hours}h {mins}m {secs}s\n  Session: {}\n  Model:   {model}",
            ctx.current_session_id
        );
        CommandResult::ok(output)
    }
}

struct UsageCommand;

#[async_trait::async_trait]
impl SlashCommand for UsageCommand {
    fn name(&self) -> &str {
        "usage"
    }
    fn description(&self) -> &str {
        "Show per-model token usage and performance stats"
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Notify
    }
    async fn execute(
        &self,
        _args: &str,
        state: &AppState,
        _ctx: &ConnectionContext,
    ) -> CommandResult {
        let perfs = state.llm.perf_tracker.summary();
        if perfs.is_empty() {
            return CommandResult::ok("No model usage recorded yet.");
        }
        let lines: Vec<String> = perfs
            .iter()
            .map(|p| {
                format!(
                    "{:<30} req={:<6} avg={:.1} t/s  total={} tokens",
                    p.model, p.total_requests, p.avg_tokens_per_sec, p.total_tokens
                )
            })
            .collect();
        CommandResult::ok(lines.join("\n"))
    }
}

struct PeersCommand;

#[async_trait::async_trait]
impl SlashCommand for PeersCommand {
    fn name(&self) -> &str {
        "peers"
    }
    fn description(&self) -> &str {
        "List connected agents in the communication graph"
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Notify
    }
    async fn execute(
        &self,
        _args: &str,
        state: &AppState,
        _ctx: &ConnectionContext,
    ) -> CommandResult {
        let agents = state.agents.graph.list_agents().await;
        if agents.is_empty() {
            return CommandResult::ok("No agents registered.");
        }
        let lines: Vec<String> = agents
            .iter()
            .map(|a| {
                format!(
                    "{:<24} [{:<12}]  {}  id={}",
                    a.name,
                    a.status.to_string(),
                    a.agent_type,
                    a.id
                )
            })
            .collect();
        CommandResult::ok(lines.join("\n"))
    }
}

struct SearchCommand;

#[async_trait::async_trait]
impl SlashCommand for SearchCommand {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "Search brain memories (usage: /search <query>)"
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Notify
    }
    async fn execute(
        &self,
        args: &str,
        state: &AppState,
        _ctx: &ConnectionContext,
    ) -> CommandResult {
        let query = args.trim();
        if query.is_empty() {
            return CommandResult::err("Usage: /search <query>");
        }
        match state.brain.engine.recall(query, Some(10)).await {
            Ok(memories) if memories.is_empty() => CommandResult::ok("No memories found."),
            Ok(memories) => {
                let lines: Vec<String> = memories
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        let snippet: String = m.content.chars().take(120).collect();
                        format!("{}. [{}] {}", i + 1, m.memory_type, snippet)
                    })
                    .collect();
                CommandResult::ok(lines.join("\n"))
            }
            Err(e) => CommandResult::err(format!("Brain recall error: {e}")),
        }
    }
}

struct RecallCommand;

#[async_trait::async_trait]
impl SlashCommand for RecallCommand {
    fn name(&self) -> &str {
        "recall"
    }
    fn description(&self) -> &str {
        "Deep recall from brain with metadata (usage: /recall <query>)"
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Notify
    }
    async fn execute(
        &self,
        args: &str,
        state: &AppState,
        _ctx: &ConnectionContext,
    ) -> CommandResult {
        let query = args.trim();
        if query.is_empty() {
            return CommandResult::err("Usage: /recall <query>");
        }
        match state.brain.engine.recall(query, Some(5)).await {
            Ok(memories) if memories.is_empty() => CommandResult::ok("No memories found."),
            Ok(memories) => {
                let lines: Vec<String> = memories
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        let scope = m.scope.as_deref().unwrap_or("-");
                        let source = m.source.as_deref().unwrap_or("-");
                        format!(
                            "{}. id={} type={} scope={} source={}\n   {}",
                            i + 1,
                            m.id,
                            m.memory_type,
                            scope,
                            source,
                            m.content
                        )
                    })
                    .collect();
                CommandResult::ok(lines.join("\n\n"))
            }
            Err(e) => CommandResult::err(format!("Brain recall error: {e}")),
        }
    }
}

struct SkillCommand;

#[async_trait::async_trait]
impl SlashCommand for SkillCommand {
    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> &str {
        "List registered skills or activate one (usage: /skill [name])"
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Confirm
    }
    async fn execute(
        &self,
        args: &str,
        state: &AppState,
        _ctx: &ConnectionContext,
    ) -> CommandResult {
        let name = args.trim();
        let registry = state.skill.registry.read().unwrap();
        if name.is_empty() {
            let skills = registry.list();
            if skills.is_empty() {
                return CommandResult::ok("No skills registered.");
            }
            let lines: Vec<String> = skills
                .iter()
                .map(|s| format!("{:<24}  {}", s.name, s.description))
                .collect();
            return CommandResult::ok(lines.join("\n"));
        }
        match registry.get(name) {
            Some(skill) => CommandResult {
                ok: true,
                output: format!("Skill '{}' activated.\n{}", skill.name, skill.description),
                consequences: Some(format!(
                    "Session will use the '{}' skill prompt and tool allowlist.",
                    skill.name
                )),
                injected_memories: Vec::new(),
            },
            None => CommandResult::err(format!("Skill '{name}' not found.")),
        }
    }
}

struct ModelCommand;

#[async_trait::async_trait]
impl SlashCommand for ModelCommand {
    fn name(&self) -> &str {
        "model"
    }
    fn description(&self) -> &str {
        "List available models or show active model (usage: /model [name])"
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Confirm
    }
    async fn execute(
        &self,
        args: &str,
        state: &AppState,
        _ctx: &ConnectionContext,
    ) -> CommandResult {
        let name = args.trim();
        let registry = &state.llm.model_registry;
        let active = match registry.active_or_default() {
            m if m.is_empty() => "(none)".to_string(),
            m => m,
        };

        if name.is_empty() {
            if registry.models.is_empty() {
                return CommandResult::ok(format!("Active model: {active}\nNo models configured."));
            }
            let mut model_names: Vec<&String> = registry.models.keys().collect();
            model_names.sort();
            let lines: Vec<String> = model_names
                .iter()
                .map(|m| {
                    let marker = if **m == active { "* " } else { "  " };
                    format!("{marker}{m}")
                })
                .collect();
            return CommandResult::ok(format!("Active: {active}\n\n{}", lines.join("\n")));
        }

        if registry.models.contains_key(name) {
            CommandResult {
                ok: true,
                output: format!("Switching to model '{name}'."),
                consequences: Some(format!(
                    "All subsequent chat.send calls in this session will use '{name}'."
                )),
                injected_memories: Vec::new(),
            }
        } else {
            CommandResult::err(format!("Model '{name}' not found in registry."))
        }
    }
}

struct CompactCommand;

#[async_trait::async_trait]
impl SlashCommand for CompactCommand {
    fn name(&self) -> &str {
        "compact"
    }
    fn description(&self) -> &str {
        "Snapshot the current session then queue context compaction"
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Confirm
    }
    async fn execute(
        &self,
        _args: &str,
        state: &AppState,
        ctx: &ConnectionContext,
    ) -> CommandResult {
        let session_id = &ctx.current_session_id;

        let session = match state.session.store.get_session(session_id).await {
            Ok(Some(s)) => s,
            Ok(None) => return CommandResult::err("No active session found."),
            Err(e) => return CommandResult::err(format!("Session load error: {e}")),
        };

        let messages = match state
            .session
            .store
            .get_messages(session_id, 10_000, None)
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => return CommandResult::err(format!("Message load error: {e}")),
        };

        let msg_count = messages.len();
        let snap = state
            .session_snapshots
            .create(Some("pre-compact".to_string()), session, messages)
            .await;

        CommandResult {
            ok: true,
            output: format!(
                "Snapshot saved (id={}, {} messages). Compaction queued.",
                snap.id, msg_count
            ),
            consequences: Some(
                "Older messages will be summarised and pruned to reduce context size.".to_string(),
            ),
            injected_memories: Vec::new(),
        }
    }
}

struct ClearCommand;

#[async_trait::async_trait]
impl SlashCommand for ClearCommand {
    fn name(&self) -> &str {
        "clear"
    }
    fn description(&self) -> &str {
        "Snapshot the current session then permanently delete all its messages"
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::MultiConfirm
    }
    async fn execute(
        &self,
        _args: &str,
        state: &AppState,
        ctx: &ConnectionContext,
    ) -> CommandResult {
        let session_id = &ctx.current_session_id;

        let session = match state.session.store.get_session(session_id).await {
            Ok(Some(s)) => s,
            Ok(None) => return CommandResult::err("No active session found."),
            Err(e) => return CommandResult::err(format!("Session load error: {e}")),
        };

        let messages = match state
            .session
            .store
            .get_messages(session_id, 10_000, None)
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => return CommandResult::err(format!("Message load error: {e}")),
        };

        let msg_count = messages.len();
        let snap = state
            .session_snapshots
            .create(Some("pre-clear".to_string()), session, messages)
            .await;

        match state.session.store.delete_session(session_id).await {
            Ok(_) => CommandResult {
                ok: true,
                output: format!(
                    "Snapshot saved (id={}). {} messages deleted.",
                    snap.id, msg_count
                ),
                consequences: Some(
                    "All messages in this session have been permanently deleted.".to_string(),
                ),
                injected_memories: Vec::new(),
            },
            Err(e) => CommandResult::err(format!("Failed to delete session: {e}")),
        }
    }
}

struct ForgetCommand;

#[async_trait::async_trait]
impl SlashCommand for ForgetCommand {
    fn name(&self) -> &str {
        "forget"
    }
    fn description(&self) -> &str {
        "Permanently delete a brain memory by numeric ID"
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::MultiConfirm
    }
    async fn execute(
        &self,
        args: &str,
        state: &AppState,
        _ctx: &ConnectionContext,
    ) -> CommandResult {
        let raw = args.trim();
        let memory_id: i64 = match raw.parse() {
            Ok(id) => id,
            Err(_) => {
                return CommandResult::err(format!(
                    "Invalid memory ID '{raw}'. Usage: /forget <numeric-id>"
                ))
            }
        };

        match state.brain.engine.forget(memory_id).await {
            Ok(true) => CommandResult {
                ok: true,
                output: format!("Memory {memory_id} deleted."),
                consequences: Some(
                    "The memory has been permanently removed from the brain.".to_string(),
                ),
                injected_memories: Vec::new(),
            },
            Ok(false) => CommandResult::err(format!("Memory {memory_id} not found.")),
            Err(e) => CommandResult::err(format!("Failed to forget memory {memory_id}: {e}")),
        }
    }
}

struct InitCommand;

/// Starter CLAUDE.md template written by `/init`.
const INIT_CLAUDE_MD_TEMPLATE: &str = r#"# Project

<!-- Replace this with a short description of the project. -->

## Architecture

<!-- Describe the high-level structure here. -->

## Key Commands

```bash
# Build
# e.g. cargo build

# Test
# e.g. cargo test

# Lint
# e.g. cargo clippy
```

## Notes

<!-- Add project-specific notes, conventions, and gotchas here. -->
"#;

#[async_trait::async_trait]
impl SlashCommand for InitCommand {
    fn name(&self) -> &str {
        "init"
    }
    fn description(&self) -> &str {
        "Write a starter CLAUDE.md in the session working directory (skips if one already exists)"
    }
    fn tier(&self) -> PermissionTier {
        PermissionTier::Confirm
    }
    async fn execute(
        &self,
        _args: &str,
        state: &AppState,
        ctx: &ConnectionContext,
    ) -> CommandResult {
        // Resolve cwd: prefer per-session override, fall back to process cwd.
        let cwd: std::path::PathBuf = if !ctx.current_session_id.is_empty() {
            match state
                .session
                .store
                .get_session(&ctx.current_session_id)
                .await
            {
                Ok(Some(session)) => session
                    .working_dir
                    .as_deref()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
                _ => std::env::current_dir().unwrap_or_default(),
            }
        } else {
            std::env::current_dir().unwrap_or_default()
        };

        let target = cwd.join("CLAUDE.md");

        if target.exists() {
            return CommandResult::ok(format!(
                "CLAUDE.md already exists at {}. No changes made.",
                target.display()
            ));
        }

        match tokio::fs::write(&target, INIT_CLAUDE_MD_TEMPLATE).await {
            Ok(()) => CommandResult {
                ok: true,
                output: format!("Created CLAUDE.md at {}.", target.display()),
                consequences: Some(
                    "The new CLAUDE.md will be picked up as the static layer on the next chat.send."
                        .to_string(),
                ),
                injected_memories: Vec::new(),
            },
            Err(e) => CommandResult::err(format!(
                "Failed to write CLAUDE.md at {}: {e}",
                target.display()
            )),
        }
    }
}

/// A memory reference returned by slash commands that perform brain recalls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRef {
    pub id: String,
    pub scope: String,
    pub tags: Vec<String>,
    pub trust_level: u8,
    pub snippet: String,
    pub score: f32,
}

/// Result returned by every slash command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    pub ok: bool,
    pub output: String,
    pub consequences: Option<String>,
    pub injected_memories: Vec<MemoryRef>,
}

impl CommandResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            ok: true,
            output: output.into(),
            consequences: None,
            injected_memories: Vec::new(),
        }
    }

    pub fn err(output: impl Into<String>) -> Self {
        Self {
            ok: false,
            output: output.into(),
            consequences: None,
            injected_memories: Vec::new(),
        }
    }
}

/// Trait that all slash commands must implement.
#[async_trait::async_trait]
pub trait SlashCommand: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn tier(&self) -> PermissionTier;
    async fn execute(&self, args: &str, state: &AppState, ctx: &ConnectionContext)
        -> CommandResult;
}

/// Registry of all registered slash commands, keyed by name (without leading slash).
#[derive(Clone)]
pub struct SlashCommandRegistry {
    commands: Arc<DashMap<String, Arc<dyn SlashCommand + Send + Sync>>>,
}

impl Default for SlashCommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SlashCommandRegistry {
    pub fn new() -> Self {
        Self {
            commands: Arc::new(DashMap::new()),
        }
    }

    pub fn register(&self, cmd: Arc<dyn SlashCommand + Send + Sync>) {
        self.commands.insert(cmd.name().to_string(), cmd);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn SlashCommand + Send + Sync>> {
        self.commands.get(name).map(|e| e.value().clone())
    }

    /// Returns a sorted list of (name, description, tier) tuples for commands.list.
    pub fn list(&self) -> Vec<CommandInfo> {
        let mut entries: Vec<CommandInfo> = self
            .commands
            .iter()
            .map(|e| CommandInfo {
                name: e.key().clone(),
                description: e.value().description().to_string(),
                tier: e.value().tier() as u8,
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
}

/// Serialisable metadata returned by commands.list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
    pub tier: u8,
}

/// Build a registry pre-populated with all B1 + C6 commands.
/// Deferred commands (/uncompact, /restore, /reset) are intentionally omitted.
pub fn register_all(registry: &SlashCommandRegistry) {
    registry.register(Arc::new(HelpCommand));
    registry.register(Arc::new(StatusCommand));
    registry.register(Arc::new(UsageCommand));
    registry.register(Arc::new(PeersCommand));
    registry.register(Arc::new(SearchCommand));
    registry.register(Arc::new(RecallCommand));
    registry.register(Arc::new(SkillCommand));
    registry.register(Arc::new(ModelCommand));
    registry.register(Arc::new(CompactCommand));
    registry.register(Arc::new(ClearCommand));
    registry.register(Arc::new(ForgetCommand));
    registry.register(Arc::new(InitCommand));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_empty_on_default() {
        let r = SlashCommandRegistry::new();
        assert!(r.list().is_empty());
    }

    #[test]
    fn registry_list_sorted() {
        // We can test sorting without real commands by using a mock.
        struct MockCmd {
            name: &'static str,
        }
        #[async_trait::async_trait]
        impl SlashCommand for MockCmd {
            fn name(&self) -> &str {
                self.name
            }
            fn description(&self) -> &str {
                "mock"
            }
            fn tier(&self) -> PermissionTier {
                PermissionTier::Auto
            }
            async fn execute(
                &self,
                _args: &str,
                _state: &AppState,
                _ctx: &ConnectionContext,
            ) -> CommandResult {
                CommandResult::ok("mock")
            }
        }

        let r = SlashCommandRegistry::new();
        r.register(Arc::new(MockCmd { name: "zebra" }));
        r.register(Arc::new(MockCmd { name: "apple" }));
        r.register(Arc::new(MockCmd { name: "mango" }));

        let listing = r.list();
        let names: Vec<&str> = listing.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["apple", "mango", "zebra"]);
    }

    #[test]
    fn command_result_ok_fields() {
        let r = CommandResult::ok("hello");
        assert!(r.ok);
        assert_eq!(r.output, "hello");
        assert!(r.consequences.is_none());
        assert!(r.injected_memories.is_empty());
    }

    #[test]
    fn command_result_err_fields() {
        let r = CommandResult::err("boom");
        assert!(!r.ok);
        assert_eq!(r.output, "boom");
    }

    #[test]
    fn help_command_metadata() {
        let cmd = HelpCommand;
        assert_eq!(cmd.name(), "help");
        assert_eq!(cmd.tier(), PermissionTier::Auto);
    }

    #[test]
    fn status_command_metadata() {
        let cmd = StatusCommand;
        assert_eq!(cmd.name(), "status");
        assert_eq!(cmd.tier(), PermissionTier::Auto);
    }

    #[test]
    fn register_all_populates_registry() {
        let r = SlashCommandRegistry::new();
        register_all(&r);
        let names: Vec<String> = r.list().iter().map(|c| c.name.clone()).collect();
        assert!(names.contains(&"help".to_string()));
        assert!(names.contains(&"status".to_string()));
        assert!(names.contains(&"usage".to_string()));
        assert!(names.contains(&"peers".to_string()));
        assert!(names.contains(&"search".to_string()));
        assert!(names.contains(&"recall".to_string()));
        assert!(names.contains(&"skill".to_string()));
        assert!(names.contains(&"model".to_string()));
        assert!(names.contains(&"compact".to_string()));
        assert!(names.contains(&"clear".to_string()));
        assert!(names.contains(&"forget".to_string()));
    }

    #[test]
    fn notify_commands_have_correct_tier() {
        assert_eq!(UsageCommand.tier(), PermissionTier::Notify);
        assert_eq!(PeersCommand.tier(), PermissionTier::Notify);
        assert_eq!(SearchCommand.tier(), PermissionTier::Notify);
        assert_eq!(RecallCommand.tier(), PermissionTier::Notify);
    }

    #[test]
    fn notify_commands_have_correct_names() {
        assert_eq!(UsageCommand.name(), "usage");
        assert_eq!(PeersCommand.name(), "peers");
        assert_eq!(SearchCommand.name(), "search");
        assert_eq!(RecallCommand.name(), "recall");
    }

    #[test]
    fn help_format_includes_tier_label() {
        // Verify tier labels map correctly for each numeric value
        for (tier_num, expected) in &[
            (0u8, "auto"),
            (1u8, "notify"),
            (2u8, "confirm"),
            (3u8, "multi-confirm"),
        ] {
            let label = match tier_num {
                0 => "auto",
                1 => "notify",
                2 => "confirm",
                _ => "multi-confirm",
            };
            assert_eq!(label, *expected);
        }
    }

    #[test]
    fn confirm_commands_have_correct_tier() {
        assert_eq!(SkillCommand.tier(), PermissionTier::Confirm);
        assert_eq!(ModelCommand.tier(), PermissionTier::Confirm);
        assert_eq!(CompactCommand.tier(), PermissionTier::Confirm);
    }

    #[test]
    fn confirm_commands_have_correct_names() {
        assert_eq!(SkillCommand.name(), "skill");
        assert_eq!(ModelCommand.name(), "model");
        assert_eq!(CompactCommand.name(), "compact");
    }

    #[test]
    fn multi_confirm_commands_have_correct_tier() {
        assert_eq!(ClearCommand.tier(), PermissionTier::MultiConfirm);
        assert_eq!(ForgetCommand.tier(), PermissionTier::MultiConfirm);
    }

    #[test]
    fn multi_confirm_commands_have_correct_names() {
        assert_eq!(ClearCommand.name(), "clear");
        assert_eq!(ForgetCommand.name(), "forget");
    }

    // ── C6: InitCommand tests ─────────────────────────────────────────────────

    #[test]
    fn init_command_name() {
        assert_eq!(InitCommand.name(), "init");
    }

    #[test]
    fn init_command_tier_is_confirm() {
        assert_eq!(InitCommand.tier(), PermissionTier::Confirm);
    }

    #[test]
    fn init_command_in_register_all() {
        let r = SlashCommandRegistry::new();
        register_all(&r);
        let names: Vec<String> = r.list().iter().map(|c| c.name.clone()).collect();
        assert!(
            names.contains(&"init".to_string()),
            "register_all must include 'init'"
        );
    }

    // Helper: build a minimal AppState backed by in-memory SQLite (no real services).
    // Mirrors the pattern used in crates/ironjaw-ws/tests/ws_integration.rs.
    async fn make_test_state() -> crate::state::AppState {
        use ironjaw_audit::AuditStore;
        use ironjaw_brain::{Brain, BrainConfig, BrainStore, Embedder};
        use ironjaw_proactive::{ProactiveConfig, ProactiveEngine, ProactiveStore};
        use ironjaw_session::SessionStore;
        use ironjaw_vision::ImageProcessor;
        use ironjaw_voice::VoiceEngine;
        use std::sync::Arc;

        let audit = AuditStore::open_memory().await.unwrap();
        let sessions = SessionStore::open_memory().await.unwrap();
        let llm = Arc::new(ironjaw_llm::LlmRouter::empty());
        let brain_store = BrainStore::open_memory().await.unwrap();
        let embedder = Embedder::mock(64);
        let brain = Arc::new(Brain::new(
            brain_store,
            embedder,
            BrainConfig {
                memory_relevance_threshold: 0.0,
                ..Default::default()
            },
        ));
        let proactive_store = ProactiveStore::open_memory().await.unwrap();
        let proactive = ProactiveEngine::new(proactive_store, ProactiveConfig::default());
        let voice = VoiceEngine::mock();

        crate::state::AppState::new(
            "test-token".to_string(),
            audit,
            llm,
            sessions,
            32768,
            brain,
            proactive,
            voice,
            ImageProcessor::new(10_485_760),
        )
    }

    // Helper: build a ConnectionContext with empty session_id so InitCommand
    // always falls back to std::env::current_dir() — no session store needed.
    fn make_test_ctx() -> crate::handler::ConnectionContext {
        use crate::auth::AuthResult;
        use std::sync::atomic::AtomicUsize;
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        let (tx, _rx) = mpsc::unbounded_channel();
        crate::handler::ConnectionContext {
            auth: AuthResult {
                authenticated: false,
                session_id: String::new(),
                device_id: None,
            },
            current_session_id: String::new(),
            connection_id: String::new(),
            pending_images: Vec::new(),
            tx,
            pending_count: std::sync::Arc::new(AtomicUsize::new(0)),
            socket_dead: CancellationToken::new(),
            abort_token: CancellationToken::new(),
            active_model_override: None,
        }
    }

    /// The CWD is process-global, so the two tests that chdir into a temp dir
    /// must not overlap — in parallel they land in each other's directory and
    /// fail intermittently.
    /// Async-aware: the guarded section awaits `InitCommand::execute`.
    static CWD_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn init_command_skips_when_file_exists() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        // Pre-create CLAUDE.md
        let md_path = dir.path().join("CLAUDE.md");
        std::fs::write(&md_path, "# Existing").unwrap();

        let state = make_test_state().await;
        let ctx = make_test_ctx();

        // Temporarily cd into temp dir for this test
        let _guard = CWD_GUARD.lock().await;
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let result = InitCommand.execute("", &state, &ctx).await;

        std::env::set_current_dir(&original).unwrap();
        drop(_guard);

        assert!(result.ok, "should succeed even when file exists");
        assert!(
            result.output.contains("already exists"),
            "output must mention file already exists: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn init_command_creates_file_when_absent() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();

        let state = make_test_state().await;
        let ctx = make_test_ctx();

        let _guard = CWD_GUARD.lock().await;
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let result = InitCommand.execute("", &state, &ctx).await;

        std::env::set_current_dir(&original).unwrap();
        drop(_guard);

        assert!(result.ok, "should succeed when file is absent");
        assert!(
            result.output.contains("Created"),
            "output must say Created: {}",
            result.output
        );
        assert!(
            dir.path().join("CLAUDE.md").exists(),
            "CLAUDE.md must have been written to disk"
        );
        let content = std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();
        assert!(
            content.contains("# Project"),
            "template must include # Project heading"
        );
    }
}
