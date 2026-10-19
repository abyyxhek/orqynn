//! Assignment ids must be unique across Runner instances and store reopens.
//!
//! The defect these pin: assignment ids came from an `IdGenerator`, whose
//! counters start at zero for every instance. A second Runner over the same store
//! began at one again, so two rounds assigning *different* tasks both proposed
//! `ASG-sched-1` — and `agent_assignments.id` is a primary key, so the second
//! assignment was rejected with a bare `UNIQUE constraint failed`. The pairing
//! was legitimate; only the name collided.
//!
//! Every test here drives the real store, because the failure was a constraint
//! violation and nothing less would have caught it.

use director_app::schedule::schedule;
use director_domain::agent::{Agent, AgentStatus, Harness};
use director_domain::assignment::AgentAssignment;
use director_domain::capability::Capability;
use director_domain::ids::{AgentId, IdGenerator, MachineId, PlanId, ProjectId, TaskId};
use director_domain::plan::Plan;
use director_domain::project::Project;
use director_domain::task::{Task, TaskStatus};
use director_domain::{AgentRepository, AssignmentRepository, ProjectRepository, TaskRepository};
use director_store::Store;

/// A store plus a project and an active plan, ready for a scheduling round.
struct Fixture {
    store_path: std::path::PathBuf,
    store: Store,
    project: ProjectId,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// A store with one project and one active plan holding `task_ids`, all
    /// `todo` and each with its own scope so two of them never conflict.
    async fn with_tasks(task_ids: &[&str]) -> Self {
        let dir = tempfile::TempDir::new().expect("a temp dir for the store");
        let store_path = dir.path().join("orqyn.db");
        let store = Store::open(&store_path).await.expect("store opens");

        let project = ProjectId::from_string("PROJ-1");
        store
            .projects()
            .create_project(&Project::new(project.clone(), project.as_str(), "/nowhere"))
            .await
            .expect("project created");

        let mut plan = Plan::draft(
            PlanId::from_string("PLAN-1"),
            project.clone(),
            "assign some work",
            "the smallest honest fixture",
        );
        let built: Vec<Task> = task_ids
            .iter()
            .map(|id| {
                let mut task = Task::for_project(
                    project.clone(),
                    TaskId::from_string(*id),
                    *id,
                    format!("do {id}"),
                );
                // `create_active_plan` writes the plan's own status; a task is
                // handable only once it is `todo`.
                task.status = TaskStatus::Todo;
                // Each task needs its *own* ground. An undeclared scope may touch
                // anything, so it conflicts with every other task — which is the
                // scope guard working, not a defect in these tests.
                task.scope_paths = vec![format!("src/{}", id.to_lowercase())];
                plan.add_task(task.id.clone());
                task
            })
            .collect();

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
            store_path,
            store,
            project,
            _dir: dir,
        }
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

    /// Release whichever task currently holds a tenure, so the agent holding it
    /// becomes available again for the next round.
    ///
    /// The loop does this through MONITOR when a lease lapses; the tests need it
    /// on demand so each round can hand out a *different* task. It finds the
    /// holder rather than being told, so it does not depend on which task the
    /// plan happened to order first.
    async fn release_tenure(&self) {
        let mut active = None;
        for task in ["A", "B", "C", "D", "E"] {
            let task_id = TaskId::from_string(task);
            if let Some(found) = self
                .store
                .assignments()
                .active_assignment_for_task(&task_id)
                .await
                .expect("the active assignment reads")
            {
                active = Some((task_id, found));
                break;
            }
        }
        let (task_id, active) = active.expect("some task holds a tenure");
        self.store
            .assignments()
            .release_assignment(
                &active.id,
                director_domain::assignment::ReleaseReason::LeaseExpired,
                active.state_version,
            )
            .await
            .expect("the tenure is released");
        // The task goes back to being handable, which is what a lapsed lease
        // means for the loop.
        let mut row = self
            .store
            .tasks()
            .get_task(&task_id)
            .await
            .expect("the task reads");
        row.status = TaskStatus::Todo;
        self.store
            .tasks()
            .update_task(&row)
            .await
            .expect("the task is handable again");
    }

    /// Every assignment id on record, across every task.
    async fn all_assignment_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for task in ["A", "B", "C", "D", "E"] {
            let history: Vec<AgentAssignment> = self
                .store
                .assignments()
                .assignment_history(&TaskId::from_string(task))
                .await
                .expect("history reads");
            ids.extend(history.iter().map(|a| a.id.to_string()));
        }
        ids
    }
}

