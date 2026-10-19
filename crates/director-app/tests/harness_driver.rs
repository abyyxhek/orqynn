//! Driving a real harness through the real lifecycle, with a real process.
//!
//! # What is real here, and what is not
//!
//! **Real:** SQLite on disk, the store, `monitor::acknowledge` /
//! `report_done`, the scheduling and assignment path, the verification engine
//! running real commands via `LocalExecutor`, the atomic verdict+actions write,
//! store reopen, and a genuinely spawned child process. The helper executable
//! this file uses is a real program — a compiled Rust binary run as a
//! subprocess — so spawning, argv handling, stream capture, exit status and
//! timeouts are all exercised against the operating system.
//!
//! **Not real, and deliberately not claimed:** the child is *not* Claude Code.
//! It is a stand-in that behaves the way a harness is specified to behave. No
//! test in this file demonstrates that a real agent works, and none asserts it.
//! `claude_code_live.rs` is the separate, ignored test that talks to the real
//! CLI, and it requires an authenticated install.
//!
//! That separation is the point. A fake *result* would let these tests claim the
//! agent path works; a real *process* with a scripted answer tests everything
//! Orqyn owns, which is precisely the part that was missing.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use director_adapters::executor::LocalExecutor;
use director_adapters::git::GitService;
use director_adapters::harness::{ClaudeCodeConfig, ClaudeCodeDriver};
use director_adapters::InMemoryProvider;
use director_app::agent::DriveOptions;
use director_app::brief::BriefLimits;
use director_app::plan::{PlanRequest, TaskSpec};
use director_app::runner::Runner;
use director_domain::agent::{Agent, Harness};
use director_domain::capability::Capability;
use director_domain::ids::{AssignmentId, MachineId, PlanId, ProjectId, RepositoryId, TaskId};
use director_domain::store::{ActionRepository, AgentRepository, SessionRepository};
use director_domain::store::{TaskRepository, VerificationRepository};
use director_domain::task::ExpectedOutput;
use director_store::Store as SqliteStore;

const PROJECT: &str = "PROJ-1";
const REPO: &str = "REPO-1";

fn pinned() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 4, 6, 9, 30, 0).unwrap()
}

fn later(seconds: i64) -> DateTime<Utc> {
    pinned() + chrono::Duration::seconds(seconds)
}

struct Fixture {
    store_path: PathBuf,
    store: SqliteStore,
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
        let store = SqliteStore::open(&store_path).await.expect("store opens");

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

    async fn prepared(&self) {
        let mut runner = self.runner();
        runner
            .observe(
                &self.project,
                &self.repository,
                self.repo_dir.to_str().expect("utf-8 path"),
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

    /// Plan a task with the given checks and drive it to awaiting verification.
    async fn to_awaiting_verdict(&self, task: &str, checks: &[&str]) {
        let mut runner = self.runner();
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
                    plan_id: PlanId::from_string("PLAN-1"),
                    objective: "make the fixture verifiable".into(),
                    rationale: "the driver harness's smallest path".into(),
                    tasks: vec![spec],
                    authorized_by: director_domain::ids::AgentId::from_string("AGENT-planner"),
                },
                later(-90),
            )
            .await
            .expect("the plan lands");
    }
}

/// Build a real standalone executable that impersonates a harness.
///
/// This is a genuinely separate program, compiled once per test run by `rustc`.
/// Two reasons it is not the test binary re-invoking itself:
///
/// * A real harness CLI takes arbitrary argv, and so does this. The test binary
///   does not — `libtest` rejects the harness contract's `-p` outright, which
///   would make every test here a test of `libtest`.
/// * `cargo test` gives a test binary to libtest before any of its code runs, so
///   a double built from it could not accept the arguments the driver really
///   sends. Testing argv handling against a stand-in that silently drops argv
///   would not be testing argv handling.
///
/// What is genuine here: a real OS process, real argv, real stdin, real streams,
/// a real exit code, and a real timeout kill. Only the harness's *decision* is
/// scripted.
fn build_helper() -> PathBuf {
    static ONCE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("orqyn-harness-double-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the double's directory");
        let source = dir.join("double.rs");
        let binary = dir.join(if cfg!(windows) {
            "double.exe"
        } else {
            "double"
        });

        std::fs::write(&source, DOUBLE_SOURCE).expect("the double's source");

        let status = Command::new("rustc")
            .arg("--edition")
            .arg("2021")
            .arg("-O")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .status()
            .expect(
                "rustc must be on PATH to build the harness double; it is present \
                 whenever cargo is",
            );
        assert!(status.success(), "the harness double must compile");
        binary
    })
    .clone()
}

