//! [AgentAssignment]: the link between a task and the agent doing it.
//!
//! This is a separate entity rather than a field on [crate::task::Task] for
//! the reason stated throughout the model: the task outlives the agent. Every
//! reassignment is a new row with its own history, so Director can always say
//! who worked on `AUTH-42`, when, and why they stopped.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, AssignmentId, SessionId, TaskId};

/// Why an assignment ended. This is what makes an audit trail useful: it
/// distinguishes "the agent finished" from "the agent vanished".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    /// Agent reported completion and the task went to verification.
    WorkComplete,
    /// Director reassigned the task, e.g. after a replan.
    Reassigned,
    /// The agent's session vanished and recovery moved the work elsewhere.
    AgentCrashed,
    /// The agent was released by an operator.
    ReleasedByOperator,
    /// The task was cancelled or superseded.
    TaskCancelled,
    /// The assignment lease expired with no heartbeat.
    LeaseExpired,
}

/// Lifecycle state of an assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentStatus {
    /// Director chose the agent; the agent has not acknowledged.
    Proposed,
    /// The agent accepted and holds the task.
    Active,
    /// The agent released it, for any [`ReleaseReason`].
    Released,
}

/// One agent's tenure on one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAssignment {
    /// This assignment's identifier.
    pub id: AssignmentId,
    /// The task being worked.
    pub task_id: TaskId,
    /// The agent holding it for this tenure.
    pub agent_id: AgentId,
    /// The specific session doing the work, if known.
    pub session_id: Option<SessionId>,
    /// Proposed, active, or released.
    pub status: AssignmentStatus,
    /// When the assignment was proposed.
    pub assigned_at: chrono::DateTime<chrono::Utc>,
    /// When it ended, if it has.
    pub released_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Why it ended, if it has.
    pub release_reason: Option<ReleaseReason>,
    /// Free-form note, e.g. "reassigned because schema changed".
    pub note: Option<String>,
}

impl AgentAssignment {
    /// Propose an assignment. Nothing is active until the agent acknowledges.
    pub fn propose(id: AssignmentId, task_id: TaskId, agent_id: AgentId) -> Self {
        AgentAssignment {
            id,
            task_id,
            agent_id,
            session_id: None,
            status: AssignmentStatus::Proposed,
            assigned_at: chrono::Utc::now(),
            released_at: None,
            release_reason: None,
            note: None,
        }
    }

    /// The agent accepted; record the session doing the work.
    pub fn activate(&mut self, session_id: SessionId) {
        self.session_id = Some(session_id);
        self.status = AssignmentStatus::Active;
    }

    /// End the assignment with a reason.
    pub fn release(&mut self, reason: ReleaseReason) {
        self.status = AssignmentStatus::Released;
        self.release_reason = Some(reason);
        self.released_at = Some(chrono::Utc::now());
    }

    /// True if this assignment is still in force.
    pub fn is_active(&self) -> bool {
        matches!(self.status, AssignmentStatus::Active)
    }

    /// True if this assignment was to `agent`.
    pub fn was_held_by(&self, agent: &AgentId) -> bool {
        &self.agent_id == agent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> TaskId {
        TaskId::from_string("AUTH-42")
    }

    #[test]
    fn a_proposed_assignment_is_not_active() {
        let a = AgentAssignment::propose(
            AssignmentId::from_string("ASG-1"),
            task(),
            AgentId::from_string("AGENT-claude"),
        );
        assert!(!a.is_active());
        assert_eq!(a.status, AssignmentStatus::Proposed);
    }

    #[test]
    fn activation_records_the_session() {
        let mut a = AgentAssignment::propose(
            AssignmentId::from_string("ASG-1"),
            task(),
            AgentId::from_string("AGENT-claude"),
        );
        a.activate(SessionId::from_string("SESS-1"));
        assert!(a.is_active());
        assert_eq!(a.session_id, Some(SessionId::from_string("SESS-1")));
    }

    #[test]
    fn release_records_reason_and_time() {
        let mut a = AgentAssignment::propose(
            AssignmentId::from_string("ASG-1"),
            task(),
            AgentId::from_string("AGENT-claude"),
        );
        a.activate(SessionId::from_string("SESS-1"));
        a.release(ReleaseReason::AgentCrashed);

        assert!(!a.is_active());
        assert_eq!(a.release_reason, Some(ReleaseReason::AgentCrashed));
        assert!(a.released_at.is_some());
    }

    #[test]
    fn the_same_task_can_be_assigned_to_three_different_agents() {
        // The acceptance scenario: TASK-42 passes Claude → Codex → DeepSeek.
        // Each is its own assignment; the task id never changes.
        let t = task();
        let a = AgentAssignment::propose(
            AssignmentId::from_string("ASG-1"),
            t.clone(),
            AgentId::from_string("AGENT-claude"),
        );
        let b = AgentAssignment::propose(
            AssignmentId::from_string("ASG-2"),
            t.clone(),
            AgentId::from_string("AGENT-codex"),
        );
        let c = AgentAssignment::propose(
            AssignmentId::from_string("ASG-3"),
            t,
            AgentId::from_string("AGENT-deepseek"),
        );

        assert_eq!(a.task_id, b.task_id);
        assert_eq!(b.task_id, c.task_id);
        assert_ne!(a.agent_id, b.agent_id);
        assert_ne!(b.agent_id, c.agent_id);

        // And each agent's ownership is answerable independently.
        assert!(a.was_held_by(&AgentId::from_string("AGENT-claude")));
        assert!(!a.was_held_by(&AgentId::from_string("AGENT-codex")));
    }

    #[test]
    fn assignment_round_trips_through_serde() {
        let mut a = AgentAssignment::propose(
            AssignmentId::from_string("ASG-1"),
            task(),
            AgentId::from_string("AGENT-claude"),
        );
        a.activate(SessionId::from_string("SESS-1"));
        a.release(ReleaseReason::Reassigned);
        let json = serde_json::to_string(&a).unwrap();
        let back: AgentAssignment = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }
}
