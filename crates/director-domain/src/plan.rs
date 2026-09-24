//! [Plan] — a deliberate, superseding artifact (Phase 7).
//!
//! A plan is never edited in place. When reality diverges from a plan — a
//! dependency failed, an estimate was wrong, a task proved unnecessary — the
//! replanner produces a **new** plan that supersedes the old one, and the old
//! one is retained as the audit trail of what was believed and when. This is
//! what makes "why did we change approach" answerable months later, which in
//! turn is what distinguishes a plan from a to-do list.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, PlanId, ProjectId, TaskId};

/// Lifecycle state of a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Being assembled; not yet authoritative.
    Draft,
    /// Authoritative. Its tasks are the ones eligible for assignment.
    Active,
    /// Replaced by a newer plan. Retained for audit.
    Superseded,
    /// Deliberately retired without a successor.
    Archived,
}

impl PlanStatus {
    /// True if this plan is the one Director should be executing against.
    pub fn is_authoritative(self) -> bool {
        matches!(self, PlanStatus::Active)
    }

    /// True if the plan has been replaced and is history only.
    pub fn is_historical(self) -> bool {
        matches!(self, PlanStatus::Superseded | PlanStatus::Archived)
    }
}

/// A decomposed objective: an ordered set of tasks plus the reasoning that
/// produced them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// This plan's identifier.
    pub id: PlanId,
    /// The project the plan is for.
    pub project_id: ProjectId,
    /// The objective the plan decomposes. Recorded here so the plan stays
    /// self-explanatory after the originating prompt scrolls away.
    pub objective: String,
    /// Tasks in intended execution order. Ordering is advisory — dependencies
    /// are the real constraint — but it is what a human reads as "the plan".
    pub task_ids: Vec<TaskId>,
    /// Why the plan looks like this: the alternatives the planner considered and
    /// rejected. Grounds the plan in observed facts rather than invention.
    pub rationale: String,
    /// Draft, active, or historical.
    pub status: PlanStatus,
    /// The plan that this one replaced, if any.
    pub supersedes: Option<PlanId>,
    /// The plan that replaced this one, set when supersession happens.
    pub superseded_by: Option<PlanId>,
    /// The agent that authorized the plan, if known.
    pub created_by: Option<AgentId>,
    /// When the plan was first written.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When it last changed.
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Plan {
    /// Draft a new plan for a project objective.
    pub fn draft(
        id: PlanId,
        project_id: ProjectId,
        objective: impl Into<String>,
        rationale: impl Into<String>,
    ) -> Self {
        let now = chrono::Utc::now();
        Plan {
            id,
            project_id,
            objective: objective.into(),
            task_ids: vec![],
            rationale: rationale.into(),
            status: PlanStatus::Draft,
            supersedes: None,
            superseded_by: None,
            created_by: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Promote a draft to the authoritative plan, recording who authorized it.
    pub fn activate(&mut self, created_by: AgentId) {
        self.created_by = Some(created_by);
        self.status = PlanStatus::Active;
        self.touch();
    }

    /// Mark this plan superseded by `successor`. Returns the successor's
    /// `supersedes` link so the caller can set it in the same transaction.
    pub fn supersede(&mut self, successor: PlanId) {
        self.status = PlanStatus::Superseded;
        self.superseded_by = Some(successor);
        self.touch();
    }

    /// Add a task to the plan, at the end.
    pub fn add_task(&mut self, task_id: TaskId) {
        if !self.task_ids.contains(&task_id) {
            self.task_ids.push(task_id);
            self.touch();
        }
    }

    /// Record a change, stamping `updated_at`.
    pub fn touch(&mut self) {
        self.updated_at = chrono::Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan::draft(
            PlanId::from_string("PLAN-1"),
            ProjectId::from_string("PROJ-x"),
            "ship auth",
            "login first, then sessions",
        )
    }

    #[test]
    fn a_new_plan_is_a_draft_and_not_authoritative() {
        let p = plan();
        assert_eq!(p.status, PlanStatus::Draft);
        assert!(!p.status.is_authoritative());
        // A draft is neither authoritative nor historical: it has not been
        // executed and has not been replaced.
        assert!(!p.status.is_historical());
    }

    #[test]
    fn activation_records_the_planner() {
        let mut p = plan();
        p.activate(AgentId::from_string("AGENT-claude"));
        assert!(p.status.is_authoritative());
        assert_eq!(p.created_by, Some(AgentId::from_string("AGENT-claude")));
    }

    #[test]
    fn supersession_links_both_ways() {
        let mut old = plan();
        old.activate(AgentId::from_string("AGENT-claude"));
        old.supersede(PlanId::from_string("PLAN-2"));

        assert!(old.status.is_historical());
        assert!(!old.status.is_authoritative());
        assert_eq!(old.superseded_by, Some(PlanId::from_string("PLAN-2")));
    }

    #[test]
    fn tasks_are_added_once_and_in_order() {
        let mut p = plan();
        p.add_task(TaskId::from_string("AUTH-1"));
        p.add_task(TaskId::from_string("AUTH-2"));
        p.add_task(TaskId::from_string("AUTH-1")); // deduplicated

        assert_eq!(
            p.task_ids,
            vec![TaskId::from_string("AUTH-1"), TaskId::from_string("AUTH-2")]
        );
    }

    #[test]
    fn plan_round_trips_through_serde() {
        let mut p = plan();
        p.add_task(TaskId::from_string("AUTH-1"));
        p.activate(AgentId::from_string("AGENT-claude"));
        p.supersede(PlanId::from_string("PLAN-2"));

        let json = serde_json::to_string(&p).unwrap();
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }
}
