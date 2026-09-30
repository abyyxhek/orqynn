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
//! Only OBSERVE exists today. It is the natural first step for a structural
//! reason: it is the only one whose inputs come entirely from outside Orqyn.
//! PLAN reads the state OBSERVE produced; ASSIGN reads what PLAN decided;
//! MONITOR and VERIFY read what ASSIGN started. Every step after the first
//! consumes the output of the one before it, so writing OBSERVE first is what
//! gives the rest something to be tested against.
//!
//! ## What this crate is not
//!
//! Not an MCP server, and not a binary yet. The loop's steps are library
//! functions so they can be tested directly against real git repositories and
//! a real store, without a process boundary in the way. When the loop is
//! complete enough to run unattended, a thin binary will wrap it; until then,
//! the tests are the caller.

pub mod observe;
pub mod plan;

use director_domain::ids::TaskId;
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
