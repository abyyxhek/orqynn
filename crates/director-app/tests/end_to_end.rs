//! End-to-end tests for the verification path through Orqyn's loop.
//!
//! These are the tests the milestone exists for: not "does the engine work in
//! isolation", but "does the running application path produce a `done` task
//! *with* the durable record that explains it". They drive the real steps in
//! the real order —
//!
//! ```text
//! PLAN → SCHEDULE → ASSIGN → MONITOR → VERIFY
//! ```
//!
//! — against a real store and a real executor, and then read persisted state
//! back out of the store rather than asserting on values held in memory. No
//! test here pokes a status by hand to set up its assertion: the task reaches
//! the state it is asserted to hold by the steps that reach it in production.
//!
//! The second test covers the path a failure takes: VERIFY fails the work,
//! REPLAN answers for it, and the failed judgment stays on record while the
//! task becomes handable again.

use director_adapters::executor::LocalExecutor;
use director_app::plan::{plan, PlanRequest, TaskSpec};
use director_app::replan::{replan, Remediation, ReplanRequest, TaskDecision};
use director_app::schedule::schedule;
use director_app::verify::verify;
use director_domain::agent::{Agent, Harness};
use director_domain::capability::Capability;
use director_domain::ids::IdGenerator;
use director_domain::ids::{AgentId, MachineId, PlanId, ProjectId, TaskId, VerificationId};
use director_domain::project::Project;
use director_domain::task::{ExpectedOutput, TaskStatus};
use director_domain::verification::VerificationStatus;
use director_domain::{
    AgentRepository, AssignmentRepository, ProjectRepository, TaskRepository,
    VerificationRepository,
};
use director_store::Store;

/// A store, a project, and one registered agent — the preconditions a running
/// loop has after OBSERVE. Everything else these tests assert is produced by
/// the steps themselves.
struct Fixture {
    store: Store,
    _dir: tempfile::TempDir,
    working_dir: std::path::PathBuf,
    project: ProjectId,
    agent: AgentId,
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
            .create_project(&Project::new(
                project.clone(),
                project.as_str(),
                dir.path().to_str().unwrap_or("/nowhere"),
            ))
            .await
            .expect("project created");

        let agent = AgentId::from_string("AGENT-1");
        store
            .agents()
            .register_agent(&Agent::register(
                agent.clone(),
                "claude-a",
                Harness::ClaudeCode,
                MachineId::from_string("MACH-a"),
                vec![Capability::Coding],
            ))
            .await
            .expect("agent registered");

        Fixture {
            store,
            working_dir: dir.path().to_path_buf(),
            _dir: dir,
            project,
            agent,
        }
    }

    async fn reload(&self, id: &str) -> director_domain::task::Task {
        self.store
            .tasks()
            .get_task(&TaskId::from_string(id))
            .await
            .expect("task exists")
    }
}

/// One expected output with a machine check.
fn checked(criterion: &str, check: &str) -> ExpectedOutput {
    ExpectedOutput {
        criterion: criterion.to_string(),
        check: Some(check.to_string()),
    }
}

/// One task spec whose single criterion is the given check.
fn spec_with_check(id: &str, criterion: &str, check: &str) -> TaskSpec {
    let mut spec = TaskSpec::new(TaskId::from_string(id), id, format!("do {id}"));
    spec.expected_outputs = vec![checked(criterion, check)];
    spec
}

