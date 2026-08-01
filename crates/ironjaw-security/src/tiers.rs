use serde::{Deserialize, Serialize};

/// Permission tiers control what operations are auto-approved vs require confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum PermissionTier {
    /// Tier 0: Auto-approve. Read-only ops -- searches, file reads, status checks.
    Auto = 0,
    /// Tier 1: Notify after execution. Writes, commits, self-messages.
    Notify = 1,
    /// Tier 2: Confirm before execution. SMS, email, external messages.
    /// Requires HITL (Human-In-The-Loop) approval.
    Confirm = 2,
    /// Tier 3: Multi-confirm. Destructive ops, credential access, irreversible actions.
    /// Requires explicit typed confirmation.
    MultiConfirm = 3,
}

impl PermissionTier {
    /// Whether this tier requires approval before execution
    pub fn requires_approval(&self) -> bool {
        matches!(self, PermissionTier::Confirm | PermissionTier::MultiConfirm)
    }

    /// Whether this tier sends a notification after execution
    pub fn notifies_after(&self) -> bool {
        matches!(self, PermissionTier::Notify)
    }

    /// Human-readable label
    pub fn label(&self) -> &'static str {
        match self {
            PermissionTier::Auto => "auto",
            PermissionTier::Notify => "notify",
            PermissionTier::Confirm => "confirm",
            PermissionTier::MultiConfirm => "multi-confirm",
        }
    }
}

impl std::fmt::Display for PermissionTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tier {} ({})", *self as u8, self.label())
    }
}

/// Result of a permission check
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    /// Operation is allowed to proceed immediately
    Allowed,
    /// Operation is allowed but user should be notified
    AllowedWithNotify,
    /// Operation requires HITL approval before proceeding
    NeedsApproval { reason: String },
    /// Operation requires explicit multi-step confirmation
    NeedsMultiConfirm { reason: String },
}

/// Check whether an operation at the given tier is allowed
pub fn check_permission(tier: PermissionTier) -> PermissionDecision {
    match tier {
        PermissionTier::Auto => PermissionDecision::Allowed,
        PermissionTier::Notify => PermissionDecision::AllowedWithNotify,
        PermissionTier::Confirm => PermissionDecision::NeedsApproval {
            reason: format!("{tier} operation requires confirmation"),
        },
        PermissionTier::MultiConfirm => PermissionDecision::NeedsMultiConfirm {
            reason: format!("{tier} operation requires explicit confirmation"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tier_ordering() {
        assert!(PermissionTier::Auto < PermissionTier::Notify);
        assert!(PermissionTier::Notify < PermissionTier::Confirm);
        assert!(PermissionTier::Confirm < PermissionTier::MultiConfirm);
    }

    #[test]
    fn test_requires_approval() {
        assert!(!PermissionTier::Auto.requires_approval());
        assert!(!PermissionTier::Notify.requires_approval());
        assert!(PermissionTier::Confirm.requires_approval());
        assert!(PermissionTier::MultiConfirm.requires_approval());
    }

    #[test]
    fn test_notifies_after() {
        assert!(!PermissionTier::Auto.notifies_after());
        assert!(PermissionTier::Notify.notifies_after());
        assert!(!PermissionTier::Confirm.notifies_after());
        assert!(!PermissionTier::MultiConfirm.notifies_after());
    }

    #[test]
    fn test_check_permission_auto() {
        assert_eq!(
            check_permission(PermissionTier::Auto),
            PermissionDecision::Allowed
        );
    }

    #[test]
    fn test_check_permission_notify() {
        assert_eq!(
            check_permission(PermissionTier::Notify),
            PermissionDecision::AllowedWithNotify
        );
    }

    #[test]
    fn test_check_permission_confirm() {
        let decision = check_permission(PermissionTier::Confirm);
        assert!(matches!(decision, PermissionDecision::NeedsApproval { .. }));
    }

    #[test]
    fn test_check_permission_multi_confirm() {
        let decision = check_permission(PermissionTier::MultiConfirm);
        assert!(matches!(
            decision,
            PermissionDecision::NeedsMultiConfirm { .. }
        ));
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", PermissionTier::Auto), "Tier 0 (auto)");
        assert_eq!(
            format!("{}", PermissionTier::MultiConfirm),
            "Tier 3 (multi-confirm)"
        );
    }

    #[test]
    fn test_serde_roundtrip() {
        let tier = PermissionTier::Confirm;
        let json = serde_json::to_string(&tier).unwrap();
        let parsed: PermissionTier = serde_json::from_str(&json).unwrap();
        assert_eq!(tier, parsed);
    }
}
