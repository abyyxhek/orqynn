//! Action-recording reliability: determinism, idempotency, and honesty about how
//! much history a briefing actually holds.
//!
//! Every test here runs against a real SQLite store and the real executor. The
//! properties under test are about durability and ordering, which an in-memory
//! stand-in would agree with while telling us nothing.

use std::path::PathBuf;
use std::process::Command;

use chrono::{DateTime, TimeZone, Utc};
use director_adapters::executor::LocalExecutor;
use director_adapters::git::GitService;
use director_adapters::InMemoryProvider;
use director_app::plan::{PlanRequest, TaskSpec};
use director_app::runner::Runner;
use director_domain::agent::{Agent, Harness};
use director_domain::capability::Capability;
use director_domain::ids::{MachineId, PlanId, ProjectId, RepositoryId, TaskId};
use director_domain::task::{ExpectedOutput, Task, TaskStatus};
use director_domain::{ActionRepository, AgentRepository, TaskRepository, VerificationRepository};
use director_store::Store;

const PROJECT: &str = "PROJ-1";
const REPO: &str = "REPO-1";
const PASSES: &str = "git --version";

/// The pinned instant every deterministic test uses. If anything in the recording
/// path still reads the wall clock, comparing two runs will show it.
fn pinned() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 4, 6, 9, 30, 0).unwrap()
}

fn later(seconds: i64) -> DateTime<Utc> {
    pinned() + chrono::Duration::seconds(seconds)
}

struct Fixture {
    store_path: PathBuf,
    store: Store,
    git: GitService,
    memory: InMemoryProvider,
    executor: LocalExecutor,
    repo_dir: PathBuf,
    project: ProjectId,
    repository: RepositoryId,
    _tmp: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("a temp dir");
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("repo dir");
        let git_in = || {
            let mut cmd = Command::new("git");
            cmd.current_dir(&repo_dir);
            cmd
        };
        git_in().args(["init", "-q"]).status().expect("git init");
        for (key, value) in [
            ("user.name", "Orqyn Tests"),
            ("user.email", "tests@orqyn.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            git_in()
                .args(["config", key, value])
                .status()
                .expect("config");
        }
        std::fs::write(repo_dir.join("README.md"), "# fixture\n").expect("write");
        git_in().args(["add", "README.md"]).status().expect("add");
        git_in()
            .args(["commit", "-q", "-m", "initial"])
            .status()
            .expect("commit");

        let state_dir = tmp.path().join("git-state");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let store_path = tmp.path().join("orqyn.db");
        let store = Store::open(&store_path).await.expect("store opens");

        Fixture {
            store_path,
            store,
            git: GitService::new(&state_dir).expect("git service"),
            memory: InMemoryProvider::new(),
            executor: LocalExecutor::default(),
            repo_dir,
            project: ProjectId::from_string(PROJECT),
            repository: RepositoryId::from_string(REPO),
            _tmp: tmp,
        }
    }