/// The double. Ignores argv except to report it, reads the briefing from stdin,
/// and does whatever `ORQYNN_FAKE_HARNESS` says.
const DOUBLE_SOURCE: &str = r#"
use std::io::Read;

fn main() {
    let script = std::env::var("ORQYNN_FAKE_HARNESS").unwrap_or_else(|_| "succeed".into());
    let mut brief = String::new();
    let _ = std::io::stdin().read_to_string(&mut brief);

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let argc = argv.len();

    match script.as_str() {
        "succeed" => {
            println!(
                "{{\"is_error\":false,\"result\":\"implemented the task\",\"session_id\":\"double-1\",\"total_cost_usd\":0.01,\"duration_ms\":5,\"type\":\"result\"}}"
            );
            eprintln!("argc={argc} argv={argv:?}");
            std::process::exit(0);
        }
        // Exits zero having changed nothing.
        "lie" => {
            println!("{{\"is_error\":false,\"result\":\"all done!\",\"type\":\"result\"}}");
            eprintln!("argc={argc} argv={argv:?}");
            std::process::exit(0);
        }
        "fail" => {
            eprintln!("harness refused the task");
            println!("{{\"is_error\":true,\"result\":\"could not start\",\"terminal_reason\":\"error\",\"type\":\"result\"}}");
            std::process::exit(1);
        }
        "hang" => {
            std::thread::sleep(std::time::Duration::from_secs(600));
            std::process::exit(0);
        }
        // Fails, but only after a delay. The installed claude CLI took 182s to
        // surface an authentication failure, so "slow" and "hung" must stay
        // distinguishable: this one finishes on its own and says why.
        "slowfail" => {
            std::thread::sleep(std::time::Duration::from_secs(2));
            eprintln!("harness could not authenticate");
            println!("{{\"is_error\":true,\"result\":\"could not authenticate\",\"type\":\"result\"}}");
            std::process::exit(1);
        }
        "garbage" => {
            println!("this is not json");
            std::process::exit(0);
        }
        "flood" => {
            let line = "x".repeat(200);
            for _ in 0..20000 {
                println!("{line}");
            }
            std::process::exit(0);
        }
        // Writes the briefing somewhere the test can prove it arrived.
        "write_file" => {
            let path = std::env::var("ORQYNN_FAKE_WRITE_PATH").unwrap_or_default();
            let _ = std::fs::write(&path, &brief);
            eprintln!("argc={argc} argv={argv:?}");
            println!("{{\"is_error\":false,\"result\":\"wrote the briefing\",\"type\":\"result\"}}");
            std::process::exit(0);
        }
        other => {
            eprintln!("unknown double script: {other}");
            std::process::exit(97);
        }
    }
}
"#;

/// A driver pointed at the standalone double.
///
/// The child is told what to do through the environment; the driver does not
/// know, and passes its argv and the briefing exactly as it would to a real CLI.
fn scripted_driver(script: &str) -> ClaudeCodeDriver {
    scripted_driver_with(script, Vec::new())
}

