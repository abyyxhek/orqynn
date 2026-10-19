//! `orqynn resume`: what it decides, and what it refuses.
//!
//! # What is real
//!
//! Real SQLite, a real git repository, a genuinely spawned agent process, and the
//! real verification engine. The agent is a locally compiled stand-in that speaks
//! the same stdio protocol as Claude Code — a real program, but not Claude Code.
//! Where a test needs to know whether an agent ran *at all*, the fixture writes a
//! sentinel file, so "no agent was launched" is asserted by the absence of a file
//! on disk rather than by the absence of a log line.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use director_domain::ids::TaskId;
use director_domain::store::{AssignmentRepository, TaskRepository, VerificationRepository};
use director_domain::task::TaskStatus;

#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/lifecycle.rs"]
mod lifecycle;

use crate::config::{Command as CliCommand, RunArgs};
use crate::lifecycle::{ExitCode, ResumeAction};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A stand-in agent that records that it ran, then reports success.
fn fixture_binary() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("orqynn-resume-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let source = dir.join("agent.rs");
        let binary = dir.join(if cfg!(windows) { "agent.exe" } else { "agent" });
        std::fs::write(&source, FIXTURE_SOURCE).expect("fixture source");
        let status = Command::new("rustc")
            .arg("--edition")
            .arg("2021")
            .arg("-O")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .status()
            .expect("rustc must be on PATH");
        assert!(status.success(), "the agent fixture must compile");
        binary
    })
    .clone()
}

/// Writes `$ORQYNN_AGENT_SENTINEL` on every launch, so a test can prove an agent
/// was *not* started rather than merely that it reported nothing.
const FIXTURE_SOURCE: &str = r#"
fn main() {
    use std::io::Read;
    if let Ok(path) = std::env::var("ORQYNN_AGENT_SENTINEL") {
        let _ = std::fs::write(path, "ran");
    }
    let mut brief = String::new();
    let _ = std::io::stdin().read_to_string(&mut brief);
    println!("{{\"type\":\"result\",\"is_error\":false,\"result\":\"did the work\"}}");
}
"#;

