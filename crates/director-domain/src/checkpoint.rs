//! [Checkpoint] — the mechanism that makes work survive (Phase 5).
//!
//! A checkpoint is a **deliberate, explicit snapshot** of everything needed to
//! continue a task after context-window exhaustion, a session crash, an agent
//! crash, a machine shutdown, or a cloud session expiring. It is not a
//! conversation summary and it is not a session page: it is a resumption
//! document addressed to whatever agent picks the task up next.
//!
//! Neither substrate has anything like this. ai-memory's `Handoff` row is a
//! session-scoped summary produced on `SessionEnd`; handoff-mcp has no
//! checkpoint concept at all. This is Director's own.

use serde::{Deserialize, Serialize};

use crate::ids::{CheckpointId, DecisionId, TaskId};
use crate::state::{StateComparison, TestResults};

/// Bumped whenever the checkpoint shape changes in a way a reader must handle.
///
/// A checkpoint written by an older Director is not necessarily wrong, but a
/// reader that disagrees on the version must treat the checkpoint as
/// *advisory* rather than authoritative — it cannot assume fields it does not
/// know about are absent for a good reason.
pub const CHECKPOINT_FORMAT_VERSION: u32 = 1;

/// Everything required to continue a task on a fresh agent, machine, and
/// harness.
///
/// `PartialEq` but not `Eq`: `progress_fraction` is an `f32`, so total equality
/// is not well defined. Comparisons are for test assertions.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    /// This checkpoint's identifier.
    pub id: CheckpointId,
    /// The task this checkpoint resumes.
    pub task_id: TaskId,
    /// The objective as understood when the checkpoint was taken. Recorded
    /// here rather than looked up so the checkpoint stays self-contained even
    /// if the task's objective is later edited.
    pub objective: String,
    /// Free-text progress narrative: what was accomplished, in order.
    pub progress: String,
    /// How far along the task is, 0.0–1.0. Derived from `expected_outputs`
    /// where possible; never an agent's own guess when a measured value exists.
    pub progress_fraction: f32,

    // --- Project state at checkpoint time ---
    /// Branch at checkpoint time, if known.
    pub branch: Option<String>,
    /// The commit `HEAD` pointed at, if known.
    pub commit_sha: Option<String>,
    /// Working-tree changes recorded at checkpoint time.
    pub changed_files: Vec<String>,
    /// The last test results observed at checkpoint time.
    pub test_results: Option<TestResults>,

    // --- What the next agent needs to know ---
    /// What is in the way, if anything.
    pub current_blocker: Option<String>,
    /// Decisions the next agent must honor.
    pub important_decisions: Vec<DecisionId>,
    /// Things assumed true when the checkpoint was taken. On resume, each
    /// assumption is a candidate staleness check.
    pub current_assumptions: Vec<String>,
    /// The concrete first step for the next agent.
    pub next_action: String,

    /// Format version of the checkpoint payload.
    pub context_version: u32,
    /// When the checkpoint was taken.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Agent and session that produced the checkpoint, for attribution only —
    /// never used to decide who resumes it.
    pub created_by_agent: Option<crate::ids::AgentId>,
    /// The session that produced it, for attribution only.
    pub created_by_session: Option<crate::ids::SessionId>,
}

