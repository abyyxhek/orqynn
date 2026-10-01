//! The ASSIGN step of Orqyn's loop.
//!
//! ASSIGN takes what PLAN decided and hands it to an agent: it picks a task the
//! active plan makes eligible, checks that the named agent can take it, and
//! makes the assignment stick — the task becomes `in_progress`, the agent
//! becomes responsible for it, and the two facts land in one transaction.
//!
//! ```text
//! OBSERVE → PLAN → ASSIGN
//!                  ▲
//!                  │
//!          the active plan
//!          + a task id
//!          + an agent id
//! ```
//!
//! ## What ASSIGN is responsible for, and what it is not
//!
//! ASSIGN owns the *legality* of a handoff. It refuses to hand out a task the
//! active plan does not name, a task whose dependencies are not finished, a
//! task an agent already holds, and an agent that is not available or that lacks
//! a capability the task requires. Every one of those checks runs before any
//! write, so a refused request writes nothing at all.
//!
//! ASSIGN does **not** decide *which* agent gets *which* task. That judgment
//! arrives from the caller, the same way PLAN's decomposition does. This is
//! deliberate for the same reason it was there: which agent should take
//! `AUTH-42` is a scheduling judgment, and keeping it outside this function is
//! what keeps the function deterministic and testable. A later phase will build
//! the scheduler on top of [`ready_tasks`] and this step; it is not this step.
//!
//! ## Why the handoff and the status move are one write
//!
//! The store's [`Store::assign_and_start`] does not just insert an assignment.
//! It releases whoever held the task, inserts the new assignment, moves the
//! task to `in_progress`, and records the status transition — in one
//! transaction. The alternative, an assignment followed by a separate status
//! update, has a window in which the task is assigned but still reads `todo`.
//! A crash in that window is exactly the state the loop cannot recover from
//! cleanly, and it is invisible to a reader: nothing about either row says the
//! write was interrupted. Folding the two writes together removes the window,
//! which is why this module never calls `assign_task` on its own.
//!
//! ## Readiness is computed, never stored
//!
//! [`ready_tasks`] derives eligibility from the plan and the tasks' current
//! statuses every time it is called. There is no `ready` flag to fall out of
//! step, and no cached set to invalidate — the same reasoning
//! [`Task::is_ready_given`] already applies at the domain level.

use director_domain::assignment::AgentAssignment;
use director_domain::ids::{AgentId, AssignmentId, ProjectId, TaskId};
use director_domain::plan::Plan;
use director_domain::task::{Task, TaskStatus};
use director_domain::{AgentRepository, AssignmentRepository, PlanRepository, TaskRepository};
use director_store::Store;

use crate::AssignError;

/// A request to hand one task to one agent.
///
/// The caller names every id. That keeps assignment auditable — the request is
/// a complete statement of who is being given what, and the caller's choice is
/// recorded in the assignment history rather than reconstructed from a policy.
#[derive(Debug, Clone)]
pub struct AssignRequest {
    /// The project whose active plan the task belongs to. The plan is looked up
    /// by project, so a task id is only meaningful in the context of the
    /// project currently executing it.
    pub project_id: ProjectId,
    /// The task to hand out. It must be a task the project's *active* plan
    /// names; a task from a superseded plan is not Orqyn's current intention,
    /// and reassigning one is a caller's deliberate act, not this step's
    /// default.
    pub task_id: TaskId,
    /// The agent taking the task. Must be registered and available, and must
    /// declare every capability the task requires.
    pub agent_id: AgentId,
    /// The identifier the new assignment will have. Each tenure is its own row,
    /// so an id is used once for the life of the project.
    pub assignment_id: AssignmentId,
}

/// The outcome of an ASSIGN step: an assignment in force and the task it
/// started.
///
/// Both are read back from the store rather than echoed from the request, so a
/// caller asserts against what was written, not what was asked for.
#[derive(Debug, Clone)]
pub struct Assigned {
    /// The assignment, now active and recorded in the task's history.
    pub assignment: AgentAssignment,
    /// The task, now `in_progress`.
    pub task: Task,
    /// The agent this handoff displaced, if the task was already held. The
    /// departed tenure is retained — marked `released` as `reassigned`, never
    /// deleted — so "who had this task, and why did they stop" stays
    /// answerable.
    pub displaced: Option<AgentId>,
}

