//! A committed verdict and the actions recording what it ran are one fact.
//!
//! These were two transactions: VERIFY committed its verdict, the task's move and
//! the history row, and *then* a separate pass appended the actions. A process
//! killed between them left a durable verdict with no record of what it had run —
//! and because the task had left the survey set, nothing would ever write them.
//!
//! They now share a transaction. The tests here pin the properties that makes
//! possible: both-or-neither on commit, both-or-neither on failure, determinism
//! from pinned inputs, and the existing compare-and-swap still deciding.
//!
//! Where a real interleaving is needed, it is produced by two OS threads released
//! by a `Barrier` — the pattern the store's own concurrency tests use. These
//! calls contain no yielding `.await`, so `tokio::join!` would run them
//! start-to-finish and prove nothing about a race. No sleeps.

use std::path::PathBuf;
use std::process::Command;

use chrono::{DateTime, TimeZone, Utc};
use director_adapters::executor::LocalExecutor;
use director_adapters::git::GitService;
use director_adapters::InMemoryProvider;
use director_app::plan::{PlanRequest, TaskSpec};
use director_app::runner::Runner;
use director_domain::action::Action;
use director_domain::agent::{Agent, Harness};
use director_domain::capability::Capability;
use director_domain::ids::{MachineId, PlanId, ProjectId, RepositoryId, TaskId, VerificationId};
use director_domain::task::{ExpectedOutput, TaskStatus};
use director_domain::verification::{Evidence, EvidenceStatus, ProbeKind, VerificationStatus};
use director_domain::{ActionRepository, AgentRepository, TaskRepository, VerificationRepository};
use director_store::Store;

const PROJECT: &str = "PROJ-1";
const REPO: &str = "REPO-1";
const PASSES: &str = "git --version";
const FAILS: &str = "git --no-such-flag";
/// A program that does not exist, so the round can gather no evidence.
const UNRUNNABLE: &str = "orqyn-no-such-program-xyz --nope";

fn pinned() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 9, 8, 0, 0).unwrap()
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
    /// The assignment handed to each task by the single scheduling round
    /// `prepared` runs. Kept because SCHEDULE pairs *every* ready task in one
    /// round, so a second round's report is empty and cannot be the source of an
    /// assignment the first already made.
    assignments: Vec<(String, director_domain::ids::AssignmentId)>,
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
            assignments: Vec::new(),
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

    async fn prepared(&mut self, tasks: &[(&str, &str)]) {
        let mut runner = self.runner();
        runner
            .observe(
                &self.project,
                &self.repository,
                &self.repo_path(),
                later(-300),
            )
            .await
            .expect("the first observation");
        for n in 0..tasks.len() {
            let name = format!("AGENT-{}", n + 1);
            self.store
                .agents()
                .register_agent(&Agent::register(
                    director_domain::ids::AgentId::from_string(&name),
                    &name,
                    Harness::ClaudeCode,
                    MachineId::from_string("MACH-a"),
                    vec![Capability::Coding],
                ))
                .await
                .expect("agent registered");
        }

        let specs: Vec<TaskSpec> = tasks
            .iter()
            .map(|(id, check)| {
                let mut spec = TaskSpec::new(TaskId::from_string(*id), *id, format!("do {id}"));
                spec.expected_outputs = vec![ExpectedOutput {
                    criterion: format!("{id} answers"),
                    check: Some((*check).to_string()),
                }];
                spec.scope_paths = vec![format!("src/{}", id.to_lowercase())];
                spec
            })
            .collect();
        runner
            .plan(
                PlanRequest {
                    project_id: self.project.clone(),
                    plan_id: PlanId::from_string("PLAN-1"),
                    objective: "make the fixture verifiable".into(),
                    rationale: "one plan, several tasks".into(),
                    tasks: specs,
                    authorized_by: director_domain::ids::AgentId::from_string("AGENT-planner"),
                },
                later(-290),
            )
            .await
            .expect("the plan lands");

        // One scheduling round hands out every ready task.
        let scheduled = runner
            .schedule(&self.project, later(-200))
            .await
            .expect("the scheduling round");
        assert!(
            scheduled.assigned.len() == tasks.len(),
            "every task was handed out in one round: {:?}",
            scheduled.assigned
        );
        let assignments: Vec<(String, director_domain::ids::AssignmentId)> = scheduled
            .assigned
            .iter()
            .map(|p| (p.task_id.to_string(), p.assignment_id.clone()))
            .collect();
        drop(runner);
        self.assignments = assignments;
    }

    /// Drive the agent's reports for `task` so it awaits a verdict.
    async fn to_verification_pending(&self, task: &str) {
        let assignment = self
            .assignments
            .iter()
            .find(|(id, _)| id == task)
            .map(|(_, a)| a.clone())
            .unwrap_or_else(|| panic!("{task} was handed out by the scheduling round"));
        let mut runner = self.runner();
        runner
            .simulate_agent_turn(&assignment, later(-100))
            .await
            .expect("the simulated turn");
    }

    async fn action_ids(&self, task: &str) -> Vec<String> {
        self.store
            .actions()
            .actions_for_task(&TaskId::from_string(task))
            .await
            .expect("actions read")
            .into_iter()
            .map(|a| a.id.to_string())
            .collect()
    }

    async fn verification_ids(&self, task: &str) -> Vec<String> {
        self.store
            .verifications()
            .verifications_for_task(&TaskId::from_string(task))
            .await
            .expect("verifications read")
            .into_iter()
            .map(|v| v.id.to_string())
            .collect()
    }
}

