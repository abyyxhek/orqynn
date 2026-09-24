//! Observed state of the project on disk and in git (Phase 6).
//!
//! Everything here is **observed, never remembered**. Director does not trust
//! memory for this: [`ProjectState`] is rebuilt from git and the filesystem
//! every time it needs to know "what is actually true now". Comparing a
//! checkpoint's recorded state against a fresh observation is what produces
//! [`StateComparison::StateChanged`] — the trigger that makes a checkpoint
//! stale and blocks a blind resume.

use serde::{Deserialize, Serialize};

use crate::ids::{CheckpointId, MachineId};

/// A commit as reported by git.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    /// Full 40-character SHA.
    pub sha: String,
    /// First line of the commit message.
    pub summary: String,
    /// The commit's author.
    pub author: String,
    /// When the commit was made.
    pub committed_at: chrono::DateTime<chrono::Utc>,
}

/// One entry in the working tree status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// The changed path, repository-relative.
    pub path: String,
    /// What kind of change git reported.
    pub change: FileChange,
}

/// The kind of working-tree change git reports for one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    /// A new file.
    Added,
    /// A changed existing file.
    Modified,
    /// A removed file.
    Deleted,
    /// A moved or renamed file.
    Renamed,
    /// A file git is not tracking.
    Untracked,
    /// A path with unresolved merge markers.
    Conflicted,
}

/// Outcome of a test run Director observed (not an agent's claim).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestResults {
    /// Tests that succeeded.
    pub passed: u32,
    /// Tests that did not.
    pub failed: u32,
    /// Tests not run — filtered or ignored.
    pub skipped: u32,
    /// Names of failing tests, capped at a bounded number so a broken suite
    /// cannot flood recent context.
    pub failures: Vec<String>,
    /// When the suite ran.
    pub run_at: chrono::DateTime<chrono::Utc>,
}

/// The observed state of the project at a moment in time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectState {
    /// Current checked-out branch, or `None` in a detached HEAD.
    pub branch: Option<String>,
    /// The commit `HEAD` points at.
    pub head_commit: String,
    /// Working tree status. Empty means clean.
    pub working_tree: Vec<ChangedFile>,
    /// Most recent commits, newest first, bounded.
    pub recent_commits: Vec<CommitInfo>,
    /// Last test results Director observed itself, if any.
    pub test_results: Option<TestResults>,
    /// Machine the observation was made on.
    pub observed_on: MachineId,
    /// When this observation was made.
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

impl ProjectState {
    /// Whether the working tree has uncommitted changes.
    pub fn is_dirty(&self) -> bool {
        !self.working_tree.is_empty()
    }

    /// Paths changed in the working tree, in arbitrary order.
    pub fn changed_paths(&self) -> Vec<&str> {
        self.working_tree.iter().map(|c| c.path.as_str()).collect()
    }
}

/// How a stored checkpoint compares against freshly observed project state.
///
/// Produced by [`crate::checkpoint::Checkpoint::compare_with`]. This is the
/// decision that protects resume: if the world moved on, the checkpoint is
/// stale and Director must say so rather than hand a new agent outdated facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateComparison {
    /// Same commit, clean tree. The checkpoint is still an accurate picture.
    Unchanged,
    /// Same commit but the working tree changed. Work happened since; the
    /// checkpoint may still be usable but its `changed_files` is stale.
    WorkingTreeChanged,
    /// `HEAD` advanced. Someone committed since the checkpoint. Resume must
    /// reconcile, never blindly apply.
    HeadAdvanced,
    /// The branch itself changed. Not the same line of work.
    BranchChanged,
    /// The commit the checkpoint recorded is gone from history (rebase,
    /// force-push, amend). The checkpoint is unreliable.
    CommitGone,
}

impl StateComparison {
    /// True if the checkpoint can be trusted as a picture of current reality.
    pub fn checkpoint_is_current(self) -> bool {
        matches!(self, StateComparison::Unchanged)
    }

    /// True if Director must re-derive task state before resuming.
    pub fn requires_reconciliation(self) -> bool {
        !self.checkpoint_is_current()
    }
}

/// Records which checkpoint was current when an observation was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStateRef {
    /// The checkpoint current when the observation was made.
    pub checkpoint_id: CheckpointId,
    /// The observed state itself.
    pub state: ProjectState,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(commit: &str) -> ProjectState {
        ProjectState {
            branch: Some("main".into()),
            head_commit: commit.into(),
            working_tree: vec![],
            recent_commits: vec![],
            test_results: None,
            observed_on: MachineId::from_string("MACH-a"),
            observed_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn clean_state_is_not_dirty() {
        assert!(!state("a").is_dirty());
    }

    #[test]
    fn dirty_state_reports_changed_paths() {
        let mut s = state("a");
        s.working_tree.push(ChangedFile {
            path: "src/lib.rs".into(),
            change: FileChange::Modified,
        });
        assert!(s.is_dirty());
        assert_eq!(s.changed_paths(), vec!["src/lib.rs"]);
    }

    #[test]
    fn unchanged_is_the_only_current_comparison() {
        assert!(StateComparison::Unchanged.checkpoint_is_current());
        assert!(!StateComparison::WorkingTreeChanged.checkpoint_is_current());
        assert!(!StateComparison::HeadAdvanced.checkpoint_is_current());
        assert!(!StateComparison::BranchChanged.checkpoint_is_current());
        assert!(!StateComparison::CommitGone.checkpoint_is_current());
    }

    #[test]
    fn every_non_unchanged_comparison_requires_reconciliation() {
        assert!(!StateComparison::Unchanged.requires_reconciliation());
        assert!(StateComparison::WorkingTreeChanged.requires_reconciliation());
        assert!(StateComparison::HeadAdvanced.requires_reconciliation());
        assert!(StateComparison::BranchChanged.requires_reconciliation());
        assert!(StateComparison::CommitGone.requires_reconciliation());
    }
}
