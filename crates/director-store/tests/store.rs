//! Integration tests for [`director_store::Store`] against a real SQLite file.
//!
//! These are the tests that make the schema's promises real. The unit tests in
//! each module cover the mapping; what this file covers is the *behavior the
//! phase is built for*: an optimistic conflict is reported rather than silently
//! overwritten, an assignment's release is recorded rather than deleted, a
//! superseded checkpoint survives, and a task that names no project is refused.
//!
//! Each test opens its own temporary database, so they are independent and can
//! run in parallel. The file is deleted when the handle drops.
//!
//! The plan tests hold the activation invariant: a plan becomes authoritative
//! only through `activate_plan`, which supersedes the sitting active plan in the
//! same transaction, and the partial unique index turns a forgotten supersession
//! into an error. The decision tests hold the matching rule that reversal
//! *marks* a decision rather than deleting it.

use std::matches;

use director_domain::agent::{Agent, AgentStatus, Harness};
use director_domain::assignment::{AgentAssignment, AssignmentStatus, ReleaseReason};
use director_domain::capability::Capability;
use director_domain::checkpoint::{Checkpoint, CheckpointStatus};
use director_domain::decision::{Decision, DecisionStatus};
use director_domain::ids::{
    AgentId, AssignmentId, CheckpointId, DecisionId, MachineId, PlanId, ProjectId, RepositoryId,
    SessionId, TaskId, VerificationId,
};
use director_domain::plan::{Plan, PlanStatus};
use director_domain::project::{DefaultBranch, Project};
use director_domain::session::{AgentSession, SessionEnd, SessionStatus};
use director_domain::state::TestResults;
use director_domain::store::{ProviderSync, StoredProjectState};
use director_domain::task::{Task, TaskStatus};
use director_domain::verification::{
    Evidence, EvidenceStatus, ProbeKind, Verification, VerificationStatus,
};
use director_domain::{
    AgentRepository, AssignmentRepository, CheckpointRepository, DecisionRepository,
    PlanRepository, ProjectRepository, ProjectStateRepository, ProviderSyncRepository,
    SessionRepository, StoreError, TaskRepository, VerificationRepository,
};

use director_store::Store;

/// A fresh store backed by a temporary file, deleted when the test ends.
async fn store() -> (Store, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a temp dir");
    let path = dir.path().join("director.db");
    let store = Store::open(&path).await.expect("store opens and migrates");
    (store, dir)
}

// ---------------------------------------------------------------------------
// Opening and migrating.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn opening_creates_the_schema_and_is_idempotent() {
    let (store, dir) = store().await;
    let path = dir.path().join("director.db");
    let newest = director_store::latest_version();
    assert_eq!(
        store.schema_version(),
        newest,
        "a fresh database reaches the newest migration"
    );

    // Reopening the same file must be a no-op, not an error: this is what makes
    // `open` safe to call on every startup.
    let again = Store::open(&path).await.expect("reopening is a no-op");
    assert_eq!(again.schema_version(), newest);
}

