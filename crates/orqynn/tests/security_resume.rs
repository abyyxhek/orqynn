//! **Finding ORQ-01 (High) — end-to-end reproduction at the CLI level.**
//!
//! The database Orqyn opens is, by default, *inside the repository*: `<repo>/.orqynn/orqynn.db`.
//! Nothing about the store's open path asks where that file came from. A task
//! row persisted in it carries `expected_outputs[].check` strings, and a
//! `resume` of a task awaiting verification executes those persisted strings —
//! the operator's `--check` arguments are not even consulted unless they are
//! passed, and then only to be *matched* against what is persisted.
//!
//! That makes a repository that ships a `.orqynn/` directory a carrier for
//! arbitrary command execution on the machine that clones and resumes it. This
//! test reproduces that attack with a harmless payload:
//!
//! 1. A task is created and left awaiting verification, the way a real run does.
//! 2. The persisted acceptance criteria are then rewritten to point at a
//!    payload executable — standing in for the attacker's check string, written
//!    by whoever placed the database. No `--check` argument is involved.
//! 3. The `.orqynn/` directory is carried into a *second, fresh* repository,
//!    i.e. the file is treated as repository-supplied content.
//! 4. `resume` runs against the second repository with **no** `--check`.
//!
//! The payload is a real compiled executable that writes one marker file next
//! to itself. The marker appearing is proof the persisted string ran. Nothing
//! in this test asks the operator for the command, and nothing destructive is
//! executed anywhere.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use director_domain::ids::TaskId;
use director_domain::store::TaskRepository;

#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/lifecycle.rs"]
mod lifecycle;

use crate::config::{Command as CliCommand, RunArgs};
use crate::lifecycle::ResumeAction;

/// The payload: writes one file beside its own executable, so the test can
/// prove it ran without any shell redirection or filesystem trick.
const PAYLOAD_SOURCE: &str = r#"
fn main() {
    let exe = match std::env::current_exe() { Ok(p) => p, Err(_) => return };
    let dir = exe.parent().unwrap_or(std::path::Path::new("."));
    let _ = std::fs::write(dir.join("PWNED-MARKER"), "the persisted check executed");
}
"#;

/// A stand-in agent: reads the briefing, reports success. It is a real process,
/// not Claude Code.
const AGENT_SOURCE: &str = r#"
fn main() {
    let mut brief = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut brief);
    println!("{{\"type\":\"result\",\"is_error\":false,\"result\":\"did the work\"}}");
}
"#;

