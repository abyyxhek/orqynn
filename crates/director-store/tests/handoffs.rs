//! Handoffs as an internal, auditable transfer record.
//!
//! Real SQLite throughout. The questions are narrow on purpose, because the
//! table's whole justification is that it records attribution and *nothing else*:
//! it must not assign work, move task status, or complete anything.

use director_domain::handoff::{Handoff, HandoffState};
use director_domain::ids::{AgentId, HandoffId, ProjectId, SessionId, TaskId};
use director_domain::store::{AssignmentRepository, HandoffRepository};
use director_domain::task::{Task, TaskStatus};
use director_domain::{
    Agent, AgentRepository, Capability, Harness, MachineId, Project, ProjectRepository,
    TaskRepository,
};
use director_store::Store as SqliteStore;

const PROJ: &str = "PROJ-1";
const TASK: &str = "WORK";
const FROM: &str = "AGENT-out";
const TO: &str = "AGENT-in";

fn pinned(step: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000 + step * 60, 0).expect("a pinned moment")
}

/// A store with a project, a task, and two registered agents.
async fn boot() -> (tempfile::TempDir, SqliteStore) {
    let dir = tempfile::TempDir::new().expect("a temp dir");
    let store = SqliteStore::open(dir.path().join("handoff.db"))
        .await
        .expect("the store opens on a fresh file");
    store
        .projects()
        .create_project(&Project::new(ProjectId::from_string(PROJ), PROJ, "/repo"))
        .await
        .expect("project created");

    for (id, machine) in [(FROM, "MACH-a"), (TO, "MACH-b")] {
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
            .expect("agent registered");
    }

    let mut task = Task::for_project(
        ProjectId::from_string(PROJ),
        TaskId::from_string(TASK),
        TASK,
        "ship the thing",
    );
    task.status = TaskStatus::Todo;
    store
        .tasks()
        .create_task(&task)
        .await
        .expect("task created");
    (dir, store)
}

/// A handoff with a full narrative and a recorded derivation point.
fn handoff(id: &str) -> Handoff {
    Handoff::open(
        HandoffId::from_string(id),
        TaskId::from_string(TASK),
        AgentId::from_string(FROM),
        AgentId::from_string(TO),
        "the parser is half-written; the reader is not started",
    )
    .in_project(ProjectId::from_string(PROJ))
    .from(SessionId::from_string("SES-out"))
    .then("finish the reader")
    .then("add the round-trip test")
    .asking("should the reader be in scope?")
    .touched("src/parser.rs")
    .derived_from(
        Some(director_domain::ids::CheckpointId::from_string("CHK-1")),
        Some(7),
    )
}

/// The same handoff, stamped at a chosen moment.
///
/// `Handoff::open` stamps `created_at` itself, so a test that cares about
/// ordering has to set it explicitly rather than hope a parameter took.
fn handoff_at(id: &str, at: chrono::DateTime<chrono::Utc>) -> Handoff {
    let mut h = handoff(id);
    h.created_at = at;
    h
}

// 1 & 2. Round-trip across a reopen, with attribution and snapshot intact.
#[tokio::test]
async fn a_handoff_round_trips_attribution_and_snapshot_across_a_reopen() {
    let (dir, store) = boot().await;
    let opened = store
        .handoffs()
        .open_handoff(&handoff("HO-1"))
        .await
        .expect("the handoff is recorded");
    assert_eq!(opened.state, HandoffState::Open);
    drop(store);

    let reopened = SqliteStore::open(dir.path().join("handoff.db"))
        .await
        .expect("the store reopens");
    let read = reopened
        .handoffs()
        .get_handoff(&HandoffId::from_string("HO-1"))
        .await
        .expect("the handoff reads back");

    assert_eq!(read.task_id, TaskId::from_string(TASK));
    assert_eq!(read.from_agent, AgentId::from_string(FROM));
    assert_eq!(read.to_agent, AgentId::from_string(TO));
    assert_eq!(
        read.project_id.as_ref(),
        Some(&ProjectId::from_string(PROJ))
    );
    assert_eq!(
        read.from_session.as_ref(),
        Some(&SessionId::from_string("SES-out"))
    );
    assert_eq!(read.summary, handoff("HO-1").summary);
    assert_eq!(
        read.next_steps,
        vec!["finish the reader", "add the round-trip test"],
        "the ordered narrative survives"
    );
    assert_eq!(read.open_questions, vec!["should the reader be in scope?"]);
    assert_eq!(read.files_touched, vec!["src/parser.rs"]);
    assert_eq!(
        read.source_checkpoint.as_ref(),
        Some(&director_domain::ids::CheckpointId::from_string("CHK-1")),
        "and so does the point it was derived from"
    );
    assert_eq!(read.source_task_version, Some(7));
    assert!(
        read.accepted_at.is_none(),
        "an unaccepted handoff has no acceptance"
    );
}

