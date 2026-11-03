//! Multi-agent coordination audit probes (Phase 42).
//!
//! These tests exist to produce evidence for `MULTI_AGENT_AUDIT.md`. They are
//! deliberately written against the store's own primitives — `assign_and_start`,
//! `expire_lease`, `report_completion`, `accept_handoff` — rather than through
//! the application loop, because the audit asks whether the *coordination
//! invariants hold in storage*, not whether any one caller happens to respect
//! them.
//!
//! Every scenario that can race uses real OS threads with a barrier, for the
//! reason `assign_concurrency.rs` documents at length: the store's calls are
//! synchronous end to end, so a `tokio::join!` runs them sequentially and
//! proves nothing about a race.
//!
//! These are audit probes, not regression tests pinned to a fix: they are
//! untracked and are expected to remain green while the audit is written.

use std::sync::{Arc, Barrier};

use director_domain::agent::{Agent, AgentStatus, Harness};
use director_domain::assignment::{AgentAssignment, AssignmentStatus};
use director_domain::capability::Capability;
use director_domain::checkpoint::Checkpoint;
use director_domain::handoff::Handoff;
use director_domain::ids::{
    AgentId, AssignmentId, CheckpointId, HandoffId, MachineId, ProjectId, SessionId, TaskId,
};
use director_domain::project::Project;
use director_domain::session::AgentSession;
use director_domain::task::{Task, TaskStatus};
use director_domain::verification::{Evidence, EvidenceStatus, ProbeKind, VerificationStatus};
use director_domain::{
    AgentRepository, AssignmentRepository, CheckpointRepository, HandoffRepository,
    ProjectRepository, SessionRepository, StoreError, TaskRepository, VerificationRepository,
};
use director_store::Store;

/// A pinned instant, so a scenario's history is the history the test wrote.
fn pinned() -> chrono::DateTime<chrono::Utc> {
    chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 11, 3, 9, 0, 0).unwrap()
}

/// Drive one future on the calling thread, the way the store's own
/// thread-based concurrency tests do.
fn run<T, F>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime")
        .block_on(future)
}

/// One project, one `todo` task, and `n` registered agents — the state every
/// claim scenario starts from. Two stores over the same file, because a race
/// needs two writers. The temp dir is leaked deliberately: it must outlive the
/// store handles that threads borrow, and on Windows a dropped dir would
/// delete the file underneath a later probe in the same test.
fn audit_dir() -> (std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a temp dir for the audit store");
    (dir.path().join("orqyn.db"), dir)
}

/// One project, one `todo` task, and `n` registered agents, on two stores
/// over the same file.
async fn fixture(task: &str, agents: &[&str]) -> (Store, Store, TaskId, Vec<AgentId>) {
    let (path, dir) = audit_dir();
    // The directory must outlive every store this test opens on it; a dropped
    // TempDir would delete the file underneath a later probe in the same test.
    std::mem::forget(dir);
    let store = Store::open(&path).await.expect("store opens");
    let other = Store::open(&path)
        .await
        .expect("a second store on the same file");

    let project = ProjectId::from_string("PROJ-1");
    store
        .projects()
        .create_project(&Project::new(project.clone(), "proj", "/nowhere"))
        .await
        .expect("project created");

    let task_id = TaskId::from_string(task);
    store
        .tasks()
        .create_task(&Task::for_project(
            project,
            task_id.clone(),
            task,
            "the audit's task",
        ))
        .await
        .expect("task created");

    let mut agent_ids = Vec::new();
    for name in agents {
        agent_ids.push(register(&store, name).await);
    }

    (store, other, task_id, agent_ids)
}

/// Register one agent as `Available`.
async fn register(store: &Store, name: &str) -> AgentId {
    let id = AgentId::from_string(name);
    let mut agent = Agent::register(
        id.clone(),
        name,
        Harness::ClaudeCode,
        MachineId::from_string("MACH-a"),
        vec![Capability::Coding],
    );
    agent.status = AgentStatus::Available;
    store
        .agents()
        .register_agent(&agent)
        .await
        .expect("agent registered");
    id
}

/// How many assignments are currently active for `task`.
async fn active_count(store: &Store, task: &TaskId) -> usize {
    store
        .assignments()
        .assignment_history(task)
        .await
        .expect("history reads")
        .into_iter()
        .filter(|a| a.status == AssignmentStatus::Active)
        .count()
}

