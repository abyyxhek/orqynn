//! Integration tests for the VERIFY step against a real SQLite store and a
//! real executor.
//!
//! These tests run actual commands through `LocalExecutor` — the reference
//! [`ExecutionProvider`] — because the whole point of this step is that Orqyn
//! observes the evidence itself. A mocked executor could confirm that `verify`
//! calls `run_command`; it could not show that an exit code the executor
//! produces and a verdict the step reaches agree, or that a task whose check
//! failed ends up `failed` in the store the next loop tick reads.
//!
//! The commands are `git`, deliberately: it is present wherever Orqyn runs,
//! `git --version` exits zero, and an unknown flag exits non-zero with
//! something on stderr. That is a real command really succeeding and really
//! failing, which is the smallest honest substitute for a test suite.
//!
//! The tasks arrive at `verification_pending` the way they do in the running
//! loop — ASSIGN hands them out, then MONITOR's `report_done` records the
//! holder finishing — rather than a status being poked by hand, because the
//! state VERIFY reads is the state those two steps produce.

use director_adapters::executor::LocalExecutor;
use director_app::assign::{assign, AssignRequest};
use director_app::monitor::{report_done, ReportRequest};
use director_app::verify::{
    judge, verify, CheckResult, FailedCheck, Judged, UnverifiableReason, Verdict, VerifyReport,
};
use director_app::VerifyError;
use director_domain::agent::{Agent, Harness};
use director_domain::assignment::AssignmentStatus;
use director_domain::capability::Capability;
use director_domain::ids::{AgentId, AssignmentId, MachineId, PlanId, ProjectId, TaskId};
use director_domain::plan::Plan;
use director_domain::project::Project;
use director_domain::task::{ExpectedOutput, Task, TaskStatus};
use director_domain::{AgentRepository, AssignmentRepository, ProjectRepository, TaskRepository};
use director_store::Store;

/// A store, a project with an active plan, a registered agent, and a working
/// directory the checks run in — everything VERIFY needs, set up the way the
/// steps before it leave it.
struct Fixture {
    store: Store,
    _dir: tempfile::TempDir,
    working_dir: std::path::PathBuf,
    project: ProjectId,
    agent: AgentId,
}

/// One expected output with a machine check.
fn checked(criterion: &str, check: &str) -> ExpectedOutput {
    ExpectedOutput {
        criterion: criterion.to_string(),
        check: Some(check.to_string()),
    }
}

/// One expected output a planner wrote as prose only.
fn prose(criterion: &str) -> ExpectedOutput {
    ExpectedOutput {
        criterion: criterion.to_string(),
        check: None,
    }
}

impl Fixture {
    /// A store with one project, one active plan holding the given tasks with
    /// their expected outputs, and one available agent.
    async fn new(tasks: &[(String, Vec<ExpectedOutput>)]) -> Self {
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

        let plan_id = PlanId::from_string("PLAN-1");
        let mut plan = Plan::draft(
            plan_id,
            project.clone(),
            "add authentication",
            "test rationale",
        );
        let built: Vec<Task> = tasks
            .iter()
            .map(|(id, outputs)| {
                let mut task =
                    Task::for_project(project.clone(), TaskId::from_string(id), id, "do it");
                task.status = TaskStatus::Todo;
                task.expected_outputs = outputs.clone();
                task
            })
            .collect();
        for task in &built {
            plan.add_task(task.id.clone());
        }
        store
            .create_active_plan(
                &plan,
                &built,
                &AgentId::from_string("AGENT-planner"),
                chrono::Utc::now(),
            )
            .await
            .expect("plan created and activated");

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

        // Where the checks run. Any directory works: the checks do not depend
        // on the working tree, only on the programs they name.
        let working_dir = dir.path().to_path_buf();

        Fixture {
            store,
            _dir: dir,
            working_dir,
            project,
            agent,
        }
    }

    /// Hand a task to the fixture's agent, the way ASSIGN does.
    async fn assign(&self, task: &str, assignment: &str) {
        assign(
            &self.store,
            AssignRequest {
                project_id: self.project.clone(),
                task_id: TaskId::from_string(task),
                agent_id: self.agent.clone(),
                assignment_id: AssignmentId::from_string(assignment),
            },
        )
        .await
        .expect("assigned");
    }

