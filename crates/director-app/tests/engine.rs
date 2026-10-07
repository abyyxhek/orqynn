//! Integration tests for the verification engine against a real SQLite store
//! and a real executor.
//!
//! These tests drive [`engine::verify_task`] directly, because that function is
//! the seam the milestone is about: the only path by which a task reaches
//! `done`. They run real commands through `LocalExecutor` — the reference
//! [`ExecutionProvider`] — for the same reason the VERIFY step's tests do: a
//! mocked executor could confirm that the engine calls `run_command`, but not
//! that an exit code the executor produced and a verdict the engine reached
//! agree, or that the evidence a `Verification` row persists is the evidence the
//! command actually printed.
//!
//! The tasks arrive at `verification_pending` the way they do in the running
//! loop — ASSIGN hands them out, then MONITOR's `report_done` records the holder
//! finishing — rather than a status being poked by hand, because the state the
//! engine judges and the version it presents are the state and version those
//! two steps produce.

use director_adapters::executor::LocalExecutor;
use director_app::assign::{assign, AssignRequest};
use director_app::engine::{self, EngineRound, VerifyTaskError};
use director_app::monitor::{report_done, ReportRequest};
use director_domain::agent::{Agent, Harness};
use director_domain::capability::Capability;
use director_domain::ids::{
    AgentId, AssignmentId, MachineId, PlanId, ProjectId, TaskId, VerificationId,
};
use director_domain::plan::Plan;
use director_domain::project::Project;
use director_domain::task::{ExpectedOutput, Task, TaskStatus};
use director_domain::verification::VerificationStatus;
use director_domain::{AgentRepository, ProjectRepository, TaskRepository, VerificationRepository};
use director_store::Store;

/// A store, a project with an active plan, a registered agent, and a working
/// directory the probes run in — everything the engine needs, set up the way
/// the steps before it leave it.
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

        // Where the probes run. Any directory works: the probes do not depend
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

    async fn reload(&self, id: &str) -> Task {
        self.store
            .tasks()
            .get_task(&TaskId::from_string(id))
            .await
            .expect("task exists")
    }

    /// Run the engine for one task as it stands in the store.
    async fn verify(&self, id: &str) -> Result<EngineRound, VerifyTaskError> {
        let task = self.reload(id).await;
        engine::verify_task(
            &self.store,
            &LocalExecutor::default(),
            &task,
            &self.working_dir,
            chrono::Utc::now(),
        )
        .await
    }

    /// The judgments on record for a task, newest first.
    async fn history(&self, id: &str) -> Vec<director_domain::verification::Verification> {
        self.store
            .verifications()
            .verifications_for_task(&TaskId::from_string(id))
            .await
            .expect("history")
    }
}

#[tokio::test]
async fn a_passing_command_probe_lands_a_passing_verification() {
    // The engine's contract in one test: a check that really ran and really
    // exited zero becomes a persisted verdict, and the record says what ran.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked("the endpoint answers", "git --version")],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let round = fixture
        .verify("A")
        .await
        .expect("the engine lands a verdict");

    assert_eq!(round.verification.status, VerificationStatus::Passed);
    assert_eq!(round.outcomes.len(), 1);
    assert_eq!(round.outcomes[0].exit_code, 0);

    // The stored record is what a reader answers "why is this done?" from.
    let stored = fixture
        .store
        .verifications()
        .latest_verification(&TaskId::from_string("A"))
        .await
        .expect("loaded")
        .expect("there is a verification");
    assert_eq!(stored.status, VerificationStatus::Passed);
    assert_eq!(stored.evidence.len(), 1);
    assert_eq!(stored.evidence[0].criterion, "the endpoint answers");
    assert!(
        stored.evidence[0].detail.contains("git --version"),
        "the record names what ran"
    );
    assert!(stored.evidence[0].detail.contains("exit: 0"));
}

#[tokio::test]
async fn a_failing_command_probe_lands_a_failing_verification() {
    // The acceptance criterion, at the record layer: the command really exited
    // non-zero, and the record says so rather than recording a pass.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked("the endpoint answers", "git --no-such-flag")],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let round = fixture
        .verify("A")
        .await
        .expect("the engine lands a verdict");

    assert_eq!(round.verification.status, VerificationStatus::Failed);
    let stored = fixture
        .store
        .verifications()
        .latest_verification(&TaskId::from_string("A"))
        .await
        .expect("loaded")
        .expect("there is a verification");
    assert_eq!(stored.status, VerificationStatus::Failed);
    assert_eq!(stored.evidence.len(), 1);
    assert!(
        stored.evidence[0].detail.contains("git --no-such-flag"),
        "the record names the failing command"
    );
    // The evidence is what the command printed, not a generic "failed".
    assert!(!stored.evidence[0].detail.is_empty());
}