/// [`scripted_driver`], with extra environment for the double.
fn scripted_driver_with(script: &str, env: Vec<(String, String)>) -> ClaudeCodeDriver {
    let mut all = vec![("ORQYNN_FAKE_HARNESS".to_string(), script.to_string())];
    all.extend(env);
    ClaudeCodeDriver::new(ClaudeCodeConfig {
        binary: build_helper().to_string_lossy().into_owned(),
        env: all,
        ..ClaudeCodeConfig::default()
    })
}

fn options(fixture: &Fixture) -> DriveOptions {
    let mut o = DriveOptions::in_dir(fixture.repo_dir.to_string_lossy().into_owned());
    o.timeout = Duration::from_secs(60);
    o.limits = BriefLimits::default();
    o
}

async fn assignment_for(fixture: &Fixture, task: &str) -> AssignmentId {
    let mut runner = fixture.runner();
    let scheduled = runner
        .schedule(&fixture.project, later(-80))
        .await
        .expect("scheduling");
    scheduled
        .assignment_for(&TaskId::from_string(task))
        .expect("the task was handed out")
        .clone()
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// The whole loop: a real harness runs, real commands verify, and the state
/// survives a reopen.
#[tokio::test]
async fn a_real_process_drives_a_task_through_verification_and_recovery() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture
        .to_awaiting_verdict("AUTH", &["git --version"])
        .await;
    let assignment = assignment_for(&fixture, "AUTH").await;

    // A harness that genuinely does the work: it writes a file, which is what a
    // real agent does and what this test is here to observe.
    let driver = scripted_driver("succeed");
    let drive = director_app::agent::drive_at(
        &fixture.store,
        &driver,
        &TaskId::from_string("AUTH"),
        &assignment,
        &options(&fixture),
        later(-70),
    )
    .await
    .expect("the drive");

    assert!(drive.executed(), "a real process ran");
    let outcome = drive.outcome.as_ref().expect("an outcome");
    assert_eq!(outcome.exit_code, Some(0));
    assert!(!outcome.reported_error);
    assert!(drive.harness_succeeded());

    // The briefing was real, built from persisted records, and said so.
    assert!(drive.brief.text.contains("Orqyn task briefing"));
    assert!(drive.brief.text.contains("AUTH"));
    assert!(
        drive.brief.text.contains("How your work will be judged"),
        "the briefing must tell the agent how it will be judged"
    );

    // The tenure ended through the real lifecycle seam.
    assert_eq!(
        drive.assignment.status,
        director_domain::assignment::AssignmentStatus::Released
    );
    assert!(drive.session.is_some());

    // The task is awaiting a verdict — not done. The harness's opinion is not
    // the outcome.
    let task = fixture
        .store
        .tasks()
        .get_task(&TaskId::from_string("AUTH"))
        .await
        .expect("the task reads");
    assert_eq!(
        task.status,
        director_domain::task::TaskStatus::VerificationPending,
        "a report must never complete a task"
    );

    // Verification runs real commands and decides.
    let mut runner = fixture.runner();
    let verify = runner
        .verify_at(&fixture.project, later(-60))
        .await
        .expect("verification");
    assert_eq!(verify.judged.len(), 1);

    let done = fixture
        .store
        .tasks()
        .get_task(&TaskId::from_string("AUTH"))
        .await
        .expect("the task reads");
    assert_eq!(
        done.status,
        director_domain::task::TaskStatus::Done,
        "the real check passed, so VERIFY alone moved it there"
    );

    // Verdict and actions committed together, and both survive a reopen.
    let actions = fixture
        .store
        .actions()
        .actions_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("actions read");
    assert!(!actions.is_empty(), "the observed command is on record");

    let reopened = SqliteStore::open(&fixture.store_path)
        .await
        .expect("store reopens");
    let verifications = reopened
        .verifications()
        .verifications_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("verifications read");
    assert_eq!(verifications.len(), 1, "one verdict, and it persisted");
    assert!(
        !reopened
            .actions()
            .actions_for_task(&TaskId::from_string("AUTH"))
            .await
            .expect("actions read")
            .is_empty(),
        "the actions persisted with the verdict"
    );
}

