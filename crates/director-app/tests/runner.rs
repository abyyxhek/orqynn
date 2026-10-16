//! End-to-end tests for the integration runner.
//!
//! These drive [`Runner`] through the whole loop against a real git repository,
//! a real SQLite store on disk, and a real [`LocalExecutor`] — the same shape as
//! `end_to_end.rs`, with one difference in emphasis. That file proves the steps
//! work in the order production uses them; this one proves the *driver* is
//! honest, which is a different question:
//!
//! - a task reaches `done` only through a verification record that exists in the
//!   store afterwards, read back rather than echoed;
//! - a simulated agent reporting success is not, and cannot be, a completion;
//! - a failed verification leads to the retry path the store actually supports;
//! - everything survives closing and reopening the database;
//! - Phase 13's scope conflict is refused through the runner, not around it;
//! - a failing provider surfaces instead of producing an empty, healthy-looking
//!   tick.
//!
//! **Nothing here is real agent execution.** Every test drives progress through
//! [`Runner::simulate_agent_turn`], which records the two reports an agent would
//! make about itself and runs nothing. The checks VERIFY executes are real
//! commands — `git --version` passes, `git --no-such-flag` fails — but they
//! answer whether the *repository* satisfies the task, not whether any model did
//! anything.
//!
//! No test pokes a task status by hand. Every status a test asserts on is one
//! the runner's own steps produced.

use std::path::PathBuf;
use std::process::Command;

use chrono::{DateTime, TimeZone, Utc};
use director_adapters::executor::LocalExecutor;
use director_adapters::git::GitService;
use director_adapters::InMemoryProvider;
use director_app::plan::{PlanRequest, TaskSpec};
use director_app::replan::{Remediation, ReplanRequest, TaskDecision};
use director_app::runner::{CheckOutcome, Runner, RunnerError};
use director_domain::agent::{Agent, Harness};
use director_domain::capability::Capability;
use director_domain::ids::{AgentId, MachineId, PlanId, ProjectId, RepositoryId, TaskId};
use director_domain::task::{ExpectedOutput, TaskStatus};
use director_domain::verification::VerificationStatus;
use director_domain::{
    AgentRepository, AssignmentRepository, ProjectRepository, TaskRepository,
    VerificationRepository,
};
use director_store::Store;

const PROJECT: &str = "PROJ-1";
const REPO: &str = "REPO-1";

/// A check that passes on any machine with git installed.
const PASSES: &str = "git --version";
/// A check that fails deterministically: git rejects the flag.
const FAILS: &str = "git --no-such-flag";

/// The pinned instant every step is run at, so a run is reproducible.
fn now() -> DateTime<Utc> {
    Utc.timestamp_opt(1_760_000_000, 0)
        .single()
        .expect("a fixed instant")
}

/// A later instant, for steps that must come after something else.
fn later() -> DateTime<Utc> {
    now() + chrono::Duration::seconds(30)
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A temporary directory holding a real git repository with one commit, a
/// separate directory for the git service's own state, and a SQLite store at a
/// remembered path so a test can close and reopen it.
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
        // A standalone commit needs an identity; the test environment has none.
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

    /// Write a file and commit it, so a later OBSERVE sees a real change rather
    /// than a restatement of what it already knew.
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

    /// OBSERVE, so the project exists and PLAN has something to plan against.
    async fn observed(&self) {
        let mut runner = self.runner();
        runner
            .observe(&self.project, &self.repository, &self.repo_path(), now())
            .await
            .expect("the first observation");
    }

    /// Register an agent with the given capabilities.
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

    /// Read a task back out of the store, rather than trusting an in-memory value.
    async fn task(&self, id: &str) -> director_domain::task::Task {
        self.store
            .tasks()
            .get_task(&TaskId::from_string(id))
            .await
            .expect("the task exists")
    }
}

// ---------------------------------------------------------------------------
// Request builders
// ---------------------------------------------------------------------------

/// One expected output with a machine check.
fn checked(criterion: &str, check: &str) -> ExpectedOutput {
    ExpectedOutput {
        criterion: criterion.to_string(),
        check: Some(check.to_string()),
    }
}

/// A task spec with a single criterion and the given check.
fn spec_with_check(id: &str, check: &str) -> TaskSpec {
    let mut spec = TaskSpec::new(TaskId::from_string(id), id, format!("do {id}"));
    spec.expected_outputs = vec![checked(&format!("{id} is checkable"), check)];
    spec
}

