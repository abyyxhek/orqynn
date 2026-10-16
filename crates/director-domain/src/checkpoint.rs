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
//! checkpoint concept at all. This is Orqyn's own.

use serde::{Deserialize, Serialize};

use crate::assignment::AgentAssignment;
use crate::decision::Decision;
use crate::ids::{CheckpointId, DecisionId, TaskId};
use crate::state::{StateComparison, TestResults};
use crate::store::TaskStatusTransition;
use crate::verification::Verification;

/// Bumped whenever the checkpoint shape changes in a way a reader must handle.
///
/// A checkpoint written by an older Orqyn is not necessarily wrong, but a
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
    /// The project's state version when the checkpoint was taken, if known.
    /// Pairs with [`Checkpoint::task_state_version`] to answer "was the world
    /// this checkpoint describes still the world when it is read".
    #[serde(default)]
    pub project_state_version: Option<u64>,
    /// The task's state version when the checkpoint was taken, if known. If the
    /// task has since moved on, the checkpoint describes work that is no longer
    /// the current effort, and a reader must reconcile rather than apply.
    #[serde(default)]
    pub task_state_version: Option<u64>,
    /// Where this checkpoint sits in the task's recovery lineage. Only the
    /// latest checkpoint for a task is [`CheckpointStatus::Current`]; earlier
    /// ones are retained, never deleted, so the task's recovery history stays
    /// answerable.
    #[serde(default)]
    pub status: CheckpointStatus,
    /// Monotonic version for optimistic concurrency — see
    /// [`crate::project::Project::state_version`].
    #[serde(default = "default_state_version")]
    pub state_version: u64,
    /// When the checkpoint was taken.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Agent and session that produced the checkpoint, for attribution only —
    /// never used to decide who resumes it.
    pub created_by_agent: Option<crate::ids::AgentId>,
    /// The session that produced it, for attribution only.
    pub created_by_session: Option<crate::ids::SessionId>,
}

/// Where a checkpoint sits in a task's recovery lineage.
///
/// Defaults to [`CheckpointStatus::Current`]: a payload written before Phase 4
/// predates the notion of supersession, so it was the latest checkpoint for its
/// task when it was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointStatus {
    /// The newest checkpoint for its task. The one a resume uses.
    #[default]
    Current,
    /// Superseded by a newer checkpoint for the same task. Retained for
    /// history; not used for recovery.
    Superseded,
}

/// The version a freshly persisted record starts on. One, not zero: a record
/// that exists has been written once.
fn default_state_version() -> u64 {
    1
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
            project_state_version: None,
            task_state_version: None,
            status: CheckpointStatus::Current,
            state_version: default_state_version(),
            created_at: chrono::Utc::now(),
            created_by_agent: None,
            created_by_session: None,
        }
    }

    /// Mark this checkpoint as no longer the latest for its task. A new
    /// checkpoint supersedes the previous one rather than replacing it, so the
    /// recovery history stays intact.
    pub fn supersede(&mut self) {
        self.status = CheckpointStatus::Superseded;
        self.state_version += 1;
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
///
/// Every field comes from records Orqyn persists. The checkpoint is the
/// narrative; the histories around it are the answers to the questions a
/// replacement actually asks — who held this before me and how did that end,
/// what has Orqyn already judged about this work, and what did the task's
/// lifecycle look like. A field that cannot be reconstructed from persisted
/// state is a field reported as empty, never a field filled with a guess.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContinuationPackage {
    /// The task to resume.
    pub task_id: TaskId,
    /// The task's state version when the package was built. A replacement that
    /// writes back presents this version, so its write is refused if anything
    /// moved the task in between rather than silently clobbering it.
    pub task_state_version: u64,
    /// What this task may touch, copied verbatim from the authoritative persisted
    /// task — never inferred from an agent's message or from the files a
    /// checkpoint happens to mention.
    ///
    /// This exists because conflict detection is Orqynn's alone. Enforcement reads
    /// the task row, so a missing field could never put two agents on the same
    /// files; but a replacement agent is handed this package and nothing else, and
    /// an agent that does not know which paths are its own will edit whatever is
    /// nearest. The briefing was incomplete about exactly the thing the agent must
    /// not get wrong.
    ///
    /// **Empty means the scope is undeclared, not unrestricted.** See
    /// [`crate::task::Task::scope_overlap`]: an undeclared scope overlaps
    /// everything, including another undeclared one. So a reader that finds this
    /// empty must ask *why* — either the planner declared no scope, or this
    /// package was serialized by a build that predates scope reporting. Both are
    /// real, and neither is resolved by guessing.
    ///
    /// Paths are stored **as the task declared them**, in declaration order, and
    /// are not normalized here. Normalization is a comparison concern and belongs
    /// to [`crate::task::normalize_scope_paths`], so this field stays a faithful
    /// copy that a reader can match against the task's own declaration.
    ///
    /// `#[serde(default)]` because the field was added to an existing serialized
    /// type, following this crate's convention for additive fields: a package
    /// written before scope reporting must stay readable rather than failing to
    /// deserialize. Defaulting to empty is the conservative direction — an absent
    /// scope is treated as undeclared, which is the assumption least likely to let
    /// an agent collide with another.
    #[serde(default)]
    pub scope_paths: Vec<String>,
    /// The latest checkpoint for that task.
    pub checkpoint: Checkpoint,
    /// The task's tenure history, newest first. This is what answers "what
    /// happened to the previous agent" — each entry carries how its tenure
    /// ended, so a replacement knows whether it is following a clean handoff,
    /// an expiry, or a failure.
    pub tenures: Vec<AgentAssignment>,
    /// The verification history with its evidence, newest first. A replacement
    /// that does not know what already failed repeats it, so the judgments are
    /// part of the briefing rather than a query the replacement must remember
    /// to make.
    pub verifications: Vec<Verification>,
    /// The task's lifecycle transitions. The most recent entry is what changed
    /// last, which is how a replacement reads "what was the previous state".
    pub transitions: Vec<TaskStatusTransition>,
    /// Decisions a replacement must honor, so a resumption does not re-litigate
    /// a question the project already settled.
    pub decisions: Vec<Decision>,
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
    /// Orqyn's verdict on whether resumption can proceed.
    pub resume_status: ResumeStatus,
}