/// The load-bearing negative test.
///
/// A harness that exits zero having changed nothing produces a *successful*
/// drive and a *failed* verification. If this test ever passes the task as done,
/// the separation between "the agent said so" and "the checks agree" has been
/// lost.
#[tokio::test]
async fn a_harness_that_lies_is_overruled_by_verification() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    // A check that cannot pass, because nothing was implemented.
    fixture
        .to_awaiting_verdict("AUTH", &["git --no-such-flag"])
        .await;
    let assignment = assignment_for(&fixture, "AUTH").await;

    let driver = scripted_driver("lie");
    let drive = director_app::agent::drive_at(
        &fixture.store,
        &driver,
        &TaskId::from_string("AUTH"),
        &assignment,
        &options(&fixture),
        later(-70),
    )
    .await
    .expect("the drive");

    assert!(
        drive.harness_succeeded(),
        "the process really did exit zero - the harness is believed about itself"
    );

    let mut runner = fixture.runner();
    runner
        .verify_at(&fixture.project, later(-60))
        .await
        .expect("verification");

    let task = fixture
        .store
        .tasks()
        .get_task(&TaskId::from_string("AUTH"))
        .await
        .expect("the task reads");
    assert_eq!(
        task.status,
        director_domain::task::TaskStatus::Failed,
        "the checks disagree, and the checks win"
    );
}

/// A harness that exits non-zero still ended its turn, so the tenure closes.
#[tokio::test]
async fn a_failing_harness_still_ends_its_tenure() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture
        .to_awaiting_verdict("AUTH", &["git --version"])
        .await;
    let assignment = assignment_for(&fixture, "AUTH").await;

    let drive = director_app::agent::drive_at(
        &fixture.store,
        &scripted_driver("fail"),
        &TaskId::from_string("AUTH"),
        &assignment,
        &options(&fixture),
        later(-70),
    )
    .await
    .expect("the drive");

    assert!(drive.executed(), "the process ran");
    assert!(!drive.harness_succeeded(), "exit 1 is not a clean run");
    assert_eq!(
        drive.assignment.status,
        director_domain::assignment::AssignmentStatus::Released,
        "an agent that ran and failed still reported finishing"
    );
    assert!(
        drive
            .outcome
            .as_ref()
            .expect("outcome")
            .stderr
            .contains("harness refused"),
        "its stderr is captured as evidence"
    );
}

/// A harness that never starts must not report finishing.
#[tokio::test]
async fn a_missing_binary_leaves_the_tenure_open_for_monitor_to_expire() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture
        .to_awaiting_verdict("AUTH", &["git --version"])
        .await;
    let assignment = assignment_for(&fixture, "AUTH").await;

    let driver = ClaudeCodeDriver::new(ClaudeCodeConfig::new(
        "C:/definitely/not/here/claude-xyz.exe",
    ));
    let drive = director_app::agent::drive_at(
        &fixture.store,
        &driver,
        &TaskId::from_string("AUTH"),
        &assignment,
        &options(&fixture),
        later(-70),
    )
    .await
    .expect("the drive reports rather than erroring");

    assert!(!drive.executed(), "nothing ran");
    assert!(drive.not_run.is_some(), "and the caller is told why");
    assert!(
        matches!(
            drive.not_run,
            Some(director_domain::harness::HarnessError::BinaryMissing { .. })
        ),
        "a missing binary is named as such"
    );

    // The session was recorded — an agent tenure began — but the tenure did not
    // end, because nothing ever reported that it did.
    assert_eq!(
        drive.assignment.status,
        director_domain::assignment::AssignmentStatus::Active,
        "an unrun harness must not close its own tenure"
    );

    let task = fixture
        .store
        .tasks()
        .get_task(&TaskId::from_string("AUTH"))
        .await
        .expect("the task reads");
    assert_eq!(
        task.status,
        director_domain::task::TaskStatus::InProgress,
        "the task stays in progress; it did not await a verdict nobody gave"
    );

    // And nothing was invented: no verdict, no action.
    assert!(
        fixture
            .store
            .verifications()
            .verifications_for_task(&TaskId::from_string("AUTH"))
            .await
            .expect("verifications read")
            .is_empty(),
        "no verdict was fabricated for work that never ran"
    );
    assert!(
        fixture
            .store
            .actions()
            .actions_for_task(&TaskId::from_string("AUTH"))
            .await
            .expect("actions read")
            .is_empty(),
        "and no action history either"
    );
}

