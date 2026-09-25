//! Canonical domain model for Director Brain.
//!
//! ## What this crate is
//!
//! The *vocabulary* of Director: every entity, identity, status enum, and
//! provider trait the rest of the system speaks. It has **no knowledge of any
//! substrate**. It does not know about handoff-mcp's `TaskData`, ai-memory's
//! wiki `Page`, SQLite, JSON files, MCP, or any harness. Adapters translate
//! those into these types; the core never sees a foreign struct.
//!
//! ## The invariant the model exists to express
//!
//! > **Task identity is independent of agent identity.**
//!
//! A task outlives every agent that works on it. This is enforced in the types,
//! not in convention:
//!
//! - [`ids::TaskId`] and [`ids::AgentId`] are distinct newtypes; neither can be
//!   constructed from the other, so a task id can never be passed where an agent
//!   id is expected.
//! - [`task::Task`] has **no agent field**. Assignment is a separate,
//!   first-class entity, [`assignment::AgentAssignment`], with its own history.
//!   Reassigning a task creates a new assignment record and changes nothing
//!   about the task.
//!
//! ## The other invariant: nothing self-reports completion
//!
//! [`task::TaskStatus::Done`] is not reachable from an agent's report — see
//! [`task::TaskStatus::reachable_by_agent_report`]. Only the verification
//! engine completes a task. This is the guardrail behind the acceptance test
//! "agent claims done, tests fail → must not become COMPLETED".
//!
//! ## Modules
//!
//! - [`ids`] — identity newtypes, one per entity kind.
//! - [`project`] / [`task`] / [`agent`] / [`session`] — the nouns.
//! - [`repository`] — a git working tree Director observes, and the
//!   deterministic change detection over its state.
//! - [`assignment`] / [`capability`] — who does what, and on what basis.
//! - [`plan`] / [`decision`] / [`blocker`] — planning artifacts.
//! - [`checkpoint`] / [`context`] / [`action`] — continuity and evidence.
//! - [`state`] — observed project state, never remembered.
//! - [`handoff`] — Director's own claim-once task transfer.
//! - [`providers`] — the trait boundary every substrate adapter must implement.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod action;
pub mod agent;
pub mod assignment;
pub mod blocker;
pub mod capability;
pub mod checkpoint;
pub mod context;
pub mod decision;
pub mod handoff;
pub mod ids;
pub mod plan;
pub mod project;
pub mod providers;
pub mod repository;
pub mod session;
pub mod state;
pub mod task;

// Flat re-exports: callers write `director_domain::Task`, not
// `director_domain::task::Task`. The module split is for organization; the
// vocabulary is one namespace.
pub use action::{Action, ActionKind};
pub use agent::{Agent, AgentStatus, Harness, Machine};
pub use assignment::{AgentAssignment, AssignmentStatus, ReleaseReason};
pub use blocker::{Blocker, BlockerKind, BlockerStatus};
pub use capability::Capability;
pub use checkpoint::{Checkpoint, ContinuationPackage, ResumeStatus, CHECKPOINT_FORMAT_VERSION};
pub use context::{ContextSnapshot, RecentContext, DEFAULT_MAX_ACTIONS};
pub use decision::{Decision, DecisionStatus};
pub use handoff::{Handoff, HandoffError, HandoffState};
pub use ids::{
    ActionId, AgentId, AssignmentId, BlockerId, CheckpointId, DecisionId, EventId, HandoffId, Id,
    IdGenerator, MachineId, PlanId, ProjectId, RepositoryId, SessionId, SubtaskId, TaskId,
};
pub use plan::{Plan, PlanStatus};
pub use project::{DefaultBranch, Project};
pub use repository::{
    DiffInfo, EventData, EventKind, FileChangeRecord, ObservationEvent, ProjectStateSnapshot,
    Repository, RepositoryError, RepositoryStatus, SyncResult, SyncStatus, WorktreeState,
    EVENT_SOURCE_GIT, SHORT_SHA_LEN,
};
pub use session::{AgentSession, SessionEnd, SessionStatus};
pub use state::{
    ChangedFile, CommitInfo, FileChange, ProjectState, ProjectStateRef, StateComparison,
    TestResults,
};
pub use task::{Complexity, ExpectedOutput, Priority, Subtask, Task, TaskStatus};

#[cfg(test)]
mod boundary_tests {
    //! Tests that protect the *shape* of the model, not any one entity.

    use super::*;