    /// Record the agent finishing the work it was handed, the way MONITOR does.
    /// This is what puts a task in front of VERIFY.
    async fn report(&self, assignment: &str) {
        report_done(
            &self.store,
            ReportRequest {
                assignment_id: AssignmentId::from_string(assignment),
            },
        )
        .await
        .expect("reported");
    }

    /// The path an agent's report leaves a task at: assign it, then report it
    /// done. The task is `verification_pending` and no one holds it.
    async fn arrive_at_verification(&self, task: &str, assignment: &str) {
        self.assign(task, assignment).await;
        self.report(assignment).await;
    }

    async fn reload_task(&self, id: &str) -> Task {
        self.store
            .tasks()
            .get_task(&TaskId::from_string(id))
            .await
            .expect("task exists")
    }

    /// Run a round with the local executor against this fixture's project.
    async fn verify(&self) -> Result<VerifyReport, VerifyError> {
        verify(
            &self.store,
            &LocalExecutor::default(),
            &self.project,
            &self.working_dir,
        )
        .await
    }
}

#[tokio::test]
async fn a_task_whose_checks_pass_becomes_done() {
    // The invariant the whole model exists to express: a task reaches `done`
    // only because Orqyn ran something and watched it succeed.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![
            checked("the login endpoint answers", "git --version"),
            checked("the session cookie is set", "git --version"),
        ],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let report = fixture.verify().await.expect("the round");

    assert_eq!(report.judged.len(), 1);
    assert_eq!(report.judged[0].task.as_str(), "A");
    assert_eq!(report.judged[0].verdict, Verdict::Passed);
    assert_eq!(report.judged[0].outcome, Some(TaskStatus::Done));
    assert!(report.all_passed());

    // The store is what the next loop tick reads.
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Done);
}

#[tokio::test]
async fn a_task_whose_check_fails_becomes_failed_and_is_not_done() {
    // The acceptance criterion: the agent claimed done, the check failed, the
    // task must not be completed. It is `failed`, which is REPLAN's to act on.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![
            checked("the login endpoint answers", "git --version"),
            checked("the logout endpoint answers", "git --no-such-flag"),
        ],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let report = fixture.verify().await.expect("the round");

    assert_eq!(report.judged[0].verdict, Verdict::Failed);
    assert_eq!(report.judged[0].outcome, Some(TaskStatus::Failed));
    assert!(!report.all_passed());

    let task = fixture.reload_task("A").await;
    assert_eq!(task.status, TaskStatus::Failed);
    assert_ne!(task.status, TaskStatus::Done);

    // The failing check is reported with what it produced, so a caller can say
    // why the work failed rather than just that it did.
    let failed = report.failed().map(|id| id.as_str()).collect::<Vec<_>>();
    assert_eq!(failed, vec!["A"]);
    let failure = match &report.judged[0].checks[1] {
        CheckResult::Failed(failure) => failure,
        other => panic!("expected a failed check, got {other:?}"),
    };
    assert_eq!(failure.command, "git --no-such-flag");
    assert_ne!(failure.exit_code, 0);
    assert!(!failure.stderr.is_empty(), "git said why it failed");
}

#[tokio::test]
async fn the_history_records_the_verdict_as_a_transition() {
    // A verdict is a status move, so it lands in the append-only history: the
    // trail says the task went to `done` from `verification_pending`.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked("the endpoint answers", "git --version")],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;
    fixture.verify().await.expect("the round");

    let history = fixture
        .store
        .tasks()
        .task_history(&TaskId::from_string("A"))
        .await
        .expect("history");
    let last = history.last().expect("there is a transition");
    assert_eq!(last.from, TaskStatus::VerificationPending);
    assert_eq!(last.to, TaskStatus::Done);
}

#[tokio::test]
async fn prose_criteria_do_not_block_a_pass() {
    // A criterion Orqyn has no machine check for is reported and never
    // satisfied — and never blocks the checks that did run.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![
            checked("the login endpoint answers", "git --version"),
            prose("the error messages read well"),
        ],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let report = fixture.verify().await.expect("the round");

    assert_eq!(report.judged[0].verdict, Verdict::Passed);
    assert!(matches!(
        report.judged[0].checks[1],
        CheckResult::Unchecked { ref criterion } if criterion == "the error messages read well"
    ));
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Done);
}