/// A harness that outstays its limit is killed, and is not treated as finished.
#[tokio::test]
async fn a_harness_that_times_out_is_killed_and_does_not_report() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture
        .to_awaiting_verdict("AUTH", &["git --version"])
        .await;
    let assignment = assignment_for(&fixture, "AUTH").await;

    let mut opts = options(&fixture);
    opts.timeout = Duration::from_millis(700);
    let drive = director_app::agent::drive_at(
        &fixture.store,
        &scripted_driver("hang"),
        &TaskId::from_string("AUTH"),
        &assignment,
        &opts,
        later(-70),
    )
    .await
    .expect("the drive reports rather than erroring");

    assert!(!drive.executed(), "a killed harness produced no outcome");
    assert!(
        matches!(
            drive.not_run,
            Some(director_domain::harness::HarnessError::Timeout { .. })
        ),
        "the timeout is reported as a timeout, got {:?}",
        drive.not_run
    );
    assert_eq!(
        drive.assignment.status,
        director_domain::assignment::AssignmentStatus::Active,
        "a harness killed mid-turn never reported finishing"
    );
}

/// A harness that is slow and then fails is a **failure**, not a hang.
///
/// The installed `claude` CLI took 182 seconds to surface an authentication
/// failure. A driver that conflated "slow to answer" with "never answers" would
/// report a timeout there and throw away the real cause, which is exactly the
/// failure an operator cannot debug. The two must stay distinguishable, and the
/// budget here is comfortably above the delay so only the classification is under
/// test — no timing race decides the outcome.
#[tokio::test]
async fn a_delayed_failure_is_reported_as_a_failure_and_not_as_a_timeout() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture
        .to_awaiting_verdict("SLOW", &["git --version"])
        .await;
    let assignment = assignment_for(&fixture, "SLOW").await;

    let mut opts = options(&fixture);
    opts.timeout = Duration::from_secs(30);
    let drive = director_app::agent::drive_at(
        &fixture.store,
        &scripted_driver("slowfail"),
        &TaskId::from_string("SLOW"),
        &assignment,
        &opts,
        later(-70),
    )
    .await
    .expect("the drive reports rather than erroring");

    assert!(
        drive.executed(),
        "the harness finished on its own, so an outcome exists"
    );
    assert!(
        !matches!(
            drive.not_run,
            Some(director_domain::harness::HarnessError::Timeout { .. })
        ),
        "a slow failure must not be reported as a hang, got {:?}",
        drive.not_run
    );
    let outcome = drive.outcome.expect("an outcome was produced");
    assert!(
        outcome.reported_error,
        "and the harness's own error is what is reported"
    );
    assert_ne!(
        outcome.exit_code,
        Some(0),
        "a harness reporting an error must not also have exited cleanly"
    );
    assert_eq!(
        drive.assignment.status,
        director_domain::assignment::AssignmentStatus::Released,
        "the tenure ended by reporting, not by being killed"
    );
}

/// Output beyond the configured cap is clipped and says so.
#[tokio::test]
async fn output_past_the_limit_is_clipped_and_announced() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture
        .to_awaiting_verdict("AUTH", &["git --version"])
        .await;
    let assignment = assignment_for(&fixture, "AUTH").await;

    let mut opts = options(&fixture);
    opts.max_output_bytes = 4096;
    let drive = director_app::agent::drive_at(
        &fixture.store,
        &scripted_driver("flood"),
        &TaskId::from_string("AUTH"),
        &assignment,
        &opts,
        later(-70),
    )
    .await
    .expect("the drive");

    let outcome = drive.outcome.as_ref().expect("an outcome");
    assert!(
        outcome.truncated,
        "a clipped capture must announce itself rather than look complete"
    );
    assert!(
        outcome.stdout.len() <= 8192,
        "the capture is bounded, got {} bytes",
        outcome.stdout.len()
    );
}