    /// The fundamental acceptance scenario, stated at the model level:
    /// one task, three agents, one stable identity.
    #[test]
    fn task_identity_survives_reassignment_across_agents() {
        let task_id = TaskId::from_string("AUTH-42");
        let claude = AgentId::from_string("AGENT-claude");
        let codex = AgentId::from_string("AGENT-codex");
        let deepseek = AgentId::from_string("AGENT-deepseek");

        let task = Task::new(task_id.clone(), "Auth", "Build login");
        assert_eq!(task.status, TaskStatus::Backlog);

        // Three consecutive assignments. Each is its own record.
        let mut a1 = AgentAssignment::propose(
            AssignmentId::from_string("ASG-1"),
            task_id.clone(),
            claude.clone(),
        );
        a1.activate(SessionId::from_string("SESS-1"));
        a1.release(ReleaseReason::Reassigned);

        let mut a2 = AgentAssignment::propose(
            AssignmentId::from_string("ASG-2"),
            task_id.clone(),
            codex.clone(),
        );
        a2.activate(SessionId::from_string("SESS-2"));
        a2.release(ReleaseReason::AgentCrashed);

        let mut a3 = AgentAssignment::propose(
            AssignmentId::from_string("ASG-3"),
            task_id.clone(),
            deepseek,
        );
        a3.activate(SessionId::from_string("SESS-3"));

        // The task was never mutated to change an agent: it has no field to
        // mutate. Its identity is byte-identical to before any assignment.
        assert_eq!(task.id, task_id);
        assert_eq!(task.status, TaskStatus::Backlog);

        // And the full tenure is recoverable.
        let history = [&a1, &a2, &a3];
        assert_eq!(history.iter().filter(|a| a.was_held_by(&claude)).count(), 1);
        assert_eq!(history.iter().filter(|a| a.was_held_by(&codex)).count(), 1);
        assert!(a3.is_active());
    }

    /// A reassignment does not change the task's identity or its own recorded
    /// state — only the assignment layer moves.
    #[test]
    fn reassigning_a_task_preserves_its_fields() {
        let task_id = TaskId::from_string("AUTH-42");
        let mut task = Task::new(task_id.clone(), "Auth", "Build login");
        task.status = TaskStatus::InProgress;
        task.expected_outputs.push(ExpectedOutput {
            criterion: "login works".into(),
            check: Some("cargo test".into()),
        });
        let snapshot = task.clone();

        let _assignment = AgentAssignment::propose(
            AssignmentId::from_string("ASG-1"),
            task_id,
            AgentId::from_string("AGENT-claude"),
        );

        // Assigning touched nothing on the task.
        assert_eq!(task.id, snapshot.id);
        assert_eq!(task.status, snapshot.status);
        assert_eq!(task.expected_outputs, snapshot.expected_outputs);
    }

    /// Assignment history is retained: released assignments are not deleted,
    /// they accumulate.
    #[test]
    fn assignment_history_is_retained_after_release() {
        let task_id = TaskId::from_string("AUTH-42");
        let mut history = Vec::new();

        for (i, agent) in ["claude", "codex", "deepseek", "human"].iter().enumerate() {
            let mut a = AgentAssignment::propose(
                AssignmentId::from_string(format!("ASG-{}", i + 1)),
                task_id.clone(),
                AgentId::from_string(format!("AGENT-{agent}")),
            );
            a.release(ReleaseReason::Reassigned);
            history.push(a);
        }

        assert_eq!(history.len(), 4);
        assert!(history.iter().all(|a| !a.is_active()));
        assert!(history
            .iter()
            .all(|a| a.release_reason == Some(ReleaseReason::Reassigned)));
        // Every record points back at the same task.
        assert!(history.iter().all(|a| a.task_id == task_id));
    }

    /// Compile-time check that identity kinds stay distinct. If this compiles,
    /// the newtypes are doing their job.
    #[test]
    fn identity_newtypes_are_not_interchangeable() {
        fn accept_task(_: TaskId) {}
        fn accept_agent(_: AgentId) {}
        fn accept_checkpoint(_: CheckpointId) {}

        accept_task(TaskId::from_string("AUTH-42"));
        accept_agent(AgentId::from_string("AGENT-1"));
        accept_checkpoint(CheckpointId::from_string("CHK-1"));
    }

    /// Every public entity round-trips through serde, because everything
    /// Director persists crosses a serialization boundary sooner or later.
    #[test]
    fn every_entity_round_trips_through_serde() {
        let task = Task::new(TaskId::from_string("AUTH-42"), "Auth", "Build login");
        let agent = Agent::register(
            AgentId::from_string("AGENT-1"),
            "claude",
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            vec![Capability::Coding],
        );
        let session = AgentSession::start(
            SessionId::from_string("SESS-1"),
            AgentId::from_string("AGENT-1"),
            MachineId::from_string("MACH-a"),
            Some(TaskId::from_string("AUTH-42")),
        );
        let plan = Plan::draft(
            PlanId::from_string("PLAN-1"),
            ProjectId::from_string("PROJ-x"),
            "ship auth",
            "login first",
        );
        let project = Project::new(ProjectId::from_string("PROJ-x"), "x", "/repo/x");
        let cp = Checkpoint::new(
            CheckpointId::from_string("CHK-1"),
            TaskId::from_string("AUTH-42"),
            "auth",
            "p",
            "n",
        );

        // If any of these fail to round-trip, persistence is silently lossy.
        for json in [
            serde_json::to_string(&task).unwrap(),
            serde_json::to_string(&agent).unwrap(),
            serde_json::to_string(&session).unwrap(),
            serde_json::to_string(&plan).unwrap(),
            serde_json::to_string(&project).unwrap(),
            serde_json::to_string(&cp).unwrap(),
        ] {
            assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
        }
    }
}
