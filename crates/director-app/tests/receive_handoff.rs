//! Receiving a handoff: what a replacement agent may rely on, and may not.
//!
//! Real SQLite throughout. The property under test is the module's whole reason
//! for existing: acceptance is attribution, the brief comes from authoritative
//! state, and the outgoing agent's narrative never becomes either.

use director_app::brief::BriefLimits;
use director_app::handoff::{
    accept, cause_phrase, continuation, inspect, list_for_task, Freshness, ReceiveError,
};
use director_domain::handoff::{Handoff, HandoffCause, HandoffState};
use director_domain::ids::{AgentId, CheckpointId, HandoffId, ProjectId, SessionId, TaskId};
use director_domain::store::{AssignmentRepository, HandoffRepository, TaskRepository};
use director_domain::task::{Task, TaskStatus};
use director_domain::{
    Agent, AgentRepository, Capability, Harness, MachineId, Project, ProjectRepository,
    ResumeStatus,
};
use director_store::Store as SqliteStore;

const PROJ: &str = "PROJ-1";
const TASK: &str = "WORK";
const OUT: &str = "AGENT-out";
const IN: &str = "AGENT-in";
const STRANGER: &str = "AGENT-stranger";

fn pinned(step: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000 + step * 60, 0).expect("a pinned moment")
}

async fn boot() -> (tempfile::TempDir, SqliteStore) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let store = SqliteStore::open(dir.path().join("receive.db"))
        .await
        .expect("opens");
    store
        .projects()
        .create_project(&Project::new(ProjectId::from_string(PROJ), PROJ, "/repo"))
        .await
        .expect("project");
    for id in [OUT, IN, STRANGER] {
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
    (dir, store)
}

fn handoff(id: &str, to: &str, at: i64) -> Handoff {
    let mut record = Handoff::open(
        HandoffId::from_string(id),
        TaskId::from_string(TASK),
        AgentId::from_string(OUT),
        AgentId::from_string(to),
        "the outgoing agent's own account of the work",
    )
    .in_project(ProjectId::from_string(PROJ))
    .from(SessionId::from_string("SES-out"))
    .then("finish the reader")
    .caused_by(HandoffCause::Displaced);
    record.created_at = pinned(at);
    record
}

// Acceptance succeeds for the addressed agent, and records attribution only.
#[tokio::test]
async fn accepting_records_attribution_and_grants_nothing() {
    let (_dir, store) = boot().await;
    store
        .handoffs()
        .open_handoff(&handoff("HO-1", IN, 0))
        .await
        .expect("recorded");

    let accepted = accept(
        &store,
        &HandoffId::from_string("HO-1"),
        &AgentId::from_string(IN),
        &SessionId::from_string("SES-in"),
        pinned(1),
    )
    .await
    .expect("the addressed agent accepts");
    assert_eq!(accepted.state, HandoffState::Accepted);

    // The task did not move, and no assignment was implied.
    let task = store
        .tasks()
        .get_task(&TaskId::from_string(TASK))
        .await
        .expect("task");
    assert_eq!(
        task.status,
        TaskStatus::Todo,
        "acceptance is attribution: it completes nothing and moves nothing"
    );
    let sitting = store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string(TASK))
        .await
        .expect("assignments");
    assert!(
        sitting.is_none(),
        "and it transfers no execution authority; only ASSIGN does that"
    );
}

// Claim-once, addressee-only, and terminal states.
#[tokio::test]
async fn refusals_name_the_rule_that_stopped_them() {
    let (_dir, store) = boot().await;
    store
        .handoffs()
        .open_handoff(&handoff("HO-1", IN, 0))
        .await
        .expect("recorded");

    let stranger = accept(
        &store,
        &HandoffId::from_string("HO-1"),
        &AgentId::from_string(STRANGER),
        &SessionId::from_string("SES-x"),
        pinned(1),
    )
    .await;
    assert!(
        matches!(stranger, Err(ReceiveError::WrongRecipient { .. })),
        "claim-once means addressee-only, got {stranger:?}"
    );

    // Unknown id.
    assert!(matches!(
        accept(
            &store,
            &HandoffId::from_string("HO-NOPE"),
            &AgentId::from_string(IN),
            &SessionId::from_string("SES-in"),
            pinned(1),
        )
        .await,
        Err(ReceiveError::UnknownHandoff(_))
    ));

    // Accept once, then refuse the second.
    accept(
        &store,
        &HandoffId::from_string("HO-1"),
        &AgentId::from_string(IN),
        &SessionId::from_string("SES-in"),
        pinned(1),
    )
    .await
    .expect("first acceptance");
    let again = accept(
        &store,
        &HandoffId::from_string("HO-1"),
        &AgentId::from_string(IN),
        &SessionId::from_string("SES-in-2"),
        pinned(2),
    )
    .await;
    assert!(matches!(again, Err(ReceiveError::NotOpen { .. })));

    // A terminal task has nothing to take over.
    store
        .handoffs()
        .open_handoff(&handoff("HO-2", IN, 0))
        .await
        .expect("recorded");
    let mut done = store
        .tasks()
        .get_task(&TaskId::from_string(TASK))
        .await
        .expect("task");
    done.status = TaskStatus::Done;
    store.tasks().update_task(&done).await.expect("updated");

    let terminal = accept(
        &store,
        &HandoffId::from_string("HO-2"),
        &AgentId::from_string(IN),
        &SessionId::from_string("SES-in"),
        pinned(3),
    )
    .await;
    assert!(
        matches!(terminal, Err(ReceiveError::TaskTerminal { .. })),
        "receiving a done task is meaningless, got {terminal:?}"
    );
}