#[tokio::test]
async fn a_task_with_no_checks_stays_awaiting_verification() {
    // Nothing ran, so nothing was verified, so nothing was written. The task is
    // exactly where the report left it — not `done`, and not failed either.
    let fixture = Fixture::new(&[("A".to_string(), vec![prose("it should feel finished")])]).await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let report = fixture.verify().await.expect("the round");

    assert_eq!(
        report.judged[0].verdict,
        Verdict::Unverifiable(UnverifiableReason::NoChecks)
    );
    assert!(report.judged[0].outcome.is_none());
    assert_eq!(
        report
            .unverifiable()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        vec!["A"]
    );
    assert_eq!(
        fixture.reload_task("A").await.status,
        TaskStatus::VerificationPending,
        "an unverifiable task is left as it was"
    );
}

#[tokio::test]
async fn a_task_whose_check_cannot_run_stays_awaiting_verification() {
    // The program does not exist, so the executor refused. The evidence is
    // missing — that is a problem with the environment, not with the work, and
    // it must not become a failure verdict on someone's task.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked(
            "the endpoint answers",
            "definitely-not-a-real-program --ok",
        )],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let report = fixture.verify().await.expect("the round");

    assert_eq!(
        report.judged[0].verdict,
        Verdict::Unverifiable(UnverifiableReason::EvidenceMissing)
    );
    assert!(report.judged[0].outcome.is_none());
    assert_eq!(
        fixture.reload_task("A").await.status,
        TaskStatus::VerificationPending
    );

    // The error is carried, so a caller can tell a broken environment from
    // broken work.
    assert!(matches!(
        report.judged[0].checks[0],
        CheckResult::Unrunnable { ref error, .. } if !error.is_empty()
    ));
}

#[tokio::test]
async fn a_round_passes_one_task_and_fails_another() {
    // The round judges each task on its own evidence: one task's failure does
    // not rub off on another, and neither does a pass.
    let fixture = Fixture::new(&[
        (
            "A".to_string(),
            vec![checked("the endpoint answers", "git --version")],
        ),
        (
            "B".to_string(),
            vec![checked("the endpoint answers", "git --no-such-flag")],
        ),
    ])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;
    fixture.arrive_at_verification("B", "ASG-2").await;

    let report = fixture.verify().await.expect("the round");

    assert_eq!(report.judged.len(), 2);
    assert_eq!(
        report.passed().map(|id| id.as_str()).collect::<Vec<_>>(),
        vec!["A"]
    );
    assert_eq!(
        report.failed().map(|id| id.as_str()).collect::<Vec<_>>(),
        vec!["B"]
    );
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Done);
    assert_eq!(fixture.reload_task("B").await.status, TaskStatus::Failed);
}

#[tokio::test]
async fn a_round_surveys_only_the_tasks_awaiting_a_verdict() {
    // Work in flight is not VERIFY's business: MONITOR is still watching it.
    let fixture = Fixture::new(&[
        (
            "A".to_string(),
            vec![checked("the endpoint answers", "git --version")],
        ),
        (
            "B".to_string(),
            vec![checked("the endpoint answers", "git --version")],
        ),
    ])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;
    // B is still being worked — the holder has not reported finishing.
    fixture.assign("B", "ASG-2").await;

    let report = fixture.verify().await.expect("the round");

    assert_eq!(report.judged.len(), 1, "only the task awaiting a verdict");
    assert_eq!(report.judged[0].task.as_str(), "A");
    assert_eq!(
        fixture.reload_task("B").await.status,
        TaskStatus::InProgress
    );
}

#[tokio::test]
async fn a_second_round_after_a_pass_has_nothing_to_judge() {
    // Idempotency: a passed task is `done`, so the next round does not survey
    // it. That is what makes the step safe to run on every tick.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked("the endpoint answers", "git --version")],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let first = fixture.verify().await.expect("the first round");
    assert_eq!(first.judged.len(), 1);

    let second = fixture.verify().await.expect("the second round");
    assert!(second.judged.is_empty());
    assert!(second.all_passed(), "nothing is awaiting a verdict");
}

