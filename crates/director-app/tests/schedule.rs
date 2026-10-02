//! Integration tests for the SCHEDULE step against a real SQLite store.
//!
//! These are integration tests for the same reason ASSIGN's and REPLAN's are:
//! the risk in this step is not that `match_agents` scores correctly, but that
//! the pairings survive contact with a real store. The eligibility filter reads
//! `list_agents` and `assignment_history`; the writes go through `assign`; the
//! report describes rows the next tick will read. A mock could confirm the
//! right calls were made; it cannot show that the agent a round committed is
//! the agent the store now shows holding the task, or that a task the round
//! could not pair is still `todo` with no assignment row.
//!
//! So every test works against a real store on disk, its preconditions are
//! produced by the loop's own steps where a loop step can produce them, and
//! every assertion reloads the records rather than trusting the value the call
//! returned.

use director_app::assign::{assign, AssignRequest};
use director_app::schedule::{schedule, schedule_at, ScheduleReport, UnassignedReason};
use director_app::ScheduleError;
use director_domain::agent::{Agent, AgentStatus, Harness};
use director_domain::assignment::ReleaseReason;
use director_domain::capability::Capability;
use director_domain::ids::{
    AgentId, AssignmentId, IdGenerator, MachineId, PlanId, ProjectId, TaskId,
};
use director_domain::plan::Plan;
use director_domain::project::Project;
use director_domain::task::{Task, TaskStatus};
use director_domain::{AgentRepository, AssignmentRepository, ProjectRepository, TaskRepository};
use director_store::Store;

/// A store and the id of a project with an active plan, ready for a round.
struct Fixture {
    store: Store,
    _dir: tempfile::TempDir,
    project: ProjectId,
}

impl Fixture {
    /// A store with one project and one active plan holding `tasks`, all `todo`.
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
                task.required_capabilities = spec.capabilities.clone();
                task
            })
            .collect();
        for task in &built {
            plan.add_task(task.id.clone());
        }

        // The same transactional operation PLAN uses, so the plan a round
        // schedules against is a plan exactly as the loop would create one.
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

    /// A store with a project and no plan at all — the state ASSIGN and
    /// SCHEDULE have to refuse.
    async fn unplanned() -> Self {
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

    async fn reload_task(&self, id: &str) -> Task {
        self.store
            .tasks()
            .get_task(&TaskId::from_string(id))
            .await
            .expect("task exists")
    }

    /// A fresh id generator, so each round mints its own assignment ids.
    fn ids() -> IdGenerator {
        IdGenerator::new()
    }

    async fn run(&self) -> ScheduleReport {
        schedule(&self.store, &mut Self::ids(), &self.project)
            .await
            .expect("the round succeeds")
    }
}

struct TestTask {
    id: &'static str,
    objective: &'static str,
    capabilities: Vec<Capability>,
}

fn task(id: &'static str) -> TestTask {
    TestTask {
        id,
        objective: "do it",
        capabilities: vec![],
    }
}

fn task_needing(id: &'static str, caps: Vec<Capability>) -> TestTask {
    TestTask {
        id,
        objective: "do it",
        capabilities: caps,
    }
}

/// Which agent a task's active assignment belongs to, or `None` if it is
/// unassigned. Asserted by reload, because the report's word is not the
/// store's.
async fn holder_of(store: &Store, task: &str) -> Option<AgentId> {
    store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string(task))
        .await
        .expect("query")
        .map(|assignment| assignment.agent_id)
}

#[tokio::test]
async fn every_ready_task_goes_to_a_distinct_available_agent() {
    let fx = Fixture::new(&[task("A"), task("B")]).await;
    fx.agent("AGENT-1", AgentStatus::Available, vec![Capability::Coding])
        .await;
    fx.agent("AGENT-2", AgentStatus::Available, vec![Capability::Coding])
        .await;

    let report = fx.run().await;

    // Both tasks were handed out, and to different agents: an agent holds one
    // task, so a round that paired both tasks with one agent would be wrong
    // even before the store refused the second.
    let a = holder_of(&fx.store, "A").await.expect("A was assigned");
    let b = holder_of(&fx.store, "B").await.expect("B was assigned");
    assert_ne!(a, b, "the two tasks went to the same agent");
    assert_eq!(report.assigned.len(), 2);
    assert!(report.refused.is_empty());
    assert!(report.unassigned.is_empty());
    assert!(report.is_settled());

    // And the tasks actually moved — the round's report is not the store's word.
    assert_eq!(fx.reload_task("A").await.status, TaskStatus::InProgress);
    assert_eq!(fx.reload_task("B").await.status, TaskStatus::InProgress);
}

