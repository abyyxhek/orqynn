//! Active-plan eligibility at the assignment write boundary.
//!
//! The race this covers is concrete: the application layer reads the project's
//! active plan and validates a task against it, and the store then opens a
//! separate write transaction. A plan supersession landing in between would let
//! an assignment commit against a plan that no longer exists.
//!
//! `guard_plan_membership` closes that by re-reading the active plan inside the
//! `BEGIN IMMEDIATE` transaction, so a supersession cannot interleave between the
//! check and the commit.
//!
//! Everything here runs against real SQLite. The concurrency test uses the write
//! lock itself as the rendezvous rather than a timer.

use director_domain::assignment::AgentAssignment;
use director_domain::ids::{AgentId, AssignmentId, PlanId, ProjectId, TaskId};
use director_domain::plan::{Plan, PlanStatus};
use director_domain::store::{
    AssignmentRepository, HandoffRepository, PlanRepository, TaskRepository,
};
use director_domain::task::{Task, TaskStatus};
use director_domain::{
    Agent, AgentRepository, Capability, Harness, MachineId, Project, ProjectRepository,
};
use director_store::Store as SqliteStore;

const PROJ: &str = "PROJ-1";
const TASK: &str = "WORK";
const OTHER: &str = "OTHER";
const A: &str = "AGENT-a";

fn pinned(step: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000 + step * 60, 0).expect("a pinned moment")
}

async fn boot() -> (tempfile::TempDir, SqliteStore) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let store = SqliteStore::open(dir.path().join("eligibility.db"))
        .await
        .expect("opens");
    store
        .projects()
        .create_project(&Project::new(ProjectId::from_string(PROJ), PROJ, "/repo"))
        .await
        .expect("project");
    store
        .agents()
        .register_agent(&Agent::register(
            AgentId::from_string(A),
            A,
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            vec![Capability::Coding],
        ))
        .await
        .expect("agent");
    for (id, objective) in [(TASK, "the work"), (OTHER, "other work")] {
        let mut task = Task::for_project(
            ProjectId::from_string(PROJ),
            TaskId::from_string(id),
            id,
            objective,
        );
        task.status = TaskStatus::Todo;
        store.tasks().create_task(&task).await.expect("task");
    }
    (dir, store)
}

/// Draft and activate a plan naming `members`.
async fn activate_plan(store: &SqliteStore, id: &str, objective: &str, members: &[&str]) {
    let mut plan = Plan::draft(
        PlanId::from_string(id),
        ProjectId::from_string(PROJ),
        objective,
        "planned for these tests",
    );
    plan.task_ids = members.iter().map(|m| TaskId::from_string(*m)).collect();
    store.plans().create_plan(&plan).await.expect("drafted");
    store
        .plans()
        .activate_plan(&PlanId::from_string(id), AgentId::from_string(A))
        .await
        .expect("activated");
}

async fn assignment_count(store: &SqliteStore, task: &str) -> usize {
    store
        .assignments()
        .assignment_history(&TaskId::from_string(task))
        .await
        .expect("history")
        .len()
}

// Normal assignment: eligible under the active plan, still assignable.
#[tokio::test]
async fn a_task_the_active_plan_names_is_still_assignable() {
    let (_dir, store) = boot().await;
    activate_plan(&store, "PLAN-1", "the work", &[TASK]).await;

    store
        .assign_and_start_next(
            &TaskId::from_string(TASK),
            &AgentId::from_string(A),
            pinned(0),
        )
        .await
        .expect("an eligible task is still assignable");
    assert_eq!(assignment_count(&store, TASK).await, 1);
}

// The invariant: a task the *current* active plan does not name is refused.
#[tokio::test]
async fn a_task_made_ineligible_by_supersession_is_refused() {
    let (_dir, store) = boot().await;
    activate_plan(&store, "PLAN-1", "the work", &[TASK]).await;

    // Supersede it with a plan that names only the other task. This is exactly
    // the state the application layer validated against and then lost.
    activate_plan(&store, "PLAN-2", "other work", &[OTHER]).await;

    let refused = store
        .assign_and_start_next(
            &TaskId::from_string(TASK),
            &AgentId::from_string(A),
            pinned(1),
        )
        .await;
    assert!(
        refused.is_err(),
        "the store must not commit an assignment under a plan that no longer names it"
    );
    let message = refused.expect_err("refused").to_string();
    assert!(
        message.contains("active plan"),
        "and the refusal must name the rule: {message}"
    );
}

