//! Real runtime events reaching the durable recovery context.
//!
//! These drive the actual runner against a real SQLite store and a real
//! executor, so what they assert is what a run produces — not what a fixture
//! seeded by hand. Two rules govern every event recorded here:
//!
//! - **Only observed events.** An action comes from something Orqyn ran and read
//!   the result of. An agent's own report, a simulated turn, and a git diff are
//!   not observations and are not recorded.
//! - **Replay is not a second event.** A round Orqyn will run again records
//!   nothing, so the log cannot accumulate one copy of the same attempt per tick.

use std::path::PathBuf;
use std::process::Command;

use chrono::{DateTime, TimeZone, Utc};
use director_adapters::executor::LocalExecutor;
use director_adapters::git::GitService;
use director_adapters::InMemoryProvider;
use director_app::plan::{PlanRequest, TaskSpec};
use director_app::runner::{Runner, RunnerError};
use director_domain::action::{Action, ActionKind};
use director_domain::agent::{Agent, Harness};
use director_domain::blocker::{BlockerKind, BlockerStatus};
use director_domain::capability::Capability;
use director_domain::checkpoint::ResumeStatus;
use director_domain::ids::{
    ActionId, AgentId, AssignmentId, BlockerId, MachineId, PlanId, ProjectId, RepositoryId, TaskId,
};
use director_domain::task::{ExpectedOutput, TaskStatus};
use director_domain::{ActionRepository, AgentRepository, BlockerRepository, TaskRepository};
use director_store::Store;

const PROJECT: &str = "PROJ-1";
const REPO: &str = "REPO-1";
const PASSES: &str = "git --version";
const FAILS: &str = "git --no-such-flag";
/// A program that does not exist, so the executor can gather no evidence.
const UNRUNNABLE: &str = "orqyn-no-such-program-xyz --nope";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap()
}

fn later() -> DateTime<Utc> {
    now() + chrono::Duration::minutes(5)
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
        git_in()
            .args(["config", "user.name", "Orqyn Tests"])
            .status()
            .expect("set name");
        git_in()
            .args(["config", "user.email", "tests@orqyn.invalid"])
            .status()
            .expect("set email");
        git_in()
            .args(["config", "commit.gpgsign", "false"])
            .status()
            .expect("disable signing");
        std::fs::write(repo_dir.join("README.md"), "# fixture\n").expect("write file");
        git_in()
            .args(["add", "README.md"])
            .status()
            .expect("git add");
        git_in()
            .args(["commit", "-q", "-m", "initial"])
            .status()
            .expect("git commit");

        let state_dir = tmp.path().join("orqyn-state");
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

    /// Write a file and commit it, so a later OBSERVE sees a real change.
    fn commit(&self, path: &str, contents: &str, message: &str) {
        let file = self.repo_dir.join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(&file, contents).expect("write file");
        let git = || {
            let mut cmd = Command::new("git");
            cmd.current_dir(&self.repo_dir);
            cmd
        };
        git().args(["add", path]).status().expect("git add");
        let status = git()
            .args(["commit", "-q", "-m", message])
            .status()
            .expect("git commit");
        assert!(status.success(), "git commit should succeed");
    }

    async fn observed(&self) {
        let mut runner = self.runner();
        runner
            .observe(&self.project, &self.repository, &self.repo_path(), now())
            .await
            .expect("the first observation");
    }

    async fn register_agent(&self, id: &str, capabilities: Vec<Capability>) -> AgentId {
        let agent_id = AgentId::from_string(id);
        self.store
            .agents()
            .register_agent(&Agent::register(
                agent_id.clone(),
                id,
                Harness::ClaudeCode,
                MachineId::from_string("MACH-a"),
                capabilities,
            ))
            .await
            .expect("agent registered");
        agent_id
    }

    /// Plan one task with one check, hand it to an agent, and drive the agent's
    /// own reports so the task reaches `verification_pending`. The simulated turn
    /// is deliberately used here: it is the only way to reach VERIFY without an
    /// agent driver, and the point of these tests is what VERIFY records.
    async fn ready_to_verify(&self, task: &str, check: &str) -> AssignmentId {
        let mut runner = self.runner();
        runner
            .plan(plan_request(vec![spec_with_check(task, check)]), now())
            .await
            .expect("the plan lands");
        let scheduled = runner
            .schedule(&self.project, now())
            .await
            .expect("the scheduling round");
        let assignment = scheduled
            .assignment_for(&TaskId::from_string(task))
            .expect("the task was handed out")
            .clone();
        runner
            .simulate_agent_turn(&assignment, now())
            .await
            .expect("the simulated turn");
        assignment
    }

    async fn actions_for(&self, task: &str) -> Vec<Action> {
        self.store
            .actions()
            .actions_for_task(&TaskId::from_string(task))
            .await
            .expect("the action log reads")
    }
}