/// A task spec with a single criterion, a check, and a declared scope.
fn scoped_spec(id: &str, check: &str, scope: &str) -> TaskSpec {
    spec_with_check(id, check).scopes([scope])
}

fn plan_request(specs: Vec<TaskSpec>) -> PlanRequest {
    PlanRequest {
        project_id: ProjectId::from_string(PROJECT),
        plan_id: PlanId::from_string("PLAN-1"),
        objective: "make the fixture verifiable".into(),
        rationale: "the runner's smallest honest path to done".into(),
        tasks: specs,
        authorized_by: AgentId::from_string("AGENT-planner"),
    }
}

// ---------------------------------------------------------------------------
// 1. The full workflow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_task_runs_the_whole_workflow_and_lands_a_persisted_verification() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;

    let mut runner = fixture.runner();

    // OBSERVE already ran in the fixture; run it again to prove a second tick is
    // legal and reports a change rather than an error.
    let observed = runner
        .observe(
            &fixture.project,
            &fixture.repository,
            &fixture.repo_path(),
            later(),
        )
        .await
        .expect("a repeat observation");
    assert!(
        !observed.first_observation,
        "the fixture already observed, so this round is a change rather than a baseline"
    );
    assert_eq!(observed.repository_id, fixture.repository);

    // PLAN
    let planned = runner
        .plan(plan_request(vec![spec_with_check("AUTH", PASSES)]), now())
        .await
        .expect("the plan lands");
    assert_eq!(planned.tasks.len(), 1);
    assert_eq!(planned.tasks[0].status, TaskStatus::Todo);

    // SCHEDULE — which applies the pairing through ASSIGN.
    let scheduled = runner
        .schedule(&fixture.project, now())
        .await
        .expect("the scheduling round");
    assert_eq!(scheduled.assigned.len(), 1, "one task, one agent");
    assert!(scheduled.refused.is_empty(), "nothing was refused");
    let assignment = scheduled
        .assignment_for(&TaskId::from_string("AUTH"))
        .expect("AUTH was handed out")
        .clone();
    assert_eq!(
        fixture.task("AUTH").await.status,
        TaskStatus::InProgress,
        "SCHEDULE moved the task into flight"
    );

    // MONITOR — the holder begins, then finishes. Simulated.
    let turn = runner
        .simulate_agent_turn(&assignment, now())
        .await
        .expect("the simulated turn");
    assert!(turn.simulated, "the runner labels its own execution");
    assert_eq!(turn.task_status, TaskStatus::VerificationPending);

    // VERIFY — real commands, real exit codes.
    let verified = runner
        .verify(&fixture.project)
        .await
        .expect("the verification round");
    assert!(verified.all_passed(), "the passing check passed");
    assert_eq!(verified.judged.len(), 1);
    assert!(verified.judged[0].passed);
    assert_eq!(verified.judged[0].outcome, Some(TaskStatus::Done));
    assert!(matches!(
        verified.judged[0].checks[0],
        CheckOutcome::Passed { .. }
    ));

    // The verdict is durable: read it out of the store, not out of the report.
    assert_eq!(
        fixture.task("AUTH").await.status,
        TaskStatus::Done,
        "the persisted task is done"
    );
    let verifications = fixture
        .store
        .verifications()
        .verifications_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("the verification history reads");
    assert_eq!(verifications.len(), 1, "exactly one verdict on record");
    assert_eq!(verifications[0].status, VerificationStatus::Passed);
}