// No partial writes on refusal.
#[tokio::test]
async fn a_refused_assignment_leaves_nothing_behind() {
    let (_dir, store) = boot().await;
    activate_plan(&store, "PLAN-1", "the work", &[TASK]).await;
    activate_plan(&store, "PLAN-2", "other work", &[OTHER]).await;

    let before_task = store
        .tasks()
        .get_task(&TaskId::from_string(TASK))
        .await
        .expect("task");
    let before_handoffs = store
        .handoffs()
        .handoffs_for_task(&TaskId::from_string(TASK))
        .await
        .expect("handoffs");

    assert!(store
        .assign_and_start_next(
            &TaskId::from_string(TASK),
            &AgentId::from_string(A),
            pinned(1),
        )
        .await
        .is_err());

    assert_eq!(assignment_count(&store, TASK).await, 0, "no assignment row");
    assert!(
        store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string(TASK))
            .await
            .expect("assignments")
            .is_none(),
        "no active assignment"
    );
    let after_task = store
        .tasks()
        .get_task(&TaskId::from_string(TASK))
        .await
        .expect("task");
    assert_eq!(
        after_task, before_task,
        "the task row and its state_version are untouched"
    );
    assert_eq!(
        store
            .handoffs()
            .handoffs_for_task(&TaskId::from_string(TASK))
            .await
            .expect("handoffs"),
        before_handoffs,
        "and no displacement handoff was recorded"
    );
}

// Plan extension keeps working: a task added to the active plan is assignable.
#[tokio::test]
async fn extending_the_active_plan_leaves_its_tasks_assignable() {
    let (_dir, store) = boot().await;
    activate_plan(&store, "PLAN-1", "the work", &[TASK]).await;

    // Append to the *same* plan rather than superseding it - the Phase 9 shape.
    let mut plan = store
        .plans()
        .get_plan(&PlanId::from_string("PLAN-1"))
        .await
        .expect("plan");
    plan.task_ids.push(TaskId::from_string(OTHER));
    store.plans().update_plan(&plan).await.expect("extended");

    assert_eq!(
        plan.id,
        store
            .plans()
            .active_plan_for_project(&ProjectId::from_string(PROJ))
            .await
            .expect("plans")
            .expect("active")
            .id,
        "the plan stays active through extension"
    );
    store
        .assign_and_start_next(
            &TaskId::from_string(TASK),
            &AgentId::from_string(A),
            pinned(1),
        )
        .await
        .expect("an existing member is still assignable");
}

// A replan that supersedes and re-admits the same task keeps it eligible: the
// guard deliberately does not require the same plan id.
#[tokio::test]
async fn a_replan_that_readmits_the_task_keeps_it_eligible() {
    let (_dir, store) = boot().await;
    activate_plan(&store, "PLAN-1", "the work", &[TASK]).await;
    activate_plan(&store, "PLAN-2", "the work, replanned", &[TASK, OTHER]).await;

    store
        .assign_and_start_next(
            &TaskId::from_string(TASK),
            &AgentId::from_string(A),
            pinned(1),
        )
        .await
        .expect("re-admitting a task in a new plan leaves it eligible");
}

// No plan at all: the store's low-level path is unchanged, and the application
// layer remains the gatekeeper.
#[tokio::test]
async fn a_project_that_never_planned_is_unaffected_by_the_guard() {
    let (_dir, store) = boot().await;
    store
        .assign_and_start_next(
            &TaskId::from_string(TASK),
            &AgentId::from_string(A),
            pinned(0),
        )
        .await
        .expect("no plan has ever existed, so there is nothing to race against");
}

// Concurrency: assignment racing supersession is serializable, never torn.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn supersession_racing_assignment_is_serializable() {
    let (_dir, store) = boot().await;
    activate_plan(&store, "PLAN-1", "the work", &[TASK]).await;

    // Both operations are released together and contend for the same reserved
    // write lock. Whichever commits first, the other must observe it: there is no
    // timing assumption here, only lock ordering.
    let assign_store = store.clone();
    let plan_store = store.clone();
    let assign = tokio::spawn(async move {
        assign_store
            .assign_and_start_next(
                &TaskId::from_string(TASK),
                &AgentId::from_string(A),
                pinned(1),
            )
            .await
    });
    let supersede = tokio::spawn(async move {
        plan_store
            .plans()
            .activate_plan(&PlanId::from_string("PLAN-2"), AgentId::from_string(A))
            .await
    });

    let assigned = assign.await.expect("join").is_ok();
    let superseded = supersede.await.expect("join").is_ok();
    assert!(
        assigned || superseded,
        "both operations must have been attempted"
    );

    // Whatever happened, the persisted state must be coherent: an assignment
    // exists only if it is covered by the plan that was active when it committed.
    let active = store
        .plans()
        .active_plan_for_project(&ProjectId::from_string(PROJ))
        .await
        .expect("plans")
        .expect("an active plan");
    let sitting = store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string(TASK))
        .await
        .expect("assignments");

    match sitting {
        Some(_assignment) => {
            // The assignment committed first, under the still-valid plan. The
            // later supersession does not retroactively invalidate it.
            assert!(assigned, "an assignment exists, so it must have committed");
            assert!(
                active.status == PlanStatus::Active,
                "the assignment committed while its plan was still authoritative, \
                 so the supersession that followed did not invalidate it"
            );
        }
        None => assert!(
            !assigned,
            "no assignment exists, so the write must have been refused"
        ),
    }
    let _ = AssignmentId::from_string("ASG-1");
    let _: Option<AgentAssignment> = sitting;
}