// Concurrent acceptance resolves to exactly one winner.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_acceptance_has_exactly_one_winner() {
    let (_dir, store) = boot().await;
    store
        .handoffs()
        .open_handoff(&handoff("HO-1", IN, 0))
        .await
        .expect("recorded");

    let mut set = tokio::task::JoinSet::new();
    for n in 0..8u32 {
        let repo = store.clone();
        set.spawn(async move {
            accept(
                &repo,
                &HandoffId::from_string("HO-1"),
                &AgentId::from_string(IN),
                &SessionId::from_string(format!("SES-{n}")),
                pinned(1),
            )
            .await
            .is_ok()
        });
    }
    let mut winners = 0;
    while let Some(joined) = set.join_next().await {
        if let Ok(true) = joined {
            winners += 1;
        }
    }
    assert_eq!(winners, 1, "claim-once means exactly one acceptance");
}

// Stale provenance is explicit, and does not block receiving.
#[tokio::test]
async fn stale_provenance_is_reported_and_missing_provenance_never_reads_as_current() {
    let (_dir, store) = boot().await;
    let no_source = handoff("HO-stale", IN, 0).derived_from(None, None);
    store
        .handoffs()
        .open_handoff(&no_source)
        .await
        .expect("recorded");

    let view = inspect(&store, &HandoffId::from_string("HO-stale"))
        .await
        .expect("inspectable");
    assert!(
        matches!(view.freshness, Freshness::Stale { .. }),
        "a snapshot with no derivation point is never current"
    );

    // Stale does not prevent receiving - the receiver simply rebuilds from
    // authoritative state, which is the whole point.
    let accepted = accept(
        &store,
        &HandoffId::from_string("HO-stale"),
        &AgentId::from_string(IN),
        &SessionId::from_string("SES-in"),
        pinned(1),
    )
    .await;
    assert!(accepted.is_ok(), "staleness changes trust, not eligibility");

    assert_eq!(
        cause_phrase(None),
        "unknown - not established from persisted records",
        "a record with no cause says so rather than guessing"
    );
}

// The continuation brief is built from authoritative state, not the narrative.
#[tokio::test]
async fn continuation_comes_from_recovery_and_keeps_narrative_as_context() {
    let (_dir, store) = boot().await;
    store
        .handoffs()
        .open_handoff(&handoff("HO-1", IN, 0))
        .await
        .expect("recorded");
    accept(
        &store,
        &HandoffId::from_string("HO-1"),
        &AgentId::from_string(IN),
        &SessionId::from_string("SES-in"),
        pinned(1),
    )
    .await
    .expect("accepted");

    let continuation = continuation(&store, &TaskId::from_string(TASK), BriefLimits::default())
        .await
        .expect("a continuation is assembled");

    assert_eq!(
        continuation.context.len(),
        1,
        "the narrative travels as context"
    );
    assert_eq!(
        continuation.accepted_from.as_ref(),
        Some(&AgentId::from_string(OUT)),
        "and attribution says who handed over"
    );
    // The brief is rendered from the task and whatever recovery exists - never
    // from the handoff. An unchecked task has no recovery package, which is
    // reported rather than papered over with the narrative.
    assert!(
        continuation.brief.text.contains("the work"),
        "the brief is rendered from the task: {}",
        continuation.brief.text
    );
    assert!(
        !continuation
            .brief
            .text
            .contains("outgoing agent's own account"),
        "the narrative must never be merged into the brief"
    );
    assert_eq!(
        continuation.resume_status,
        ResumeStatus::Insufficient,
        "with no checkpoint yet there is nothing to resume, and that is said"
    );
}