impl Checkpoint {
    /// Take a checkpoint of a task.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: CheckpointId,
        task_id: TaskId,
        objective: impl Into<String>,
        progress: impl Into<String>,
        next_action: impl Into<String>,
    ) -> Self {
        Checkpoint {
            id,
            task_id,
            objective: objective.into(),
            progress: progress.into(),
            progress_fraction: 0.0,
            branch: None,
            commit_sha: None,
            changed_files: vec![],
            test_results: None,
            current_blocker: None,
            important_decisions: vec![],
            current_assumptions: vec![],
            next_action: next_action.into(),
            context_version: CHECKPOINT_FORMAT_VERSION,
            created_at: chrono::Utc::now(),
            created_by_agent: None,
            created_by_session: None,
        }
    }

    /// Whether the checkpoint records the same commit as observed state.
    ///
    /// The cheap and decisive test. It is deliberately conservative: it only
    /// answers "am I definitely still current", leaving every other case to
    /// [`compare_with`], which can distinguish a tree change from a head
    /// advance.
    pub fn matches_commit(&self, observed_sha: &str) -> bool {
        self.commit_sha.as_deref() == Some(observed_sha)
    }

    /// Compare this checkpoint against freshly observed project state.
    ///
    /// This is the guard on every resume (Phase 12). If the world moved on,
    /// the caller must reconcile rather than hand a new agent outdated facts.
    /// The checkpoint records a commit it cannot find in the observed history
    /// at all, that is [`StateComparison::CommitGone`] — a rebase or amend
    /// invalidated the checkpoint's frame of reference.
    pub fn compare_with(
        &self,
        observed: &crate::state::ProjectState,
        observed_history: &[crate::state::CommitInfo],
    ) -> StateComparison {
        let observed_sha = observed.head_commit.as_str();

        match &self.commit_sha {
            None => {
                // A checkpoint with no recorded commit cannot be compared
                // reliably. Treat it as needing reconciliation rather than
                // silently trusting it.
                if observed.is_dirty() {
                    StateComparison::WorkingTreeChanged
                } else {
                    StateComparison::Unchanged
                }
            }
            Some(recorded) if recorded == observed_sha => {
                if observed.is_dirty() {
                    StateComparison::WorkingTreeChanged
                } else {
                    StateComparison::Unchanged
                }
            }
            Some(recorded) => {
                // Different commit. Is our commit still in history at all?
                let still_present = observed_history.iter().any(|c| c.sha == *recorded);

                if !still_present {
                    StateComparison::CommitGone
                } else if self.branch.as_deref() != observed.branch.as_deref() {
                    StateComparison::BranchChanged
                } else {
                    StateComparison::HeadAdvanced
                }
            }
        }
    }
}

/// A resumption document: the latest checkpoint for a task plus enough
/// surrounding context for a new agent to continue (Phase 12).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContinuationPackage {
    /// The task to resume.
    pub task_id: TaskId,
    /// The latest checkpoint for that task.
    pub checkpoint: Checkpoint,
    /// The bounded activity window.
    pub recent_context: crate::context::RecentContext,
    /// Freshly observed project state.
    pub observed_state: crate::state::ProjectState,
    /// How the checkpoint compares to that state.
    pub comparison: StateComparison,
    /// Agents currently working in the project.
    pub active_agents: Vec<crate::ids::AgentId>,
    /// Open blockers on the task.
    pub blockers: Vec<crate::ids::BlockerId>,
    /// The concrete first step for the receiving agent.
    pub next_action: String,
    /// Director's verdict on whether resumption can proceed.
    pub resume_status: ResumeStatus,
}