// ---------------------------------------------------------------------------
// 2. A report is not a completion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_simulated_agent_report_alone_leaves_the_task_awaiting_verification() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;

    let mut runner = fixture.runner();
    // The task's criterion is one the repository does *not* satisfy. That is
    // what makes this test say something: the simulated agent reports success
    // either way, so if the report carried any weight the task would be done.
    runner
        .plan(plan_request(vec![spec_with_check("AUTH", FAILS)]), now())
        .await
        .expect("the plan lands");
    let scheduled = runner
        .schedule(&fixture.project, now())
        .await
        .expect("the scheduling round");
    let assignment = scheduled
        .assignment_for(&TaskId::from_string("AUTH"))
        .expect("AUTH was handed out")
        .clone();

    // The agent "reports" it finished.
    let turn = runner
        .simulate_agent_turn(&assignment, now())
        .await
        .expect("the simulated turn");
    assert!(turn.simulated, "the runner never claims this was real");
    assert_eq!(turn.task_status, TaskStatus::VerificationPending);
    assert_eq!(
        fixture.task("AUTH").await.status,
        TaskStatus::VerificationPending,
        "the store agrees: a report stops short of done"
    );

    // No verification exists, because nothing has judged the work.
    let verifications = fixture
        .store
        .verifications()
        .verifications_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("the verification history reads");
    assert!(
        verifications.is_empty(),
        "a report alone produced no verdict, so there is nothing that completed it"
    );

    // VERIFY reads the repository, not the agent's claim. The claim was
    // "finished"; the repository says otherwise, and the repository wins.
    let verified = runner.verify(&fixture.project).await.expect("verify");
    assert_eq!(verified.judged.len(), 1, "the pending task is surveyed");
    assert!(
        !verified.judged[0].passed,
        "the agent reported success and the task still did not pass"
    );
    assert_eq!(verified.judged[0].verdict, "failed");
    assert_eq!(
        fixture.task("AUTH").await.status,
        TaskStatus::Failed,
        "an agent report cannot make a task done, whatever it said"
    );
}

// ---------------------------------------------------------------------------
// 3. Failed verification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_verification_leaves_the_task_failed_and_replannable() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;

    let mut runner = fixture.runner();
    runner
        .plan(plan_request(vec![spec_with_check("AUTH", FAILS)]), now())
        .await
        .expect("the plan lands");
    let scheduled = runner
        .schedule(&fixture.project, now())
        .await
        .expect("the scheduling round");
    let assignment = scheduled
        .assignment_for(&TaskId::from_string("AUTH"))
        .expect("AUTH was handed out")
        .clone();

    runner
        .simulate_agent_turn(&assignment, now())
        .await
        .expect("the simulated turn");

    // VERIFY fails, and says why.
    let verified = runner
        .verify(&fixture.project)
        .await
        .expect("the verification round");
    assert!(!verified.all_passed());
    assert_eq!(verified.judged[0].verdict, "failed");
    let CheckOutcome::Failed { exit_code, .. } = &verified.judged[0].checks[0] else {
        panic!(
            "expected a failed check, got {:?}",
            verified.judged[0].checks[0]
        );
    };
    assert_ne!(*exit_code, 0, "git rejected the flag");

    assert_eq!(
        fixture.task("AUTH").await.status,
        TaskStatus::Failed,
        "the store moved it to failed, not done"
    );
    let verifications = fixture
        .store
        .verifications()
        .verifications_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("the verification history reads");
    assert_eq!(verifications[0].status, VerificationStatus::Failed);

    // REPLAN retries it, and the task becomes handable again.
    let replanned = runner
        .replan(
            ReplanRequest {
                project_id: fixture.project.clone(),
                decisions: vec![TaskDecision {
                    task_id: TaskId::from_string("AUTH"),
                    remediation: Remediation::Retry,
                    reason: None,
                }],
                decided_by: None,
            },
            later(),
        )
        .await
        .expect("the replanning round");
    assert_eq!(replanned.retried, vec![TaskId::from_string("AUTH")]);
    assert!(
        replanned.undecided.is_empty(),
        "the caller decided every task the survey found"
    );
    assert_eq!(
        fixture.task("AUTH").await.status,
        TaskStatus::Todo,
        "a retried task is handable again"
    );

    // And it really is handable: a second scheduling round picks it up.
    let rescheduled = runner
        .schedule(&fixture.project, later())
        .await
        .expect("the second scheduling round");
    assert_eq!(rescheduled.assigned.len(), 1);
    let new_assignment = rescheduled
        .assignment_for(&TaskId::from_string("AUTH"))
        .expect("AUTH was handed out again")
        .clone();
    assert_ne!(
        new_assignment, assignment,
        "the retry is a new tenure, not the old one revived"
    );
}

// ---------------------------------------------------------------------------
// 4. Durability across a reopen
// ---------------------------------------------------------------------------

