//! Orqyn's loop — the process that opens the store and drives the phases.
//!
//! This crate is the first code anywhere in the workspace that *uses* the
//! pieces the earlier phases built rather than building more of them. Until now
//! each layer was verified against its own tests and nothing else: the domain
//! model, the substrate adapters, the git observation layer, and the store all
//! existed in isolation. That was deliberate — each one had to be right on its
//! own terms — but it also means no code has ever opened a [`Store`] and done
//! something with it.
//!
//! ## The loop
//!
//! ```text
//! OBSERVE → PLAN → ASSIGN → MONITOR → VERIFY → REPLAN
//! ```
//!
//! This crate implements that loop one step at a time. Each step is a small
//! module that composes the layers underneath it, and each one is landed and
//! tested before the next is written — the loop is built in the order it will
//! run, so a step is never written against steps that do not exist yet.
//!
//! Five of the six steps are in place. OBSERVE is first for a structural
//! reason: it is the only one whose inputs come entirely from outside Orqyn.
//! PLAN reads the state OBSERVE produced; ASSIGN reads what PLAN decided;
//! MONITOR and VERIFY read what ASSIGN started. Every step after the first
//! consumes the output of the one before it, so writing OBSERVE first is what
//! gave the rest something to be tested against. REPLAN is built last, with
//! the remaining steps the README's fuller diagram names.
//!
//! ## What this crate is not
//!
//! Not an MCP server, and not a binary yet. The loop's steps are library
//! functions so they can be tested directly against real git repositories and
//! a real store, without a process boundary in the way. When the loop is
//! complete enough to run unattended, a thin binary will wrap it; until then,
//! the tests are the caller.

pub mod assign;
pub mod monitor;
pub mod observe;
pub mod plan;
pub mod verify;

use director_domain::capability::Capability;
use director_domain::ids::{AgentId, AssignmentId, PlanId, ProjectId, SessionId, TaskId};
use director_domain::task::TaskStatus;
use thiserror::Error;

/// Every way an OBSERVE round can fail, in one place.
///
/// The error deliberately flattens the two layers it composes into one enum
/// with no source chains, because the two failure modes are disjoint and a
/// caller that wants to react has to know which one happened:
///
/// - [`ObserveError::Store`] means Orqyn could not record what it learned. The
///   git observation may still have succeeded, so the raw state under the git
///   service's directory and Orqyn's normalized belief can be out of step.
///   Retrying the whole round reconciles them, which is why the round is
///   idempotent rather than partial.
/// - [`ObserveError::Git`] means Orqyn could not look at the repository at all.
///   Nothing was learned and nothing should be recorded.
#[derive(Debug, Error)]
pub enum ObserveError {
    /// Orqyn could not read or write its own state.
    #[error("the store rejected the observation: {0}")]
    Store(String),

    /// Orqyn could not observe the repository — the path is wrong, it is not a
    /// git repository, or git itself failed.
    #[error("could not observe the repository: {0}")]
    Git(String),
}

impl From<director_domain::StoreError> for ObserveError {
    fn from(err: director_domain::StoreError) -> Self {
        ObserveError::Store(err.to_string())
    }
}

impl From<director_domain::RepositoryError> for ObserveError {
    fn from(err: director_domain::RepositoryError) -> Self {
        ObserveError::Git(err.to_string())
    }
}

/// Every way a PLAN round can fail, in one place.
///
/// Like [`ObserveError`], this flattens its sources into one enum with no
/// source chains, because the failure modes are disjoint and a caller that
/// wants to react has to know which one happened. The split that matters here
/// is between a request that is wrong and a store that would not take it:
///
/// - The shape errors ([`Self::MissingObjective`], [`Self::NoTasks`],
///   [`Self::DuplicateTask`], [`Self::InvalidTask`], [`Self::DuplicatePlan`])
///   are rejected before anything is persisted. Nothing was written; the caller
///   fixes the request and tries again.
/// - The graph errors ([`Self::InvalidDependency`], [`Self::UnknownDependency`],
///   [`Self::CircularDependency`]) name the task ids involved, because "invalid
///   graph" is not actionable.
/// - [`Self::UnknownProject`] means the caller planned a project OBSERVE has
///   never run for — work with no context to ground it.
/// - [`Self::Store`] means the store rejected the write. Because
///   [`crate::plan::plan`] writes the plan, its tasks, and the activation in one
///   transaction, this leaves the project running against the plan it had.
#[derive(Debug, Error)]
pub enum PlanError {
    /// The project the plan is for is not known to Orqyn. In the running loop
    /// OBSERVE creates it as a side effect, so this means the caller is planning
    /// a project Orqyn has never observed.
    #[error("unknown project: {0}")]
    UnknownProject(String),

