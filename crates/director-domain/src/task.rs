//! The [Task] — Director's central unit of work.
//!
//! ## The fundamental invariant
//!
//! A `Task` **has no agent field**. Assignment is a separate, first-class
//! entity ([`crate::assignment::AgentAssignment`]) that links a task to an
//! agent for a period. This is not stylistic: it is what makes
//!
//! > TASK-42: Claude works on it, then Codex, then DeepSeek, then a human.
//!
//! an ordinary reassignment rather than a data migration. A task's identity,
//! objective, dependencies, and history all outlive every agent that touches
//! it, and the compiler refuses to let an agent id reach a task's fields.
//!
//! Compare this with handoff-mcp's `TaskData`, which carries `assignee` and a
//! `lock` on the record itself — fine for a single-project substrate, wrong
//! for Director, where the assignment *history* is part of the value.

use serde::{Deserialize, Serialize};

use crate::capability::Capability;
use crate::ids::{SubtaskId, TaskId};

/// Scheduling priority. Lower `rank()` sorts earlier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Must be done before anything else.
    Critical,
    /// Important, but not blocking.
    High,
    /// Normal priority.
    Medium,
    /// Can wait.
    Low,
}

impl Priority {
    /// Sort order: Critical first. `None` (no priority set) sorts last, which
    /// is why this returns `u32::MAX` for the absent case via the caller.
    pub const fn rank(self) -> u32 {
        match self {
            Priority::Critical => 0,
            Priority::High => 1,
            Priority::Medium => 2,
            Priority::Low => 3,
        }
    }
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Priority::Critical => "critical",
            Priority::High => "high",
            Priority::Medium => "medium",
            Priority::Low => "low",
        };
        f.write_str(s)
    }
}

/// Rough complexity, used for load balancing and estimate-based scheduling.
///
/// Director deliberately does not emit hour estimates at plan time: plans are
/// grounded in observed state, and an invented "this will take 4 hours" is a
/// fact the planner cannot actually check. `Complexity` is a coarse ordinal
/// that a planner can justify from the task's surface area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    /// A one-line change.
    Trivial,
    /// A small, contained change.
    Small,
    /// A multi-file change.
    Medium,
    /// A substantial change touching many files.
    Large,
    /// Not yet estimated.
    Unknown,
}

impl Complexity {
    /// Ordinal for load balancing. `Unknown` sorts heaviest so an unestimated
    /// task is not silently preferred over a sized one.
    pub const fn weight(self) -> u32 {
        match self {
            Complexity::Trivial => 1,
            Complexity::Small => 2,
            Complexity::Medium => 4,
            Complexity::Large => 8,
            Complexity::Unknown => 16,
        }
    }
}

/// Lifecycle state of a task.
///
/// Transitions are driven by Director, not by the agent working on it. In
/// particular, an agent reporting "done" moves a task to
/// [`TaskStatus::VerificationPending`], **never** to [`TaskStatus::Done`] —
/// only the verification engine (Phase 10) can do that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Known to Director, not yet planned into an active plan.
    Backlog,
    /// In an active plan, waiting for a dependency or an agent.
    Todo,
    /// An agent is assigned and expected to be working.
    InProgress,
    /// Blocked on a [Blocker](crate::blocker::Blocker).
    Blocked,
    /// Agent claims completion; awaiting independent verification.
    VerificationPending,
    /// Verification passed against the task's expected output.
    Done,
    /// Verification failed. The task needs rework, possibly by another agent.
    Failed,
    /// Superseded or deliberately abandoned by the planner.
    Cancelled,
}

impl TaskStatus {
    /// True if the task still requires work of some kind.
    pub fn is_open(self) -> bool {
        matches!(
            self,
            TaskStatus::Backlog
                | TaskStatus::Todo
                | TaskStatus::InProgress
                | TaskStatus::Blocked
                | TaskStatus::VerificationPending
        )
    }

    /// True if the task is finished, one way or another.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }

    /// True if an agent's report can put the task into this state. `Done` is
    /// deliberately absent — this is the guardrail behind the acceptance test
    /// "agent claims done, tests fail → must not become COMPLETED".
    pub fn reachable_by_agent_report(self) -> bool {
        matches!(
            self,
            TaskStatus::InProgress | TaskStatus::Blocked | TaskStatus::VerificationPending
        )
    }
}

/// A requirement that must be observably true for the task to be done.
///
/// Textual, because it is what the planner produces from the objective. The
/// verification engine (Phase 10) is what turns these into checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedOutput {
    /// Human-readable acceptance criterion, e.g. "POST /login returns 200 with
    /// a session cookie for a valid user".
    pub criterion: String,
    /// Optional machine-checkable form, e.g. a test name or a command.
    pub check: Option<String>,
}

