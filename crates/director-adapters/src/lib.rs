//! Adapters that turn Director's provider traits into something concrete.
//!
//! ## What lives here
//!
//! - [`InMemoryProvider`] — a single struct implementing **every** provider
//!   trait against in-process `HashMap`s. It is the reason Director's loop can
//!   be developed and tested with zero external processes.
//! - [`LocalExecutor`] — a real [`ExecutionProvider`](director_domain::providers::ExecutionProvider)
//!   that runs commands via `std::process`, used as the reference local
//!   executor and as the fallback when a substrate cannot execute commands.
//!
//! ## What does *not* live here
//!
//! The real substrate adapters — `HandoffAdapter` (MCP client for
//! handoff-mcp) and `AiMemoryAdapter` (MCP client for ai-memory) — are Phase 2
//! and Phase 3. They are deliberately absent from Phase 1: Director's core must
//! be provably independent of the substrates before any substrate is wired in,
//! and this crate is the proof.
//!
//! ## Git observation
//!
//! The [`git`] module is the Phase 2 observation layer: a git2-backed
//! observer, a file-backed store, and the service that composes them into
//! project state. Like [`LocalExecutor`], it is concrete tool integration
//! rather than substrate coupling — git is a tool Director reads, not a
//! substrate Director talks to over MCP.
//!
//! ## The coupling rule
//!
//! This is the **only** crate that is permitted to know a substrate's name.
//! The boundary test in `tests/boundary.rs` enforces that: no other crate in
//! the workspace may reference a substrate at all.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod executor;
pub mod git;
pub mod handoff;
pub mod memory;

pub use executor::LocalExecutor;
pub use memory::InMemoryError;
pub use memory::InMemoryProvider;

// The git observation layer, re-exported flat: callers say `GitService`, not
// `git::service::GitService`.
pub use git::{GitObserver, GitService, RangeWalk, RepositoryStatusView, RepositoryStore};

// The handoff adapter's sub-modules are re-exported flat so callers can reach
// the transport and the mapping without knowing the internal split.
pub use handoff::mapping;
pub use handoff::transport::{McpTransport, TransportError};
pub use handoff::wire;