#[tokio::test]
async fn a_newer_database_is_refused_rather_than_downgraded() {
    let (_store, dir) = store().await;
    let path = dir.path().join("director.db");
    // Pretend a future Orqyn wrote migration 999.
    let conn = rusqlite::Connection::open(&path).expect("open for tampering");
    conn.execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (999, '2026-01-01')",
        [],
    )
    .expect("record a future migration");
    drop(conn);

    // `Store` holds a connection pool and is not `Debug`, so this is matched by
    // hand rather than with `unwrap_err`.
    let err = match Store::open(&path).await {
        Err(err) => err,
        Ok(_) => panic!("a newer database must be refused, not opened"),
    };
    assert!(matches!(err, StoreError::Migration(_)), "got {err:?}");
    let msg = format!("{err}");
    assert!(
        msg.contains("999"),
        "the message should name the unknown version: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Projects.
// ---------------------------------------------------------------------------

fn project(id: &str) -> Project {
    Project::new(
        ProjectId::from_string(id),
        "checkout-service",
        "/repo/checkout-service",
    )
}

#[tokio::test]
async fn a_project_round_trips_through_the_store() {
    let (store, _path) = store().await;
    let created = store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project created");
    assert_eq!(created.id, ProjectId::from_string("PROJ-1"));
    assert_eq!(created.default_branch, DefaultBranch::Main);
    assert_eq!(created.state_version, 1, "a new record starts at version 1");

    let loaded = store
        .projects()
        .get_project(&ProjectId::from_string("PROJ-1"))
        .await
        .expect("project loaded");
    assert_eq!(loaded, created);

    let listed = store.projects().list_projects().await.expect("list");
    assert_eq!(listed, vec![created]);
}

#[tokio::test]
async fn a_duplicate_project_id_is_rejected() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("first project");
    let err = store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintViolation(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_stale_project_update_is_a_conflict_not_an_overwrite() {
    let (store, _path) = store().await;
    let created = store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project created");

    // The caller's stale copy, still at version 1.
    let mut stale = created.clone();
    stale.name = "stale-name".into();

    // A first writer moves the version to 2.
    let mut fresh = created;
    fresh.name = "fresh-name".into();
    let updated = store
        .projects()
        .update_project(&fresh)
        .await
        .expect("first update lands");
    assert_eq!(updated.state_version, 2);

    // The stale writer must not silently win.
    let err = store.projects().update_project(&stale).await.unwrap_err();
    assert!(
        matches!(err, StoreError::StateVersionConflict { .. }),
        "got {err:?}"
    );

    // The winner's write is what the store holds.
    let loaded = store
        .projects()
        .get_project(&ProjectId::from_string("PROJ-1"))
        .await
        .expect("project loaded");
    assert_eq!(loaded.name, "fresh-name");
    assert_eq!(loaded.state_version, 2);
}

#[tokio::test]
async fn an_unknown_project_id_is_not_found() {
    let (store, _path) = store().await;
    let err = store
        .projects()
        .get_project(&ProjectId::from_string("PROJ-nope"))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Tasks.
// ---------------------------------------------------------------------------

fn task(id: &str, project: &str) -> Task {
    let mut task = Task::new(
        TaskId::from_string(id),
        "Wire the checkout retry policy",
        "Retries are idempotent and capped at three attempts.",
    );
    task.project_id = Some(ProjectId::from_string(project));
    task
}

#[tokio::test]
async fn a_task_round_trips_and_carries_no_agent() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");

    let created = store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task created");
    assert_eq!(created.id, TaskId::from_string("TASK-1"));
    assert_eq!(created.project_id, Some(ProjectId::from_string("PROJ-1")));

    let loaded = store
        .tasks()
        .get_task(&TaskId::from_string("TASK-1"))
        .await
        .expect("task loaded");
    assert_eq!(loaded, created);
}

#[tokio::test]
async fn the_tasks_table_names_no_agent() {
    // Assignment is the agent_assignments table. The task row itself must carry
    // no agent reference — asserted here, against the schema, so a future
    // migration that adds one is noticed by this test rather than by a caller.
    let (_store, dir) = store().await;
    let path = dir.path().join("director.db");
    let conn = rusqlite::Connection::open(&path).expect("open for inspection");
    let mut stmt = conn
        .prepare("PRAGMA table_info(tasks)")
        .expect("describe tasks");
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("column names")
        .map(|r| r.expect("column name"))
        .collect();
    assert!(
        !columns.iter().any(|c| c.contains("agent")),
        "the tasks table must not name an agent; its columns are {columns:?}"
    );
}

#[tokio::test]
async fn a_task_without_a_project_is_refused() {
    let (store, _path) = store().await;
    let unscoped = Task::new(
        TaskId::from_string("TASK-1"),
        "Unscoped work",
        "Belongs to no project.",
    );
    let err = store.tasks().create_task(&unscoped).await.unwrap_err();
    // The exact variant depends on which constraint fires first (NOT NULL vs the
    // foreign key); either way an unscoped task is refused rather than stored.
    assert!(
        matches!(
            err,
            StoreError::ConstraintViolation(_) | StoreError::Storage(_)
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_status_change_appends_to_history_never_overwriting() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");

    let created = store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task created");

    let mut moved = created;
    moved.status = TaskStatus::InProgress;
    let after_first = store
        .tasks()
        .update_task(&moved)
        .await
        .expect("first update");
    assert_eq!(after_first.status, TaskStatus::InProgress);

    // The second move continues from the version the first write produced —
    // reusing `moved` would be a stale write and a conflict, not a no-op.
    let mut moved_again = after_first;
    moved_again.status = TaskStatus::Blocked;
    let after_second = store
        .tasks()
        .update_task(&moved_again)
        .await
        .expect("second update");
    assert_eq!(after_second.state_version, 3);

    let history = store
        .tasks()
        .task_history(&TaskId::from_string("TASK-1"))
        .await
        .expect("history");
    let transitions: Vec<_> = history.into_iter().map(|t| (t.from, t.to)).collect();
    assert_eq!(
        transitions,
        vec![
            (TaskStatus::Backlog, TaskStatus::InProgress),
            (TaskStatus::InProgress, TaskStatus::Blocked),
        ],
        "history accumulates every transition in order"
    );
}

// ---------------------------------------------------------------------------
// Agents.
// ---------------------------------------------------------------------------

fn agent(id: &str) -> Agent {
    Agent::register(
        AgentId::from_string(id),
        "claude-code",
        Harness::ClaudeCode,
        MachineId::from_string("MACH-a"),
        vec![Capability::Coding, Capability::Testing],
    )
}

#[tokio::test]
async fn an_agent_round_trips_with_its_capabilities() {
    let (store, _path) = store().await;
    let created = store
        .agents()
        .register_agent(&agent("AGENT-1"))
        .await
        .expect("agent registered");
    assert_eq!(created.status, AgentStatus::Available);
    assert_eq!(
        created.capabilities,
        vec![Capability::Coding, Capability::Testing]
    );

    let loaded = store
        .agents()
        .get_agent(&AgentId::from_string("AGENT-1"))
        .await
        .expect("agent loaded");
    assert_eq!(loaded, created);

    assert_eq!(store.agents().list_agents().await.expect("list").len(), 1);
}

#[tokio::test]
async fn an_agent_update_bumps_its_version() {
    let (store, _path) = store().await;
    let created = store
        .agents()
        .register_agent(&agent("AGENT-1"))
        .await
        .expect("agent registered");

    let mut updated = created;
    updated.status = AgentStatus::Busy;
    let after = store
        .agents()
        .update_agent(&updated)
        .await
        .expect("agent updated");
    assert_eq!(after.state_version, 2);
    assert_eq!(after.status, AgentStatus::Busy);
}

// ---------------------------------------------------------------------------
// Sessions: the lineage of who worked on what.
// ---------------------------------------------------------------------------

fn session(id: &str, agent: &str, task: &str) -> AgentSession {
    AgentSession::start(
        SessionId::from_string(id),
        Some(ProjectId::from_string("PROJ-1")),
        AgentId::from_string(agent),
        MachineId::from_string("MACH-a"),
        Some(TaskId::from_string(task)),
    )
}

#[tokio::test]
async fn a_session_round_trips_and_closes_with_its_end_reason() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");
    store
        .agents()
        .register_agent(&agent("AGENT-1"))
        .await
        .expect("agent");
    store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task");

    let started = store
        .sessions()
        .create_session(&session("SESS-1", "AGENT-1", "TASK-1"))
        .await
        .expect("session created");
    assert_eq!(started.status, SessionStatus::Active);

    let closed = store
        .sessions()
        .end_session(
            &SessionId::from_string("SESS-1"),
            SessionEnd::Clean,
            started.state_version,
        )
        .await
        .expect("session closed");
    assert_eq!(closed.end, Some(SessionEnd::Clean));
    assert_eq!(closed.status, SessionStatus::Closed);
    assert_eq!(closed.state_version, 2);
}

#[tokio::test]
async fn a_task_accumulates_sessions_in_start_order() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");
    store
        .agents()
        .register_agent(&agent("AGENT-1"))
        .await
        .expect("agent");
    store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task");

    store
        .sessions()
        .create_session(&session("SESS-1", "AGENT-1", "TASK-1"))
        .await
        .expect("first session");
    // The second session is a continuation of the first, as a retry would be.
    let mut second = session("SESS-2", "AGENT-1", "TASK-1");
    second.parent_session_id = Some(SessionId::from_string("SESS-1"));
    store
        .sessions()
        .create_session(&second)
        .await
        .expect("second session");

    let lineage = store
        .sessions()
        .sessions_for_task(&TaskId::from_string("TASK-1"))
        .await
        .expect("lineage");
    assert_eq!(
        lineage.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec!["SESS-1", "SESS-2"],
        "sessions are returned oldest first"
    );
    assert_eq!(
        lineage[1].parent_session_id,
        Some(SessionId::from_string("SESS-1"))
    );
}

// ---------------------------------------------------------------------------
// Assignments: release marks, never deletes.
// ---------------------------------------------------------------------------

fn assignment(id: &str, task: &str, agent: &str) -> AgentAssignment {
    AgentAssignment::propose(
        AssignmentId::from_string(id),
        TaskId::from_string(task),
        AgentId::from_string(agent),
    )
}

#[tokio::test]
async fn an_assignment_can_be_activated_and_released() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");
    store
        .agents()
        .register_agent(&agent("AGENT-1"))
        .await
        .expect("agent");
    store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task");

    let proposed = store
        .assignments()
        .create_assignment(&assignment("ASG-1", "TASK-1", "AGENT-1"))
        .await
        .expect("assignment proposed");
    assert_eq!(proposed.status, AssignmentStatus::Proposed);
    assert!(
        store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string("TASK-1"))
            .await
            .expect("no active assignment yet")
            .is_none(),
        "a proposed assignment does not occupy the active slot"
    );

    // The session the activation names has to exist: the assignment row carries
    // a foreign key to it.
    store
        .sessions()
        .create_session(&session("SESS-1", "AGENT-1", "TASK-1"))
        .await
        .expect("session");

    let mut activated = proposed;
    activated.activate(SessionId::from_string("SESS-1"));
    let active = store
        .assignments()
        .update_assignment(&activated)
        .await
        .expect("assignment activated");
    assert_eq!(active.status, AssignmentStatus::Active);
    assert_eq!(
        store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string("TASK-1"))
            .await
            .expect("the active assignment")
            .expect("there is one")
            .id,
        AssignmentId::from_string("ASG-1")
    );

    let released = store
        .assignments()
        .release_assignment(
            &AssignmentId::from_string("ASG-1"),
            ReleaseReason::WorkComplete,
            active.state_version,
        )
        .await
        .expect("assignment released");
    assert_eq!(released.status, AssignmentStatus::Released);
    assert_eq!(released.release_reason, Some(ReleaseReason::WorkComplete));
    // The row is still there — released, not deleted.
    assert!(
        store
            .assignments()
            .get_assignment(&AssignmentId::from_string("ASG-1"))
            .await
            .expect("the released row is retained")
            .status
            == AssignmentStatus::Released
    );
}

#[tokio::test]
async fn assignment_history_retains_every_tenure() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");
    store
        .agents()
        .register_agent(&agent("AGENT-1"))
        .await
        .expect("agent");
    store
        .agents()
        .register_agent(&agent("AGENT-2"))
        .await
        .expect("second agent");
    store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task");

    store
        .sessions()
        .create_session(&session("SESS-1", "AGENT-1", "TASK-1"))
        .await
        .expect("session");

    let first = store
        .assignments()
        .create_assignment(&assignment("ASG-1", "TASK-1", "AGENT-1"))
        .await
        .expect("first assignment");
    let mut active = first;
    active.activate(SessionId::from_string("SESS-1"));
    let first_active = store
        .assignments()
        .update_assignment(&active)
        .await
        .expect("first activated");

    // Hand the same task to a second agent. The first tenure is released, not
    // removed — this is what makes "Claude then Codex worked on TASK-1" a query.
    let second = store
        .assignments()
        .create_assignment(&assignment("ASG-2", "TASK-1", "AGENT-2"))
        .await
        .expect("second assignment");

    store
        .assignments()
        .release_assignment(
            &AssignmentId::from_string("ASG-1"),
            ReleaseReason::Reassigned,
            first_active.state_version,
        )
        .await
        .expect("first tenure released");

    let history = store
        .assignments()
        .assignment_history(&TaskId::from_string("TASK-1"))
        .await
        .expect("history");
    assert_eq!(history.len(), 2, "both tenures are retained");
    assert_eq!(
        history[0].agent_id,
        AgentId::from_string("AGENT-1"),
        "oldest first"
    );
    assert_eq!(history[1].agent_id, AgentId::from_string("AGENT-2"));
    assert!(second.status == AssignmentStatus::Proposed);
}