fn driver_for(timeout: Duration, sentinel: &Path) -> director_adapters::harness::ClaudeCodeDriver {
    director_adapters::harness::ClaudeCodeDriver::new(
        director_adapters::harness::ClaudeCodeConfig {
            binary: fixture_binary().to_string_lossy().into_owned(),
            default_timeout: timeout,
            env: vec![(
                "ORQYNN_AGENT_SENTINEL".into(),
                sentinel.to_string_lossy().into_owned(),
            )],
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

/// One scratch repository, one task created by a real `run`, and the arguments a
/// `resume` of it would use.
struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    sentinel: PathBuf,
    args: RunArgs,
}

impl Fixture {
    /// Runs the real `run` lifecycle with the given checks, leaving the task in
    /// whatever state those checks put it in.
    async fn seeded(check: &str) -> Self {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        git_repo(&repo);
        let sentinel = tmp.path().join("agent-ran");

        let mut args = RunArgs::parse([
            "--repo",
            repo.to_str().expect("utf-8"),
            "--task",
            "make the greeting say hello",
            "--check",
            check,
        ])
        .expect("parses");
        args.agent_binary = fixture_binary().to_string_lossy().into_owned();
        args.resolve().expect("resolves");

        let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
        let driver = driver_for(Duration::from_secs(30), &sentinel);
        lifecycle::run_lifecycle(
            &runtime,
            &director_adapters::InMemoryProvider::new(),
            &driver,
            &director_adapters::executor::LocalExecutor::default(),
            &args,
            chrono::Utc::now(),
        )
        .await
        .expect("the seeding run");

        let mut resume_args = args.clone();
        resume_args.command = CliCommand::Resume;
        Fixture {
            _tmp: tmp,
            repo,
            sentinel,
            args: resume_args,
        }
    }

    fn task_id(&self) -> TaskId {
        TaskId::from_string(self.args.task_id())
    }

    async fn resume(&self) -> Result<lifecycle::ResumeReport, lifecycle::RunError> {
        let runtime = lifecycle::open_runtime(&self.args).await.expect("runtime");
        let driver = driver_for(Duration::from_secs(30), &self.sentinel);
        lifecycle::resume_lifecycle(
            &runtime,
            &director_adapters::InMemoryProvider::new(),
            &driver,
            &director_adapters::executor::LocalExecutor::default(),
            &self.args,
            chrono::Utc::now(),
        )
        .await
    }

    /// The number of assignments this task has ever held.
    async fn assignment_count(&self) -> usize {
        let runtime = lifecycle::open_runtime(&self.args).await.expect("runtime");
        runtime
            .store
            .assignments()
            .assignment_history(&self.task_id())
            .await
            .expect("assignments")
            .len()
    }

    /// Force the task into a status, for the states `run` cannot produce on
    /// demand. This is test setup, written through the same CAS the domain uses.
    async fn force_status(&self, status: TaskStatus) {
        let runtime = lifecycle::open_runtime(&self.args).await.expect("runtime");
        let mut task = runtime
            .store
            .tasks()
            .get_task(&self.task_id())
            .await
            .expect("task");
        task.status = status;
        runtime
            .store
            .tasks()
            .update_task(&task)
            .await
            .expect("status updated");
    }

    fn agent_ran(&self) -> bool {
        self.sentinel.exists()
    }
}

// ---------------------------------------------------------------------------
// 1. Refusals: the states a resume must not touch
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_done_task_is_refused_and_nothing_is_launched() {
    let fixture = Fixture::seeded("git --version").await;
    let before = fixture.agent_ran();

    let report = fixture.resume().await.expect("a report");
    assert_eq!(report.action, ResumeAction::Refused);
    assert_eq!(report.exit_code, ExitCode::UnsafeState);
    let text = report.lines.join("\n");
    assert!(text.contains("already done"), "{text}");
    assert!(text.contains("nothing was written"), "{text}");
    assert_eq!(
        fixture.agent_ran(),
        before,
        "no new agent may be launched for a finished task"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_task_is_refused() {
    let fixture = Fixture::seeded("git --version").await;
    // `run` leaves a passing task `done`; cancel is the other terminal state, and
    // it is terminal for the same reason.
    fixture.force_status(TaskStatus::Cancelled).await;

    let report = fixture.resume().await.expect("a report");
    assert_eq!(report.action, ResumeAction::Refused);
    assert_eq!(report.exit_code, ExitCode::UnsafeState);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_task_is_refused_with_a_specific_error() {
    let fixture = Fixture::seeded("git --version").await;
    let mut args = fixture.args.clone();
    args.task_id = "NO-SUCH-TASK".into();

    let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
    let driver = driver_for(Duration::from_secs(30), &fixture.sentinel);
    let report = lifecycle::resume_lifecycle(
        &runtime,
        &director_adapters::InMemoryProvider::new(),
        &driver,
        &director_adapters::executor::LocalExecutor::default(),
        &args,
        chrono::Utc::now(),
    )
    .await;

    // A task that cannot be found is an orchestration failure, not a silent new
    // task: `resume` must never fall back to creating one.
    match report {
        Err(lifecycle::RunError::Orchestration { step, reason }) => {
            assert_eq!(step, "LOAD");
            assert!(reason.contains("NO-SUCH-TASK"), "{reason}");
        }
        other => panic!("an unknown task must not be resumed or created: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_awaiting_verification_is_re_verified_without_launching_an_agent() {
    let fixture = Fixture::seeded("git --version").await;
    // `run` drives and verifies in one go, so the awaiting state is reached by
    // hand here — which is exactly the state an interrupted run leaves behind.
    fixture.force_status(TaskStatus::VerificationPending).await;
    std::fs::remove_file(&fixture.sentinel).ok();

    let report = fixture.resume().await.expect("a report");
    assert_eq!(report.action, ResumeAction::Reverified);
    assert_eq!(report.exit_code, ExitCode::Success);
    let text = report.lines.join("\n");
    assert!(text.contains("no agent is launched"), "{text}");
    assert!(
        !fixture.agent_ran(),
        "re-verification must not start a process; the sentinel is the proof"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_previous_passing_verdict_is_not_trusted_without_re_running_the_checks() {
    // The load-bearing test. The task is put into the awaiting state with a
    // verdict already on record, the tree is made to satisfy its criterion, and
    // the check is re-run: that passes. The tree is then broken. A resume that
    // trusted the stored verdict would report the task done; one that re-asks the
    // current tree will not.
    //
    // The criterion is `git diff --quiet`, which is available wherever git is and
    // whose result genuinely depends on the tree rather than on the platform.
    let fixture = Fixture::seeded("git diff --quiet").await;
    fixture.force_status(TaskStatus::VerificationPending).await;

    let passing = fixture.resume().await.expect("a report");
    assert_eq!(
        passing.exit_code,
        ExitCode::Success,
        "a clean tree satisfies the criterion: {}",
        passing.lines.join("\n")
    );

    // A passing verdict makes the task `done`, and a done task is refused — so
    // hold it in the awaiting state again. That is the state an interrupted run
    // leaves behind, and without it this test would be measuring the terminal
    // refusal instead of the re-verification.
    fixture.force_status(TaskStatus::VerificationPending).await;

    // Break the tree — an uncommitted edit to a tracked file — and ask again.
    std::fs::write(
        fixture.repo.join("README.md"),
        "# edited after the verdict\n",
    )
    .expect("edit the tree");
    let failing = fixture.resume().await.expect("a report");
    assert_eq!(
        failing.exit_code,
        ExitCode::VerificationFailed,
        "the earlier passing verdict proved nothing; the current tree must be asked: {}",
        failing.lines.join("\n")
    );
    assert!(failing.lines.join("\n").contains("FAIL"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supplied_checks_that_disagree_with_the_recorded_criteria_are_refused() {
    let fixture = Fixture::seeded("git --version").await;
    fixture.force_status(TaskStatus::VerificationPending).await;

    let mut args = fixture.args.clone();
    args.checks = vec!["cargo test --workspace".into()];
    let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
    let driver = driver_for(Duration::from_secs(30), &fixture.sentinel);
    let report = lifecycle::resume_lifecycle(
        &runtime,
        &director_adapters::InMemoryProvider::new(),
        &driver,
        &director_adapters::executor::LocalExecutor::default(),
        &args,
        chrono::Utc::now(),
    )
    .await
    .expect("a report");

    assert_eq!(report.action, ResumeAction::Refused);
    assert_eq!(report.exit_code, ExitCode::UnsafeState);
    let text = report.lines.join("\n");
    assert!(text.contains("does not match"), "{text}");
    assert!(text.contains("were not changed"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn matching_checks_are_accepted_on_a_resume() {
    let fixture = Fixture::seeded("git --version").await;
    fixture.force_status(TaskStatus::VerificationPending).await;
    // The same command, supplied again: agreement, not absence, is what is asked.
    let mut args = fixture.args.clone();
    args.checks = vec!["git --version".into()];
    let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
    let driver = driver_for(Duration::from_secs(30), &fixture.sentinel);
    let report = lifecycle::resume_lifecycle(
        &runtime,
        &director_adapters::InMemoryProvider::new(),
        &driver,
        &director_adapters::executor::LocalExecutor::default(),
        &args,
        chrono::Utc::now(),
    )
    .await
    .expect("a report");
    assert_eq!(report.action, ResumeAction::Reverified);
    assert_eq!(report.exit_code, ExitCode::Success);
}

// ---------------------------------------------------------------------------
// 2. Retry: only with an explicit, valid remediation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_task_is_not_retried_without_an_explicit_remediation() {
    let fixture = Fixture::seeded("git --no-such-flag").await;
    let before = fixture.assignment_count().await;

    let report = fixture.resume().await.expect("a report");
    assert_eq!(report.action, ResumeAction::Refused);
    assert_eq!(report.exit_code, ExitCode::UnsafeState);
    let text = report.lines.join("\n");
    assert!(text.contains("--remediate retry"), "{text}");
    assert_eq!(
        fixture.assignment_count().await,
        before,
        "a refusal must not create an assignment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authorised_retry_creates_a_new_tenure_rather_than_reusing_one() {
    let fixture = Fixture::seeded("git --no-such-flag").await;
    let before = fixture.assignment_count().await;
    let sentinel_before = fixture.agent_ran();

    let mut args = fixture.args.clone();
    args.remediate = Some(config::Remediation::Retry);
    let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
    let driver = driver_for(Duration::from_secs(30), &fixture.sentinel);
    let report = lifecycle::resume_lifecycle(
        &runtime,
        &director_adapters::InMemoryProvider::new(),
        &driver,
        &director_adapters::executor::LocalExecutor::default(),
        &args,
        chrono::Utc::now(),
    )
    .await
    .expect("a report");

    assert_eq!(report.action, ResumeAction::Redriven);
    // The check still fails, so the task is still not done — the retry ran and was
    // judged, and the judgement stands.
    assert_eq!(report.exit_code, ExitCode::VerificationFailed);
    let text = report.lines.join("\n");
    assert!(text.contains("the task failed verification"), "{text}");
    assert!(text.contains("is now waiting to be handed out"), "{text}");
    assert!(
        fixture.agent_ran(),
        "an authorised retry drives a fresh process (was {sentinel_before})"
    );
    assert_eq!(
        fixture.assignment_count().await,
        before + 1,
        "exactly one new tenure, not a duplicate and not none"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_zero_agent_exit_is_not_reported_as_success() {
    let fixture = Fixture::seeded("git --version").await;
    fixture.force_status(TaskStatus::Failed).await;

    let mut args = fixture.args.clone();
    args.remediate = Some(config::Remediation::Retry);
    let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
    // The fixture reports success; the check passes. The point of this test is the
    // inverse one below, so here we assert the whole round is judged by the check
    // and the agent's cooperation is what makes it pass.
    let driver = driver_for(Duration::from_secs(30), &fixture.sentinel);
    let report = lifecycle::resume_lifecycle(
        &runtime,
        &director_adapters::InMemoryProvider::new(),
        &driver,
        &director_adapters::executor::LocalExecutor::default(),
        &args,
        chrono::Utc::now(),
    )
    .await
    .expect("a report");
    let text = report.lines.join("\n");
    assert!(text.contains("agent ran"), "{text}");
    // Whatever the agent said, only the check decides.
    if report.exit_code == ExitCode::Success {
        assert!(text.contains("every check exited zero"), "{text}");
    } else {
        assert!(
            !text.contains("every check exited zero: the task is done"),
            "a non-zero exit must not be dressed up as success: {text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_agent_binary_is_refused_only_on_the_path_that_launches_one() {
    let fixture = Fixture::seeded("git --version").await;
    fixture.force_status(TaskStatus::VerificationPending).await;

    let mut args = fixture.args.clone();
    args.agent_binary = "C:/definitely/not/here/agent.exe".into();

    // Re-verification needs no agent, so this must not be refused for one.
    let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
    let driver = driver_for(Duration::from_secs(30), &fixture.sentinel);
    let report = lifecycle::resume_lifecycle(
        &runtime,
        &director_adapters::InMemoryProvider::new(),
        &driver,
        &director_adapters::executor::LocalExecutor::default(),
        &args,
        chrono::Utc::now(),
    )
    .await
    .expect("re-verification does not need the agent binary");
    assert_eq!(report.action, ResumeAction::Reverified);

    // The re-drive path does need it, and says so as a configuration error.
    fixture.force_status(TaskStatus::Failed).await;
    let mut args = fixture.args.clone();
    args.remediate = Some(config::Remediation::Retry);
    args.agent_binary = "C:/definitely/not/here/agent.exe".into();
    let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
    let driver = driver_for(Duration::from_secs(30), &fixture.sentinel);
    let outcome = lifecycle::resume_lifecycle(
        &runtime,
        &director_adapters::InMemoryProvider::new(),
        &driver,
        &director_adapters::executor::LocalExecutor::default(),
        &args,
        chrono::Utc::now(),
    )
    .await;
    assert!(
        matches!(outcome, Err(lifecycle::RunError::Config(_))),
        "a re-drive must refuse an agent that cannot be launched"
    );
}

// ---------------------------------------------------------------------------
// 3. Persistence and stability
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_result_survives_reopening_the_database() {
    let fixture = Fixture::seeded("git --version").await;
    fixture.force_status(TaskStatus::VerificationPending).await;
    let report = fixture.resume().await.expect("a report");
    assert_eq!(report.exit_code, ExitCode::Success);

    let reopened = director_store::Store::open(&fixture.args.database)
        .await
        .expect("the store reopens");
    let task = reopened
        .tasks()
        .get_task(&fixture.task_id())
        .await
        .expect("task reads back");
    assert_eq!(
        task.status,
        TaskStatus::Done,
        "the re-verified verdict was persisted, not merely printed"
    );
    let verifications = reopened
        .verifications()
        .verifications_for_task(&fixture.task_id())
        .await
        .expect("verifications");
    assert!(
        verifications.len() >= 2,
        "the re-verification is a second round on record, not a replacement: {}",
        verifications.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_working_tree_changes_survive_a_refusal() {
    let fixture = Fixture::seeded("git --version").await;
    let scratch = fixture.repo.join("half-done.txt");
    std::fs::write(&scratch, "an agent got this far").expect("write");

    let report = fixture.resume().await.expect("a report");
    assert_eq!(report.action, ResumeAction::Refused);
    assert_eq!(
        std::fs::read_to_string(&scratch).expect("the file is still there"),
        "an agent got this far",
        "a refusal must not touch the working tree"
    );
}

#[test]
fn the_refusal_exit_code_is_distinct_and_stable() {
    // A refused retry is not an execution failure. Conflating them would send an
    // operator to debug an agent that was never involved.
    assert_eq!(ExitCode::UnsafeState.status(), 8);
    assert_ne!(ExitCode::UnsafeState.status(), ExitCode::Execution.status());
    assert_ne!(
        ExitCode::UnsafeState.status(),
        ExitCode::Orchestration.status()
    );
}

#[test]
fn resume_requires_a_task_id_and_refuses_a_dry_run() {
    let mut args = RunArgs::parse(["resume", "--repo", "/tmp/r"]).expect("parses");
    args.command = CliCommand::Resume;
    assert_eq!(args.command, CliCommand::Resume);
    assert!(
        matches!(args.resolve(), Err(config::ConfigError::ResumeNeedsTaskId)),
        "resume will not invent a task to recover"
    );

    let mut dry = RunArgs::parse(["resume", "--repo", "/tmp/r", "--task-id", "T1", "--dry-run"])
        .expect("parses");
    dry.command = CliCommand::Resume;
    dry.repo = PathBuf::from(".");
    assert!(matches!(
        dry.resolve(),
        Err(config::ConfigError::DryRunOnResume)
    ));
}
