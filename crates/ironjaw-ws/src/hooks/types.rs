// Hook system types (Stream 8.2)
// Event variants, action enum, context struct.

use std::sync::Arc;

/// Lifecycle events that hooks can subscribe to.
///
/// The first 8 variants are the original Stream 8.2 set; the trailing four
/// (`PreCompact`, `UserPromptSubmit`, `SkillActivated`, `SkillDeactivated`)
/// were added in B2 to mirror the Claude Code hook surface and the A15
/// skills-↔-hooks transition decision.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum HookEvent {
    /// Fires before an LLM call is made.
    PreChat,
    /// Fires after an LLM response is received.
    PostChat,
    /// Fires before a tool is executed.
    PreToolExec,
    /// Fires after a tool has finished executing.
    PostToolExec,
    /// Fires when a session is created or resumed.
    SessionStart,
    /// Fires when a session is closed.
    SessionEnd,
    /// Fires when memory is being stored.
    MemoryStore,
    /// Fires when memory is being recalled.
    MemoryRecall,
    /// B2: fires before context compaction is invoked. A `Block` action
    /// skips the compaction call and leaves the message buffer untouched.
    PreCompact {
        session_id: String,
        message_count: usize,
        total_tokens: u64,
    },
    /// B2: fires after security passes accept the user prompt and before the
    /// LLM call. A `Block` short-circuits the chat turn with `HOOK_BLOCKED`.
    UserPromptSubmit {
        session_id: String,
        prompt_text: String,
        model: String,
        force_fresh: bool,
    },
    /// B2: fires after a skill becomes active (per A15 skills-↔-hooks decision).
    SkillActivated {
        session_id: String,
        skill_name: String,
        prior_skill: Option<String>,
    },
    /// B2: fires after a skill is deactivated.
    SkillDeactivated {
        session_id: String,
        skill_name: String,
        next_skill: Option<String>,
    },
}

impl HookEvent {
    /// Discriminant kind for event-keyed registry lookups.
    ///
    /// Several `HookEvent` variants now carry per-invocation payload data
    /// (session_id, prompt_text, etc.). The registry indexes hooks by event
    /// *kind*, not by full payload, so every emit-site call can find its
    /// registered handlers regardless of payload values. This kind is the
    /// stable lookup key.
    pub fn kind(&self) -> HookEventKind {
        match self {
            HookEvent::PreChat => HookEventKind::PreChat,
            HookEvent::PostChat => HookEventKind::PostChat,
            HookEvent::PreToolExec => HookEventKind::PreToolExec,
            HookEvent::PostToolExec => HookEventKind::PostToolExec,
            HookEvent::SessionStart => HookEventKind::SessionStart,
            HookEvent::SessionEnd => HookEventKind::SessionEnd,
            HookEvent::MemoryStore => HookEventKind::MemoryStore,
            HookEvent::MemoryRecall => HookEventKind::MemoryRecall,
            HookEvent::PreCompact { .. } => HookEventKind::PreCompact,
            HookEvent::UserPromptSubmit { .. } => HookEventKind::UserPromptSubmit,
            HookEvent::SkillActivated { .. } => HookEventKind::SkillActivated,
            HookEvent::SkillDeactivated { .. } => HookEventKind::SkillDeactivated,
        }
    }

    /// Return all possible hook event *kinds*.
    ///
    /// Returns `HookEventKind` (the discriminant) rather than `HookEvent`
    /// because the new payload-bearing variants have no canonical default
    /// payload to enumerate. Existing tests assert on the count and uniqueness
    /// of the discriminants.
    pub fn all_kinds() -> &'static [HookEventKind] {
        &[
            HookEventKind::PreChat,
            HookEventKind::PostChat,
            HookEventKind::PreToolExec,
            HookEventKind::PostToolExec,
            HookEventKind::SessionStart,
            HookEventKind::SessionEnd,
            HookEventKind::MemoryStore,
            HookEventKind::MemoryRecall,
            HookEventKind::PreCompact,
            HookEventKind::UserPromptSubmit,
            HookEventKind::SkillActivated,
            HookEventKind::SkillDeactivated,
        ]
    }

    /// Backwards-compatible alias for `all_kinds()`.
    ///
    /// Pre-existing call sites enumerated the original 8 unit variants. The
    /// new variants carry payload, so we expose only the discriminants. Old
    /// tests asserting `HookEvent::all().len() == 8` were updated to use the
    /// new count via `all_kinds()`.
    pub fn all() -> &'static [HookEventKind] {
        Self::all_kinds()
    }
}