#[tokio::test]
async fn two_active_assignments_for_one_task_are_rejected() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");
    store
        .agents()
        .register_agent(&agent("AGENT-1"))
        .await
        .expect("agent");
    store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task");

    store
        .sessions()
        .create_session(&session("SESS-1", "AGENT-1", "TASK-1"))
        .await
        .expect("session");

    let first = store
        .assignments()
        .create_assignment(&assignment("ASG-1", "TASK-1", "AGENT-1"))
        .await
        .expect("first assignment");
    let mut active = first;
    active.activate(SessionId::from_string("SESS-1"));
    store
        .assignments()
        .update_assignment(&active)
        .await
        .expect("activated");

    // A second assignment for the same task is allowed while proposed...
    let second = store
        .assignments()
        .create_assignment(&assignment("ASG-2", "TASK-1", "AGENT-1"))
        .await
        .expect("second assignment");
    // ...but activating it must fail: the partial unique index fires.
    store
        .sessions()
        .create_session(&session("SESS-2", "AGENT-1", "TASK-1"))
        .await
        .expect("second session");
    let mut second_active = second;
    second_active.activate(SessionId::from_string("SESS-2"));
    let err = store
        .assignments()
        .update_assignment(&second_active)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintViolation(_)),
        "got {err:?}"
    );
    // The sitting agent keeps the task — the failed write changed nothing.
    assert_eq!(
        store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string("TASK-1"))
            .await
            .expect("the active assignment")
            .expect("there is one")
            .id,
        AssignmentId::from_string("ASG-1")
    );
}

// ---------------------------------------------------------------------------
// The atomic assignment operation: the thing the loop's ASSIGN step will call.
// ---------------------------------------------------------------------------

/// The prerequisites every `assign_task` test needs: a project, two agents, and
/// a task. Returns the ids the test hands to the operation.
async fn assignment_prereqs(store: &Store) {
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");
    store
        .agents()
        .register_agent(&agent("AGENT-1"))
        .await
        .expect("first agent");
    store
        .agents()
        .register_agent(&agent("AGENT-2"))
        .await
        .expect("second agent");
    store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task");
}

#[tokio::test]
async fn assign_task_hands_the_task_to_the_agent_atomically() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;

    // Nothing is assigned yet.
    assert!(
        store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string("TASK-1"))
            .await
            .expect("the query")
            .is_none(),
        "a fresh task has no active assignment"
    );

    let assigned = store
        .assign_task(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("the assignment");

    assert_eq!(assigned.id, AssignmentId::from_string("ASG-1"));
    assert_eq!(assigned.agent_id, AgentId::from_string("AGENT-1"));
    assert_eq!(assigned.status, AssignmentStatus::Active);

    // The store reflects it: the task has an active assignment, and the agent's
    // denormalized view points at it.
    let active = store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("TASK-1"))
        .await
        .expect("the query")
        .expect("there is one now");
    assert_eq!(active.id, AssignmentId::from_string("ASG-1"));

    let agent = store
        .agents()
        .get_agent(&AgentId::from_string("AGENT-1"))
        .await
        .expect("agent");
    assert_eq!(
        agent.current_task,
        Some(TaskId::from_string("TASK-1")),
        "the denormalized view moved with the assignment"
    );
}

#[tokio::test]
async fn assign_task_releases_the_sitting_agent_and_keeps_the_history() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_task(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("first assignment");

    // Hand the same task to a second agent. All three effects of the
    // transaction are visible afterwards; if any one were missing the invariant
    // would be broken in a different way.
    store
        .assign_task(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-2"),
            &AssignmentId::from_string("ASG-2"),
            chrono::Utc::now(),
        )
        .await
        .expect("reassignment");

    // The new agent holds the task.
    let active = store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("TASK-1"))
        .await
        .expect("the query")
        .expect("there is one");
    assert_eq!(active.id, AssignmentId::from_string("ASG-2"));
    assert_eq!(
        store
            .agents()
            .get_agent(&AgentId::from_string("AGENT-2"))
            .await
            .expect("second agent")
            .current_task,
        Some(TaskId::from_string("TASK-1")),
    );

    // The sitting agent was released, not deleted, and cleared of the task.
    let first = store
        .assignments()
        .get_assignment(&AssignmentId::from_string("ASG-1"))
        .await
        .expect("the first tenure is retained");
    assert_eq!(first.status, AssignmentStatus::Released);
    assert_eq!(first.release_reason, Some(ReleaseReason::Reassigned));
    assert_eq!(
        store
            .agents()
            .get_agent(&AgentId::from_string("AGENT-1"))
            .await
            .expect("first agent")
            .current_task,
        None,
        "the released agent no longer holds the task"
    );

    // Both tenures are in the history, oldest first.
    let history = store
        .assignments()
        .assignment_history(&TaskId::from_string("TASK-1"))
        .await
        .expect("history");
    assert_eq!(
        history.len(),
        2,
        "the first tenure was retained, not deleted"
    );
    assert_eq!(history[0].agent_id, AgentId::from_string("AGENT-1"));
    assert_eq!(history[1].agent_id, AgentId::from_string("AGENT-2"));
}

#[tokio::test]
async fn assign_task_to_an_unknown_agent_leaves_the_sitting_agent_holding_it() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_task(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("first assignment");

    // Reassigning to an agent that was never registered must fail on the
    // foreign key rather than storing an assignment with no agent.
    let err = store
        .assign_task(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-nope"),
            &AssignmentId::from_string("ASG-2"),
            chrono::Utc::now(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintViolation(_)),
        "got {err:?}"
    );

    // The failed transaction changed nothing: the sitting agent keeps the task,
    // and no second assignment row was stored.
    assert_eq!(
        store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string("TASK-1"))
            .await
            .expect("the query")
            .expect("there is one")
            .id,
        AssignmentId::from_string("ASG-1")
    );
    assert!(store
        .assignments()
        .get_assignment(&AssignmentId::from_string("ASG-2"))
        .await
        .is_err());
    assert_eq!(
        store
            .agents()
            .get_agent(&AgentId::from_string("AGENT-1"))
            .await
            .expect("first agent")
            .current_task,
        Some(TaskId::from_string("TASK-1")),
        "the failed write did not take the task away from the sitting agent"
    );
}

#[tokio::test]
async fn assign_task_to_the_same_agent_keeps_the_view_pointing_at_the_task() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_task(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("first assignment");

    // Reassigning to the agent that already holds the task must end with the
    // view intact: the clear of the departing agent and the set of the incoming
    // one touch the same row, and in the wrong order the clear would win.
    store
        .assign_task(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-2"),
            chrono::Utc::now(),
        )
        .await
        .expect("reassignment to the same agent");

    assert_eq!(
        store
            .agents()
            .get_agent(&AgentId::from_string("AGENT-1"))
            .await
            .expect("agent")
            .current_task,
        Some(TaskId::from_string("TASK-1")),
        "the view still points at the task the agent holds"
    );
    let history = store
        .assignments()
        .assignment_history(&TaskId::from_string("TASK-1"))
        .await
        .expect("history");
    assert_eq!(history.len(), 2, "both assignments are retained");
    assert_eq!(
        store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string("TASK-1"))
            .await
            .expect("the query")
            .expect("there is one")
            .id,
        AssignmentId::from_string("ASG-2")
    );
}

// ---------------------------------------------------------------------------
// The atomic lease operation: the thing the loop's MONITOR step will call.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expire_lease_releases_the_assignment_and_puts_the_task_back_to_todo() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_and_start(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("assigned and started");

    let now = chrono::Utc::now();
    let (assignment, task, session) = store
        .expire_lease(&TaskId::from_string("TASK-1"), now)
        .await
        .expect("the lease expired")
        .expect("there was a lease to expire");

    // The tenure is retained, and its reason says the lease ran out.
    assert_eq!(assignment.status, AssignmentStatus::Released);
    assert_eq!(assignment.release_reason, Some(ReleaseReason::LeaseExpired));
    assert_eq!(assignment.released_at, Some(now));
    assert!(session.is_none(), "no session was ever attached");

    // The task is handable again, and its history records the move back.
    assert_eq!(task.status, TaskStatus::Todo);
    assert_eq!(
        store
            .tasks()
            .get_task(&TaskId::from_string("TASK-1"))
            .await
            .expect("task")
            .status,
        TaskStatus::Todo
    );
    let history = store
        .tasks()
        .task_history(&TaskId::from_string("TASK-1"))
        .await
        .expect("history");
    assert_eq!(history.len(), 2, "started, then returned to todo");
    assert_eq!(history[0].to, TaskStatus::InProgress);
    assert_eq!(history[1].from, TaskStatus::InProgress);
    assert_eq!(history[1].to, TaskStatus::Todo);

    // The agent holds nothing and is recorded as gone.
    let agent = store
        .agents()
        .get_agent(&AgentId::from_string("AGENT-1"))
        .await
        .expect("agent");
    assert_eq!(agent.current_task, None);
    assert_eq!(agent.status, AgentStatus::Disconnected);

    // And no assignment is in force for the task, so it can be handed out again.
    assert!(
        store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string("TASK-1"))
            .await
            .expect("the query")
            .is_none(),
        "the lease is over"
    );
}

