//! Integration tests for the ASSIGN step against a real SQLite store.
//!
//! These are integration tests for the same reason OBSERVE's are: the risk in
//! this step is not that `assign` calls the right methods, but that the layers
//! actually line up. A mock could confirm that `active_plan_for_project` is
//! consulted; it cannot show that the plan's `task_ids` and the rows
//! `list_tasks` returns agree, or that the assignment and the task's status move
//! landed together. So every test here works against a real store on disk and
//! asserts by *reloading* the records, not by inspecting the value the call
//! returned.
//!
//! The property that matters most, and the one a validation bug would silently
//! break, is that a refused assignment writes nothing: the task is still `todo`
//! and the sitting agent still holds it. That is what makes retrying cheap, and
//! several tests below check it directly.

use director_app::assign::{assign, ready_tasks, AssignRequest};
use director_app::AssignError;
use director_domain::agent::{Agent, AgentStatus, Harness};
use director_domain::capability::Capability;
use director_domain::ids::{AgentId, AssignmentId, MachineId, PlanId, ProjectId, TaskId};
use director_domain::plan::Plan;
use director_domain::project::Project;
use director_domain::task::{Task, TaskStatus};
use director_domain::{AgentRepository, AssignmentRepository, ProjectRepository, TaskRepository};
use director_store::Store;

/// A store and the id of a project with an active plan, ready for a test to
/// assign from.
struct Fixture {
    store: Store,
    _dir: tempfile::TempDir,
    project: ProjectId,
}

impl Fixture {
    /// A store with one project and one active plan holding `tasks`, all `todo`.
    /// Dependencies are wired from each task's `depends_on`.
    async fn new(tasks: &[TestTask]) -> Self {
        let dir = tempfile::TempDir::new().expect("a temp dir for the store");
        let store = Store::open(dir.path().join("orqyn.db"))
            .await
            .expect("store opens");

        let project = ProjectId::from_string("PROJ-1");
        store
            .projects()
            .create_project(&Project::new(project.clone(), project.as_str(), "/nowhere"))
            .await
            .expect("project created");

        let plan_id = PlanId::from_string("PLAN-1");
        let mut plan = Plan::draft(
            plan_id.clone(),
            project.clone(),
            "add authentication",
            "test rationale",
        );
        let built: Vec<Task> = tasks
            .iter()
            .map(|spec| {
                let mut task = Task::for_project(
                    project.clone(),
                    TaskId::from_string(spec.id),
                    spec.id,
                    spec.objective,
                );
                task.status = TaskStatus::Todo;
                task.dependencies = spec.depends_on.clone();
                task.required_capabilities = spec.capabilities.clone();
                task
            })
            .collect();
        for task in &built {
            plan.add_task(task.id.clone());
        }

        // The same transactional operation PLAN uses, so the plan these tests
        // assign from is a plan exactly as the loop would create one.
        store
            .create_active_plan(
                &plan,
                &built,
                &AgentId::from_string("AGENT-planner"),
                chrono::Utc::now(),
            )
            .await
            .expect("plan created and activated");

        Fixture {
            store,
            _dir: dir,
            project,
        }
    }

    /// Register an agent, available unless the caller says otherwise.
    async fn agent(&self, id: &str, status: AgentStatus, caps: Vec<Capability>) -> AgentId {
        let agent_id = AgentId::from_string(id);
        let mut agent = Agent::register(
            agent_id.clone(),
            id,
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            caps,
        );
        agent.status = status;
        self.store
            .agents()
            .register_agent(&agent)
            .await
            .expect("agent registered");
        agent_id
    }

    fn request(&self, task: &str, agent: &AgentId, assignment: &str) -> AssignRequest {
        AssignRequest {
            project_id: self.project.clone(),
            task_id: TaskId::from_string(task),
            agent_id: agent.clone(),
            assignment_id: AssignmentId::from_string(assignment),
        }
    }