#[tokio::test]
async fn one_agent_gets_one_task_and_the_loser_is_reported_not_reassigned() {
    let fx = Fixture::new(&[task("A"), task("B")]).await;
    fx.agent("AGENT-1", AgentStatus::Available, vec![Capability::Coding])
        .await;

    let report = fx.run().await;

    // A is first in the plan's order, so it takes the only agent.
    assert_eq!(
        holder_of(&fx.store, "A").await,
        Some(AgentId::from_string("AGENT-1"))
    );
    assert!(
        holder_of(&fx.store, "B").await.is_none(),
        "B was not handed out"
    );

    // B is reported as contention rather than refused: an eligible agent
    // existed in the snapshot, and the round itself committed it to A.
    assert_eq!(report.assigned.len(), 1);
    let unassigned: Vec<_> = report.unassigned().collect();
    assert_eq!(
        unassigned,
        vec![(
            &TaskId::from_string("B"),
            UnassignedReason::AgentTakenEarlier
        )],
        "B lost the agent to an earlier task, not to a shortage"
    );
    assert!(!report.is_settled(), "B is still waiting");

    // Nothing was written for B: it is still `todo` with no assignment row, so
    // the next round can hand it out.
    assert_eq!(fx.reload_task("B").await.status, TaskStatus::Todo);
}

#[tokio::test]
async fn a_specialist_takes_the_specialist_task_and_the_generalist_the_rest() {
    let fx = Fixture::new(&[task_needing("A", vec![Capability::Database]), task("B")]).await;
    fx.agent(
        "AGENT-specialist",
        AgentStatus::Available,
        vec![Capability::Database],
    )
    .await;
    fx.agent(
        "AGENT-generalist",
        AgentStatus::Available,
        vec![
            Capability::Coding,
            Capability::Frontend,
            Capability::Backend,
        ],
    )
    .await;

    fx.run().await;

    // The specialist gets the database task; the generalist is left free for B,
    // which only a generalist can cover. Ranking the other way would spend the
    // only agent who can do B on the task anybody could do.
    assert_eq!(
        holder_of(&fx.store, "A").await,
        Some(AgentId::from_string("AGENT-specialist")),
        "the specialist took the specialist task"
    );
    assert_eq!(
        holder_of(&fx.store, "B").await,
        Some(AgentId::from_string("AGENT-generalist")),
        "the generalist took the generalist task"
    );
}

#[tokio::test]
async fn work_no_available_agent_can_do_is_reported_and_left_alone() {
    let fx = Fixture::new(&[task_needing("A", vec![Capability::Security])]).await;
    fx.agent(
        "AGENT-1",
        AgentStatus::Available,
        vec![Capability::Frontend],
    )
    .await;

    let report = fx.run().await;

    // The work is real and ready, and nobody on the registry can do it. That is
    // reported as a staffing signal rather than silently staying `todo` round
    // after round — the reason this step is worth having over calling `assign`
    // by hand.
    assert!(report.assigned.is_empty());
    let unassigned: Vec<_> = report.unassigned().collect();
    assert_eq!(
        unassigned,
        vec![(&TaskId::from_string("A"), UnassignedReason::NoEligibleAgent)]
    );
    assert!(!report.is_settled());

    // And nothing was written: the task is still `todo` with no agent.
    assert_eq!(fx.reload_task("A").await.status, TaskStatus::Todo);
    assert!(holder_of(&fx.store, "A").await.is_none());
}