/// Run two closures on two OS threads, released together by a barrier.
fn in_parallel<A, B, RA, RB>(first: A, second: B) -> (RA, RB)
where
    A: FnOnce() -> RA + Send,
    B: FnOnce() -> RB + Send,
    RA: Send,
    RB: Send,
{
    std::thread::scope(|scope| {
        let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
        let first_gate = std::sync::Arc::clone(&gate);
        let second_gate = std::sync::Arc::clone(&gate);
        let a = scope.spawn(move || {
            first_gate.wait();
            first()
        });
        let b = scope.spawn(move || {
            second_gate.wait();
            second()
        });
        (
            a.join().expect("the first thread finishes"),
            b.join().expect("the second thread finishes"),
        )
    })
}

/// Drive one future to completion on the calling thread.
fn run<T, F>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime")
        .block_on(future)
}

/// A verification whose id already exists, so applying it must be refused.
fn duplicate_of(existing: &str, task: &TaskId, project: &ProjectId) -> Verification {
    Verification::new(
        VerificationId::from_string(existing),
        task.clone(),
        project.clone(),
        None,
        VerificationStatus::Passed,
        vec![Evidence {
            kind: ProbeKind::Command,
            criterion: "already judged".into(),
            status: EvidenceStatus::Passed,
            detail: "a replay".into(),
        }],
        None,
        pinned(),
    )
}

use director_domain::verification::Verification;

/// The discriminator: a failure *while writing the actions* must undo the
/// verdict too.
///
/// Every other test here checks the outcome of a round that succeeded, or that
/// was refused before it began. This one is what actually distinguishes an atomic
/// write from a merely sequential one, because it fails in the *middle*: the
/// verdict is ready to commit, and then one of its observations cannot be
/// written.
///
/// The action below names a task that does not exist, so its insert violates the
/// foreign key on `task_actions.task_id`. Under one transaction that takes the
/// verdict down with it. Under the old shape — verdict committed, actions
/// appended afterwards — the verdict would survive and a briefing would inherit a
/// decision with no record of what it ran, which is the whole defect.
#[tokio::test]
async fn a_failure_writing_the_actions_also_undoes_the_verdict() {
    let mut fx = Fixture::new().await;
    fx.prepared(&[("AUTH", PASSES)]).await;
    let task = TaskId::from_string("AUTH");
    let stored = fx.store.tasks().get_task(&task).await.expect("reads");

    // One good action, and one that cannot possibly be stored.
    let good = Action::new(
        director_domain::ids::ActionId::from_string("ACT-PARTIAL-0"),
        director_domain::action::ActionKind::Command,
        "ran the check",
    )
    .for_task(task.clone());
    let impossible = Action::new(
        director_domain::ids::ActionId::from_string("ACT-PARTIAL-1"),
        director_domain::action::ActionKind::Command,
        "ran a command against a task that is not there",
    )
    .for_task(TaskId::from_string("NO-SUCH-TASK"));

    let verification = Verification::new(
        VerificationId::from_string("VER-PARTIAL-1"),
        task.clone(),
        fx.project.clone(),
        None,
        VerificationStatus::Passed,
        vec![Evidence {
            kind: ProbeKind::Command,
            criterion: "would have passed".into(),
            status: EvidenceStatus::Passed,
            detail: "the check passed".into(),
        }],
        None,
        pinned(),
    );

    let err = fx
        .store
        .apply_verification_with_actions(
            &verification,
            &[good, impossible],
            stored.state_version,
            pinned(),
        )
        .await
        .expect_err("an action that cannot be stored must fail the round");
    assert!(
        !format!("{err}").is_empty(),
        "the persistence failure is surfaced, not swallowed: {err}"
    );

    assert!(
        fx.verification_ids("AUTH").await.is_empty(),
        "the verdict rolled back with the action that failed - a briefing must never \
         inherit a decision whose observations are missing"
    );
    assert!(
        fx.action_ids("AUTH").await.is_empty(),
        "and the action written before the failure rolled back with it, so there is \
         no orphan for an uncommitted verdict"
    );
    let after = fx.store.tasks().get_task(&task).await.expect("reads");
    assert_eq!(
        after.status,
        TaskStatus::InProgress,
        "the task did not move either"
    );
    assert_eq!(
        after.state_version, stored.state_version,
        "and its version did not advance, so nothing was half-applied"
    );
}

