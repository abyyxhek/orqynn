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
//! Nothing, any more. `HandoffAdapter` (Phase 2) and `AiMemoryAdapter` (Phase
//! 3) are both here, and that is the point: Director's core was kept
//! independent of the substrates until both adapters existed, and this crate
//! is the proof. Every other crate in the workspace still cannot name a
//! substrate.
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

pub mod aimemory;
pub mod executor;
pub mod git;
pub mod handoff;
pub mod memory;
pub mod stdio_mcp;

pub use executor::LocalExecutor;
pub use memory::InMemoryError;
pub use memory::InMemoryProvider;

// The git observation layer, re-exported flat: callers say `GitService`, not
// `git::service::GitService`.
pub use git::{GitObserver, GitService, RangeWalk, RepositoryStatusView, RepositoryStore};

// The handoff adapter's sub-modules are re-exported flat so callers can reach
// the transport and the mapping without knowing the internal split.
pub use handoff::adapter::{HandoffAdapter, HandoffAdapterError, HandoffWire};
pub use handoff::mapping;
pub use handoff::transport::{McpTransport, TransportError as HandoffTransportError};
pub use handoff::wire;

// Flat for the same reason: callers name the adapter and its seam, not the
// module split. Its transport error is renamed on the way out so it does not
// collide with the handoff one at the crate root — both are the shared stdio
// error underneath, but a caller matching on one should not get the other by
// accident. `wire` is deliberately not re-exported flat: both substrates have
// a module of that name, so the handoff one keeps the flat slot and this
// substrate's stays reachable as `aimemory::wire`.
pub use aimemory::adapter::{AiMemoryAdapter, AiMemoryAdapterError, AiMemoryWire};
pub use aimemory::transport::{MemoryTransport, TransportError as AiMemoryTransportError};
