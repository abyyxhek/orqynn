//! [Blocker] — something standing between a task and its next step (Phase 7/11).
//!
//! A blocker is a first-class entity rather than a string on a task for one
//! reason: **a blocked task must not silently stay blocked**. Director's loop
//! revisits open blockers; the entity carries the state (`Open` → `Resolved` →
//! possibly `Obsolete`) that makes "still blocked after three replans" an
//! observable, reportable condition rather than a task that just sits there.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, BlockerId, TaskId};

/// What kind of thing is blocking. Drives who can clear it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerKind {
    /// Needs a person: an approval, a credential, a product decision.
    External,
    /// Waiting on another Director task.
    Dependency,
    /// The path forward is unknown; needs investigation or a decision.
    Unknown,
    /// Two sources disagree and Director will not guess.
    Conflict,
}

/// Lifecycle state of a blocker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerStatus {
    /// Currently blocking.
    Open,
    /// Cleared; work can proceed.
    Resolved,
    /// The blocker is no longer relevant — the task was cancelled or replanned
    /// around it. Distinct from `Resolved` so a retrospective can tell "we
    /// fixed it" from "we stopped trying".
    Obsolete,
}

impl BlockerStatus {
    /// True if the blocker still stands in the way.
    pub fn is_blocking(self) -> bool {
        matches!(self, BlockerStatus::Open)
    }
}

/// Something preventing progress on a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocker {
    /// This blocker's identifier.
    pub id: BlockerId,
    /// The task it stands against.
    pub task_id: TaskId,
    /// What kind of thing is blocking.
    pub kind: BlockerKind,
    /// Open, resolved, or obsolete.
    pub status: BlockerStatus,
    /// What is blocking, stated so someone else can act on it.
    pub description: String,
    /// What unblocking looks like, if known.
    pub resolution: Option<String>,
    /// The agent that noticed it, if any.
    pub raised_by: Option<AgentId>,
    /// When it was raised.
    pub raised_at: chrono::DateTime<chrono::Utc>,
    /// When it was cleared, if it was.
    pub resolved_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Blocker {
    /// Raise a new open blocker against a task.
    pub fn raise(
        id: BlockerId,
        task_id: TaskId,
        kind: BlockerKind,
        description: impl Into<String>,
    ) -> Self {
        Blocker {
            id,
            task_id,
            kind,
            status: BlockerStatus::Open,
            description: description.into(),
            resolution: None,
            raised_by: None,
            raised_at: chrono::Utc::now(),
            resolved_at: None,
        }
    }

    /// Attribute the blocker to the agent that noticed it.
    pub fn by(mut self, agent_id: AgentId) -> Self {
        self.raised_by = Some(agent_id);
        self
    }

    /// Record how this blocker will be cleared.
    pub fn resolvable_by(mut self, resolution: impl Into<String>) -> Self {
        self.resolution = Some(resolution.into());
        self
    }

    /// Mark the blocker cleared.
    pub fn resolve(&mut self) {
        self.status = BlockerStatus::Resolved;
        self.resolved_at = Some(chrono::Utc::now());
    }

    /// Mark the blocker no longer relevant.
    pub fn make_obsolete(&mut self) {
        self.status = BlockerStatus::Obsolete;
        self.resolved_at = Some(chrono::Utc::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_raised_blocker_is_open_and_blocking() {
        let b = Blocker::raise(
            BlockerId::from_string("BLK-1"),
            TaskId::from_string("AUTH-42"),
            BlockerKind::External,
            "need SSO credentials",
        );
        assert!(b.status.is_blocking());
        assert!(b.resolution.is_none());
    }

    #[test]
    fn resolving_clears_the_blocker_and_records_when() {
        let mut b = Blocker::raise(
            BlockerId::from_string("BLK-1"),
            TaskId::from_string("AUTH-42"),
            BlockerKind::External,
            "need SSO credentials",
        );
        b.resolve();
        assert!(!b.status.is_blocking());
        assert!(b.resolved_at.is_some());
    }

    #[test]
    fn obsoleting_is_distinct_from_resolving() {
        let mut b = Blocker::raise(
            BlockerId::from_string("BLK-1"),
            TaskId::from_string("AUTH-42"),
            BlockerKind::Dependency,
            "waiting on AUTH-1",
        );
        b.make_obsolete();
        assert!(!b.status.is_blocking());
        assert_eq!(b.status, BlockerStatus::Obsolete);
    }

    #[test]
    fn provenance_and_resolution_are_chainable() {
        let b = Blocker::raise(
            BlockerId::from_string("BLK-1"),
            TaskId::from_string("AUTH-42"),
            BlockerKind::Unknown,
            "flaky on windows",
        )
        .by(AgentId::from_string("AGENT-codex"))
        .resolvable_by("reproduce on a clean runner");

        assert_eq!(b.raised_by, Some(AgentId::from_string("AGENT-codex")));
        assert_eq!(b.resolution, Some("reproduce on a clean runner".into()));
    }

    #[test]
    fn blocker_round_trips_through_serde() {
        let b = Blocker::raise(
            BlockerId::from_string("BLK-1"),
            TaskId::from_string("AUTH-42"),
            BlockerKind::Conflict,
            "two agents edited the same file",
        );
        let json = serde_json::to_string(&b).unwrap();
        let back: Blocker = serde_json::from_str(&json).unwrap();
        assert_eq!(b, back);
    }
}