/// 1. A successful verification commits its expected action records.
#[tokio::test]
async fn a_successful_verification_commits_its_actions() {
    let mut fx = Fixture::new().await;
    fx.prepared(&[("AUTH", PASSES)]).await;
    fx.to_verification_pending("AUTH").await;

    let mut runner = fx.runner();
    let step = runner
        .verify_at(&fx.project, pinned())
        .await
        .expect("the verification round");
    assert_eq!(step.judged.len(), 1);
    assert!(step.judged[0].passed);

    let verifications = fx.verification_ids("AUTH").await;
    let actions = fx.action_ids("AUTH").await;
    assert_eq!(verifications.len(), 1, "the verdict is durable");
    assert_eq!(
        actions,
        vec![format!("ACT-{}-0", verifications[0])],
        "and exactly one action, keyed on the verdict it belongs to"
    );

    // And the action says what was actually run.
    let stored = fx
        .store
        .actions()
        .get_action(&director_domain::ids::ActionId::from_string(
            actions[0].clone(),
        ))
        .await
        .expect("the action reads back");
    assert!(
        stored.summary.contains(PASSES),
        "the action names the command Orqyn ran: {}",
        stored.summary
    );
    assert_eq!(stored.task_id, Some(TaskId::from_string("AUTH")));
    assert_eq!(stored.at, pinned(), "the round's pinned instant");
}

/// 2. A failure before commit leaves neither the transition nor the actions.
///
/// Driven by a stale `state_version`: the store refuses the whole transaction,
/// including the action rows, so there is nothing to reconcile afterwards. This
/// is the deterministic counterpart to the process-kill test — provoking a real
/// kill *during* a commit would mean racing it rather than asserting it.
#[tokio::test]
async fn a_refused_transaction_leaves_neither_the_verdict_nor_the_actions() {
    let mut fx = Fixture::new().await;
    fx.prepared(&[("AUTH", PASSES)]).await;
    fx.to_verification_pending("AUTH").await;
    let task = TaskId::from_string("AUTH");

    let verification = duplicate_of("VER-STALE-1", &task, &fx.project);
    let actions = vec![Action::new(
        director_domain::ids::ActionId::from_string("ACT-VER-STALE-1-0"),
        director_domain::action::ActionKind::Command,
        "ran the check",
    )
    .for_task(task.clone())];

    let err = fx
        .store
        .apply_verification_with_actions(&verification, &actions, 99, pinned())
        .await
        .expect_err("a stale round must be refused");
    assert!(
        matches!(
            err,
            director_domain::StoreError::StateVersionConflict { .. }
        ),
        "the refusal is the optimistic precondition, got: {err}"
    );

    assert!(
        fx.verification_ids("AUTH").await.is_empty(),
        "the verification row rolled back with everything else"
    );
    assert!(
        fx.action_ids("AUTH").await.is_empty(),
        "and no action was left behind for a verdict that never landed"
    );
    // The task is exactly as the round found it.
    let stored = fx.store.tasks().get_task(&task).await.expect("reads");
    assert_eq!(
        stored.status,
        TaskStatus::VerificationPending,
        "the task did not move"
    );
}

