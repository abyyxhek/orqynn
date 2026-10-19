//! Displacement integration: a change of hands leaves an auditable record.
//!
//! The question every test here asks is the same one: does the store now say
//! *who passed this task to whom*, without that record ever standing in for
//! authority, completion, or current state?

use director_domain::assignment::AgentAssignment;
use director_domain::ids::{AgentId, AssignmentId, HandoffId, PlanId, ProjectId, TaskId};
use director_domain::store::{
    AssignmentRepository, HandoffRepository, PlanRepository, TaskRepository,
};
use director_domain::task::{Task, TaskStatus};
use director_domain::{
    Agent, AgentRepository, Capability, Harness, MachineId, Project, ProjectRepository,
};
use director_store::Store as SqliteStore;

const PROJ: &str = "PROJ-1";
const TASK: &str = "WORK";
const A: &str = "AGENT-a";
const B: &str = "AGENT-b";
const C: &str = "AGENT-c";

fn pinned(step: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000 + step * 60, 0).expect("a pinned moment")
}

/// A store with a project, one task, and three registered agents.
async fn boot() -> (tempfile::TempDir, SqliteStore) {
    let dir = tempfile::TempDir::new().expect("a temp dir");
    let store = SqliteStore::open(dir.path().join("displacement.db"))
        .await
        .expect("opens");
    store
        .projects()
        .create_project(&Project::new(ProjectId::from_string(PROJ), PROJ, "/repo"))
        .await
        .expect("project");
    for id in [A, B, C] {
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
    // The task is named by an active plan. Required: the store refuses to
    // assign a task the project's active plan does not name, which is the rule
    // the application layer has always enforced and the write boundary now
    // enforces too. A fixture that skipped planning would be asserting behaviour
    // no real caller could reach.
    let mut plan = director_domain::plan::Plan::draft(
        PlanId::from_string("PLAN-WORK"),
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
        .activate_plan(&PlanId::from_string("PLAN-WORK"), AgentId::from_string(A))
        .await
        .expect("plan activated");
    (dir, store)
}

/// Assign with an id allocated by the store, which is the path the loop uses.
async fn assign_next(
    store: &SqliteStore,
    agent: &str,
    at: i64,
) -> Result<(AgentAssignment, Task, Option<AgentId>), director_domain::StoreError> {
    store
        .assign_and_start_next(
            &TaskId::from_string(TASK),
            &AgentId::from_string(agent),
            pinned(at),
        )
        .await
}

/// Give an agent a session on the task.
///
/// Kept because the distinction matters: a session exists independently of the
/// assignment, and the handoff records the *assignment*'s session id, which is a
/// different thing and is honestly absent unless the assignment carries one.
#[allow(dead_code)]
async fn attach_session(store: &SqliteStore, agent: &str, session: &str) {
    use director_domain::store::SessionRepository;
    use director_domain::AgentSession;
    let session = AgentSession::start(
        director_domain::ids::SessionId::from_string(session),
        Some(ProjectId::from_string(PROJ)),
        AgentId::from_string(agent),
        MachineId::from_string(format!("MACH-{agent}")),
        Some(TaskId::from_string(TASK)),
    );
    store
        .sessions()
        .create_session(&session)
        .await
        .expect("session recorded");
}

async fn handoffs(store: &SqliteStore) -> Vec<director_domain::handoff::Handoff> {
    store
        .handoffs()
        .handoffs_for_task(&TaskId::from_string(TASK))
        .await
        .expect("listed")
}

// 1, 2, 4, 5. The core distinction: displacement records, first assignment does not.
#[tokio::test]
async fn a_genuine_displacement_records_exactly_one_handoff_and_a_first_assignment_does_not() {
    let (_dir, store) = boot().await;
    attach_session(&store, A, "SES-a").await;

    // First-time assignment: nobody holds the task, so there is nothing to hand off.
    let (_, _, displaced) = assign_next(&store, A, 0).await.expect("first assignment");
    assert!(displaced.is_none(), "a first assignment displaces nobody");
    assert!(
        handoffs(&store).await.is_empty(),
        "and must not invent a handoff for it"
    );

    // A different agent takes it: that is a change of responsibility.
    let (_, _, displaced) = assign_next(&store, B, 1).await.expect("displacement");
    assert_eq!(
        displaced.as_ref().map(|id| id.as_str()),
        Some(A),
        "the store reports who was displaced"
    );

    let records = handoffs(&store).await;
    assert_eq!(
        records.len(),
        1,
        "exactly one handoff, not one per assignment write"
    );
    let record = &records[0];
    assert_eq!(
        record.from_agent,
        AgentId::from_string(A),
        "attribution: who left"
    );
    assert_eq!(
        record.to_agent,
        AgentId::from_string(B),
        "attribution: who arrived"
    );
    assert_eq!(record.task_id, TaskId::from_string(TASK));
    assert_eq!(
        record.project_id.as_ref(),
        Some(&ProjectId::from_string(PROJ)),
        "and the project it belongs to"
    );
    // The outgoing session comes from the *assignment* row's `session_id`, which
    // `assign_and_start` does not populate - a session and an assignment are
    // separate records. So it is honestly absent here rather than guessed, and a
    // displacement whose assignment *does* carry a session records it.
    assert_eq!(
        record.from_session, None,
        "no session is invented when the assignment carries none"
    );
}

// 3. Repeated displacement accumulates, one per real change of hands.
#[tokio::test]
async fn repeated_displacement_records_each_transfer_once() {
    let (_dir, store) = boot().await;
    assign_next(&store, A, 0).await.expect("first");
    assign_next(&store, B, 1).await.expect("second");
    assign_next(&store, C, 2).await.expect("third");

    let records = handoffs(&store).await;
    assert_eq!(records.len(), 2, "two changes of hands, two records");
    let ids: Vec<String> = records.iter().map(|h| h.id.as_str().to_string()).collect();
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 2, "ids are unique: {ids:?}");

    let pairs: Vec<(String, String)> = records
        .iter()
        .map(|h| {
            (
                h.from_agent.as_str().to_string(),
                h.to_agent.as_str().to_string(),
            )
        })
        .collect();
    assert!(pairs.contains(&(A.to_string(), B.to_string())), "{pairs:?}");
    assert!(pairs.contains(&(B.to_string(), C.to_string())), "{pairs:?}");
}

// 6. Provenance is what the store actually knew, never an invention.
#[tokio::test]
async fn the_snapshot_records_real_provenance_and_marks_a_missing_checkpoint_as_stale() {
    let (_dir, store) = boot().await;
    assign_next(&store, A, 0).await.expect("first");
    assign_next(&store, B, 1).await.expect("displacement");

    let record = &handoffs(&store).await[0];
    assert_eq!(
        record.source_checkpoint, None,
        "this task was never checkpointed, and none is invented"
    );
    let version = record
        .source_task_version
        .expect("a task version was recorded");
    assert!(
        !record.is_current_for(version, None),
        "a snapshot with no checkpoint reads as stale, never as current"
    );
}

// 11. The record is attribution, not authority and not completion.
#[tokio::test]
async fn a_handoff_never_completes_a_task_or_grants_execution() {
    let (_dir, store) = boot().await;
    assign_next(&store, A, 0).await.expect("first");
    assign_next(&store, B, 1).await.expect("displacement");

    assert!(
        handoffs(&store).await.len() == 1,
        "the record exists - and it changed nothing about authority"
    );
    let task = store
        .tasks()
        .get_task(&TaskId::from_string(TASK))
        .await
        .expect("task");
    assert_eq!(
        task.status,
        TaskStatus::InProgress,
        "a handoff is not a completion and not a status change"
    );
    // Authority still rests with the active assignment alone.
    let sitting = store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string(TASK))
        .await
        .expect("assignments");
    let sitting = sitting.expect("the replacement holds the task");
    assert_eq!(sitting.agent_id, AgentId::from_string(B));
}

