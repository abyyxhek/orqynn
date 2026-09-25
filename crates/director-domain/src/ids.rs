//! Canonical identity types for Director Brain.
//!
//! The single most important property of this module: **every identity is a
//! distinct newtype**. A [`TaskId`] and an [`AgentId`] are never
//! interchangeable, even though both wrap a `String`. This is what makes the
//! fundamental rule of the system a compile-time guarantee rather than a
//! convention:
//!
//! > Task identity MUST be independent of agent identity.
//!
//! A task can be worked on by Claude, then Codex, then DeepSeek, then a human.
//! The task is still `TASK-42`. Because [`Task`] holds no agent field and
//! [`TaskId`] cannot be constructed from an [`AgentId`], the compiler enforces
//! that a task outlives every agent that touches it.
//!
//! Identifiers are strings, not integers, so they can carry a human-readable
//! prefix (`AUTH-42`) that survives across machines, harnesses, and substrates.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Marker trait implemented by every identity newtype.
///
/// Exists so that generic helpers can accept "any Director id" while still
/// refusing a bare `String`.
pub trait Id: Sealed {
    /// The human-readable prefix convention for this id kind (`TASK`, `AGENT`).
    const PREFIX: &'static str;

    /// The raw underlying string.
    fn as_str(&self) -> &str;

    /// Construct an id from a string. Declared on the trait so generic code
    /// (notably [`IdGenerator::next`]) can build any id kind. Each newtype also
    /// has this as an inherent method, which takes precedence at call sites.
    fn from_string(value: impl Into<String>) -> Self;
}

// Sealing: external crates cannot invent their own `Id`, so the set of
// identity kinds stays closed and auditable. Named `private` (not `Sealed`)
// so the module and the trait inside it cannot be confused for one another.
mod private {
    pub trait Sealed {}
}

// Bring the sealed trait into scope for the `Id` supertrait bound below.
use private::Sealed;

/// Generate an identity newtype.
///
/// Implements `Display`, `Debug`, `Clone`, `PartialEq`, `Eq`, `Hash`,
/// `Serialize`, `Deserialize`, and [`Id`]. Ordering is deliberately *not*
/// derived: ids are opaque labels, and sorting tasks by id would invite
/// callers to read meaning into an arbitrary ordering.
macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Create a new id from any string. The caller is responsible for
            /// the format; Director generates ids with the `new` function on
            /// [`IdGenerator`].
            pub fn from_string(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Borrow the underlying string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl crate::ids::Id for $name {
            const PREFIX: &'static str = $prefix;

            fn as_str(&self) -> &str {
                &self.0
            }

            fn from_string(value: impl Into<String>) -> Self {
                Self(value.into())
            }
        }

        impl crate::ids::private::Sealed for $name {}
    };
}

define_id!(
    /// Unique identifier for a [Project](crate::project::Project).
    ///
    /// Example: `PROJ-checkout-service`
    ProjectId,
    "PROJ"
);

define_id!(
    /// Unique identifier for a [Task](crate::task::Task).
    ///
    /// Task ids are **agent-independent and stable for the lifetime of the
    /// task**, across every agent that works on it.
    ///
    /// Example: `AUTH-42`
    TaskId,
    "TASK"
);

define_id!(
    /// Unique identifier for a [Subtask](crate::task::Subtask).
    ///
    /// Example: `AUTH-42.1`
    SubtaskId,
    "SUB"
);

define_id!(
    /// Unique identifier for an [Agent](crate::agent::Agent).
    ///
    /// An agent is a specific harness+model+machine worker, e.g. "the Claude
    /// Code session on MACHINE-A". Agents are replaceable; tasks are not.
    ///
    /// Example: `AGENT-claude-machine-a`
    AgentId,
    "AGENT"
);

define_id!(
    /// Unique identifier for an [AgentSession](crate::session::AgentSession) —
    /// one concrete invocation of one agent on one machine.
    SessionId,
    "SESS"
);

define_id!(
    /// Unique identifier for a [Machine](crate::agent::Machine).
    ///
    /// Machines appear and disappear. The work survives.
    MachineId,
    "MACH"
);

