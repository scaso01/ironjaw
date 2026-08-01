// Conversation forking (Stream 13 / P3 Extension Points)
// Allows conversations to be forked (branched) at any message point.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// A single fork linking a parent session to a child session at a specific message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fork {
    /// Unique fork identifier.
    pub id: String,
    /// Session that was forked from.
    pub parent_session_id: String,
    /// New session created by the fork.
    pub child_session_id: String,
    /// Message ID at which the fork was created.
    pub fork_point_message_id: String,
    /// When the fork was created.
    pub created_at: DateTime<Utc>,
    /// Optional human-readable description of why this fork was created.
    pub description: Option<String>,
}

/// Tree node tracking a session's position in the fork hierarchy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkTree {
    /// The session this node represents.
    pub session_id: String,
    /// Fork ID of the parent relationship (None for root sessions).
    pub parent: Option<String>,
    /// Fork IDs of child relationships.
    pub children: Vec<String>,
}

/// Errors returned by [`ForkManager`] operations.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ForkError {
    #[error("fork not found: {0}")]
    NotFound(String),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("cannot fork: parent and child must be different")]
    SameSession,
    #[error("duplicate fork: already exists between these sessions")]
    DuplicateFork,
}

/// Manages fork relationships between sessions.
///
/// Tracks parent-child session relationships and supports lineage traversal.
pub struct ForkManager {
    forks: HashMap<String, Fork>,
    trees: HashMap<String, ForkTree>,
}

impl ForkManager {
    /// Create an empty fork manager.
    pub fn new() -> Self {
        Self {
            forks: HashMap::new(),
            trees: HashMap::new(),
        }
    }

    /// Create a fork from `parent_session_id` to `child_session_id` at the given message.
    ///
    /// Returns the newly created fork ID.
    pub fn create_fork(
        &mut self,
        parent_session_id: &str,
        child_session_id: &str,
        fork_point_message_id: &str,
        description: Option<String>,
    ) -> Result<String, ForkError> {
        if parent_session_id == child_session_id {
            return Err(ForkError::SameSession);
        }

        // Check for duplicate: same parent -> same child.
        let duplicate = self.forks.values().any(|f| {
            f.parent_session_id == parent_session_id && f.child_session_id == child_session_id
        });
        if duplicate {
            return Err(ForkError::DuplicateFork);
        }

        let fork_id = Uuid::new_v4().to_string();
        let fork = Fork {
            id: fork_id.clone(),
            parent_session_id: parent_session_id.to_string(),
            child_session_id: child_session_id.to_string(),
            fork_point_message_id: fork_point_message_id.to_string(),
            created_at: Utc::now(),
            description,
        };
        self.forks.insert(fork_id.clone(), fork);

        // Ensure parent tree node exists and add child fork.
        self.trees
            .entry(parent_session_id.to_string())
            .or_insert_with(|| ForkTree {
                session_id: parent_session_id.to_string(),
                parent: None,
                children: Vec::new(),
            })
            .children
            .push(fork_id.clone());

        // Ensure child tree node exists and set parent fork.
        let child_tree = self
            .trees
            .entry(child_session_id.to_string())
            .or_insert_with(|| ForkTree {
                session_id: child_session_id.to_string(),
                parent: None,
                children: Vec::new(),
            });
        child_tree.parent = Some(fork_id.clone());

        Ok(fork_id)
    }

    /// Look up a fork by its ID.
    pub fn get_fork(&self, fork_id: &str) -> Option<&Fork> {
        self.forks.get(fork_id)
    }

    /// List all forks originating from or targeting a session, sorted by creation time.
    pub fn list_forks_for_session(&self, session_id: &str) -> Vec<&Fork> {
        let mut forks: Vec<&Fork> = self
            .forks
            .values()
            .filter(|f| f.parent_session_id == session_id || f.child_session_id == session_id)
            .collect();
        forks.sort_by_key(|a| a.created_at);
        forks
    }

