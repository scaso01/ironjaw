use crate::tiers::PermissionTier;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Static mapping of protocol methods to their permission tiers.
/// Methods not in this map default to Tier 2 (confirm) for safety.
static METHOD_TIERS: LazyLock<HashMap<&'static str, PermissionTier>> = LazyLock::new(|| {
    let mut m = HashMap::new();

    // Tier 0 -- Auto (read-only)
    m.insert("auth.connect", PermissionTier::Auto);
    m.insert("ping", PermissionTier::Auto);
    m.insert("chat.history", PermissionTier::Auto);
    m.insert("chat.search", PermissionTier::Auto);
    m.insert("session.list", PermissionTier::Auto);
    m.insert("sessions.list", PermissionTier::Auto);
    m.insert("session.get", PermissionTier::Auto);
    m.insert("model.list", PermissionTier::Auto);
    m.insert("model.status", PermissionTier::Auto);
    m.insert("model.available", PermissionTier::Auto);
    m.insert("tools.list", PermissionTier::Auto);
    // tools.execute passes through here as Auto; actual tier is per-tool inside handle_tools_execute
    m.insert("tools.execute", PermissionTier::Auto);
    m.insert("infra.status", PermissionTier::Auto);
    m.insert("brain.recall", PermissionTier::Auto);
    m.insert("brain.status", PermissionTier::Auto);
    m.insert("brain.health", PermissionTier::Auto);
    m.insert("brain.memories", PermissionTier::Auto);
    m.insert("brain.timeline", PermissionTier::Auto);
    m.insert("brain.summaries", PermissionTier::Auto);
    m.insert("brain.context.list", PermissionTier::Auto);
    m.insert("brain.context.load", PermissionTier::Auto);
    m.insert("brain.context.peek", PermissionTier::Auto);
    m.insert("brain.pending.list", PermissionTier::Auto);
    m.insert("session.rename", PermissionTier::Auto);
    m.insert("session.pin", PermissionTier::Auto);
    m.insert("session.archive", PermissionTier::Auto);
    m.insert("schedule.list", PermissionTier::Auto);
    m.insert("workflow.status", PermissionTier::Auto);
    m.insert("voice.tts", PermissionTier::Auto);
    m.insert("tts.convert", PermissionTier::Auto);
    m.insert("tts.generate", PermissionTier::Auto);
    m.insert("tts.voices", PermissionTier::Auto);
    m.insert("metrics.counters", PermissionTier::Auto);

    // Tier 1 -- Notify (writes)
    m.insert("chat.send", PermissionTier::Notify);
    m.insert("chat.abort", PermissionTier::Notify);
    m.insert("session.create", PermissionTier::Notify);
    m.insert("session.switch", PermissionTier::Notify);
    m.insert("brain.remember", PermissionTier::Notify);
    m.insert("brain.consolidate", PermissionTier::Notify);
    m.insert("brain.cleanup.stale", PermissionTier::Notify);
    m.insert("brain.cleanup.contradictions", PermissionTier::Notify);
    m.insert("brain.cleanup.duplicates", PermissionTier::Notify);
    m.insert("brain.reload", PermissionTier::Notify);
    m.insert("brain.context.set", PermissionTier::Notify);
    m.insert("brain.pending.approve", PermissionTier::Notify);
    m.insert("brain.pending.reject", PermissionTier::Notify);

    // Brain auto_learn telemetry (D2)
    m.insert("brain.auto_learn.report", PermissionTier::Auto);

    // Brain snapshot tiers (D0)
    m.insert("brain.snapshot.list", PermissionTier::Auto);
    m.insert("brain.snapshot.create", PermissionTier::Notify);
    m.insert("brain.snapshot.delete", PermissionTier::Confirm);
    m.insert("brain.snapshot.restore", PermissionTier::MultiConfirm);

    // Tier 1 -- Notify (proactive)
    m.insert("schedule.toggle", PermissionTier::Notify);
    m.insert("workflow.approve", PermissionTier::Notify);

    // Tier 1 -- Notify (voice)
    m.insert("voice.start", PermissionTier::Notify);
    m.insert("voice.stop", PermissionTier::Notify);

    // Tier 0 -- Auto (plugins)
    m.insert("plugins.list", PermissionTier::Auto);

    // Tier 1 -- Notify (plugins)
    m.insert("plugins.toggle", PermissionTier::Notify);

    // Tier 0 -- Auto (Phase 11: devices + notifications)
    m.insert("device.list", PermissionTier::Auto);
    m.insert("infra.metrics", PermissionTier::Auto);

    // Tier 0 -- Auto (gateway health check for Dr. CLAW app)
    m.insert("gateway.model", PermissionTier::Auto);

    // Tier 0 -- Auto (CC Bridge proxy)
    m.insert("ccbridge.sessions", PermissionTier::Auto);
    m.insert("ccbridge.sessions.rename", PermissionTier::Auto);

    // Tier 1 -- Notify (CC Bridge chat)
    m.insert("ccbridge.chat", PermissionTier::Notify);

    // Tier 0 -- Auto (CC Chronicle proxy)
    m.insert("chronicle.sessions", PermissionTier::Auto);
    m.insert("chronicle.session", PermissionTier::Auto);
    m.insert("chronicle.search", PermissionTier::Auto);
    m.insert("chronicle.topics", PermissionTier::Auto);
    m.insert("chronicle.insights", PermissionTier::Auto);
    m.insert("chronicle.stats", PermissionTier::Auto);
    m.insert("chronicle.timeline", PermissionTier::Auto);

    // Tier 0 -- Auto (dr-claw-rpc equivalents)
    m.insert("project.status", PermissionTier::Auto);
    m.insert("file.get", PermissionTier::Auto);
    m.insert("model.llm", PermissionTier::Auto);

    // Tier 2 -- Confirm (dr-claw-rpc writes — model.switch documented as Confirm)
    m.insert("model.switch", PermissionTier::Confirm);
    m.insert("model.session", PermissionTier::Notify);
    m.insert("role.list", PermissionTier::Auto);
    m.insert("role.switch", PermissionTier::Notify);
    m.insert("role.active", PermissionTier::Auto);
    // Skill method aliases (same tiers as role.* equivalents)
    m.insert("skills.list", PermissionTier::Auto);
    m.insert("skill.list", PermissionTier::Auto);
    m.insert("skills.active", PermissionTier::Auto);
    m.insert("session.skills", PermissionTier::Auto);
    m.insert("role.preset.apply", PermissionTier::Notify);
    m.insert("chat.inject", PermissionTier::Notify);
    m.insert("chronicle.enrich", PermissionTier::Notify);

    // Tier 2 -- Confirm (gateway restart)
    m.insert("gateway.restart", PermissionTier::Confirm);

    // Dr. CLAW compatibility aliases
    m.insert("sessions.patch", PermissionTier::Auto);
    m.insert("sessions.delete", PermissionTier::Confirm);
    m.insert("sessions.preview", PermissionTier::Auto);

    // Tier 1 -- Notify (Phase 11)
    m.insert("notify.send", PermissionTier::Notify);

    // Tier 2 -- Confirm (forking)
    m.insert("session.fork", PermissionTier::Confirm);
    m.insert("session.set_mode", PermissionTier::Confirm);
    // C1 (Wave C): set per-session working directory override. Confirm because
    // it scopes file/shell tools to a new FS root — privilege-equivalent to
    // a path-traversal escape if abused.
    m.insert("session.set_cwd", PermissionTier::Confirm);

    // Tier 2 -- Confirm (external effects)
    m.insert("session.delete", PermissionTier::Confirm);
    m.insert("brain.forget", PermissionTier::Confirm);
    m.insert("brain.context.delete", PermissionTier::Confirm);
    m.insert("export.conversations", PermissionTier::Confirm);
    m.insert("export.brain", PermissionTier::Confirm);
    m.insert("schedule.create", PermissionTier::Confirm);
    m.insert("schedule.delete", PermissionTier::Confirm);
    m.insert("workflow.create", PermissionTier::Confirm);
    m.insert("workflow.cancel", PermissionTier::Confirm);
    m.insert("device.cmd", PermissionTier::Confirm);
    m.insert("backup.create", PermissionTier::Confirm);
    m.insert("brain.import", PermissionTier::Confirm);

    // Tier 0 -- Auto (Claude Code session reads)
    m.insert("claude.session.list", PermissionTier::Auto);
    m.insert("claude.session.get", PermissionTier::Auto);
    m.insert("claude.session.attach", PermissionTier::Auto);
    m.insert("claude.session.detach", PermissionTier::Auto);
    m.insert("claude.sessions.historical", PermissionTier::Auto);
    m.insert("claude.budget.get", PermissionTier::Auto);

    // Tier 1 -- Notify (Claude Code session writes)
    m.insert("claude.session.send", PermissionTier::Notify);
    m.insert("claude.session.interrupt", PermissionTier::Notify);
    m.insert("claude.permission.respond", PermissionTier::Notify);

    // Tier 2 -- Confirm (Claude Code session lifecycle)
    m.insert("claude.session.create", PermissionTier::Notify);
    m.insert("claude.session.resume", PermissionTier::Confirm);
    m.insert("claude.session.destroy", PermissionTier::Confirm);
    m.insert("claude.budget.set", PermissionTier::Confirm);

    // Tier 0 -- Auto (CC session reads)
    m.insert("cc.sessions", PermissionTier::Auto);
    m.insert("cc.session.get", PermissionTier::Auto);
    m.insert("cc.history", PermissionTier::Auto);
    m.insert("cc.history.since", PermissionTier::Auto);
    m.insert("cc.attach", PermissionTier::Auto);
    m.insert("cc.detach", PermissionTier::Auto);

    // Tier 1 -- Notify (CC session writes)
    m.insert("cc.chat", PermissionTier::Notify);
    m.insert("cc.chat.abort", PermissionTier::Notify);

    // Tier 2 -- Confirm (CC session lifecycle — documented as Confirm)
    m.insert("cc.session.create", PermissionTier::Confirm);

    // Tier 0 -- Auto (Command Center reads)
    m.insert("command.sessions", PermissionTier::Auto);
    m.insert("command.session.get", PermissionTier::Auto);
    m.insert("command.usage.summary", PermissionTier::Auto);
    m.insert("command.usage.by_role", PermissionTier::Auto);

    // Tier 0 -- Auto (Phase 4 Polish: system + model perf)
    m.insert("system.resources", PermissionTier::Auto);
    m.insert("model.perf", PermissionTier::Auto);

    // Tier 2 -- Confirm (Command Center actions — documented as Confirm)
    m.insert("command.session.action", PermissionTier::Confirm);

    // Tier 2 -- Confirm (device pairing)
    m.insert("device.pair.request", PermissionTier::Confirm);
    m.insert("device.pair.revoke", PermissionTier::Confirm);

    // Tier 0 -- Auto (device pairing confirmation -- user already confirmed on device)
    m.insert("device.pair.confirm", PermissionTier::Auto);

    // Tier 0 -- Auto (evolution reads)
    m.insert("brain.evolve.status", PermissionTier::Auto);
    m.insert("brain.evolve.history", PermissionTier::Auto);

    // Tier 1 -- Notify (evolution writes)
    m.insert("brain.evolve.stop", PermissionTier::Notify);

    // Tier 2 -- Confirm (evolution lifecycle)
    m.insert("brain.evolve.start", PermissionTier::Confirm);
    m.insert("brain.evolve.promote", PermissionTier::Confirm);

    // Tier 0 -- Auto (experiment reads)
    m.insert("experiment.status", PermissionTier::Auto);
    m.insert("experiment.results", PermissionTier::Auto);

    // Tier 1 -- Notify (experiment writes)
    m.insert("experiment.stop", PermissionTier::Notify);

    // Tier 2 -- Confirm (experiment lifecycle)
    m.insert("experiment.create", PermissionTier::Confirm);
    m.insert("experiment.start", PermissionTier::Confirm);
    m.insert("experiment.resume", PermissionTier::Confirm);

    // Tier 1 -- Notify (agent architecture: I5/I9/I7/I8)
    m.insert("agent.handoff", PermissionTier::Notify);
    m.insert("agent.scatter", PermissionTier::Notify);
    m.insert("blackboard.write", PermissionTier::Notify);
    m.insert("blackboard.subscribe", PermissionTier::Notify);
    m.insert("group.message", PermissionTier::Notify);

    // Tier 0 -- Auto (agent architecture reads)
    m.insert("blackboard.read", PermissionTier::Auto);
    m.insert("blackboard.list", PermissionTier::Auto);
    m.insert("group.history", PermissionTier::Auto);
    m.insert("group.list", PermissionTier::Auto);

    // Tier 2 -- Confirm (agent architecture: creation/destruction)
    m.insert("blackboard.create", PermissionTier::Confirm);
    m.insert("blackboard.destroy", PermissionTier::Confirm);
    m.insert("group.create", PermissionTier::Confirm);
    m.insert("group.terminate", PermissionTier::Confirm);

    // ---- CC Parity Phase 6: Context & Safety (Task 18) ----
    m.insert("tools.search", PermissionTier::Auto);
    m.insert("context.status", PermissionTier::Auto);
    m.insert("context.compact", PermissionTier::Notify);
    m.insert("context.collapse", PermissionTier::Notify);
    m.insert("cache.stats", PermissionTier::Auto);
    m.insert("file.snapshots", PermissionTier::Auto);
    m.insert("file.restore", PermissionTier::Confirm);
    m.insert("security.rules.list", PermissionTier::Auto);
    m.insert("security.rules.add", PermissionTier::Confirm);
    m.insert("security.rules.remove", PermissionTier::Confirm);
    m.insert("security.rules.test", PermissionTier::Auto);

    // ---- CC Parity Phase 6: Agent & Advanced (Task 19) ----
    m.insert("agent.fork", PermissionTier::Confirm);
    m.insert("agent.worktree.create", PermissionTier::Confirm);
    m.insert("agent.worktree.cleanup", PermissionTier::Confirm);
    m.insert("agent.mailbox.send", PermissionTier::Notify);
    m.insert("agent.mailbox.peek", PermissionTier::Auto);
    m.insert("agent.mailbox.claim", PermissionTier::Notify);
    m.insert("session.denials", PermissionTier::Auto);
    m.insert("security.classifier.stats", PermissionTier::Auto);
    m.insert("daemon.status", PermissionTier::Auto);
    m.insert("daemon.log", PermissionTier::Auto);
    m.insert("brain.dream.status", PermissionTier::Auto);
    m.insert("brain.dream.trigger", PermissionTier::Confirm);
    m.insert("sentiment.check", PermissionTier::Auto);
    m.insert("security.anti_distill.status", PermissionTier::Auto);
    m.insert("prefetch.status", PermissionTier::Auto);

    // Tier 0 -- Auto (prompt metrics)
    m.insert("prompt.stats", PermissionTier::Auto);

    // Tier 0 -- Auto (slash command interface)
    m.insert("commands.list", PermissionTier::Auto);
    m.insert("commands.execute", PermissionTier::Auto);

    // B2 CP6 -- hook framework RPC
    // hooks.list is Auto (read-only catalog dump).
    // hooks.reload is MultiConfirm: rebuilds the live HookRegistry from disk
    // and can introduce arbitrary new pre-* hooks (subprocess-gated, but
    // still a privileged operation).
    m.insert("hooks.list", PermissionTier::Auto);
    m.insert("hooks.reload", PermissionTier::MultiConfirm);

    // Tier 1 -- Notify (slash commands: usage, peers, search, recall)
    m.insert("slash.usage", PermissionTier::Notify);
    m.insert("slash.peers", PermissionTier::Notify);
    m.insert("slash.search", PermissionTier::Notify);
    m.insert("slash.recall", PermissionTier::Notify);

    // Tier 3 -- Multi-confirm (destructive)
    // None defined in Phase 1 -- added as tools are implemented

    m
});