#[tokio::test]
async fn an_unavailable_agent_is_not_paired_even_if_capable() {
    let fx = Fixture::new(&[task("A")]).await;
    let capable = fx
        .agent("AGENT-1", AgentStatus::Available, vec![Capability::Coding])
        .await;

    // The agent goes quiet between the plan and the round. A scheduler that
    // paired by capability alone would hand work to an agent that cannot
    // answer for it.
    let mut agent = fx.store.agents().get_agent(&capable).await.expect("agent");
    agent.status = AgentStatus::Stale;
    fx.store
        .agents()
        .update_agent(&agent)
        .await
        .expect("agent updated");

    let report = fx.run().await;

    // An agent Orqyn cannot reach is not a shortage the round can fix; the task
    // stays exactly where it was.
    assert!(report.assigned.is_empty());
    assert_eq!(
        report.unassigned().count(),
        1,
        "the task was not handed to an unreachable agent"
    );
    assert!(holder_of(&fx.store, "A").await.is_none());
    assert_eq!(fx.reload_task("A").await.status, TaskStatus::Todo);
}

#[tokio::test]
async fn an_agent_who_never_held_the_task_is_preferred() {
    // AGENT-1 has already had this task and handed it back unfinished; the
    // round prefers an agent who has never tried it.
    let fx = Fixture::new(&[task("A")]).await;
    let first = fx
        .agent("AGENT-1", AgentStatus::Available, vec![Capability::Coding])
        .await;
    fx.agent("AGENT-2", AgentStatus::Available, vec![Capability::Coding])
        .await;

    // The loop's own step makes the first tenure, so the history the scheduler
    // reads is a real one.
    assign(
        &fx.store,
        AssignRequest {
            project_id: fx.project.clone(),
            task_id: TaskId::from_string("A"),
            agent_id: first.clone(),
            assignment_id: AssignmentId::from_string("ASG-1"),
        },
    )
    .await
    .expect("the first assignment");

    // No loop step yet produces "the task is back to `todo` and its former
    // holder is still available" — MONITOR's lease expiry marks the holder
    // `Disconnected` — so the precondition is set at the store level: the
    // tenure is released as an abandoned one, the task goes back to `todo`, and
    // the operator marks the agent available again.
    let sitting = fx
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("A"))
        .await
        .expect("query")
        .expect("the tenure is active");
    fx.store
        .assignments()
        .release_assignment(
            &sitting.id,
            ReleaseReason::AgentCrashed,
            sitting.state_version,
        )
        .await
        .expect("tenure released");

    let mut task_a = fx.reload_task("A").await;
    task_a.status = TaskStatus::Todo;
    fx.store
        .tasks()
        .update_task(&task_a)
        .await
        .expect("task back to todo");

    let mut agent = fx.store.agents().get_agent(&first).await.expect("agent");
    agent.status = AgentStatus::Available;
    agent.current_task = None;
    fx.store
        .agents()
        .update_agent(&agent)
        .await
        .expect("agent available again");

    let report = fx.run().await;

    assert_eq!(
        holder_of(&fx.store, "A").await,
        Some(AgentId::from_string("AGENT-2")),
        "the agent who never held the task takes it"
    );
    assert_eq!(report.assigned.len(), 1);
    assert!(report.is_settled());
}