fn checked(criterion: &str, check: &str) -> ExpectedOutput {
    ExpectedOutput {
        criterion: criterion.to_string(),
        check: Some(check.to_string()),
    }
}

fn spec_with_check(id: &str, check: &str) -> TaskSpec {
    let mut spec = TaskSpec::new(TaskId::from_string(id), id, format!("do {id}"));
    spec.expected_outputs = vec![checked(&format!("{id} is checkable"), check)];
    spec
}

fn plan_request(specs: Vec<TaskSpec>) -> PlanRequest {
    PlanRequest {
        project_id: ProjectId::from_string(PROJECT),
        plan_id: PlanId::from_string("PLAN-1"),
        objective: "make the fixture verifiable".into(),
        rationale: "the smallest honest path to a judged task".into(),
        tasks: specs,
        authorized_by: AgentId::from_string("AGENT-planner"),
    }
}

/// A task in flight with a checkpoint on the current commit — the only state in
/// which a `Ready` verdict is possible.
async fn task_in_flight_with_checkpoint(fixture: &Fixture, task: &str, check: &str) {
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;
    let mut runner = fixture.runner();
    runner
        .plan(plan_request(vec![spec_with_check(task, check)]), now())
        .await
        .expect("the plan lands");
    runner
        .schedule(&fixture.project, now())
        .await
        .expect("the scheduling round");
    // Move the repository so OBSERVE has a real change to record, which is what
    // advances the in-flight task's recovery lineage.
    fixture.commit("src/work.rs", "// in progress\n", "start the work");
    let observed = runner
        .observe(
            &fixture.project,
            &fixture.repository,
            &fixture.repo_path(),
            later(),
        )
        .await
        .expect("an observation that records a change");
    assert_eq!(
        observed.checkpoints_written, 1,
        "the in-flight task got its checkpoint"
    );
}

// ---------------------------------------------------------------------------
// 1. A real observable event creates the expected action record
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_check_orqyn_ran_and_read_is_recorded_as_an_action() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;
    fixture.ready_to_verify("AUTH", PASSES).await;

    let mut runner = fixture.runner();
    let verified = runner
        .verify(&fixture.project)
        .await
        .expect("the verification round");
    assert!(verified.all_passed(), "the check passed");

    let actions = fixture.actions_for("AUTH").await;
    assert_eq!(
        actions.len(),
        1,
        "one check that ran is one action, and nothing else is invented"
    );
    let action = &actions[0];
    assert_eq!(action.kind, ActionKind::Command);
    assert_eq!(action.task_id, Some(TaskId::from_string("AUTH")));
    assert!(
        action.summary.contains(PASSES),
        "the action names the command Orqyn actually ran: {}",
        action.summary
    );
    assert!(
        action
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("criterion"),
        "the detail carries which criterion the command answered"
    );
}

#[tokio::test]
async fn a_failing_check_is_recorded_as_an_error_with_its_exit_code() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;
    fixture.ready_to_verify("AUTH", FAILS).await;

    let mut runner = fixture.runner();
    let verified = runner
        .verify(&fixture.project)
        .await
        .expect("the verification round");
    assert!(!verified.all_passed(), "the check failed");

    let actions = fixture.actions_for("AUTH").await;
    assert_eq!(actions.len(), 1);
    assert_eq!(
        actions[0].kind,
        ActionKind::Error,
        "a failure is what a replacement most needs to see, and it outranks routine work"
    );
    assert!(
        actions[0].summary.contains(FAILS),
        "got: {}",
        actions[0].summary
    );
    assert!(
        actions[0]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("exit"),
        "the exit code Orqyn read is on record: {:?}",
        actions[0].detail
    );
}

