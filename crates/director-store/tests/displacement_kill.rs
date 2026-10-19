//! What survives a process that dies mid-transaction.
//!
//! # What this does and does not prove
//!
//! It proves one thing: **a process killed while holding the database's write
//! lock leaves no partial state behind.** The child opens the same real SQLite
//! file, takes the same `BEGIN IMMEDIATE` reserved write lock the store's
//! displacement transaction takes, writes a partial transaction, signals the
//! parent through a rendezvous file, and parks to be killed. The parent then
//! reopens the database and asserts the partial write is gone.
//!
//! It does **not** prove power-loss durability, and makes no such claim. A kill
//! is an ordinary process termination: the operating system reaps the process and
//! closes its file descriptors, so SQLite's own recovery runs. A power cut is a
//! different event with different guarantees, and nothing here simulates one.
//!
//! # No sleeps, no probabilistic termination
//!
//! The parent kills the child only after the child has written the rendezvous
//! file, which it does from inside the transaction. There is no timer deciding
//! anything: the transaction is provably open at the moment of the kill, because
//! the child said so after acquiring the lock and before parking.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use director_domain::ids::{AgentId, ProjectId, TaskId};
use director_domain::store::{AssignmentRepository, HandoffRepository, PlanRepository};
use director_domain::task::{Task, TaskStatus};
use director_domain::{
    Agent, AgentRepository, Capability, Harness, MachineId, Project, ProjectRepository,
    TaskRepository,
};
use director_store::Store as SqliteStore;

const PROJ: &str = "PROJ-1";
const TASK: &str = "WORK";
const A: &str = "AGENT-a";
const B: &str = "AGENT-b";

/// Roles are chosen by environment variable so a stray manual run of this binary
/// cannot start writing to a database.
const ROLE: &str = "ORQYNN_DISPLACEMENT_KILL_ROLE";
const DB_PATH: &str = "ORQYNN_DISPLACEMENT_KILL_DB";
const READY: &str = "ORQYNN_DISPLACEMENT_KILL_READY";

