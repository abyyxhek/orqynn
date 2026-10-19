//! The CLI's contract: parsing, validation, safety defaults, and the lifecycle.
//!
//! # What is real, and what is not
//!
//! **Real:** argument parsing, path validation, SQLite on disk, the whole
//! lifecycle through `director-app`, a genuinely spawned child process, real
//! verification commands run by the existing engine, and the resulting exit
//! codes.
//!
//! **Not claimed:** that a real agent works. The orchestration tests drive the
//! deterministic local harness fixture — a real program, but not Claude Code. The
//! real CLI's only claim about Claude Code is that it passes the binary through;
//! that is checked by asserting on the configuration it builds, not by pretending
//! a fake agent is one.
//!
//! # The harness fixture
//!
//! `orqynn` builds its driver as a [`ClaudeCodeDriver`], because that is what it
//! must do in production — the argv contract is verified in `director-adapters`.
//! For tests, the same driver type is pointed at a locally compiled fixture that
//! speaks the same protocol, which keeps the code under test identical to the
//! code that ships. Its behaviour is chosen by an environment variable, so a test
//! can make the agent succeed, fail, or vanish without mocking anything.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

// The CLI's own modules live in the binary crate, so the tests compile the very
// same sources `main` uses, under the module paths the sources themselves expect.
//
// Each is allowed to keep items `main` uses and these tests do not (`ProcessEnv`,
// `claude_driver`, `CliMemory`), because the binary target is what exercises those
// and removing them here would fork the code under test.
#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/lifecycle.rs"]
mod lifecycle;

use crate::config as cli_config;
use crate::lifecycle as cli_lifecycle;

use cli_config::{derive_task_id, ConfigError, MapEnv, RunArgs};
use cli_lifecycle::ExitCode;

use director_domain::{ActionRepository, TaskRepository, VerificationRepository};

// ---------------------------------------------------------------------------
// The harness fixture
// ---------------------------------------------------------------------------

/// A compiled stand-in for an agent binary.
///
/// Speaks the same stdio contract the driver expects — reads the briefing from
/// stdin, writes one result object to stdout, exits with a code — and does
/// whatever `ORQYNN_FAKE_AGENT` says. It is a real program on every test's PATH
/// equivalent: the driver spawns it and observes it exactly as it would observe
/// Claude Code.
fn fixture_binary() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("orqynn-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the fixture directory");
        let source = dir.join("agent.rs");
        let binary = dir.join(if cfg!(windows) { "agent.exe" } else { "agent" });
        std::fs::write(&source, FIXTURE_SOURCE).expect("the fixture source");

        let status = Command::new("rustc")
            .arg("--edition")
            .arg("2021")
            .arg("-O")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .status()
            .expect("rustc must be on PATH; it is present whenever cargo is");
        assert!(status.success(), "the agent fixture must compile");
        binary
    })
    .clone()
}

/// Reads the briefing from stdin, then does what the script says.
///
/// `succeed` — writes a result object and exits 0, having made no change. The
/// verification checks, not this, decide whether the task is done.
/// `fail` — exits 1 with a message, as a harness that cannot start would.
/// `vanish` — exits 0 without a result object, so the driver sees nothing usable.
/// `hang` — outlives any budget a test gives it, so a timeout is produced by the
///             clock rather than by winning a race against a spawn.
const FIXTURE_SOURCE: &str = r#"
use std::io::Read;

fn main() {
    let mut brief = String::new();
    let _ = std::io::stdin().read_to_string(&mut brief);

    match std::env::var("ORQYNN_FAKE_AGENT").unwrap_or_else(|_| "succeed".into()).as_str() {
        "fail" => {
            eprintln!("the fixture agent refused the task");
            println!("{{\"is_error\":true,\"result\":\"could not start\",\"type\":\"result\"}}");
            std::process::exit(1);
        }
        "vanish" => {
            std::process::exit(0);
        }
        "hang" => {
            std::thread::sleep(std::time::Duration::from_secs(120));
            std::process::exit(0);
        }
        _ => {
            println!("{{\"is_error\":false,\"result\":\"implemented the task\",\"type\":\"result\"}}");
            std::process::exit(0);
        }
    }
}
"#;