#[tokio::test]
async fn task_and_verification_state_survive_closing_and_reopening_the_store() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;

    let store_path = fixture.store_path.clone();
    {
        let mut runner = fixture.runner();
        runner
            .plan(plan_request(vec![spec_with_check("AUTH", PASSES)]), now())
            .await
            .expect("the plan lands");
        let scheduled = runner
            .schedule(&fixture.project, now())
            .await
            .expect("the scheduling round");
        let assignment = scheduled
            .assignment_for(&TaskId::from_string("AUTH"))
            .expect("AUTH was handed out")
            .clone();
        runner
            .simulate_agent_turn(&assignment, now())
            .await
            .expect("the simulated turn");
        runner
            .verify(&fixture.project)
            .await
            .expect("the verification round");
    }

    // Close the store entirely and open a fresh handle on the same file. This is
    // a different `Store` value with its own pool, reading what the first one
    // committed to disk.
    drop(fixture.store);
    let reopened = Store::open(&store_path).await.expect("the store reopens");

    let task = reopened
        .tasks()
        .get_task(&TaskId::from_string("AUTH"))
        .await
        .expect("the task survived");
    assert_eq!(task.status, TaskStatus::Done);

    let verifications = reopened
        .verifications()
        .verifications_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("the verification history survived");
    assert_eq!(verifications.len(), 1);
    assert_eq!(verifications[0].status, VerificationStatus::Passed);

    // The project the observation created survived too.
    assert!(
        reopened
            .projects()
            .get_project(&fixture.project)
            .await
            .expect("the project survived")
            .id
            == fixture.project
    );

    // And a runner over the reopened store still reads the plan as active, so
    // the recovered database is a working one rather than a readable wreck.
    let git = GitService::new(fixture.repo_dir.join("state")).expect("git service");
    let memory = InMemoryProvider::new();
    let executor = LocalExecutor::default();
    let mut runner = Runner::new(&reopened, &git, &memory, &executor, &fixture.repo_dir);
    let rescheduled = runner
        .schedule(&fixture.project, later())
        .await
        .expect("a scheduling round against the reopened store");
    assert!(
        rescheduled.assigned.is_empty(),
        "the only task is done, so there is nothing to hand out"
    );
    assert!(
        rescheduled.unassigned.is_empty(),
        "a done task is not ready work, so it is not reported as unassigned either"
    );
}

// ---------------------------------------------------------------------------
// 5. Phase 13 scope conflicts, through the runner
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_tasks_with_overlapping_scopes_are_not_both_handed_out() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    // Two agents, so the only thing that can stop a second pairing is the scope
    // check. One agent would be refused for being busy instead, which would not
    // prove anything about Phase 13.
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;
    fixture
        .register_agent("AGENT-2", vec![Capability::Coding])
        .await;

    let mut runner = fixture.runner();
    runner
        .plan(
            plan_request(vec![
                scoped_spec("AUTH", PASSES, "src/auth"),
                scoped_spec("API", PASSES, "src/auth/login.rs"),
            ]),
            now(),
        )
        .await
        .expect("the plan lands");

    let scheduled = runner
        .schedule(&fixture.project, now())
        .await
        .expect("the scheduling round");

    assert_eq!(
        scheduled.assigned.len(),
        1,
        "two agents were free, so only the scope check explains a single pairing"
    );
    assert!(
        scheduled.refused.is_empty(),
        "the conflict is reported as unassigned, not as an assign refusal: {:?}",
        scheduled.refused
    );
    let unassigned = scheduled
        .unassigned
        .iter()
        .find(|entry| entry.task_id == TaskId::from_string("API"))
        .expect("the overlapping task is reported unassigned");
    assert!(
        unassigned.reason.contains("overlaps"),
        "the reason names the overlap, got: {}",
        unassigned.reason
    );
    assert!(
        unassigned.reason.contains("AUTH"),
        "the reason names the holder, got: {}",
        unassigned.reason
    );

    // The store agrees: one task in flight, one still waiting, and no assignment
    // for the loser.
    assert_eq!(fixture.task("AUTH").await.status, TaskStatus::InProgress);
    assert_eq!(fixture.task("API").await.status, TaskStatus::Todo);
    assert!(
        !has_active_assignment(&fixture, "API").await,
        "nothing is holding the task that lost the conflict"
    );
}

/// Whether a task currently has an active assignment, read from the store.
async fn has_active_assignment(fixture: &Fixture, task: &str) -> bool {
    fixture
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string(task))
        .await
        .expect("the assignment lookup works")
        .is_some()
}