/// A live session attached to the task's active assignment, the way MONITOR's
/// acknowledge step does it. Returns the assignment id.
async fn acknowledge(store: &Store, task: &TaskId, agent: &AgentId, n: usize) -> AssignmentId {
    let assignment = store
        .assignments()
        .active_assignment_for_task(task)
        .await
        .expect("assignment reads")
        .expect("the task is assigned");
    let session = AgentSession::start(
        SessionId::from_string(format!("SESS-{n}")),
        Some(ProjectId::from_string("PROJ-1")),
        agent.clone(),
        MachineId::from_string("MACH-a"),
        Some(task.clone()),
    );
    store
        .acknowledge_assignment(&assignment.id, &session, pinned())
        .await
        .expect("the assignment acknowledges the session");
    assignment.id
}

// ---------------------------------------------------------------------------
// Scenario 1: two agents attempt to claim the same task simultaneously.
//
// The coordination model has a single director assigning work, so a "claim" at
// this layer is an assignment. The invariant under test is the unambiguity
// rule: whatever interleaving occurs, exactly one active assignment exists for
// the task afterwards.
// ---------------------------------------------------------------------------

#[test]
fn s1_two_simultaneous_claims_leave_exactly_one_active_assignment() {
    let (store_a, store_b, task, agents) =
        run(async { fixture("AUTH-1", &["AGENT-a", "AGENT-b"]).await });
    let (agent_a, agent_b) = (agents[0].clone(), agents[1].clone());

    let store_for_a = store_a.clone();
    let task_for_a = task.clone();
    let store_for_b = store_b.clone();
    let task_b = task.clone();
    let agent_bb = agent_b.clone();
    let gate = Arc::new(Barrier::new(2));
    let gate_b = gate.clone();
    let a = std::thread::spawn(move || {
        gate.wait();
        run(async move {
            store_for_a
                .assign_and_start_next(&task_for_a, &agent_a, pinned())
                .await
        })
    });
    let b = std::thread::spawn(move || {
        gate_b.wait();
        run(async move {
            store_for_b
                .assign_and_start_next(&task_b, &agent_bb, pinned())
                .await
        })
    });
    let a_outcome = a.join().expect("A finishes");
    let b_outcome = b.join().expect("B finishes");
    a_outcome.expect("A's handoff commits");
    b_outcome.expect("B's handoff commits");

    let history = run(async {
        store_a
            .assignments()
            .assignment_history(&task)
            .await
            .expect("history reads")
    });
    let active: Vec<_> = history
        .into_iter()
        .filter(|a| a.status == AssignmentStatus::Active)
        .collect();
    assert_eq!(
        active.len(),
        1,
        "exactly one assignment is active no matter who committed first"
    );
    // Whichever handoff committed first is not determined by anything the tests
    // control — `BEGIN IMMEDIATE` serializes the pair, but either can win the
    // race to it. What *is* invariant: both tenures are recorded, the loser is
    // released as a reassignment rather than deleted, and the task is in flight
    // exactly once.
    let history = run(async {
        store_a
            .assignments()
            .assignment_history(&task)
            .await
            .expect("tenures read")
    });
    assert_eq!(history.len(), 2, "both tenures are recorded");
    let released: Vec<_> = history
        .into_iter()
        .filter(|a| a.status == AssignmentStatus::Released)
        .collect();
    assert_eq!(
        released.len(),
        1,
        "the loser's tenure is retained, not deleted"
    );
    assert_eq!(
        released[0].release_reason,
        Some(director_domain::assignment::ReleaseReason::Reassigned)
    );

    let task_row = run(async { store_a.tasks().get_task(&task).await.expect("reads") });
    assert_eq!(task_row.status, TaskStatus::InProgress);
    assert_eq!(task_row.state_version, 3, "one version bump per handoff");
}

// ---------------------------------------------------------------------------
// Scenario 2: one agent crashes after claiming a task.
//
// A crash is modeled as it actually appears to the store: an assignment left
// active with no session, and no further writes from the agent. Recovery is
// the lease expiry path, which must put the task back into a handable state
// without losing the crashed agent's tenure.
// ---------------------------------------------------------------------------

#[test]
fn s2_a_crash_after_claiming_is_reclaimed_by_lease_expiry() {
    let (store, _other, task, agents) = run(async { fixture("AUTH-2", &["AGENT-a"]).await });

    run(async {
        store
            .assign_and_start_next(&task, &agents[0], pinned())
            .await
            .expect("the task is claimed");
        // No acknowledge: the agent died before it could report a session.
    });

    let (assignment, task_row, session) = run(async {
        store
            .expire_lease(&task, pinned())
            .await
            .expect("the lease is expired")
            .expect("there was a lease to expire")
    });

    assert_eq!(assignment.status, AssignmentStatus::Released);
    assert_eq!(
        assignment.release_reason,
        Some(director_domain::assignment::ReleaseReason::LeaseExpired)
    );
    assert_eq!(
        task_row.status,
        TaskStatus::Todo,
        "the task is handable again"
    );
    assert!(session.is_none(), "no session existed to close");
    assert_eq!(
        run(async { active_count(&store, &task).await }),
        0,
        "no assignment remains active on the reclaimed task"
    );
    let history = run(async {
        store
            .assignments()
            .assignment_history(&task)
            .await
            .expect("the tenure history reads")
    });
    assert_eq!(history.len(), 1, "the crashed agent's tenure is retained");
}

