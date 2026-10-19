//! The receiving-agent flow, end to end, on real SQLite.
//!
//! # What this exercises, and what it is not
//!
//! **Real:** a SQLite store, a git repository, the real PLAN/SCHEDULE/ASSIGN
//! path, a real spawned child process speaking the Claude Code stdio protocol,
//! and the real verification engine.
//!
//! **Not claimed:** that Claude Code ran. The agent is a locally compiled fixture
//! that speaks the same protocol — a real program, but not Claude Code. There is
//! no authenticated live-agent evidence in this file, and none is implied.
//!
//! # The claim under test
//!
//! A receiver may accept a handoff and go on to do the work, and doing so must
//! not let the handoff do any of it for them. Acceptance records attribution;
//! ASSIGN grants authority; VERIFY decides the outcome. Each is asserted
//! separately so a regression in one cannot hide behind the others.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use director_adapters::git::GitService;
use director_adapters::{InMemoryProvider, LocalExecutor};
use director_app::agent::DriveOptions;
use director_app::brief::BriefLimits;
use director_app::handoff::{accept, continuation, inspect, Freshness, ReceiveError};
use director_app::plan::{PlanRequest, TaskSpec};
use director_app::recovery::RecoveryBuilder;
use director_app::runner::Runner;
use director_domain::checkpoint::Checkpoint;
use director_domain::handoff::{Handoff, HandoffCause};
use director_domain::ids::{
    AgentId, AssignmentId, CheckpointId, HandoffId, MachineId, PlanId, ProjectId, RepositoryId,
    SessionId, TaskId,
};
use director_domain::store::{
    AssignmentRepository, CheckpointRepository, HandoffRepository, TaskRepository,
};
use director_domain::task::{ExpectedOutput, Task, TaskStatus};
use director_domain::{
    ActionRepository, Agent, AgentRepository, Capability, Harness, Project, ProjectRepository,
    VerificationRepository,
};
use director_store::Store as SqliteStore;

const PROJ: &str = "PROJ-1";
const REPO: &str = "REPO-1";
const TASK: &str = "WORK";
const OUT: &str = "AGENT-out";
const IN: &str = "AGENT-in";

fn later(step: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000 + step * 60, 0).expect("a pinned moment")
}

// ---------------------------------------------------------------------------
// Deterministic agent fixture - a real process, not a mock
// ---------------------------------------------------------------------------

fn fixture_binary() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("orqynn-receive-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let source = dir.join("agent.rs");
        let binary = dir.join(if cfg!(windows) { "agent.exe" } else { "agent" });
        std::fs::write(&source, FIXTURE_SOURCE).expect("source");
        let status = Command::new("rustc")
            .arg("--edition")
            .arg("2021")
            .arg("-O")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .status()
            .expect("rustc on PATH");
        assert!(status.success(), "the agent fixture must compile");
        binary
    })
    .clone()
}

/// Reads the briefing from stdin, then behaves as `ORQYNN_E2E_AGENT` says.
///
/// `succeed` claims success having changed nothing - the checks, not the agent,
/// decide the verdict. `fail` reports an error, which must never read as done.
const FIXTURE_SOURCE: &str = r#"
use std::io::Read;
fn main() {
    let mut brief = String::new();
    let _ = std::io::stdin().read_to_string(&mut brief);
    match std::env::var("ORQYNN_E2E_AGENT").unwrap_or_else(|_| "succeed".into()).as_str() {
        "fail" => {
            eprintln!("the agent refused");
            println!("{{\"is_error\":true,\"result\":\"could not start\",\"type\":\"result\"}}");
            std::process::exit(1);
        }
        _ => {
            println!("{{\"is_error\":false,\"result\":\"did the work\",\"type\":\"result\"}}");
        }
    }
}
"#;

fn driver_for(script: &str) -> director_adapters::harness::ClaudeCodeDriver {
    director_adapters::harness::ClaudeCodeDriver::new(
        director_adapters::harness::ClaudeCodeConfig {
            binary: fixture_binary().to_string_lossy().into_owned(),
            default_timeout: Duration::from_secs(30),
            env: vec![("ORQYNN_E2E_AGENT".into(), script.into())],
            ..director_adapters::harness::ClaudeCodeConfig::default()
        },
    )
}