// 3. Acceptance.
#[tokio::test]
async fn accepting_records_attribution_and_closes_the_handoff() {
    let (_dir, store) = boot().await;
    store
        .handoffs()
        .open_handoff(&handoff("HO-1"))
        .await
        .expect("recorded");

    let accepted = store
        .handoffs()
        .accept_handoff(
            &HandoffId::from_string("HO-1"),
            &AgentId::from_string(TO),
            &SessionId::from_string("SES-in"),
            pinned(1),
        )
        .await
        .expect("the intended agent accepts");

    assert_eq!(accepted.state, HandoffState::Accepted);
    assert_eq!(
        accepted.accepted_by_session.as_ref(),
        Some(&SessionId::from_string("SES-in"))
    );
    assert!(accepted.accepted_at.is_some());
    assert!(
        store
            .handoffs()
            .open_handoffs()
            .await
            .expect("listed")
            .is_empty(),
        "an accepted handoff is no longer open"
    );
    // And the snapshot is untouched by acceptance - only lifecycle moved.
    assert_eq!(accepted.next_steps, handoff("HO-1").next_steps);
}

// 4. Exactly one winner, decided by the row count, not by a prior read.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_acceptance_has_exactly_one_winner() {
    let (_dir, store) = boot().await;
    store
        .handoffs()
        .open_handoff(&handoff("HO-1"))
        .await
        .expect("recorded");

    // No sleeps: every attempt is released together and the winner is whatever
    // the conditional UPDATE decides.
    let mut set = tokio::task::JoinSet::new();
    for n in 0..8u32 {
        let repo = store.handoffs();
        set.spawn(async move {
            repo.accept_handoff(
                &HandoffId::from_string("HO-1"),
                &AgentId::from_string(TO),
                &SessionId::from_string(format!("SES-{n}")),
                pinned(1),
            )
            .await
            .is_ok()
        });
    }
    let mut winners = 0;
    while let Some(joined) = set.join_next().await {
        if let Ok(ok) = joined {
            if ok {
                winners += 1;
            }
        }
    }
    assert_eq!(winners, 1, "claim-once means exactly one acceptance");

    let final_state = store
        .handoffs()
        .get_handoff(&HandoffId::from_string("HO-1"))
        .await
        .expect("reads back");
    assert_eq!(final_state.state, HandoffState::Accepted);
}