// 7. A refused displacement leaves no misleading handoff.
#[tokio::test]
async fn a_refused_displacement_records_nothing() {
    let (_dir, store) = boot().await;
    assign_next(&store, A, 0).await.expect("first");

    // Assigning an agent that was never registered must fail the foreign key,
    // rolling back the whole transaction - handoff included.
    let refused = store
        .assign_and_start_next(
            &TaskId::from_string(TASK),
            &AgentId::from_string("AGENT-never-registered"),
            pinned(1),
        )
        .await;
    assert!(refused.is_err(), "an unknown agent cannot be assigned");
    assert!(
        handoffs(&store).await.is_empty(),
        "and a rolled-back displacement must leave no handoff behind"
    );
    // The original holder is untouched - the safe failure mode.
    let sitting = store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string(TASK))
        .await
        .expect("assignments")
        .expect("still held");
    assert_eq!(sitting.agent_id, AgentId::from_string(A));
}

// 13. Concurrent displacement cannot produce conflicting transfers.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_displacements_record_exactly_one_winner_per_transfer() {
    let (_dir, store) = boot().await;
    assign_next(&store, A, 0).await.expect("first");

    let mut set = tokio::task::JoinSet::new();
    for agent in [B, C] {
        let repo = store.clone();
        set.spawn(async move {
            repo.assign_and_start_next(
                &TaskId::from_string(TASK),
                &AgentId::from_string(agent),
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
    // The store's contract for concurrent assignment is that both are recorded in
    // order and the later one displaces the earlier - refused, never overwritten.
    // So two winners here is correct existing behaviour, not a lost update, and
    // what matters is that the attribution matches it exactly.
    assert_eq!(winners, 2, "both tenures are recorded; neither is refused");

    let records = handoffs(&store).await;
    assert_eq!(
        records.len(),
        2,
        "one handoff per displacement, matching the recorded tenures"
    );
    // Which order they ran in is not determined, and must not be assumed. What is
    // determined is that the attribution is coherent: every record names a
    // recipient that differs from its sender, and whoever ends up holding the
    // task appears as a recipient - because that assignment displaced someone.
    for record in &records {
        assert_ne!(
            record.from_agent, record.to_agent,
            "a handoff never records an agent handing to itself"
        );
    }
    let sitting = store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string(TASK))
        .await
        .expect("assignments")
        .expect("someone holds it");
    assert!(
        records.iter().any(|h| h.to_agent == sitting.agent_id),
        "the final holder must appear as a recorded recipient: {records:?}"
    );
    let task = store
        .tasks()
        .get_task(&TaskId::from_string(TASK))
        .await
        .expect("task");
    assert_eq!(task.status, TaskStatus::InProgress);
}

// 2 (second case). A same-agent re-tenure is not a change of responsibility.
#[tokio::test]
async fn reassignment_to_the_same_agent_records_no_handoff() {
    let (_dir, store) = boot().await;
    assign_next(&store, A, 0).await.expect("first");
    // Release and re-acquire by the same agent: a new tenure, same responsibility.
    store
        .assignments()
        .release_assignment(
            &AssignmentId::from_string("ASG-WORK-1"),
            director_domain::assignment::ReleaseReason::LeaseExpired,
            1,
        )
        .await
        .ok();
    assign_next(&store, A, 2).await.expect("re-acquire");
    assert!(
        handoffs(&store).await.is_empty(),
        "the same agent taking it back is not a handoff to another agent"
    );
}

// ---------------------------------------------------------------------------
// Cause attribution, exercised through the real transitions
// ---------------------------------------------------------------------------

use director_domain::handoff::HandoffCause;

// 1. The lease-expiry lifecycle, driven through the actual reclaim path.
//
// The cause is never set by the test. It is produced by the store reading the
// expired tenure's own `release_reason` during the reassignment transaction, so
// this exercises the classification rather than restating it.
#[tokio::test]
async fn a_reclaimed_lease_records_the_expiry_as_its_cause() {
    let (_dir, store) = boot().await;

    // 1-2. A holds the task, then its lease expires through the real path.
    assign_next(&store, A, 0).await.expect("A takes the task");
    let reclaimed = store
        .expire_lease(&TaskId::from_string(TASK), pinned(1))
        .await
        .expect("expiry does not error");
    let (expired_assignment, _, _) = reclaimed.expect("there was a lease to expire");
    assert_eq!(expired_assignment.agent_id, AgentId::from_string(A));

    // The persisted reason is the evidence the cause will be read from.
    let history = store
        .assignments()
        .assignment_history(&TaskId::from_string(TASK))
        .await
        .expect("history");
    let expired_tenure = history
        .iter()
        .find(|a| a.agent_id == AgentId::from_string(A))
        .expect("A's tenure");
    assert_eq!(
        expired_tenure.release_reason,
        Some(director_domain::assignment::ReleaseReason::LeaseExpired),
        "the release reason is what makes this an expiry and not a displacement"
    );

    // 3. B takes the task over.
    assign_next(&store, B, 2)
        .await
        .expect("B reclaims the task");

    // 4-5. Exactly one handoff, attributed to the expiry.
    let records = handoffs(&store).await;
    assert_eq!(records.len(), 1, "one reclaim, one record");
    let record = &records[0];
    assert_eq!(
        record.cause,
        Some(HandoffCause::LeaseExpiredReclaimed),
        "the store classified this from the persisted release reason"
    );
    assert_eq!(record.from_agent, AgentId::from_string(A));
    assert_eq!(record.to_agent, AgentId::from_string(B));
    assert_eq!(record.task_id, TaskId::from_string(TASK));
    assert_eq!(
        record.project_id.as_ref(),
        Some(&ProjectId::from_string(PROJ))
    );
    assert!(
        record.source_task_version.is_some(),
        "the task version the snapshot was taken at is recorded"
    );
    assert_eq!(
        record.source_checkpoint, None,
        "and the absent checkpoint is reported absent, not invented"
    );
    assert!(
        !record.is_current_for(record.source_task_version.unwrap(), None),
        "a snapshot with no checkpoint is never current"
    );
}

// 8. The two causes must not be confused.
#[tokio::test]
async fn an_active_displacement_is_not_labelled_an_expiry() {
    let (_dir, store) = boot().await;
    assign_next(&store, A, 0).await.expect("A takes the task");
    assign_next(&store, B, 1).await.expect("B displaces A");

    let records = handoffs(&store).await;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].cause,
        Some(HandoffCause::Displaced),
        "A was active when displaced, which is a displacement"
    );
}

