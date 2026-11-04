//! Store-layer adversarial security tests (see `SECURITY_AUDIT.md`, §5).
//!
//! These complement the existing `tests/store.rs` suite with the two checks the
//! audit names as controls rather than as changes:
//!
//! 1. **SQL-metacharacter ids are data, never syntax.** Every query the store
//!    runs is parameterized; an id that contains SQL syntax must round-trip as
//!    a literal and must never alter the schema.
//! 2. **A tampered status string is a hard error, never a silent default.**
//!    The schema carries statuses as JSON text with no `CHECK` constraint, so
//!    the read path is the only place an unrecognized value can be caught. It
//!    must be caught, not coerced.
//!
//! Both use real SQLite and a real store, under a temp dir.

use director_domain::ids::{ProjectId, TaskId};
use director_domain::store::TaskRepository;
use director_domain::task::{Task, TaskStatus};
use director_domain::ProjectRepository;
use director_store::Store;

/// A store, and the temp dir it lives in.
async fn store() -> (Store, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a temp dir");
    let store = Store::open(dir.path().join("orqyn.db"))
        .await
        .expect("the store opens");
    (store, dir)
}

/// Create the project a task needs before it can be stored.
async fn project(store: &Store) -> ProjectId {
    let project = ProjectId::from_string("PROJ-1");
    store
        .projects()
        .create_project(&director_domain::project::Project::new(
            project.clone(),
            project.as_str(),
            "/nowhere",
        ))
        .await
        .expect("project created");
    project
}

/// **Control for SQLite injection.** An id built entirely of SQL syntax round
/// trips as one literal string: the task comes back with that exact id, the
/// schema is intact, and the sibling table the id "targets" still stands.
#[tokio::test]
async fn an_id_full_of_sql_syntax_is_data_not_sql() {
    let (store, dir) = store().await;
    let project = project(&store).await;

    let poison = "x'; DROP TABLE tasks;--";
    let mut task = Task::for_project(
        project.clone(),
        TaskId::from_string(poison.to_string()),
        "t",
        "o",
    );
    task.status = TaskStatus::Todo;
    store.tasks().create_task(&task).await.expect("created");

    // The task is readable, under the poisoned id, exactly as written.
    let back = store
        .tasks()
        .get_task(&TaskId::from_string(poison.to_string()))
        .await
        .expect("read back");
    assert_eq!(back.id.as_str(), poison);

    // The table the id tried to drop still exists, and so does another table.
    let conn = rusqlite::Connection::open(dir.path().join("orqyn.db")).expect("inspect");
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("prepared")
        .query_map([], |row| row.get(0))
        .expect("queried")
        .filter_map(|row| row.ok())
        .collect();
    assert!(tables.iter().any(|name| name == "tasks"), "tasks survives");
    assert!(
        tables.iter().any(|name| name == "agent_assignments"),
        "agent_assignments survives: {tables:?}"
    );
    // And the poisoned id is stored as one row, not executed.
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
        .expect("counted");
    assert_eq!(count, 1);
}

/// **Control for data integrity.** A task row whose status column holds a
/// string the model does not recognize must surface as an error, never become a
/// silent default status — because a silent default is how a tampered database
/// would launder a status nobody ever wrote.
#[tokio::test]
async fn an_unrecognized_status_string_is_an_error_not_a_default() {
    let (store, dir) = store().await;
    let project = project(&store).await;

    let mut task = Task::for_project(project, TaskId::from_string("A"), "t", "o");
    task.status = TaskStatus::Todo;
    store.tasks().create_task(&task).await.expect("created");

    // Tamper directly, the way a process with write access to the file would.
    let conn = rusqlite::Connection::open(dir.path().join("orqyn.db")).expect("tamper");
    conn.execute(
        "UPDATE tasks SET status = 'totally-not-a-status' WHERE id = 'A'",
        [],
    )
    .expect("status tampered");
    drop(conn);

    let err = store
        .tasks()
        .get_task(&TaskId::from_string("A"))
        .await
        .expect_err("an unrecognized status must not read back as anything");
    assert!(
        matches!(err, director_domain::StoreError::Serialization(_)),
        "expected a serialization error, got {err:?}"
    );
}

/// **Control for Q3 of the audit (write-path validation).** The store has no
/// length limits and no id-format checks by design; the one shape rule it does
/// enforce is that an Orqyn task belongs to a project. That rule is the last
/// line between a caller-written task row and Orqyn's own project state.
#[tokio::test]
async fn a_task_with_no_project_is_refused_at_the_store() {
    let (store, _dir) = store().await;
    let mut task = Task::new(
        TaskId::from_string("ORPHAN"),
        "no project",
        "nothing to verify against",
    );
    task.status = TaskStatus::Todo;
    let err = store
        .tasks()
        .create_task(&task)
        .await
        .expect_err("an orphan task is refused");
    assert!(matches!(
        err,
        director_domain::StoreError::ConstraintViolation(_)
    ));
}