// ---------------------------------------------------------------------------
// Scenario 3: a lease expires while an agent is working.
//
// The agent acknowledged a session, so expiry must also close that session as
// `Vanished` and mark the agent `Disconnected`, or a later round would hand
// the work right back to the agent that went quiet.
// ---------------------------------------------------------------------------

#[test]
fn s3_a_lease_expiring_mid_work_closes_the_session_and_disconnects_the_agent() {
    let (store, _other, task, agents) = run(async { fixture("AUTH-3", &["AGENT-a"]).await });

    run(async {
        store
            .assign_and_start_next(&task, &agents[0], pinned())
            .await
            .expect("the task is claimed");
        acknowledge(&store, &task, &agents[0], 1).await;
    });

    let result = run(async {
        store
            .expire_lease(&task, pinned())
            .await
            .expect("the lease is expired")
    });
    assert!(result.is_some(), "an active lease was taken");

    let agent = run(async {
        store
            .agents()
            .get_agent(&agents[0])
            .await
            .expect("agent reads")
    });
    assert_eq!(agent.status, AgentStatus::Disconnected);

    let session = run(async {
        store
            .sessions()
            .get_session(&SessionId::from_string("SESS-1"))
            .await
            .expect("session reads")
    });
    assert!(matches!(
        session.end,
        Some(director_domain::session::SessionEnd::Vanished)
    ));

    let task_row = run(async { store.tasks().get_task(&task).await.expect("task reads") });
    assert_eq!(task_row.status, TaskStatus::Todo);
}

// ---------------------------------------------------------------------------
// Scenario 4: an agent submits a result after its lease expires.
//
// The post-expiry completion must not move the task: the tenure that the
// evidence describes is over, and the task has been handed back. The report is
// therefore refused by the store, not silently applied.
// ---------------------------------------------------------------------------

#[test]
fn s4_a_result_submitted_after_expiry_does_not_move_the_task() {
    let (store, _other, task, agents) = run(async { fixture("AUTH-4", &["AGENT-a"]).await });

    let assignment_id = run(async {
        store
            .assign_and_start_next(&task, &agents[0], pinned())
            .await
            .expect("the task is claimed");
        acknowledge(&store, &task, &agents[0], 1).await
    });

    run(async {
        store
            .expire_lease(&task, pinned())
            .await
            .expect("the lease is expired");
    });

    let outcome = run(async {
        store
            .report_completion(&assignment_id, pinned())
            .await
            .expect("the report is handled")
    });
    assert!(
        outcome.is_none(),
        "a released tenure's completion is not recorded: the task is no longer the agent's to finish"
    );

    let task_row = run(async { store.tasks().get_task(&task).await.expect("task reads") });
    assert_eq!(
        task_row.status,
        TaskStatus::Todo,
        "the reclaimed task stays handable"
    );
    assert_eq!(
        run(async { store.verifications().verifications_for_task(&task).await })
            .unwrap()
            .len(),
        0,
        "no verdict lands for a post-expiry report"
    );

    // And the task is still assignable: recovery did not strand it.
    let agent_b = run(async { register(&store, "AGENT-b").await });
    run(async {
        store
            .assign_and_start_next(&task, &agent_b, pinned())
            .await
            .expect("a replacement takes the reclaimed task");
    });
    let task_row = run(async { store.tasks().get_task(&task).await.expect("reads") });
    assert_eq!(task_row.status, TaskStatus::InProgress);
}

// ---------------------------------------------------------------------------
// Scenario 5: an old agent submits an update after another agent takes
// ownership.
//
// Two halves: the tenure-scoped write (completion) and the unscoped write
// (checkpoint). The first must be refused; the second is what the audit asks
// the uncomfortable question about.
// ---------------------------------------------------------------------------