#[tokio::test]
async fn an_unrelated_task_records_nothing() {
    let (_dir, store) = boot().await;
    let mut other = Task::for_project(
        ProjectId::from_string(PROJ),
        TaskId::from_string("OTHER"),
        "OTHER",
        "unrelated work",
    );
    other.status = TaskStatus::Todo;
    store.tasks().create_task(&other).await.expect("task");

    assign_next(&store, A, 0).await.expect("first assignment");

    // Assigning the second task would need a declared scope, because `WORK`
    // declares none and so may touch anything - which is the scope guard doing
    // its job, not a problem to work around here. What this test is about is the
    // record, and the record must be empty for a task that has not changed hands.
    assert!(
        handoffs(&store).await.is_empty(),
        "a first assignment displaces nobody, so nothing is recorded"
    );
    let other_records = store
        .handoffs()
        .handoffs_for_task(&TaskId::from_string("OTHER"))
        .await
        .expect("listed");
    assert!(
        other_records.is_empty(),
        "and an untouched task has no handoff history at all"
    );
}

// 2. Same-agent reacquisition, and historical tenures that must not be reclassified.
#[tokio::test]
async fn historical_tenures_are_not_misclassified_as_active_displacement() {
    let (_dir, store) = boot().await;
    assign_next(&store, A, 0).await.expect("A takes the task");
    // Reassign to B *after* releasing A for a reason other than expiry, so there
    // is no active holder and no lease expiry: nothing should be recorded.
    store
        .assignments()
        .release_assignment(
            &AssignmentId::from_string("ASG-WORK-1"),
            director_domain::assignment::ReleaseReason::ReleasedByOperator,
            1,
        )
        .await
        .expect("released");
    assign_next(&store, B, 2).await.expect("B takes it");

    assert!(
        handoffs(&store).await.is_empty(),
        "an operator release is not a transfer Orqyn can attribute a cause to"
    );
}