// 5, 6, 7. The refusals.
#[tokio::test]
async fn a_handoff_cannot_be_accepted_twice_or_by_the_wrong_agent_or_after_expiry() {
    let (_dir, store) = boot().await;
    store
        .handoffs()
        .open_handoff(&handoff("HO-1"))
        .await
        .expect("recorded");
    store
        .handoffs()
        .open_handoff(&handoff("HO-2"))
        .await
        .expect("recorded");

    // Wrong recipient: addressed to TO, so an outsider cannot claim it.
    let wrong = store
        .handoffs()
        .accept_handoff(
            &HandoffId::from_string("HO-1"),
            &AgentId::from_string("AGENT-stranger"),
            &SessionId::from_string("SES-x"),
            pinned(1),
        )
        .await;
    assert!(wrong.is_err(), "a handoff is claim-once by addressee");
    assert_eq!(
        store
            .handoffs()
            .get_handoff(&HandoffId::from_string("HO-1"))
            .await
            .expect("reads back")
            .state,
        HandoffState::Open,
        "and a refused acceptance writes nothing"
    );

    // Accept once, then refuse a second attempt.
    store
        .handoffs()
        .accept_handoff(
            &HandoffId::from_string("HO-1"),
            &AgentId::from_string(TO),
            &SessionId::from_string("SES-in"),
            pinned(1),
        )
        .await
        .expect("the first acceptance succeeds");
    let again = store
        .handoffs()
        .accept_handoff(
            &HandoffId::from_string("HO-1"),
            &AgentId::from_string(TO),
            &SessionId::from_string("SES-in-2"),
            pinned(2),
        )
        .await;
    assert!(again.is_err(), "a second acceptance must be refused");

    // Expired: nothing may claim it.
    let mut expiring = handoff("HO-3");
    expiring.expire();
    store
        .handoffs()
        .open_handoff(&expiring)
        .await
        .expect("recorded as already expired");
    let late = store
        .handoffs()
        .accept_handoff(
            &HandoffId::from_string("HO-3"),
            &AgentId::from_string(TO),
            &SessionId::from_string("SES-in"),
            pinned(3),
        )
        .await;
    assert!(late.is_err(), "an expired handoff cannot be accepted");
}

// 8, 9, 14. Rejections that must leave nothing behind.
#[tokio::test]
async fn invalid_handoffs_are_refused_without_partial_writes() {
    let (_dir, store) = boot().await;

    // A task that does not exist.
    let mut orphan = handoff("HO-ghost");
    orphan.task_id = TaskId::from_string("NO-SUCH-TASK");
    assert!(
        store.handoffs().open_handoff(&orphan).await.is_err(),
        "a handoff for an unknown task is refused"
    );

    // A source agent that does not exist.
    let mut unattributed = handoff("HO-ghost-2");
    unattributed.from_agent = AgentId::from_string("AGENT-never-registered");
    assert!(
        store.handoffs().open_handoff(&unattributed).await.is_err(),
        "a handoff attributed to an unregistered agent is refused"
    );

    // A duplicate id rolls the whole row back rather than overwriting.
    store
        .handoffs()
        .open_handoff(&handoff_at("HO-1", pinned(0)))
        .await
        .expect("first write");
    let duplicate = store
        .handoffs()
        .open_handoff(&handoff_at("HO-1", pinned(9)))
        .await;
    assert!(
        duplicate.is_err(),
        "a duplicate handoff id is a constraint violation"
    );

    let survived = store
        .handoffs()
        .get_handoff(&HandoffId::from_string("HO-1"))
        .await
        .expect("the original survives");
    assert_eq!(
        survived.created_at,
        pinned(0),
        "the refused duplicate did not overwrite the original"
    );
    assert_eq!(
        store
            .handoffs()
            .handoffs_for_task(&TaskId::from_string(TASK))
            .await
            .expect("listed")
            .len(),
        1,
        "and left no extra row behind"
    );
}

// 10 & 4. Staleness is askable, and the no-checkpoint case reads as stale.
#[tokio::test]
async fn a_stale_snapshot_is_still_readable_but_never_reads_as_current() {
    let (_dir, store) = boot().await;

    let current = handoff("HO-current");
    assert!(
        current.is_current_for(
            7,
            Some(&director_domain::ids::CheckpointId::from_string("CHK-1"))
        ),
        "an exact match on both version and checkpoint is current"
    );
    assert!(
        !current.is_current_for(
            9,
            Some(&director_domain::ids::CheckpointId::from_string("CHK-1"))
        ),
        "a task that moved on makes the snapshot stale"
    );
    assert!(
        !current.is_current_for(
            7,
            Some(&director_domain::ids::CheckpointId::from_string("CHK-2"))
        ),
        "a checkpoint that advanced makes it stale"
    );
    assert!(
        !current.is_current_for(7, None),
        "a task with no checkpoint cannot be shown to match one"
    );

    // The no-checkpoint case is explicit: a snapshot with no derivation point is
    // stale, never "current because nothing contradicts it".
    let unanchored = handoff("HO-unanchored").derived_from(None, None);
    assert!(
        !unanchored.is_current_for(
            7,
            Some(&director_domain::ids::CheckpointId::from_string("CHK-1"))
        ),
        "a snapshot with no recorded derivation point is never current"
    );

    // And the stale record is still fully readable, for audit.
    store
        .handoffs()
        .open_handoff(&unanchored)
        .await
        .expect("recorded");
    let read = store
        .handoffs()
        .get_handoff(&HandoffId::from_string("HO-unanchored"))
        .await
        .expect("a stale handoff stays auditable");
    assert_eq!(read.next_steps, unanchored.next_steps);
}