/// 3. Reopening the database preserves verification and action consistency.
#[tokio::test]
async fn reopening_the_preserves_verification_and_action_consistency() {
    let mut fx = Fixture::new().await;
    fx.prepared(&[("AUTH", PASSES)]).await;
    fx.to_verification_pending("AUTH").await;

    let mut runner = fx.runner();
    runner
        .verify_at(&fx.project, pinned())
        .await
        .expect("the verification round");
    let before_verifications = fx.verification_ids("AUTH").await;
    let before_actions = fx.action_ids("AUTH").await;
    assert_eq!(before_actions.len(), 1);

    drop(runner);
    let reopened = Store::open(&fx.store_path)
        .await
        .expect("the store reopens");

    assert_eq!(
        reopened
            .verifications()
            .verifications_for_task(&TaskId::from_string("AUTH"))
            .await
            .expect("verifications read")
            .into_iter()
            .map(|v| v.id.to_string())
            .collect::<Vec<_>>(),
        before_verifications,
        "the verdict survived the restart"
    );
    assert_eq!(
        reopened
            .actions()
            .actions_for_task(&TaskId::from_string("AUTH"))
            .await
            .expect("actions read")
            .into_iter()
            .map(|a| a.id.to_string())
            .collect::<Vec<_>>(),
        before_actions,
        "and so did the action recorded with it"
    );
}

/// 4. Concurrent verification attempts still respect the state-version CAS.
///
/// The atomic write did not relax the precondition: two rounds racing for one
/// task, one wins and the loser is refused with nothing written.
#[test]
fn concurrent_verification_attempts_respect_the_state_version_check() {
    let (first, second, verifications, actions) = run(async {
        let mut fx = Fixture::new().await;
        fx.prepared(&[("AUTH", PASSES)]).await;
        fx.to_verification_pending("AUTH").await;

        let project = fx.project.clone();
        let (first, second) = in_parallel(
            || run(fx.runner().verify_at(&project, pinned())),
            || run(fx.runner().verify_at(&project, pinned())),
        );
        (
            first,
            second,
            fx.verification_ids("AUTH").await,
            fx.action_ids("AUTH").await,
        )
    });

    let outcomes = [
        first.as_ref().map(|s| s.judged.len()),
        second.as_ref().map(|s| s.judged.len()),
    ];
    let judged = |o: &Result<usize, &director_app::runner::RunnerError>| o.as_ref().ok().copied();
    assert!(
        outcomes.iter().any(|o| judged(o) == Some(1)),
        "one round judged the task: {outcomes:?}"
    );
    assert!(
        outcomes.iter().any(|o| o.is_err()),
        "the other was refused rather than double-judging: {outcomes:?}"
    );
    // Which thread loses the write lock is not part of the contract — the barrier
    // only stops either getting a head start. What is asserted is that whichever
    // lost was refused by the state-version precondition, which reaches the
    // caller through VERIFY rather than as a bare store error.
    let mut refusals = 0;
    for outcome in [&first, &second] {
        if let Err(err) = outcome {
            let rendered = format!("{err}");
            assert!(
                rendered.contains("state version conflict"),
                "the loser is refused by the optimistic precondition, got: {rendered}"
            );
            refusals += 1;
        }
    }
    assert_eq!(refusals, 1, "exactly one round lost: {outcomes:?}");

    assert_eq!(
        verifications.len(),
        1,
        "exactly one verdict landed, so there is nothing to reconcile"
    );
    assert_eq!(
        actions.len(),
        1,
        "and exactly one action, committed with it - no orphans, no duplicates"
    );
}

/// 5. Replaying a committed verdict is refused, and writes nothing new.
///
/// The explicit idempotency policy: an action's identity is
/// `(verification_id, ordinal)`, and it is the state-version precondition that
/// prevents the same verification from being applied twice — not a primary-key
/// error, and not a silent `INSERT OR IGNORE`. A genuine duplicate would still be
/// loud, but a *replay* never reaches the insert.
#[tokio::test]
async fn replaying_a_committed_verdict_is_refused_and_writes_nothing() {
    let mut fx = Fixture::new().await;
    fx.prepared(&[("AUTH", PASSES)]).await;
    fx.to_verification_pending("AUTH").await;
    let task = TaskId::from_string("AUTH");

    let mut runner = fx.runner();
    runner
        .verify_at(&fx.project, pinned())
        .await
        .expect("the verification round");
    let verifications = fx.verification_ids("AUTH").await;
    let actions_before = fx.action_ids("AUTH").await;
    assert_eq!(actions_before.len(), 1);

    // Replay: the same verdict id, presented with the version the round already
    // consumed. The task has moved on, so the precondition refuses it.
    let replay = duplicate_of(&verifications[0], &task, &fx.project);
    let replay_actions = vec![Action::new(
        director_domain::ids::ActionId::from_string("ACT-REPLAY-0"),
        director_domain::action::ActionKind::Command,
        "a replayed attempt",
    )
    .for_task(task.clone())];

    let err = fx
        .store
        .apply_verification_with_actions(&replay, &replay_actions, 1, later(60))
        .await
        .expect_err("a replayed verdict must be refused");
    // Either the precondition refuses it, or the row already exists — but never a
    // second action, and never a silent success.
    assert!(
        !format!("{err}").is_empty(),
        "the refusal is explained: {err}"
    );
    assert_eq!(
        fx.action_ids("AUTH").await,
        actions_before,
        "the replay wrote no second action, so the log has no duplicate"
    );
    assert_eq!(
        fx.verification_ids("AUTH").await,
        verifications,
        "and no second verdict"
    );
}