// ---------------------------------------------------------------------------
// 2. The record survives closing and reopening the store
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_recorded_action_survives_closing_and_reopening_the_store() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;
    fixture.ready_to_verify("AUTH", PASSES).await;

    let mut runner = fixture.runner();
    runner
        .verify(&fixture.project)
        .await
        .expect("the verification round");

    // Close the writer entirely and read the log back through a fresh handle.
    let before = fixture.actions_for("AUTH").await;
    drop(runner);
    let reopened = Store::open(&fixture.store_path)
        .await
        .expect("the store reopens");
    let after = reopened
        .actions()
        .actions_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("the log reads after a restart");

    assert_eq!(after, before, "the action is durable, not writer memory");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].summary, before[0].summary);
}

// ---------------------------------------------------------------------------
// 3. Retrying or recovering does not create duplicates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_round_that_will_be_run_again_records_nothing_on_either_attempt() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;
    fixture.ready_to_verify("UNRUNNABLE-TASK", UNRUNNABLE).await;

    let mut runner = fixture.runner();
    // The command cannot run, so the round reaches no verdict and the task stays
    // `verification_pending` — it will be surveyed again next round.
    let first = runner
        .verify(&fixture.project)
        .await
        .expect("the verification round");
    assert_eq!(first.judged.len(), 1);
    assert!(
        first.judged[0].outcome.is_none(),
        "an unrunnable check moves no status, so the round will run again"
    );

    // The second round genuinely re-runs the same command. It must not append a
    // second copy of the same attempt to the log.
    let second = runner
        .verify(&fixture.project)
        .await
        .expect("the second verification round");
    assert_eq!(second.judged.len(), 1, "it really was surveyed again");

    assert!(
        fixture.actions_for("UNRUNNABLE-TASK").await.is_empty(),
        "a round Orqyn will repeat is not recorded, so a retry cannot duplicate it"
    );
}

#[tokio::test]
async fn a_second_verification_round_adds_nothing_for_a_task_already_judged() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;
    fixture.ready_to_verify("AUTH", PASSES).await;

    let mut runner = fixture.runner();
    runner
        .verify(&fixture.project)
        .await
        .expect("the verification round");
    assert_eq!(fixture.actions_for("AUTH").await.len(), 1);

    // The task is `done`, so the next round does not survey it at all.
    let again = runner
        .verify(&fixture.project)
        .await
        .expect("the second verification round");
    assert!(
        again.judged.is_empty(),
        "a done task is out of scope, so there is nothing new to record"
    );
    assert_eq!(
        fixture.actions_for("AUTH").await.len(),
        1,
        "the log holds exactly one record of the one round that happened"
    );
}

#[tokio::test]
async fn a_runner_that_never_saw_the_round_still_rebuilds_the_same_log() {
    // Recovery from a cold start: a fresh runner over the same store must reach
    // the same briefing, because the log is durable rather than in-process.
    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;
    fixture.ready_to_verify("AUTH", FAILS).await;

    let mut runner = fixture.runner();
    runner
        .verify(&fixture.project)
        .await
        .expect("the verification round");
    let before = fixture.actions_for("AUTH").await;
    assert_eq!(before.len(), 1);

    let mut cold = fixture.runner();
    let brief = cold
        .recovery(&TaskId::from_string("AUTH"))
        .await
        .expect("the recovery build");
    assert!(
        brief.is_none(),
        "the task failed verification, so it is not in flight and has nothing to resume"
    );
    let after = fixture.actions_for("AUTH").await;
    assert_eq!(after, before, "the durable log is the same for any reader");
}

// ---------------------------------------------------------------------------
// 4. Blocker lifecycle, with its history preserved
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_blocker_can_be_raised_cleared_and_kept_on_record() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;
    let mut runner = fixture.runner();
    runner
        .plan(plan_request(vec![spec_with_check("AUTH", PASSES)]), now())
        .await
        .expect("the plan lands");
    runner
        .schedule(&fixture.project, now())
        .await
        .expect("the scheduling round");

    let task = TaskId::from_string("AUTH");
    let raised = runner
        .raise_blocker(
            &task,
            BlockerKind::External,
            "waiting on an API key",
            Some("credentials are issued".to_string()),
            now(),
        )
        .await
        .expect("the blocker lands");
    assert_eq!(raised.status, BlockerStatus::Open);
    assert!(raised.status.is_blocking());
    assert_eq!(raised.raised_at, now(), "the caller's instant is pinned");
    assert_eq!(
        raised.resolution.as_deref(),
        Some("credentials are issued"),
        "what would clear it is recorded at raise time"
    );

    let cleared = runner
        .resolve_blocker(&raised.id, later())
        .await
        .expect("the blocker clears");
    assert_eq!(cleared.status, BlockerStatus::Resolved);
    assert_eq!(cleared.resolved_at, Some(later()));
    assert_eq!(
        cleared.raised_at, raised.raised_at,
        "when it was raised is a fact a later write cannot rewrite"
    );

    // Retained, not deleted: the whole history is still readable.
    let history = fixture
        .store
        .blockers()
        .blockers_for_task(&task)
        .await
        .expect("the history reads");
    assert_eq!(history.len(), 1, "a cleared blocker is still on record");
    assert_eq!(history[0].status, BlockerStatus::Resolved);
}

