//! Orqyn's own persistent store — the domain-facing half of the boundary.
//!
//! ## What this module is, and what it is deliberately not
//!
//! Phase 1 built the *provider* traits: the boundary to the two external
//! substrates, over MCP, for the things a substrate actually owns (tasks as
//! handoff-mcp sees them, memories as ai-memory sees them). This module is the
//! other boundary, and it is not that one.
//!
//! [`crate::providers`] asks: *"what does the substrate say?"*
//! These traits ask: *"what does Orqyn itself know?"*
//!
//! The difference is load-bearing. A task in handoff-mcp is a row the substrate
//! owns; a task in Orqyn's store is a record Orqyn owns, with a version,
//! a project, and an assignment history that survives the substrate being
//! unreachable. Orqyn must be able to load every entity below with both
//! substrates down — that is the acceptance property in the Phase 4
//! specification, and it is why these traits are not provider traits.
//!
//! ## What lives where
//!
//! ```text
//! director-domain
//!     │  the traits in this module, and StoreError
//!     ▼
//! director-store
//!     │  SQLite, migrations, transactions
//!     ▼
//! a .db file Orqyn owns
//! ```
//!
//! Nothing in this module knows about SQLite. [`StoreError`] is a domain
//! vocabulary: a caller can match on [`StoreError::StateVersionConflict`] and
//! retry without ever learning that SQLite exists. The reverse direction is
//! enforced too — the store crate is where database-specific code stays, and
//! the boundary test would fail the build if a substrate leaked into the
//! domain.
//!
//! ## Optimistic concurrency
//!
//! Every mutable entity carries a `state_version` started at 1 and bumped once
//! per successful write — the same mechanism Phase 2 put on
//! [`crate::repository::ProjectStateSnapshot`], extended, not replaced. An
//! update takes the version the caller read; the store writes only if that
//! version is still current, and otherwise returns
//! [`StoreError::StateVersionConflict`]. Two writers who both read version 12
//! cannot both write 13: one wins, one gets a conflict it can act on, and
//! version 13 is never silently overwritten.
//!
//! ## Assignment history is never destroyed
//!
//! [`AssignmentRepository`] records each tenure as its own row. Releasing an
//! assignment marks it `Released`; it never deletes it. That is what makes
//! "Claude then Codex then DeepSeek worked on TASK-42" an ordinary query rather
//! than a data migration — see [`crate::assignment`].

use async_trait::async_trait;

use crate::agent::Agent;
use crate::assignment::AgentAssignment;
use crate::checkpoint::Checkpoint;
use crate::decision::Decision;
use crate::ids::{
    AgentId, AssignmentId, CheckpointId, DecisionId, PlanId, ProjectId, RepositoryId, SessionId,
    TaskId, VerificationId,
};
use crate::plan::Plan;
use crate::project::Project;
use crate::session::AgentSession;
use crate::task::{Task, TaskStatus};
use crate::verification::Verification;

/// The error vocabulary for Orqyn's own store.
///
/// Deliberately not `ProviderError` and deliberately not SQLite's error type.
/// Not `ProviderError`, because a store failure means something different from
/// a substrate failure — a conflict is retryable here and a substrate outage
/// is not the same fact at all. Not SQLite's, because a caller matching on
/// [`StoreError::StateVersionConflict`] must not have to know a database error
/// code to do it. The store crate is responsible for translating whatever its
/// engine reports into these variants.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// No record with this id exists. The caller asked for something that was
    /// never written, or was deleted.
    #[error("not found: {0}")]
    NotFound(String),
    /// An optimistic-concurrency conflict: the caller's `state_version` is no
    /// longer current. The stored version is the newer one; the caller must
    /// re-read and retry. Never silently overwritten.
    #[error(
        "state version conflict on {entity} {id}: stored version {stored}, caller had {caller}"
    )]
    StateVersionConflict {
        /// Which entity kind conflicted, e.g. `"task"`.
        entity: &'static str,
        /// The id of the record.
        id: String,
        /// The version the store now holds.
        stored: u64,
        /// The version the caller read and wrote against.
        caller: u64,
    },
    /// A uniqueness or referential constraint was violated: a duplicate id, or
    /// a reference to a project/task/agent that does not exist.
    #[error("constraint violation: {0}")]
    ConstraintViolation(String),
    /// A migration could not be applied, or the schema is in a state the code
    /// does not know how to handle. The database is not usable as-is.
    #[error("migration error: {0}")]
    Migration(String),
    /// A transaction could not be committed or rolled back as the store
    /// intended — a lower-level failure of the atomicity guarantee.
    #[error("transaction error: {0}")]
    Transaction(String),
    /// The store itself failed: it could not be opened, read, or written.
    #[error("storage error: {0}")]
    Storage(String),
    /// A value could not be serialized or deserialized into the shape a column
    /// requires — a sign the stored JSON and the domain type have drifted.
    #[error("serialization error: {0}")]
    Serialization(String),
}