    async fn reload_task(&self, id: &str) -> Task {
        self.store
            .tasks()
            .get_task(&TaskId::from_string(id))
            .await
            .expect("task exists")
    }
}

struct TestTask {
    id: &'static str,
    objective: &'static str,
    depends_on: Vec<TaskId>,
    capabilities: Vec<Capability>,
}

fn task(id: &'static str) -> TestTask {
    TestTask {
        id,
        objective: "do it",
        depends_on: vec![],
        capabilities: vec![],
    }
}

/// One task with no dependencies — the simplest handable plan.
fn simple() -> Vec<TestTask> {
    vec![task("A")]
}

#[tokio::test]
async fn a_ready_task_is_assigned_and_becomes_in_progress() {
    let fx = Fixture::new(&simple()).await;
    let agent = fx.agent("AGENT-1", AgentStatus::Available, vec![]).await;

    let assigned = assign(&fx.store, fx.request("A", &agent, "ASG-1"))
        .await
        .expect("assignment succeeds");

    // Assert by reloading, not by trusting the returned value: the store is what
    // the next loop tick reads.
    let task = fx.reload_task("A").await;
    assert_eq!(task.status, TaskStatus::InProgress, "the task started");
    assert_eq!(assigned.task.status, TaskStatus::InProgress);

    let assignment = fx
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("A"))
        .await
        .expect("query")
        .expect("an active assignment exists");
    assert_eq!(assignment.agent_id, agent);
    assert_eq!(assignment.id, AssignmentId::from_string("ASG-1"));
    assert!(assignment.is_active());

    // The assignment and the status move are one write — the returned task is
    // the reloaded one, so the two cannot disagree.
    assert_eq!(assigned.assignment.id, assignment.id);
}

#[tokio::test]
async fn the_agent_record_points_at_the_task_it_was_given() {
    let fx = Fixture::new(&simple()).await;
    let agent = fx.agent("AGENT-1", AgentStatus::Available, vec![]).await;

    assign(&fx.store, fx.request("A", &agent, "ASG-1"))
        .await
        .expect("assignment succeeds");

    let agent = fx
        .store
        .agents()
        .get_agent(&agent)
        .await
        .expect("agent exists");
    assert_eq!(
        agent.current_task,
        Some(TaskId::from_string("A")),
        "the denormalized view reflects the assignment"
    );
}

#[tokio::test]
async fn a_task_whose_dependency_is_not_done_is_refused_and_nothing_is_written() {
    let fx = Fixture::new(&[
        TestTask {
            id: "A",
            objective: "model",
            depends_on: vec![],
            capabilities: vec![],
        },
        TestTask {
            id: "B",
            objective: "endpoints",
            depends_on: vec![TaskId::from_string("A")],
            capabilities: vec![],
        },
    ])
    .await;
    let agent = fx.agent("AGENT-1", AgentStatus::Available, vec![]).await;

    let err = assign(&fx.store, fx.request("B", &agent, "ASG-1"))
        .await
        .expect_err("B is not ready");

    assert!(
        matches!(err, AssignError::DependenciesNotDone { ref unmet, .. } if *unmet == vec![TaskId::from_string("A")]),
        "got {err:?}"
    );

    // Nothing was written: B is still waiting, and no assignment exists.
    assert_eq!(fx.reload_task("B").await.status, TaskStatus::Todo);
    assert!(fx
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("B"))
        .await
        .expect("query")
        .is_none());
}