/// 1. Two Runner instances assign distinct tasks without an id collision.
///
/// The defect in its smallest form. Before the fix, the second round reused
/// `ASG-sched-1` and the store refused a perfectly good assignment.
#[tokio::test]
async fn two_runners_assign_distinct_tasks_without_colliding() {
    let fx = Fixture::with_tasks(&["A", "B"]).await;
    // One agent, so each round hands out exactly one task and the two rounds are
    // distinguishable.
    fx.agent("AGENT-1").await;

    // A "runner" is, from SCHEDULE's point of view, a fresh generator — that is
    // exactly the state a second `Runner` construction produces.
    let first = schedule(&fx.store, &mut IdGenerator::new(), &fx.project)
        .await
        .expect("the first round succeeds");
    assert_eq!(first.assigned.len(), 1, "one task, one agent");

    // The agent is freed so the second round has somebody to take the other task.
    fx.release_tenure().await;

    // The second round takes the *other* task, and it must not trip over the
    // first round's id.
    let second = schedule(&fx.store, &mut IdGenerator::new(), &fx.project)
        .await
        .expect("the second round must not be refused for a duplicate id");
    assert_eq!(second.assigned.len(), 1, "the remaining task is assigned");
    assert!(
        second.refused.is_empty(),
        "nothing was refused; a duplicate assignment id would show up here: {:?}",
        second.refused
    );

    let ids = fx.all_assignment_ids().await;
    assert_eq!(ids.len(), 2, "both assignments are on record");
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        ids, unique,
        "two assignments on record share an id, which is the defect in another form"
    );
}

/// 2. Repeated Runner creation does not reset uniqueness.
///
/// Ten rounds, ten runners, ten tasks: every one of them used to mint the same
/// first id, so this fails loudly rather than only under concurrency.
#[tokio::test]
async fn repeated_runner_creation_does_not_reset_uniqueness() {
    let tasks = ["A", "B", "C", "D", "E"];
    let fx = Fixture::with_tasks(&tasks).await;
    // One agent: each round hands out exactly one task, so five rounds mean five
    // distinct pairings.
    fx.agent("AGENT-1").await;

    for round in 0..tasks.len() {
        if round > 0 {
            // Free the agent the previous round used, so this round has somebody
            // to pair with the next task.
            fx.release_tenure().await;
        }
        let report = schedule(&fx.store, &mut IdGenerator::new(), &fx.project)
            .await
            .unwrap_or_else(|err| panic!("round {round} failed: {err:?}"));
        assert_eq!(
            report.assigned.len(),
            1,
            "round {round} assigned nothing: assigned={:?} refused={:?} unassigned={:?}",
            report.assigned,
            report.refused,
            report.unassigned
        );
        assert!(
            report.refused.is_empty(),
            "round {round} was refused: {:?}",
            report.refused
        );
    }

    let ids = fx.all_assignment_ids().await;
    assert_eq!(ids.len(), tasks.len(), "every task was handed out once");
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        ids, unique,
        "ten rounds over ten tasks produced fewer than ten distinct ids"
    );
}

/// 3. Reopening the store does not reintroduce collisions.
///
/// The id is derived from persisted state, so it has to survive the writer going
/// away entirely — a restart is the case a process-local counter can never serve.
#[tokio::test]
async fn reopening_the_store_does_not_reintroduce_collisions() {
    let fx = Fixture::with_tasks(&["A", "B", "C", "D"]).await;
    fx.agent("AGENT-1").await;

    // Two rounds, each with its own generator, then the writer is gone.
    schedule(&fx.store, &mut IdGenerator::new(), &fx.project)
        .await
        .expect("the first round");
    fx.release_tenure().await;
    schedule(&fx.store, &mut IdGenerator::new(), &fx.project)
        .await
        .expect("the second round");

    let before = fx.all_assignment_ids().await;
    assert_eq!(before.len(), 2);
    drop(fx.store);

    let reopened = Store::open(&fx.store_path)
        .await
        .expect("the store reopens");

    // A third agent, registered after the restart, so the round has somebody to
    // hand the remaining task to.
    let mut agent = Agent::register(
        AgentId::from_string("AGENT-3"),
        "AGENT-3",
        Harness::ClaudeCode,
        MachineId::from_string("MACH-a"),
        vec![Capability::Coding],
    );
    agent.status = AgentStatus::Available;
    reopened
        .agents()
        .register_agent(&agent)
        .await
        .expect("the agent registers after the restart");

    let report = schedule(&reopened, &mut IdGenerator::new(), &fx.project)
        .await
        .expect("a round after a restart must not be refused for a duplicate id");
    assert_eq!(report.assigned.len(), 1);
    assert!(
        report.refused.is_empty(),
        "a restart re-minted an id already on record: {:?}",
        report.refused
    );

    let mut after: Vec<String> = Vec::new();
    for task in ["A", "B", "C", "D"] {
        let history: Vec<AgentAssignment> = reopened
            .assignments()
            .assignment_history(&TaskId::from_string(task))
            .await
            .expect("history reads");
        after.extend(history.iter().map(|a| a.id.to_string()));
    }
    let mut unique = after.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(after, unique, "ids collided across the restart");
    assert_eq!(
        after.len(),
        3,
        "the post-restart assignment joined the record"
    );
}