// ---------------------------------------------------------------------------
// 6. Errors surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_provider_error_surfaces_to_the_caller_instead_of_a_quiet_empty_tick() {
    use director_domain::providers::{CommandOutcome, CommandSpec, ExecutionProvider, Provider};

    /// An executor that fails every command, standing in for a broken
    /// environment — a machine with no shell, a missing binary, a full disk.
    struct BrokenExecutor;

    #[derive(Debug)]
    struct BrokenExecutorError;

    impl std::fmt::Display for BrokenExecutorError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "this executor cannot run anything")
        }
    }

    impl std::error::Error for BrokenExecutorError {}

    impl Provider for BrokenExecutor {
        type Error = BrokenExecutorError;
    }

    #[async_trait::async_trait]
    impl ExecutionProvider for BrokenExecutor {
        async fn run_command(&self, _spec: &CommandSpec) -> Result<CommandOutcome, Self::Error> {
            Err(BrokenExecutorError)
        }
    }

    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;

    let broken = BrokenExecutor;
    let mut runner = Runner::new(
        &fixture.store,
        &fixture.git,
        &fixture.memory,
        &broken,
        &fixture.repo_dir,
    );

    runner
        .plan(plan_request(vec![spec_with_check("AUTH", PASSES)]), now())
        .await
        .expect("the plan lands");
    let scheduled = runner
        .schedule(&fixture.project, now())
        .await
        .expect("the scheduling round");
    let assignment = scheduled
        .assignment_for(&TaskId::from_string("AUTH"))
        .expect("AUTH was handed out")
        .clone();
    runner
        .simulate_agent_turn(&assignment, now())
        .await
        .expect("the simulated turn");

    // VERIFY cannot run its checks. The step's own contract is that this is not
    // a verdict on the work — it is an environment failure, surfaced as an
    // unverifiable task and not as a failed one. The runner must report that
    // distinction rather than rounding it to "not passed".
    let verified = runner
        .verify(&fixture.project)
        .await
        .expect("a round that cannot run checks is not an error");
    assert!(!verified.all_passed());
    assert_eq!(
        verified.judged[0].verdict,
        "unverifiable: the evidence is missing"
    );
    assert!(matches!(
        verified.judged[0].checks[0],
        CheckOutcome::Unrunnable { .. }
    ));
    assert_eq!(
        verified.judged[0].outcome, None,
        "nothing was written about a task the round could not judge"
    );
    assert_eq!(
        fixture.task("AUTH").await.status,
        TaskStatus::VerificationPending,
        "still awaiting a verdict, and therefore certainly not done"
    );
}

#[tokio::test]
async fn a_store_level_refusal_reaches_the_caller_as_an_error() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;

    let mut runner = fixture.runner();

    // SCHEDULE for a project that was never observed: the step refuses, and the
    // refusal is an `Err`, not an empty report.
    let unknown = ProjectId::from_string("PROJ-unknown");
    let err = runner
        .schedule(&unknown, now())
        .await
        .expect_err("scheduling an unplanned project is refused");
    assert!(matches!(err, RunnerError::Schedule(_)), "got {err:?}");
    assert!(
        err.to_string().contains("no active plan"),
        "the reason reaches the caller intact, got: {err}"
    );

    // PLAN for the same project fails the same way, and both are propagated
    // rather than swallowed into a default.
    let plan_err = runner
        .plan(
            PlanRequest {
                project_id: unknown,
                plan_id: PlanId::from_string("PLAN-x"),
                objective: "objective".into(),
                rationale: "rationale".into(),
                tasks: vec![spec_with_check("AUTH", PASSES)],
                authorized_by: AgentId::from_string("AGENT-planner"),
            },
            now(),
        )
        .await
        .expect_err("planning an unobserved project is refused");
    assert!(matches!(plan_err, RunnerError::Plan(_)), "got {plan_err:?}");

    // Planning twice under the same plan id is a refusal too — a plan's id is
    // its identity.
    runner
        .plan(plan_request(vec![spec_with_check("AUTH", PASSES)]), now())
        .await
        .expect("the first plan lands");
    let duplicate = runner
        .plan(plan_request(vec![spec_with_check("AUTH", PASSES)]), later())
        .await
        .expect_err("a duplicate plan id is refused");
    assert!(
        matches!(duplicate, RunnerError::Plan(_)),
        "got {duplicate:?}"
    );
}