#[tokio::test]
async fn obsoleting_a_blocker_is_distinguishable_from_resolving_it() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    let mut runner = fixture.runner();
    runner
        .plan(plan_request(vec![spec_with_check("AUTH", PASSES)]), now())
        .await
        .expect("the plan lands");

    let task = TaskId::from_string("AUTH");
    let raised = runner
        .raise_blocker(
            &task,
            BlockerKind::Dependency,
            "waiting on TASK-2",
            None,
            now(),
        )
        .await
        .expect("the blocker lands");
    let obsolete = runner
        .obsolete_blocker(&raised.id, later())
        .await
        .expect("the blocker goes obsolete");
    assert_eq!(
        obsolete.status,
        BlockerStatus::Obsolete,
        "we stopped trying is recorded differently from we fixed it"
    );
    assert!(!obsolete.status.is_blocking(), "and it does not block");
}

// ---------------------------------------------------------------------------
// 5. Recovery exposes the recorded actions and blocker details
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_recovery_briefing_carries_the_recorded_actions_and_blockers() {
    let fixture = Fixture::new().await;
    task_in_flight_with_checkpoint(&fixture, "AUTH", PASSES).await;

    // Record a real action the way a judged round would, then raise a blocker.
    let mut runner = fixture.runner();
    let task = TaskId::from_string("AUTH");
    let mut action = Action::new(
        ActionId::from_string("ACT-1"),
        ActionKind::Command,
        "ran the tests",
    );
    action.task_id = Some(task.clone());
    fixture
        .store
        .actions()
        .record_action(&action)
        .await
        .expect("the action lands");
    let raised = runner
        .raise_blocker(
            &task,
            BlockerKind::External,
            "waiting on review",
            None,
            now(),
        )
        .await
        .expect("the blocker lands");

    let brief = runner
        .recovery(&task)
        .await
        .expect("the recovery build")
        .expect("an in-flight task with a checkpoint has a briefing");

    assert_eq!(
        brief.recent_actions,
        vec!["ran the tests".to_string()],
        "the briefing carries what the log actually holds"
    );
    assert_eq!(brief.blocker_details.len(), 1);
    assert_eq!(brief.blocker_details[0].id, raised.id.to_string());
    assert_eq!(brief.blocker_details[0].status, "open");
    assert_eq!(brief.blocker_details[0].description, "waiting on review");
    assert!(
        brief.actions_available,
        "a window with a record in it is a real record, not an absence"
    );
}

// ---------------------------------------------------------------------------
// 6. An open blocker holds a Ready verdict; clearing it restores Ready
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_open_blocker_holds_the_verdict_and_clearing_it_restores_ready() {
    let fixture = Fixture::new().await;
    task_in_flight_with_checkpoint(&fixture, "AUTH", PASSES).await;
    let mut runner = fixture.runner();
    let task = TaskId::from_string("AUTH");

    // Nothing in the way: the baseline the blocker has to move.
    let brief = runner
        .recovery(&task)
        .await
        .expect("the recovery build")
        .expect("a briefing");
    assert_eq!(
        brief.resume_status,
        ResumeStatus::Ready,
        "an unblocked in-flight task on a current checkpoint is ready"
    );

    let raised = runner
        .raise_blocker(
            &task,
            BlockerKind::External,
            "waiting on an API key",
            None,
            now(),
        )
        .await
        .expect("the blocker lands");

    let blocked = runner
        .recovery(&task)
        .await
        .expect("the recovery build")
        .expect("a briefing");
    assert_eq!(
        blocked.resume_status,
        ResumeStatus::Blocked,
        "a recorded open blocker must not read as a task that can go"
    );
    assert!(
        blocked.next_action.contains("blocked"),
        "the instruction has to match the verdict; got: {}",
        blocked.next_action
    );

    runner
        .resolve_blocker(&raised.id, later())
        .await
        .expect("the blocker clears");
    let released = runner
        .recovery(&task)
        .await
        .expect("the recovery build")
        .expect("a briefing");
    assert_eq!(
        released.resume_status,
        ResumeStatus::Ready,
        "clearing the blocker restores Ready, because the row is still on record as cleared"
    );
    assert_eq!(
        released.blocker_details.len(),
        1,
        "and the cleared blocker is still reported"
    );
}