fn compile(source: &str, name: &str) -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR
        .get_or_init(|| {
            let d =
                std::env::temp_dir().join(format!("orqynn-security-resume-{}", std::process::id()));
            std::fs::create_dir_all(&d).expect("fixture dir");
            d
        })
        .clone();
    let exe = dir.join(name);
    if !exe.exists() {
        // The source name must be a valid crate name, so it is derived from
        // `name` without any extension.
        let stem = name.trim_end_matches(".exe");
        let src = dir.join(format!("{stem}_src.rs"));
        std::fs::write(&src, source).expect("source written");
        let status = Command::new("rustc")
            .arg("--edition")
            .arg("2021")
            .arg("-O")
            .arg(&src)
            .arg("-o")
            .arg(&exe)
            .status()
            .expect("rustc on PATH");
        assert!(status.success(), "the fixture must compile");
    }
    exe
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
    run(&["config", "user.name", "Orqynn Security Tests"]);
    run(&["config", "user.email", "security@orqyn.invalid"]);
    run(&["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "# fixture\n").expect("write");
    run(&["add", "README.md"]);
    run(&["commit", "-q", "-m", "initial"]);
}

/// A task at `verification_pending` in a repository whose state directory is
/// the default `<repo>/.orqynn` — plus the compiled payload, standing in for
/// the executable a malicious repository would ship.
struct AttackerDb {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    payload: PathBuf,
    payload_name: String,
    args: RunArgs,
}

impl AttackerDb {
    /// Runs a real `run` with a deliberately unrunnable check, which leaves the
    /// task awaiting verification — the state `resume` re-verifies.
    async fn planted() -> Self {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let repo = tmp.path().join("repo-a");
        std::fs::create_dir_all(&repo).expect("repo dir");
        git_repo(&repo);

        let mut args = RunArgs::parse([
            "--repo",
            repo.to_str().expect("utf-8"),
            "--task",
            "ship the helper script",
            "--check",
            "definitely-not-a-real-program-xyz",
        ])
        .expect("parses");
        args.agent_binary = compile(
            AGENT_SOURCE,
            if cfg!(windows) { "agent.exe" } else { "agent" },
        )
        .to_string_lossy()
        .into_owned();
        args.resolve().expect("resolves");

        let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
        let driver = director_adapters::harness::ClaudeCodeDriver::new(
            director_adapters::harness::ClaudeCodeConfig {
                binary: args.agent_binary.clone(),
                default_timeout: Duration::from_secs(30),
                ..director_adapters::harness::ClaudeCodeConfig::default()
            },
        );
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

        // The task must be awaiting verification: the agent reported, and the
        // unrunnable check left the round with no verdict.
        let task = runtime
            .store
            .tasks()
            .get_task(&TaskId::from_string(args.task_id()))
            .await
            .expect("the task exists");
        assert_eq!(
            task.status,
            director_domain::task::TaskStatus::VerificationPending,
            "setup: the task must be awaiting verification, it is {:?}",
            task.status
        );

        AttackerDb {
            _tmp: tmp,
            repo,
            payload: PathBuf::new(),
            payload_name: String::new(),
            args,
        }
    }

    /// Rewrite the persisted acceptance criteria — the write a database
    /// supplied with the repository arrives with. The payload executable is
    /// compiled into the repo itself, the way a malicious repository ships one,
    /// and the check names it *relative to the repository root*, which is the
    /// form that survives being cloned to another machine.
    /// Compile the payload executable and place it in the repository, the way
    /// a malicious repository ships the command its database names.
    fn plant_payload(&mut self) {
        let payload = compile(
            PAYLOAD_SOURCE,
            if cfg!(windows) {
                "payload.exe"
            } else {
                "payload"
            },
        );
        let name = if cfg!(windows) {
            "payload.exe"
        } else {
            "payload"
        };
        let in_repo = self.repo.join(name);
        std::fs::copy(&payload, &in_repo).expect("the payload ships in the repo");
        self.payload = in_repo.clone();
        self.payload_name = name.to_string();
    }

    /// Rewrite the persisted acceptance criteria — the write a database
    /// supplied with the repository arrives with. The persisted string is an
    /// absolute path into the repository the payload ships in, so what the
    /// operator's machine later executes is exactly what the database author
    /// wrote. No `--check` argument is involved anywhere.
    async fn rewrite_criteria_to(&self, store: &director_store::Store, check: &str) {
        let id = TaskId::from_string(self.args.task_id());
        let mut task = store.tasks().get_task(&id).await.expect("the task exists");
        task.expected_outputs = vec![director_domain::task::ExpectedOutput {
            criterion: "the shipped helper runs".into(),
            check: Some(check.to_string()),
        }];
        store
            .tasks()
            .update_task(&task)
            .await
            .expect("the attacker's criteria are persisted");
    }

    /// Carry the state directory into a second, fresh repository: the database
    /// is now repository-supplied content, and `resume` is run against it with
    /// no `--check` at all.
    async fn resumed_in_a_fresh_clone(&self) -> (ResumeAction, bool) {
        let clone_dir = self._tmp.path().join("repo-clone");
        std::fs::create_dir_all(&clone_dir).expect("clone dir");
        git_repo(&clone_dir);
        // The shipped state: the whole `.orqynn/` directory, database included,
        // carried into the clone at the same relative path.
        let shipped = self.repo.join(".orqynn");
        let state_dest = clone_dir.join(".orqynn");
        std::fs::create_dir_all(&state_dest).expect("the clone's state dir");
        for entry in std::fs::read_dir(&shipped).expect("the state dir exists") {
            let entry = entry.expect("readable");
            let dest = state_dest.join(entry.file_name());
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                copy_dir(&entry.path(), &dest);
            } else {
                std::fs::copy(entry.path(), &dest).expect("the state ships");
            }
        }
        // The repository also ships the executable the database names.
        std::fs::copy(&self.payload, clone_dir.join(&self.payload_name))
            .expect("the payload ships in the clone");

        let mut args = RunArgs::parse([
            "--repo",
            clone_dir.to_str().expect("utf-8"),
            "--task-id",
            self.args.task_id(),
        ])
        .expect("parses");
        args.command = CliCommand::Resume;
        args.agent_binary = compile(
            AGENT_SOURCE,
            if cfg!(windows) { "agent.exe" } else { "agent" },
        )
        .to_string_lossy()
        .into_owned();
        args.resolve().expect("resolves");

        // The attacker's criteria are written into the shipped database, naming
        // the payload *as the clone receives it*: an absolute path into the
        // repository, the form the database author controls end to end.
        let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
        let check = clone_dir
            .join(&self.payload_name)
            .to_string_lossy()
            .into_owned();
        self.rewrite_criteria_to(&runtime.store, &check).await;
        drop(runtime);

        let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
        let driver = director_adapters::harness::ClaudeCodeDriver::new(
            director_adapters::harness::ClaudeCodeConfig {
                binary: args.agent_binary.clone(),
                default_timeout: Duration::from_secs(30),
                ..director_adapters::harness::ClaudeCodeConfig::default()
            },
        );
        let report = lifecycle::resume_lifecycle(
            &runtime,
            &director_adapters::InMemoryProvider::new(),
            &driver,
            &director_adapters::executor::LocalExecutor::default(),
            &args,
            chrono::Utc::now(),
        )
        .await
        .expect("the resume ran");
        (report.action, self.marker_exists_in(&clone_dir))
    }

    /// The payload's marker: written beside the executable the check named, so
    /// a marker in the *clone's* repo is proof the shipped command ran there.
    fn marker_exists_in(&self, clone_dir: &Path) -> bool {
        clone_dir.join("PWNED-MARKER").exists()
    }
}

fn copy_dir(src: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).expect("dest dir");
    for entry in std::fs::read_dir(src).expect("readable") {
        let entry = entry.expect("readable");
        let target = dest.join(entry.file_name());
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copied");
        }
    }
}

/// The attack: a repository-supplied database, a resumed task, a command the
/// operator never typed. If this test passes (marker present), the finding is
/// confirmed end to end; if the marker is absent AND the action is `Refused`,
/// a control has been added and this test should be updated to assert it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repository_supplied_database_executes_its_persisted_check_at_resume() {
    let mut attacker = AttackerDb::planted().await;
    attacker.plant_payload();
    let (action, marker) = attacker.resumed_in_a_fresh_clone().await;

    assert_eq!(
        action,
        ResumeAction::Reverified,
        "the resume re-verified the task against the persisted criteria"
    );
    assert!(
        marker,
        "ORQ-01 confirmed: the check persisted in the repository-supplied database was \
         executed by `resume`, and the operator passed no --check at all"
    );
}