    fn runner(&self) -> Runner<'_, InMemoryProvider, LocalExecutor> {
        Runner::new(
            &self.store,
            &self.git,
            &self.memory,
            &self.executor,
            &self.repo_dir,
        )
    }

    fn repo_path(&self) -> String {
        self.repo_dir.to_string_lossy().replace('\\', "/")
    }

    async fn prepared(&self) {
        let mut runner = self.runner();
        runner
            .observe(
                &self.project,
                &self.repository,
                &self.repo_path(),
                later(-100),
            )
            .await
            .expect("the first observation");
        self.store
            .agents()
            .register_agent(&Agent::register(
                director_domain::ids::AgentId::from_string("AGENT-1"),
                "AGENT-1",
                Harness::ClaudeCode,
                MachineId::from_string("MACH-a"),
                vec![Capability::Coding],
            ))
            .await
            .expect("agent registered");
    }

    /// Register a second agent, for tests that need two tasks in flight at once.
    ///
    /// SCHEDULE pairs a task with an *available* agent, so a second task cannot be
    /// handed out while the first still holds the only one.
    async fn register_second_agent(&self) {
        self.store
            .agents()
            .register_agent(&Agent::register(
                director_domain::ids::AgentId::from_string("AGENT-2"),
                "AGENT-2",
                Harness::ClaudeCode,
                MachineId::from_string("MACH-a"),
                vec![Capability::Coding],
            ))
            .await
            .expect("the second agent registers");
    }

    /// Plan, hand out, and drive the agent's reports so the task is awaiting a
    /// verdict. The simulated turn is the only way to reach VERIFY without an
    /// agent driver.
    ///
    /// Takes the runner rather than making one, so a caller can drive several
    /// rounds through the same runner. Assignment ids are allocated by the store
    /// inside the transaction that writes the row, so two runners would not
    /// collide — the tests here stay inside one runner anyway because a single
    /// runner keeps each test's account of the flow simple to read.
    async fn to_verification_pending_with(
        &self,
        runner: &mut Runner<'_, InMemoryProvider, LocalExecutor>,
        task: &str,
        checks: &[&str],
        plan_id: &str,
    ) {
        let task_id = TaskId::from_string(task);
        let mut spec = TaskSpec::new(task_id.clone(), task, format!("do {task}"));
        spec.expected_outputs = checks
            .iter()
            .map(|check| ExpectedOutput {
                criterion: format!("{task} satisfies {check}"),
                check: Some((*check).to_string()),
            })
            .collect();
        runner
            .plan(
                PlanRequest {
                    project_id: self.project.clone(),
                    plan_id: PlanId::from_string(plan_id),
                    objective: "make the fixture verifiable".into(),
                    rationale: "the reliability harness's smallest path".into(),
                    tasks: vec![spec],
                    authorized_by: director_domain::ids::AgentId::from_string("AGENT-planner"),
                },
                later(-90),
            )
            .await
            .expect("the plan lands");
        let scheduled = runner
            .schedule(&self.project, later(-80))
            .await
            .expect("the scheduling round");
        let assignment = scheduled
            .assignment_for(&task_id)
            .unwrap_or_else(|| panic!("{task} was handed out; report: {scheduled:?}"))
            .clone();
        runner
            .simulate_agent_turn(&assignment, later(-70))
            .await
            .expect("the simulated turn");
    }

    /// [`Self::to_verification_pending_with`] on a runner of its own.
    async fn to_verification_pending(&self, task: &str, checks: &[&str]) {
        let mut runner = self.runner();
        self.to_verification_pending_with(&mut runner, task, checks, "PLAN-1")
            .await;
    }

    /// Run one judged round at the pinned instant and read back the whole log.
    async fn judged_action_log(&self, task: &str) -> Vec<String> {
        let actions = self
            .store
            .actions()
            .actions_for_task(&TaskId::from_string(task))
            .await
            .expect("the action log reads");
        actions
            .into_iter()
            .map(|action| serde_json::to_string(&action).expect("an action serializes"))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

/// The same pinned logical run must produce the same serialized action log.
///
/// This is the property that clock-derived identifiers cannot hold: two runs of
/// identical inputs separated by real time produced different ids and different
/// timestamps, so no run could ever be compared with a later run of the same
/// thing.
#[tokio::test]
async fn the_same_pinned_run_produces_an_identical_action_log() {
    let mut logs = Vec::new();
    for _ in 0..2 {
        let fixture = Fixture::new().await;
        fixture.prepared().await;
        fixture.to_verification_pending("AUTH", &[PASSES]).await;

        let mut runner = fixture.runner();
        let step = runner
            .verify_at(&fixture.project, pinned())
            .await
            .expect("the verification round");
        assert_eq!(step.judged.len(), 1);

        logs.push(fixture.judged_action_log("AUTH").await);
    }

    assert_eq!(logs[0].len(), 1, "the round recorded one command");
    assert_eq!(
        logs[0], logs[1],
        "two runs of the same pinned inputs produced different action logs, so something \
         in the recording path is still reading the clock"
    );
}

/// Two tasks judged in one run get distinct actions.
///
/// A generator's counters restart with every runner, so two runners in one process
/// would once have minted the same identifier for two different tasks — and the
/// second insert would have failed the round. The id is keyed on the verification
/// instead, so it cannot.
#[tokio::test]
async fn two_tasks_judged_in_one_run_get_distinct_actions() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture.register_second_agent().await;

    // One runner for both tasks, so the flow reads as a single driver rather than
    // two. Assignment ids are allocated by the store, so sharing a runner is a
    // readability choice here and not a constraint.
    let mut runner = fixture.runner();
    fixture
        .to_verification_pending_with(&mut runner, "ALPHA", &[PASSES], "PLAN-1")
        .await;
    fixture
        .to_verification_pending_with(&mut runner, "BETA", &[PASSES], "PLAN-2")
        .await;

    runner
        .verify_at(&fixture.project, pinned())
        .await
        .expect("both tasks are judged in one round");

    let alpha = fixture.judged_action_log("ALPHA").await;
    let beta = fixture.judged_action_log("BETA").await;
    assert_eq!(alpha.len(), 1);
    assert_eq!(beta.len(), 1);

    let id_of = |line: &String| {
        let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        value["id"].as_str().expect("an id").to_string()
    };
    assert_ne!(
        id_of(&alpha[0]),
        id_of(&beta[0]),
        "two tasks judged in one run must not share an action id"
    );
}