/// A unit of work.
///
/// Note what is absent: no `assignee`, no `agent_id`, no lock. Those live on
/// [`crate::assignment::AgentAssignment`] and on the substrate's own lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Stable, agent-independent identifier.
    pub id: TaskId,
    /// Short human-facing label.
    pub title: String,
    /// What "done" means, in the agent's terms.
    pub objective: String,
    /// Observable criteria for completion. Empty is legal but produces a task
    /// that can never be verified — the planner (Phase 7) must fill this in.
    pub expected_outputs: Vec<ExpectedOutput>,
    /// Lifecycle state.
    pub status: TaskStatus,
    /// Scheduling priority, if set.
    pub priority: Option<Priority>,
    /// Rough size, for load balancing.
    pub complexity: Complexity,
    /// Tasks that must reach a terminal state before this one starts.
    pub dependencies: Vec<TaskId>,
    /// What this task may touch, used for deterministic conflict detection
    /// (Phase 13). Advisory for the agent, authoritative for Director.
    pub scope_paths: Vec<String>,
    /// Capabilities an agent must declare to be assigned this task.
    pub required_capabilities: Vec<Capability>,
    /// Decomposition for larger tasks.
    pub subtasks: Vec<Subtask>,
    /// When Director first recorded the task.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When the task last changed.
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Task {
    /// Create a new task in `Backlog` with no dependencies or subtasks.
    pub fn new(id: TaskId, title: impl Into<String>, objective: impl Into<String>) -> Self {
        let now = chrono::Utc::now();
        Task {
            id,
            title: title.into(),
            objective: objective.into(),
            expected_outputs: vec![],
            status: TaskStatus::Backlog,
            priority: None,
            complexity: Complexity::Unknown,
            dependencies: vec![],
            scope_paths: vec![],
            required_capabilities: vec![],
            subtasks: vec![],
            created_at: now,
            updated_at: now,
        }
    }

    /// Whether every dependency has completed successfully.
    ///
    /// Used by the scheduler. This is deliberately a pure function over the
    /// statuses passed in: Director never trusts a stored "ready" flag.
    ///
    /// A dependency only satisfies this when it is [`TaskStatus::Done`] — a
    /// `Failed` or `Cancelled` dependency does **not** unblock the task. That
    /// is the point where the planner's dependency walk must notice the
    /// failure and replan, rather than silently proceeding onto work whose
    /// premise no longer holds.
    pub fn is_ready_given<'a, I>(&self, status_of: I) -> bool
    where
        I: Fn(&TaskId) -> Option<TaskStatus> + 'a,
    {
        self.dependencies
            .iter()
            .all(|dep| matches!(status_of(dep), Some(TaskStatus::Done)))
    }

    /// True if this task's scope overlaps another task's scope by path prefix.
    ///
    /// Deterministic, cheap, and intentionally conservative: it reports
    /// overlap on a shared directory even when the two tasks touch disjoint
    /// files inside it. Semantic conflict detection is explicitly out of scope
    /// for the first version.
    ///
    /// Trailing slashes are tolerated on both sides, so `"src/auth/"` and
    /// `"src/auth"` are the same scope.
    pub fn overlaps_scope(&self, other: &Task) -> bool {
        fn normalize(path: &str) -> &str {
            path.trim_end_matches('/')
        }

        self.id != other.id
            && self.scope_paths.iter().any(|a| {
                other.scope_paths.iter().any(|b| {
                    let (a, b) = (normalize(a), normalize(b));
                    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
                })
            })
    }

    /// Record a change, stamping `updated_at`.
    pub fn touch(&mut self) {
        self.updated_at = chrono::Utc::now();
    }
}

