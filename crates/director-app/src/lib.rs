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