/// A driver over the fixture, behaving as the given script.
fn driver_for(script: &str, timeout: Duration) -> director_adapters::harness::ClaudeCodeDriver {
    director_adapters::harness::ClaudeCodeDriver::new(
        director_adapters::harness::ClaudeCodeConfig {
            binary: fixture_binary().to_string_lossy().into_owned(),
            default_timeout: timeout,
            env: vec![("ORQYNN_FAKE_AGENT".into(), script.into())],
            ..director_adapters::harness::ClaudeCodeConfig::default()
        },
    )
}

/// A scratch git repository, because `observe` reads real git state.
fn git_repo(dir: &Path) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git must be on PATH");
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

/// A scratch repository plus resolved arguments pointing at it.
struct Cli {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    args: RunArgs,
}

impl Cli {
    fn new(checks: &[&str]) -> Self {
        let tmp = tempfile::TempDir::new().expect("a temp dir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        git_repo(&repo);

        let mut args = RunArgs::parse([
            "--repo",
            repo.to_str().expect("utf-8"),
            "--task",
            "make the greeting say hello",
        ])
        .expect("the base invocation parses");
        for check in checks {
            args.checks.push((*check).to_string());
        }
        args.agent_binary = fixture_binary().to_string_lossy().into_owned();
        args.resolve().expect("the base invocation resolves");
        Cli {
            _tmp: tmp,
            repo,
            args,
        }
    }

    async fn run(
        &self,
        script: &str,
        timeout: Duration,
    ) -> Result<cli_lifecycle::RunReport, cli_lifecycle::RunError> {
        let runtime = cli_lifecycle::open_runtime(&self.args)
            .await
            .expect("the runtime opens");
        let memory = director_adapters::InMemoryProvider::new();
        let driver = driver_for(script, timeout);
        cli_lifecycle::run_lifecycle(
            &runtime,
            &memory,
            &driver,
            &director_adapters::executor::LocalExecutor::default(),
            &self.args,
            chrono::Utc::now(),
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// 1. Argument parsing
// ---------------------------------------------------------------------------

#[test]
fn the_documented_invocation_parses() {
    let args = RunArgs::parse([
        "orqynn",
        "run",
        "--repo",
        "/tmp/repo",
        "--task",
        "add a greeting",
        "--check",
        "cargo test",
        "--check",
        "cargo fmt --check",
    ])
    .expect("parses");
    assert_eq!(args.repo, PathBuf::from("/tmp/repo"));
    assert_eq!(args.task, "add a greeting");
    assert_eq!(
        args.checks,
        vec!["cargo test".to_string(), "cargo fmt --check".to_string()],
        "a check may be repeated, because one flag per command is the only way to say several"
    );
}

#[test]
fn flags_may_be_written_with_an_equals() {
    let args = RunArgs::parse([
        "--repo=/tmp/repo",
        "--task=do a thing",
        "--check=cargo test",
    ])
    .expect("parses");
    assert_eq!(args.repo, PathBuf::from("/tmp/repo"));
    assert_eq!(args.task, "do a thing");
}

#[test]
fn a_test_may_omit_the_program_name() {
    // The parser does not require a program name, so a test can pass flags alone.
    let args = RunArgs::parse(["--repo", "/tmp/repo"]).expect("parses");
    assert_eq!(args.repo, PathBuf::from("/tmp/repo"));
}

#[test]
fn an_unrecognised_option_is_refused_rather_than_ignored() {
    // A silently dropped `--chek` would remove a verification check and make the
    // run look more trustworthy than it is.
    let err = RunArgs::parse(["--repo", "/tmp/r", "--chek", "cargo test"])
        .expect_err("an unknown flag must not be ignored");
    assert_eq!(err, ConfigError::Unknown("--chek".into()));
}

#[test]
fn a_positional_argument_is_refused() {
    let err =
        RunArgs::parse(["--repo", "/tmp/r", "extra"]).expect_err("every input is a named option");
    assert!(matches!(err, ConfigError::Unexpected(_)));
}

#[test]
fn a_flag_given_twice_is_refused_when_it_may_appear_once() {
    assert_eq!(
        RunArgs::parse(["--repo", "/a", "--repo", "/b"]).expect_err("repeated"),
        ConfigError::Repeated { flag: "--repo" }
    );
    // And `--check` is not in that set.
    assert!(RunArgs::parse(["--repo", "/a", "--check", "x", "--check", "y"]).is_ok());
}

#[test]
fn a_flag_with_no_value_is_refused() {
    assert!(matches!(
        RunArgs::parse(["--repo"]).expect_err("no value"),
        ConfigError::EmptyValue { flag: "--repo" }
    ));
    assert!(matches!(
        RunArgs::parse(["--repo="]).expect_err("empty value"),
        ConfigError::EmptyValue { flag: "--repo" }
    ));
}

#[test]
fn a_zero_timeout_is_refused() {
    assert!(matches!(
        RunArgs::parse(["--repo", "/a", "--agent-timeout", "0"]).expect_err("zero"),
        ConfigError::OutOfRange { .. }
    ));
    assert!(matches!(
        RunArgs::parse(["--repo", "/a", "--memory-timeout", "soon"]).expect_err("not a number"),
        ConfigError::OutOfRange { .. }
    ));
}

// ---------------------------------------------------------------------------
// 2 & 3. Required fields, invalid paths, no secrets
// ---------------------------------------------------------------------------

#[test]
fn required_fields_are_named_rather_than_guessed() {
    let mut args = RunArgs::parse(["--task", "something"]).expect("parses");
    assert_eq!(
        args.resolve().expect_err("no repo"),
        ConfigError::Missing("--repo")
    );

    let mut args = RunArgs::parse(["--repo", "/tmp/r"]).expect("parses");
    assert_eq!(
        args.resolve().expect_err("no task"),
        ConfigError::Missing("--task")
    );
}

#[test]
fn a_run_without_a_verification_check_is_refused() {
    let mut args = RunArgs::parse(["--repo", "/tmp/r", "--task", "do it"]).expect("parses");
    let err = args.resolve().expect_err("no check given");
    assert!(
        matches!(err, ConfigError::NoVerificationChecks),
        "a task with no checks can never be verified, so a run without one cannot claim success: {err}"
    );
    // A dry run is exempt, because it will not verify anything.
    let cli = Cli::new(&["git --version"]);
    let mut dry = cli.args.clone();
    dry.checks.clear();
    dry.dry_run = true;
    dry.resolve()
        .expect("a dry run resolves without a check, because it verifies nothing");
    assert!(dry.checks.is_empty());
}

#[test]
fn a_missing_repository_is_refused() {
    let cli = Cli::new(&["git --version"]);
    let mut args = cli.args.clone();
    args.repo = PathBuf::from("C:/definitely/not/here");
    assert!(matches!(
        args.validate_paths().expect_err("no such directory"),
        ConfigError::NotADirectory { .. }
    ));
}

#[test]
fn a_directory_that_is_not_a_repository_is_refused() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let mut args = RunArgs::parse(["--repo", tmp.path().to_str().expect("utf-8")]).expect("parses");
    args.agent_binary = fixture_binary().to_string_lossy().into_owned();
    let err = args.validate_paths().expect_err("not a repository");
    assert!(
        matches!(err, ConfigError::NotAGitRepository { .. }),
        "Orqyn observes real git state and will not invent it: {err}"
    );
}

#[test]
fn an_unusable_database_location_is_refused() {
    let cli = Cli::new(&["git --version"]);
    let mut args = cli.args.clone();
    // A path under a file, not a directory: nothing can be created there.
    args.database = cli.repo.join("README.md").join("nested.db");
    assert!(
        matches!(
            args.validate_paths().expect_err("unusable"),
            ConfigError::UnusableDatabase { .. }
        ),
        "a database under a regular file must be refused"
    );
}

#[test]
fn no_configuration_error_can_print_a_secret() {
    // Every message Orqyn builds is assembled from flag names and the paths it
    // was given. A future field carrying a credential would show up here, so the
    // cases are the ones the parser and resolver actually produce.
    let cases: Vec<ConfigError> = vec![
        ConfigError::Missing("--repo"),
        ConfigError::Missing("--task"),
        ConfigError::Unknown("--secret".into()),
        ConfigError::EmptyValue { flag: "--db" },
        ConfigError::Repeated { flag: "--task" },
        ConfigError::NoVerificationChecks,
        ConfigError::MemoryNotConfigured,
        ConfigError::OutOfRange {
            flag: "--agent-timeout",
            expected: "greater than zero seconds",
        },
        ConfigError::UnusableDatabase {
            path: "C:/db".into(),
            problem: "not writable".into(),
        },
    ];
    for case in cases {
        let text = case.to_string();
        assert!(!text.contains("sk-"), "no error may carry a token: {text}");
        assert!(
            !text.to_lowercase().contains("api_key"),
            "no error may name a credential: {text}"
        );
    }

    // And a real invocation with a credential in the environment still yields an
    // error that mentions only the flag that was actually wrong.
    let env = MapEnv(vec![
        ("ANTHROPIC_API_KEY".into(), "sk-secret".into()),
        ("ORQYNN_REPO".into(), "C:/nope".into()),
    ]);
    let mut args = RunArgs::from_env_and_args(["run", "--task", "x"], &env).expect("parses");
    let err = args.resolve().expect_err("the repository does not exist");
    let text = err.to_string();
    assert!(
        !text.contains("sk-secret"),
        "no error may carry a token: {text}"
    );
}

#[test]
fn provenance_reports_where_a_value_came_from_and_not_what_it_was() {
    let cli = Cli::new(&["git --version"]);
    let text = cli.args.provenance().join("\n");
    assert!(text.contains("repository:"));
    assert!(text.contains("permission mode:"));
    assert!(
        text.contains("will not choose a permissive one"),
        "the safety default must be visible, not just true"
    );
}

// ---------------------------------------------------------------------------
// 4 & 5. Environment, memory disabled, missing agent
// ---------------------------------------------------------------------------

#[test]
fn the_environment_fills_gaps_and_arguments_win() {
    let env = MapEnv(vec![
        ("ORQYNN_REPO".into(), "/from/env".into()),
        ("ORQYNN_TASK".into(), "from env".into()),
        ("ORQYNN_AGENT_BINARY".into(), "/env/agent".into()),
        ("ORQYNN_AGENT_PERMISSION_MODE".into(), "acceptEdits".into()),
    ]);
    let from_env = RunArgs::from_env_and_args(["run"], &env).expect("parses");
    assert_eq!(from_env.repo, PathBuf::from("/from/env"));
    assert_eq!(from_env.task, "from env");
    assert_eq!(from_env.agent_binary, "/env/agent");
    assert_eq!(from_env.permission_mode.as_deref(), Some("acceptEdits"));

    let overridden =
        RunArgs::from_env_and_args(["run", "--repo", "/from/args", "--task", "from args"], &env)
            .expect("parses");
    assert_eq!(
        overridden.repo,
        PathBuf::from("/from/args"),
        "arguments win"
    );
    assert_eq!(overridden.task, "from args");
}

#[test]
fn no_secret_is_read_from_the_environment() {
    // A credential in the environment must be ignored, not copied into config.
    let env = MapEnv(vec![
        ("ANTHROPIC_API_KEY".into(), "sk-secret".into()),
        ("CLAUDE_SESSION_ID".into(), "secret-session".into()),
        ("ORQYNN_REPO".into(), "/r".into()),
    ]);
    let args = RunArgs::from_env_and_args(["run"], &env).expect("parses");
    let text = format!("{:?}", args);
    assert!(
        !text.contains("sk-secret"),
        "no key may be captured: {text}"
    );
    assert!(
        !text.contains("secret-session"),
        "no session id either: {text}"
    );
}

#[test]
fn memory_is_off_unless_it_is_asked_for() {
    let cli = Cli::new(&["git --version"]);
    assert!(!cli.args.memory, "local-only operation is the default");
    let report = cli_lifecycle::dry_run_report(&cli.args).join("\n");
    assert!(
        report.contains("checkpoints stay in SQLite"),
        "a local run must say that nothing external is touched"
    );
}

#[test]
fn a_missing_agent_executable_is_refused_before_anything_runs() {
    let cli = Cli::new(&["git --version"]);
    let mut args = cli.args.clone();
    args.agent_binary = "C:/definitely/not/here/agent.exe".into();
    assert!(
        matches!(
            args.validate_paths().expect_err("missing agent"),
            ConfigError::AgentBinaryMissing { .. }
        ),
        "a missing executable is caught at validation, not at spawn"
    );
}

#[test]
fn a_bare_agent_name_that_is_not_on_path_is_refused() {
    let cli = Cli::new(&["git --version"]);
    let mut args = cli.args.clone();
    args.agent_binary = "orqynn-definitely-not-installed".into();
    assert!(matches!(
        args.validate_paths().expect_err("not on PATH"),
        ConfigError::AgentBinaryNotOnPath { .. }
    ));
}

#[test]
fn memory_is_refused_when_asked_for_but_unconfigured() {
    let mut args = RunArgs::parse([
        "--repo", "/tmp/r", "--task", "x", "--memory", "--check", "y",
    ])
    .expect("parses");
    // The configuration is checked by the substrate's own reader, which names the
    // missing variable; this proves the flag alone is not enough.
    args.repo = PathBuf::from(".");
    if std::env::var("AI_MEMORY_BINARY").is_err() || std::env::var("AI_MEMORY_DATA_DIR").is_err() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        // Matched rather than `expect_err`, because the success type carries an
        // adapter that has no `Debug` — and printing it is not something this
        // test should be able to do even by accident.
        match runtime.block_on(cli_lifecycle::connect_memory(&args)) {
            Err(cli_lifecycle::RunError::Config(ConfigError::MemoryNotConfigured)) => {}
            Err(other) => panic!("expected a configuration refusal, got {other:?}"),
            Ok(_) => panic!("memory was reported as usable without configuration"),
        }
    }
}

// ---------------------------------------------------------------------------
// The lifecycle, with a real process and real SQLite
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_passing_check_reports_success_even_when_the_agent_did_nothing() {
    // The load-bearing test: the fixture agent exits zero having changed nothing,
    // and the task is still judged by its own check. `git --version` passes, so
    // the task is done — and it is done because the *check* said so, not the
    // agent.
    let cli = Cli::new(&["git --version"]);
    let report = cli
        .run("succeed", Duration::from_secs(30))
        .await
        .expect("the run");
    assert_eq!(report.exit_code, ExitCode::Success);
    let text = report.lines.join("\n");
    assert!(text.contains("every check exited zero"));
    assert!(text.contains("PASS"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_check_reports_failure_even_though_the_agent_exited_zero() {
    // The inverse, and the one that matters most: a successful harness process
    // plus a failing check must never be reported as success.
    let cli = Cli::new(&["git --no-such-flag"]);
    let report = cli
        .run("succeed", Duration::from_secs(30))
        .await
        .expect("the run");
    assert_eq!(report.exit_code, ExitCode::VerificationFailed);
    let text = report.lines.join("\n");
    assert!(text.contains("the task is NOT done"), "{text}");
    assert!(text.contains("FAIL"));
    assert!(!text.contains("every check exited zero"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_that_reports_an_error_is_an_execution_failure_even_when_checks_pass() {
    // The process ran and exited non-zero, and the check passes anyway. What must
    // not happen is a *silent* success: the harness told Orqyn it could not do the
    // task, so the run is an execution failure regardless of what verification
    // says.
    let cli = Cli::new(&["git --version"]);
    let report = cli
        .run("fail", Duration::from_secs(30))
        .await
        .expect("the run");
    assert_eq!(report.exit_code, ExitCode::Execution);
    let text = report.lines.join("\n");
    assert!(text.contains("the harness reported an error"), "{text}");
    assert!(
        !text.contains("every check exited zero: the task is done"),
        "a harness that admitted failure must not be reported as a done task: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_agent_that_exits_zero_is_judged_only_by_its_checks_and_reported_honestly() {
    // The fixture exits zero having printed nothing. Orqyn did not judge the
    // silence: verification ran and passed, so that is the verdict. What the
    // report must do is describe what actually happened, so a reader can see that
    // the agent produced nothing rather than being told the work was done.
    let cli = Cli::new(&["git --version"]);
    let report = cli
        .run("vanish", Duration::from_secs(30))
        .await
        .expect("the run");
    assert_eq!(report.exit_code, ExitCode::Success);
    let text = report.lines.join("\n");
    assert!(
        text.contains("0 bytes of stdout"),
        "the report must show that the agent produced no output: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_harness_that_times_out_is_reported_rather_than_looked_like_success() {
    // The fixture outlives the budget, so the driver must give up on the clock
    // rather than on a race with the spawn. A budget of zero or a nanosecond
    // would make this test flaky: sometimes the spawn finishes first and the
    // timeout never fires.
    let cli = Cli::new(&["git --version"]);
    let mut args = cli.args.clone();
    args.agent_timeout = Duration::from_secs(1);
    let runtime = cli_lifecycle::open_runtime(&args).await.expect("runtime");
    let report = cli_lifecycle::run_lifecycle(
        &runtime,
        &director_adapters::InMemoryProvider::new(),
        &driver_for("hang", Duration::from_secs(1)),
        &director_adapters::executor::LocalExecutor::default(),
        &args,
        chrono::Utc::now(),
    )
    .await
    .expect("the run");
    assert_ne!(
        report.exit_code,
        ExitCode::Success,
        "a run whose agent could not finish must not report success"
    );
    assert!(report.lines.join("\n").contains("did not run"));
}

// ---------------------------------------------------------------------------
// 10. Store reopen and recovery
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_survives_a_reopen_and_a_recovery_context_is_assembled() {
    let cli = Cli::new(&["git --version"]);
    let report = cli
        .run("succeed", Duration::from_secs(30))
        .await
        .expect("the run");
    assert_eq!(report.exit_code, ExitCode::Success);

    // Reopen the same database with a fresh handle: the run's records are there.
    let reopened = director_store::Store::open(&cli.args.database)
        .await
        .expect("the store reopens");
    let task = reopened
        .tasks()
        .get_task(&director_domain::ids::TaskId::from_string(
            cli.args.task_id(),
        ))
        .await
        .expect("the task reads back");
    assert_eq!(
        task.status,
        director_domain::task::TaskStatus::Done,
        "the verdict was persisted, not merely reported"
    );

    let verifications = reopened
        .verifications()
        .verifications_for_task(&director_domain::ids::TaskId::from_string(
            cli.args.task_id(),
        ))
        .await
        .expect("verifications read");
    assert_eq!(verifications.len(), 1, "one verdict on disk");

    let actions = reopened
        .actions()
        .actions_for_task(&director_domain::ids::TaskId::from_string(
            cli.args.task_id(),
        ))
        .await
        .expect("actions read");
    assert!(
        !actions.is_empty(),
        "the observed command is on record, atomically with the verdict"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_run_also_persists_its_verdict_and_actions() {
    // A failure is not the absence of a record.
    let cli = Cli::new(&["git --no-such-flag"]);
    let report = cli
        .run("succeed", Duration::from_secs(30))
        .await
        .expect("the run");
    assert_eq!(report.exit_code, ExitCode::VerificationFailed);

    let reopened = director_store::Store::open(&cli.args.database)
        .await
        .expect("the store reopens");
    let task_id = director_domain::ids::TaskId::from_string(cli.args.task_id());
    assert_eq!(
        reopened
            .tasks()
            .get_task(&task_id)
            .await
            .expect("task reads")
            .status,
        director_domain::task::TaskStatus::Failed
    );
    assert_eq!(
        reopened
            .verifications()
            .verifications_for_task(&task_id)
            .await
            .expect("verifications read")
            .len(),
        1
    );
    assert!(!reopened
        .actions()
        .actions_for_task(&task_id)
        .await
        .expect("actions read")
        .is_empty());
}

// ---------------------------------------------------------------------------
// 11. A retry does not duplicate
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retrying_the_same_task_id_refuses_rather_than_planning_a_second_task() {
    let cli = Cli::new(&["git --version"]);
    cli.run("succeed", Duration::from_secs(30))
        .await
        .expect("the first run");

    let runtime = cli_lifecycle::open_runtime(&cli.args)
        .await
        .expect("runtime");
    let second = cli_lifecycle::run_lifecycle(
        &runtime,
        &director_adapters::InMemoryProvider::new(),
        &driver_for("succeed", Duration::from_secs(30)),
        &director_adapters::executor::LocalExecutor::default(),
        &cli.args,
        chrono::Utc::now(),
    )
    .await;

    // The plan id is derived from the task id, so re-running the same task id
    // addresses a task that already exists. It is refused before the plan is
    // attempted, with a message that says nothing was duplicated and says what to
    // do instead — rather than surfacing a SQLite constraint violation.
    match second {
        Err(cli_lifecycle::RunError::Orchestration { step, reason }) => {
            assert_eq!(step, "PLAN");
            assert!(
                reason.contains("nothing was duplicated"),
                "the refusal must say so: {reason}"
            );
            assert!(
                !reason.to_lowercase().contains("sqlite"),
                "and it must not leak a storage-engine error: {reason}"
            );
        }
        other => panic!("a retry must not silently re-plan: {other:?}"),
    }

    // And exactly one task exists.
    let tasks = runtime
        .store
        .tasks()
        .list_tasks(&cli_lifecycle::project_id())
        .await
        .expect("tasks read");
    assert_eq!(tasks.len(), 1, "the retry created no second task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_different_task_id_plans_a_second_task() {
    let cli = Cli::new(&["git --version"]);
    cli.run("succeed", Duration::from_secs(30))
        .await
        .expect("the first run");

    let mut args = cli.args.clone();
    args.task_id = "A-SECOND-TASK".into();
    let runtime = cli_lifecycle::open_runtime(&args).await.expect("runtime");
    // The plan id is derived from the task id, so a different task is not a retry
    // and must not be refused as one. This is what keeps the CLI usable for more
    // than one task per database.
    let report = cli_lifecycle::run_lifecycle(
        &runtime,
        &director_adapters::InMemoryProvider::new(),
        &driver_for("succeed", Duration::from_secs(30)),
        &director_adapters::executor::LocalExecutor::default(),
        &args,
        chrono::Utc::now(),
    )
    .await
    .expect("a different task id is a different task, and must be plannable");
    assert_eq!(report.exit_code, ExitCode::Success);

    let tasks = runtime
        .store
        .tasks()
        .list_tasks(&cli_lifecycle::project_id())
        .await
        .expect("tasks read");
    assert_eq!(tasks.len(), 2, "both tasks exist, exactly once each");
    let ids: Vec<String> = tasks.iter().map(|t| t.id.as_str().to_string()).collect();
    let mut unique = ids.clone();
    unique.sort();
    let mut sorted = ids.clone();
    sorted.sort();
    unique.dedup();
    assert_eq!(unique, sorted, "task ids are unique: {ids:?}");
}

// ---------------------------------------------------------------------------
// Dry run and derived ids
// ---------------------------------------------------------------------------

#[test]
fn a_dry_run_reports_the_plan_and_starts_nothing() {
    let cli = Cli::new(&["cargo test"]);
    let mut args = cli.args.clone();
    args.dry_run = true;
    let report = cli_lifecycle::dry_run_report(&args).join("\n");

    assert!(report.starts_with("DRY RUN — nothing was executed"));
    assert!(report.contains("report the task done only if every check exits zero"));
    assert!(report.contains("cargo test"), "the checks must be named");
    assert!(
        report.contains("will not pick a permissive one"),
        "and the permission-mode default must be visible"
    );
}

#[test]
fn a_dry_run_needs_no_check_because_it_verifies_nothing() {
    let cli = Cli::new(&["git --version"]);
    let mut args = cli.args.clone();
    args.checks.clear();
    args.dry_run = true;
    args.resolve()
        .expect("a dry run resolves without a check, because it verifies nothing");
    assert!(args.checks.is_empty());
    assert!(
        cli_lifecycle::dry_run_report(&args)
            .join("\n")
            .starts_with("DRY RUN — nothing was executed"),
        "and it must be unmistakable that nothing ran"
    );
}

#[test]
fn task_ids_are_derived_deterministically_and_safely() {
    assert_eq!(derive_task_id("add a greeting"), "ADD-A-GREETING");
    assert_eq!(
        derive_task_id("add a greeting"),
        derive_task_id("add a greeting")
    );
    assert_eq!(derive_task_id("  spaced   out  "), "SPACED-OUT");
    assert_eq!(
        derive_task_id("!!!"),
        "TASK",
        "punctuation alone still yields an id"
    );
    assert!(
        derive_task_id(&"x".repeat(200)).len() <= 48,
        "a long sentence must not produce an unwieldy id"
    );
    assert!(
        derive_task_id("a/b\\c")
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "an id must not contain a path separator"
    );
}

#[test]
fn the_exit_codes_are_distinct_and_stable() {
    // A script depends on these, so they are asserted rather than assumed.
    assert_eq!(ExitCode::Success.status(), 0);
    assert_eq!(ExitCode::Configuration.status(), 2);
    assert_eq!(ExitCode::Orchestration.status(), 3);
    assert_eq!(ExitCode::Execution.status(), 4);
    assert_eq!(ExitCode::VerificationFailed.status(), 5);
    assert_eq!(ExitCode::Unverifiable.status(), 6);
    assert_eq!(ExitCode::DryRun.status(), 7);
}

/// The fixture and a `DriveOptions` that both agree on the timeout, so the
/// lifecycle cannot pass a budget the driver would ignore.
#[test]
fn the_lifecycle_and_the_driver_are_given_the_same_timeout() {
    let cli = Cli::new(&["git --version"]);
    let config = cli_lifecycle::harness_config(&cli.args);
    assert_eq!(config.default_timeout, cli.args.agent_timeout);
    assert_eq!(
        config.binary, cli.args.agent_binary,
        "the binary the CLI validated is the one it launches"
    );
    assert_eq!(
        config.permission_mode, cli.args.permission_mode,
        "and no permissive mode is chosen on the caller's behalf"
    );
}

/// The model is configurable end to end, and unconfigured means *absent* rather
/// than guessed.
///
/// Orqyn has no opinion about which model to use. With none given it passes no
/// `--model` flag at all, so the harness applies its own configured default —
/// inventing one here would override a deliberate operator choice with a guess.
#[test]
fn the_model_is_configurable_and_never_guessed() {
    let cli = Cli::new(&["git --version"]);

    // Unconfigured: the driver carries no model, so no flag reaches the harness.
    assert_eq!(cli.args.model, None, "no model is chosen for the caller");
    assert_eq!(lifecycle::harness_config(&cli.args).model, None);

    // Configured: it is carried onto the driver, which is what emits `--model`.
    let mut configured = cli.args.clone();
    configured.model = Some("some-model".into());
    assert_eq!(
        lifecycle::harness_config(&configured).model.as_deref(),
        Some("some-model"),
        "an explicitly configured model reaches the harness configuration"
    );

    // And the environment is a source, with arguments winning over it.
    let env = MapEnv(vec![("ORQYNN_AGENT_MODEL".into(), "from-env".into())]);
    let from_env = RunArgs::from_env_and_args(["run"], &env).expect("parses");
    assert_eq!(from_env.model.as_deref(), Some("from-env"));
    let overridden =
        RunArgs::from_env_and_args(["run", "--model", "from-args"], &env).expect("parses");
    assert_eq!(overridden.model.as_deref(), Some("from-args"));
}

/// Guards against the fixture drifting from what the driver expects.
#[test]
fn the_agent_fixture_speaks_the_protocol_the_driver_parses() {
    let source = std::fs::read_to_string(fixture_binary().with_file_name("agent.rs"))
        .expect("the fixture source sits beside its binary");
    assert!(
        source.contains("\\\"type\\\":\\\"result\\\"") || source.contains("\"type\":\"result\""),
        "the fixture must emit the result object the driver parses"
    );
}