/// What the store recorded about one lifecycle transition of a task.
///
/// Phase 4's history requirement is an append-oriented record: a status change
/// is written as a new row, never as an overwrite of the previous one. That is
/// what makes "what was the task state before?" answerable after the fact, and
/// what keeps the transition trail visible when a task moves
/// `ready → in_progress → verification_pending → blocked → in_progress`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaskStatusTransition {
    /// The task that transitioned.
    pub task_id: TaskId,
    /// The status it moved from.
    pub from: TaskStatus,
    /// The status it moved to.
    pub to: TaskStatus,
    /// When the transition was recorded.
    pub at: chrono::DateTime<chrono::Utc>,
}

/// Metadata about one Orqyn entity's synchronization with one external
/// provider.
///
/// Orqyn persists only what is needed to understand the state of a
/// synchronization: who the provider is, which external resource corresponds to
/// the Orqyn id, when it last synced, and what it said last. The provider's
/// own internal schema is not copied — this is a pointer and a status, not a
/// replica.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderSync {
    /// The Orqyn id of the entity this record describes.
    pub entity_id: String,
    /// Which provider the external resource belongs to, e.g. `"handoff-mcp"`.
    pub provider: String,
    /// The provider's own id for the same resource, when known.
    pub external_id: Option<String>,
    /// When Orqyn last attempted a synchronization.
    pub last_sync_at: chrono::DateTime<chrono::Utc>,
    /// When a synchronization last succeeded.
    pub last_success_at: Option<chrono::DateTime<chrono::Utc>>,
    /// What the last failed attempt said, when it failed.
    pub last_error: Option<String>,
    /// The provider's reported version, when it exposes one.
    pub external_version: Option<String>,
}

/// The normalized project state Orqyn currently knows about.
///
/// This is **not** a git observation. The git observer answers *"what did git
/// say just now?"*; this answers *"what does Orqyn currently believe the
/// project state to be?"*. The two are kept separate on purpose: the observer
/// is re-read from git every time, while this record is what Orqyn holds
/// between observations, what a checkpoint is compared against, and what
/// survives a restart. See [`crate::repository`] for the observer's own types.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredProjectState {
    /// The project this state belongs to.
    pub project_id: ProjectId,
    /// The repository the state was observed from, when one is registered.
    pub repository_id: Option<RepositoryId>,
    /// The checked-out branch, or `None` under a detached HEAD.
    pub branch: Option<String>,
    /// The commit `HEAD` pointed at when Orqyn last observed this project.
    pub head_commit: String,
    /// Whether the working tree was clean at the last observation.
    pub working_tree_clean: bool,
    /// How many observations Orqyn has made. Monotonic.
    pub observation_version: u64,
    /// When Orqyn last observed the project.
    pub last_observed_at: chrono::DateTime<chrono::Utc>,
    /// The project's state version. Bumped once per observation that records a
    /// meaningful change — the same counter a checkpoint records in
    /// [`Checkpoint::project_state_version`].
    pub state_version: u64,
}

// ---------------------------------------------------------------------------
// Repository traits. Each is a small, named set of operations rather than a
// generic CRUD surface, because the operations are not interchangeable: an
// assignment is released, never saved-as-a-generic-thing.
// ---------------------------------------------------------------------------

/// Marker supertrait, so every store implementation shares one error bound the
/// caller can rely on without knowing which concrete store it holds.
pub trait Store: Send + Sync {
    /// The error this store produces. Always [`StoreError`] for the real store;
    /// left as an associated type so a test double can say something else.
    type Error: std::error::Error + Send + Sync + 'static;
}

/// Persistence for projects — the root everything else hangs off.
#[async_trait]
pub trait ProjectRepository: Store {
    /// Persist a new project. Fails with
    /// [`StoreError::ConstraintViolation`] if the id is already taken, so two
    /// sessions initializing the same project cannot each silently win.
    async fn create_project(&self, project: &Project) -> Result<Project, Self::Error>;

    /// Load a project by id, or [`StoreError::NotFound`].
    async fn get_project(&self, id: &ProjectId) -> Result<Project, Self::Error>;

    /// Persist an updated project. The caller's `state_version` is the
    /// precondition; a stale version is a [`StoreError::StateVersionConflict`].
    async fn update_project(&self, project: &Project) -> Result<Project, Self::Error>;