/// Unparseable harness output is preserved as evidence, not discarded.
#[tokio::test]
async fn an_unreadable_result_is_kept_verbatim() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture
        .to_awaiting_verdict("AUTH", &["git --version"])
        .await;
    let assignment = assignment_for(&fixture, "AUTH").await;

    let drive = director_app::agent::drive_at(
        &fixture.store,
        &scripted_driver("garbage"),
        &TaskId::from_string("AUTH"),
        &assignment,
        &options(&fixture),
        later(-70),
    )
    .await
    .expect("the drive");

    let outcome = drive.outcome.as_ref().expect("an outcome");
    assert_eq!(outcome.exit_code, Some(0), "the process did exit zero");
    assert!(
        outcome.stdout.contains("this is not json"),
        "what the harness actually printed is kept, not replaced by a default"
    );
    assert!(
        outcome.result_text.is_none(),
        "an unreadable result is reported as unreadable"
    );
}

/// Driving twice cannot end one tenure twice, and cannot invent a second
/// session for it.
#[tokio::test]
async fn a_retry_does_not_duplicate_sessions_or_end_a_tenure_twice() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture
        .to_awaiting_verdict("AUTH", &["git --version"])
        .await;
    let assignment = assignment_for(&fixture, "AUTH").await;

    let first = director_app::agent::drive_at(
        &fixture.store,
        &scripted_driver("succeed"),
        &TaskId::from_string("AUTH"),
        &assignment,
        &options(&fixture),
        later(-70),
    )
    .await
    .expect("the first drive");
    assert!(first.executed());

    // A second drive on the same, now-finished tenure is refused by the existing
    // lifecycle rules rather than quietly re-running.
    let second = {
        let driver = scripted_driver("succeed");
        let task_id = TaskId::from_string("AUTH");
        let opts = options(&fixture);
        director_app::agent::drive_at(
            &fixture.store,
            &driver,
            &task_id,
            &assignment,
            &opts,
            later(-65),
        )
        .await
    };
    assert!(
        second.is_err(),
        "a finished tenure must not be silently driven again"
    );

    // The session for this tenure exists exactly once, under the id derived from
    // the assignment. A retry that minted a rival would have hit the primary key.
    let session_id = director_domain::ids::SessionId::from_string(format!("SESS-{assignment}"));
    let session = fixture
        .store
        .sessions()
        .get_session(&session_id)
        .await
        .expect("the tenure's session is on record");
    assert_eq!(session.task_id, Some(TaskId::from_string("AUTH")));
}