/// Actions that share a timestamp read back in a stable order.
///
/// Every action from one round carries the round's single instant, so equal
/// timestamps are the normal case rather than an edge case. Ordering therefore
/// has to come from the order the actions were recorded, and it has to be
/// *stable*: a briefing that reordered itself between two reads of the same rows
/// would disagree with itself for no reason.
///
/// This is the two-action case, where recorded order and lexical id order happen
/// to coincide. `action_ordering.rs` covers the cases where they do not.
#[tokio::test]
async fn actions_sharing_a_timestamp_read_back_in_a_stable_order() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    // Two checks in one round: same instant, two actions.
    fixture
        .to_verification_pending("MULTI", &[PASSES, "git --version"])
        .await;

    let mut runner = fixture.runner();
    runner
        .verify_at(&fixture.project, pinned())
        .await
        .expect("the verification round");

    let first = fixture.judged_action_log("MULTI").await;
    assert_eq!(
        first.len(),
        2,
        "two commands ran, so two actions exist and they share the round's timestamp"
    );

    // Confirm the premise: the timestamps really are equal.
    let actions = fixture
        .store
        .actions()
        .actions_for_task(&TaskId::from_string("MULTI"))
        .await
        .expect("actions read");
    assert_eq!(
        actions[0].at, actions[1].at,
        "the test is only meaningful if the two actions really do tie on time"
    );

    // Repeated reads agree, and the order is the order the round recorded them in
    // — the tie-break. With two checks that is also ascending by id, so this
    // assertion cannot tell the two mechanisms apart on its own; it pins the
    // stability contract, which is the point of the test.
    for _ in 0..3 {
        assert_eq!(
            fixture.judged_action_log("MULTI").await,
            first,
            "the same rows read twice gave two different orders"
        );
    }
    let ordinals: Vec<usize> = actions.iter().map(|a| ordinal_of(&a.id)).collect();
    assert_eq!(
        ordinals,
        vec![0, 1],
        "the round's own checks stay in order, which is the order they were written"
    );
}

/// The check index embedded in an action id, i.e. the `0` in `ACT-...-1-0`.
fn ordinal_of(id: &director_domain::ids::ActionId) -> usize {
    id.as_str()
        .rsplit('-')
        .next()
        .and_then(|last| last.parse().ok())
        .unwrap_or_else(|| panic!("{id} carries no ordinal"))
}

// ---------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------

/// Re-running a round does not duplicate anything.
///
/// The task has left the survey set after a decisive verdict, so a second round
/// has nothing to judge — and because the log is append-only, "nothing to judge"
/// has to mean nothing was written.
#[tokio::test]
async fn re_running_a_judged_round_writes_nothing_further() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture.to_verification_pending("AUTH", &[PASSES]).await;

    let mut runner = fixture.runner();
    runner
        .verify_at(&fixture.project, pinned())
        .await
        .expect("the first round");
    let after_first = fixture.judged_action_log("AUTH").await;
    assert_eq!(after_first.len(), 1);

    let second = runner
        .verify_at(&fixture.project, later(60))
        .await
        .expect("the second round");
    assert!(
        second.judged.is_empty(),
        "a done task is not surveyed again, so the replay had nothing to record"
    );
    assert_eq!(
        fixture.judged_action_log("AUTH").await,
        after_first,
        "a replayed round must not add a second copy of the same event"
    );
}