#[tokio::test]
async fn a_task_an_agent_already_holds_is_refused_and_the_sitting_agent_keeps_it() {
    let fx = Fixture::new(&simple()).await;
    let first = fx.agent("AGENT-1", AgentStatus::Available, vec![]).await;
    let second = fx.agent("AGENT-2", AgentStatus::Available, vec![]).await;

    assign(&fx.store, fx.request("A", &first, "ASG-1"))
        .await
        .expect("first assignment");

    // A second agent asks for the same task. This must not displace the first
    // silently — that is a reassignment, and it has to be asked for on purpose.
    let err = assign(&fx.store, fx.request("A", &second, "ASG-2"))
        .await
        .expect_err("task is held");
    assert!(
        matches!(err, AssignError::TaskAlreadyAssigned { ref agent, .. } if *agent == first),
        "got {err:?}"
    );

    let holding = fx
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("A"))
        .await
        .expect("query")
        .expect("still assigned");
    assert_eq!(holding.agent_id, first, "the sitting agent keeps the task");

    let still_busy = fx.store.agents().get_agent(&first).await.expect("agent");
    assert_eq!(still_busy.current_task, Some(TaskId::from_string("A")));

    let not_given = fx.store.agents().get_agent(&second).await.expect("agent");
    assert_eq!(not_given.current_task, None);
}

#[tokio::test]
async fn an_unavailable_agent_is_refused_and_nothing_is_written() {
    let fx = Fixture::new(&simple()).await;
    let busy = fx.agent("AGENT-1", AgentStatus::Busy, vec![]).await;

    let err = assign(&fx.store, fx.request("A", &busy, "ASG-1"))
        .await
        .expect_err("agent is busy");
    assert!(
        matches!(err, AssignError::AgentUnavailable { ref agent, status } if *agent == busy && status == AgentStatus::Busy),
        "got {err:?}"
    );

    assert_eq!(fx.reload_task("A").await.status, TaskStatus::Todo);
    assert!(fx
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("A"))
        .await
        .expect("query")
        .is_none());
}

#[tokio::test]
async fn an_agent_missing_a_required_capability_is_refused() {
    let fx = Fixture::new(&[TestTask {
        id: "A",
        objective: "write the tests",
        depends_on: vec![],
        capabilities: vec![Capability::Testing],
    }])
    .await;
    let wrong = fx
        .agent(
            "AGENT-1",
            AgentStatus::Available,
            vec![Capability::Documentation],
        )
        .await;

    let err = assign(&fx.store, fx.request("A", &wrong, "ASG-1"))
        .await
        .expect_err("agent cannot test");
    assert!(
        matches!(err, AssignError::AgentMissingCapabilities { ref missing, .. } if *missing == vec![Capability::Testing]),
        "got {err:?}"
    );

    assert_eq!(fx.reload_task("A").await.status, TaskStatus::Todo);
}

#[tokio::test]
async fn a_task_from_a_superseded_plan_is_refused() {
    // Plan 2 supersedes plan 1 and contains only task B. Task A belongs to the
    // old plan; assigning it would restart abandoned work.
    let fx = Fixture::new(&[task("A")]).await;
    let agent = fx.agent("AGENT-1", AgentStatus::Available, vec![]).await;

    let mut plan2 = Plan::draft(
        PlanId::from_string("PLAN-2"),
        fx.project.clone(),
        "changed approach",
        "the model came first after all",
    );
    let task_b = Task::for_project(
        fx.project.clone(),
        TaskId::from_string("B"),
        "B",
        "do it differently",
    );
    plan2.add_task(TaskId::from_string("B"));
    fx.store
        .create_active_plan(
            &plan2,
            std::slice::from_ref(&task_b),
            &AgentId::from_string("AGENT-planner"),
            chrono::Utc::now(),
        )
        .await
        .expect("plan 2 activated");

    let err = assign(&fx.store, fx.request("A", &agent, "ASG-1"))
        .await
        .expect_err("A is not in the active plan");
    assert!(
        matches!(err, AssignError::TaskNotInPlan { ref task, .. } if *task == TaskId::from_string("A")),
        "got {err:?}"
    );

    assert_eq!(fx.reload_task("A").await.status, TaskStatus::Todo);
}