    /// Every project Orqyn knows, ordered by name.
    async fn list_projects(&self) -> Result<Vec<Project>, Self::Error>;
}

/// Persistence for tasks.
///
/// Note the absence of any agent-shaped parameter: assigning is
/// [`AssignmentRepository`]'s job, and a task row carries no `current_agent_id`
/// — that invariant from [`crate::task`] holds all the way down to storage.
#[async_trait]
pub trait TaskRepository: Store {
    /// Persist a new task. The task's `project_id` must name an existing
    /// project; the store rejects an unscoped task rather than inventing one.
    async fn create_task(&self, task: &Task) -> Result<Task, Self::Error>;

    /// Load a task by id, or [`StoreError::NotFound`].
    async fn get_task(&self, id: &TaskId) -> Result<Task, Self::Error>;

    /// Persist an updated task, optimistic version check included. A status
    /// change is also recorded in the transition history.
    async fn update_task(&self, task: &Task) -> Result<Task, Self::Error>;

    /// Every task in a project, ordered by creation.
    async fn list_tasks(&self, project: &ProjectId) -> Result<Vec<Task>, Self::Error>;

    /// The recorded status transitions for a task, oldest first — the
    /// append-only history from [`TaskStatusTransition`].
    async fn task_history(&self, task: &TaskId) -> Result<Vec<TaskStatusTransition>, Self::Error>;
}

/// Persistence for agents — the registry of who is available.
#[async_trait]
pub trait AgentRepository: Store {
    /// Register a new agent. A duplicate id is a constraint violation.
    async fn register_agent(&self, agent: &Agent) -> Result<Agent, Self::Error>;

    /// Load an agent by id, or [`StoreError::NotFound`].
    async fn get_agent(&self, id: &AgentId) -> Result<Agent, Self::Error>;

    /// Persist an updated agent, optimistic version check included.
    async fn update_agent(&self, agent: &Agent) -> Result<Agent, Self::Error>;

    /// Every agent Orqyn knows.
    async fn list_agents(&self) -> Result<Vec<Agent>, Self::Error>;
}

/// Persistence for agent sessions — the evidence trail.
///
/// A task accumulates many sessions over its life; none of them are the task.
#[async_trait]
pub trait SessionRepository: Store {
    /// Record that a session started.
    async fn create_session(&self, session: &AgentSession) -> Result<AgentSession, Self::Error>;

    /// Load a session by id, or [`StoreError::NotFound`].
    async fn get_session(&self, id: &SessionId) -> Result<AgentSession, Self::Error>;

    /// Persist an updated session, optimistic version check included.
    async fn update_session(&self, session: &AgentSession) -> Result<AgentSession, Self::Error>;

    /// Close a session, recording how it ended. Atomic with the version bump.
    async fn end_session(
        &self,
        id: &SessionId,
        end: crate::session::SessionEnd,
        caller_version: u64,
    ) -> Result<AgentSession, Self::Error>;

    /// Every session that worked on a task, in start order — the lineage that
    /// makes "who worked on TASK-42" answerable.
    async fn sessions_for_task(&self, task: &TaskId) -> Result<Vec<AgentSession>, Self::Error>;
}

/// Persistence for assignments — the first-class link between a task and an
/// agent for a period.
///
/// Releasing never deletes; history accumulates. That is the whole point of the
/// entity, and the store preserves it.
#[async_trait]
pub trait AssignmentRepository: Store {
    /// Record a new assignment, typically `Proposed`. If the task already has
    /// an active assignment, the store rejects the attempt rather than
    /// silently displacing the sitting agent.
    async fn create_assignment(
        &self,
        assignment: &AgentAssignment,
    ) -> Result<AgentAssignment, Self::Error>;

    /// Load an assignment by id, or [`StoreError::NotFound`].
    async fn get_assignment(&self, id: &AssignmentId) -> Result<AgentAssignment, Self::Error>;

    /// Persist an updated assignment (activation, release), optimistic version
    /// check included.
    async fn update_assignment(
        &self,
        assignment: &AgentAssignment,
    ) -> Result<AgentAssignment, Self::Error>;

    /// Release an assignment with a reason. The prior assignment is marked, not
    /// removed, so the tenure stays queryable.
    async fn release_assignment(
        &self,
        id: &AssignmentId,
        reason: crate::assignment::ReleaseReason,
        caller_version: u64,
    ) -> Result<AgentAssignment, Self::Error>;

    /// The active assignment for a task, if any. There is at most one — the
    /// invariant that makes assignment unambiguous.
    async fn active_assignment_for_task(
        &self,
        task: &TaskId,
    ) -> Result<Option<AgentAssignment>, Self::Error>;