#[tokio::test]
async fn the_loop_produces_a_done_task_with_a_durable_verification_record() {
    // PLAN → SCHEDULE → ASSIGN → MONITOR → VERIFY, and the persisted state at
    // the end: a task that is `done`, a verification that says why, and a
    // transition trail that says how it got there.
    let fixture = Fixture::new().await;

    // PLAN: one task with one checkable criterion.
    let planned = plan(
        &fixture.store,
        PlanRequest {
            project_id: fixture.project.clone(),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "add authentication".into(),
            rationale: "the loop's smallest honest path to done".into(),
            tasks: vec![spec_with_check(
                "AUTH",
                "the login endpoint answers",
                "git --version",
            )],
            authorized_by: AgentId::from_string("AGENT-planner"),
        },
    )
    .await
    .expect("the plan lands");
    assert_eq!(planned.tasks.len(), 1);
    assert_eq!(planned.tasks[0].status, TaskStatus::Todo);

    // SCHEDULE: the step decides who gets the ready task, and applies it
    // through ASSIGN in the same round.
    let mut ids = IdGenerator::new();
    let scheduled = schedule(&fixture.store, &mut ids, &fixture.project)
        .await
        .expect("the round");
    assert_eq!(scheduled.assigned.len(), 1);
    assert_eq!(scheduled.assigned[0].task_id, TaskId::from_string("AUTH"));
    let assignment = scheduled.assigned[0].assignment.id.clone();
    assert_eq!(
        fixture.reload("AUTH").await.status,
        TaskStatus::InProgress,
        "ASSIGN moved the task into flight"
    );

    // MONITOR: the holder reports finishing. The report stops at
    // `verification_pending` — it never completes the work.
    director_app::monitor::report_done(
        &fixture.store,
        director_app::monitor::ReportRequest {
            assignment_id: assignment,
        },
    )
    .await
    .expect("the report");
    assert_eq!(
        fixture.reload("AUTH").await.status,
        TaskStatus::VerificationPending
    );

    // VERIFY: the engine judges the claim by running the check itself.
    let report = verify(
        &fixture.store,
        &LocalExecutor::default(),
        &fixture.project,
        &fixture.working_dir,
    )
    .await
    .expect("the round");

    assert_eq!(report.judged.len(), 1);
    assert_eq!(report.passed().count(), 1);

    // --- Persisted state, read back from the store a fresh tick would read --
    let task = fixture.reload("AUTH").await;
    assert_eq!(task.status, TaskStatus::Done);

    let verification = fixture
        .store
        .verifications()
        .latest_verification(&TaskId::from_string("AUTH"))
        .await
        .expect("history loads")
        .expect("a done task has a verification behind it");
    assert_eq!(verification.status, VerificationStatus::Passed);
    assert_eq!(verification.id, VerificationId::from_string("VER-AUTH-1"));
    assert_eq!(verification.task_id, TaskId::from_string("AUTH"));
    assert_eq!(verification.project_id, fixture.project);
    assert_eq!(verification.evidence.len(), 1);
    assert_eq!(
        verification.evidence[0].criterion,
        "the login endpoint answers"
    );
    assert!(verification.evidence[0].detail.contains("git --version"));

    // The move is in the append-only history, recorded once, from the status
    // the report left the task at.
    let history = fixture
        .store
        .tasks()
        .task_history(&TaskId::from_string("AUTH"))
        .await
        .expect("history");
    let last = history.last().expect("there is a transition");
    assert_eq!(last.from, TaskStatus::VerificationPending);
    assert_eq!(last.to, TaskStatus::Done);

    // The tenure the schedule started is closed and retained: the verdict
    // judged the work, it did not re-open the assignment.
    let tenures = fixture
        .store
        .assignments()
        .assignment_history(&TaskId::from_string("AUTH"))
        .await
        .expect("tenure history");
    assert_eq!(tenures.len(), 1);
    assert_eq!(
        tenures[0].status,
        director_domain::assignment::AssignmentStatus::Released
    );

    // A second round has nothing to judge: the task is done, and the record
    // behind it is why the loop can stop asking.
    let second = verify(
        &fixture.store,
        &LocalExecutor::default(),
        &fixture.project,
        &fixture.working_dir,
    )
    .await
    .expect("the second round");
    assert!(second.judged.is_empty());
    assert_eq!(
        fixture
            .store
            .verifications()
            .verifications_for_task(&TaskId::from_string("AUTH"))
            .await
            .expect("history")
            .len(),
        1,
        "judging a task once leaves one judgment"
    );
}