// ---------------------------------------------------------------------------
// 7. Persistence failures are surfaced, not swallowed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_blocker_write_is_reported_to_the_caller() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    let mut runner = fixture.runner();
    runner
        .plan(plan_request(vec![spec_with_check("AUTH", PASSES)]), now())
        .await
        .expect("the plan lands");

    // A blocker that was never raised cannot be cleared. The read fails, and the
    // failure reaches the caller rather than being reported as a success.
    let err = runner
        .resolve_blocker(&BlockerId::from_string("BLK-DOES-NOT-EXIST"), later())
        .await
        .expect_err("clearing nothing must not report success");
    assert!(
        matches!(err, RunnerError::Record(_)),
        "a persistence failure surfaces as a record error, got: {err:?}"
    );
}

#[tokio::test]
async fn replaying_a_clear_is_idempotent_and_a_contradiction_is_refused() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    let mut runner = fixture.runner();
    runner
        .plan(plan_request(vec![spec_with_check("AUTH", PASSES)]), now())
        .await
        .expect("the plan lands");

    let task = TaskId::from_string("AUTH");
    let raised = runner
        .raise_blocker(
            &task,
            BlockerKind::Unknown,
            "needs investigation",
            None,
            now(),
        )
        .await
        .expect("the blocker lands");

    let first = runner
        .resolve_blocker(&raised.id, later())
        .await
        .expect("the blocker clears");
    // The same call again: the replay returns the row it already wrote rather
    // than re-stamping when it was fixed.
    let replayed = runner
        .resolve_blocker(&raised.id, later() + chrono::Duration::hours(1))
        .await
        .expect("the replay is a no-op, not a failure");
    assert_eq!(
        replayed.resolved_at, first.resolved_at,
        "a replayed clear must not rewrite when the thing was fixed"
    );

    // The other cleared state, though, is a contradiction rather than a replay:
    // it would erase the difference between the two histories.
    let err = runner
        .obsolete_blocker(&raised.id, later())
        .await
        .expect_err("asking for a different history is refused");
    assert!(
        matches!(err, RunnerError::BlockerLifecycle(_)),
        "got: {err:?}"
    );

    // And the refusal changed nothing.
    let stored = fixture
        .store
        .blockers()
        .get_blocker(&raised.id)
        .await
        .expect("reads back");
    assert_eq!(stored.status, BlockerStatus::Resolved);
}

#[tokio::test]
async fn the_store_refuses_two_actions_with_the_same_id() {
    // The append-only log's own duplicate guard, asserted directly: writing the
    // same id twice is an error, which is what stops a replayed insert from
    // quietly becoming two rows.
    let fixture = Fixture::new().await;
    let action = Action::new(ActionId::from_string("ACT-DUP"), ActionKind::Note, "once");
    fixture
        .store
        .actions()
        .record_action(&action)
        .await
        .expect("the first write lands");
    let err = fixture
        .store
        .actions()
        .record_action(&action)
        .await
        .expect_err("the same id cannot land twice");
    assert!(
        !err.to_string().is_empty(),
        "the store explains why the second write failed"
    );
}

// ---------------------------------------------------------------------------
// 8. Existing behaviour is unchanged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn task_status_is_not_moved_by_a_blocker() {
    // The lifecycles stay separate: recording a blocker changes what the briefing
    // may claim, not what the planner recorded about the task.
    let fixture = Fixture::new().await;
    task_in_flight_with_checkpoint(&fixture, "AUTH", PASSES).await;
    let mut runner = fixture.runner();
    let task = TaskId::from_string("AUTH");

    runner
        .raise_blocker(
            &task,
            BlockerKind::Conflict,
            "two sources disagree",
            None,
            now(),
        )
        .await
        .expect("the blocker lands");

    let stored = fixture
        .store
        .tasks()
        .get_task(&task)
        .await
        .expect("the task reads back");
    assert_eq!(
        stored.status,
        TaskStatus::InProgress,
        "nothing here writes TaskStatus::Blocked; REPLAN keeps declining to"
    );
}