#[test]
fn s5_an_old_agents_completion_is_refused_after_ownership_moves() {
    let (store, _other, task, agents) =
        run(async { fixture("AUTH-5", &["AGENT-a", "AGENT-b"]).await });

    let a_assignment = run(async {
        store
            .assign_and_start_next(&task, &agents[0], pinned())
            .await
            .expect("A takes the task");
        acknowledge(&store, &task, &agents[0], 5).await
    });

    // B displaces A while A is still working.
    run(async {
        store
            .assign_and_start_next(&task, &agents[1], pinned())
            .await
            .expect("B takes over");
    });

    // A's late completion arrives.
    let outcome = run(async {
        store
            .report_completion(&a_assignment, pinned())
            .await
            .expect("the report is handled")
    });
    assert!(
        outcome.is_none(),
        "A's tenure was released by the reassignment, so its report changes nothing"
    );

    let task_row = run(async { store.tasks().get_task(&task).await.expect("task reads") });
    assert_eq!(
        task_row.status,
        TaskStatus::InProgress,
        "the task reflects B's tenure, not A's late report"
    );
    assert_eq!(
        run(async { active_count(&store, &task).await }),
        1,
        "B holds the task, alone"
    );
    assert_eq!(
        task_row.state_version, 3,
        "create (1) -> A's in_progress (2) -> B's in_progress (3): exactly the two handoffs"
    );
}

#[test]
fn s5b_a_stale_checkpoint_cannot_bury_the_new_holders_checkpoint() {
    let (store, _other, task, agents) =
        run(async { fixture("AUTH-5b", &["AGENT-a", "AGENT-b"]).await });

    // A checkpoints, is displaced, and B checkpoints.
    let a_checkpoint = Checkpoint::new(
        CheckpointId::from_string("CP-A"),
        task.clone(),
        "objective",
        "A's progress",
        "A's next action",
    );
    let a_version = run(async {
        store
            .assign_and_start_next(&task, &agents[0], pinned())
            .await
            .expect("A takes the task");
        store
            .checkpoints()
            .create_checkpoint(&a_checkpoint, None)
            .await
            .expect("A checkpoints")
            .state_version
    });

    run(async {
        store
            .assign_and_start_next(&task, &agents[1], pinned())
            .await
            .expect("B takes over");
    });
    let b_checkpoint = Checkpoint::new(
        CheckpointId::from_string("CP-B"),
        task.clone(),
        "objective",
        "B's progress",
        "B's next action",
    );
    run(async {
        store
            .checkpoints()
            .create_checkpoint(&b_checkpoint, Some(a_version))
            .await
            .expect("B checkpoints over A's")
    });

    // A, having read version a_version before the displacement, writes again.
    // Its expected version is stale, so the gate must refuse it.
    let a_late = Checkpoint::new(
        CheckpointId::from_string("CP-A-2"),
        task.clone(),
        "objective",
        "A's late progress",
        "A's next action",
    );
    let err = run(async {
        store
            .checkpoints()
            .create_checkpoint(&a_late, Some(a_version))
            .await
            .expect_err("a stale-supersession write is refused")
    });
    assert!(
        matches!(err, StoreError::StateVersionConflict { .. }),
        "the version gate is what protects B's checkpoint, got: {err}"
    );

    let latest = run(async {
        store
            .checkpoints()
            .latest_checkpoint(&task)
            .await
            .expect("the lineage reads")
            .expect("there is a current checkpoint")
    });
    assert_eq!(
        latest.id.as_str(),
        "CP-B",
        "B's checkpoint is still the current one"
    );
}

// ---------------------------------------------------------------------------
// Scenario 6: two agents update the same task version.
//
// The optimistic-concurrency precondition on `tasks.state_version`: two
// writers that both read version N cannot both write N+1.
// ---------------------------------------------------------------------------

#[test]
fn s6_two_writers_on_the_same_task_version_conflict() {
    let (store, store_b, task, _agents) = run(async { fixture("AUTH-6", &["AGENT-a"]).await });

    let (mut a_task, mut b_task) = run(async {
        (
            store.tasks().get_task(&task).await.expect("A reads"),
            store_b.tasks().get_task(&task).await.expect("B reads"),
        )
    });
    assert_eq!(a_task.state_version, b_task.state_version);

    a_task.status = TaskStatus::InProgress;
    run(async {
        store
            .tasks()
            .update_task(&a_task)
            .await
            .expect("A's update lands");
    });

    b_task.status = TaskStatus::VerificationPending;
    let err = run(async {
        store_b
            .tasks()
            .update_task(&b_task)
            .await
            .expect_err("B's stale update conflicts")
    });
    assert!(
        matches!(err, StoreError::StateVersionConflict { caller, stored, .. } if caller == 1 && stored >= 2),
        "B is told its read is stale, got: {err}"
    );

    let final_row = run(async { store.tasks().get_task(&task).await.expect("reads") });
    assert_eq!(final_row.status, TaskStatus::InProgress);
    assert_eq!(final_row.state_version, 2);
}

