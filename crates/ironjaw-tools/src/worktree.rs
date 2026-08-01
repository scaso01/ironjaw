//! Git worktree isolation for agents. Creates temporary worktrees
//! for blast-radius containment during destructive operations.

use std::path::{Path, PathBuf};

pub struct WorktreeManager {
    repo_root: PathBuf,
    worktree_base: PathBuf,
}

impl WorktreeManager {
    pub fn new(repo_root: impl AsRef<Path>, worktree_base: impl AsRef<Path>) -> Self {
        Self {
            repo_root: repo_root.as_ref().to_path_buf(),
            worktree_base: worktree_base.as_ref().to_path_buf(),
        }
    }

    pub fn worktree_path(&self, agent_id: &str) -> PathBuf {
        self.worktree_base.join(agent_id)
    }

    pub fn create_command(&self, agent_id: &str, branch: &str) -> String {
        let wt_path = self.worktree_path(agent_id);
        format!(
            "cd {} && git worktree add {} -b {}",
            self.repo_root.display(),
            wt_path.display(),
            branch,
        )
    }

    pub fn cleanup_command(&self, agent_id: &str) -> String {
        let wt_path = self.worktree_path(agent_id);
        format!(
            "cd {} && git worktree remove {} --force",
            self.repo_root.display(),
            wt_path.display(),
        )
    }

    /// Create a worktree for an agent. Returns the worktree path.
    pub async fn create(&self, agent_id: &str, branch: &str) -> Result<PathBuf, String> {
        let cmd = self.create_command(agent_id, branch);
        let output = tokio::process::Command::new("bash")
            .args(["-c", &cmd])
            .output()
            .await
            .map_err(|e| format!("Failed to create worktree: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("Worktree creation failed: {stderr}"));
        }

        Ok(self.worktree_path(agent_id))
    }

    /// Remove a worktree after agent completes. Checks for uncommitted changes first.
    pub async fn cleanup(&self, agent_id: &str, force: bool) -> Result<bool, String> {
        let wt_path = self.worktree_path(agent_id);

        if !force {
            // Check for uncommitted changes (including submodules)
            let check = format!(
                "cd {} && git status --porcelain --ignore-submodules=none",
                wt_path.display()
            );
            let output = tokio::process::Command::new("bash")
                .args(["-c", &check])
                .output()
                .await
                .map_err(|e| format!("Failed to check worktree status: {e}"))?;

            let status = String::from_utf8_lossy(&output.stdout);
            if !status.trim().is_empty() {
                return Ok(false); // Has uncommitted changes, don't clean up
            }
        }

        let cmd = self.cleanup_command(agent_id);
        let output = tokio::process::Command::new("bash")
            .args(["-c", &cmd])
            .output()
            .await
            .map_err(|e| format!("Failed to cleanup worktree: {e}"))?;

        Ok(output.status.success())
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub fn worktree_base(&self) -> &Path {
        &self.worktree_base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worktree_path_generation() {
        let mgr = WorktreeManager::new("/repo", "/tmp/worktrees");
        let path = mgr.worktree_path("agent-abc");
        assert!(path.ends_with("agent-abc"));
        assert!(path.starts_with("/tmp/worktrees"));
    }

    #[test]
    fn test_worktree_create_command() {
        let mgr = WorktreeManager::new("/repo", "/tmp/worktrees");
        let cmd = mgr.create_command("agent-123", "feature-branch");
        assert!(cmd.contains("git worktree add"));
        assert!(cmd.contains("agent-123"));
        assert!(cmd.contains("feature-branch"));
    }

    #[test]
    fn test_worktree_cleanup_command() {
        let mgr = WorktreeManager::new("/repo", "/tmp/worktrees");
        let cmd = mgr.cleanup_command("agent-123");
        assert!(cmd.contains("git worktree remove"));
        assert!(cmd.contains("agent-123"));
    }

    #[test]
    fn test_worktree_path_different_agents() {
        let mgr = WorktreeManager::new("/repo", "/tmp/worktrees");
        let path_a = mgr.worktree_path("agent-a");
        let path_b = mgr.worktree_path("agent-b");
        assert_ne!(path_a, path_b);
    }

    #[test]
    fn test_repo_root_and_base_accessors() {
        let mgr = WorktreeManager::new("/my/repo", "/tmp/wt");
        assert_eq!(mgr.repo_root(), Path::new("/my/repo"));
        assert_eq!(mgr.worktree_base(), Path::new("/tmp/wt"));
    }

    #[test]
    fn test_create_command_contains_all_parts() {
        let mgr = WorktreeManager::new("/home/user/project", "/tmp/agents");
        let cmd = mgr.create_command("worker-1", "feat/task-1");
        assert!(cmd.starts_with("cd /home/user/project && git worktree add"));
        assert!(cmd.contains("worker-1"));
        assert!(cmd.contains("-b feat/task-1"));
    }

    #[test]
    fn test_cleanup_command_contains_all_parts() {
        let mgr = WorktreeManager::new("/home/user/project", "/tmp/agents");
        let cmd = mgr.cleanup_command("worker-1");
        assert!(cmd.starts_with("cd /home/user/project && git worktree remove"));
        assert!(cmd.contains("worker-1"));
        assert!(cmd.contains("--force"));
    }
}
