//! [Decision] — a choice the project is now committed to (Phase 7).
//!
//! Decisions are recorded separately from progress narrative because they have
//! a different half-life. "I wrote the login handler" is soon irrelevant; "we
//! chose stateless JWTs over server sessions" is relevant for the life of the
//! codebase. A checkpoint carries decision ids precisely so a resuming agent
//! does not re-litigate a settled question.
//!
//! Decisions can be **superseded**: a later decision records that an earlier
//! one was reversed, and why. This is the mechanism that keeps the record
//! honest when the team changes its mind.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, DecisionId, TaskId};

/// Lifecycle state of a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    /// Stands.
    Active,
    /// Reversed by a later decision.
    Superseded,
    /// Recorded, then found to be moot or wrong without a direct reversal.
    Withdrawn,
}

impl DecisionStatus {
    /// True if the decision is still in force and should be honored.
    pub fn stands(self) -> bool {
        matches!(self, DecisionStatus::Active)
    }
}

/// A committed choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// This decision's identifier.
    pub id: DecisionId,
    /// The task this decision was made in the context of, if any. Decisions
    /// often outlive their task, so this is provenance, not scope.
    pub task_id: Option<TaskId>,
    /// The choice, stated as a rule: "we use stateless JWTs".
    pub title: String,
    /// Why. A decision without a rationale is an opinion, and the next agent
    /// has no basis to respect or re-evaluate it.
    pub rationale: String,
    /// What was considered and rejected, so the same dead ends are not
    /// re-explored.
    pub alternatives_considered: Vec<String>,
    /// Active, superseded, or withdrawn.
    pub status: DecisionStatus,
    /// The decision that reversed this one.
    pub superseded_by: Option<DecisionId>,
    /// The agent that made it, if known.
    pub made_by: Option<AgentId>,
    /// When it was made.
    pub made_at: chrono::DateTime<chrono::Utc>,
    /// When it last changed.
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Decision {
    /// Record a new active decision.
    pub fn new(id: DecisionId, title: impl Into<String>, rationale: impl Into<String>) -> Self {
        let now = chrono::Utc::now();
        Decision {
            id,
            task_id: None,
            title: title.into(),
            rationale: rationale.into(),
            alternatives_considered: vec![],
            status: DecisionStatus::Active,
            superseded_by: None,
            made_by: None,
            made_at: now,
            updated_at: now,
        }
    }

    /// Scope the decision to the task it arose from.
    pub fn in_task(mut self, task_id: TaskId) -> Self {
        self.task_id = Some(task_id);
        self
    }

    /// Attribute the decision to an agent.
    pub fn by(mut self, agent_id: AgentId) -> Self {
        self.made_by = Some(agent_id);
        self
    }

    /// Note a rejected alternative, for the record.
    pub fn considered(mut self, alternative: impl Into<String>) -> Self {
        self.alternatives_considered.push(alternative.into());
        self
    }

    /// Reverse this decision in favor of `replacement`.
    pub fn supersede(&mut self, replacement: DecisionId) {
        self.status = DecisionStatus::Superseded;
        self.superseded_by = Some(replacement);
        self.updated_at = chrono::Utc::now();
    }

    /// Withdraw the decision without a direct replacement.
    pub fn withdraw(&mut self) {
        self.status = DecisionStatus::Withdrawn;
        self.updated_at = chrono::Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_decision_stands() {
        let d = Decision::new(
            DecisionId::from_string("DEC-1"),
            "use JWTs",
            "stateless scaling",
        );
        assert!(d.status.stands());
        assert_eq!(d.alternatives_considered, Vec::<String>::new());
    }

    #[test]
    fn a_superseded_decision_no_longer_stands() {
        let mut d = Decision::new(
            DecisionId::from_string("DEC-1"),
            "use JWTs",
            "stateless scaling",
        );
        d.supersede(DecisionId::from_string("DEC-2"));
        assert!(!d.status.stands());
        assert_eq!(d.superseded_by, Some(DecisionId::from_string("DEC-2")));
    }

    #[test]
    fn a_withdrawn_decision_does_not_stand() {
        let mut d = Decision::new(
            DecisionId::from_string("DEC-1"),
            "use JWTs",
            "stateless scaling",
        );
        d.withdraw();
        assert!(!d.status.stands());
    }

    #[test]
    fn provenance_is_optional_and_chainable() {
        let d = Decision::new(DecisionId::from_string("DEC-1"), "use JWTs", "scaling")
            .in_task(TaskId::from_string("AUTH-42"))
            .by(AgentId::from_string("AGENT-claude"))
            .considered("server sessions")
            .considered("mTLS");

        assert_eq!(d.task_id, Some(TaskId::from_string("AUTH-42")));
        assert_eq!(d.made_by, Some(AgentId::from_string("AGENT-claude")));
        assert_eq!(d.alternatives_considered.len(), 2);
    }

    #[test]
    fn decision_round_trips_through_serde() {
        let d = Decision::new(DecisionId::from_string("DEC-1"), "use JWTs", "scaling")
            .considered("sessions");
        let json = serde_json::to_string(&d).unwrap();
        let back: Decision = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }
}
