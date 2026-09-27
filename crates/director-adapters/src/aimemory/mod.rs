//! [`AiMemoryAdapter`] — Director's long-term memory over a live ai-memory.
//!
//! Three layers, mirroring the handoff adapter's shape so the two substrates
//! read as one pattern:
//!
//! - [`wire`] — hand-written mirrors of the JSON replies Director parses,
//!   verified against a running server.
//! - [`transport`] — a thin wrapper over the shared stdio transport, owning the
//!   spawn shape and the data directory.
//! - [`adapter`] — the [`MemoryProvider`] implementation, and the mapping
//!   between ai-memory's page model and Director's [`Memory`].
//!
//! See [`adapter`]'s docs for the four substrate-specific decisions that live
//! there, and `docs/PHASE3-MEMORY.md` for how they were verified.
//!
//! [`Memory`]: director_domain::providers::Memory
//! [`MemoryProvider`]: director_domain::providers::MemoryProvider

pub mod adapter;
pub mod transport;
pub mod wire;

// Mirrors the handoff module: only the adapter's public surface is re-exported
// here. The transport and the wire mirrors stay reachable by path
// (`aimemory::transport`, `aimemory::wire`) so the two substrates' internals do
// not collide at any namespace they share.
pub use adapter::{AiMemoryAdapter, AiMemoryAdapterError, AiMemoryWire};