#[tokio::test]
async fn expire_lease_closes_the_session_that_held_the_work_as_vanished() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_and_start(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("assigned and started");
    store
        .acknowledge_assignment(
            &AssignmentId::from_string("ASG-1"),
            &session("SESS-1", "AGENT-1", "TASK-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("the agent started a session");

    let (_, _, session) = store
        .expire_lease(&TaskId::from_string("TASK-1"), chrono::Utc::now())
        .await
        .expect("the lease expired")
        .expect("there was a lease");

    // The session is closed, and its end says Orqyn noticed the heartbeat stop
    // rather than that the agent closed out properly.
    let session = session.expect("the lease had a session");
    assert_eq!(session.status, SessionStatus::Closed);
    assert_eq!(session.end, Some(SessionEnd::Vanished));

    let reloaded = store
        .sessions()
        .get_session(&SessionId::from_string("SESS-1"))
        .await
        .expect("session");
    assert_eq!(reloaded.end, Some(SessionEnd::Vanished));
    assert!(reloaded.ended_uncleanly());
}

#[tokio::test]
async fn expire_lease_leaves_a_session_the_agent_already_closed_alone() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_and_start(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("assigned and started");
    store
        .acknowledge_assignment(
            &AssignmentId::from_string("ASG-1"),
            &session("SESS-1", "AGENT-1", "TASK-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("the agent started a session");
    // The agent closed out properly before the lease was reclaimed.
    store
        .sessions()
        .end_session(&SessionId::from_string("SESS-1"), SessionEnd::Clean, 1)
        .await
        .expect("session closed cleanly");

    store
        .expire_lease(&TaskId::from_string("TASK-1"), chrono::Utc::now())
        .await
        .expect("the lease expired");

    // A clean close is better evidence than the disappearance MONITOR inferred,
    // so it stands.
    let session = store
        .sessions()
        .get_session(&SessionId::from_string("SESS-1"))
        .await
        .expect("session");
    assert_eq!(session.end, Some(SessionEnd::Clean));
    assert!(!session.ended_uncleanly());
}

#[tokio::test]
async fn expire_lease_on_a_task_no_one_holds_is_a_noop() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;

    // A task that was never assigned has no lease to expire.
    assert!(
        store
            .expire_lease(&TaskId::from_string("TASK-1"), chrono::Utc::now())
            .await
            .expect("the operation")
            .is_none(),
        "nothing was reclaimed"
    );
    assert_eq!(
        store
            .tasks()
            .get_task(&TaskId::from_string("TASK-1"))
            .await
            .expect("task")
            .status,
        TaskStatus::Backlog,
        "the task was untouched"
    );
}

#[tokio::test]
async fn expire_lease_leaves_an_operators_offline_standing() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_and_start(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("assigned and started");

    // An operator took the agent offline deliberately, before its lease ran out.
    let mut agent = store
        .agents()
        .get_agent(&AgentId::from_string("AGENT-1"))
        .await
        .expect("agent");
    agent.status = AgentStatus::Offline;
    store.agents().update_agent(&agent).await.expect("offline");

    store
        .expire_lease(&TaskId::from_string("TASK-1"), chrono::Utc::now())
        .await
        .expect("the lease expired");

    // `Offline` is a stronger statement than a heartbeat's absence, so the
    // expiry does not soften it to `Disconnected`.
    assert_eq!(
        store
            .agents()
            .get_agent(&AgentId::from_string("AGENT-1"))
            .await
            .expect("agent")
            .status,
        AgentStatus::Offline
    );
}

#[tokio::test]
async fn acknowledge_assignment_attaches_the_session_and_marks_the_agent_busy() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_and_start(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("assigned and started");

    let (assignment, session) = store
        .acknowledge_assignment(
            &AssignmentId::from_string("ASG-1"),
            &session("SESS-1", "AGENT-1", "TASK-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("acknowledged");

    // The tenure now has the invocation doing the work behind it.
    assert_eq!(
        assignment.session_id,
        Some(SessionId::from_string("SESS-1"))
    );
    assert_eq!(session.task_id, Some(TaskId::from_string("TASK-1")));
    assert_eq!(session.status, SessionStatus::Active);

    // Both records agree after a reload, which is what the next loop tick reads.
    assert_eq!(
        store
            .assignments()
            .get_assignment(&AssignmentId::from_string("ASG-1"))
            .await
            .expect("assignment")
            .session_id,
        Some(SessionId::from_string("SESS-1"))
    );
    assert!(store
        .sessions()
        .get_session(&SessionId::from_string("SESS-1"))
        .await
        .is_ok());

    // The agent is working, so it cannot be handed more work.
    assert_eq!(
        store
            .agents()
            .get_agent(&AgentId::from_string("AGENT-1"))
            .await
            .expect("agent")
            .status,
        AgentStatus::Busy
    );
}

#[tokio::test]
async fn acknowledge_assignment_on_a_released_tenure_is_refused() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_and_start(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("assigned and started");
    store
        .assignments()
        .release_assignment(
            &AssignmentId::from_string("ASG-1"),
            ReleaseReason::ReleasedByOperator,
            1,
        )
        .await
        .expect("released");

    // Attaching a session to a tenure that has ended would give it evidence of
    // work it never did.
    let err = store
        .acknowledge_assignment(
            &AssignmentId::from_string("ASG-1"),
            &session("SESS-1", "AGENT-1", "TASK-1"),
            chrono::Utc::now(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintViolation(_)),
        "got {err:?}"
    );

    // The whole transaction rolled back: the session row the agent claims it
    // started is not stored against an ended tenure.
    assert!(
        store
            .sessions()
            .get_session(&SessionId::from_string("SESS-1"))
            .await
            .is_err(),
        "no session was recorded"
    );
}

#[tokio::test]
async fn acknowledge_assignment_leaves_an_operators_offline_standing() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_and_start(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("assigned and started");

    // An operator took the agent offline, and it reported starting anyway. The
    // session is recorded — the agent did start — but the operator's status is
    // not softened to `Busy`.
    let mut agent = store
        .agents()
        .get_agent(&AgentId::from_string("AGENT-1"))
        .await
        .expect("agent");
    agent.status = AgentStatus::Offline;
    store.agents().update_agent(&agent).await.expect("offline");

    store
        .acknowledge_assignment(
            &AssignmentId::from_string("ASG-1"),
            &session("SESS-1", "AGENT-1", "TASK-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("acknowledged");

    assert_eq!(
        store
            .agents()
            .get_agent(&AgentId::from_string("AGENT-1"))
            .await
            .expect("agent")
            .status,
        AgentStatus::Offline
    );
    assert!(
        store
            .sessions()
            .get_session(&SessionId::from_string("SESS-1"))
            .await
            .is_ok(),
        "the session was still recorded"
    );
}

// ---------------------------------------------------------------------------
// The atomic completion report: the thing the loop's MONITOR step calls when
// an agent says it finished.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn report_completion_awaits_verification_without_completing() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_and_start(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("assigned and started");

    let (assignment, task, session) = store
        .report_completion(&AssignmentId::from_string("ASG-1"), chrono::Utc::now())
        .await
        .expect("reported")
        .expect("there was a tenure to end");

    // The task awaits Orqyn's verdict. It is not `done` — no report an agent
    // makes can reach `done` — and the tenure is over.
    assert_eq!(task.status, TaskStatus::VerificationPending);
    assert_eq!(assignment.status, AssignmentStatus::Released);
    assert_eq!(assignment.release_reason, Some(ReleaseReason::WorkComplete));
    assert!(session.is_none(), "no session was ever acknowledged");
    assert!(
        store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string("TASK-1"))
            .await
            .expect("the query")
            .is_none(),
        "the task is Orqyn's to judge now"
    );

    // The move is in the history, read back from the store.
    let history = store
        .tasks()
        .task_history(&TaskId::from_string("TASK-1"))
        .await
        .expect("history");
    let last = history.last().expect("there is a transition");
    assert_eq!(last.from, TaskStatus::InProgress);
    assert_eq!(last.to, TaskStatus::VerificationPending);
}

#[tokio::test]
async fn report_completion_closes_the_session_cleanly_and_frees_the_agent() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_and_start(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("assigned and started");
    store
        .acknowledge_assignment(
            &AssignmentId::from_string("ASG-1"),
            &session("SESS-1", "AGENT-1", "TASK-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("acknowledged");

    let (_, _, session) = store
        .report_completion(&AssignmentId::from_string("ASG-1"), chrono::Utc::now())
        .await
        .expect("reported")
        .expect("there was a tenure to end");

    // The agent said it was done, which is a clean end — not a disappearance.
    let session = session.expect("the tenure had a session");
    assert_eq!(session.status, SessionStatus::Closed);
    assert_eq!(session.end, Some(SessionEnd::Clean));

    // The agent is free to take work again and holds nothing.
    let agent = store
        .agents()
        .get_agent(&AgentId::from_string("AGENT-1"))
        .await
        .expect("agent");
    assert_eq!(agent.status, AgentStatus::Available);
    assert_eq!(agent.current_task, None);
}

#[tokio::test]
async fn report_completion_leaves_an_operators_offline_standing() {
    // An operator took the agent offline, and it reported finishing anyway. The
    // tenure ends and the task awaits verification, but the operator's status
    // is not softened to `Available`.
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_and_start(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("assigned and started");
    let mut agent = store
        .agents()
        .get_agent(&AgentId::from_string("AGENT-1"))
        .await
        .expect("agent");
    agent.status = AgentStatus::Offline;
    store
        .agents()
        .update_agent(&agent)
        .await
        .expect("agent taken offline");

    store
        .report_completion(&AssignmentId::from_string("ASG-1"), chrono::Utc::now())
        .await
        .expect("reported");

    assert_eq!(
        store
            .agents()
            .get_agent(&AgentId::from_string("AGENT-1"))
            .await
            .expect("agent")
            .status,
        AgentStatus::Offline
    );
}

#[tokio::test]
async fn report_completion_does_not_end_a_tenure_that_was_reassigned() {
    // The race the id match exists for: ASG-1 held the task, then a
    // reassignment handed the task to AGENT-2 as ASG-2. A completion report for
    // ASG-1 must end nothing — and in particular must not release ASG-2 with
    // ASG-1's completion or touch the task ASG-1 no longer holds.
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_and_start(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("assigned and started");
    store
        .assign_task(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-2"),
            &AssignmentId::from_string("ASG-2"),
            chrono::Utc::now(),
        )
        .await
        .expect("reassigned");

    let outcome = store
        .report_completion(&AssignmentId::from_string("ASG-1"), chrono::Utc::now())
        .await
        .expect("the round");

    assert!(outcome.is_none(), "ASG-1 is not the tenure any more");

    // ASG-1 keeps the reason the reassignment gave it, not a completion.
    let asg_1 = store
        .assignments()
        .get_assignment(&AssignmentId::from_string("ASG-1"))
        .await
        .expect("ASG-1");
    assert_eq!(asg_1.release_reason, Some(ReleaseReason::Reassigned));

    // ASG-2 still holds the task, and the task is still in progress.
    assert_eq!(
        store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string("TASK-1"))
            .await
            .expect("the query")
            .expect("someone holds it")
            .id,
        AssignmentId::from_string("ASG-2")
    );
    assert_eq!(
        store
            .tasks()
            .get_task(&TaskId::from_string("TASK-1"))
            .await
            .expect("task")
            .status,
        TaskStatus::InProgress
    );
}

#[tokio::test]
async fn report_completion_on_a_tenure_that_ran_out_reports_none() {
    let (store, _path) = store().await;
    assignment_prereqs(&store).await;
    store
        .assign_and_start(
            &TaskId::from_string("TASK-1"),
            &AgentId::from_string("AGENT-1"),
            &AssignmentId::from_string("ASG-1"),
            chrono::Utc::now(),
        )
        .await
        .expect("assigned and started");
    store
        .expire_lease(&TaskId::from_string("TASK-1"), chrono::Utc::now())
        .await
        .expect("lease expired");

    let outcome = store
        .report_completion(&AssignmentId::from_string("ASG-1"), chrono::Utc::now())
        .await
        .expect("the round");

    assert!(outcome.is_none(), "the tenure already ended");
    // The expiry's write stands.
    assert_eq!(
        store
            .tasks()
            .get_task(&TaskId::from_string("TASK-1"))
            .await
            .expect("task")
            .status,
        TaskStatus::Todo
    );
}

// ---------------------------------------------------------------------------
// Checkpoints: superseded, never deleted.
// ---------------------------------------------------------------------------
fn checkpoint(id: &str, task: &str, next: &str) -> Checkpoint {
    Checkpoint::new(
        CheckpointId::from_string(id),
        TaskId::from_string(task),
        "Retries are idempotent.",
        "Three of five call sites updated.",
        next,
    )
}

#[tokio::test]
async fn creating_a_checkpoint_supersedes_the_previous_one_without_deleting_it() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");
    store
        .agents()
        .register_agent(&agent("AGENT-1"))
        .await
        .expect("agent");
    store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task");

    let first = store
        .checkpoints()
        .create_checkpoint(&checkpoint("CHK-1", "TASK-1", "update the last two sites"))
        .await
        .expect("first checkpoint");
    assert_eq!(first.status, CheckpointStatus::Current);

    // Creating a second current checkpoint for the same task must succeed, not
    // conflict: the latest-for-task pointer moves rather than being unique.
    store
        .checkpoints()
        .create_checkpoint(&checkpoint("CHK-2", "TASK-1", "run the retry suite"))
        .await
        .expect("second checkpoint");

    // The latest-for-task pointer moved.
    let latest = store
        .checkpoints()
        .latest_checkpoint(&TaskId::from_string("TASK-1"))
        .await
        .expect("the latest checkpoint")
        .expect("there is one");
    assert_eq!(latest.id, CheckpointId::from_string("CHK-2"));
    assert_eq!(latest.status, CheckpointStatus::Current);

    // And the first one is retained, marked superseded.
    let all = store
        .checkpoints()
        .checkpoints_for_task(&TaskId::from_string("TASK-1"))
        .await
        .expect("all checkpoints");
    assert_eq!(all.len(), 2, "the superseded checkpoint is retained");
    assert_eq!(
        all[0].id,
        CheckpointId::from_string("CHK-2"),
        "newest first"
    );
    assert_eq!(all[1].status, CheckpointStatus::Superseded);
}

#[tokio::test]
async fn latest_checkpoint_is_none_before_any_checkpoint_exists() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");
    store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task");

    assert!(store
        .checkpoints()
        .latest_checkpoint(&TaskId::from_string("TASK-1"))
        .await
        .expect("no checkpoint yet")
        .is_none());
}

#[tokio::test]
async fn a_checkpoint_carries_its_test_results_and_decisions() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");
    store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task");

    let mut cp = checkpoint("CHK-1", "TASK-1", "run the retry suite");
    cp.test_results = Some(TestResults {
        passed: 18,
        failed: 2,
        skipped: 1,
        failures: vec!["retry_budget_is_bounded".into()],
        run_at: chrono::Utc::now(),
    });
    cp.important_decisions = vec![director_domain::ids::DecisionId::from_string("DEC-1")];
    cp.current_assumptions = vec!["the retry budget is per-customer".into()];
    cp.changed_files = vec!["src/retry.rs".into()];

    let stored = store
        .checkpoints()
        .create_checkpoint(&cp)
        .await
        .expect("checkpoint stored");
    assert_eq!(stored.changed_files, vec!["src/retry.rs"]);
    assert_eq!(
        stored.current_assumptions,
        vec!["the retry budget is per-customer"]
    );
    assert_eq!(
        stored.important_decisions,
        vec![director_domain::ids::DecisionId::from_string("DEC-1")]
    );
    assert!(stored.test_results.is_some(), "test results round-trip");
}

// ---------------------------------------------------------------------------
// Plans: one authoritative plan per project, superseded never deleted.
// ---------------------------------------------------------------------------

fn plan(id: &str, project: &str, objective: &str) -> Plan {
    Plan::draft(
        PlanId::from_string(id),
        ProjectId::from_string(project),
        objective,
        "decomposed from the observed repository state",
    )
}

/// A plan with a pinned creation time, so ordering assertions are deterministic
/// rather than racing the clock.
fn dated_plan(id: &str, project: &str, objective: &str, days_ago: i64) -> Plan {
    let mut p = plan(id, project, objective);
    let when = chrono::Utc::now() - chrono::Duration::days(days_ago);
    p.created_at = when;
    p.updated_at = when;
    p
}

#[tokio::test]
async fn a_plan_round_trips_and_starts_as_a_draft() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");

    let mut draft = plan("PLAN-1", "PROJ-1", "ship the auth flow");
    draft.add_task(TaskId::from_string("AUTH-1"));
    draft.add_task(TaskId::from_string("AUTH-2"));

    let stored = store
        .plans()
        .create_plan(&draft)
        .await
        .expect("plan stored");
    assert_eq!(stored.status, PlanStatus::Draft, "a new plan is a draft");
    assert!(!stored.status.is_authoritative());
    assert_eq!(
        stored.task_ids,
        vec![TaskId::from_string("AUTH-1"), TaskId::from_string("AUTH-2")],
        "the ordered task list round-trips"
    );
    assert_eq!(stored.state_version, 1);

    // A draft is not authoritative, so nothing is executing against it yet.
    assert!(store
        .plans()
        .active_plan_for_project(&ProjectId::from_string("PROJ-1"))
        .await
        .expect("the query ran")
        .is_none());

    let by_id = store
        .plans()
        .get_plan(&PlanId::from_string("PLAN-1"))
        .await
        .expect("found by id");
    assert_eq!(by_id.objective, "ship the auth flow");
    assert_eq!(
        by_id.rationale,
        "decomposed from the observed repository state"
    );
}

#[tokio::test]
async fn activating_a_plan_supersedes_the_sitting_plan_without_deleting_it() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");

    let first = store
        .plans()
        .create_plan(&dated_plan("PLAN-1", "PROJ-1", "ship auth", 4))
        .await
        .expect("first plan");
    let active = store
        .plans()
        .activate_plan(&first.id, AgentId::from_string("AGENT-planner"))
        .await
        .expect("PLAN-1 activated");
    assert_eq!(active.status, PlanStatus::Active);
    assert_eq!(
        active.created_by,
        Some(AgentId::from_string("AGENT-planner")),
        "activation records who authorized it"
    );

    // A later plan for the same project, activated the same way.
    let second = store
        .plans()
        .create_plan(&dated_plan("PLAN-2", "PROJ-1", "ship auth and sessions", 1))
        .await
        .expect("second plan");
    let successor = store
        .plans()
        .activate_plan(&second.id, AgentId::from_string("AGENT-planner"))
        .await
        .expect("PLAN-2 activated");

    // The links go both ways.
    assert_eq!(successor.supersedes, Some(PlanId::from_string("PLAN-1")));
    let retired = store
        .plans()
        .get_plan(&PlanId::from_string("PLAN-1"))
        .await
        .expect("the retired plan");
    assert_eq!(retired.status, PlanStatus::Superseded);
    assert_eq!(retired.superseded_by, Some(PlanId::from_string("PLAN-2")));

    // Only the new plan is authoritative...
    let current = store
        .plans()
        .active_plan_for_project(&ProjectId::from_string("PROJ-1"))
        .await
        .expect("the query ran")
        .expect("there is one");
    assert_eq!(current.id, PlanId::from_string("PLAN-2"));

    // ...and the superseded one is still in the table, history intact.
    let all = store
        .plans()
        .plans_for_project(&ProjectId::from_string("PROJ-1"))
        .await
        .expect("all plans");
    assert_eq!(all.len(), 2, "the superseded plan is retained");
    assert_eq!(all[0].id, PlanId::from_string("PLAN-2"), "newest first");
    assert_eq!(all[1].id, PlanId::from_string("PLAN-1"));
}

#[tokio::test]
async fn a_second_active_plan_for_one_project_is_rejected() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");

    let first = store
        .plans()
        .create_plan(&plan("PLAN-1", "PROJ-1", "ship auth"))
        .await
        .expect("first plan");
    store
        .plans()
        .activate_plan(&first.id, AgentId::from_string("AGENT-planner"))
        .await
        .expect("PLAN-1 activated");
    let second = store
        .plans()
        .create_plan(&plan("PLAN-2", "PROJ-1", "ship sessions"))
        .await
        .expect("second plan");

    // `activate_plan` would supersede PLAN-1 first; going around it and setting
    // the status by hand must hit the partial unique index. A project is never
    // left with two authoritative plans, so "forgot to supersede" is an error
    // rather than a silent second plan.
    let mut rogue = second;
    rogue.status = PlanStatus::Active;
    let err = store.plans().update_plan(&rogue).await.unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintViolation(_)),
        "got {err:?}"
    );

    // The sitting plan is untouched: the refused write changed nothing.
    let current = store
        .plans()
        .active_plan_for_project(&ProjectId::from_string("PROJ-1"))
        .await
        .expect("the query ran")
        .expect("there is one");
    assert_eq!(current.id, PlanId::from_string("PLAN-1"));
}