#[tokio::test]
async fn a_failed_verification_records_itself_and_replan_can_act_on_it() {
    // The recovery path: the work claims done, the check fails, the task is
    // `failed` with a record explaining it — and REPLAN can put the work back
    // in front of the loop without erasing the judgment that stopped it.
    let fixture = Fixture::new().await;

    plan(
        &fixture.store,
        PlanRequest {
            project_id: fixture.project.clone(),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "add authentication".into(),
            rationale: "the loop's smallest honest path to a failure".into(),
            tasks: vec![spec_with_check(
                "AUTH",
                "the login endpoint answers",
                "git --no-such-flag",
            )],
            authorized_by: AgentId::from_string("AGENT-planner"),
        },
    )
    .await
    .expect("the plan lands");

    let mut ids = IdGenerator::new();
    let scheduled = schedule(&fixture.store, &mut ids, &fixture.project)
        .await
        .expect("the round");
    let assignment = scheduled.assigned[0].assignment.id.clone();

    director_app::monitor::report_done(
        &fixture.store,
        director_app::monitor::ReportRequest {
            assignment_id: assignment,
        },
    )
    .await
    .expect("the report");

    // VERIFY judges, and the judgment is failure.
    let report = verify(
        &fixture.store,
        &LocalExecutor::default(),
        &fixture.project,
        &fixture.working_dir,
    )
    .await
    .expect("the round");

    assert_eq!(report.failed().count(), 1);
    let task = fixture.reload("AUTH").await;
    assert_eq!(task.status, TaskStatus::Failed);
    assert_ne!(task.status, TaskStatus::Done);

    // The failed judgment is persisted, so "why did this fail" is answerable
    // after the round that decided it is gone.
    let failed = fixture
        .store
        .verifications()
        .latest_verification(&TaskId::from_string("AUTH"))
        .await
        .expect("history loads")
        .expect("the failure is recorded");
    assert_eq!(failed.status, VerificationStatus::Failed);
    assert!(failed.evidence[0].detail.contains("git --no-such-flag"));

    // REPLAN answers for the failed work: the caller decides to retry, and the
    // task becomes handable again.
    let replanned = replan(
        &fixture.store,
        ReplanRequest {
            project_id: fixture.project.clone(),
            decisions: vec![TaskDecision {
                task_id: TaskId::from_string("AUTH"),
                remediation: Remediation::Retry,
                reason: Some("the check is right, the work is not".into()),
            }],
            decided_by: Some(fixture.agent.clone()),
        },
    )
    .await
    .expect("the replan round");

    assert_eq!(replanned.retried().count(), 1);
    assert_eq!(
        fixture.reload("AUTH").await.status,
        TaskStatus::Todo,
        "a retried task is handable again"
    );

    // The failed judgment survived the retry — history is append-only, and the
    // record of the failure is what makes the next attempt a re-attempt.
    let history = fixture
        .store
        .verifications()
        .verifications_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, VerificationStatus::Failed);

    // And the reworked task, judged again on its new merits, lands a *second*
    // record rather than overwriting the first.
    let mut reworked = fixture.reload("AUTH").await;
    reworked.status = TaskStatus::VerificationPending;
    reworked.expected_outputs = vec![checked("the login endpoint answers", "git --version")];
    fixture
        .store
        .tasks()
        .update_task(&reworked)
        .await
        .expect("the task is queued for verification again");

    let rejudged = verify(
        &fixture.store,
        &LocalExecutor::default(),
        &fixture.project,
        &fixture.working_dir,
    )
    .await
    .expect("the round");

    assert_eq!(rejudged.passed().count(), 1);
    assert_eq!(fixture.reload("AUTH").await.status, TaskStatus::Done);

    let history = fixture
        .store
        .verifications()
        .verifications_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("history");
    assert_eq!(
        history.len(),
        2,
        "the failure and the pass are both on record"
    );
    assert_eq!(history[0].status, VerificationStatus::Passed);
    assert_eq!(history[1].status, VerificationStatus::Failed);
    assert_ne!(history[0].id, history[1].id);
}