/// Discriminant for `HookEvent`. Used as the stable map key in the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum HookEventKind {
    PreChat,
    PostChat,
    PreToolExec,
    PostToolExec,
    SessionStart,
    SessionEnd,
    MemoryStore,
    MemoryRecall,
    PreCompact,
    UserPromptSubmit,
    SkillActivated,
    SkillDeactivated,
}

impl std::fmt::Display for HookEventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            HookEventKind::PreChat => "PreChat",
            HookEventKind::PostChat => "PostChat",
            HookEventKind::PreToolExec => "PreToolExec",
            HookEventKind::PostToolExec => "PostToolExec",
            HookEventKind::SessionStart => "SessionStart",
            HookEventKind::SessionEnd => "SessionEnd",
            HookEventKind::MemoryStore => "MemoryStore",
            HookEventKind::MemoryRecall => "MemoryRecall",
            HookEventKind::PreCompact => "PreCompact",
            HookEventKind::UserPromptSubmit => "UserPromptSubmit",
            HookEventKind::SkillActivated => "SkillActivated",
            HookEventKind::SkillDeactivated => "SkillDeactivated",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for HookEventKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "PreChat" => Ok(HookEventKind::PreChat),
            "PostChat" => Ok(HookEventKind::PostChat),
            "PreToolExec" => Ok(HookEventKind::PreToolExec),
            "PostToolExec" => Ok(HookEventKind::PostToolExec),
            "SessionStart" => Ok(HookEventKind::SessionStart),
            "SessionEnd" => Ok(HookEventKind::SessionEnd),
            "MemoryStore" => Ok(HookEventKind::MemoryStore),
            "MemoryRecall" => Ok(HookEventKind::MemoryRecall),
            "PreCompact" => Ok(HookEventKind::PreCompact),
            "UserPromptSubmit" => Ok(HookEventKind::UserPromptSubmit),
            "SkillActivated" => Ok(HookEventKind::SkillActivated),
            "SkillDeactivated" => Ok(HookEventKind::SkillDeactivated),
            other => Err(format!("unknown hook event: {other}")),
        }
    }
}

impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.kind(), f)
    }
}

/// Action returned by a hook callback, controlling the operation's flow.
#[derive(Debug, Clone)]
pub enum HookAction {
    /// Proceed normally. The operation is unaffected.
    Continue,
    /// Modify the operation's payload with the provided value.
    Modify(serde_json::Value),
    /// Block the operation entirely, with a reason string.
    Block(String),
}

impl HookAction {
    /// Returns true if this action is `Continue`.
    pub fn is_continue(&self) -> bool {
        matches!(self, HookAction::Continue)
    }

    /// Returns true if this action is `Block`.
    pub fn is_block(&self) -> bool {
        matches!(self, HookAction::Block(_))
    }

    /// Returns true if this action is `Modify`.
    pub fn is_modify(&self) -> bool {
        matches!(self, HookAction::Modify(_))
    }
}