/// The child entry point. `#[ignore]`d because it is a program, not a test: it
/// does nothing unless the role variable is set, and the parent runs it with
/// `--ignored --exact child_harness`.
#[test]
#[ignore = "child-process harness, spawned explicitly by the parent test"]
fn child_harness() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    if role != "hold-write-lock" {
        panic!("unknown role {role:?}");
    }
    let db = std::env::var(DB_PATH).expect("a database path");
    let ready = std::env::var(READY).expect("a rendezvous path");

    let conn = rusqlite::Connection::open(&db).expect("the child opens the database");
    // The same reserved write lock the store's displacement transaction takes.
    // Taking it here means the parent can be certain the transaction is open.
    conn.execute_batch("BEGIN IMMEDIATE")
        .expect("the child takes the write lock");
    conn.execute_batch(
        "INSERT INTO handoffs (id, task_id, from_agent, to_agent, state, summary,
             open_questions, next_steps, files_touched, created_at, cause)
         VALUES ('HO-PARTIAL','WORK','AGENT-a','AGENT-b','\"open\"','partial',
                 '[]','[]','[]','2024-01-01T00:00:00Z','null')",
    )
    .expect("the child writes inside the open transaction");

    // Signalled from inside the transaction, so the rendezvous *is* the proof that
    // the kill lands while the transaction is open.
    std::fs::write(&ready, b"holding").expect("the child signals");
    loop {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn store_with_task(dir: &Path) -> SqliteStore {
    let store = futures_block_on(SqliteStore::open(dir.join("orqynn.db"))).expect("opens");
    futures_block_on(async {
        store
            .projects()
            .create_project(&Project::new(ProjectId::from_string(PROJ), PROJ, "/repo"))
            .await
            .expect("project");
        for id in [A, B] {
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
        // Named by an active plan, as any assignable task must be.
        let mut plan = director_domain::plan::Plan::draft(
            director_domain::ids::PlanId::from_string("PLAN-WORK"),
            ProjectId::from_string(PROJ),
            "the work",
            "planned for these tests",
        );
        plan.task_ids = vec![TaskId::from_string(TASK)];
        store
            .plans()
            .create_plan(&plan)
            .await
            .expect("plan drafted");
        store
            .plans()
            .activate_plan(
                &director_domain::ids::PlanId::from_string("PLAN-WORK"),
                AgentId::from_string(A),
            )
            .await
            .expect("plan activated");
    });
    store
}

/// A minimal block-on, so the fixture can build its store without pulling the
/// fixture into an async test.
fn futures_block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(future)
}

fn handoff_count(store: &SqliteStore) -> usize {
    futures_block_on(
        store
            .handoffs()
            .handoffs_for_task(&TaskId::from_string(TASK)),
    )
    .expect("handoffs read")
    .len()
}

#[test]
fn a_process_killed_inside_the_write_transaction_leaves_nothing_behind() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let store = store_with_task(dir.path());
    let db = dir.path().join("orqynn.db");
    drop(store);

    let ready = dir.path().join("ready");
    let mut child: Child = Command::new(std::env::current_exe().expect("this binary"))
        .arg("--ignored")
        .arg("--exact")
        .arg("child_harness")
        .arg("--nocapture")
        .env(ROLE, "hold-write-lock")
        .env(DB_PATH, &db)
        .env(READY, &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the child starts");

    // Wait for the rendezvous, not for a duration: the child writes it from
    // inside the open transaction, so its presence proves the transaction is open.
    let mut waited = Duration::ZERO;
    while !ready.exists() {
        assert!(
            waited < Duration::from_secs(60),
            "the child never took the lock"
        );
        std::thread::sleep(Duration::from_millis(20));
        waited += Duration::from_millis(20);
    }

    // An ordinary process kill - not a power cut, and not asserted as one.
    child.kill().expect("the child is killed");
    child.wait().expect("the child is reaped");

    // Reopen exactly as a fresh process would.
    let reopened = futures_block_on(SqliteStore::open(&db)).expect("the database reopens");
    assert_eq!(
        handoff_count(&reopened),
        0,
        "the uncommitted handoff is gone: SQLite recovered the journal, so the \
         transaction's writes are not visible"
    );
    let task = futures_block_on(reopened.tasks().get_task(&TaskId::from_string(TASK)))
        .expect("the task is intact");
    assert_eq!(task.status, TaskStatus::Todo, "and untouched");

    // The database is usable afterwards, which is the practical consequence that
    // matters: recovery left no lock behind.
    futures_block_on(async {
        reopened
            .agents()
            .get_agent(&AgentId::from_string(A))
            .await
            .expect("the store accepts work after recovery");
    });
}

/// The committed counterpart: a displacement that runs to completion leaves every
/// part of the transaction visible together. Without this, the kill test could
/// pass simply because nothing was ever written.
#[test]
fn a_committed_displacement_is_visible_in_full() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let store = store_with_task(dir.path());
    let db: PathBuf = dir.path().join("orqynn.db");

    // Synchronous throughout: this test shares the fixture with the kill test
    // above, which must run outside any runtime so it can wait on a rendezvous.
    futures_block_on(store.assign_and_start_next(
        &TaskId::from_string(TASK),
        &AgentId::from_string(A),
        chrono::Utc::now(),
    ))
    .expect("A takes the task");
    futures_block_on(store.assign_and_start_next(
        &TaskId::from_string(TASK),
        &AgentId::from_string(B),
        chrono::Utc::now(),
    ))
    .expect("B displaces A");

    drop(store);
    let reopened = futures_block_on(SqliteStore::open(&db)).expect("reopens");

    let records = futures_block_on(
        reopened
            .handoffs()
            .handoffs_for_task(&TaskId::from_string(TASK)),
    )
    .expect("handoffs read");
    assert_eq!(
        records.len(),
        1,
        "the handoff committed with the transaction"
    );
    let sitting = futures_block_on(
        reopened
            .assignments()
            .active_assignment_for_task(&TaskId::from_string(TASK)),
    )
    .expect("assignments")
    .expect("someone holds it");
    assert_eq!(
        sitting.agent_id,
        AgentId::from_string(B),
        "and the replacement assignment committed too"
    );
    assert_eq!(
        records[0].from_agent,
        AgentId::from_string(A),
        "consistent with it: the handoff names the agent that was displaced"
    );
}