#[tokio::test]
async fn a_command_the_environment_cannot_run_is_unverifiable_not_failed() {
    // The program does not exist, so the executor refused. Broken evidence is
    // the environment's problem, and it must never become a verdict on the
    // work — this is the distinction the whole tri-state model exists to keep.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked(
            "the endpoint answers",
            "definitely-not-a-real-program --ok",
        )],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let round = fixture
        .verify("A")
        .await
        .expect("the engine lands a verdict");

    assert_eq!(round.verification.status, VerificationStatus::Unverifiable);
    assert_eq!(
        fixture.reload("A").await.status,
        TaskStatus::VerificationPending,
        "an unverifiable task is left exactly where the report put it"
    );
    let stored = fixture
        .store
        .verifications()
        .latest_verification(&TaskId::from_string("A"))
        .await
        .expect("loaded")
        .expect("the attempt is history even though it reached no verdict");
    assert_eq!(stored.status, VerificationStatus::Unverifiable);
}

#[tokio::test]
async fn unverifiable_evidence_does_not_mark_the_task_failed() {
    // Explicitly, because this is the rule the milestone names: a task the
    // engine could not gather evidence for is *not* failed, and a caller
    // surveying it again next tick is not re-judging a failure.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![
            checked("the endpoint answers", "git --version"),
            checked("the suite runs", "definitely-not-a-real-program"),
        ],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let round = fixture
        .verify("A")
        .await
        .expect("the engine lands a verdict");

    assert_eq!(round.verification.status, VerificationStatus::Unverifiable);
    let task = fixture.reload("A").await;
    assert_ne!(task.status, TaskStatus::Failed);
    assert_ne!(task.status, TaskStatus::Done);
    assert_eq!(task.status, TaskStatus::VerificationPending);
}

#[tokio::test]
async fn a_passing_verification_moves_the_task_to_done() {
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked("the endpoint answers", "git --version")],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let round = fixture
        .verify("A")
        .await
        .expect("the engine lands a verdict");

    assert_eq!(
        round.verification.status.task_status(),
        Some(TaskStatus::Done)
    );
    assert_eq!(fixture.reload("A").await.status, TaskStatus::Done);
}

#[tokio::test]
async fn a_failing_verification_moves_the_task_to_failed_and_not_done() {
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked("the endpoint answers", "git --no-such-flag")],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let round = fixture
        .verify("A")
        .await
        .expect("the engine lands a verdict");

    assert_eq!(
        round.verification.status.task_status(),
        Some(TaskStatus::Failed)
    );
    let task = fixture.reload("A").await;
    assert_eq!(task.status, TaskStatus::Failed);
    assert_ne!(
        task.status,
        TaskStatus::Done,
        "a failure never completes work"
    );
}

#[tokio::test]
async fn the_verification_carries_the_evidence_and_the_pinned_repository_state() {
    // The record has to be answerable on its own: which criteria, what each
    // probe found, and against which observed state of the project.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![
            checked("the endpoint answers", "git --version"),
            prose("the error messages read well"),
        ],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let round = fixture
        .verify("A")
        .await
        .expect("the engine lands a verdict");

    // Prose contributes no probe, so the evidence holds only what Orqyn asked.
    assert_eq!(round.outcomes.len(), 1);
    let stored = round.verification;
    assert_eq!(stored.evidence.len(), 1);
    assert_eq!(stored.evidence[0].criterion, "the endpoint answers");
    assert_eq!(
        stored.evidence[0].kind,
        director_domain::verification::ProbeKind::Command
    );
    assert_eq!(
        stored.evidence[0].status,
        director_domain::verification::EvidenceStatus::Passed
    );
    // The task and project the record claims.
    assert_eq!(stored.task_id, TaskId::from_string("A"));
    assert_eq!(stored.project_id, fixture.project);
    // No OBSERVE ran in this fixture, so the engine had no observed state to
    // pin the verdict to — and records that honestly rather than inventing a
    // commit.
    assert!(stored.repository_id.is_none());
    assert!(stored.head_commit.is_none());
}