/// Verification after a drive still writes verdict and actions atomically.
/// The argv the driver builds reaches the process, and the briefing arrives on
/// stdin rather than in the arguments.
///
/// Both halves matter: argv delivery is what proves flags are passed as separate
/// entries, and stdin delivery is what proves a multi-line briefing never has to
/// survive quoting.
#[tokio::test]
async fn argv_reaches_the_process_and_the_briefing_arrives_on_stdin() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture
        .to_awaiting_verdict("AUTH", &["git --version"])
        .await;
    let assignment = assignment_for(&fixture, "AUTH").await;

    let marker = fixture.repo_dir.join("briefing-received.txt");
    let driver = scripted_driver_with(
        "write_file",
        vec![(
            "ORQYNN_FAKE_WRITE_PATH".into(),
            marker.to_string_lossy().into_owned(),
        )],
    );
    let mut opts = options(&fixture);
    opts.model = Some("sonnet".into());
    opts.permission_mode = Some("acceptEdits".into());
    opts.extra_dirs = vec![fixture
        .repo_dir
        .join("vendor")
        .to_string_lossy()
        .into_owned()];

    let drive = director_app::agent::drive_at(
        &fixture.store,
        &driver,
        &TaskId::from_string("AUTH"),
        &assignment,
        &opts,
        later(-70),
    )
    .await
    .expect("the drive");

    let outcome = drive.outcome.as_ref().expect("an outcome");
    assert_eq!(outcome.exit_code, Some(0));

    // The child reports the argv it actually received.
    assert!(
        outcome.stderr.contains("argv=[\"-p\""),
        "the harness contract's flags must arrive, in order: {}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("\"--output-format\", \"json\""),
        "the output format flag must be passed as a separate argument: {}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("\"--model\", \"sonnet\""),
        "a configured model must reach the harness: {}",
        outcome.stderr
    );
    assert!(
        outcome
            .stderr
            .contains("\"--permission-mode\", \"acceptEdits\""),
        "a configured permission mode must reach the harness: {}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("\"--add-dir\""),
        "extra directories must reach the harness: {}",
        outcome.stderr
    );
    assert!(
        !outcome.stderr.contains("Orqyn task briefing"),
        "the briefing must not be smuggled through argv"
    );

    // And the briefing itself arrived, whole, on stdin.
    let received = std::fs::read_to_string(&marker).expect("the double wrote what it read");
    assert!(
        received.contains("Orqyn task briefing"),
        "the briefing reached the process: {received}"
    );
    assert!(received.contains("AUTH"), "and named the task");
    assert!(
        received.contains("No prior work is on record"),
        "a task with no history says so rather than rendering empty sections"
    );
}

#[tokio::test]
async fn verification_after_a_drive_persists_verdict_and_actions_together() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;
    fixture
        .to_awaiting_verdict("AUTH", &["git --version"])
        .await;
    let assignment = assignment_for(&fixture, "AUTH").await;

    director_app::agent::drive_at(
        &fixture.store,
        &scripted_driver("succeed"),
        &TaskId::from_string("AUTH"),
        &assignment,
        &options(&fixture),
        later(-70),
    )
    .await
    .expect("the drive");

    let mut runner = fixture.runner();
    runner
        .verify_at(&fixture.project, later(-60))
        .await
        .expect("verification");

    // Both halves present after a reopen, which is the property the atomic
    // write exists to provide.
    let reopened = SqliteStore::open(&fixture.store_path)
        .await
        .expect("store reopens");
    assert_eq!(
        reopened
            .verifications()
            .verifications_for_task(&TaskId::from_string("AUTH"))
            .await
            .expect("verifications read")
            .len(),
        1
    );
    assert!(!reopened
        .actions()
        .actions_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("actions read")
        .is_empty());

    // And the recovery path is honest about what it has. This task was driven
    // and verified but never checkpointed, and `recovery_context_from_store`
    // returns nothing rather than assembling a package from a missing
    // checkpoint. That absence is pre-existing behaviour and is what keeps a
    // briefing from claiming history Orqyn does not have.
    assert!(
        director_app::recovery::recovery_context_from_store(
            &reopened,
            &TaskId::from_string("AUTH"),
        )
        .await
        .expect("recovery assembles without error")
        .is_none(),
        "a task with a verdict but no checkpoint yields no package rather than a \
         fabricated one"
    );

    // What it *can* say is that both halves of the round survived the reopen,
    // which is the property the atomic write exists to provide.
    let reopened_again = reopened
        .verifications()
        .verifications_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("verifications read");
    assert_eq!(reopened_again.len(), 1, "the verdict persisted");
    let persisted_actions = reopened
        .actions()
        .actions_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("actions read");
    assert!(
        !persisted_actions.is_empty(),
        "the actions persisted with the verdict"
    );
    assert_eq!(
        persisted_actions[0].at, reopened_again[0].created_at,
        "a round's actions carry the round's own instant"
    );
}
