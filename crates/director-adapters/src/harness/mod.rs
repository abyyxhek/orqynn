//! Driving real agent harnesses.
//!
//! One driver exists per harness that has been both implemented and tested
//! against its installed CLI. Today that is [`claude::ClaudeCodeDriver`], and
//! the list is deliberately short: a [`Harness`](director_domain::agent::Harness)
//! variant with no driver behind it is a domain fact with no way to act on it,
//! and this module would rather say so than imply coverage it does not have.

pub mod claude;

pub use claude::{ClaudeCodeConfig, ClaudeCodeDriver};