#[tokio::test]
async fn a_recovery_briefing_is_reconstructible_from_the_store_alone() {
    let fixture = Fixture::new().await;
    fixture.observed().await;
    fixture
        .register_agent("AGENT-1", vec![Capability::Coding])
        .await;

    let mut runner = fixture.runner();
    runner
        .plan(plan_request(vec![spec_with_check("AUTH", FAILS)]), now())
        .await
        .expect("the plan lands");
    let scheduled = runner
        .schedule(&fixture.project, now())
        .await
        .expect("the scheduling round");
    let assignment = scheduled
        .assignment_for(&TaskId::from_string("AUTH"))
        .expect("AUTH was handed out")
        .clone();

    // A recovery briefing is built from a checkpoint, and checkpoints advance
    // when OBSERVE records a real change while work is in flight. So: move the
    // repository, then observe again. This is Phase 12's activation arriving
    // through the runner's own OBSERVE call.
    fixture.commit("src/auth.rs", "// work in progress\n", "start the work");
    let checkpointed = runner
        .observe(
            &fixture.project,
            &fixture.repository,
            &fixture.repo_path(),
            later(),
        )
        .await
        .expect("an observation that records a change");
    assert_eq!(
        checkpointed.checkpoints_written, 1,
        "the in-flight task's recovery lineage advanced on the change"
    );

    // The task is in progress with a checkpoint and a tenure, which is exactly
    // the state resumption is a question about.
    let brief = runner
        .recovery(&TaskId::from_string("AUTH"))
        .await
        .expect("the recovery build")
        .expect("an in-flight task with a checkpoint has a briefing");
    assert_eq!(brief.task_id, TaskId::from_string("AUTH"));
    assert_eq!(brief.tenures, 1, "the tenure is in the briefing");
    assert_eq!(brief.verifications, 0, "nothing has been judged yet");
    assert!(
        !brief.next_action.is_empty(),
        "a replacement is told what to do first"
    );
    assert!(
        !brief.gaps_clean,
        "the structural gaps are reported rather than papered over"
    );

    // The briefing is reconstructed from persisted records alone, so a runner
    // that saw none of this still produces the same answer. A fresh runner over
    // the same store is exactly that.
    let mut cold = fixture.runner();
    let rebuilt = cold
        .recovery(&TaskId::from_string("AUTH"))
        .await
        .expect("the recovery build")
        .expect("a briefing is reconstructible from the store alone");
    assert_eq!(rebuilt.resume_status, brief.resume_status);
    assert_eq!(rebuilt.tenures, brief.tenures);
    assert_eq!(rebuilt.next_action, brief.next_action);

    // And once the work has been judged, the verdict joins the briefing.
    runner
        .simulate_agent_turn(&assignment, later())
        .await
        .expect("the simulated turn");
    runner
        .verify(&fixture.project)
        .await
        .expect("the verification round");
    let after = runner
        .recovery(&TaskId::from_string("AUTH"))
        .await
        .expect("the recovery build")
        .expect("the briefing still builds");
    assert_eq!(after.verifications, 1, "the verdict is in the briefing");

    // A task Orqyn has never seen is a distinct answer — `None`, not a package
    // full of nothing.
    assert!(
        runner
            .recovery(&TaskId::from_string("NO-SUCH-TASK"))
            .await
            .expect("the lookup itself succeeds")
            .is_none(),
        "an absent task has no briefing, and that is not an error"
    );
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_same_pinned_run_twice_produces_identical_records() {
    // `IdGenerator` is a deterministic counter, and every step here is run at a
    // caller-supplied instant, so a full run is reproducible down to the
    // assignment ids it mints. That is the property a serialized run contract
    // depends on, so it is asserted over the whole record rather than a slice.
    let mut first: Option<String> = None;

    for _ in 0..2 {
        let fixture = Fixture::new().await;
        fixture.observed().await;
        fixture
            .register_agent("AGENT-1", vec![Capability::Coding])
            .await;

        let mut runner = fixture.runner();
        let planned = runner
            .plan(plan_request(vec![spec_with_check("AUTH", PASSES)]), now())
            .await
            .expect("the plan lands");
        let scheduled = runner
            .schedule(&fixture.project, now())
            .await
            .expect("the scheduling round");
        let assignment = scheduled
            .assignment_for(&TaskId::from_string("AUTH"))
            .expect("AUTH was handed out")
            .clone();
        let turn = runner
            .simulate_agent_turn(&assignment, now())
            .await
            .expect("the simulated turn");
        let verified = runner
            .verify(&fixture.project)
            .await
            .expect("the verification round");

        let rendered = serde_json::to_string_pretty(&(&planned, &scheduled, &turn, &verified))
            .expect("the records serialize");

        match &first {
            None => first = Some(rendered),
            Some(previous) => assert_eq!(
                *previous, rendered,
                "a pinned clock and a deterministic id generator should make two runs identical"
            ),
        }
    }
}