#[tokio::test]
async fn a_task_that_failed_is_judged_once_and_not_resurveyed() {
    // A failed task is terminal as far as VERIFY is concerned; REPLAN decides
    // what happens to it. The round does not keep re-running its checks.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked("the endpoint answers", "git --no-such-flag")],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    fixture.verify().await.expect("the first round");
    let second = fixture.verify().await.expect("the second round");

    assert!(
        second.judged.is_empty(),
        "the failed task is not resurveyed"
    );
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Failed);
}

#[tokio::test]
async fn the_tenure_is_already_over_when_the_verdict_lands() {
    // MONITOR's report ended the tenure when the agent finished, so a task
    // awaiting verification has no tenant. The verdict is a single task write,
    // and this is what keeps it that way.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked("the endpoint answers", "git --version")],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let tenure = fixture
        .store
        .assignments()
        .assignment_history(&TaskId::from_string("A"))
        .await
        .expect("history");
    assert_eq!(tenure.len(), 1);
    assert_eq!(tenure[0].status, AssignmentStatus::Released);

    fixture.verify().await.expect("the round");

    // The verdict changed nothing about the tenure: it judged the work.
    let tenure_after = fixture
        .store
        .assignments()
        .assignment_history(&TaskId::from_string("A"))
        .await
        .expect("history");
    assert_eq!(tenure_after.len(), 1, "the verdict is one task write");
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Done);
}

#[test]
fn the_verdict_rules_are_the_pure_half_of_the_step() {
    // The precedence between the outcomes is the design, and it is checkable
    // without running anything.
    let pass = CheckResult::Passed {
        criterion: "works".into(),
        command: "git --version".into(),
    };
    let fail = CheckResult::Failed(Box::new(FailedCheck {
        criterion: "broken".into(),
        command: "git --nope".into(),
        exit_code: 129,
        timed_out: false,
        stdout: String::new(),
        stderr: String::new(),
    }));
    let unrunnable = CheckResult::Unrunnable {
        criterion: "missing".into(),
        command: "nope".into(),
        error: "spawn failed".into(),
    };
    let unchecked = CheckResult::Unchecked {
        criterion: "prose".into(),
    };

    use UnverifiableReason;

    // A failure decides, whatever else the round saw.
    assert_eq!(judge(&[pass.clone(), fail.clone()]), Verdict::Failed);
    assert_eq!(judge(&[fail.clone(), unrunnable.clone()]), Verdict::Failed);
    assert_eq!(judge(&[fail.clone(), unchecked.clone()]), Verdict::Failed);

    // Missing evidence blocks a pass only when nothing failed.
    assert_eq!(
        judge(&[pass.clone(), unrunnable.clone()]),
        Verdict::Unverifiable(UnverifiableReason::EvidenceMissing)
    );

    // Prose never blocks.
    assert_eq!(judge(&[pass, unchecked.clone()]), Verdict::Passed);

    // An unrunnable check is evidence missing even with nothing else to say:
    // the round knew what to run and could not run it.
    assert_eq!(
        judge(&[unrunnable]),
        Verdict::Unverifiable(UnverifiableReason::EvidenceMissing)
    );

    // And with only prose, there was nothing to run at all — the planner gave
    // the task no checkable criterion.
    assert_eq!(
        judge(&[unchecked]),
        Verdict::Unverifiable(UnverifiableReason::NoChecks)
    );
}

#[test]
fn an_empty_report_is_not_a_failure() {
    // A round that found nothing awaiting a verdict: no task was judged, and
    // nothing failed.
    let report = VerifyReport::default();
    assert!(report.all_passed());
    assert_eq!(report.passed().count(), 0);
    assert_eq!(report.failed().count(), 0);
    assert_eq!(report.unverifiable().count(), 0);

    // And the accessors partition a mixed round.
    let mixed = VerifyReport {
        judged: vec![
            Judged {
                task: TaskId::from_string("A"),
                verdict: Verdict::Passed,
                checks: vec![],
                outcome: Some(TaskStatus::Done),
            },
            Judged {
                task: TaskId::from_string("B"),
                verdict: Verdict::Failed,
                checks: vec![],
                outcome: Some(TaskStatus::Failed),
            },
        ],
    };
    assert!(!mixed.all_passed());
    assert_eq!(mixed.passed().count(), 1);
    assert_eq!(mixed.failed().count(), 1);
}