// ---------------------------------------------------------------------------
// Scenario 7: a handoff is submitted twice.
//
// Claim-once is the handoff table's reason to exist. The same handoff accepted
// twice — a retry after a timeout, or two threads racing the same transfer —
// must see exactly one acceptance.
// ---------------------------------------------------------------------------

#[test]
fn s7_a_handoff_accepted_twice_is_accepted_exactly_once() {
    let (store, store_b, task, agents) =
        run(async { fixture("AUTH-7", &["AGENT-a", "AGENT-b"]).await });

    let handoff = Handoff::open(
        HandoffId::from_string("HO-1"),
        task.clone(),
        agents[0].clone(),
        agents[1].clone(),
        "A hands AUTH-7 to B",
    );
    run(async {
        store
            .handoffs()
            .open_handoff(&handoff)
            .await
            .expect("the handoff opens");
    });

    // Two sessions of the same intended recipient race to accept.
    let session_a = SessionId::from_string("SESS-b1");
    let session_b = SessionId::from_string("SESS-b2");
    let store_for_b = store_b.clone();
    let barrier = Arc::new(Barrier::new(2));
    let barrier_b = barrier.clone();

    let handoff_id_a = handoff.id.clone();
    let acceptor_a = agents[1].clone();
    let store_for_a = store.clone();
    let session_for_check_a = session_a.clone();
    let barrier_a = barrier.clone();
    let a = std::thread::spawn(move || {
        run(async move {
            barrier_a.wait();
            store_for_a
                .handoffs()
                .accept_handoff(&handoff_id_a, &acceptor_a, &session_a, pinned())
                .await
        })
    });
    let handoff_id_b = handoff.id.clone();
    let acceptor_b = agents[1].clone();
    let session_for_check_b = session_b.clone();
    let b = std::thread::spawn(move || {
        run(async move {
            barrier_b.wait();
            store_for_b
                .handoffs()
                .accept_handoff(&handoff_id_b, &acceptor_b, &session_b, pinned())
                .await
        })
    });
    let a_outcome = a.join().expect("A finishes");
    let b_outcome = b.join().expect("B finishes");

    let outcomes = [a_outcome, b_outcome];
    let landed = outcomes.iter().filter(|o| o.is_ok()).count();
    assert_eq!(landed, 1, "exactly one acceptance lands: {outcomes:?}");
    let refusal = outcomes
        .iter()
        .filter_map(|o| o.as_ref().err())
        .next()
        .expect("there is a refusal");
    assert!(
        matches!(refusal, StoreError::ConstraintViolation(_)),
        "the second acceptance is refused, got: {refusal}"
    );

    let stored = run(async {
        store
            .handoffs()
            .get_handoff(&handoff.id)
            .await
            .expect("the handoff reads")
    });
    assert_eq!(
        stored.state,
        director_domain::handoff::HandoffState::Accepted
    );
    // The winner is whichever committed first; both sessions were eligible, so
    // the recorded one is one of them.
    assert!(
        stored.accepted_by_session == Some(session_for_check_a)
            || stored.accepted_by_session == Some(session_for_check_b)
    );
}

// ---------------------------------------------------------------------------
// Scenario 8: a task is recovered after a process restart.
//
// The store is a file. Opening it again must see the assignment, the
// checkpoint, and the session, and the lease path must be able to reclaim the
// work from the reopened store alone.
// ---------------------------------------------------------------------------