/// Orqyn's verdict on whether a task can be resumed as-is (Phase 12).
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
    /// Orqyn does not have enough information to resume safely — no
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

    #[test]
    fn a_new_checkpoint_is_current_at_version_one() {
        let cp = Checkpoint::new(
            CheckpointId::from_string("CHK-1"),
            TaskId::from_string("AUTH-42"),
            "auth",
            "p",
            "n",
        );
        assert_eq!(cp.status, CheckpointStatus::Current);
        assert_eq!(cp.state_version, 1);
        assert!(cp.project_state_version.is_none());
        assert!(cp.task_state_version.is_none());
    }

    #[test]
    fn superseding_a_checkpoint_marks_it_and_advances_the_version() {
        let mut cp = Checkpoint::new(
            CheckpointId::from_string("CHK-1"),
            TaskId::from_string("AUTH-42"),
            "auth",
            "p",
            "n",
        );
        assert_eq!(cp.status, CheckpointStatus::Current);
        cp.supersede();
        assert_eq!(cp.status, CheckpointStatus::Superseded);
        assert_eq!(cp.state_version, 2);
    }

    #[test]
    fn a_pre_phase4_checkpoint_payload_still_deserializes() {
        // A payload written before Phase 4 has none of the new fields. All four
        // default, so an old checkpoint still reads.
        let legacy = serde_json::json!({
            "id": "CHK-1",
            "task_id": "AUTH-42",
            "objective": "auth",
            "progress": "p",
            "progress_fraction": 0.0,
            "branch": null,
            "commit_sha": null,
            "changed_files": [],
            "test_results": null,
            "current_blocker": null,
            "important_decisions": [],
            "current_assumptions": [],
            "next_action": "n",
            "context_version": 1,
            "created_at": "2026-01-01T00:00:00Z",
            "created_by_agent": null,
            "created_by_session": null
        });
        let parsed: Checkpoint = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.state_version, 1);
        assert_eq!(parsed.status, CheckpointStatus::Current);
        assert!(parsed.project_state_version.is_none());
        assert!(parsed.task_state_version.is_none());
    }

    // -----------------------------------------------------------------------
    // ContinuationPackage scope
    // -----------------------------------------------------------------------

    /// A package with only the fields that matter to these tests; the rest are
    /// empty histories, which are all legal.
    fn package_with_scope(scope_paths: Vec<String>) -> ContinuationPackage {
        ContinuationPackage {
            task_id: TaskId::from_string("AUTH-42"),
            task_state_version: 3,
            checkpoint: Checkpoint::new(
                CheckpointId::from_string("CHK-1"),
                TaskId::from_string("AUTH-42"),
                "auth",
                "p",
                "n",
            ),
            scope_paths,
            tenures: vec![],
            verifications: vec![],
            transitions: vec![],
            decisions: vec![],
            recent_context: crate::context::RecentContext::for_task(TaskId::from_string("AUTH-42")),
            observed_state: observed("abc123", Some("main")),
            comparison: StateComparison::Unchanged,
            active_agents: vec![],
            blockers: vec![],
            next_action: "n".into(),
            resume_status: ResumeStatus::Ready,
        }
    }

    #[test]
    fn a_continuation_package_carries_its_task_scope_through_serde() {
        let package = package_with_scope(vec![
            "src/auth".to_string(),
            "src/auth/login.rs".to_string(),
        ]);

        let json = serde_json::to_string(&package).expect("the package serializes");
        let parsed: ContinuationPackage =
            serde_json::from_str(&json).expect("the package deserializes");

        assert_eq!(
            parsed.scope_paths,
            vec!["src/auth".to_string(), "src/auth/login.rs".to_string()],
            "a replacement agent must be told which paths are its own, in the order the \
             task declared them"
        );
        assert_eq!(parsed, package, "the whole package round-trips unchanged");
    }

    #[test]
    fn an_empty_scope_survives_serde_as_empty() {
        // Empty is not a missing value and not a bug: it is the *undeclared*
        // scope, which conflicts with everything. It has to survive the round trip
        // as empty rather than being filled in or dropped.
        let package = package_with_scope(vec![]);
        let json = serde_json::to_string(&package).expect("the package serializes");
        let parsed: ContinuationPackage =
            serde_json::from_str(&json).expect("the package deserializes");

        assert!(
            parsed.scope_paths.is_empty(),
            "an undeclared scope must stay undeclared across a handoff"
        );
        assert_eq!(parsed, package);
    }

    #[test]
    fn a_package_written_before_scope_reporting_still_deserializes() {
        // The compatibility case: a payload from a build that predates the field.
        // It has to keep reading, and it has to read *conservatively* — an absent
        // scope is treated as undeclared rather than as unrestricted.
        let mut legacy = serde_json::to_value(package_with_scope(vec!["src/auth".to_string()]))
            .expect("the package serializes");
        legacy
            .as_object_mut()
            .expect("the payload is an object")
            .remove("scope_paths");

        let parsed: ContinuationPackage =
            serde_json::from_value(legacy).expect("an older package still reads");
        assert!(
            parsed.scope_paths.is_empty(),
            "a package with no scope field defaults to undeclared, which is the \
             conservative direction: it cannot let an agent collide with another"
        );
    }
}