    /// Every assignment a task has had, in order — released ones included and
    /// retained.
    async fn assignment_history(&self, task: &TaskId) -> Result<Vec<AgentAssignment>, Self::Error>;
}

/// Persistence for checkpoints — Orqyn's own resumption documents.
///
/// Phase 4 stores the durable foundation: the record, the latest-for-task
/// marking, and the versions it must be compared against. The automatic
/// checkpoint *engine* is a later phase; this trait deliberately has no
/// "take a checkpoint automatically" method.
#[async_trait]
pub trait CheckpointRepository: Store {
    /// Persist a new checkpoint and mark it current for its task. The previous
    /// current checkpoint for that task is superseded, not deleted.
    async fn create_checkpoint(&self, checkpoint: &Checkpoint) -> Result<Checkpoint, Self::Error>;

    /// Load a checkpoint by id, or [`StoreError::NotFound`].
    async fn get_checkpoint(&self, id: &CheckpointId) -> Result<Checkpoint, Self::Error>;

    /// The latest checkpoint for a task — the one a resume would use — or
    /// `None` if the task has never been checkpointed.
    async fn latest_checkpoint(&self, task: &TaskId) -> Result<Option<Checkpoint>, Self::Error>;

    /// Every checkpoint for a task, newest first. Superseded ones included: the
    /// recovery history is retained, not garbage collected.
    async fn checkpoints_for_task(&self, task: &TaskId) -> Result<Vec<Checkpoint>, Self::Error>;
}

/// Persistence for plans — Orqyn's superseding artifacts.
///
/// A plan is never edited in place. When reality diverges, the replanner writes
/// a *new* plan that supersedes the old one, and the old one is retained as
/// audit trail. The store preserves that: supersession marks a row, it never
/// deletes it, and a project is never left with two active plans because the
/// schema refuses the second one.
#[async_trait]
pub trait PlanRepository: Store {
    /// Persist a new plan. A plan is created as a [`crate::plan::PlanStatus::Draft`];
    /// it becomes authoritative only through [`Self::activate_plan`], so an
    /// abandoned draft never becomes the plan a project executes against.
    async fn create_plan(&self, plan: &Plan) -> Result<Plan, Self::Error>;

    /// Load a plan by id, or [`StoreError::NotFound`].
    async fn get_plan(&self, id: &PlanId) -> Result<Plan, Self::Error>;

    /// Persist an updated plan, optimistic version check included.
    async fn update_plan(&self, plan: &Plan) -> Result<Plan, Self::Error>;

    /// Promote a draft to the authoritative plan for its project. The project's
    /// currently active plan — if any — is superseded in the same transaction,
    /// linked both ways, and the promoted plan's `supersedes` is set. This is
    /// the only way a plan becomes active, which is what makes "at most one
    /// active plan per project" an invariant rather than a caller's discipline.
    async fn activate_plan(&self, id: &PlanId, authorized_by: AgentId)
        -> Result<Plan, Self::Error>;

    /// The plan a project is currently executing against, or `None` if no plan
    /// is active. This is what the loop's ASSIGN step reads.
    async fn active_plan_for_project(
        &self,
        project: &ProjectId,
    ) -> Result<Option<Plan>, Self::Error>;

    /// Every plan for a project, newest first. Superseded and archived ones are
    /// retained: "what did we used to believe, and when" is the point of the
    /// entity.
    async fn plans_for_project(&self, project: &ProjectId) -> Result<Vec<Plan>, Self::Error>;
}

/// Persistence for decisions — the choices a project is committed to.
///
/// Like plans, decisions supersede rather than disappear. A reversed decision
/// keeps its row and gains a `superseded_by` link, so the record stays honest
/// when the project changes its mind.
#[async_trait]
pub trait DecisionRepository: Store {
    /// Record a new active decision.
    async fn create_decision(&self, decision: &Decision) -> Result<Decision, Self::Error>;

    /// Load a decision by id, or [`StoreError::NotFound`].
    async fn get_decision(&self, id: &DecisionId) -> Result<Decision, Self::Error>;

    /// Reverse a decision, recording which later decision replaced it. The
    /// reversed decision is marked, not deleted.
    async fn supersede_decision(
        &self,
        id: &DecisionId,
        superseded_by: &DecisionId,
    ) -> Result<Decision, Self::Error>;

    /// The decisions made in the context of a task, newest first. A resuming
    /// agent reads these so it does not re-litigate a settled question.
    async fn decisions_for_task(&self, task: &TaskId) -> Result<Vec<Decision>, Self::Error>;
}