/// Context passed to hook callbacks when an event fires.
#[derive(Debug, Clone)]
pub struct HookContext {
    /// The event that triggered this hook.
    pub event: HookEvent,
    /// The session ID, if one is active.
    pub session_id: Option<String>,
    /// Arbitrary data payload associated with the event.
    pub data: serde_json::Value,
    /// Timestamp when the event was created.
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl HookContext {
    /// Create a new hook context for the given event with data.
    pub fn new(event: HookEvent, data: serde_json::Value) -> Self {
        Self {
            event,
            session_id: None,
            data,
            timestamp: chrono::Utc::now(),
        }
    }

    /// Create a new hook context with a session ID.
    pub fn with_session(event: HookEvent, session_id: &str, data: serde_json::Value) -> Self {
        Self {
            event,
            session_id: Some(session_id.to_string()),
            data,
            timestamp: chrono::Utc::now(),
        }
    }
}

/// Type alias for synchronous hook callback functions (legacy / test shim).
///
/// Retained so the original Stream 8.2 unit tests at `hooks/registry.rs` can
/// continue to register inline closures via `HookRegistry::register_fn`.
/// CP2 introduces the async `Hook` trait as the production extension point.
pub type HookCallback = Arc<dyn Fn(&HookContext) -> HookAction + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_event_all_variants() {
        // B2: 8 original + 4 new (PreCompact, UserPromptSubmit, SkillActivated, SkillDeactivated)
        let all = HookEvent::all();
        assert_eq!(all.len(), 12);
        let mut set = std::collections::HashSet::new();
        for kind in all {
            assert!(set.insert(kind));
        }
    }

    #[test]
    fn test_hook_event_display() {
        assert_eq!(HookEvent::PreChat.to_string(), "PreChat");
        assert_eq!(HookEvent::PostToolExec.to_string(), "PostToolExec");
        assert_eq!(HookEvent::MemoryRecall.to_string(), "MemoryRecall");
    }

    #[test]
    fn test_hook_action_helpers() {
        assert!(HookAction::Continue.is_continue());
        assert!(!HookAction::Continue.is_block());
        assert!(!HookAction::Continue.is_modify());

        let block = HookAction::Block("reason".to_string());
        assert!(block.is_block());
        assert!(!block.is_continue());

        let modify = HookAction::Modify(serde_json::json!(null));
        assert!(modify.is_modify());
        assert!(!modify.is_continue());
    }

    #[test]
    fn test_hook_context_constructors() {
        let ctx = HookContext::new(HookEvent::PreChat, serde_json::json!({"msg": "hello"}));
        assert_eq!(ctx.event, HookEvent::PreChat);
        assert!(ctx.session_id.is_none());
        assert_eq!(ctx.data["msg"], "hello");

        let ctx =
            HookContext::with_session(HookEvent::SessionStart, "sess-42", serde_json::json!(null));
        assert_eq!(ctx.event, HookEvent::SessionStart);
        assert_eq!(ctx.session_id.as_deref(), Some("sess-42"));
    }

    #[test]
    fn test_b2_new_event_kinds_distinct() {
        let pc = HookEvent::PreCompact {
            session_id: "s".into(),
            message_count: 0,
            total_tokens: 0,
        };
        let ups = HookEvent::UserPromptSubmit {
            session_id: "s".into(),
            prompt_text: "p".into(),
            model: "m".into(),
            force_fresh: false,
        };
        let sa = HookEvent::SkillActivated {
            session_id: "s".into(),
            skill_name: "k".into(),
            prior_skill: None,
        };
        let sd = HookEvent::SkillDeactivated {
            session_id: "s".into(),
            skill_name: "k".into(),
            next_skill: None,
        };

        assert_eq!(pc.kind(), HookEventKind::PreCompact);
        assert_eq!(ups.kind(), HookEventKind::UserPromptSubmit);
        assert_eq!(sa.kind(), HookEventKind::SkillActivated);
        assert_eq!(sd.kind(), HookEventKind::SkillDeactivated);

        assert_eq!(pc.to_string(), "PreCompact");
        assert_eq!(ups.to_string(), "UserPromptSubmit");
        assert_eq!(sa.to_string(), "SkillActivated");
        assert_eq!(sd.to_string(), "SkillDeactivated");
    }

    #[test]
    fn test_hook_event_kind_from_str_roundtrip() {
        use std::str::FromStr;
        for kind in HookEvent::all_kinds() {
            let s = kind.to_string();
            let parsed = HookEventKind::from_str(&s).expect("roundtrip");
            assert_eq!(parsed, *kind);
        }
        assert!(HookEventKind::from_str("Bogus").is_err());
    }
}