#[test]
fn s8_a_task_survives_a_restart_and_is_recoverable() {
    let (path, _dir) = audit_dir();

    // Process one: assign, acknowledge, checkpoint. The store is then dropped
    // with no orderly shutdown — the process simply ends.
    {
        let store = run(async { Store::open(&path).await.expect("store opens") });
        let project = ProjectId::from_string("PROJ-1");
        let task = TaskId::from_string("AUTH-8");
        run(async {
            store
                .projects()
                .create_project(&Project::new(project.clone(), "proj", "/nowhere"))
                .await
                .expect("project created");
            store
                .tasks()
                .create_task(&Task::for_project(
                    project,
                    task.clone(),
                    "AUTH-8",
                    "objective",
                ))
                .await
                .expect("task created");
            let agent = register(&store, "AGENT-a").await;
            store
                .assign_and_start_next(&task, &agent, pinned())
                .await
                .expect("the task is assigned");
            acknowledge(&store, &task, &agent, 8).await;
            let checkpoint = Checkpoint::new(
                CheckpointId::from_string("CP-8"),
                task.clone(),
                "objective",
                "halfway",
                "resume here",
            );
            store
                .checkpoints()
                .create_checkpoint(&checkpoint, None)
                .await
                .expect("the checkpoint lands");
        });
    }

    // Process two: everything the first process wrote must be visible.
    let store = run(async { Store::open(&path).await.expect("the store reopens") });
    let task = TaskId::from_string("AUTH-8");
    let assignment = run(async {
        store
            .assignments()
            .active_assignment_for_task(&task)
            .await
            .expect("the assignment reads")
            .expect("the assignment survived the restart")
    });
    assert_eq!(assignment.agent_id.as_str(), "AGENT-a");

    let checkpoint = run(async {
        store
            .checkpoints()
            .latest_checkpoint(&task)
            .await
            .expect("the lineage reads")
            .expect("the checkpoint survived the restart")
    });
    assert_eq!(checkpoint.progress, "halfway");

    let session = run(async {
        store
            .sessions()
            .sessions_for_task(&TaskId::from_string("AUTH-8"))
            .await
            .expect("the session reads")
    });
    assert_eq!(session.len(), 1, "the session survived the restart");

    // Reclaiming the task is still possible from the reopened store.
    let agent_b = run(async { register(&store, "AGENT-b").await });
    let (assignment, task_row, _) = run(async {
        store
            .expire_lease(&task, pinned())
            .await
            .expect("the lease is expired")
            .expect("there was a lease to expire")
    });
    assert_eq!(
        assignment.release_reason,
        Some(director_domain::assignment::ReleaseReason::LeaseExpired)
    );
    assert_eq!(task_row.status, TaskStatus::Todo);

    run(async {
        store
            .assign_and_start_next(&task, &agent_b, pinned())
            .await
            .expect("B picks the task up");
    });
    let task_row = run(async { store.tasks().get_task(&task).await.expect("reads") });
    assert_eq!(task_row.status, TaskStatus::InProgress);
    let history = run(async {
        store
            .assignments()
            .assignment_history(&task)
            .await
            .expect("tenures read")
    });
    assert_eq!(history.len(), 2, "A's tenure is retained beside B's");
}

// ---------------------------------------------------------------------------
// Scenario 9: one agent completes a task while another attempts to claim it.
//
// The uncomfortable probe. `assign_and_start` has no task-status precondition:
// it moves whatever status it finds to `in_progress`. If a completion lands
// first, the claim brings a `verification_pending` task back into flight.
// ---------------------------------------------------------------------------

#[test]
fn s9_a_claim_that_races_a_completion_revives_the_task() {
    let (store, store_b, task, agents) = run(async { fixture("AUTH-9", &["AGENT-a"]).await });

    let assignment_id = run(async {
        store
            .assign_and_start_next(&task, &agents[0], pinned())
            .await
            .expect("A takes the task");
        acknowledge(&store, &task, &agents[0], 9).await
    });

    // A completes first; the task is awaiting verification.
    run(async {
        store
            .report_completion(&assignment_id, pinned())
            .await
            .expect("A completes")
            .expect("there was a tenure to end");
    });
    let after_completion = run(async { store.tasks().get_task(&task).await.expect("reads") });
    assert_eq!(after_completion.status, TaskStatus::VerificationPending);

    // A second agent's claim arrives afterwards.
    let agent_b = run(async { register(&store_b, "AGENT-b").await });
    let (assignment, task_row, displaced) = run(async {
        store_b
            .assign_and_start_next(&task, &agent_b, pinned())
            .await
            .expect("the claim lands")
    });
    assert_eq!(assignment.agent_id, agent_b);
    assert_eq!(
        task_row.status,
        TaskStatus::InProgress,
        "the task is back in flight"
    );
    assert!(
        displaced.is_none(),
        "no active tenure was displaced: A had already completed"
    );

    // And yet exactly one assignment is active, so the unambiguity rule holds.
    assert_eq!(run(async { active_count(&store, &task).await }), 1);
}