/// Persistence for normalized project state — what Orqyn currently knows.
///
/// Not the git observation itself; see [`StoredProjectState`] for the
/// separation.
#[async_trait]
pub trait ProjectStateRepository: Store {
    /// Record or replace the state Orqyn holds for a project, bumping the
    /// state version when the observation reports a meaningful change.
    async fn update_project_state(
        &self,
        state: &StoredProjectState,
    ) -> Result<StoredProjectState, Self::Error>;

    /// The state Orqyn currently holds for a project, or `None` before the
    /// first observation.
    async fn get_project_state(
        &self,
        project: &ProjectId,
    ) -> Result<Option<StoredProjectState>, Self::Error>;
}

/// Persistence for verifications — the durable record of Orqyn judging a task's
/// work (Phase 10).
///
/// A verification is append-only history, not a mutable record: re-judging a
/// task writes a new row with its own id, so "what did Orqyn believe about this
/// task, and what changed its mind" stays answerable. Nothing in this trait
/// updates or deletes a verification, by design — the store preserves the
/// judgment history the way it preserves assignment and checkpoint history.
#[async_trait]
pub trait VerificationRepository: Store {
    /// Record one act of verification. The verification is new by construction:
    /// a duplicate id is a constraint violation, because two verifications with
    /// one id is a judgment history that has lost a row.
    async fn create_verification(
        &self,
        verification: &Verification,
    ) -> Result<Verification, Self::Error>;

    /// Load a verification by id, or [`StoreError::NotFound`].
    async fn get_verification(&self, id: &VerificationId) -> Result<Verification, Self::Error>;

    /// The most recent verification of a task, or `None` when Orqyn has never
    /// judged it. This is the record a caller reads to answer "what does Orqyn
    /// currently believe about this task's work, and on what evidence".
    async fn latest_verification(&self, task: &TaskId)
        -> Result<Option<Verification>, Self::Error>;

    /// Every verification of a task, newest first. Judgments accumulate rather
    /// than being overwritten, so a task that failed twice and passed on the
    /// third attempt returns all three.
    async fn verifications_for_task(&self, task: &TaskId)
        -> Result<Vec<Verification>, Self::Error>;
}

/// Persistence for provider synchronization metadata.
#[async_trait]
pub trait ProviderSyncRepository: Store {
    /// Record the outcome of a synchronization attempt for one entity, by
    /// Orqyn id and provider.
    async fn record_sync(&self, sync: &ProviderSync) -> Result<ProviderSync, Self::Error>;

    /// The last recorded synchronization for an entity and provider, if any.
    async fn last_sync(
        &self,
        entity_id: &str,
        provider: &str,
    ) -> Result<Option<ProviderSync>, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_errors_have_readable_messages() {
        // A caller must be able to report and to match on these without knowing
        // anything about the engine underneath.
        let e = StoreError::NotFound("TASK-42".into());
        assert!(format!("{e}").contains("TASK-42"));

        let conflict = StoreError::StateVersionConflict {
            entity: "task",
            id: "AUTH-42".into(),
            stored: 13,
            caller: 12,
        };
        let msg = format!("{conflict}");
        assert!(msg.contains("AUTH-42"));
        assert!(msg.contains("13"));
        assert!(msg.contains("12"));

        // The variant a retry loop matches on.
        assert!(matches!(conflict, StoreError::StateVersionConflict { .. }));
    }

    #[test]
    fn a_transition_records_both_ends() {
        let t = TaskStatusTransition {
            task_id: TaskId::from_string("AUTH-42"),
            from: TaskStatus::Todo,
            to: TaskStatus::InProgress,
            at: chrono::Utc::now(),
        };
        assert_eq!(t.from, TaskStatus::Todo);
        assert_eq!(t.to, TaskStatus::InProgress);
    }

    #[test]
    fn stored_project_state_round_trips_through_serde() {
        let s = StoredProjectState {
            project_id: ProjectId::from_string("PROJ-1"),
            repository_id: Some(RepositoryId::from_string("REPO-1")),
            branch: Some("main".into()),
            head_commit: "abc".into(),
            working_tree_clean: true,
            observation_version: 3,
            last_observed_at: chrono::Utc::now(),
            state_version: 2,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: StoredProjectState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn provider_sync_round_trips_through_serde() {
        let s = ProviderSync {
            entity_id: "AUTH-42".into(),
            provider: "handoff-mcp".into(),
            external_id: Some("t1".into()),
            last_sync_at: chrono::Utc::now(),
            last_success_at: Some(chrono::Utc::now()),
            last_error: None,
            external_version: Some("0.35.1".into()),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: ProviderSync = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