#[tokio::test]
async fn a_project_with_no_active_plan_has_nothing_to_assign() {
    // A store with a project and a task but no plan at all.
    let dir = tempfile::TempDir::new().expect("a temp dir for the store");
    let store = Store::open(dir.path().join("orqyn.db"))
        .await
        .expect("store opens");
    let project = ProjectId::from_string("PROJ-1");
    store
        .projects()
        .create_project(&Project::new(project.clone(), project.as_str(), "/nowhere"))
        .await
        .expect("project");
    let task = Task::for_project(project.clone(), TaskId::from_string("A"), "A", "do it");
    store.tasks().create_task(&task).await.expect("task");
    let agent_id = AgentId::from_string("AGENT-1");
    store
        .agents()
        .register_agent(&Agent::register(
            agent_id.clone(),
            "AGENT-1",
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            vec![],
        ))
        .await
        .expect("agent");

    let err = assign(
        &store,
        AssignRequest {
            project_id: project,
            task_id: TaskId::from_string("A"),
            agent_id,
            assignment_id: AssignmentId::from_string("ASG-1"),
        },
    )
    .await
    .expect_err("no plan");
    assert!(matches!(err, AssignError::NoActivePlan(_)), "got {err:?}");
}

#[tokio::test]
async fn ready_tasks_reports_exactly_the_handable_work_in_plan_order() {
    let fx = Fixture::new(&[
        TestTask {
            id: "A",
            objective: "model",
            depends_on: vec![],
            capabilities: vec![],
        },
        TestTask {
            id: "B",
            objective: "endpoints",
            depends_on: vec![TaskId::from_string("A")],
            capabilities: vec![],
        },
        TestTask {
            id: "C",
            objective: "docs",
            depends_on: vec![],
            capabilities: vec![],
        },
    ])
    .await;

    let ready = ready_tasks(&fx.store, &fx.project).await.expect("query");
    let ids: Vec<_> = ready.iter().map(|t| t.id.as_str().to_string()).collect();
    // B is blocked on A; A and C are handable, in the plan's order.
    assert_eq!(ids, vec!["A", "C"]);

    // Assigning A makes it leave the ready set — an in-progress task is not
    // handable a second time.
    let agent = fx.agent("AGENT-1", AgentStatus::Available, vec![]).await;
    assign(&fx.store, fx.request("A", &agent, "ASG-1"))
        .await
        .expect("assign A");

    let ready = ready_tasks(&fx.store, &fx.project).await.expect("query");
    let ids: Vec<_> = ready.iter().map(|t| t.id.as_str().to_string()).collect();
    assert_eq!(ids, vec!["C"], "A is in progress and B is still blocked");

    // Finishing A unblocks B. VERIFY is what moves a task to Done in the real
    // loop; here the store is written directly, since this test is about
    // readiness, not verification.
    let mut a = fx.reload_task("A").await;
    a.status = TaskStatus::Done;
    fx.store.tasks().update_task(&a).await.expect("task done");

    let ready = ready_tasks(&fx.store, &fx.project).await.expect("query");
    let ids: Vec<_> = ready.iter().map(|t| t.id.as_str().to_string()).collect();
    assert_eq!(ids, vec!["B", "C"], "B is unblocked now that A is done");
}

#[tokio::test]
async fn an_assigned_task_records_the_transition_in_its_history() {
    let fx = Fixture::new(&simple()).await;
    let agent = fx.agent("AGENT-1", AgentStatus::Available, vec![]).await;

    assign(&fx.store, fx.request("A", &agent, "ASG-1"))
        .await
        .expect("assignment");

    let history = fx
        .store
        .tasks()
        .task_history(&TaskId::from_string("A"))
        .await
        .expect("history");
    assert_eq!(history.len(), 1, "exactly one transition recorded");
    assert_eq!(history[0].from, TaskStatus::Todo);
    assert_eq!(history[0].to, TaskStatus::InProgress);
}