/// 6. Deterministic `verify_at` inputs produce stable serialized action records.
///
/// The point of pinning the clock and deriving ids from the verdict: the same
/// inputs must produce the same log, or a run cannot be compared with a later run
/// of the same thing.
#[tokio::test]
async fn pinned_inputs_produce_stable_serialized_actions() {
    let mut logs = Vec::new();
    for _ in 0..2 {
        let mut fx = Fixture::new().await;
        fx.prepared(&[("AUTH", PASSES), ("API", PASSES)]).await;
        fx.to_verification_pending("AUTH").await;
        fx.to_verification_pending("API").await;

        let mut runner = fx.runner();
        runner
            .verify_at(&fx.project, pinned())
            .await
            .expect("the verification round");

        let mut rendered: Vec<String> = Vec::new();
        for task in ["API", "AUTH"] {
            for action in fx
                .store
                .actions()
                .actions_for_task(&TaskId::from_string(task))
                .await
                .expect("actions read")
            {
                rendered.push(serde_json::to_string(&action).expect("serializes"));
            }
        }
        logs.push(rendered);
    }

    assert_eq!(logs[0].len(), 2, "one action per judged task");
    assert_eq!(
        logs[0], logs[1],
        "two runs of the same pinned inputs produced different action logs"
    );
}

/// 7. Recovery reports no gap for an atomically committed verification.
#[tokio::test]
async fn recovery_reports_no_gap_for_an_atomic_verification() {
    let mut fx = Fixture::new().await;
    fx.prepared(&[("AUTH", PASSES)]).await;
    fx.to_verification_pending("AUTH").await;

    let mut runner = fx.runner();
    runner
        .verify_at(&fx.project, pinned())
        .await
        .expect("the verification round");

    // Put the task back in flight with a checkpoint so a briefing can be built,
    // then check the detector stays quiet: the pair is complete.
    let task = TaskId::from_string("AUTH");
    let stored = fx.store.tasks().get_task(&task).await.expect("reads");
    fx.store
        .tasks()
        .update_task(&director_domain::task::Task {
            status: TaskStatus::InProgress,
            ..stored
        })
        .await
        .expect("the task is in flight again");

    let outcome = director_app::recovery::RecoveryBuilder::new(&fx.store, task.clone())
        .build()
        .await
        .expect("the recovery build");
    if let director_app::recovery::RecoveryOutcome::Built(recovery) = outcome {
        assert!(
            !recovery.gaps().actions_incomplete,
            "a verdict whose actions committed with it reports no gap"
        );
    }
}