/// Run the ASSIGN step: hand a task to an agent and start it.
///
/// The handoff, the task's move to `in_progress`, and the status transition that
/// records it are one store transaction, so the outcome is all or nothing — an
/// assigned task is an in-progress task, or nothing was written.
pub async fn assign(store: &Store, request: AssignRequest) -> Result<Assigned, AssignError> {
    assign_at(store, request, chrono::Utc::now()).await
}

/// [`assign`], with the assignment time supplied by the caller. Exposed so a
/// test can pin the clock and distinguish two assignments; production passes
/// [`chrono::Utc::now`].
pub async fn assign_at(
    store: &Store,
    request: AssignRequest,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Assigned, AssignError> {
    // 1. What is the project currently executing? Only the active plan's tasks
    //    are Orqyn's current intention; a task a superseded plan named is
    //    history, and handing one out would restart abandoned work.
    let plan = store
        .plans()
        .active_plan_for_project(&request.project_id)
        .await?;
    let Some(plan) = plan else {
        return Err(AssignError::NoActivePlan(request.project_id.clone()));
    };

    // 2. The task as it stands. Loaded before validation rather than trusted
    //    from the request, because legality is a question about the world: the
    //    task's real status and its real dependencies are what decide it.
    let task = store.tasks().get_task(&request.task_id).await?;

    // 3. Every other task in the project, to evaluate dependencies against.
    //    Readiness asks whether each dependency is `done` — not which plan did
    //    it — so work a superseded plan already finished still counts, and a
    //    dependency that will never be done is what makes this a replanning
    //    signal rather than an assignment.
    let plan_tasks = store.tasks().list_tasks(&request.project_id).await?;

    // 4. The agent as it stands, and whether anything is already holding the
    //    task. Both are inputs to validation, so both are loaded before any
    //    write.
    let agent = store.agents().get_agent(&request.agent_id).await?;
    let sitting = store
        .assignments()
        .active_assignment_for_task(&request.task_id)
        .await?;

    // 5. The legality checks. Pure, once the four loads have happened — this is
    //    the half of the step that can be tested without a store, as
    //    [`validate_assignment`].
    validate_assignment(&plan, &plan_tasks, &task, &agent, sitting.as_ref())?;

    // 6. The handoff, atomic. Everything from here is one transaction in the
    //    store: release any sitting tenant, insert this assignment, move the
    //    task to `in_progress`, record the transition.
    let (assignment, task, displaced) = store
        .assign_and_start(
            &request.task_id,
            &request.agent_id,
            &request.assignment_id,
            now,
        )
        .await?;

    Ok(Assigned {
        assignment,
        task,
        displaced,
    })
}

/// The tasks in the active plan that can be handed out right now, in the plan's
/// execution order.
///
/// A task is ready when it is still `todo`, every dependency it names is `done`,
/// and no agent currently holds it. This is the read a scheduler will make to
/// decide what to hand out — pure, writing nothing, so a caller can survey the
/// plan without committing to any assignment.
///
/// Tasks already `in_progress` are absent: an agent has them, and a second hand
/// would be a reassignment, not an assignment. Tasks whose dependencies failed
/// are absent too — a failed dependency does not unblock its dependents, and
/// handing out work whose premise has broken is how silent rework happens.
pub async fn ready_tasks(store: &Store, project_id: &ProjectId) -> Result<Vec<Task>, AssignError> {
    let plan = store.plans().active_plan_for_project(project_id).await?;
    let Some(plan) = plan else {
        return Err(AssignError::NoActivePlan(project_id.clone()));
    };
    let tasks = store.tasks().list_tasks(project_id).await?;
    Ok(ready_in_plan(&plan, &tasks))
}

/// Which of `tasks` are assignable right now, in the plan's execution order.
///
/// Extracted from [`ready_tasks`] so the readiness rule is testable without a
/// store, and so a caller can apply it to a task list it already has.
pub fn ready_in_plan(plan: &Plan, tasks: &[Task]) -> Vec<Task> {
    // A task's status is looked up by id, so readiness is a pure function of
    // the task list — no stored flag, no cache to invalidate.
    let status_of = |id: &TaskId| {
        tasks
            .iter()
            .find(|task| &task.id == id)
            .map(|task| task.status)
    };

    let mut ready = Vec::new();
    for id in &plan.task_ids {
        let Some(task) = tasks.iter().find(|task| &task.id == id) else {
            // A plan can name a task that is not in this list — a task removed
            // by a later write, or a list scoped more narrowly than the plan.
            // It is simply not handable here.
            continue;
        };
        if task.status != TaskStatus::Todo {
            continue;
        }
        if !task.is_ready_given(status_of) {
            continue;
        }
        ready.push(task.clone());
    }
    ready
}

/// The pure half of validation: given the active plan, its tasks, the task being
/// assigned, the agent taking it, and any assignment already holding it, is the
/// handoff legal?
///
/// No store, no I/O, no clock — everything about the world has already been
/// loaded by [`assign_at`]. Extracted so the legality rules can be tested
/// directly, and so a caller can reject a handout before it pays for a write.
pub fn validate_assignment(
    plan: &Plan,
    plan_tasks: &[Task],
    task: &Task,
    agent: &director_domain::agent::Agent,
    sitting: Option<&AgentAssignment>,
) -> Result<(), AssignError> {
    // The plan must be the project's current intention, and the task must be
    // one it names — not a task a superseded plan owned, and not a task that
    // belongs to a different project's plan.
    if !plan.status.is_authoritative() {
        return Err(AssignError::PlanNotActive(plan.id.clone()));
    }
    if !plan.task_ids.iter().any(|id| id == &task.id) {
        return Err(AssignError::TaskNotInPlan {
            task: task.id.clone(),
            plan: plan.id.clone(),
        });
    }

    // A task already held is not this step's to hand out. `assign_and_start`
    // would displace the sitting agent silently; refusing instead means a
    // reassignment has to be asked for on purpose, and the caller sees the
    // tenure it would end.
    if let Some(sitting) = sitting {
        return Err(AssignError::TaskAlreadyAssigned {
            task: task.id.clone(),
            agent: sitting.agent_id.clone(),
            assignment: sitting.id.clone(),
        });
    }

    // A task that is not `todo` is not handable: finished work needs no agent,
    // blocked work needs a blocker resolved, and work awaiting verification
    // needs VERIFY. The state, not the request, says which is which.
    if task.status != TaskStatus::Todo {
        return Err(AssignError::TaskNotTodo {
            task: task.id.clone(),
            status: task.status,
        });
    }

    // Every dependency must be `done` — a `failed` or `cancelled` dependency
    // does not unblock the task, which is the point where the caller should
    // replan rather than hand out work whose premise no longer holds.
    let status_of = |id: &TaskId| {
        plan_tasks
            .iter()
            .find(|other| &other.id == id)
            .map(|other| other.status)
    };
    let unmet: Vec<TaskId> = task
        .dependencies
        .iter()
        .filter(|dep| !matches!(status_of(dep), Some(TaskStatus::Done)))
        .cloned()
        .collect();
    if !unmet.is_empty() {
        return Err(AssignError::DependenciesNotDone {
            task: task.id.clone(),
            unmet,
        });
    }

    // The agent has to be able to take the work. An agent that is busy, stale,
    // or gone is not available, and one that lacks a required capability is
    // being set up to fail.
    if !agent.status.can_accept_work() {
        return Err(AssignError::AgentUnavailable {
            agent: agent.id.clone(),
            status: agent.status,
        });
    }
    let missing: Vec<_> = task
        .required_capabilities
        .iter()
        .filter(|required| !agent.has_capability(required))
        .collect();
    if !missing.is_empty() {
        return Err(AssignError::AgentMissingCapabilities {
            agent: agent.id.clone(),
            task: task.id.clone(),
            missing: missing.into_iter().cloned().collect(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use director_domain::agent::{Agent, AgentStatus, Harness};
    use director_domain::capability::Capability;
    use director_domain::ids::{AgentId, MachineId, PlanId, ProjectId, TaskId};
    use director_domain::plan::{Plan, PlanStatus};
    use director_domain::task::{Task, TaskStatus};

    /// A plan with `ids` as its task order, authoritative by default.
    fn plan(ids: &[&str]) -> Plan {
        let mut plan = Plan::draft(
            PlanId::from_string("PLAN-1"),
            ProjectId::from_string("PROJ-1"),
            "add authentication",
            "why",
        );
        plan.status = PlanStatus::Active;
        for id in ids {
            plan.add_task(TaskId::from_string(*id));
        }
        plan
    }

    /// A task in the plan, `todo` unless the caller says otherwise.
    fn task(id: &str, status: TaskStatus) -> Task {
        let mut task = Task::for_project(
            ProjectId::from_string("PROJ-1"),
            TaskId::from_string(id),
            id,
            "do it",
        );
        task.status = status;
        task
    }

    /// The task list for a plan, with a status per task.
    fn tasks(with_status: &[(&str, TaskStatus)]) -> Vec<Task> {
        with_status.iter().map(|(id, s)| task(id, *s)).collect()
    }

    fn agent(status: AgentStatus, caps: Vec<Capability>) -> Agent {
        let mut agent = Agent::register(
            AgentId::from_string("AGENT-1"),
            "claude-a",
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            caps,
        );
        agent.status = status;
        agent
    }

    #[test]
    fn a_todo_task_with_done_dependencies_is_handable() {
        let plan = plan(&["A", "B"]);
        let tasks = tasks(&[("A", TaskStatus::Done), ("B", TaskStatus::Todo)]);
        let agent = agent(AgentStatus::Available, vec![Capability::Coding]);

        assert!(validate_assignment(&plan, &tasks, &tasks[1], &agent, None).is_ok());
    }

    #[test]
    fn a_task_from_a_superseded_plan_is_refused() {
        // The plan is history: its tasks are not Orqyn's current intention.
        let mut plan = plan(&["A"]);
        plan.status = PlanStatus::Superseded;
        let tasks = tasks(&[("A", TaskStatus::Todo)]);
        let agent = agent(AgentStatus::Available, vec![]);

        let err = validate_assignment(&plan, &tasks, &tasks[0], &agent, None).unwrap_err();
        assert!(
            matches!(err, AssignError::PlanNotActive(ref p) if *p == PlanId::from_string("PLAN-1")),
            "got {err:?}"
        );
    }

    #[test]
    fn a_task_the_plan_does_not_name_is_refused() {
        let plan = plan(&["A"]);
        let tasks = tasks(&[("A", TaskStatus::Todo)]);
        let outsider = task("Z", TaskStatus::Todo);
        let agent = agent(AgentStatus::Available, vec![]);

        let err = validate_assignment(&plan, &tasks, &outsider, &agent, None).unwrap_err();
        assert!(
            matches!(err, AssignError::TaskNotInPlan { ref task, .. } if *task == TaskId::from_string("Z")),
            "got {err:?}"
        );
    }

    #[test]
    fn a_task_an_agent_already_holds_is_refused() {
        let plan = plan(&["A"]);
        let tasks = tasks(&[("A", TaskStatus::Todo)]);
        let agent = agent(AgentStatus::Available, vec![]);
        let sitting = AgentAssignment::propose(
            AssignmentId::from_string("ASG-old"),
            TaskId::from_string("A"),
            AgentId::from_string("AGENT-2"),
        );

        let err =
            validate_assignment(&plan, &tasks, &tasks[0], &agent, Some(&sitting)).unwrap_err();
        assert!(
            matches!(err, AssignError::TaskAlreadyAssigned { ref task, ref agent, ref assignment }
                if *task == TaskId::from_string("A")
                && *agent == AgentId::from_string("AGENT-2")
                && *assignment == AssignmentId::from_string("ASG-old")),
            "got {err:?}"
        );
    }

    #[test]
    fn a_finished_task_is_refused() {
        let plan = plan(&["A"]);
        let tasks = tasks(&[("A", TaskStatus::Done)]);
        let agent = agent(AgentStatus::Available, vec![]);

        let err = validate_assignment(&plan, &tasks, &tasks[0], &agent, None).unwrap_err();
        assert!(
            matches!(err, AssignError::TaskNotTodo { ref task, status }
                if *task == TaskId::from_string("A") && status == TaskStatus::Done),
            "got {err:?}"
        );
    }

    #[test]
    fn a_task_with_an_unfinished_dependency_is_refused() {
        let plan = plan(&["A", "B"]);
        let mut tasks = tasks(&[("A", TaskStatus::Todo), ("B", TaskStatus::Todo)]);
        tasks[1].dependencies = vec![TaskId::from_string("A")];
        let agent = agent(AgentStatus::Available, vec![]);

        let err = validate_assignment(&plan, &tasks, &tasks[1], &agent, None).unwrap_err();
        assert!(
            matches!(err, AssignError::DependenciesNotDone { ref task, ref unmet }
                if *task == TaskId::from_string("B") && *unmet == vec![TaskId::from_string("A")]),
            "got {err:?}"
        );
    }

    #[test]
    fn a_failed_dependency_does_not_unblock_the_task() {
        // A dependency that failed is not done; handing out the dependent task
        // would be work on a premise that no longer holds.
        let plan = plan(&["A", "B"]);
        let mut tasks = tasks(&[("A", TaskStatus::Failed), ("B", TaskStatus::Todo)]);
        tasks[1].dependencies = vec![TaskId::from_string("A")];
        let agent = agent(AgentStatus::Available, vec![]);

        let err = validate_assignment(&plan, &tasks, &tasks[1], &agent, None).unwrap_err();
        assert!(
            matches!(err, AssignError::DependenciesNotDone { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_dependency_outside_the_plan_blocks() {
        // The task depends on work the active plan does not include, so the
        // plan cannot satisfy the premise. That is a replan signal.
        let plan = plan(&["B"]);
        let mut b = task("B", TaskStatus::Todo);
        b.dependencies = vec![TaskId::from_string("A")];
        let agent = agent(AgentStatus::Available, vec![]);

        let err = validate_assignment(&plan, &[b.clone()], &b, &agent, None).unwrap_err();
        assert!(
            matches!(err, AssignError::DependenciesNotDone { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_busy_agent_is_refused() {
        let plan = plan(&["A"]);
        let tasks = tasks(&[("A", TaskStatus::Todo)]);

        for status in [
            AgentStatus::Busy,
            AgentStatus::Stale,
            AgentStatus::Disconnected,
            AgentStatus::Offline,
        ] {
            let agent = agent(status, vec![]);
            let err = validate_assignment(&plan, &tasks, &tasks[0], &agent, None).unwrap_err();
            assert!(
                matches!(err, AssignError::AgentUnavailable { ref agent, .. }
                    if *agent == AgentId::from_string("AGENT-1")),
                "status {status:?} should be refused, got {err:?}"
            );
        }
    }

    #[test]
    fn an_agent_missing_a_required_capability_is_refused() {
        let plan = plan(&["A"]);
        let mut tasks = tasks(&[("A", TaskStatus::Todo)]);
        tasks[0].required_capabilities = vec![Capability::Testing];
        let specialist = agent(AgentStatus::Available, vec![Capability::Testing]);
        let generalist = agent(AgentStatus::Available, vec![Capability::Coding]);

        // The specialist passes.
        assert!(validate_assignment(&plan, &tasks, &tasks[0], &specialist, None).is_ok());
        // The generalist's `Coding` is a super-capability, so it passes too.
        assert!(validate_assignment(&plan, &tasks, &tasks[0], &generalist, None).is_ok());

        // An agent with neither is refused, and the message names what is
        // missing.
        let none = agent(AgentStatus::Available, vec![Capability::Documentation]);
        let err = validate_assignment(&plan, &tasks, &tasks[0], &none, None).unwrap_err();
        assert!(
            matches!(err, AssignError::AgentMissingCapabilities { ref missing, .. } if *missing == vec![Capability::Testing]),
            "got {err:?}"
        );
    }

    #[test]
    fn ready_in_plan_lists_only_the_handable_tasks_in_plan_order() {
        let plan = plan(&["A", "B", "C", "D"]);
        let mut b = task("B", TaskStatus::Todo);
        b.dependencies = vec![TaskId::from_string("A")];
        let tasks = vec![
            task("A", TaskStatus::Done),
            b,
            task("C", TaskStatus::InProgress),
            task("D", TaskStatus::Todo),
        ];

        let ready = ready_in_plan(&plan, &tasks);
        let ids: Vec<_> = ready
            .iter()
            .map(|task| task.id.as_str().to_string())
            .collect();
        // B is ready now that A is done; C is already being worked; D is ready.
        // Order is the plan's order, not the task list's.
        assert_eq!(ids, vec!["B", "D"]);
    }

    #[test]
    fn ready_in_plan_skips_a_task_whose_dependency_failed() {
        let plan = plan(&["A", "B"]);
        let mut b = task("B", TaskStatus::Todo);
        b.dependencies = vec![TaskId::from_string("A")];
        let tasks = vec![task("A", TaskStatus::Failed), b];

        assert!(ready_in_plan(&plan, &tasks).is_empty());
    }

    #[test]
    fn ready_in_plan_ignores_a_task_the_plan_does_not_name() {
        // A task in the list but not in the plan is not handable, even if it is
        // otherwise ready.
        let plan = plan(&["A"]);
        let tasks = vec![task("A", TaskStatus::Todo), task("Z", TaskStatus::Todo)];

        let ready = ready_in_plan(&plan, &tasks);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, TaskId::from_string("A"));
    }

    #[test]
    fn a_plan_with_no_tasks_yields_no_ready_work() {
        let plan = plan(&[]);
        assert!(ready_in_plan(&plan, &[]).is_empty());
    }
}