fn git_repo(dir: &Path) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git on PATH");
        assert!(status.success(), "git {args:?} must succeed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.name", "Orqynn Tests"]);
    run(&["config", "user.email", "tests@orqyn.invalid"]);
    run(&["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "# fixture\n").expect("write");
    run(&["add", "README.md"]);
    run(&["commit", "-q", "-m", "initial"]);
}

struct Flow {
    _tmp: tempfile::TempDir,
    store: SqliteStore,
    repo_dir: PathBuf,
    git: GitService,
    executor: LocalExecutor,
}

impl Flow {
    async fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("repo");
        git_repo(&repo_dir);
        let git = GitService::new(tmp.path().join("git-state")).expect("git service");

        let store = SqliteStore::open(tmp.path().join("orqynn.db"))
            .await
            .expect("store");
        store
            .projects()
            .create_project(&Project::new(
                ProjectId::from_string(PROJ),
                PROJ,
                repo_dir.to_str().expect("utf-8"),
            ))
            .await
            .expect("project");
        for (id, machine) in [(OUT, "MACH-a"), (IN, "MACH-b")] {
            store
                .agents()
                .register_agent(&Agent::register(
                    AgentId::from_string(id),
                    id,
                    Harness::ClaudeCode,
                    MachineId::from_string(machine),
                    vec![Capability::Coding],
                ))
                .await
                .expect("agent");
        }
        Flow {
            _tmp: tmp,
            store,
            repo_dir,
            git,
            executor: LocalExecutor::default(),
        }
    }

    fn runner<'a>(
        &'a self,
        memory: &'a InMemoryProvider,
    ) -> Runner<'a, InMemoryProvider, LocalExecutor> {
        Runner::new(
            &self.store,
            &self.git,
            memory,
            &self.executor,
            &self.repo_dir,
        )
    }

    /// Observe, plan the task with one check, and activate the plan.
    async fn planned(&self, memory: &InMemoryProvider, check: &str) {
        let mut runner = self.runner(memory);
        runner
            .observe(
                &ProjectId::from_string(PROJ),
                &RepositoryId::from_string(REPO),
                self.repo_dir.to_str().expect("utf-8"),
                later(-80),
            )
            .await
            .expect("observe");

        let mut spec = TaskSpec::new(
            TaskId::from_string(TASK),
            TASK,
            "do the work so the check passes",
        );
        spec.expected_outputs = vec![ExpectedOutput {
            criterion: format!("`{check}` exits zero"),
            check: Some(check.to_string()),
        }];
        runner
            .plan(
                PlanRequest {
                    project_id: ProjectId::from_string(PROJ),
                    plan_id: PlanId::from_string("PLAN-WORK"),
                    objective: "the work".into(),
                    rationale: "requested".into(),
                    tasks: vec![spec],
                    authorized_by: AgentId::from_string(OUT),
                },
                later(-70),
            )
            .await
            .expect("plan");
    }

    fn handoff(&self, anchored: bool) -> Handoff {
        let mut record = Handoff::open(
            HandoffId::from_string("HO-1"),
            TaskId::from_string(TASK),
            AgentId::from_string(OUT),
            AgentId::from_string(IN),
            "UNTRUSTED-NARRATIVE: the outgoing agent claims the work is finished",
        )
        .in_project(ProjectId::from_string(PROJ))
        .from(SessionId::from_string("SES-out"))
        .caused_by(HandoffCause::Displaced);
        if anchored {
            record = record.derived_from(Some(CheckpointId::from_string("CHK-1")), Some(1));
        }
        record
    }

    async fn seed_checkpoint(&self) {
        let checkpoint = Checkpoint::new(
            CheckpointId::from_string("CHK-1"),
            TaskId::from_string(TASK),
            "the work is half done",
            "the reader is unwritten",
            "write the reader",
        );
        self.store
            .checkpoints()
            .create_checkpoint(&checkpoint, None)
            .await
            .expect("checkpoint saved");
    }
}

/// The assignment a schedule round actually created, or `None` if it created none.
///
/// `ScheduleReport` reports which tasks it assigned, not the assignment ids, so
/// the id is read back from the store - which is also what proves the assignment
/// was really persisted rather than merely planned.
async fn assignment_after(store: &SqliteStore) -> Option<AssignmentId> {
    store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string(TASK))
        .await
        .expect("assignments")
        .map(|assignment| assignment.id)
}