    /// Get the tree node for a session.
    pub fn get_tree(&self, session_id: &str) -> Option<&ForkTree> {
        self.trees.get(session_id)
    }

    /// Get child session IDs for a session (via its fork children).
    pub fn get_children(&self, session_id: &str) -> Vec<String> {
        self.trees
            .get(session_id)
            .map(|tree| {
                tree.children
                    .iter()
                    .filter_map(|fork_id| {
                        self.forks.get(fork_id).map(|f| f.child_session_id.clone())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get the parent session ID for a session (via its parent fork).
    pub fn get_parent(&self, session_id: &str) -> Option<String> {
        self.trees.get(session_id).and_then(|tree| {
            tree.parent
                .as_ref()
                .and_then(|fork_id| self.forks.get(fork_id).map(|f| f.parent_session_id.clone()))
        })
    }

    /// Walk up the parent chain to build the full ancestry path from root to this session.
    ///
    /// Returns a vec starting with the root ancestor and ending with `session_id`.
    pub fn get_lineage(&self, session_id: &str) -> Vec<String> {
        let mut lineage = vec![session_id.to_string()];
        let mut current = session_id.to_string();

        while let Some(parent) = self.get_parent(&current) {
            // Guard against cycles.
            if lineage.contains(&parent) {
                break;
            }
            lineage.push(parent.clone());
            current = parent;
        }

        lineage.reverse();
        lineage
    }

    /// Delete a fork by ID, cleaning up tree references.
    pub fn delete_fork(&mut self, fork_id: &str) -> Result<(), ForkError> {
        let fork = self
            .forks
            .remove(fork_id)
            .ok_or_else(|| ForkError::NotFound(fork_id.to_string()))?;

        // Remove from parent's children list.
        if let Some(parent_tree) = self.trees.get_mut(&fork.parent_session_id) {
            parent_tree.children.retain(|id| id != fork_id);
        }

        // Clear child's parent reference.
        if let Some(child_tree) = self.trees.get_mut(&fork.child_session_id) {
            if child_tree.parent.as_deref() == Some(fork_id) {
                child_tree.parent = None;
            }
        }

        Ok(())
    }

    /// Total number of forks.
    pub fn fork_count(&self) -> usize {
        self.forks.len()
    }

    /// Total number of sessions tracked in the fork tree.
    pub fn session_count(&self) -> usize {
        self.trees.len()
    }
}

impl Default for ForkManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- create_fork -----------------------------------------------------

    #[test]
    fn create_fork_returns_id() {
        let mut mgr = ForkManager::new();
        let id = mgr
            .create_fork("s1", "s2", "msg-1", Some("branch A".into()))
            .unwrap();
        assert!(!id.is_empty());
    }

    #[test]
    fn create_fork_stores_fork() {
        let mut mgr = ForkManager::new();
        let id = mgr.create_fork("s1", "s2", "msg-1", None).unwrap();

        let fork = mgr.get_fork(&id).unwrap();
        assert_eq!(fork.parent_session_id, "s1");
        assert_eq!(fork.child_session_id, "s2");
        assert_eq!(fork.fork_point_message_id, "msg-1");
        assert!(fork.description.is_none());
    }

    #[test]
    fn create_fork_with_description() {
        let mut mgr = ForkManager::new();
        let id = mgr
            .create_fork("s1", "s2", "msg-1", Some("experiment".into()))
            .unwrap();

        let fork = mgr.get_fork(&id).unwrap();
        assert_eq!(fork.description.as_deref(), Some("experiment"));
    }

    #[test]
    fn create_fork_same_session_fails() {
        let mut mgr = ForkManager::new();
        let err = mgr.create_fork("s1", "s1", "msg-1", None).unwrap_err();
        assert_eq!(err, ForkError::SameSession);
    }

    #[test]
    fn create_fork_duplicate_fails() {
        let mut mgr = ForkManager::new();
        mgr.create_fork("s1", "s2", "msg-1", None).unwrap();

        let err = mgr.create_fork("s1", "s2", "msg-5", None).unwrap_err();
        assert_eq!(err, ForkError::DuplicateFork);
    }

    #[test]
    fn create_fork_reverse_direction_allowed() {
        let mut mgr = ForkManager::new();
        mgr.create_fork("s1", "s2", "msg-1", None).unwrap();
        // s2 -> s1 is a different direction, should succeed.
        mgr.create_fork("s2", "s1", "msg-2", None).unwrap();
        assert_eq!(mgr.fork_count(), 2);
    }

    // -- get_fork --------------------------------------------------------

    #[test]
    fn get_fork_missing_returns_none() {
        let mgr = ForkManager::new();
        assert!(mgr.get_fork("nonexistent").is_none());
    }

    // -- list_forks_for_session ------------------------------------------

    #[test]
    fn list_forks_for_session_finds_parent_and_child() {
        let mut mgr = ForkManager::new();
        mgr.create_fork("s1", "s2", "msg-1", None).unwrap();
        mgr.create_fork("s1", "s3", "msg-2", None).unwrap();

        // s1 is parent of both -- should see both.
        let forks = mgr.list_forks_for_session("s1");
        assert_eq!(forks.len(), 2);

        // s2 is child of one -- should see one.
        let forks = mgr.list_forks_for_session("s2");
        assert_eq!(forks.len(), 1);
    }

    #[test]
    fn list_forks_for_session_empty() {
        let mgr = ForkManager::new();
        assert!(mgr.list_forks_for_session("s1").is_empty());
    }

    // -- tree navigation -------------------------------------------------

    #[test]
    fn get_tree_after_fork() {
        let mut mgr = ForkManager::new();
        let fork_id = mgr.create_fork("s1", "s2", "msg-1", None).unwrap();

        let parent_tree = mgr.get_tree("s1").unwrap();
        assert_eq!(parent_tree.children, vec![fork_id.clone()]);
        assert!(parent_tree.parent.is_none());

        let child_tree = mgr.get_tree("s2").unwrap();
        assert_eq!(child_tree.parent.as_deref(), Some(fork_id.as_str()));
        assert!(child_tree.children.is_empty());
    }

    #[test]
    fn get_tree_missing_returns_none() {
        let mgr = ForkManager::new();
        assert!(mgr.get_tree("ghost").is_none());
    }

    #[test]
    fn get_children_returns_child_session_ids() {
        let mut mgr = ForkManager::new();
        mgr.create_fork("s1", "s2", "msg-1", None).unwrap();
        mgr.create_fork("s1", "s3", "msg-2", None).unwrap();

        let mut children = mgr.get_children("s1");
        children.sort();
        assert_eq!(children, vec!["s2", "s3"]);
    }

    #[test]
    fn get_children_empty_for_leaf() {
        let mut mgr = ForkManager::new();
        mgr.create_fork("s1", "s2", "msg-1", None).unwrap();
        assert!(mgr.get_children("s2").is_empty());
    }

    #[test]
    fn get_children_empty_for_unknown_session() {
        let mgr = ForkManager::new();
        assert!(mgr.get_children("ghost").is_empty());
    }

    #[test]
    fn get_parent_returns_parent_session_id() {
        let mut mgr = ForkManager::new();
        mgr.create_fork("s1", "s2", "msg-1", None).unwrap();

        assert_eq!(mgr.get_parent("s2"), Some("s1".to_string()));
    }

    #[test]
    fn get_parent_none_for_root() {
        let mut mgr = ForkManager::new();
        mgr.create_fork("s1", "s2", "msg-1", None).unwrap();
        assert!(mgr.get_parent("s1").is_none());
    }

    #[test]
    fn get_parent_none_for_unknown() {
        let mgr = ForkManager::new();
        assert!(mgr.get_parent("ghost").is_none());
    }

    // -- get_lineage -----------------------------------------------------

    #[test]
    fn lineage_single_session() {
        let mgr = ForkManager::new();
        assert_eq!(mgr.get_lineage("s1"), vec!["s1"]);
    }

    #[test]
    fn lineage_two_levels() {
        let mut mgr = ForkManager::new();
        mgr.create_fork("root", "child", "msg-1", None).unwrap();

        assert_eq!(mgr.get_lineage("child"), vec!["root", "child"]);
    }

    #[test]
    fn lineage_three_levels() {
        let mut mgr = ForkManager::new();
        mgr.create_fork("root", "mid", "msg-1", None).unwrap();
        mgr.create_fork("mid", "leaf", "msg-2", None).unwrap();

        assert_eq!(mgr.get_lineage("leaf"), vec!["root", "mid", "leaf"]);
    }

    #[test]
    fn lineage_root_is_itself() {
        let mut mgr = ForkManager::new();
        mgr.create_fork("root", "child", "msg-1", None).unwrap();

        assert_eq!(mgr.get_lineage("root"), vec!["root"]);
    }

    // -- delete_fork -----------------------------------------------------

    #[test]
    fn delete_fork_removes_fork() {
        let mut mgr = ForkManager::new();
        let id = mgr.create_fork("s1", "s2", "msg-1", None).unwrap();
        assert_eq!(mgr.fork_count(), 1);

        mgr.delete_fork(&id).unwrap();
        assert_eq!(mgr.fork_count(), 0);
        assert!(mgr.get_fork(&id).is_none());
    }

    #[test]
    fn delete_fork_cleans_tree_references() {
        let mut mgr = ForkManager::new();
        let id = mgr.create_fork("s1", "s2", "msg-1", None).unwrap();

        mgr.delete_fork(&id).unwrap();

        // Parent should have no children.
        let parent_tree = mgr.get_tree("s1").unwrap();
        assert!(parent_tree.children.is_empty());

        // Child should have no parent.
        let child_tree = mgr.get_tree("s2").unwrap();
        assert!(child_tree.parent.is_none());
    }

    #[test]
    fn delete_fork_nonexistent_fails() {
        let mut mgr = ForkManager::new();
        let err = mgr.delete_fork("ghost").unwrap_err();
        assert_eq!(err, ForkError::NotFound("ghost".to_string()));
    }

    // -- counts ----------------------------------------------------------

    #[test]
    fn fork_count_and_session_count() {
        let mut mgr = ForkManager::new();
        assert_eq!(mgr.fork_count(), 0);
        assert_eq!(mgr.session_count(), 0);

        mgr.create_fork("s1", "s2", "msg-1", None).unwrap();
        assert_eq!(mgr.fork_count(), 1);
        assert_eq!(mgr.session_count(), 2);

        mgr.create_fork("s1", "s3", "msg-2", None).unwrap();
        assert_eq!(mgr.fork_count(), 2);
        assert_eq!(mgr.session_count(), 3);
    }

    // -- default / empty -------------------------------------------------

    #[test]
    fn empty_manager() {
        let mgr = ForkManager::new();
        assert_eq!(mgr.fork_count(), 0);
        assert_eq!(mgr.session_count(), 0);
        assert!(mgr.get_fork("x").is_none());
        assert!(mgr.get_tree("x").is_none());
    }

    #[test]
    fn default_trait() {
        let mgr = ForkManager::default();
        assert_eq!(mgr.fork_count(), 0);
    }

    // -- error display ---------------------------------------------------

    #[test]
    fn error_display_messages() {
        assert_eq!(
            ForkError::NotFound("x".into()).to_string(),
            "fork not found: x"
        );
        assert_eq!(
            ForkError::SessionNotFound("s".into()).to_string(),
            "session not found: s"
        );
        assert_eq!(
            ForkError::SameSession.to_string(),
            "cannot fork: parent and child must be different"
        );
        assert_eq!(
            ForkError::DuplicateFork.to_string(),
            "duplicate fork: already exists between these sessions"
        );
    }
}
