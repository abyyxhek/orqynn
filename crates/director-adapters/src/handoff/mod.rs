//! Adapter for the handoff-mcp substrate.
//!
//! This is the Phase 2 deliverable: a real substrate adapter, built on the
//! boundary Phase 1 established. Three pieces, each with one job:
//!
//! - [`wire`] — serde mirrors of handoff-mcp's JSON shapes. Deliberately *not*
//!   the domain types and unable to become them, so every schema difference
//!   between the substrate and Director's model is visible in one place.
//! - [`transport`] — a stdio JSON-RPC client. Director talks to handoff-mcp
//!   exactly the way a harness would: it spawns the server as a child process
//!   and speaks line-delimited JSON-RPC 2.0 over stdin/stdout. Director never
//!   links the substrate's code; it crosses a process boundary.
//! - [`mapping`] — the bidirectional translation between the two models.
//!
//! ## The mapping is not boilerplate
//!
//! The two models are **not isomorphic**, and pretending otherwise would
//! silently corrupt state. Three vocabularies do not line up:
//!
//! 1. **Task status.** The substrate has 6 states; Director has 8. Director-only
//!    states ride in `TaskData.extra` (a `#[serde(flatten)]` map the substrate
//!    preserves) and fall back to the nearest honest substrate state on write.
//! 2. **Priority.** The substrate has `low`/`medium`/`high`; Director adds
//!    `Critical`, which maps down to `high` and is recovered from `extra`.
//! 3. **The important one.** handoff-mcp's `done` is an agent self-report —
//!    Phase 0 finding R6: `handoff_check_criterion` is a checkbox an agent
//!    ticks. So a substrate `done` does **not** become Director `Done`. It
//!    becomes [`VerificationPending`](director_domain::task::TaskStatus::VerificationPending),
//!    because nothing has been verified. Only a `done` that Director itself
//!    produced — marked with [`DIRECTOR_VERIFIED_KEY`](mapping::DIRECTOR_VERIFIED_KEY)
//!    — maps back to `Done`.
//!
//! That third rule is the acceptance criterion "agent claims done, tests fail →
//! must not become COMPLETED", enforced at the boundary rather than in the
//! loop. It is the reason the adapter exists as a separate layer instead of a
//! set of `From` impls.
//!
//! ## Identity: one child process per agent
//!
//! handoff-mcp holds a single process-wide agent identity, derived from the
//! `CLAUDE_SESSION_ID` environment variable at `handoff_load_context` time.
//! There is no per-request agent id. So the transport *bakes one identity into
//! the child's environment at spawn time* rather than passing it per call.
//!
//! The consequence is a real constraint, not a limitation of this code: **one
//! adapter instance speaks as one agent**. A Director process multiplexing
//! several agents needs one child process per agent identity. The pool that
//! manages that is Phase 8.
//!
//! ## What is not here
//!
//! The `HandoffAdapter` struct that implements `TaskProvider`,
//! `AgentProvider`, and `SessionProvider` by composing the transport and the
//! mapping. The pieces it will compose are complete and tested; the struct is
//! Phase 2's remaining step, and nothing here presupposes its shape.

pub mod mapping;
pub mod transport;
pub mod wire;