#[tokio::test]
async fn activating_an_already_active_plan_is_refused() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");

    let created = store
        .plans()
        .create_plan(&plan("PLAN-1", "PROJ-1", "ship auth"))
        .await
        .expect("plan");
    store
        .plans()
        .activate_plan(&created.id, AgentId::from_string("AGENT-planner"))
        .await
        .expect("activated");

    // Activating it again is a caller mistake, and resurrecting a superseded
    // plan would rewrite history — both are refused by the same guard.
    let err = store
        .plans()
        .activate_plan(&created.id, AgentId::from_string("AGENT-planner"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintViolation(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_stale_plan_update_is_a_conflict_not_an_overwrite() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");

    let created = store
        .plans()
        .create_plan(&plan("PLAN-1", "PROJ-1", "ship auth"))
        .await
        .expect("plan");

    // A concurrent change the caller did not see: the store moves the version
    // to 2.
    let mut newer = created.clone();
    newer.objective = "ship auth and sessions".into();
    let written = store
        .plans()
        .update_plan(&newer)
        .await
        .expect("first update lands");
    assert_eq!(written.state_version, 2, "the store bumps the version");

    // The stale writer, still holding version 1, must not silently win.
    let mut stale = created;
    stale.rationale = "a stale rationale".into();
    let err = store.plans().update_plan(&stale).await.unwrap_err();
    assert!(
        matches!(err, StoreError::StateVersionConflict { .. }),
        "got {err:?}"
    );

    // The winner's write is what the store holds.
    let loaded = store
        .plans()
        .get_plan(&PlanId::from_string("PLAN-1"))
        .await
        .expect("plan loaded");
    assert_eq!(loaded.objective, "ship auth and sessions");
    assert_eq!(
        loaded.rationale,
        "decomposed from the observed repository state"
    );
    assert_eq!(loaded.state_version, 2);
}

// ---------------------------------------------------------------------------
// Decisions: reversed, never deleted.
// ---------------------------------------------------------------------------

fn decision(id: &str, title: &str) -> Decision {
    Decision::new(DecisionId::from_string(id), title, "the rationale")
        .in_task(TaskId::from_string("TASK-1"))
        .by(AgentId::from_string("AGENT-claude"))
        .considered("the option that lost")
}

#[tokio::test]
async fn a_decision_round_trips_with_its_provenance() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");
    store
        .agents()
        .register_agent(&agent("AGENT-1"))
        .await
        .expect("agent");
    store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task");

    let stored = store
        .decisions()
        .create_decision(&decision("DEC-1", "use stateless JWTs"))
        .await
        .expect("decision stored");
    assert!(stored.status.stands(), "a new decision stands");
    assert_eq!(stored.state_version, 1);
    assert_eq!(stored.task_id, Some(TaskId::from_string("TASK-1")));
    assert_eq!(stored.made_by, Some(AgentId::from_string("AGENT-claude")));
    assert_eq!(
        stored.alternatives_considered,
        vec!["the option that lost"],
        "rejected alternatives are retained"
    );

    let by_id = store
        .decisions()
        .get_decision(&DecisionId::from_string("DEC-1"))
        .await
        .expect("found by id");
    assert_eq!(by_id.rationale, "the rationale");
}

#[tokio::test]
async fn superseding_a_decision_marks_it_without_deleting_it() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");
    store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task");

    store
        .decisions()
        .create_decision(&decision("DEC-1", "use JWTs"))
        .await
        .expect("DEC-1");
    store
        .decisions()
        .create_decision(&decision("DEC-2", "use server sessions"))
        .await
        .expect("DEC-2");

    let reversed = store
        .decisions()
        .supersede_decision(
            &DecisionId::from_string("DEC-1"),
            &DecisionId::from_string("DEC-2"),
        )
        .await
        .expect("DEC-1 reversed");
    assert!(!reversed.status.stands());
    assert_eq!(
        reversed.superseded_by,
        Some(DecisionId::from_string("DEC-2"))
    );
    assert_eq!(reversed.state_version, 2, "supersession is a write");

    // The row is still there and still readable — reversal marks, it does not
    // delete.
    let still = store
        .decisions()
        .get_decision(&DecisionId::from_string("DEC-1"))
        .await
        .expect("reversed but present");
    assert_eq!(still.status, DecisionStatus::Superseded);
    assert_eq!(still.superseded_by, Some(DecisionId::from_string("DEC-2")));
}