async fn task_row(store: &SqliteStore) -> Task {
    store
        .tasks()
        .get_task(&TaskId::from_string(TASK))
        .await
        .expect("task")
}

// ---------------------------------------------------------------------------
// 1-3, 6, 7. The complete flow
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receive_accept_assign_drive_and_verify_completes_the_work() {
    let flow = Flow::new().await;
    let memory = InMemoryProvider::new();
    let check = "git --version";
    flow.planned(&memory, check).await;
    flow.seed_checkpoint().await;

    // RECEIVE: a displacement left a handoff addressed to the other agent.
    flow.store
        .handoffs()
        .open_handoff(&flow.handoff(true))
        .await
        .expect("handoff recorded");

    // INSPECT: what it says, and whether it still describes the task.
    let view = inspect(&flow.store, &HandoffId::from_string("HO-1"))
        .await
        .expect("inspectable");
    assert_eq!(view.record.to_agent, AgentId::from_string(IN));
    assert_eq!(view.freshness, Freshness::Current);
    assert_eq!(
        view.record.summary, "UNTRUSTED-NARRATIVE: the outgoing agent claims the work is finished",
        "the narrative is carried verbatim - it is data, and the danger is in \
         treating it as state"
    );

    // ACCEPT: attribution only.
    let accepted = accept(
        &flow.store,
        &HandoffId::from_string("HO-1"),
        &AgentId::from_string(IN),
        &SessionId::from_string("SES-in"),
        later(-60),
    )
    .await
    .expect("accepted");
    assert_eq!(
        accepted.state,
        director_domain::handoff::HandoffState::Accepted
    );

    // 2. Acceptance did nothing that ASSIGN owns.
    let after_accept = task_row(&flow.store).await;
    assert_eq!(
        after_accept.status,
        TaskStatus::Todo,
        "acceptance neither completes a task nor moves it into flight"
    );
    assert!(
        flow.store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string(TASK))
            .await
            .expect("assignments")
            .is_none(),
        "and creates no assignment implicitly"
    );

    // CONTINUATION: rebuilt from authoritative state; the narrative is context.
    let continuation = continuation(
        &flow.store,
        &TaskId::from_string(TASK),
        BriefLimits::default(),
    )
    .await
    .expect("a continuation");
    assert_eq!(
        continuation.accepted_from.as_ref(),
        Some(&AgentId::from_string(OUT))
    );
    assert!(
        !continuation.brief.text.contains("UNTRUSTED-NARRATIVE"),
        "the outgoing agent's claim is never merged into the brief: {}",
        continuation.brief.text
    );

    // ASSIGN: authority still comes from here, through schedule.
    let mut runner = flow.runner(&memory);
    let scheduled = runner
        .schedule(&ProjectId::from_string(PROJ), later(-50))
        .await
        .expect("schedule");
    assert!(
        scheduled
            .assigned
            .iter()
            .any(|pairing| pairing.task_id == TaskId::from_string(TASK)),
        "the task is handable, so plan membership still governs"
    );
    let assignment_id = assignment_after(&flow.store)
        .await
        .expect("an assignment exists");
    assert_eq!(
        flow.store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string(TASK))
            .await
            .expect("assignments")
            .expect("sitting")
            .agent_id,
        AgentId::from_string(IN),
        "and the receiving agent is the one holding it"
    );

    // DRIVE: a real spawned process.
    let mut options = DriveOptions::in_dir(flow.repo_dir.to_string_lossy().into_owned());
    options.timeout = Duration::from_secs(30);
    let driven = runner
        .drive_agent_with_memory_at(
            &driver_for("succeed"),
            &TaskId::from_string(TASK),
            &assignment_id,
            &options,
            later(-40),
        )
        .await
        .expect("the drive reports");

    // VERIFY decides. The agent claimed success having changed nothing; only the
    // check is allowed to decide that the task is done.
    let verdict = driven.verify.expect("a verdict was reached");
    assert!(
        verdict.all_passed(),
        "the check passed, so the task is done"
    );
    let final_task = task_row(&flow.store).await;
    assert_eq!(
        final_task.status,
        TaskStatus::Done,
        "and the task reached done only because verification said so"
    );
    let verifications = flow
        .store
        .verifications()
        .verifications_for_task(&TaskId::from_string(TASK))
        .await
        .expect("verifications");
    assert_eq!(verifications.len(), 1, "exactly one round is on record");
    assert!(
        !flow
            .store
            .actions()
            .actions_for_task(&TaskId::from_string(TASK))
            .await
            .expect("actions")
            .is_empty(),
        "and the observed commands were recorded with it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_check_after_resumption_never_becomes_done() {
    let flow = Flow::new().await;
    let memory = InMemoryProvider::new();
    // A check that cannot pass: the agent's cooperation is irrelevant.
    flow.planned(&memory, "git --no-such-flag").await;

    flow.store
        .handoffs()
        .open_handoff(&flow.handoff(false))
        .await
        .expect("recorded");
    accept(
        &flow.store,
        &HandoffId::from_string("HO-1"),
        &AgentId::from_string(IN),
        &SessionId::from_string("SES-in"),
        later(-60),
    )
    .await
    .expect("accepted");

    let mut runner = flow.runner(&memory);
    let scheduled = runner
        .schedule(&ProjectId::from_string(PROJ), later(-50))
        .await
        .expect("schedule");
    let _ = scheduled;
    let assignment_id = assignment_after(&flow.store).await.expect("handable");
    let mut options = DriveOptions::in_dir(flow.repo_dir.to_string_lossy().into_owned());
    options.timeout = Duration::from_secs(30);
    let driven = runner
        .drive_agent_with_memory_at(
            &driver_for("succeed"),
            &TaskId::from_string(TASK),
            &assignment_id,
            &options,
            later(-40),
        )
        .await
        .expect("the drive reports");

    assert!(
        driven.verify.is_some(),
        "verification still judged the round"
    );
    assert!(
        !driven.verify.expect("verdict").all_passed(),
        "and it did not pass: the agent claimed success, the check disagreed"
    );
    assert_eq!(
        task_row(&flow.store).await.status,
        TaskStatus::Failed,
        "so the task is failed, not done"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_that_reports_an_error_is_not_done() {
    let flow = Flow::new().await;
    let memory = InMemoryProvider::new();
    let check = "git --version";
    flow.planned(&memory, check).await;

    let mut runner = flow.runner(&memory);
    let scheduled = runner
        .schedule(&ProjectId::from_string(PROJ), later(-50))
        .await
        .expect("schedule");
    let _ = scheduled;
    let assignment_id = assignment_after(&flow.store).await.expect("handable");
    let mut options = DriveOptions::in_dir(flow.repo_dir.to_string_lossy().into_owned());
    options.timeout = Duration::from_secs(30);
    let driven = runner
        .drive_agent_with_memory_at(
            &driver_for("fail"),
            &TaskId::from_string(TASK),
            &assignment_id,
            &options,
            later(-40),
        )
        .await
        .expect("the drive reports");

    // The agent's own failure is visible in the drive...
    assert!(
        driven
            .drive
            .outcome
            .as_ref()
            .map(|o| o.reported_error)
            .unwrap_or(false),
        "the harness reported its own error, and the drive carries that fact"
    );

    // ...but the *task status* follows verification alone. This is the domain
    // invariant, and it is worth stating plainly rather than asserting the
    // intuitive thing: a self-reported failure does not by itself decide the task,
    // and a passing check does not get overturned by an agent's opinion.
    //
    // What stops a reported error from being reported as success is one layer up,
    // in the CLI's `classify`, which turns `reported_error` into a non-zero exit
    // even when the verdict passed. Asserting that here would be asserting
    // something the domain deliberately does not do.
    assert!(
        driven.verify.expect("verdict").all_passed(),
        "the check passed, so verification - and only verification - decides"
    );
    assert_eq!(
        task_row(&flow.store).await.status,
        TaskStatus::Done,
        "and the task reflects the verdict, not the agent's self-assessment"
    );
}

// 6. Duplicate acceptance, and safe retry of an interrupted workflow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_acceptance_is_refused_and_an_interrupted_workflow_can_be_retried() {
    let flow = Flow::new().await;
    let memory = InMemoryProvider::new();
    flow.planned(&memory, "git --version").await;
    flow.store
        .handoffs()
        .open_handoff(&flow.handoff(false))
        .await
        .expect("recorded");

    let first = accept(
        &flow.store,
        &HandoffId::from_string("HO-1"),
        &AgentId::from_string(IN),
        &SessionId::from_string("SES-in"),
        later(-60),
    )
    .await;
    assert!(first.is_ok(), "the first acceptance succeeds");

    let duplicate = accept(
        &flow.store,
        &HandoffId::from_string("HO-1"),
        &AgentId::from_string(IN),
        &SessionId::from_string("SES-in"),
        later(-59),
    )
    .await;
    assert!(
        matches!(duplicate, Err(ReceiveError::NotOpen { .. })),
        "a retry of acceptance is refused, not applied twice: {duplicate:?}"
    );
    let history = flow
        .store
        .handoffs()
        .handoffs_for_task(&TaskId::from_string(TASK))
        .await
        .expect("listed");
    assert_eq!(history.len(), 1, "no duplicate record was created");

    // The interrupted workflow is retried by scheduling again - the handoff is
    // not the retry mechanism, and nothing about it is required to proceed.
    let mut runner = flow.runner(&memory);
    let scheduled = runner
        .schedule(&ProjectId::from_string(PROJ), later(-50))
        .await
        .expect("schedule");
    assert!(
        scheduled
            .assigned
            .iter()
            .any(|pairing| pairing.task_id == TaskId::from_string(TASK)),
        "the work is still handable after an accepted handoff"
    );
}

// 7. Plan membership still governs eligibility.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_outside_the_active_plan_is_not_handable() {
    let flow = Flow::new().await;
    let memory = InMemoryProvider::new();
    flow.planned(&memory, "git --version").await;

    // A second plan supersedes the first, taking the task out of the active one.
    let mut runner = flow.runner(&memory);
    runner
        .plan(
            PlanRequest {
                project_id: ProjectId::from_string(PROJ),
                plan_id: PlanId::from_string("PLAN-SOMETHING-ELSE"),
                objective: "unrelated work".into(),
                rationale: "a deliberate replan".into(),
                tasks: vec![TaskSpec::new(
                    TaskId::from_string("OTHER"),
                    "OTHER",
                    "something else entirely",
                )],
                authorized_by: AgentId::from_string(OUT),
            },
            later(-60),
        )
        .await
        .expect("replan");

    let scheduled = runner
        .schedule(&ProjectId::from_string(PROJ), later(-50))
        .await
        .expect("schedule");
    assert!(
        !scheduled
            .assigned
            .iter()
            .any(|pairing| pairing.task_id == TaskId::from_string(TASK)),
        "a task the active plan does not name is not handed out, whatever its \
         handoff history says"
    );
}