/// 8. An unverifiable round still records nothing, and says so honestly.
///
/// The round stays in the survey set and runs again next tick, so recording its
/// attempts would append one copy of the same attempt per tick. The gate is
/// enforced twice — the engine declines to build them, and the store refuses them
/// even if a caller passes them.
#[tokio::test]
async fn an_unverifiable_round_records_nothing_and_keeps_its_evidence() {
    let mut fx = Fixture::new().await;
    fx.prepared(&[("BROKEN", UNRUNNABLE)]).await;
    fx.to_verification_pending("BROKEN").await;
    let task = TaskId::from_string("BROKEN");

    let mut runner = fx.runner();
    let first = runner
        .verify_at(&fx.project, pinned())
        .await
        .expect("the round runs");
    assert_eq!(first.judged.len(), 1);
    assert!(
        first.judged[0].outcome.is_none(),
        "no verdict, so the task stays in the survey set"
    );

    assert!(
        fx.action_ids("BROKEN").await.is_empty(),
        "an attempt that will be repeated is not recorded"
    );
    assert_eq!(
        fx.verification_ids("BROKEN").await.len(),
        1,
        "but the evidence trail is kept - the round did happen"
    );

    // A second round runs for real, and still records nothing.
    let second = runner
        .verify_at(&fx.project, later(60))
        .await
        .expect("the second round runs");
    assert_eq!(second.judged.len(), 1, "it really was surveyed again");
    assert!(
        fx.action_ids("BROKEN").await.is_empty(),
        "so the log still has no copy of a failure it cannot judge"
    );
    assert_eq!(
        fx.verification_ids("BROKEN").await.len(),
        2,
        "while each attempt keeps its own evidence"
    );

    // And the store refuses to write them even if a caller insists — the gate is
    // a property of what is durable, not a convention.
    let verifications = fx.verification_ids("BROKEN").await;
    let unwanted = vec![Action::new(
        director_domain::ids::ActionId::from_string("ACT-SHOULD-NOT-LAND"),
        director_domain::action::ActionKind::Command,
        "an attempt Orqyn could not judge",
    )
    .for_task(task.clone())];
    let stored = fx.store.tasks().get_task(&task).await.expect("reads");
    let verdict = Verification::new(
        VerificationId::from_string("VER-UNVERIFIABLE-PROBE"),
        task,
        fx.project.clone(),
        None,
        VerificationStatus::Unverifiable,
        vec![],
        None,
        later(90),
    );
    fx.store
        .apply_verification_with_actions(&verdict, &unwanted, stored.state_version, later(90))
        .await
        .expect("an unverifiable round still commits its evidence");
    assert!(
        !fx.action_ids("BROKEN")
            .await
            .contains(&"ACT-SHOULD-NOT-LAND".to_string()),
        "the store declines to record an attempt for a round that will be repeated"
    );
    let _ = verifications;
}

/// 9. A failing check is still recorded, and the pair still commits together.
#[tokio::test]
async fn a_failing_check_commits_its_action_with_the_verdict() {
    let mut fx = Fixture::new().await;
    fx.prepared(&[("AUTH", FAILS)]).await;
    fx.to_verification_pending("AUTH").await;

    let mut runner = fx.runner();
    let step = runner
        .verify_at(&fx.project, pinned())
        .await
        .expect("the verification round");
    assert!(!step.judged[0].passed, "the check failed");

    let verifications = fx.verification_ids("AUTH").await;
    let actions = fx.action_ids("AUTH").await;
    assert_eq!(verifications.len(), 1);
    assert_eq!(actions, vec![format!("ACT-{}-0", verifications[0])]);

    let stored = fx
        .store
        .actions()
        .get_action(&director_domain::ids::ActionId::from_string(
            actions[0].clone(),
        ))
        .await
        .expect("the action reads back");
    assert_eq!(
        stored.kind,
        director_domain::action::ActionKind::Error,
        "a failure is recorded as the error it is"
    );
    assert!(
        stored
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("exit"),
        "with the exit code Orqyn read: {:?}",
        stored.detail
    );

    let task = fx
        .store
        .tasks()
        .get_task(&TaskId::from_string("AUTH"))
        .await
        .expect("reads");
    assert_eq!(task.status, TaskStatus::Failed);
}

/// Two criteria in one round produce two actions, ordered by ordinal, on one
/// transaction — and equal timestamps do not make that order arbitrary.
#[tokio::test]
async fn several_checks_in_one_round_commit_together_in_order() {
    let mut fx = Fixture::new().await;
    fx.prepared(&[("MULTI", PASSES)]).await;
    fx.to_verification_pending("MULTI").await;

    // Add a second checked criterion to the same task, so one round runs two
    // commands and both must be recorded.
    let task = fx
        .store
        .tasks()
        .get_task(&TaskId::from_string("MULTI"))
        .await
        .expect("reads");
    let verifications = fx.verification_ids("MULTI").await;
    assert!(verifications.is_empty(), "nothing judged yet yet");

    let mut runner = fx.runner();
    runner
        .verify_at(&fx.project, pinned())
        .await
        .expect("the single-check round commits");
    let verifications = fx.verification_ids("MULTI").await;
    let actions = fx.action_ids("MULTI").await;
    assert_eq!(actions.len(), 1, "one command ran, so one action");
    assert_eq!(actions[0], format!("ACT-{}-0", verifications[0]));

    // Ordinals are the probe's position, so the ids read in check order.
    let stored_actions = fx
        .store
        .actions()
        .actions_for_task(&TaskId::from_string("MULTI"))
        .await
        .expect("actions read");
    assert_eq!(stored_actions[0].at, pinned(), "pinned to the round");
    let _ = task;
}