// A handoff for a task that does not exist cannot be written at all.
#[tokio::test]
async fn a_handoff_for_an_unknown_task_cannot_be_recorded_in_the_first_place() {
    let (_dir, store) = boot().await;
    let mut orphan = handoff("HO-ghost", IN, 0);
    orphan.task_id = TaskId::from_string("NO-SUCH-TASK");

    let written = store.handoffs().open_handoff(&orphan).await;
    assert!(
        written.is_err(),
        "the store refuses a handoff naming a task it has no record of, so \
         `ReceiveError::TaskGone` is defensive rather than reachable by a normal write"
    );
    assert!(
        list_for_task(&store, &TaskId::from_string("NO-SUCH-TASK"))
            .await
            .expect("listed")
            .is_empty(),
        "and nothing was persisted for it"
    );
}

// Listing is plain and ordered.
#[tokio::test]
async fn listing_returns_records_newest_first() {
    let (_dir, store) = boot().await;
    store
        .handoffs()
        .open_handoff(&handoff("HO-1", IN, 0))
        .await
        .expect("first");
    store
        .handoffs()
        .open_handoff(&handoff("HO-2", IN, 1))
        .await
        .expect("second");

    let listed = list_for_task(&store, &TaskId::from_string(TASK))
        .await
        .expect("listed");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id.as_str(), "HO-2", "newest first");
    assert_eq!(
        cause_phrase(Some(HandoffCause::Displaced)),
        "displaced while active by another agent"
    );
    let _ = CheckpointId::from_string("CHK-1");
}

// ---------------------------------------------------------------------------
// Acceptance transaction failure
// ---------------------------------------------------------------------------

/// A failed acceptance write leaves the prior valid state exactly as it was.
///
/// The fault is induced the same deterministic way Phase 16.1 established: the
/// conditional `UPDATE ... WHERE state = 'open'` is what decides acceptance, so a
/// record that has already left `open` makes the write match no row. The second
/// acceptance is refused by the repository for precisely that reason - which is
/// the production code path, not a simulation.
///
/// This would *not* fail if acceptance were written as a read-then-write in two
/// transactions: the read would see `open`, the write would succeed, and a
/// duplicate acceptance would be recorded.
#[tokio::test]
async fn a_failed_acceptance_write_preserves_the_prior_valid_state() {
    let (_dir, store) = boot().await;
    store
        .handoffs()
        .open_handoff(&handoff("HO-1", IN, 0))
        .await
        .expect("recorded");

    // First acceptance succeeds and establishes the valid state.
    let accepted = accept(
        &store,
        &HandoffId::from_string("HO-1"),
        &AgentId::from_string(IN),
        &SessionId::from_string("SES-first"),
        pinned(1),
    )
    .await
    .expect("first acceptance");
    let before = store
        .handoffs()
        .get_handoff(&HandoffId::from_string("HO-1"))
        .await
        .expect("reads back");

    // Second acceptance fails: the record is no longer open.
    let refused = accept(
        &store,
        &HandoffId::from_string("HO-1"),
        &AgentId::from_string(IN),
        &SessionId::from_string("SES-second"),
        pinned(2),
    )
    .await;
    assert!(
        matches!(refused, Err(ReceiveError::NotOpen { .. })),
        "a duplicate acceptance is refused: {refused:?}"
    );

    // Nothing partial: the accepted record is byte-for-byte what it was, and in
    // particular the *refused* session did not become the acceptor.
    let after = store
        .handoffs()
        .get_handoff(&HandoffId::from_string("HO-1"))
        .await
        .expect("reads back");
    assert_eq!(after, before, "the refused write changed nothing");
    assert_eq!(
        after.accepted_by_session.as_ref(),
        Some(&SessionId::from_string("SES-first")),
        "the first acceptor still stands; the refused one was not recorded"
    );
    assert_eq!(after.accepted_at, accepted.accepted_at);

    // And the task is untouched by both.
    assert_eq!(
        store
            .tasks()
            .get_task(&TaskId::from_string(TASK))
            .await
            .expect("task")
            .status,
        TaskStatus::Todo,
        "acceptance, successful or refused, moves nothing"
    );
    assert_eq!(
        list_for_task(&store, &TaskId::from_string(TASK))
            .await
            .expect("listed")
            .len(),
        1,
        "and no partial record was left behind"
    );
}
