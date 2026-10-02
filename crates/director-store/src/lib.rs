//! Orqyn Brain's own persistent store — the SQLite half of the boundary.
//!
//! ## The two boundaries, and why this crate is neither of the others
//!
//! Orqyn has two boundaries outwards and they are not similar:
//!
//! - `director-adapters` talks to handoff-mcp and ai-memory **over MCP**. Those
//!   servers own their schemas; Orqyn asks and receives.
//! - This crate talks to a `.db` file Orqyn **owns**. Nobody else writes it,
//!   nobody else's schema constrains it, and it is still readable when both
//!   substrates are down.
//!
//! The domain defines what "the store" means ([`director_domain::Store`] and the
//! repository traits); this crate is what those traits are made of. Nothing
//! above it ever learns that SQLite is involved — a caller matching on
//! [`director_domain::StoreError::StateVersionConflict`] retries without
//! importing a single rusqlite type.
//!
//! ## What the schema owns, and what it refuses to own
//!
//! The migration files are the only place tables are created; the application
//! never issues DDL at runtime (see [`migrations`]). The schema deliberately
//! holds Orqyn's own entities — checkpoints, assignments, normalized project
//! state — and pointedly *not* a copy of either substrate's tables. Copying a
//! provider's schema here is how a store starts being coupled to a substrate;
//! [`director_domain::ProviderSync`] is a pointer and a status, never a replica.
//!
//! ## Opening a store
//!
//! ```no_run
//! # use director_store::Store;
//! # use director_domain::{TaskId, TaskRepository};
//! # async fn example() -> Result<(), director_domain::StoreError> {
//! let store = Store::open("director.db").await?;
//! let task = store
//!     .tasks()
//!     .get_task(&TaskId::from_string("TASK-1"))
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! Opening brings the schema up to date on every connection: an old database is
//! migrated forward, and a database written by a *newer* Orqyn is refused
//! rather than silently downgraded.

// Phase 5 is building this crate's operations ahead of the callers that use
// them. A handful of helpers — `set_current_task`, `find_session`,
// `register_repository`/`list_repositories`, `current_version`, and the
// `EMPTY_ARRAY` collection encoding — are written and tested but not yet
// reached through the [`Store`] aggregate or the repository traits, so
// dead_code fires on them. Allow it crate-wide until the wiring catches up
// rather than deleting code that the phase needs; when [`Store`] exposes the
// last of these, drop this allow and let the lint police the rest.
//
// `assign_task` used to be on that list. It is reached through
// [`Store::assign_task`] and covered by the integration suite, so the lint
// already holds it — that is the shape the remaining helpers should take.
#![allow(dead_code)]

mod agents;
mod assignments;
mod checkpoints;
mod connection;
mod decisions;
mod json;
mod migrations;
mod plans;
mod projects;
mod sessions;
mod state_sync;
mod tasks;

use std::path::Path;

use director_domain::assignment::AgentAssignment;
use director_domain::ids::{AgentId, AssignmentId, TaskId};
use director_domain::plan::Plan;
use director_domain::session::AgentSession;
use director_domain::task::Task;
use director_domain::StoreError;

pub use migrations::{latest_version, migrations, Migration};
pub use state_sync::{SqliteProjectStateRepository, SqliteProviderSyncRepository};

// Each repository is public so a caller can hold one and pass it along, but
// none of them can be built from outside the crate — they all need the
// connection pool, and the only way to the pool is [`Store::open`]. That keeps
// "every connection is configured and migrated" an invariant rather than a
// convention someone could forget to honor.
pub use agents::SqliteAgentRepository;
pub use assignments::SqliteAssignmentRepository;
pub use checkpoints::SqliteCheckpointRepository;
pub use decisions::SqliteDecisionRepository;
pub use plans::SqlitePlanRepository;
pub use projects::SqliteProjectRepository;
pub use sessions::SqliteSessionRepository;
pub use tasks::SqliteTaskRepository;

/// The store, and the only way to reach any repository in it.
///
/// One [`Store`] is a small pool of connections to one database file, shared by
/// every repository it hands out. Cheap to clone if a caller needs to hand a
/// copy to several places — the pool underneath is reference-counted, so the
/// repositories are too.
#[derive(Clone)]
pub struct Store {
    pool: connection::ConnectionPool,
}