/// A task retried after a failure gets its own actions, not a duplicate.
///
/// This is the case a naive idempotency key gets wrong: keying on the *task*
/// would make the second judgment look like the first and swallow a real event.
/// Keyed on the verification, the retry is a new round with a new identity.
#[tokio::test]
async fn a_retried_task_gets_its_own_events_rather_than_a_duplicate() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    // One runner for the whole flow: assignment ids come from a per-runner
    // counter, so a second runner would collide on `agent_assignments.id` before
    // the retry could be judged at all.
    let mut runner = fixture.runner();
    // A check that fails, so the task lands in `failed` and is retryable.
    fixture
        .to_verification_pending_with(&mut runner, "AUTH", &["git --no-such-flag"], "PLAN-1")
        .await;

    runner
        .verify_at(&fixture.project, pinned())
        .await
        .expect("the first round fails the task");
    let after_first = fixture.judged_action_log("AUTH").await;
    assert_eq!(after_first.len(), 1, "the failed command is on record");

    let task = fixture
        .store
        .tasks()
        .get_task(&TaskId::from_string("AUTH"))
        .await
        .expect("the task reads");
    assert_eq!(task.status, TaskStatus::Failed);

    // REPLAN reopens it, and the loop drives it back to a verdict.
    runner
        .replan(
            director_app::replan::ReplanRequest {
                project_id: fixture.project.clone(),
                decisions: vec![director_app::replan::TaskDecision {
                    task_id: TaskId::from_string("AUTH"),
                    remediation: director_app::replan::Remediation::Retry,
                    reason: None,
                }],
                decided_by: None,
            },
            later(10),
        )
        .await
        .expect("the retry lands");
    let reopened = fixture
        .store
        .tasks()
        .get_task(&TaskId::from_string("AUTH"))
        .await
        .expect("the task reads");
    assert_eq!(
        reopened.status,
        TaskStatus::Todo,
        "a retry hands the task back to the loop"
    );

    // Bring it to a verdict again with the same runner, so its assignment counter
    // keeps advancing.
    {
        let scheduled = runner
            .schedule(&fixture.project, later(20))
            .await
            .expect("the retry is scheduled");
        let assignment = scheduled
            .assignment_for(&TaskId::from_string("AUTH"))
            .unwrap_or_else(|| panic!("the retried task was handed out; report: {scheduled:?}"))
            .clone();
        runner
            .simulate_agent_turn(&assignment, later(30))
            .await
            .expect("the retried turn");
    }

    runner
        .verify_at(&fixture.project, later(40))
        .await
        .expect("the second round");

    let after_second = fixture.judged_action_log("AUTH").await;
    assert_eq!(
        after_second.len(),
        2,
        "two judgments happened, so the log holds two records - not one swallowed as a duplicate"
    );
    let id_of = |line: &String| {
        let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        value["id"].as_str().expect("an id").to_string()
    };
    assert_ne!(
        id_of(&after_second[0]),
        id_of(&after_second[1]),
        "the retry is a distinct event with a distinct id"
    );
}

/// The recorded log is durable, not writer memory.
///
/// The writer is dropped and a fresh handle opens the same file, which is the
/// only way to tell a committed action from one still sitting in a pool.
#[tokio::test]
async fn the_recorded_log_survives_closing_and_reopening_the_store() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture.to_verification_pending("AUTH", &[PASSES]).await;

    let mut runner = fixture.runner();
    runner
        .verify_at(&fixture.project, pinned())
        .await
        .expect("the verification round");
    let before = fixture.judged_action_log("AUTH").await;
    assert_eq!(before.len(), 1);
    drop(runner);

    let reopened = Store::open(&fixture.store_path)
        .await
        .expect("the store reopens");
    let after: Vec<String> = reopened
        .actions()
        .actions_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("the log reads")
        .into_iter()
        .map(|action| serde_json::to_string(&action).expect("an action serializes"))
        .collect();

    assert_eq!(
        after, before,
        "a committed action must read back identically after a restart"
    );
}

// ---------------------------------------------------------------------------
// Honesty about completeness
// ---------------------------------------------------------------------------