    /// The store rejected the plan. Nothing was written: the plan, its tasks,
    /// and the activation are one transaction.
    #[error("the store rejected the plan: {0}")]
    Store(String),

    /// The objective is empty or whitespace. An empty objective is not a plan.
    #[error("the plan has no objective")]
    MissingObjective,

    /// The request decomposes into no tasks at all.
    #[error("the plan has no tasks")]
    NoTasks,

    /// Two task specs share an id, which would silently collapse two pieces of
    /// work into one stored task.
    #[error("duplicate task id in the plan: {0}")]
    DuplicateTask(TaskId),

    /// A task spec is missing a title or an objective — the fields a task has to
    /// have to be meaningful.
    #[error("task {id} is invalid: {problem}")]
    InvalidTask {
        /// The id of the spec that is wrong.
        id: TaskId,
        /// What is wrong with it, in the caller's terms.
        problem: String,
    },

    /// A plan id is the plan's identity, and reusing one would overwrite the
    /// audit trail. Replanning means a new plan id.
    #[error("a plan with id {0} already exists")]
    DuplicatePlan(String),

    /// A task depends on something that cannot be a dependency — itself.
    #[error("task {task} has an invalid dependency: {problem}")]
    InvalidDependency {
        /// The task with the bad edge.
        task: TaskId,
        /// Why the edge is invalid.
        problem: String,
    },

    /// A task depends on a task that is not in this plan. A dependency Orqyn
    /// cannot check is a premise, and premises are rejected rather than assumed.
    #[error("task {task} depends on {dependency}, which is not in the plan")]
    UnknownDependency {
        /// The task holding the edge.
        task: TaskId,
        /// The id it depends on, which no spec in the plan names.
        dependency: TaskId,
    },

    /// The dependency graph has a cycle, so no execution order exists. The ids
    /// are returned in the order the cycle was walked, closing the loop.
    #[error("the plan's dependency graph has a cycle: {}",
        cycle.iter().map(|id| id.as_str()).collect::<Vec<_>>().join(" → ")) ]
    CircularDependency {
        /// The task ids forming the cycle, in walk order, closing the loop.
        cycle: Vec<String>,
    },
}

impl From<director_domain::StoreError> for PlanError {
    fn from(err: director_domain::StoreError) -> Self {
        PlanError::Store(err.to_string())
    }
}

/// Every way an ASSIGN round can fail, in one place.
///
/// Like [`ObserveError`] and [`PlanError`], this flattens its sources into one
/// enum with no chains, because the failure modes are disjoint and a caller
/// that wants to react has to know which one happened. The split that matters
/// here is between a handoff that is illegal as stated and a store that would
/// not take it:
///
/// - The legality errors are rejected before anything is written. Each one
///   names the ids involved, because "cannot assign" is not actionable — a
///   caller needs the task, and usually the reason.
/// - [`Self::NoActivePlan`] means the project is not executing anything, so
///   there is nothing to hand out. PLAN has not run, or its plan was archived.
/// - [`Self::Store`] means the store rejected the write. Because
///   [`crate::assign::assign`] does the whole handoff in one transaction, this
///   leaves the task unassigned and `todo`, so a retry is clean.
#[derive(Debug, Error)]
pub enum AssignError {
    /// Orqyn could not read or write its own state.
    #[error("the store rejected the assignment: {0}")]
    Store(String),

    /// The project has no active plan, so no task is eligible for assignment.
    /// In the running loop PLAN runs before ASSIGN, so this means the project
    /// was never planned or its plan was archived without a successor.
    #[error("no active plan for project {0}, so nothing can be assigned")]
    NoActivePlan(ProjectId),

    /// The plan is not authoritative — a draft or a superseded plan. Only an
    /// active plan's tasks are Orqyn's current intention.
    #[error("plan {0} is not active")]
    PlanNotActive(PlanId),

    /// The task is not one the active plan names, so handing it out would
    /// restart or invent work the plan does not intend.
    #[error("task {task} is not in plan {plan}")]
    TaskNotInPlan {
        /// The task the caller tried to assign.
        task: TaskId,
        /// The active plan for its project, which does not name it.
        plan: PlanId,
    },

    /// Another agent already holds the task. [`crate::assign::assign`] refuses
    /// rather than displacing them silently — a reassignment should be a
    /// deliberate act, and the sitting tenure is named so the caller can see
    /// what it would end.
    #[error("task {task} is already assigned to agent {agent} in assignment {assignment}")]
    TaskAlreadyAssigned {
        /// The task that is held.
        task: TaskId,
        /// The agent holding it.
        agent: AgentId,
        /// The active assignment giving them the task.
        assignment: AssignmentId,
    },