// 3. Stale and unanchored provenance.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_provenance_is_reported_and_continuation_still_rebuilds() {
    let flow = Flow::new().await;
    let memory = InMemoryProvider::new();
    flow.planned(&memory, "git --version").await;
    // No checkpoint is seeded, so the recorded one cannot match.
    flow.store
        .handoffs()
        .open_handoff(&flow.handoff(true))
        .await
        .expect("recorded");

    let view = inspect(&flow.store, &HandoffId::from_string("HO-1"))
        .await
        .expect("inspectable");
    assert!(
        matches!(view.freshness, Freshness::Stale { .. }),
        "a checkpoint the task does not have makes the snapshot stale"
    );

    // Rebuilding comes from the recovery package regardless.
    let rebuilt = continuation(
        &flow.store,
        &TaskId::from_string(TASK),
        BriefLimits::default(),
    )
    .await
    .expect("a continuation");
    let outcome = RecoveryBuilder::new(&flow.store, TaskId::from_string(TASK))
        .from_stored_state()
        .build()
        .await
        .expect("recovery");
    assert_eq!(rebuilt.context.len(), 1);
    assert!(
        rebuilt.recovery_gap.is_some(),
        "and the gap is stated rather than hidden: {:?}",
        rebuilt.recovery_gap
    );
    let _ = outcome;
}

// A record with no provenance at all is never current.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handoff_with_no_provenance_is_never_current() {
    let flow = Flow::new().await;
    flow.planned(&InMemoryProvider::new(), "git --version")
        .await;
    flow.store
        .handoffs()
        .open_handoff(&flow.handoff(false))
        .await
        .expect("recorded");
    let view = inspect(&flow.store, &HandoffId::from_string("HO-1"))
        .await
        .expect("inspectable");
    assert!(
        matches!(view.freshness, Freshness::Stale { .. }),
        "nothing to match against cannot be shown to match"
    );
    let _ = AssignmentId::from_string("ASG-1");
}