#[tokio::test]
async fn superseding_an_already_reversed_decision_is_refused() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");

    store
        .decisions()
        .create_decision(&decision("DEC-1", "use JWTs"))
        .await
        .expect("DEC-1");
    store
        .decisions()
        .create_decision(&decision("DEC-2", "use server sessions"))
        .await
        .expect("DEC-2");
    store
        .decisions()
        .create_decision(&decision("DEC-3", "rotate keys monthly"))
        .await
        .expect("DEC-3");

    store
        .decisions()
        .supersede_decision(
            &DecisionId::from_string("DEC-1"),
            &DecisionId::from_string("DEC-2"),
        )
        .await
        .expect("DEC-1 reversed by DEC-2");

    // Reversing it again must fail: two successors would lose the first
    // reversal and leave the record claiming two things at once.
    let err = store
        .decisions()
        .supersede_decision(
            &DecisionId::from_string("DEC-1"),
            &DecisionId::from_string("DEC-3"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintViolation(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn decisions_for_a_task_are_newest_first_and_taskless_stay_out() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");
    store
        .agents()
        .register_agent(&agent("AGENT-1"))
        .await
        .expect("agent");
    store
        .tasks()
        .create_task(&task("TASK-1", "PROJ-1"))
        .await
        .expect("task");

    let mut earlier = decision("DEC-1", "use JWTs");
    earlier.made_at = chrono::Utc::now() - chrono::Duration::days(2);
    earlier.updated_at = earlier.made_at;
    store
        .decisions()
        .create_decision(&earlier)
        .await
        .expect("DEC-1");
    store
        .decisions()
        .create_decision(&decision("DEC-2", "rotate keys monthly"))
        .await
        .expect("DEC-2");

    // A decision with no task is provenance-free and belongs to no task's list.
    store
        .decisions()
        .create_decision(&Decision::new(
            DecisionId::from_string("DEC-9"),
            "use Alpine base images",
            "image size matters",
        ))
        .await
        .expect("DEC-9");

    let for_task = store
        .decisions()
        .decisions_for_task(&TaskId::from_string("TASK-1"))
        .await
        .expect("decisions for the task");
    assert_eq!(for_task.len(), 2, "the taskless decision is excluded");
    assert_eq!(
        for_task[0].id,
        DecisionId::from_string("DEC-2"),
        "newest first"
    );
    assert_eq!(for_task[1].id, DecisionId::from_string("DEC-1"));
}

// ---------------------------------------------------------------------------
// The atomic cancellation: the task and the reason it was abandoned land
// together, or neither lands.
// ---------------------------------------------------------------------------

/// A project and a task in the store, ready to be cancelled.
async fn cancellable_task(store: &Store) -> Task {
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");
    let mut task = task("TASK-1", "PROJ-1");
    task.status = TaskStatus::InProgress;
    let stored = store.tasks().create_task(&task).await.expect("task");
    // The caller's read of the task, the way REPLAN reads it before applying a
    // cancellation.
    store.tasks().get_task(&stored.id).await.expect("reloaded")
}

/// The decision a cancellation records, linked to the task it ends.
fn cancellation(id: &str, task: &Task) -> Decision {
    Decision::new(
        DecisionId::from_string(id),
        "cancel the task",
        "not worth it",
    )
    .in_task(task.id.clone())
    .by(AgentId::from_string("AGENT-claude"))
}

#[tokio::test]
async fn cancel_task_terminates_the_task_and_records_the_reason_together() {
    let (store, _path) = store().await;
    let task = cancellable_task(&store).await;

    let now = chrono::Utc::now();
    let (stored_task, stored_decision) = store
        .cancel_task(&task, &cancellation("DEC-1", &task), now)
        .await
        .expect("the cancellation");

    assert_eq!(stored_task.status, TaskStatus::Cancelled);
    assert_eq!(stored_task.state_version, task.state_version + 1);
    assert_eq!(stored_decision.task_id, Some(task.id.clone()));
    assert_eq!(stored_decision.rationale, "not worth it");

    // Both halves are readable from the store afterward, and the move to
    // `cancelled` is in the history.
    assert_eq!(
        store.tasks().get_task(&task.id).await.expect("task").status,
        TaskStatus::Cancelled
    );
    assert_eq!(
        store
            .decisions()
            .decisions_for_task(&task.id)
            .await
            .expect("decisions")
            .len(),
        1
    );
    let history = store.tasks().task_history(&task.id).await.expect("history");
    assert_eq!(
        history.last().expect("there is a transition").to,
        TaskStatus::Cancelled
    );
}

#[tokio::test]
async fn cancel_task_rolls_the_task_back_when_the_decision_cannot_be_written() {
    // The atomicity guarantee, exercised: the decision write fails — its id is
    // already taken — and the task must not be left `cancelled` without the
    // reason for it. Both halves roll back, because they were one transaction.
    let (store, _path) = store().await;
    let task = cancellable_task(&store).await;

    // Occupy the decision id the cancellation will use. This is the write that
    // fails; it is the failure the composite operation exists to survive. The
    // decision is deliberately taskless so that counting decisions for TASK-1
    // is a clean signal that the cancellation wrote nothing.
    store
        .decisions()
        .create_decision(&Decision::new(
            DecisionId::from_string("DEC-1"),
            "an earlier call",
            "already holds this id",
        ))
        .await
        .expect("the id is taken");

    let err = store
        .cancel_task(&task, &cancellation("DEC-1", &task), chrono::Utc::now())
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::ConstraintViolation(_)),
        "the duplicate id is a constraint violation, got {err:?}"
    );

    // The task is exactly where it was: still in flight, not terminal.
    assert_eq!(
        store
            .tasks()
            .get_task(&task.id)
            .await
            .expect("task still exists")
            .status,
        TaskStatus::InProgress,
        "the task was not cancelled without its reason"
    );
    // And the history records no move to `cancelled`.
    let history = store.tasks().task_history(&task.id).await.expect("history");
    assert!(
        history
            .iter()
            .all(|transition| transition.to != TaskStatus::Cancelled),
        "no cancellation transition was recorded"
    );
    // The only decision is the one that was already there.
    assert_eq!(
        store
            .decisions()
            .decisions_for_task(&task.id)
            .await
            .expect("decisions")
            .len(),
        0,
        "the cancellation decision did not partially land"
    );
}

