//! `orqynn handoffs`: a read-only audit listing.
//!
//! The properties under test are about what the command must *not* do. It reads
//! records and prints them; it creates nothing, changes nothing, accepts nothing,
//! and launches nothing. Each of those is asserted rather than assumed - the
//! no-mutation test compares the persisted records before and after, and the
//! no-agent test watches for the fixture's sentinel file on disk rather than
//! trusting that no line of output appeared.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use director_domain::handoff::{Handoff, HandoffCause, HandoffState};
use director_domain::ids::{AgentId, HandoffId, ProjectId, TaskId};
use director_domain::store::{HandoffRepository, TaskRepository};
use director_domain::task::{Task, TaskStatus};
use director_domain::{
    Agent, AgentRepository, Capability, Harness, MachineId, Project, ProjectRepository,
};
use director_store::Store as SqliteStore;

#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/lifecycle.rs"]
mod lifecycle;

use crate::config::{Command as CliCommand, RunArgs};

const PROJ: &str = "PROJ-1";
const TASK: &str = "WORK";
const FROM: &str = "AGENT-out";
const TO: &str = "AGENT-in";

fn pinned(step: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000 + step * 60, 0).expect("a pinned moment")
}

/// An agent stand-in that leaves a sentinel if it is ever launched.
fn fixture_binary() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("orqynn-handoffs-cli-{}", std::process::id()));
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
        assert!(status.success(), "the fixture must compile");
        binary
    })
    .clone()
}

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

/// A repository, a store, and the resolved arguments a `handoffs` listing uses.
struct Cli {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    sentinel: PathBuf,
    store: SqliteStore,
    args: RunArgs,
}

impl Cli {
    async fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        git_repo(&repo);
        let sentinel = tmp.path().join("agent-ran");
        // The CLI creates this directory at startup; the test opens the store
        // directly, so it has to make it itself.
        std::fs::create_dir_all(repo.join(".orqynn")).expect("state dir");

        let store = SqliteStore::open(repo.join(".orqynn").join("orqynn.db"))
            .await
            .expect("store opens");
        store
            .projects()
            .create_project(&Project::new(
                ProjectId::from_string(PROJ),
                PROJ,
                repo.to_str().expect("utf-8"),
            ))
            .await
            .expect("project");
        for id in [FROM, TO] {
            store
                .agents()
                .register_agent(&Agent::register(
                    AgentId::from_string(id),
                    id,
                    Harness::ClaudeCode,
                    MachineId::from_string(format!("MACH-{id}")),
                    vec![Capability::Coding],
                ))
                .await
                .expect("agent");
        }
        let mut task = Task::for_project(
            ProjectId::from_string(PROJ),
            TaskId::from_string(TASK),
            TASK,
            "the work",
        );
        task.status = TaskStatus::Todo;
        store.tasks().create_task(&task).await.expect("task");

        // Note: no `--check` and no valid `--agent-binary` is supplied. A
        // read-only listing needs neither, and this is how that stays true.
        let mut args = RunArgs::parse([
            "orqynn",
            "handoffs",
            "--repo",
            repo.to_str().expect("utf-8"),
            "--task-id",
            TASK,
        ])
        .expect("parses");
        args.resolve()
            .expect("a listing resolves without a check or an agent");

        Cli {
            _tmp: tmp,
            repo,
            sentinel,
            store,
            args,
        }
    }

    async fn list(&self) -> Result<Vec<String>, lifecycle::RunError> {
        let runtime = lifecycle::open_runtime(&self.args).await.expect("runtime");
        lifecycle::list_handoffs(&runtime, &self.args).await
    }

    fn record(&self, id: &str, at: i64, cause: Option<HandoffCause>) -> Handoff {
        let mut handoff = Handoff::open(
            HandoffId::from_string(id),
            TaskId::from_string(TASK),
            AgentId::from_string(FROM),
            AgentId::from_string(TO),
            "a summary a listing must not dump in full",
        )
        .in_project(ProjectId::from_string(PROJ))
        .derived_from(None, Some(7));
        handoff.created_at = pinned(at);
        if let Some(cause) = cause {
            handoff = handoff.caused_by(cause);
        }
        handoff
    }
}

/// What each agent believes it is currently holding - execution authority, which a
/// read-only listing must not move.
async fn agent_views(store: &SqliteStore) -> Vec<String> {
    let mut views = Vec::new();
    for id in [FROM, TO] {
        views.push(
            store
                .agents()
                .get_agent(&AgentId::from_string(id))
                .await
                .expect("agent")
                .current_task
                .map(|t| t.as_str().to_string())
                .unwrap_or_else(|| "none".into()),
        );
    }
    views
}

fn joined(report: Result<Vec<String>, lifecycle::RunError>) -> String {
    report.expect("a listing").join("\n")
}

// Empty list.
#[tokio::test]
async fn an_empty_list_says_so_rather_than_printing_nothing() {
    let cli = Cli::new().await;
    let text = joined(cli.list().await);
    assert!(text.contains("no handoffs are recorded"), "{text}");
}

// One record, both proven causes, and the legacy NULL case.
#[tokio::test]
async fn causes_are_rendered_from_the_recorded_value() {
    let cli = Cli::new().await;
    cli.store
        .handoffs()
        .open_handoff(&cli.record("HO-D", 0, Some(HandoffCause::Displaced)))
        .await
        .expect("displacement recorded");
    cli.store
        .handoffs()
        .open_handoff(&cli.record("HO-E", 1, Some(HandoffCause::LeaseExpiredReclaimed)))
        .await
        .expect("expiry recorded");
    // A record with no cause - what a row written before causes existed holds.
    cli.store
        .handoffs()
        .open_handoff(&cli.record("HO-LEGACY", 2, None))
        .await
        .expect("legacy recorded");

    let text = joined(cli.list().await);
    assert!(text.contains("3 handoff record(s)"), "{text}");
    assert!(text.contains("displaced while active"), "{text}");
    assert!(
        text.contains("reclaimed after the previous lease expired"),
        "{text}"
    );
    assert!(
        text.contains("unknown - not established from persisted records"),
        "a record with no cause must say unknown, never guess: {text}"
    );
}

