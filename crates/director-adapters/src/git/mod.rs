//! Git observation: turning a working tree into Director's project state.
//!
//! ## The three-way split
//!
//! Phase 2's architectural rule is that observing git, interpreting what it
//! means, and deciding what to do about it are three different jobs. This
//! module holds the first two and deliberately not the third:
//!
//! - [`observer::GitObserver`] — "what happened in git?" A read-only lens over
//!   one working tree, backed by git2. Persists nothing.
//! - [`store::RepositoryStore`] — "what has Director recorded?" One JSON file
//!   per repository, written atomically, with optimistic version checks.
//! - [`service::GitService`] — "what does this mean for the project state?"
//!   Composes the two, runs the deterministic comparison, and persists the
//!   result.
//!
//! What is **not** here: anything that decides which agent should act, how a
//! task should be replanned, or whether a change is semantically important.
//! Those are later phases and they consume these types; they do not live here.
//!
//! ## Why git2 and not the git CLI
//!
//! The specification asks for a proper git library and warns against fragile
//! shell-string parsing. Using git2 also removes the entire class of command
//! injection this subsystem could otherwise present: there is no shell, so
//! there is nothing to inject into. A caller supplies a **path**, never a
//! command, and the only operations available are the named ones on
//! [`observer::GitObserver`].

pub mod observer;
pub mod service;
pub mod store;

pub use observer::{GitObserver, RangeWalk};
pub use service::{GitService, RepositoryStatusView};
pub use store::{RepositoryStore, StoredRepository};