#[tokio::test]
async fn verification_history_is_append_only() {
    // Judging a task twice leaves both judgments. Nothing is updated, nothing
    // is replaced: the second round writes a new row with a new id.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked("the endpoint answers", "git --no-such-flag")],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let first = fixture.verify("A").await.expect("the first judgment");
    assert_eq!(first.verification.status, VerificationStatus::Failed);

    // The rework: put the task back in front of verification and fix the check.
    let mut reworked = fixture.reload("A").await;
    reworked.status = TaskStatus::VerificationPending;
    reworked.expected_outputs = vec![checked("the endpoint answers", "git --version")];
    fixture
        .store
        .tasks()
        .update_task(&reworked)
        .await
        .expect("task requeued");

    let second = fixture.verify("A").await.expect("the second judgment");

    let history = fixture.history("A").await;
    assert_eq!(history.len(), 2, "both judgments survive");
    // Newest first.
    assert_eq!(history[0].id, second.verification.id);
    assert_eq!(history[1].id, first.verification.id);
    // The first judgment is unchanged by the second: it is still a failure.
    assert_eq!(history[1].status, VerificationStatus::Failed);
    assert_eq!(history[0].status, VerificationStatus::Passed);
    // And the ids differ, because a re-judgment is a new act, not an edit.
    assert_ne!(history[0].id, history[1].id);
}

#[tokio::test]
async fn an_unverifiable_round_records_itself_and_moves_nothing() {
    // "Orqyn looked, and could not yet tell" is history worth keeping, and it
    // moves nothing — which is exactly what makes the task safe to survey
    // again.
    let fixture = Fixture::new(&[("A".to_string(), vec![prose("it should feel finished")])]).await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let round = fixture
        .verify("A")
        .await
        .expect("the engine lands a verdict");

    assert_eq!(round.verification.status, VerificationStatus::Unverifiable);
    assert!(round.outcomes.is_empty(), "there were no checks to ask");
    assert_eq!(
        fixture.reload("A").await.status,
        TaskStatus::VerificationPending
    );
    // The attempt still landed, so a caller can see Orqyn tried.
    assert_eq!(fixture.history("A").await.len(), 1);
}

#[tokio::test]
async fn an_agents_self_report_cannot_complete_a_task() {
    // The model's central rule, tested at the seam: MONITOR's report stops at
    // `verification_pending`. There is no argument to `report_done` and no
    // path from it to `done`, and no verification exists for work Orqyn has
    // not judged.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked("the endpoint answers", "git --version")],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let task = fixture.reload("A").await;
    assert_eq!(
        task.status,
        TaskStatus::VerificationPending,
        "a report leaves the task awaiting a verdict"
    );
    assert_ne!(task.status, TaskStatus::Done);
    assert!(
        fixture
            .store
            .verifications()
            .latest_verification(&TaskId::from_string("A"))
            .await
            .expect("history loads")
            .is_none(),
        "no verification exists for unjudged work"
    );
}

#[tokio::test]
async fn the_engine_never_writes_the_task_row_itself() {
    // The engine's write is `apply_verification` alone. The witness to that is
    // atomicity: the verification row, the status move, and the history row
    // carry one timestamp, because they were one transaction — and the task's
    // version advanced exactly once, because nothing else wrote it.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked("the endpoint answers", "git --version")],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;
    let before = fixture.reload("A").await;

    let round = fixture
        .verify("A")
        .await
        .expect("the engine lands a verdict");
    let after = fixture.reload("A").await;

    assert_eq!(after.state_version, before.state_version + 1);
    // The record and the move are the same act, so they agree on when it
    // happened.
    assert_eq!(round.verification.created_at, after.updated_at);

    // And the move is in the append-only history, recorded once.
    let history = fixture
        .store
        .tasks()
        .task_history(&TaskId::from_string("A"))
        .await
        .expect("history");
    let moves_to_done = history
        .iter()
        .filter(|transition| transition.to == TaskStatus::Done)
        .count();
    assert_eq!(moves_to_done, 1);
}