#[tokio::test]
async fn a_task_that_is_not_todo_is_refused() {
    // VERIFY is what finishes a task in the real loop; here the store is
    // written directly, because this test is about the step's guard, not about
    // verification.
    let fx = Fixture::new(&simple()).await;
    let agent = fx.agent("AGENT-1", AgentStatus::Available, vec![]).await;

    let mut done = fx.reload_task("A").await;
    done.status = TaskStatus::Done;
    fx.store
        .tasks()
        .update_task(&done)
        .await
        .expect("task done");

    let err = assign(&fx.store, fx.request("A", &agent, "ASG-1"))
        .await
        .expect_err("a finished task needs no agent");
    assert!(
        matches!(err, AssignError::TaskNotTodo { ref task, status } if *task == TaskId::from_string("A") && status == TaskStatus::Done),
        "got {err:?}"
    );
}

#[tokio::test]
async fn the_handoff_and_the_status_move_are_atomic() {
    // An agent that does not exist fails the assignment insert on its foreign
    // key, and the whole transaction rolls back. This is the property that a
    // two-call implementation would break: the task must not be left
    // `in_progress` with no assignment behind it.
    let fx = Fixture::new(&simple()).await;
    let ghost = AgentId::from_string("AGENT-nobody");

    let err = assign(&fx.store, fx.request("A", &ghost, "ASG-1"))
        .await
        .expect_err("unknown agent");
    assert!(matches!(err, AssignError::Store(_)), "got {err:?}");

    let task = fx.reload_task("A").await;
    assert_eq!(
        task.status,
        TaskStatus::Todo,
        "the task was not started: the write was all or nothing"
    );
    assert!(fx
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("A"))
        .await
        .expect("query")
        .is_none());

    // And the task's history records nothing for the attempt that rolled back.
    let history = fx
        .store
        .tasks()
        .task_history(&TaskId::from_string("A"))
        .await
        .expect("history");
    assert!(
        history.is_empty(),
        "a rolled-back write records no transition"
    );
}

#[tokio::test]
async fn an_assignment_can_be_released_and_the_task_reassigned() {
    // The store's handoff is what a *deliberate* reassignment will use once
    // that step exists; here it proves the tenure history the app step refuses
    // to displace is preserved when a caller asks for the move on purpose.
    let fx = Fixture::new(&simple()).await;
    let first = fx.agent("AGENT-1", AgentStatus::Available, vec![]).await;
    let second = fx.agent("AGENT-2", AgentStatus::Available, vec![]).await;

    assign(&fx.store, fx.request("A", &first, "ASG-1"))
        .await
        .expect("first assignment");

    let (_assignment, task, displaced) = fx
        .store
        .assign_and_start(
            &TaskId::from_string("A"),
            &second,
            &AssignmentId::from_string("ASG-2"),
            chrono::Utc::now(),
        )
        .await
        .expect("reassignment");
    assert_eq!(displaced, Some(first.clone()));
    assert_eq!(task.status, TaskStatus::InProgress);

    let history = fx
        .store
        .assignments()
        .assignment_history(&TaskId::from_string("A"))
        .await
        .expect("history");
    assert_eq!(
        history.len(),
        2,
        "both tenures are retained — assignment history is never destroyed"
    );
    assert!(history
        .iter()
        .any(|a| a.id == AssignmentId::from_string("ASG-1")));
    assert!(history
        .iter()
        .any(|a| a.id == AssignmentId::from_string("ASG-2")));

    let active = fx
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("A"))
        .await
        .expect("query")
        .expect("an active assignment exists");
    assert_eq!(active.agent_id, second);

    // The departing agent no longer points at the task; the new one does.
    let departing = fx.store.agents().get_agent(&first).await.expect("agent");
    assert_eq!(departing.current_task, None);
    let incoming = fx.store.agents().get_agent(&second).await.expect("agent");
    assert_eq!(incoming.current_task, Some(TaskId::from_string("A")));
}