/// A subdivision of a [Task].
///
/// Subtasks share the parent's identity space: they are addressed as
/// `AUTH-42.1` and do not have their own dependencies or capabilities — those
/// come from the parent. If a piece of work needs its own dependencies, it is
/// a [Task], not a subtask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subtask {
    /// This subtask's identifier.
    pub id: SubtaskId,
    /// The task it belongs to.
    pub parent: TaskId,
    /// Short label.
    pub title: String,
    /// Whether the subtask is complete.
    pub done: bool,
    /// Position among siblings.
    pub order: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_task_has_no_agent() {
        let t = Task::new(TaskId::from_string("AUTH-42"), "Auth", "Build login");
        // The point of the exercise: there is nowhere on a Task to put an agent.
        assert_eq!(t.status, TaskStatus::Backlog);
        assert!(t.dependencies.is_empty());
        assert!(t.subtasks.is_empty());
    }

    #[test]
    fn done_is_not_reachable_by_agent_report() {
        // The acceptance criterion: an agent saying "done" must not complete
        // a task. Only verification does.
        assert!(!TaskStatus::Done.reachable_by_agent_report());
        assert!(TaskStatus::VerificationPending.reachable_by_agent_report());
    }

    #[test]
    fn open_and_terminal_partition_the_lifecycle() {
        assert!(TaskStatus::Todo.is_open());
        assert!(!TaskStatus::Todo.is_terminal());
        assert!(TaskStatus::Done.is_terminal());
        assert!(!TaskStatus::Done.is_open());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
    }

    #[test]
    fn readiness_follows_terminal_dependencies() {
        let mut t = Task::new(TaskId::from_string("AUTH-42"), "Auth", "Build login");
        t.dependencies = vec![TaskId::from_string("DB-1"), TaskId::from_string("DB-2")];

        let mut statuses = std::collections::HashMap::new();
        assert!(!t.is_ready_given(|id| statuses.get(id).copied()));

        statuses.insert(TaskId::from_string("DB-1"), TaskStatus::Done);
        assert!(!t.is_ready_given(|id| statuses.get(id).copied()));

        statuses.insert(TaskId::from_string("DB-2"), TaskStatus::Done);
        assert!(t.is_ready_given(|id| statuses.get(id).copied()));
    }

    #[test]
    fn a_failed_dependency_blocks_readiness() {
        let mut t = Task::new(TaskId::from_string("AUTH-42"), "Auth", "Build login");
        t.dependencies = vec![TaskId::from_string("DB-1")];

        let failed: std::collections::HashMap<TaskId, TaskStatus> =
            [(TaskId::from_string("DB-1"), TaskStatus::Failed)]
                .into_iter()
                .collect();
        // A failed dependency is terminal but not Done, so the task stays
        // blocked. The planner must detect this and replan.
        assert!(!t.is_ready_given(|id| failed.get(id).copied()));

        let done: std::collections::HashMap<TaskId, TaskStatus> =
            [(TaskId::from_string("DB-1"), TaskStatus::Done)]
                .into_iter()
                .collect();
        assert!(t.is_ready_given(|id| done.get(id).copied()));
    }

    #[test]
    fn an_unknown_dependency_blocks_readiness() {
        // A dependency Director has no status for is treated as unresolved:
        // better to wait than to start work on an unverified premise.
        let mut t = Task::new(TaskId::from_string("AUTH-42"), "Auth", "Build login");
        t.dependencies = vec![TaskId::from_string("MISSING-1")];
        assert!(!t.is_ready_given(|_| None));
    }

    #[test]
    fn scope_overlap_is_symmetric_and_path_aware() {
        let mut a = Task::new(TaskId::from_string("A"), "A", "A");
        let mut b = Task::new(TaskId::from_string("B"), "B", "B");
        a.scope_paths = vec!["src/auth/".into()];
        b.scope_paths = vec!["src/auth/login.rs".into()];

        assert!(a.overlaps_scope(&b));
        assert!(b.overlaps_scope(&a));
    }

    #[test]
    fn disjoint_scopes_do_not_overlap() {
        let mut a = Task::new(TaskId::from_string("A"), "A", "A");
        let mut b = Task::new(TaskId::from_string("B"), "B", "B");
        a.scope_paths = vec!["src/auth/".into()];
        b.scope_paths = vec!["src/db/".into()];

        assert!(!a.overlaps_scope(&b));
    }

    #[test]
    fn a_task_does_not_overlap_itself() {
        let mut a = Task::new(TaskId::from_string("A"), "A", "A");
        a.scope_paths = vec!["src/".into()];
        assert!(!a.overlaps_scope(&a));
    }

    #[test]
    fn priority_and_complexity_order_sensibly() {
        assert!(Priority::Critical.rank() < Priority::Low.rank());
        assert!(Complexity::Trivial.weight() < Complexity::Large.weight());
        assert_eq!(Complexity::Unknown.weight(), 16);
    }

    #[test]
    fn task_round_trips_through_serde() {
        let mut t = Task::new(TaskId::from_string("AUTH-42"), "Auth", "Build login");
        t.dependencies = vec![TaskId::from_string("DB-1")];
        t.scope_paths = vec!["src/auth/".into()];
        t.expected_outputs.push(ExpectedOutput {
            criterion: "login works".into(),
            check: Some("cargo test login".into()),
        });
        t.subtasks.push(Subtask {
            id: SubtaskId::from_string("AUTH-42.1"),
            parent: t.id.clone(),
            title: "login endpoint".into(),
            done: false,
            order: 1,
        });

        let json = serde_json::to_string(&t).unwrap();
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }
}
