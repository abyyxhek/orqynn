//! Director Brain's own persistent store — the SQLite half of the boundary.
//!
//! ## The two boundaries, and why this crate is neither of the others
//!
//! Director has two boundaries outwards and they are not similar:
//!
//! - `director-adapters` talks to handoff-mcp and ai-memory **over MCP**. Those
//!   servers own their schemas; Director asks and receives.
//! - This crate talks to a `.db` file Director **owns**. Nobody else writes it,
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
//! holds Director's own entities — checkpoints, assignments, normalized project
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
//! migrated forward, and a database written by a *newer* Director is refused
//! rather than silently downgraded.

// Phase 5 is building this crate's operations ahead of the callers that use
// them. A handful of helpers — `assign_task`, `set_current_task`,
// `find_session`, `register_repository`/`list_repositories`, `current_version`,
// and the `EMPTY_ARRAY` collection encoding — are written and tested but not
// yet reached through the [`Store`] aggregate or the repository traits, so
// dead_code fires on them. Allow it crate-wide until the wiring catches up
// rather than deleting code that the phase needs; when [`Store`] exposes the
// last of these, drop this allow and let the lint police the rest.
#![allow(dead_code)]

mod agents;
mod assignments;
mod checkpoints;
mod connection;
mod json;
mod migrations;
mod projects;
mod sessions;
mod state_sync;
mod tasks;

use std::path::Path;

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
    /// "is this Director older or newer than the database it is looking at".
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

    /// Checkpoints: Director's own resumption documents. Superseded ones are
    /// retained.
    pub fn checkpoints(&self) -> SqliteCheckpointRepository {
        SqliteCheckpointRepository::new(self.pool.clone())
    }

    /// Normalized project state — what Director currently believes about a
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