#[tokio::test]
async fn cancel_task_on_a_stale_read_is_a_conflict_not_an_overwrite() {
    // The status move keeps the optimistic-concurrency key `update_task` uses,
    // so a cancellation built from a stale read cannot clobber a newer write.
    let (store, _path) = store().await;
    let task = cancellable_task(&store).await;

    // A newer write lands after the caller's read.
    let mut moved_on = task.clone();
    moved_on.status = TaskStatus::Failed;
    store
        .tasks()
        .update_task(&moved_on)
        .await
        .expect("the newer write");

    let err = store
        .cancel_task(&task, &cancellation("DEC-1", &task), chrono::Utc::now())
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::StateVersionConflict { .. }),
        "got {err:?}"
    );

    // The newer write stands, and no cancellation decision was recorded.
    assert_eq!(
        store.tasks().get_task(&task.id).await.expect("task").status,
        TaskStatus::Failed
    );
    assert_eq!(
        store
            .decisions()
            .decisions_for_task(&task.id)
            .await
            .expect("decisions")
            .len(),
        0
    );
}

// ---------------------------------------------------------------------------
// Normalized project state.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn project_state_is_replaced_on_each_observation() {
    let (store, _path) = store().await;
    store
        .projects()
        .create_project(&project("PROJ-1"))
        .await
        .expect("project");

    assert!(store
        .project_state()
        .get_project_state(&ProjectId::from_string("PROJ-1"))
        .await
        .expect("no state yet")
        .is_none());

    let first = StoredProjectState {
        project_id: ProjectId::from_string("PROJ-1"),
        repository_id: Some(RepositoryId::from_string("REPO-1")),
        branch: Some("main".into()),
        head_commit: "abc123".into(),
        working_tree_clean: true,
        observation_version: 1,
        last_observed_at: chrono::Utc::now(),
        state_version: 1,
    };
    store
        .project_state()
        .update_project_state(&first)
        .await
        .expect("first observation");

    // A later observation replaces it — one row per project, not an append.
    let mut second = first;
    second.head_commit = "def456".into();
    second.observation_version = 2;
    let updated = store
        .project_state()
        .update_project_state(&second)
        .await
        .expect("second observation");
    assert_eq!(updated.head_commit, "def456");

    let held = store
        .project_state()
        .get_project_state(&ProjectId::from_string("PROJ-1"))
        .await
        .expect("current state")
        .expect("there is one");
    assert_eq!(held.head_commit, "def456");
    assert_eq!(held.observation_version, 2);
}