/// Look up the permission tier for a given method name.
/// Unknown methods default to Tier 2 (Confirm) for safety.
pub fn tier_for_method(method: &str) -> PermissionTier {
    METHOD_TIERS
        .get(method)
        .copied()
        .unwrap_or(PermissionTier::Confirm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_tier0_methods() {
        assert_eq!(tier_for_method("auth.connect"), PermissionTier::Auto);
        assert_eq!(tier_for_method("ping"), PermissionTier::Auto);
        assert_eq!(tier_for_method("session.list"), PermissionTier::Auto);
        assert_eq!(tier_for_method("gateway.model"), PermissionTier::Auto);
    }

    #[test]
    fn test_known_tier1_methods() {
        assert_eq!(tier_for_method("chat.send"), PermissionTier::Notify);
        assert_eq!(tier_for_method("session.create"), PermissionTier::Notify);
    }

    #[test]
    fn test_slash_notify_commands() {
        assert_eq!(tier_for_method("slash.usage"), PermissionTier::Notify);
        assert_eq!(tier_for_method("slash.peers"), PermissionTier::Notify);
        assert_eq!(tier_for_method("slash.search"), PermissionTier::Notify);
        assert_eq!(tier_for_method("slash.recall"), PermissionTier::Notify);
    }

    #[test]
    fn test_known_tier2_methods() {
        assert_eq!(tier_for_method("session.delete"), PermissionTier::Confirm);
    }

    #[test]
    fn test_unknown_method_defaults_to_confirm() {
        assert_eq!(
            tier_for_method("something.unknown"),
            PermissionTier::Confirm
        );
        assert_eq!(tier_for_method(""), PermissionTier::Confirm);
    }

    #[test]
    fn test_new_method_tiers() {
        assert_eq!(tier_for_method("chat.inject"), PermissionTier::Notify);
        assert_eq!(tier_for_method("gateway.restart"), PermissionTier::Confirm);
        assert_eq!(tier_for_method("chronicle.enrich"), PermissionTier::Notify);
    }

    #[test]
    fn test_phase4_polish_method_tiers() {
        assert_eq!(tier_for_method("system.resources"), PermissionTier::Auto);
        assert_eq!(tier_for_method("model.perf"), PermissionTier::Auto);
    }

    #[test]
    fn test_cc_parity_task18_method_tiers() {
        // Auto (read-only)
        assert_eq!(tier_for_method("tools.search"), PermissionTier::Auto);
        assert_eq!(tier_for_method("context.status"), PermissionTier::Auto);
        assert_eq!(tier_for_method("cache.stats"), PermissionTier::Auto);
        assert_eq!(tier_for_method("file.snapshots"), PermissionTier::Auto);
        assert_eq!(tier_for_method("security.rules.list"), PermissionTier::Auto);
        assert_eq!(tier_for_method("security.rules.test"), PermissionTier::Auto);
        // Notify (mutations)
        assert_eq!(tier_for_method("context.compact"), PermissionTier::Notify);
        assert_eq!(tier_for_method("context.collapse"), PermissionTier::Notify);
        // Confirm (destructive)
        assert_eq!(tier_for_method("file.restore"), PermissionTier::Confirm);
        assert_eq!(
            tier_for_method("security.rules.add"),
            PermissionTier::Confirm
        );
        assert_eq!(
            tier_for_method("security.rules.remove"),
            PermissionTier::Confirm
        );
    }

    #[test]
    fn test_cc_parity_task19_method_tiers() {
        // Auto (reads)
        assert_eq!(tier_for_method("agent.mailbox.peek"), PermissionTier::Auto);
        assert_eq!(tier_for_method("session.denials"), PermissionTier::Auto);
        assert_eq!(
            tier_for_method("security.classifier.stats"),
            PermissionTier::Auto
        );
        assert_eq!(tier_for_method("daemon.status"), PermissionTier::Auto);
        assert_eq!(tier_for_method("daemon.log"), PermissionTier::Auto);
        assert_eq!(tier_for_method("brain.dream.status"), PermissionTier::Auto);
        assert_eq!(tier_for_method("sentiment.check"), PermissionTier::Auto);
        assert_eq!(
            tier_for_method("security.anti_distill.status"),
            PermissionTier::Auto
        );
        assert_eq!(tier_for_method("prefetch.status"), PermissionTier::Auto);
        // Notify (writes)
        assert_eq!(
            tier_for_method("agent.mailbox.send"),
            PermissionTier::Notify
        );
        assert_eq!(
            tier_for_method("agent.mailbox.claim"),
            PermissionTier::Notify
        );
        // Confirm (lifecycle)
        assert_eq!(tier_for_method("agent.fork"), PermissionTier::Confirm);
        assert_eq!(
            tier_for_method("agent.worktree.create"),
            PermissionTier::Confirm
        );
        assert_eq!(
            tier_for_method("agent.worktree.cleanup"),
            PermissionTier::Confirm
        );
        assert_eq!(
            tier_for_method("brain.dream.trigger"),
            PermissionTier::Confirm
        );
    }

    #[test]
    fn test_tools_execute_is_auto_tier() {
        // A12: tools.execute method-level gate is Auto so the handler can
        // apply per-tool tiers internally instead of blanket Notify.
        assert_eq!(tier_for_method("tools.execute"), PermissionTier::Auto);
        assert!(!tier_for_method("tools.execute").requires_approval());
        assert!(!tier_for_method("tools.execute").notifies_after());
    }

    #[test]
    fn test_commands_method_tiers() {
        assert_eq!(tier_for_method("commands.list"), PermissionTier::Auto);
        assert_eq!(tier_for_method("commands.execute"), PermissionTier::Auto);
    }

    #[test]
    fn test_c1_session_set_cwd_tier() {
        // C1 (Wave C): per-session cwd override. Confirm tier — scoping
        // tools to an arbitrary FS root warrants a HITL prompt.
        assert_eq!(tier_for_method("session.set_cwd"), PermissionTier::Confirm);
        assert!(tier_for_method("session.set_cwd").requires_approval());
    }

    #[test]
    fn test_hooks_method_tiers() {
        // B2 CP6: hooks.list is read-only; hooks.reload mutates the live
        // registry from disk and is gated MultiConfirm.
        assert_eq!(tier_for_method("hooks.list"), PermissionTier::Auto);
        assert_eq!(
            tier_for_method("hooks.reload"),
            PermissionTier::MultiConfirm
        );
        assert!(tier_for_method("hooks.reload").requires_approval());
    }
}