    /// The task is not `todo` — finished, blocked, in progress, or awaiting
    /// verification. Only a waiting task can be handed out.
    #[error("task {task} is {status:?}, not todo")]
    TaskNotTodo {
        /// The task the caller tried to assign.
        task: TaskId,
        /// The status it actually has.
        status: TaskStatus,
    },

    /// The task depends on work that is not `done` yet. A `failed` or
    /// `cancelled` dependency is not done either, and a dependency that no task
    /// in the project satisfies is a premise nothing is going to fulfill — both
    /// belong here rather than being silently assumed away.
    #[error("task {task} is blocked: dependencies not done: {}",
        unmet.iter().map(|id| id.as_str()).collect::<Vec<_>>().join(", ")) ]
    DependenciesNotDone {
        /// The task whose dependencies are unmet.
        task: TaskId,
        /// The dependencies that are not `done`, in the order the task lists
        /// them.
        unmet: Vec<TaskId>,
    },

    /// The agent cannot take work right now — busy, stale, disconnected, or
    /// offline.
    #[error("agent {agent} cannot accept work (status: {status:?})")]
    AgentUnavailable {
        /// The agent the caller named.
        agent: AgentId,
        /// Why it cannot take work, in the registry's own terms.
        status: director_domain::agent::AgentStatus,
    },

    /// The task requires a capability the agent does not declare. Assigning it
    /// anyway would be setting the agent up to fail; the verification engine is
    /// what catches failure, but there is no reason to arrange for it.
    #[error("agent {agent} is missing capabilities for task {task}: {}",
        missing.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ")) ]
    AgentMissingCapabilities {
        /// The agent that lacks the capabilities.
        agent: AgentId,
        /// The task requiring them.
        task: TaskId,
        /// The required capabilities the agent does not declare.
        missing: Vec<Capability>,
    },
}

impl From<director_domain::StoreError> for AssignError {
    fn from(err: director_domain::StoreError) -> Self {
        AssignError::Store(err.to_string())
    }
}

/// Every way a MONITOR round can fail, in one place.
///
/// Like the other steps' error enums, this is flat with no source chains,
/// because the failure modes are disjoint and a caller that wants to react has
/// to know which one happened. The split that matters here is between a
/// monitoring request that is wrong and a store that would not take the write:
///
/// - The acknowledgment errors ([`Self::UnknownAssignment`],
///   [`Self::AssignmentNotActive`], [`Self::AlreadyAcknowledged`]) are rejected
///   before anything is written. Nothing was recorded; the caller fixes the
///   request and tries again.
/// - [`Self::Store`] means the store rejected the read or the write. A lease
///   expiry is one transaction, so this leaves the task held and `in_progress`,
///   which is exactly the state the next round will survey again.
#[derive(Debug, Error)]
pub enum MonitorError {
    /// Orqyn could not read or write its own state.
    #[error("the store rejected the monitoring round: {0}")]
    Store(String),

    /// The assignment the caller asked to acknowledge does not exist. Either it
    /// was never made, or its id is wrong.
    #[error("no assignment {0}")]
    UnknownAssignment(AssignmentId),

    /// The assignment is not in force — it was released, so no session can be
    /// attached to it.
    #[error("assignment {0} is not active")]
    AssignmentNotActive(AssignmentId),

    /// The assignment already records the session doing the work. A tenure has
    /// one invocation; the first acknowledgment is the evidence trail.
    #[error("assignment {assignment} was already acknowledged by session {session}")]
    AlreadyAcknowledged {
        /// The assignment that already has a session.
        assignment: AssignmentId,
        /// The session it already records.
        session: SessionId,
    },
}

impl From<director_domain::StoreError> for MonitorError {
    fn from(err: director_domain::StoreError) -> Self {
        MonitorError::Store(err.to_string())
    }
}

/// Every way a VERIFY round can fail, in one place.
///
/// Like the other steps' error enums, this is flat with no source chains,
/// because there is only one failure mode: the store. The verdict itself never
/// errors — a check that cannot be run is reported as
/// [`crate::verify::UnverifiableReason::EvidenceMissing`] rather than raised,
/// because broken evidence is a property of the environment, not a failure of
/// the round. That distinction is what keeps an environment hiccup from being
/// recorded as a verdict on someone's work.
///
/// [`Self::Store`] means the round could not read the tasks awaiting a verdict
/// or could not persist one. A verdict is one task write with its history row,
/// so this leaves the task awaiting verification exactly as the round found it
/// — which is also why an unverifiable task is safe to survey again next tick.
#[derive(Debug, Error)]
pub enum VerifyError {
    /// Orqyn could not read or write its own state.
    #[error("the store rejected the verification round: {0}")]
    Store(String),
}

impl From<director_domain::StoreError> for VerifyError {
    fn from(err: director_domain::StoreError) -> Self {
        VerifyError::Store(err.to_string())
    }
}