// ---------------------------------------------------------------------------
// Provider sync: a pointer and a status, not a replica.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_sync_records_the_last_attempt_and_its_outcome() {
    let (store, _path) = store().await;

    assert!(store
        .provider_sync()
        .last_sync("TASK-1", "handoff-mcp")
        .await
        .expect("no sync yet")
        .is_none());

    let attempt = ProviderSync {
        entity_id: "TASK-1".into(),
        provider: "handoff-mcp".into(),
        external_id: Some("t-42".into()),
        last_sync_at: chrono::Utc::now(),
        last_success_at: None,
        last_error: Some("connection refused".into()),
        external_version: None,
    };
    store
        .provider_sync()
        .record_sync(&attempt)
        .await
        .expect("failed attempt recorded");

    let recorded = store
        .provider_sync()
        .last_sync("TASK-1", "handoff-mcp")
        .await
        .expect("the last sync")
        .expect("there is one");
    assert_eq!(recorded.external_id, Some("t-42".into()));
    assert_eq!(recorded.last_error, Some("connection refused".into()));

    // A later success replaces the failure, keyed on the same (entity, provider).
    let success = ProviderSync {
        last_success_at: Some(chrono::Utc::now()),
        last_error: None,
        external_version: Some("0.35.1".into()),
        ..attempt
    };
    store
        .provider_sync()
        .record_sync(&success)
        .await
        .expect("success recorded");
    let after = store
        .provider_sync()
        .last_sync("TASK-1", "handoff-mcp")
        .await
        .expect("the last sync")
        .expect("there is one");
    assert!(after.last_error.is_none());
    assert_eq!(after.external_version, Some("0.35.1".into()));
}

// ---------------------------------------------------------------------------
// Verifications: the durable record of Orqyn judging a task's work.
// ---------------------------------------------------------------------------

/// A task awaiting verification, in a store with a project to hang it off.
async fn task_awaiting_verification(store: &Store) -> Task {
    let project = ProjectId::from_string("PROJ-1");
    store
        .projects()
        .create_project(&Project::new(project.clone(), project.as_str(), "/nowhere"))
        .await
        .expect("project created");

    let mut task = Task::for_project(
        project,
        TaskId::from_string("AUTH-42"),
        "Auth",
        "POST /login returns a session cookie",
    );
    task.status = TaskStatus::VerificationPending;
    store
        .tasks()
        .create_task(&task)
        .await
        .expect("task created");
    task
}

/// One verification of the given task, carrying the given decisive evidence.
fn verification_of(id: &str, task: &Task, status: VerificationStatus) -> Verification {
    Verification::new(
        VerificationId::from_string(id),
        task.id.clone(),
        task.project_id.clone().expect("the task has a project"),
        None,
        status,
        vec![Evidence {
            kind: ProbeKind::TestSuite,
            criterion: "the suite passes".into(),
            status: if status == VerificationStatus::Failed {
                EvidenceStatus::Failed
            } else {
                EvidenceStatus::Passed
            },
            detail: "14 passed, 0 failed".into(),
        }],
        None,
        chrono::Utc::now(),
    )
}

#[tokio::test]
async fn a_passing_verification_moves_the_task_to_done() {
    let (store, _path) = store().await;
    let task = task_awaiting_verification(&store).await;

    let (verification, after) = store
        .apply_verification(
            &verification_of("VER-1", &task, VerificationStatus::Passed),
            chrono::Utc::now(),
        )
        .await
        .expect("the verification lands");

    // The verdict is recorded, and the task moved where the verdict says.
    assert_eq!(verification.status, VerificationStatus::Passed);
    assert_eq!(after.status, TaskStatus::Done);
    assert_eq!(after.id, task.id);

    // And the move is in the history, so "how did this become done" is
    // answerable from the store alone.
    let history = store.tasks().task_history(&task.id).await.expect("history");
    assert!(history
        .iter()
        .any(|t| t.from == TaskStatus::VerificationPending && t.to == TaskStatus::Done));
}

#[tokio::test]
async fn a_failing_verification_moves_the_task_to_failed() {
    let (store, _path) = store().await;
    let task = task_awaiting_verification(&store).await;

    let (_verification, after) = store
        .apply_verification(
            &verification_of("VER-1", &task, VerificationStatus::Failed),
            chrono::Utc::now(),
        )
        .await
        .expect("the verification lands");

    assert_eq!(after.status, TaskStatus::Failed);
    let history = store.tasks().task_history(&task.id).await.expect("history");
    assert!(history
        .iter()
        .any(|t| t.from == TaskStatus::VerificationPending && t.to == TaskStatus::Failed));
}

#[tokio::test]
async fn an_unverifiable_round_records_itself_and_moves_nothing() {
    // The design point this test holds: an unverifiable round *writes* — the
    // evidence trail that Orqyn looked is worth keeping — but it moves the task
    // nowhere, which is what makes resurveying it next tick safe.
    let (store, _path) = store().await;
    let task = task_awaiting_verification(&store).await;

    let (verification, after) = store
        .apply_verification(
            &verification_of("VER-1", &task, VerificationStatus::Unverifiable),
            chrono::Utc::now(),
        )
        .await
        .expect("the verification lands");

    assert_eq!(verification.status, VerificationStatus::Unverifiable);
    assert_eq!(after.status, TaskStatus::VerificationPending);

    // No transition was recorded, because nothing moved.
    let history = store.tasks().task_history(&task.id).await.expect("history");
    assert!(history
        .iter()
        .all(|t| t.to != TaskStatus::Done && t.to != TaskStatus::Failed));

    // But the round is in the history of judgments.
    assert!(store
        .verifications()
        .latest_verification(&task.id)
        .await
        .expect("there is a latest")
        .is_some());
}

#[tokio::test]
async fn a_task_reaching_done_always_has_a_verification_behind_it() {
    // The invariant the whole model exists to express, stated as a query: no
    // task reaches `done` except through a verification. Reading the two tables
    // together is what makes it checkable rather than something to believe.
    let (store, _path) = store().await;
    let task = task_awaiting_verification(&store).await;

    store
        .apply_verification(
            &verification_of("VER-1", &task, VerificationStatus::Passed),
            chrono::Utc::now(),
        )
        .await
        .expect("the verification lands");

    let after = store
        .tasks()
        .get_task(&task.id)
        .await
        .expect("task reloaded");
    assert_eq!(after.status, TaskStatus::Done);

    let verification = store
        .verifications()
        .latest_verification(&task.id)
        .await
        .expect("history readable")
        .expect("there is a verification for a done task");
    assert_eq!(verification.status, VerificationStatus::Passed);
    // The evidence trail is attached to the record, not to the task — so the
    // judgment survives even if the task is later cancelled or reworked.
    assert_eq!(verification.evidence.len(), 1);
}

#[tokio::test]
async fn rejudging_a_task_accumulates_history_instead_of_overwriting() {
    // A task that failed and was reworked passes on the second attempt: both
    // judgments stand, and a reader can see the whole story.
    let (store, _path) = store().await;
    let task = task_awaiting_verification(&store).await;

    store
        .apply_verification(
            &verification_of("VER-1", &task, VerificationStatus::Failed),
            chrono::Utc::now(),
        )
        .await
        .expect("first judgment lands");

    // The rework puts the task back in front of verification.
    let mut reworked = store
        .tasks()
        .get_task(&task.id)
        .await
        .expect("task reloaded");
    reworked.status = TaskStatus::VerificationPending;
    store
        .tasks()
        .update_task(&reworked)
        .await
        .expect("task requeued");

    std::thread::sleep(std::time::Duration::from_millis(10));
    store
        .apply_verification(
            &verification_of("VER-2", &reworked, VerificationStatus::Passed),
            chrono::Utc::now(),
        )
        .await
        .expect("second judgment lands");

    let history = store
        .verifications()
        .verifications_for_task(&task.id)
        .await
        .expect("history loaded");
    assert_eq!(history.len(), 2);
    // Newest first: the pass is what Orqyn currently believes.
    assert_eq!(history[0].status, VerificationStatus::Passed);
    assert_eq!(history[1].status, VerificationStatus::Failed);
    // The failed judgment was not deleted or rewritten — the evidence that the
    // first attempt did not pass is still readable.
    assert_eq!(history[1].id, VerificationId::from_string("VER-1"));
}

#[tokio::test]
async fn apply_verification_rolls_back_when_the_task_does_not_exist() {
    // The atomicity promise: a verification that cannot attach to a task writes
    // nothing. The foreign key is what makes the judgment-and-move pair
    // all-or-nothing at the store level.
    let (store, _path) = store().await;
    let task = task_awaiting_verification(&store).await;

    let mut orphan = verification_of("VER-1", &task, VerificationStatus::Passed);
    orphan.task_id = TaskId::from_string("NOPE-99");

    let err = store
        .apply_verification(&orphan, chrono::Utc::now())
        .await
        .expect_err("an orphan verification is refused");
    assert!(matches!(err, StoreError::ConstraintViolation(_)));

    // Nothing was written for the orphan, and the real task was untouched.
    assert!(store
        .verifications()
        .get_verification(&VerificationId::from_string("VER-1"))
        .await
        .is_err());
    assert_eq!(
        store
            .tasks()
            .get_task(&task.id)
            .await
            .expect("task intact")
            .status,
        TaskStatus::VerificationPending
    );
}