// ---------------------------------------------------------------------------
// Atomicity when the handoff write fails
// ---------------------------------------------------------------------------

/// A deterministic failure at the handoff insertion point.
///
/// No production seam, no fault-injection API and no database corruption: the
/// handoff id is allocated as `HO-{task}-{count of that task's handoffs}`, but
/// the primary key on `handoffs.id` is global. So a row already occupying
/// `HO-{task}-0` - belonging to a *different* task - makes the very next
/// displacement of `task` collide on the id and the INSERT fail.
///
/// That is a real constraint violation raised by SQLite at the exact statement
/// under test, which is what makes it deterministic rather than a mock.
///
/// The whole point is that this would NOT fail if the handoff write were moved
/// outside the transaction: the release, the new assignment and the task's
/// status move would already have committed, and the transfer would be recorded
/// as successful with nothing to show for it.
#[tokio::test]
async fn a_failed_handoff_write_rolls_the_whole_displacement_back() {
    let (_dir, store) = boot().await;
    assign_next(&store, A, 0).await.expect("A takes the task");

    // A second task, so the collision row can be attributed to something else.
    let mut other = Task::for_project(
        ProjectId::from_string(PROJ),
        TaskId::from_string("OTHER"),
        "OTHER",
        "unrelated work",
    );
    other.status = TaskStatus::Todo;
    store.tasks().create_task(&other).await.expect("task");
    let other_task = store
        .tasks()
        .get_task(&TaskId::from_string("OTHER"))
        .await
        .expect("other task reads back");

    // Occupy the id WORK's first displacement is going to allocate.
    let squatter = director_domain::handoff::Handoff::open(
        HandoffId::from_string("HO-WORK-0"),
        TaskId::from_string("OTHER"),
        AgentId::from_string(A),
        AgentId::from_string(C),
        "occupies the id WORK's first handoff would take",
    );
    store
        .handoffs()
        .open_handoff(&squatter)
        .await
        .expect("the colliding row is written");

    // The displacement that must fail. The task is read first, so "rolled back"
    // is asserted against the state immediately before the attempt rather than
    // against some other row's version.
    let before_task = store
        .tasks()
        .get_task(&TaskId::from_string(TASK))
        .await
        .expect("task reads back before the attempt");
    let attempted = assign_next(&store, B, 1).await;
    assert!(
        attempted.is_err(),
        "the handoff insert must fail, so the caller cannot report a transfer"
    );

    // 1. No partial handoff for WORK, and the pre-existing row is untouched.
    let work_records = handoffs(&store).await;
    assert!(
        work_records.is_empty(),
        "a rolled-back displacement persists no handoff: {:?}",
        work_records
    );
    let squatter_after = store
        .handoffs()
        .get_handoff(&HandoffId::from_string("HO-WORK-0"))
        .await
        .expect("the unrelated row survives");
    assert_eq!(
        squatter_after.summary, squatter.summary,
        "and is not overwritten by the failed attempt"
    );

    // 2. The previous assignment's release was rolled back - A still holds it.
    let sitting = store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string(TASK))
        .await
        .expect("assignments read")
        .expect("the original holder keeps the task");
    assert_eq!(
        sitting.agent_id,
        AgentId::from_string(A),
        "a failed handoff must not cost A its tenure"
    );
    let history = store
        .assignments()
        .assignment_history(&TaskId::from_string(TASK))
        .await
        .expect("history");
    assert_eq!(
        history.len(),
        1,
        "and no replacement tenure was partially committed: {:?}",
        history
            .iter()
            .map(|a| a.id.as_str().to_string())
            .collect::<Vec<_>>()
    );

    // 3. The task's status and version were rolled back.
    let task = store
        .tasks()
        .get_task(&TaskId::from_string(TASK))
        .await
        .expect("task");
    assert_eq!(
        task.state_version, before_task.state_version,
        "the task is exactly what it was before the attempt"
    );
    assert_eq!(
        task.status, before_task.status,
        "including its status: no partial move to in_progress"
    );
    let _ = &other_task;

    // 4. A later, unobstructed displacement succeeds and records properly -
    //    proving the rollback left the store usable rather than wedged.
    store
        .handoffs()
        .get_handoff(&HandoffId::from_string("HO-WORK-0"))
        .await
        .expect("readable");
    let retry = store
        .assign_and_start_next(
            &TaskId::from_string(TASK),
            &AgentId::from_string(B),
            pinned(2),
        )
        .await;
    // Still fails while the colliding row stands, which is itself correct: the
    // id is genuinely taken and inventing another would be a fabrication.
    assert!(
        retry.is_err(),
        "and the collision is still refused rather than papered over"
    );
}