/// A window with records in it is still not the whole history.
///
/// `actions_available` says Orqyn recorded something it ran. It cannot say it
/// recorded everything, because nothing observes what an agent did between being
/// handed a task and reporting on it. A briefing must not be able to read
/// "available" as "complete".
#[tokio::test]
async fn a_recorded_window_is_available_and_still_not_complete() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture.to_verification_pending("AUTH", &[PASSES]).await;

    // A briefing only exists for work in flight, and a judged task is terminal —
    // so the two states cannot be reached by one round. The action is written
    // directly here: what is under test is how recovery *reports* a window, not
    // which step produced it.
    let task = TaskId::from_string("AUTH");
    let mut action = director_domain::action::Action::new(
        director_domain::ids::ActionId::from_string("ACT-seeded-0"),
        director_domain::action::ActionKind::Command,
        "ran the check",
    )
    .for_task(task.clone());
    action.at = pinned();
    fixture
        .store
        .actions()
        .record_action(&action)
        .await
        .expect("the action lands");
    fixture
        .store
        .tasks()
        .update_task(&Task {
            status: TaskStatus::InProgress,
            ..fixture
                .store
                .tasks()
                .get_task(&task)
                .await
                .expect("the task reads")
        })
        .await
        .expect("the task is in flight again");

    // A briefing needs a checkpoint, and OBSERVE only publishes one for an
    // in-flight task when it records a real change.
    std::fs::create_dir_all(fixture.repo_dir.join("src")).expect("a directory");
    std::fs::write(fixture.repo_dir.join("src").join("mid.rs"), "// mid\n").expect("a write");
    let git = || {
        let mut cmd = Command::new("git");
        cmd.current_dir(&fixture.repo_dir);
        cmd
    };
    git().args(["add", "-A"]).status().expect("add");
    git()
        .args(["commit", "-q", "-m", "in flight"])
        .status()
        .expect("commit");
    let mut observer = fixture.runner();
    let observed = observer
        .observe(
            &fixture.project,
            &fixture.repository,
            &fixture.repo_path(),
            later(5),
        )
        .await
        .expect("an observation of the change");
    assert_eq!(
        observed.checkpoints_written, 1,
        "the in-flight task's checkpoint was published"
    );

    let outcome = director_app::recovery::RecoveryBuilder::new(&fixture.store, task.clone())
        .build()
        .await
        .expect("the recovery build");
    let recovery = match outcome {
        director_app::recovery::RecoveryOutcome::Built(recovery) => recovery,
        other => panic!("an in-flight task with a checkpoint must build a briefing, got {other:?}"),
    };

    assert!(
        recovery.actions_available(),
        "the precondition: the window has a record in it"
    );
    assert!(
        !recovery.actions_complete(),
        "a briefing must never claim its action window is the whole history; nothing \
         observes what an agent did between its reports"
    );
    assert!(
        !recovery.gaps().actions_incomplete,
        "nothing is provably missing here, which is a weaker claim than completeness \
         and must not be reported as the stronger one"
    );
}

/// The verdict and its actions commit together, so the gap cannot open.
///
/// This used to be the defect: the step committed its verdict in one transaction
/// and the actions recording what it observed were written afterwards, in
/// transactions of their own, and a process killed in between left a durable
/// verdict with no record of what it had run. The actions now go through the same
/// write, so a caller running the step directly — with no runner, and therefore
/// nothing appending afterwards — still gets both or neither.
#[tokio::test]
async fn the_step_writes_the_verdict_and_its_actions_together() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture.to_verification_pending("AUTH", &[PASSES]).await;

    // The step on its own: no runner, no recording pass of any kind.
    let report = director_app::verify::verify_at(
        &fixture.store,
        &fixture.executor,
        &fixture.project,
        &fixture.repo_dir,
        pinned(),
    )
    .await
    .expect("the verification round commits");
    assert_eq!(report.judged.len(), 1);

    let verifications = fixture
        .store
        .verifications()
        .verifications_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("verifications read");
    assert_eq!(verifications.len(), 1, "the verdict is durable");

    let log = fixture.judged_action_log("AUTH").await;
    assert_eq!(
        log.len(),
        1,
        "and its actions came with it, in the same transaction"
    );
    assert_eq!(
        log[0].clone(),
        serde_json::to_string(
            &fixture
                .store
                .actions()
                .get_action(&director_domain::ids::ActionId::from_string(format!(
                    "ACT-{}-0",
                    verifications[0].id
                )))
                .await
                .expect("the action reads back")
        )
        .expect("serializes"),
        "the action is keyed on the verification it belongs to"
    );

    // Nothing is provably missing, so the gap flag must stay quiet.
    let outcome =
        director_app::recovery::RecoveryBuilder::new(&fixture.store, TaskId::from_string("AUTH"))
            .build()
            .await
            .expect("the recovery build");
    if let director_app::recovery::RecoveryOutcome::Built(recovery) = outcome {
        assert!(
            !recovery.gaps().actions_incomplete,
            "an atomically committed verdict reports no gap"
        );
    }
}

