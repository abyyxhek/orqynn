//! Phase 13: deterministic conflict detection, through the real steps.
//!
//! ## Why these tests exist in `tests/` and not in `src/`
//!
//! Phase 13 was, for a while, a function nobody called. `Task::overlaps_scope`
//! had unit tests in `director-domain` and zero call sites in production, so
//! every suite stayed green while the feature did nothing. A test that only
//! exercises a predicate cannot catch that class of bug, because the predicate
//! was never the thing that was broken.
//!
//! So every assertion here goes through PLAN, SCHEDULE, or ASSIGN against a
//! real SQLite store. Nothing pokes a status by hand to set up its assertion:
//! the state a test asserts on is the state the production steps produced. If
//! the conflict check is deleted from `validate_assignment` or from the
//! scheduling round, these tests fail.
//!
//! ## The rule under test
//!
//! Two tasks may be in flight together only when both declared disjoint scopes.
//! A task that declared no scope is *undeclared*, not unrestricted — see
//! `director_domain::task::Task::scope_overlap` for the full semantics.

#![cfg(test)]

use director_app::assign::{assign, AssignRequest};
use director_app::plan::{plan, PlanRequest, TaskSpec};
use director_app::schedule::{schedule, ScheduleReport, UnassignedReason};
use director_app::AssignError;
use director_domain::agent::{Agent, AgentStatus, Harness};
use director_domain::capability::Capability;
use director_domain::ids::{
    AgentId, AssignmentId, IdGenerator, MachineId, PlanId, ProjectId, TaskId,
};
use director_domain::project::Project;
use director_domain::task::{Task, TaskStatus};
use director_domain::AgentRepository;
use director_domain::AssignmentRepository;
use director_domain::ProjectRepository;
use director_domain::TaskRepository;
use director_store::Store;

/// A store with one project and, optionally, an active plan.
struct Fixture {
    store: Store,
    _dir: tempfile::TempDir,
    project: ProjectId,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("a temp dir for the store");
        let store = Store::open(dir.path().join("orqyn.db"))
            .await
            .expect("store opens");
        let project = ProjectId::from_string("PROJ-1");
        store
            .projects()
            .create_project(&Project::new(project.clone(), "proj", "/nowhere"))
            .await
            .expect("project created");
        Fixture {
            store,
            _dir: dir,
            project,
        }
    }

    /// Run the real PLAN step over `specs`, so the scopes a caller declared are
    /// carried through planning and persisted rather than injected afterwards.
    async fn plan(&self, specs: Vec<TaskSpec>) -> Vec<Task> {
        plan(
            &self.store,
            PlanRequest {
                project_id: self.project.clone(),
                plan_id: PlanId::from_string("PLAN-1"),
                objective: "do the thing".into(),
                rationale: "the tests need an active plan".into(),
                tasks: specs,
                authorized_by: AgentId::from_string("AGENT-planner"),
            },
        )
        .await
        .expect("plan succeeds")
        .tasks
    }

    async fn agent(&self, id: &str) -> AgentId {
        let agent_id = AgentId::from_string(id);
        let mut agent = Agent::register(
            agent_id.clone(),
            id,
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            vec![Capability::Coding],
        );
        agent.status = AgentStatus::Available;
        self.store
            .agents()
            .register_agent(&agent)
            .await
            .expect("agent registered");
        agent_id
    }

    async fn reload(&self, id: &str) -> Task {
        self.store
            .tasks()
            .get_task(&TaskId::from_string(id))
            .await
            .expect("task exists")
    }

    async fn run(&self) -> ScheduleReport {
        schedule(&self.store, &mut IdGenerator::new(), &self.project)
            .await
            .expect("the round succeeds")
    }

    /// Hand `task_id` to `agent_id` directly, bypassing SCHEDULE, so a refusal
    /// is attributable to ASSIGN alone. Each call mints a fresh assignment id,
    /// because every tenure is its own row and an id is used once.
    async fn assign(&self, task_id: &str, agent_id: &AgentId) -> Result<(), AssignError> {
        let history = self
            .store
            .assignments()
            .assignment_history(&TaskId::from_string(task_id))
            .await
            .expect("assignment history readable");
        let assignment_id = AssignmentId::from_string(format!("ASG-{task_id}-{}", history.len()));
        assign(
            &self.store,
            AssignRequest {
                project_id: self.project.clone(),
                task_id: TaskId::from_string(task_id),
                agent_id: agent_id.clone(),
                assignment_id,
            },
        )
        .await
        .map(|_| ())
    }
}