impl Store {
    /// Open a store at `path`, creating the file if it is absent and bringing
    /// its schema up to date.
    ///
    /// Every connection in the pool runs the migrations on open. That is
    /// idempotent — `schema_migrations` records what is applied, so an
    /// up-to-date database costs one cheap query per connection, and a caller
    /// can call this on every startup without thinking about it.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let pool = connection::ConnectionPool::open(path.as_ref())?;
        Ok(Store { pool })
    }

    /// The schema version this build can bring a database to. Diagnostic — for
    /// "is this Orqyn older or newer than the database it is looking at".
    pub fn schema_version(&self) -> u32 {
        latest_version()
    }

    /// Projects: the root everything else hangs off.
    pub fn projects(&self) -> SqliteProjectRepository {
        SqliteProjectRepository::new(self.pool.clone())
    }

    /// Tasks. The task row never names an agent; assignment is
    /// [`Self::assignments`].
    pub fn tasks(&self) -> SqliteTaskRepository {
        SqliteTaskRepository::new(self.pool.clone())
    }

    /// Agents: the registry of who is available.
    pub fn agents(&self) -> SqliteAgentRepository {
        SqliteAgentRepository::new(self.pool.clone())
    }

    /// Agent sessions: the lineage of who worked on what, and how each stint
    /// ended.
    pub fn sessions(&self) -> SqliteSessionRepository {
        SqliteSessionRepository::new(self.pool.clone())
    }

    /// Assignments: the first-class link between a task and an agent for a
    /// period. Releasing marks, never deletes.
    pub fn assignments(&self) -> SqliteAssignmentRepository {
        SqliteAssignmentRepository::new(self.pool.clone())
    }

    /// Hand a task to an agent atomically: any currently-active assignment for
    /// the task is released as `Reassigned`, the new assignment is inserted as
    /// `active`, and the agent's denormalized `current_task` view moves to this
    /// task — one transaction, all or nothing.
    ///
    /// This is the operation a caller uses to *assign* a task. The repository's
    /// `create_assignment` inserts a row in any status (a `Proposed` assignment
    /// that is not yet acknowledged); this is the operation that actually hands
    /// the task to an agent.
    pub async fn assign_task(
        &self,
        task_id: &TaskId,
        agent_id: &AgentId,
        assignment_id: &AssignmentId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<AgentAssignment, StoreError> {
        assignments::assign_task(&self.pool, task_id, agent_id, assignment_id, now).await
    }

    /// Hand a task to an agent and start it: [`Self::assign_task`]'s atomic
    /// handoff, plus the task's move to `in_progress` and the status transition
    /// that records it — all one transaction, all or nothing.
    ///
    /// This is the operation the loop's ASSIGN step uses. The handoff and the
    /// status move cannot be left in separate calls, because the window between
    /// them is exactly the state a crash would strand: an agent assigned to a
    /// task that still reads `todo`. Folding them together makes "an assigned
    /// task is an in-progress task" a property of the write rather than an
    /// ordering the caller has to get right.
    ///
    /// Returns the assignment, the task as it now stands, and the agent the
    /// handoff displaced — read back from the store, not echoed from the input.
    pub async fn assign_and_start(
        &self,
        task_id: &TaskId,
        agent_id: &AgentId,
        assignment_id: &AssignmentId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<(AgentAssignment, Task, Option<AgentId>), StoreError> {
        assignments::assign_and_start(&self.pool, task_id, agent_id, assignment_id, now).await
    }

    /// Create a plan, persist every task it decomposes into, and activate it:
    /// one transaction, all or nothing. The project's sitting active plan is
    /// superseded — marked and linked, never deleted — inside the same
    /// transaction, so a project is never left with two active plans and a
    /// failed plan never leaves half-written tasks behind.
    ///
    /// This is the operation the loop's PLAN step uses; the repository's
    /// `create_plan` and `activate_plan` are the lower-level halves.
    pub async fn create_active_plan(
        &self,
        plan: &Plan,
        tasks: &[Task],
        authorized_by: &AgentId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<(Plan, Vec<Task>), StoreError> {
        plans::create_active_plan(&self.pool, plan, tasks, authorized_by, now).await
    }

    /// Reclaim a task whose holder has gone quiet: release the assignment as
    /// `LeaseExpired`, close the session that held it as `Vanished` if it was
    /// still live, record the agent as `Disconnected`, clear its `current_task`
    /// view, and put the task back to `todo` — one transaction, all or nothing.
    ///
    /// This is the operation the loop's MONITOR step uses once a heartbeat has
    /// been quiet past the stale window. Returns `None` when no assignment holds
    /// the task at write time, which is the race a concurrent release or
    /// expiry wins: nothing is reclaimed.
    pub async fn expire_lease(
        &self,
        task_id: &TaskId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<(AgentAssignment, Task, Option<AgentSession>)>, StoreError> {
        assignments::expire_lease(&self.pool, task_id, now).await
    }

    /// Record that the agent holding an assignment began the session doing the
    /// work: insert the session row, attach it to the assignment, and mark the
    /// agent `Busy` — one transaction, all or nothing.
    ///
    /// This is the operation the loop's MONITOR step uses when an agent reports
    /// it has started the work it was handed. The assignment must still be
    /// active; a released assignment cannot acknowledge a session.
    pub async fn acknowledge_assignment(
        &self,
        assignment_id: &AssignmentId,
        session: &AgentSession,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<(AgentAssignment, AgentSession), StoreError> {
        assignments::acknowledge_assignment(&self.pool, assignment_id, session, now).await
    }

    /// Checkpoints: Orqyn's own resumption documents. Superseded ones are
    /// retained.
    pub fn checkpoints(&self) -> SqliteCheckpointRepository {
        SqliteCheckpointRepository::new(self.pool.clone())
    }

    /// Plans: the authoritative decomposition a project is executing against.
    /// Old plans supersede rather than disappear.
    pub fn plans(&self) -> SqlitePlanRepository {
        SqlitePlanRepository::new(self.pool.clone())
    }

    /// Decisions: the choices a project is committed to. Reversed ones are
    /// retained, linked to the decision that reversed them.
    pub fn decisions(&self) -> SqliteDecisionRepository {
        SqliteDecisionRepository::new(self.pool.clone())
    }

    /// Normalized project state — what Orqyn currently believes about a
    /// project, as distinct from what git said just now.
    pub fn project_state(&self) -> SqliteProjectStateRepository {
        SqliteProjectStateRepository::new(self.pool.clone())
    }

    /// Provider synchronization metadata: a pointer and a status per entity per
    /// provider.
    pub fn provider_sync(&self) -> SqliteProviderSyncRepository {
        SqliteProviderSyncRepository::new(self.pool.clone())
    }
}