define_id!(
    /// Unique identifier for a [Checkpoint](crate::checkpoint::Checkpoint).
    CheckpointId,
    "CHK"
);

define_id!(
    /// Unique identifier for an [Action](crate::action::Action) recorded in
    /// recent context.
    ActionId,
    "ACT"
);

define_id!(
    /// Unique identifier for a [Plan](crate::plan::Plan).
    PlanId,
    "PLAN"
);

define_id!(
    /// Unique identifier for a [Decision](crate::decision::Decision).
    DecisionId,
    "DEC"
);

define_id!(
    /// Unique identifier for a [Blocker](crate::blocker::Blocker).
    BlockerId,
    "BLK"
);

define_id!(
    /// Unique identifier for a [Handoff](crate::handoff::Handoff) from one
    /// agent to another.
    HandoffId,
    "HOF"
);

define_id!(
    /// Unique identifier for an [AgentAssignment](crate::assignment::AgentAssignment).
    AssignmentId,
    "ASG"
);

define_id!(
    /// Unique identifier for a [Repository](crate::repository::Repository) that
    /// Director observes.
    ///
    /// Example: `REPO-checkout`
    RepositoryId,
    "REPO"
);

define_id!(
    /// Unique identifier for an [ObservationEvent](crate::repository::ObservationEvent)
    /// — one fact that Director learned by looking at git.
    ///
    /// Example: `EVT-a1b2c3`
    EventId,
    "EVT"
);

/// Generates ids with a monotonic per-generator counter.
///
/// Director never relies on an external id service: a new machine with no
/// network must still be able to create tasks. The generated form is
/// `<PREFIX>-<human-key>-<n>`, e.g. `TASK-auth-1`. Within a single Director
/// process the counter is monotonic; across processes or machines the
/// `<human-key>` disambiguates, and full global uniqueness is established by
/// the store on persistence.
#[derive(Debug, Clone, Default)]
pub struct IdGenerator {
    counters: std::collections::HashMap<&'static str, u64>,
}

impl IdGenerator {
    /// Create a fresh generator with all counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Generate the next id of kind `T` under the given human-readable key.
    ///
    /// ```
    /// use director_domain::ids::{IdGenerator, TaskId};
    ///
    /// let mut gen = IdGenerator::new();
    /// let a: TaskId = gen.next("auth");
    /// let b: TaskId = gen.next("auth");
    /// assert_ne!(a, b);
    /// ```
    pub fn next<T: Id>(&mut self, key: &str) -> T {
        let n = self.counters.entry(T::PREFIX).or_insert(0);
        *n += 1;
        T::from_string(format!("{}-{}-{}", T::PREFIX, key, *n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_and_agent_ids_are_distinct_types() {
        // This is the property the whole system rests on: a TaskId can never
        // stand in for an AgentId, so a task can never be "owned" by an agent
        // at the type level.
        fn takes_task(_id: TaskId) {}
        fn takes_agent(_id: AgentId) {}

        let task = TaskId::from_string("AUTH-42");
        let agent = AgentId::from_string("AGENT-1");
        takes_task(task);
        takes_agent(agent);
    }

    #[test]
    fn ids_round_trip_through_serde() {
        let id = CheckpointId::from_string("CHK-1");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"CHK-1\"");
        let back: CheckpointId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn id_generator_is_monotonic_per_kind() {
        let mut gen = IdGenerator::new();
        let t1: TaskId = gen.next("auth");
        let t2: TaskId = gen.next("auth");
        let a1: AgentId = gen.next("claude");
        let a2: AgentId = gen.next("codex");

        assert_eq!(t1.as_str(), "TASK-auth-1");
        assert_eq!(t2.as_str(), "TASK-auth-2");
        assert_eq!(a1.as_str(), "AGENT-claude-1");
        assert_eq!(a2.as_str(), "AGENT-codex-2");
    }

    #[test]
    fn ids_display_as_their_raw_string() {
        assert_eq!(TaskId::from_string("AUTH-42").to_string(), "AUTH-42");
    }
}