/// Scenario 9 under genuine contention: A's completion and B's claim are in
/// flight at the same moment.
///
/// There are three interleavings, and which one occurs is not under the test's
/// control:
///
/// 1. **B commits first.** A's `report_completion` then reads the released
///    tenure and returns `Ok(None)` — the report is dropped.
/// 2. **A snapshots first, B commits, A writes.** A's deferred transaction took
///    its read before B's commit, so SQLite refuses its write with
///    `SQLITE_BUSY` — and this is the classification defect the audit records:
///    the store reports it as `StoreError::ConstraintViolation("sqlite error 5:
///    database is locked")`, indistinguishable from a unique-key violation.
///    The 5s busy timeout does not help, because this is a snapshot conflict,
///    not a lock wait.
/// 3. **A commits first.** B's claim then lands on the `verification_pending`
///    task and drags it back to `in_progress` — A's work now dodges the
///    verification it was waiting for.
///
/// What holds in every one of them is asserted below; the rest is recorded
/// rather than pinned.
#[test]
fn s9b_a_claim_and_a_completion_actually_race() {
    let (store, store_b, task, agents) = run(async { fixture("AUTH-9b", &["AGENT-a"]).await });

    let assignment_id = run(async {
        store
            .assign_and_start_next(&task, &agents[0], pinned())
            .await
            .expect("A takes the task");
        acknowledge(&store, &task, &agents[0], 92).await
    });

    let agent_b = run(async { register(&store_b, "AGENT-b").await });
    let store_for_a = store.clone();
    let task_for_thread = task.clone();
    let assignment_for_a = assignment_id.clone();
    let gate = Arc::new(Barrier::new(2));
    let gate_b = gate.clone();

    let a = std::thread::spawn(move || {
        run(async move {
            gate.wait();
            store_for_a
                .report_completion(&assignment_for_a, pinned())
                .await
        })
    });
    let b = std::thread::spawn(move || {
        run(async move {
            gate_b.wait();
            store_b
                .assign_and_start_next(&task_for_thread, &agent_b, pinned())
                .await
        })
    });

    // B's claim takes `BEGIN IMMEDIATE`, so it serializes against anything and
    // always lands: it waits out a held lock rather than conflicting with a
    // snapshot. A's completion is a deferred read-then-write, so it may fail
    // with the misclassified busy snapshot (interleaving 2) or report `None`
    // (1) — or report a completion (3), if it commits first.
    let a_outcome = a.join().expect("A finishes");
    let _b_assigned = b
        .join()
        .expect("B finishes")
        .expect("B's claim lands: IMMEDIATE waits out contention");

    let task_row = run(async { store.tasks().get_task(&task).await.expect("reads") });
    assert_eq!(
        task_row.status,
        TaskStatus::InProgress,
        "either B displaced A, or B revived the completed task: both end in flight"
    );
    assert_eq!(run(async { active_count(&store, &task).await }), 1);
    let verifications = run(async {
        store
            .verifications()
            .verifications_for_task(&task)
            .await
            .expect("the verdict history reads")
    });
    assert_eq!(
        verifications.len(),
        0,
        "no verdict was ever reached, so nothing records a judgment of A's work"
    );
    let history = run(async {
        store
            .assignments()
            .assignment_history(&task)
            .await
            .expect("tenures read")
    });
    assert_eq!(
        history.len(),
        2,
        "A's tenure and B's tenure are both recorded"
    );

    // Record which interleaving occurred without pinning it.
    match &a_outcome {
        Ok(None) => eprintln!("s9b: interleaving 1 — A's report found the tenure released"),
        Ok(Some(_)) => eprintln!("s9b: interleaving 3 — A completed, B revived the task"),
        Err(err) => eprintln!("s9b: interleaving 2 — A's report failed as: {err:?}"),
    }
    assert!(
        matches!(&a_outcome, Ok(None) | Ok(Some(_)))
            || matches!(&a_outcome, Err(StoreError::ConstraintViolation(msg)) if msg.contains("locked")),
        "A's report is either accepted, dropped, or surfaces the snapshot conflict as a \
         constraint violation — the classification defect this audit records: {a_outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 10: multiple independent tasks are assigned concurrently.
//
// Disjoint ground must be claimable in parallel: the scope guard exists to
// stop overlap, not throughput.
// ---------------------------------------------------------------------------

#[test]
fn s10_independent_tasks_are_assigned_concurrently_without_interference() {
    let (store, store_b, task_one, agents) =
        run(async { fixture("AUTH-10a", &["AGENT-a", "AGENT-b"]).await });
    let (agent_a, agent_b) = (agents[0].clone(), agents[1].clone());

    // Both tasks must declare disjoint ground, or the scope guard rightly
    // refuses them: a task with no declared scope may touch anything, so it
    // conflicts with every other in-flight task.
    async fn declare(store: &Store, id: &TaskId, paths: Vec<String>) {
        let mut task = store.tasks().get_task(id).await.expect("the task reads");
        task.scope_paths = paths;
        store
            .tasks()
            .update_task(&task)
            .await
            .expect("the scope is declared");
    }
    run(async {
        declare(&store, &task_one, vec!["src/module-a/".into()]).await;
    });

    let task_two = TaskId::from_string("AUTH-10b");
    run(async {
        store
            .tasks()
            .create_task(&Task::for_project(
                ProjectId::from_string("PROJ-1"),
                task_two.clone(),
                "AUTH-10b",
                "a second independent task",
            ))
            .await
            .expect("the second task is created");
        declare(&store, &task_two, vec!["src/module-b/".into()]).await;
    });

    let barrier = Arc::new(Barrier::new(2));
    let store_for_b = store_b.clone();
    let task_two_b = task_two.clone();
    let barrier_a = barrier.clone();

    let store_for_a = store.clone();
    let a = std::thread::spawn(move || {
        run(async move {
            barrier_a.wait();
            store_for_a
                .assign_and_start_next(&task_one, &agent_a, pinned())
                .await
        })
    });
    let b = std::thread::spawn(move || {
        run(async move {
            barrier.wait();
            store_for_b
                .assign_and_start_next(&task_two_b, &agent_b, pinned())
                .await
        })
    });
    a.join().expect("A finishes").expect("task one is assigned");
    b.join().expect("B finishes").expect("task two is assigned");

    let (one, two) = run(async {
        (
            store
                .tasks()
                .get_task(&TaskId::from_string("AUTH-10a"))
                .await
                .unwrap(),
            store.tasks().get_task(&task_two).await.unwrap(),
        )
    });
    assert_eq!(one.status, TaskStatus::InProgress);
    assert_eq!(two.status, TaskStatus::InProgress);
    assert_eq!(
        run(async { active_count(&store, &TaskId::from_string("AUTH-10a")).await }),
        1
    );
    assert_eq!(run(async { active_count(&store, &task_two).await }), 1);
}

// ---------------------------------------------------------------------------
// Supplementary probes: the invariants the scenarios above lean on.
// ---------------------------------------------------------------------------

/// A verification that lands after the task moved is refused — the
/// optimistic precondition on the verdict, probed directly.
#[test]
fn a_stale_verdict_does_not_overwrite_a_newer_task_state() {
    let (store, _other, task, _agents) = run(async { fixture("AUTH-V", &["AGENT-a"]).await });
    let project = ProjectId::from_string("PROJ-1");

    let version = run(async { store.tasks().get_task(&task).await.unwrap().state_version });
    // Move the task out from under the round.
    let mut moved = run(async { store.tasks().get_task(&task).await.unwrap() });
    moved.status = TaskStatus::VerificationPending;
    run(async {
        store
            .tasks()
            .update_task(&moved)
            .await
            .expect("the task moves")
    });

    let err = run(async {
        store
            .apply_verification_with_actions_next(
                &task,
                &project,
                None,
                VerificationStatus::Passed,
                vec![Evidence {
                    kind: ProbeKind::Command,
                    criterion: "it works".into(),
                    status: EvidenceStatus::Passed,
                    detail: "exit 0".into(),
                }],
                None,
                |_| Vec::new(),
                version, // the version the round read *before* the move
                pinned(),
            )
            .await
            .expect_err("a stale round is refused")
    });
    assert!(matches!(err, StoreError::StateVersionConflict { .. }));

    let task_row = run(async { store.tasks().get_task(&task).await.unwrap() });
    assert_eq!(task_row.status, TaskStatus::VerificationPending);
    assert_eq!(
        run(async { store.verifications().verifications_for_task(&task).await })
            .unwrap()
            .len(),
        0,
        "the stale verdict left nothing behind"
    );
}

/// The partial unique index is load-bearing for the unambiguity rule. This
/// probes it through the repository's own insert path: an assignment row built
/// as `active` and inserted by `create_assignment` must be refused when the
/// task already has an active tenure — without any help from `handoff`'s
/// release-first ordering.
#[test]
fn the_partial_unique_index_refuses_a_second_active_assignment() {
    let (store, _other, task, agents) =
        run(async { fixture("AUTH-U", &["AGENT-a", "AGENT-b"]).await });
    run(async {
        store
            .assign_and_start_next(&task, &agents[0], pinned())
            .await
            .expect("the first assignment is active");
    });

    let mut second = AgentAssignment::propose(
        AssignmentId::from_string("ASG-RAW-2"),
        task.clone(),
        agents[1].clone(),
    );
    second.status = AssignmentStatus::Active;
    let err = run(async {
        store
            .assignments()
            .create_assignment(&second)
            .await
            .expect_err("a second active assignment is refused")
    });
    assert!(
        matches!(err, StoreError::ConstraintViolation(_)),
        "the refusal is the partial unique index, got: {err}"
    );
    assert_eq!(
        run(async {
            store
                .assignments()
                .active_assignment_for_task(&task)
                .await
                .unwrap()
                .unwrap()
                .agent_id
        })
        .as_str(),
        "AGENT-a",
        "the sitting tenure survived the attempt"
    );
}