#[tokio::test]
async fn a_stale_engine_round_cannot_move_a_task_that_already_moved() {
    // Two rounds both read the task at the same version; only one may write the
    // next one. The loser is refused with a conflict, and — because the
    // verification row and the move are one transaction — it leaves no judgment
    // behind either.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked("the endpoint answers", "git --version")],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;
    let read = fixture.reload("A").await;

    // The round that lands first.
    let _first = engine::verify_task(
        &fixture.store,
        &LocalExecutor::default(),
        &read,
        &fixture.working_dir,
        chrono::Utc::now(),
    )
    .await
    .expect("the first round lands");

    // The stale round, still holding the version it read before the first one
    // landed.
    let err = engine::verify_task(
        &fixture.store,
        &LocalExecutor::default(),
        &read,
        &fixture.working_dir,
        chrono::Utc::now(),
    )
    .await
    .expect_err("a stale round is refused");

    match err {
        VerifyTaskError::Store(store_error) => {
            assert!(
                matches!(
                    store_error,
                    director_domain::StoreError::StateVersionConflict { .. }
                ),
                "expected a version conflict, got {store_error}"
            );
        }
        other => panic!("expected a store error, got {other:?}"),
    }

    // Nothing from the stale round survived.
    assert_eq!(
        fixture.history("A").await.len(),
        1,
        "only one judgment landed"
    );
    assert_eq!(fixture.reload("A").await.status, TaskStatus::Done);
}

#[tokio::test]
async fn a_task_with_no_project_cannot_be_verified() {
    // A task the store itself would refuse to hold — no project to persist a
    // judgment against — is rejected before anything runs.
    let mut orphan = Task::for_project(
        ProjectId::from_string("PROJ-1"),
        TaskId::from_string("ORPHAN"),
        "orphan",
        "do it",
    );
    orphan.project_id = None;
    orphan.expected_outputs = vec![checked("the endpoint answers", "git --version")];

    let fixture = Fixture::new(&[]).await;
    let err = engine::verify_task(
        &fixture.store,
        &LocalExecutor::default(),
        &orphan,
        &fixture.working_dir,
        chrono::Utc::now(),
    )
    .await
    .expect_err("an orphan task is refused");

    assert!(
        matches!(err, VerifyTaskError::NoProject(task) if task == TaskId::from_string("ORPHAN"))
    );
    // And it wrote nothing.
    assert!(fixture
        .store
        .verifications()
        .latest_verification(&TaskId::from_string("ORPHAN"))
        .await
        .expect("history loads")
        .is_none());
}

#[tokio::test]
async fn a_task_reaching_done_through_the_step_has_a_verification_record() {
    // The loop's entry point, not the engine's: `verify` surveys the project,
    // and a task it passes must have a durable record behind its `done`.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![checked("the endpoint answers", "git --version")],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let report = director_app::verify::verify(
        &fixture.store,
        &LocalExecutor::default(),
        &fixture.project,
        &fixture.working_dir,
    )
    .await
    .expect("the round");

    assert_eq!(report.judged.len(), 1);
    assert_eq!(fixture.reload("A").await.status, TaskStatus::Done);

    let verification = fixture
        .store
        .verifications()
        .latest_verification(&TaskId::from_string("A"))
        .await
        .expect("history loads")
        .expect("a done task has a verification behind it");
    assert_eq!(verification.status, VerificationStatus::Passed);
    assert_eq!(
        verification.id,
        VerificationId::from_string("VER-A-1"),
        "the id names the task and its position in that task's history"
    );
}

#[tokio::test]
async fn a_done_task_and_its_verification_agree_about_the_evidence() {
    // The record is not decoration: the verdict it states is the verdict the
    // evidence it holds supports, which is what makes it safe to trust the row
    // instead of re-running the checks.
    let fixture = Fixture::new(&[(
        "A".to_string(),
        vec![
            checked("the endpoint answers", "git --version"),
            checked("the logout endpoint answers", "git --version"),
        ],
    )])
    .await;
    fixture.arrive_at_verification("A", "ASG-1").await;

    let round = fixture
        .verify("A")
        .await
        .expect("the engine lands a verdict");

    assert_eq!(round.verification.status, VerificationStatus::Passed);
    assert_eq!(round.verification.evidence.len(), 2);
    assert!(round
        .verification
        .evidence
        .iter()
        .all(|evidence| evidence.status == director_domain::verification::EvidenceStatus::Passed));
    assert!(round.verification.is_passing());
}