/// The gap detector still works, which is what makes its silence meaningful.
///
/// `actions_incomplete` is the canary for a future path that commits a verdict
/// without its observations. It is exercised here by writing a verification
/// directly through the repository — bypassing the atomic path, exactly as an
/// older Orqyn or a hand-made record would — so the detector is proven rather
/// than merely unused.
#[tokio::test]
async fn a_verdict_written_without_its_actions_is_still_reported() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture.to_verification_pending("AUTH", &[PASSES]).await;

    // Judge for real is unnecessary here: a verdict with no actions is still a
    // reachable state, because `apply_verification` — the API the engine used
    // before the atomic write, still supported for callers that record their own
    // observations — commits exactly this shape.
    let task = TaskId::from_string("AUTH");
    let stored = fixture
        .store
        .tasks()
        .get_task(&task)
        .await
        .expect("the task reads");
    let verification = director_domain::verification::Verification::new(
        director_domain::ids::VerificationId::from_string("VER-HANDMADE-1"),
        task.clone(),
        fixture.project.clone(),
        None,
        director_domain::verification::VerificationStatus::Passed,
        vec![director_domain::verification::Evidence {
            kind: director_domain::verification::ProbeKind::Command,
            criterion: "handmade".into(),
            status: director_domain::verification::EvidenceStatus::Passed,
            detail: "the check passed".into(),
        }],
        None,
        pinned(),
    );
    fixture
        .store
        .apply_verification(&verification, stored.state_version, pinned())
        .await
        .expect("a verdict with no actions is still writable through this API");

    assert!(
        fixture.judged_action_log("AUTH").await.is_empty(),
        "the verdict really did land with no observations behind it"
    );

    let outcome =
        director_app::recovery::RecoveryBuilder::new(&fixture.store, TaskId::from_string("AUTH"))
            .build()
            .await
            .expect("the recovery build");
    if let director_app::recovery::RecoveryOutcome::Built(recovery) = outcome {
        assert!(
            recovery.gaps().actions_incomplete,
            "a verdict with no action behind it must be reported, not hidden"
        );
        assert!(
            !recovery.gaps().is_clean(),
            "and a known hole is not a clean build"
        );
    }
}

/// Recovery invents nothing for a task with no history at all.
///
/// The other direction of the same honesty question: an absent record is reported
/// as absent, and is never back-filled from task status or a git change.
#[tokio::test]
async fn recovery_invents_no_events_for_a_task_with_no_history() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;

    // Plan and hand out, but never judge: nothing has been observed.
    {
        let mut runner = fixture.runner();
        let task_id = TaskId::from_string("AUTH");
        let mut spec = TaskSpec::new(task_id.clone(), "auth", "do auth");
        spec.expected_outputs = vec![ExpectedOutput {
            criterion: "auth answers".into(),
            check: Some(PASSES.into()),
        }];
        runner
            .plan(
                PlanRequest {
                    project_id: fixture.project.clone(),
                    plan_id: PlanId::from_string("PLAN-1"),
                    objective: "make the fixture verifiable".into(),
                    rationale: "no round has judged this yet".into(),
                    tasks: vec![spec],
                    authorized_by: director_domain::ids::AgentId::from_string("AGENT-planner"),
                },
                later(-90),
            )
            .await
            .expect("the plan lands");
        let scheduled = runner
            .schedule(&fixture.project, later(-80))
            .await
            .expect("the scheduling round");
        let assignment = scheduled
            .assignment_for(&task_id)
            .expect("handed out")
            .clone();
        runner
            .simulate_agent_turn(&assignment, later(-70))
            .await
            .expect("the turn");
    }

    assert!(
        fixture.judged_action_log("AUTH").await.is_empty(),
        "nothing ran, so nothing is recorded"
    );

    // A git change must not become an action either.
    std::fs::create_dir_all(fixture.repo_dir.join("src")).expect("dir");
    std::fs::write(fixture.repo_dir.join("src").join("a.rs"), "// work\n").expect("write");
    let git = || {
        let mut cmd = Command::new("git");
        cmd.current_dir(&fixture.repo_dir);
        cmd
    };
    git().args(["add", "-A"]).status().expect("add");
    git()
        .args(["commit", "-q", "-m", "agent work"])
        .status()
        .expect("commit");

    let mut runner = fixture.runner();
    runner
        .observe(
            &fixture.project,
            &fixture.repository,
            &fixture.repo_path(),
            later(-50),
        )
        .await
        .expect("an observation of the change");

    assert!(
        fixture.judged_action_log("AUTH").await.is_empty(),
        "a commit is not an action; Orqyn does not infer activity from a diff"
    );
}