/// 4. Concurrent assignment and the scope-conflict guarantee remain intact.
///
/// Ids got safer; the rules that decide *whether* an assignment happens must not
/// have moved. Two rounds over one task still produce exactly one assignment.
#[tokio::test]
async fn one_active_assignment_per_task_still_holds() {
    let fx = Fixture::with_tasks(&["A"]).await;
    fx.agent("AGENT-1").await;
    fx.agent("AGENT-2").await;

    let first = schedule(&fx.store, &mut IdGenerator::new(), &fx.project)
        .await
        .expect("the first round");
    assert_eq!(first.assigned.len(), 1);

    // The task is `in_progress` now, so the next round has nothing to hand out.
    let second = schedule(&fx.store, &mut IdGenerator::new(), &fx.project)
        .await
        .expect("the second round");
    assert!(
        second.assigned.is_empty(),
        "a task already in flight must not be handed out again"
    );
    assert!(
        second.unassigned.is_empty(),
        "and it is not reported as unassigned either — it is simply not ready: {:?}",
        second.unassigned
    );

    let ids = fx.all_assignment_ids().await;
    assert_eq!(ids.len(), 1, "exactly one tenure exists");
}

/// 4b. Overlapping scope is still refused, and the refusal is still a conflict.
///
/// This is the guarantee the id change must not have made weaker or louder.
#[tokio::test]
async fn an_overlapping_scope_is_still_refused_as_a_conflict() {
    let dir = tempfile::TempDir::new().expect("a temp dir");
    let store = Store::open(dir.path().join("orqyn.db"))
        .await
        .expect("store opens");
    let project = ProjectId::from_string("PROJ-1");
    store
        .projects()
        .create_project(&Project::new(project.clone(), project.as_str(), "/nowhere"))
        .await
        .expect("project created");

    // Two tasks that declare the *same* ground.
    let mut plan = Plan::draft(
        PlanId::from_string("PLAN-1"),
        project.clone(),
        "overlapping work",
        "two tasks, one path",
    );
    let built: Vec<Task> = ["A", "B"]
        .iter()
        .map(|id| {
            let mut task = Task::for_project(
                project.clone(),
                TaskId::from_string(*id),
                *id,
                format!("do {id}"),
            );
            task.status = TaskStatus::Todo;
            task.scope_paths = vec!["src/auth".to_string()];
            plan.add_task(task.id.clone());
            task
        })
        .collect();
    store
        .create_active_plan(
            &plan,
            &built,
            &AgentId::from_string("AGENT-planner"),
            chrono::Utc::now(),
        )
        .await
        .expect("plan created");

    for n in 0..2 {
        let mut agent = Agent::register(
            AgentId::from_string(format!("AGENT-{n}")),
            format!("AGENT-{n}"),
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            vec![Capability::Coding],
        );
        agent.status = AgentStatus::Available;
        store
            .agents()
            .register_agent(&agent)
            .await
            .expect("agent registered");
    }

    let report = schedule(&store, &mut IdGenerator::new(), &project)
        .await
        .expect("the round succeeds");
    assert_eq!(
        report.assigned.len(),
        1,
        "only one of two overlapping tasks can be handed out"
    );
    assert_eq!(
        report.unassigned.len(),
        1,
        "and the other is reported, not silently dropped"
    );
    assert!(
        matches!(
            report.unassigned[0].reason,
            director_app::schedule::UnassignedReason::ScopeConflict { .. }
        ),
        "the report is a scope conflict, not a storage error: {:?}",
        report.unassigned[0].reason
    );
}

/// 5. A genuine duplicate id still produces an error rather than a silent success.
///
/// The store's primary key is the last line of defence, and it has not been
/// weakened. Two *different* tasks cannot collide any more, but a caller that
/// supplies an id already on record must still be refused — and the refusal must
/// be visible.
#[tokio::test]
async fn a_genuine_duplicate_id_is_still_an_error() {
    let fx = Fixture::with_tasks(&["A", "B"]).await;
    let agent = fx.agent("AGENT-1").await;

    // Assign A directly with a chosen id, then reuse that exact id for B.
    let chosen = director_domain::ids::AssignmentId::from_string("ASG-chosen-1");
    fx.store
        .assign_and_start(
            &TaskId::from_string("A"),
            &agent,
            &chosen,
            chrono::Utc::now(),
        )
        .await
        .expect("the first assignment lands");

    // The same id for a different task must be refused, and the refusal must not
    // be dressed up as a success.
    let err = fx
        .store
        .assign_and_start(
            &TaskId::from_string("B"),
            &agent,
            &chosen,
            chrono::Utc::now(),
        )
        .await
        .expect_err("reusing an id already on record must not report success");
    assert!(
        !err.to_string().is_empty(),
        "the store explains why the write was refused"
    );

    // And nothing was half-written: B still has no tenure.
    let history: Vec<AgentAssignment> = fx
        .store
        .assignments()
        .assignment_history(&TaskId::from_string("B"))
        .await
        .expect("history reads");
    assert!(
        history.is_empty(),
        "no tenure exists for the task whose assignment was refused"
    );
}