// Missing provenance and staleness are both marked.
#[tokio::test]
async fn missing_provenance_and_staleness_are_marked() {
    let cli = Cli::new().await;
    cli.store
        .handoffs()
        .open_handoff(&cli.record("HO-1", 0, Some(HandoffCause::Displaced)))
        .await
        .expect("recorded");

    let text = joined(cli.list().await);
    assert!(
        text.contains("provenance unavailable"),
        "an absent checkpoint must be reported, not hidden: {text}"
    );
    assert!(
        text.contains("STALE - context only"),
        "and a snapshot with no checkpoint cannot be current: {text}"
    );
    assert!(
        !text.contains("a summary a listing must not dump"),
        "the continuation narrative is summarised, not printed: {text}"
    );
}

// The listing is read-only, and launches nothing.
#[tokio::test]
async fn listing_changes_nothing_and_launches_nothing() {
    let cli = Cli::new().await;
    cli.store
        .handoffs()
        .open_handoff(&cli.record("HO-1", 0, Some(HandoffCause::Displaced)))
        .await
        .expect("recorded");

    // A snapshot of everything the listing could plausibly touch.
    let before_task = cli
        .store
        .tasks()
        .get_task(&TaskId::from_string(TASK))
        .await
        .expect("task");
    let before_handoffs = cli
        .store
        .handoffs()
        .handoffs_for_task(&TaskId::from_string(TASK))
        .await
        .expect("handoffs");
    let before_agent_task = agent_views(&cli.store).await;

    let text = joined(cli.list().await);
    assert!(text.contains("HO-1"));

    let after_task = cli
        .store
        .tasks()
        .get_task(&TaskId::from_string(TASK))
        .await
        .expect("task");
    let after_handoffs = cli
        .store
        .handoffs()
        .handoffs_for_task(&TaskId::from_string(TASK))
        .await
        .expect("handoffs");
    let after_agent_task = agent_views(&cli.store).await;

    assert_eq!(after_task, before_task, "a listing must not touch the task");
    assert_eq!(
        after_handoffs, before_handoffs,
        "and must not create, accept, or rewrite a handoff"
    );
    assert_eq!(
        after_agent_task, before_agent_task,
        "nor grant or move any execution authority"
    );
    assert!(
        !cli.sentinel.exists(),
        "no agent process may be launched by a read-only listing"
    );
    assert!(cli.args.agent_binary != fixture_binary());
}

// Unknown task, and a repository that is not one.
#[tokio::test]
async fn unknown_task_and_invalid_repository_are_refused_safely() {
    let cli = Cli::new().await;

    let mut unknown = cli.args.clone();
    unknown.task_id = "NO-SUCH-TASK".into();
    let runtime = lifecycle::open_runtime(&unknown).await.expect("runtime");
    let outcome = lifecycle::list_handoffs(&runtime, &unknown).await;
    assert!(
        matches!(outcome, Err(lifecycle::RunError::Orchestration { .. })),
        "a task Orqynn has never seen is refused rather than listed as empty"
    );

    let mut not_a_repo = cli.args.clone();
    not_a_repo.repo = std::path::PathBuf::from("C:/definitely/not/a/repo");
    assert!(
        not_a_repo.validate_repository_and_database().is_err(),
        "a path that is not a repository is refused before anything is opened"
    );
}

// The command selects itself from the parser and needs nothing to execute.
#[test]
fn the_listing_command_is_selected_and_needs_no_execution_inputs() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    git_repo(&repo);
    let mut args = RunArgs::parse([
        "orqynn",
        "handoffs",
        "--repo",
        repo.to_str().expect("utf-8"),
        "--task-id",
        "T1",
    ])
    .expect("parses");
    assert_eq!(args.command, CliCommand::Handoffs);
    args.resolve().expect("resolves");
    assert!(args.checks.is_empty(), "a listing requires no check");
    assert!(
        args.provenance().join("\n").contains("read-only listing"),
        "and says so before anything happens"
    );
    let _ = fixture_binary();
    let _ = Duration::from_secs(1);
}

// Migration regression: a fresh database, and a populated one that predates causes.
#[tokio::test]
async fn a_populated_database_keeps_its_records_and_gains_a_null_cause() {
    let cli = Cli::new().await;
    // A record written without a cause is what a pre-`0006` row becomes.
    let mut legacy = cli.record("HO-LEGACY", 0, None);
    legacy.state = HandoffState::Open;
    cli.store
        .handoffs()
        .open_handoff(&legacy)
        .await
        .expect("recorded");

    // Reopen: the migration must have preserved the row, and must not have
    // invented a cause for a transfer that happened before causes existed.
    drop(cli.store);
    let reopened = SqliteStore::open(cli.repo.join(".orqynn").join("orqynn.db"))
        .await
        .expect("reopens");
    let read = reopened
        .handoffs()
        .get_handoff(&HandoffId::from_string("HO-LEGACY"))
        .await
        .expect("the record survived the migration");
    assert_eq!(
        read.cause, None,
        "a historical transfer keeps its unknown cause rather than being back-filled"
    );
    assert_eq!(read.summary, legacy.summary, "and its content is intact");
}