/// Director's verdict on whether a task can be resumed as-is (Phase 12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeStatus {
    /// Checkpoint is current, context is live, no blockers. Go.
    Ready,
    /// The checkpoint describes a world that has moved on. Reconcile first.
    Stale,
    /// Two sources disagree about the task's state (e.g. substrate says done,
    /// verification says failed). A human or the planner must adjudicate.
    Conflicted,
    /// A blocker stands in the way.
    Blocked,
    /// Director does not have enough information to resume safely — no
    /// checkpoint, no agent, or missing project state.
    Insufficient,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::MachineId;
    use crate::state::{ChangedFile, CommitInfo, FileChange, ProjectState};

    fn observed(commit: &str, branch: Option<&str>) -> ProjectState {
        ProjectState {
            branch: branch.map(|b| b.to_string()),
            head_commit: commit.to_string(),
            working_tree: vec![],
            recent_commits: vec![],
            test_results: None,
            observed_on: MachineId::from_string("MACH-a"),
            observed_at: chrono::Utc::now(),
        }
    }

    fn history(shas: &[&str]) -> Vec<CommitInfo> {
        shas.iter()
            .map(|sha| CommitInfo {
                sha: sha.to_string(),
                summary: "c".into(),
                author: "a".into(),
                committed_at: chrono::Utc::now(),
                parents: vec![],
                committer: "a".into(),
                message: "c".into(),
            })
            .collect()
    }

    #[test]
    fn a_checkpoint_of_the_current_head_on_a_clean_tree_is_unchanged() {
        let mut cp = Checkpoint::new(
            CheckpointId::from_string("CHK-1"),
            TaskId::from_string("AUTH-42"),
            "auth",
            "did a thing",
            "write tests",
        );
        cp.commit_sha = Some("abc".into());
        cp.branch = Some("main".into());

        let state = observed("abc", Some("main"));
        assert_eq!(
            cp.compare_with(&state, &history(&["abc"])),
            StateComparison::Unchanged
        );
        assert!(cp
            .compare_with(&state, &history(&["abc"]))
            .checkpoint_is_current());
    }

    #[test]
    fn a_dirty_tree_on_the_same_head_is_a_working_tree_change() {
        let mut cp = Checkpoint::new(
            CheckpointId::from_string("CHK-1"),
            TaskId::from_string("AUTH-42"),
            "auth",
            "did a thing",
            "write tests",
        );
        cp.commit_sha = Some("abc".into());

        let mut state = observed("abc", Some("main"));
        state.working_tree.push(ChangedFile {
            path: "src/lib.rs".into(),
            change: FileChange::Modified,
        });

        assert_eq!(
            cp.compare_with(&state, &history(&["abc"])),
            StateComparison::WorkingTreeChanged
        );
        assert!(cp
            .compare_with(&state, &history(&["abc"]))
            .requires_reconciliation());
    }

    #[test]
    fn an_advanced_head_is_detected_when_the_old_commit_remains() {
        let mut cp = Checkpoint::new(
            CheckpointId::from_string("CHK-1"),
            TaskId::from_string("AUTH-42"),
            "auth",
            "did a thing",
            "write tests",
        );
        cp.commit_sha = Some("abc".into());
        cp.branch = Some("main".into());

        let state = observed("def", Some("main"));
        // abc is still in history, branch unchanged → head advanced.
        assert_eq!(
            cp.compare_with(&state, &history(&["def", "abc"])),
            StateComparison::HeadAdvanced
        );
    }

    #[test]
    fn a_rebased_away_commit_is_detected_as_gone() {
        let mut cp = Checkpoint::new(
            CheckpointId::from_string("CHK-1"),
            TaskId::from_string("AUTH-42"),
            "auth",
            "did a thing",
            "write tests",
        );
        cp.commit_sha = Some("abc".into());
        cp.branch = Some("main".into());

        let state = observed("def", Some("main"));
        // abc is absent from history → rebased/amended away.
        assert_eq!(
            cp.compare_with(&state, &history(&["def", "xyz"])),
            StateComparison::CommitGone
        );
    }

    #[test]
    fn a_branch_change_is_detected() {
        let mut cp = Checkpoint::new(
            CheckpointId::from_string("CHK-1"),
            TaskId::from_string("AUTH-42"),
            "auth",
            "did a thing",
            "write tests",
        );
        cp.commit_sha = Some("abc".into());
        cp.branch = Some("main".into());

        let state = observed("def", Some("feature/auth"));
        assert_eq!(
            cp.compare_with(&state, &history(&["def", "abc"])),
            StateComparison::BranchChanged
        );
    }

    #[test]
    fn a_checkpoint_with_no_recorded_commit_needs_reconciliation_when_dirty() {
        let cp = Checkpoint::new(
            CheckpointId::from_string("CHK-1"),
            TaskId::from_string("AUTH-42"),
            "auth",
            "did a thing",
            "write tests",
        );

        let mut state = observed("abc", Some("main"));
        state.working_tree.push(ChangedFile {
            path: "x".into(),
            change: FileChange::Added,
        });
        assert_eq!(
            cp.compare_with(&state, &[]),
            StateComparison::WorkingTreeChanged
        );
    }

    #[test]
    fn matches_commit_is_decisive_and_conservative() {
        let mut cp = Checkpoint::new(
            CheckpointId::from_string("CHK-1"),
            TaskId::from_string("AUTH-42"),
            "auth",
            "p",
            "n",
        );
        assert!(!cp.matches_commit("abc")); // no recorded commit
        cp.commit_sha = Some("abc".into());
        assert!(cp.matches_commit("abc"));
        assert!(!cp.matches_commit("def"));
    }

    #[test]
    fn checkpoint_round_trips_through_serde() {
        let mut cp = Checkpoint::new(
            CheckpointId::from_string("CHK-1"),
            TaskId::from_string("AUTH-42"),
            "auth",
            "did a thing",
            "write tests",
        );
        cp.commit_sha = Some("abc".into());
        cp.branch = Some("main".into());
        cp.changed_files = vec!["src/lib.rs".into()];
        cp.current_assumptions = vec!["schema is stable".into()];
        cp.important_decisions = vec![DecisionId::from_string("DEC-1")];

        let json = serde_json::to_string(&cp).unwrap();
        let back: Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(cp, back);
        assert_eq!(back.context_version, CHECKPOINT_FORMAT_VERSION);
    }
}