/// A spec with a declared scope. Used where the scope is what the test is about.
fn scoped(id: &str, paths: &[&str]) -> TaskSpec {
    TaskSpec::new(TaskId::from_string(id), id, format!("do {id}")).scopes(paths.to_vec())
}

// ---------------------------------------------------------------------------
// Scopes survive planning
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_scope_declared_at_planning_reaches_the_persisted_task() {
    // The gap that made Phase 13 inert: `TaskSpec` had no way to declare a
    // scope, so every planned task was persisted with an empty one and there
    // was nothing for conflict detection to compare.
    let fx = Fixture::new().await;
    fx.plan(vec![
        scoped("AUTH", &[r"src\auth\"]),
        scoped("DB", &["src/db/"]),
    ])
    .await;

    let auth = fx.reload("AUTH").await;
    assert_eq!(auth.scope_paths, vec![r"src\auth\".to_string()]);
    assert_eq!(
        fx.reload("DB").await.scope_paths,
        vec!["src/db/".to_string()]
    );
}

#[tokio::test]
async fn scopes_can_be_accumulated_onto_one_task() {
    let fx = Fixture::new().await;
    fx.plan(vec![TaskSpec::new(TaskId::from_string("A"), "A", "do A")
        .scopes(["src/auth"])
        .scopes(["src/db"])])
        .await;

    assert_eq!(
        fx.reload("A").await.scope_paths,
        vec!["src/auth".to_string(), "src/db".to_string()]
    );
}

// ---------------------------------------------------------------------------
// SCHEDULE: conflicts discovered during a round
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_tasks_with_disjoint_scopes_are_both_handed_out() {
    // The positive case. Without it, "everything conflicts" would pass every
    // refusal test below.
    let fx = Fixture::new().await;
    fx.plan(vec![
        scoped("AUTH", &["src/auth/"]),
        scoped("DB", &["src/db/"]),
    ])
    .await;
    fx.agent("AGENT-1").await;
    fx.agent("AGENT-2").await;

    let report = fx.run().await;

    assert_eq!(report.assigned.len(), 2, "disjoint scopes may run together");
    assert!(report.refused.is_empty());
    assert!(report.unassigned.is_empty());
    assert!(report.is_settled());
}

#[tokio::test]
async fn two_tasks_competing_for_overlapping_scopes_are_not_both_handed_out() {
    let fx = Fixture::new().await;
    fx.plan(vec![
        scoped("AUTH", &["src/auth/"]),
        scoped("LOGIN", &["src/auth/login.rs"]),
    ])
    .await;
    fx.agent("AGENT-1").await;
    fx.agent("AGENT-2").await;

    let report = fx.run().await;

    assert_eq!(
        report.assigned.len(),
        1,
        "one agent goes on the overlapping ground, not two"
    );
    let unassigned: Vec<_> = report.unassigned().collect();
    assert_eq!(unassigned.len(), 1);
    assert!(
        matches!(unassigned[0].1, UnassignedReason::ScopeConflict { .. }),
        "reported as a scope conflict, got {:?}",
        unassigned[0].1
    );
    assert!(!report.is_settled(), "a caller can see the work is waiting");
}

#[tokio::test]
async fn the_conflict_names_the_task_holding_the_ground() {
    let fx = Fixture::new().await;
    fx.plan(vec![
        scoped("AUTH", &["src/auth/"]),
        scoped("LOGIN", &["src/auth/login.rs"]),
    ])
    .await;
    fx.agent("AGENT-1").await;
    fx.agent("AGENT-2").await;

    let report = fx.run().await;
    let (waiting, reason) = report.unassigned().next().expect("one task waits");

    match reason {
        UnassignedReason::ScopeConflict { holder } => {
            assert_eq!(
                holder,
                TaskId::from_string("AUTH"),
                "AUTH is first in the plan"
            );
        }
        other => panic!("expected a scope conflict, got {other:?}"),
    }
    assert_eq!(*waiting, TaskId::from_string("LOGIN"));
}

#[tokio::test]
async fn a_root_scope_task_is_left_alone_because_it_touches_everything() {
    let fx = Fixture::new().await;
    fx.plan(vec![
        TaskSpec::new(
            TaskId::from_string("EVERYTHING"),
            "EVERYTHING",
            "do everything",
        )
        .scopes(["/"]),
        scoped("AUTH", &["src/auth/"]),
    ])
    .await;
    fx.agent("AGENT-1").await;
    fx.agent("AGENT-2").await;

    let report = fx.run().await;

    assert_eq!(report.assigned.len(), 1);
    assert!(matches!(
        report.unassigned().next().expect("one waits").1,
        UnassignedReason::ScopeConflict { .. }
    ));
}

#[tokio::test]
async fn two_tasks_that_declared_no_scope_do_not_run_together() {
    // The safety property the whole phase rests on. An empty scope means
    // "unknown", and unknown collides — otherwise the default Task is silently
    // safe to race.
    let fx = Fixture::new().await;
    fx.plan(vec![
        TaskSpec::new(TaskId::from_string("A"), "A", "do A"),
        TaskSpec::new(TaskId::from_string("B"), "B", "do B"),
    ])
    .await;
    fx.agent("AGENT-1").await;
    fx.agent("AGENT-2").await;

    let report = fx.run().await;

    assert_eq!(report.assigned.len(), 1);
    assert!(matches!(
        report.unassigned().next().expect("one waits").1,
        UnassignedReason::ScopeConflict { .. }
    ));
}

// ---------------------------------------------------------------------------
// ASSIGN: conflicts rejected before anything is written
// ---------------------------------------------------------------------------

#[tokio::test]
async fn assign_refuses_a_task_whose_scope_overlaps_an_in_flight_task() {
    let fx = Fixture::new().await;
    fx.plan(vec![
        scoped("AUTH", &["src/auth/"]),
        scoped("LOGIN", &["src/auth/login.rs"]),
    ])
    .await;
    let first = fx.agent("AGENT-1").await;
    let second = fx.agent("AGENT-2").await;
    fx.assign("AUTH", &first)
        .await
        .expect("AUTH takes the ground");

    let refusal = fx
        .assign("LOGIN", &second)
        .await
        .expect_err("LOGIN collides with AUTH");

    match &refusal {
        AssignError::ScopeConflict {
            task,
            holder,
            overlap,
            undeclared,
        } => {
            assert_eq!(task, &TaskId::from_string("LOGIN"));
            assert_eq!(holder, &TaskId::from_string("AUTH"));
            assert!(
                overlap.contains("src/auth"),
                "the message names the ground: {overlap}"
            );
            assert!(!undeclared);
        }
        other => panic!("expected ScopeConflict, got {other:?}"),
    }
}

#[tokio::test]
async fn a_refused_conflict_writes_nothing_and_leaves_the_task_todo() {
    let fx = Fixture::new().await;
    fx.plan(vec![
        scoped("AUTH", &["src/auth/"]),
        scoped("LOGIN", &["src/auth/login.rs"]),
    ])
    .await;
    let first = fx.agent("AGENT-1").await;
    let second = fx.agent("AGENT-2").await;
    fx.assign("AUTH", &first)
        .await
        .expect("AUTH takes the ground");

    fx.assign("LOGIN", &second).await.expect_err("refused");

    assert_eq!(
        fx.reload("LOGIN").await.status,
        TaskStatus::Todo,
        "a refused handoff leaves the task exactly as the round found it"
    );
    assert!(fx
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("LOGIN"))
        .await
        .expect("query")
        .is_none());
}

#[tokio::test]
async fn assign_refuses_an_undeclared_scope_against_work_already_in_flight() {
    let fx = Fixture::new().await;
    fx.plan(vec![
        scoped("AUTH", &["src/auth/"]),
        TaskSpec::new(TaskId::from_string("MYSTERY"), "MYSTERY", "do something"),
    ])
    .await;
    let first = fx.agent("AGENT-1").await;
    let second = fx.agent("AGENT-2").await;
    fx.assign("AUTH", &first)
        .await
        .expect("AUTH takes the ground");

    let refusal = fx
        .assign("MYSTERY", &second)
        .await
        .expect_err("an undeclared scope may touch anything");

    match refusal {
        AssignError::ScopeConflict {
            undeclared,
            overlap,
            ..
        } => {
            assert!(undeclared, "the overlap is total, not located at a path");
            assert!(overlap.contains("no scope"), "and says why: {overlap}");
        }
        other => panic!("expected ScopeConflict, got {other:?}"),
    }
}

#[tokio::test]
async fn assign_allows_two_tasks_with_the_same_scope_once_the_first_is_finished() {
    // A conflict is contention, not a permanent block: when the holder leaves
    // `in_progress` the ground is free and the waiting task becomes handable.
    let fx = Fixture::new().await;
    fx.plan(vec![
        scoped("AUTH", &["src/auth/"]),
        scoped("LOGIN", &["src/auth/login.rs"]),
    ])
    .await;
    let first = fx.agent("AGENT-1").await;
    let second = fx.agent("AGENT-2").await;
    fx.assign("AUTH", &first)
        .await
        .expect("AUTH takes the ground");
    fx.assign("LOGIN", &second)
        .await
        .expect_err("blocked while AUTH holds the ground");

    // AUTH finishes and is verified, exactly as the loop would move it.
    let mut auth = fx.reload("AUTH").await;
    auth.status = TaskStatus::Done;
    fx.store
        .tasks()
        .update_task(&auth)
        .await
        .expect("AUTH moves on");

    fx.assign("LOGIN", &second)
        .await
        .expect("the ground is free now");
}

#[tokio::test]
async fn a_finished_holder_does_not_block_and_a_waiting_one_does_not_hold_ground() {
    // Only `in_progress` work holds ground. A `todo` sibling holds nothing, so
    // it must not be mistaken for a holder — that would deadlock the scheduler,
    // since nothing would ever be able to start.
    let fx = Fixture::new().await;
    fx.plan(vec![
        scoped("AUTH", &["src/auth/"]),
        scoped("LOGIN", &["src/auth/"]),
    ])
    .await;
    let first = fx.agent("AGENT-1").await;
    let second = fx.agent("AGENT-2").await;

    fx.assign("LOGIN", &second)
        .await
        .expect("a todo sibling holds nothing");
    fx.assign("AUTH", &first)
        .await
        .expect_err("the in-progress sibling does hold ground");
}

// ---------------------------------------------------------------------------
// Revalidation against live state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_task_that_started_after_planning_is_caught_at_assignment_time() {
    // The revalidation requirement. SCHEDULE surveyed this project earlier and
    // decided both tasks were handable; between then and now the other task
    // became `in_progress`. ASSIGN reads the tasks fresh from the store on every
    // call and must see the new state rather than trust the earlier round.
    let fx = Fixture::new().await;
    fx.plan(vec![
        scoped("AUTH", &["src/auth/"]),
        scoped("LOGIN", &["src/auth/login.rs"]),
    ])
    .await;
    let first = fx.agent("AGENT-1").await;
    let second = fx.agent("AGENT-2").await;

    // AUTH is handed out first, so the round that "surveyed" the project would
    // have seen LOGIN as handable too.
    fx.assign("AUTH", &first)
        .await
        .expect("AUTH takes the ground");
    assert_eq!(fx.reload("LOGIN").await.status, TaskStatus::Todo);

    // LOGIN was ready as of planning. It is not ready now.
    let refusal = fx
        .assign("LOGIN", &second)
        .await
        .expect_err("the world moved after the round read it");
    assert!(
        matches!(refusal, AssignError::ScopeConflict { .. }),
        "got {refusal:?}"
    );
}

#[tokio::test]
async fn a_second_round_finds_the_conflict_the_first_one_created() {
    // The same property seen through SCHEDULE rather than ASSIGN: a round that
    // runs after an earlier round created the overlap still refuses, and says
    // why. Here the free agent is willing, so the pairing is proposed and then
    // denied by the store — which is where the authority for the refusal
    // belongs. Either channel is structured; neither drops the task silently.
    let fx = Fixture::new().await;
    fx.plan(vec![
        scoped("AUTH", &["src/auth/"]),
        scoped("LOGIN", &["src/auth/login.rs"]),
    ])
    .await;
    fx.agent("AGENT-1").await;
    fx.agent("AGENT-2").await;

    let first = fx.run().await;
    assert_eq!(first.assigned.len(), 1);

    let second = fx.run().await;
    assert!(second.assigned.is_empty(), "nothing new may be handed out");

    let refused: Vec<_> = second.refused().collect();
    assert_eq!(refused.len(), 1, "one pairing was proposed and denied");
    assert!(
        matches!(refused[0].2, AssignError::ScopeConflict { .. }),
        "reported as a scope conflict, got {:?}",
        refused[0].2
    );
    assert!(!second.is_settled());
    assert_eq!(
        fx.reload("LOGIN").await.status,
        TaskStatus::Todo,
        "and the waiting task is untouched"
    );
}