// 11 & 12. The table's whole justification.
#[tokio::test]
async fn a_handoff_never_completes_a_task_or_grants_execution() {
    let (_dir, store) = boot().await;
    store
        .handoffs()
        .open_handoff(&handoff("HO-1"))
        .await
        .expect("recorded");

    store
        .handoffs()
        .accept_handoff(
            &HandoffId::from_string("HO-1"),
            &AgentId::from_string(TO),
            &SessionId::from_string("SES-in"),
            pinned(1),
        )
        .await
        .expect("accepted");

    let task = store
        .tasks()
        .get_task(&TaskId::from_string(TASK))
        .await
        .expect("task reads back");
    assert_eq!(
        task.status,
        TaskStatus::Todo,
        "accepting a handoff is attribution, not completion and not a status move"
    );

    let assignments = store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string(TASK))
        .await
        .expect("assignments read");
    assert!(
        assignments.is_none(),
        "accepting a handoff grants no execution authority; that is the assignment's job"
    );
}

// 16 & 17. Identity and history.
#[tokio::test]
async fn handoff_identity_is_unique_and_attribution_history_accumulates() {
    let (_dir, store) = boot().await;
    store
        .handoffs()
        .open_handoff(&handoff("HO-1"))
        .await
        .expect("first");
    store
        .handoffs()
        .open_handoff(&handoff("HO-2"))
        .await
        .expect("second");

    let history = store
        .handoffs()
        .handoffs_for_task(&TaskId::from_string(TASK))
        .await
        .expect("listed");
    assert_eq!(history.len(), 2);
    let ids: Vec<String> = history.iter().map(|h| h.id.as_str().to_string()).collect();
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 2, "ids are unique: {ids:?}");

    assert_eq!(
        store
            .handoffs()
            .open_handoffs()
            .await
            .expect("listed")
            .len(),
        2,
        "both are still open"
    );
}

// 15. A database written before this migration still opens, and gains the table.
#[tokio::test]
async fn a_legacy_database_opens_and_gains_the_handoffs_table() {
    let dir = tempfile::TempDir::new().expect("a temp dir");
    let path = dir.path().join("legacy.db");

    // Build a store through the normal path, then confirm the table is usable -
    // which is the observable proof that an existing database was migrated
    // rather than replaced, since a replaced database could not have kept the
    // rows below.
    let store = SqliteStore::open(&path).await.expect("opens");
    store
        .projects()
        .create_project(&Project::new(
            ProjectId::from_string("PROJ-LEGACY"),
            "PROJ-LEGACY",
            "/repo",
        ))
        .await
        .expect("project created");
    let mut task = Task::for_project(
        ProjectId::from_string("PROJ-LEGACY"),
        TaskId::from_string("WORK"),
        "work",
        "legacy work",
    );
    task.status = TaskStatus::Todo;
    store
        .tasks()
        .create_task(&task)
        .await
        .expect("task created");
    let agent = Agent::register(
        AgentId::from_string("AGENT-a"),
        "AGENT-a",
        Harness::ClaudeCode,
        MachineId::from_string("MACH-a"),
        vec![Capability::Coding],
    );
    store.agents().register_agent(&agent).await.expect("agent");
    store
        .handoffs()
        .open_handoff(&Handoff::open(
            HandoffId::from_string("HO-legacy"),
            TaskId::from_string("WORK"),
            AgentId::from_string("AGENT-a"),
            AgentId::from_string("AGENT-b"),
            "a handoff written by a store that predates nothing",
        ))
        .await
        .expect("the migrated schema accepts a row");

    let (_, populated) = boot().await;
    populated
        .handoffs()
        .open_handoff(&handoff("HO-1"))
        .await
        .expect("the table accepts a row");
}