#[tokio::test]
async fn a_prior_holder_is_still_used_when_nobody_else_can_take_the_task() {
    // The de-prioritization must not strand work: if the prior holder is the
    // only eligible agent, the task still moves.
    let fx = Fixture::new(&[task("A")]).await;
    let only = fx
        .agent("AGENT-1", AgentStatus::Available, vec![Capability::Coding])
        .await;

    assign(
        &fx.store,
        AssignRequest {
            project_id: fx.project.clone(),
            task_id: TaskId::from_string("A"),
            agent_id: only.clone(),
            assignment_id: AssignmentId::from_string("ASG-1"),
        },
    )
    .await
    .expect("the first assignment");

    // Release the tenure and reopen the task, as the previous test does.
    let sitting = fx
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("A"))
        .await
        .expect("query")
        .expect("the tenure is active");
    fx.store
        .assignments()
        .release_assignment(
            &sitting.id,
            ReleaseReason::AgentCrashed,
            sitting.state_version,
        )
        .await
        .expect("tenure released");

    let mut task_a = fx.reload_task("A").await;
    task_a.status = TaskStatus::Todo;
    fx.store
        .tasks()
        .update_task(&task_a)
        .await
        .expect("task back to todo");

    let mut agent = fx.store.agents().get_agent(&only).await.expect("agent");
    agent.status = AgentStatus::Available;
    agent.current_task = None;
    fx.store
        .agents()
        .update_agent(&agent)
        .await
        .expect("agent available again");

    // Two tenures for AGENT-1 now, and the round takes it anyway.
    let history = fx
        .store
        .assignments()
        .assignment_history(&TaskId::from_string("A"))
        .await
        .expect("history");
    assert_eq!(
        history.len(),
        1,
        "the released tenure is retained, not deleted"
    );

    let report = fx.run().await;

    assert_eq!(
        holder_of(&fx.store, "A").await,
        Some(AgentId::from_string("AGENT-1")),
        "the prior holder is the fallback, not a prohibition"
    );
    assert_eq!(report.assigned.len(), 1);
    assert!(report.is_settled());
}

#[tokio::test]
async fn a_second_round_finds_nothing_ready_so_nothing_changes() {
    // The round is idempotent: a task it handed out is `in_progress`, so
    // `ready_tasks` drops it and the next round has nothing to do.
    let fx = Fixture::new(&[task("A")]).await;
    fx.agent("AGENT-1", AgentStatus::Available, vec![Capability::Coding])
        .await;

    let first = fx.run().await;
    assert_eq!(first.assigned.len(), 1);

    let second = schedule(&fx.store, &mut Fixture::ids(), &fx.project)
        .await
        .expect("the round succeeds");

    assert!(second.assigned.is_empty(), "nothing was handable");
    assert!(second.refused.is_empty());
    assert!(second.unassigned.is_empty());
    assert!(second.is_settled(), "an idle round with no work is settled");

    // And the first round's write is intact.
    assert_eq!(fx.reload_task("A").await.status, TaskStatus::InProgress);
}

#[tokio::test]
async fn a_round_pins_the_clock_so_two_assignments_are_distinguishable() {
    // The `schedule_at` variant exists so a caller can distinguish two
    // assignments; the assignment's timestamp is the one a test can pin.
    let fx = Fixture::new(&[task("A"), task("B")]).await;
    fx.agent("AGENT-1", AgentStatus::Available, vec![Capability::Coding])
        .await;
    fx.agent("AGENT-2", AgentStatus::Available, vec![Capability::Coding])
        .await;
    let now = chrono::Utc::now();

    let report = schedule_at(&fx.store, &mut Fixture::ids(), &fx.project, now)
        .await
        .expect("the round succeeds");

    assert_eq!(report.assigned.len(), 2);
    for pairing in &report.assigned {
        assert_eq!(
            pairing.assignment.assigned_at, now,
            "the pinned clock reached every assignment the round made"
        );
    }
}

#[tokio::test]
async fn a_project_with_no_active_plan_cannot_be_scheduled() {
    let fx = Fixture::unplanned().await;
    fx.agent("AGENT-1", AgentStatus::Available, vec![Capability::Coding])
        .await;

    let err = schedule(&fx.store, &mut Fixture::ids(), &fx.project)
        .await
        .expect_err("nothing is being executed");

    // Not a store failure and not a refused pairing: there is no plan, so there
    // is no work to schedule, and a caller has to know the difference.
    assert!(
        matches!(err, ScheduleError::NoActivePlan(ref p) if *p == fx.project),
        "got {err:?}"
    );
}

#[tokio::test]
async fn an_empty_registry_leaves_every_task_unassigned() {
    let fx = Fixture::new(&[task("A")]).await;

    let report = fx.run().await;

    assert!(report.assigned.is_empty());
    let unassigned: Vec<_> = report.unassigned().collect();
    assert_eq!(
        unassigned,
        vec![(&TaskId::from_string("A"), UnassignedReason::NoAgentsAtAll)]
    );
}
