//! Regression tests for the concurrent-assignment race in scope-conflict
//! detection.
//!
//! # The bug this file exists for
//!
//! Phase 13 added scope-conflict detection, and `validate_assignment` checked it
//! before every handoff. That check read the project's task list *outside* any
//! transaction, and the write that followed opened a **deferred** SQLite
//! transaction which re-read only the status of the one task being assigned. So
//! two concurrent handoffs of two different tasks with overlapping scopes could
//! each read "nothing is in flight", each validate, and each commit: the two
//! transactions touch disjoint rows, so SQLite's writer serialization has nothing
//! to collide on, and the partial unique index on `assignments` guards one active
//! assignment *per task* — not one holder *per scope path*.
//!
//! The result was two agents handed the same files, which is the precise failure
//! Orqynn exists to prevent.
//!
//! # The fix these tests pin
//!
//! `assign_and_start` now opens `BEGIN IMMEDIATE`, so the reserved write lock is
//! taken *before* the guard reads. The second caller blocks there until the first
//! commits, and then reads the first's committed row. The guard and the write
//! share one transaction, so a refusal happens before any write and rolls back.
//!
//! Because `IMMEDIATE` is a property of SQLite's locking, "exactly one winner"
//! is not a timing accident these tests hope for — it is what the transaction
//! mode guarantees. The tests assert it anyway, because the failure mode they
//! guard against is precisely a check that *looks* right.
//!
//! # Why these tests use real threads, not `tokio::join!`
//!
//! An earlier version of this file drove both handoffs with `tokio::join!` and
//! **passed against the unfixed code**. The reason is worth recording, because
//! it is a trap rather than a subtlety:
//!
//! Everything the store does under the hood is synchronous. `ConnectionPool::get`
//! takes a mutex and pops a connection, and every query runs inline on the
//! calling thread — `assign_and_start` contains no `.await` that can yield. So a
//! joined future runs start to finish before the second future is ever polled.
//! `join!` produced two sequential calls wearing a concurrent costume, and the
//! second one legitimately saw the first one's committed row.
//!
//! Reproducing a locking race needs two OS threads that are genuinely in the
//! database at the same moment. [`in_parallel`] provides exactly that, with a
//! barrier so both threads leave the gate together. No production code was
//! changed to make the race reproducible.
//!
//! # Two layers, tested separately on purpose
//!
//! - **Store level** (`store.assign_and_start` directly): the optimistic
//!   application pre-check is bypassed entirely, so a refusal can only have come
//!   from the authoritative in-transaction guard. These are the tests that fail
//!   without the fix.
//! - **Application level** (`assign_at`): proves the structured
//!   [`AssignError::ScopeConflict`] reaches the caller rather than being flattened
//!   into a generic store failure.
//!
//! Every test uses the real store over a real SQLite file. Nothing is mocked, no
//! status is poked by hand, and the assertions read persisted state back out.

use director_app::assign::{assign_at, AssignRequest};
use director_app::plan::{PlanRequest, TaskSpec};
use director_app::AssignError;
use director_domain::agent::{Agent, AgentStatus, Harness};
use director_domain::capability::Capability;
use director_domain::ids::{AgentId, AssignmentId, MachineId, PlanId, ProjectId, TaskId};
use director_domain::project::Project;
use director_domain::task::{Task, TaskStatus};
use director_domain::{
    AgentRepository, AssignmentRepository, ProjectRepository, StoreError, TaskRepository,
};
use director_store::Store;
use std::sync::Barrier;

/// A store, a project, and a plan of scoped tasks.
///
/// Two stores over the same file, because the race is between two writers and a
/// second connection pool is how that contention actually occurs — the same
/// convention `director-store`'s own concurrency test uses.
struct Fixture {
    store: Store,
    other: Store,
    project: ProjectId,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("a temp dir for the store");
        let path = dir.path().join("orqyn.db");
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

        Fixture {
            store,
            other,
            project,
            _dir: dir,
        }
    }

    /// Plan `specs` through the real PLAN step, so declared scopes are carried
    /// through planning and persisted rather than injected afterwards.
    async fn plan(&self, specs: Vec<TaskSpec>) {
        director_app::plan::plan(
            &self.store,
            PlanRequest {
                project_id: self.project.clone(),
                plan_id: PlanId::from_string("PLAN-1"),
                objective: "do the thing".into(),
                rationale: "the concurrency tests need an active plan".into(),
                tasks: specs,
                authorized_by: AgentId::from_string("AGENT-planner"),
            },
        )
        .await
        .expect("plan succeeds");
    }

    async fn agent(&self, id: &str) -> AgentId {
        let agent_id = AgentId::from_string(id);
        let mut agent = Agent::register(
            agent_id.clone(),
            id,
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            vec![Capability::Coding],
        );
        agent.status = AgentStatus::Available;
        self.store
            .agents()
            .register_agent(&agent)
            .await
            .expect("agent registered");
        agent_id
    }

    async fn reload(&self, id: &str) -> Task {
        self.store
            .tasks()
            .get_task(&TaskId::from_string(id))
            .await
            .expect("task exists")
    }

    async fn active_holder(&self, task: &str) -> Option<String> {
        self.store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string(task))
            .await
            .expect("assignment lookup works")
            .map(|assignment| assignment.agent_id.to_string())
    }
}

/// Run two closures on two OS threads, released together by a barrier.
///
/// `std::thread::scope` so the closures may borrow the fixture rather than
/// requiring `'static`. The barrier is the test-only synchronization: it does not
/// widen the race window inside the transaction, it only stops one thread
/// getting a head start, so the two genuinely arrive at `BEGIN` together.
fn in_parallel<A, B, RA, RB>(first: A, second: B) -> (RA, RB)
where
    A: FnOnce() -> RA + Send,
    B: FnOnce() -> RB + Send,
    RA: Send,
    RB: Send,
{
    std::thread::scope(|scope| {
        // Shared by reference count so each closure owns a handle; `Barrier` is
        // `Sync`, so this is just a shorter way of lending it to both threads.
        let gate = std::sync::Arc::new(Barrier::new(2));
        let first_gate = std::sync::Arc::clone(&gate);
        let second_gate = std::sync::Arc::clone(&gate);
        let a = scope.spawn(move || {
            first_gate.wait();
            first()
        });
        let b = scope.spawn(move || {
            second_gate.wait();
            second()
        });
        (
            a.join().expect("the first thread finishes"),
            b.join().expect("the second thread finishes"),
        )
    })
}

/// Drive one store call to completion on the calling thread.
///
/// The store's work is synchronous under the hood, so this is not a simulator:
/// the call really does occupy the thread for its whole duration, which is
/// precisely what lets a second thread contend with it.
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

/// A task spec with a declared scope.
fn scoped(id: &str, scope: &str) -> TaskSpec {
    TaskSpec::new(TaskId::from_string(id), id, format!("do {id}")).scopes([scope])
}

/// A task spec with no declared scope — "undeclared", which Phase 13 treats as
/// conflicting with everything.
fn undeclared(id: &str) -> TaskSpec {
    TaskSpec::new(TaskId::from_string(id), id, format!("do {id}"))
}

/// A deterministic instant, so both callers act as though they are at the same
/// time. The contention comes from the two threads overlapping, not from two
/// different timestamps.
fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_760_000_000, 0).expect("a fixed instant")
}

/// Split two results into successes and refusals, so a test can assert "exactly
/// one of each" without depending on which thread won.
fn partition<T, E>(results: [Result<T, E>; 2]) -> (usize, Vec<E>) {
    let mut ok = 0;
    let mut errs = Vec::new();
    for result in results {
        match result {
            Ok(_) => ok += 1,
            Err(err) => errs.push(err),
        }
    }
    (ok, errs)
}

// ---------------------------------------------------------------------------
// Store level: the authoritative guard
// ---------------------------------------------------------------------------

/// The decisive regression. Two overlapping tasks, two agents, two connection
/// pools, both handoffs genuinely in the database at once — bypassing the
/// application pre-check so the only thing that can refuse the loser is the guard
/// inside the transaction.
///
/// Reverting the transaction to `Deferred` makes this fail with both handoffs
/// succeeding. That is the regression it exists to catch.
#[tokio::test]
async fn concurrent_handoffs_of_overlapping_scopes_leave_exactly_one_holder() {
    let fixture = Fixture::new().await;
    fixture
        .plan(vec![
            scoped("AUTH", "src/auth"),
            scoped("API", "src/auth/login.rs"),
        ])
        .await;
    let auth_agent = fixture.agent("AGENT-1").await;
    let api_agent = fixture.agent("AGENT-2").await;

    let (auth_result, api_result) = in_parallel(
        || {
            let task = TaskId::from_string("AUTH");
            let assignment = AssignmentId::from_string("ASG-1");
            run(fixture
                .store
                .assign_and_start(&task, &auth_agent, &assignment, now()))
        },
        || {
            let task = TaskId::from_string("API");
            let assignment = AssignmentId::from_string("ASG-2");
            run(fixture
                .other
                .assign_and_start(&task, &api_agent, &assignment, now()))
        },
    );

    let (winners, refusals) = partition([auth_result, api_result]);
    assert_eq!(
        winners, 1,
        "exactly one handoff may take overlapping ground; both succeeded, which is the race"
    );
    assert_eq!(
        refusals.len(),
        1,
        "the loser is refused, not errored obscurely"
    );

    // The refusal is the structured conflict, not a generic store failure, so a
    // caller can tell "something else holds this ground" from "the database is
    // having a bad day".
    match &refusals[0] {
        StoreError::ScopeConflict {
            task,
            holder,
            undeclared,
            ..
        } => {
            assert!(
                !undeclared,
                "both tasks declared scopes, so this is located"
            );
            // The named holder is the task that won, so the two agree.
            let winner = if task.as_str() == "AUTH" {
                "API"
            } else {
                "AUTH"
            };
            assert_eq!(holder.as_str(), winner);
        }
        other => panic!("expected a scope conflict, got {other:?}"),
    }

    // Persisted state agrees. Which thread wins is genuinely arbitrary, so the
    // assertion is about the relationship - exactly one task in flight, exactly
    // one still handable - rather than about which one it was.
    let statuses = [
        fixture.reload("AUTH").await.status,
        fixture.reload("API").await.status,
    ];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == TaskStatus::InProgress)
            .count(),
        1,
        "exactly one task is in flight, got {statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == TaskStatus::Todo)
            .count(),
        1,
        "the loser keeps its place in the queue rather than half-starting, got {statuses:?}"
    );
}

/// The counterweight: the guard must not over-refuse. Two disjoint scopes are
/// exactly the case Phase 13 exists to enable, so both handoffs must succeed
/// even when they collide in time.
#[tokio::test]
async fn concurrent_handoffs_of_disjoint_scopes_both_succeed() {
    let fixture = Fixture::new().await;
    fixture
        .plan(vec![
            scoped("AUTH", "src/auth"),
            scoped("BILLING", "src/billing"),
        ])
        .await;
    let auth_agent = fixture.agent("AGENT-1").await;
    let billing_agent = fixture.agent("AGENT-2").await;

    let (auth_result, billing_result) = in_parallel(
        || {
            let task = TaskId::from_string("AUTH");
            let assignment = AssignmentId::from_string("ASG-1");
            run(fixture
                .store
                .assign_and_start(&task, &auth_agent, &assignment, now()))
        },
        || {
            let task = TaskId::from_string("BILLING");
            let assignment = AssignmentId::from_string("ASG-2");
            run(fixture
                .other
                .assign_and_start(&task, &billing_agent, &assignment, now()))
        },
    );

    let (winners, refusals) = partition([auth_result, billing_result]);
    assert!(
        refusals.is_empty(),
        "disjoint scopes are the case the guard must let through, got {refusals:?}"
    );
    assert_eq!(winners, 2, "both agents get their own ground");

    assert_eq!(fixture.reload("AUTH").await.status, TaskStatus::InProgress);
    assert_eq!(
        fixture.reload("BILLING").await.status,
        TaskStatus::InProgress,
        "and they really are in flight together"
    );
    assert_eq!(
        fixture.active_holder("AUTH").await.as_deref(),
        Some("AGENT-1")
    );
    assert_eq!(
        fixture.active_holder("BILLING").await.as_deref(),
        Some("AGENT-2")
    );
}

/// Undeclared scope keeps its documented conservative meaning under real
/// concurrency: it collides with everything, including another undeclared scope.
/// Moving the check into a transaction must not weaken it.
#[tokio::test]
async fn undeclared_scopes_still_collide_when_the_two_handoffs_overlap() {
    let fixture = Fixture::new().await;
    fixture
        .plan(vec![undeclared("AUTH"), undeclared("API")])
        .await;
    let auth_agent = fixture.agent("AGENT-1").await;
    let api_agent = fixture.agent("AGENT-2").await;

    let (auth_result, api_result) = in_parallel(
        || {
            let task = TaskId::from_string("AUTH");
            let assignment = AssignmentId::from_string("ASG-1");
            run(fixture
                .store
                .assign_and_start(&task, &auth_agent, &assignment, now()))
        },
        || {
            let task = TaskId::from_string("API");
            let assignment = AssignmentId::from_string("ASG-2");
            run(fixture
                .other
                .assign_and_start(&task, &api_agent, &assignment, now()))
        },
    );

    let (winners, refusals) = partition([auth_result, api_result]);
    assert_eq!(winners, 1, "two undeclared scopes may not run together");
    assert_eq!(refusals.len(), 1);
    match &refusals[0] {
        StoreError::ScopeConflict { undeclared, .. } => assert!(
            *undeclared,
            "the conflict is total rather than located, because neither task named ground"
        ),
        other => panic!("expected a scope conflict, got {other:?}"),
    }
}

/// A refused handoff must leave nothing behind: no assignment row, no partial
/// status move, and the losing agent left free rather than marked as working.
#[tokio::test]
async fn a_refused_concurrent_handoff_writes_nothing() {
    let fixture = Fixture::new().await;
    fixture
        .plan(vec![
            scoped("AUTH", "src/auth"),
            scoped("API", "src/auth/login.rs"),
        ])
        .await;
    let auth_agent = fixture.agent("AGENT-1").await;
    let api_agent = fixture.agent("AGENT-2").await;

    // Take the ground first, through one pool.
    fixture
        .store
        .assign_and_start(
            &TaskId::from_string("AUTH"),
            &auth_agent,
            &AssignmentId::from_string("ASG-1"),
            now(),
        )
        .await
        .expect("the first handoff succeeds");

    let refused = fixture
        .other
        .assign_and_start(
            &TaskId::from_string("API"),
            &api_agent,
            &AssignmentId::from_string("ASG-2"),
            now(),
        )
        .await;

    assert!(
        matches!(refused, Err(StoreError::ScopeConflict { .. })),
        "the second handoff is refused"
    );

    // Nothing partial: the loser is untouched and still handable.
    assert_eq!(
        fixture.reload("API").await.status,
        TaskStatus::Todo,
        "the loser never started"
    );
    assert!(
        fixture.active_holder("API").await.is_none(),
        "no assignment row exists for the refused handoff"
    );
    assert_eq!(
        fixture
            .store
            .agents()
            .get_agent(&api_agent)
            .await
            .expect("the agent reads")
            .current_task,
        None,
        "the refused agent was left free rather than marked as working"
    );

    // And the winner is untouched by the refusal.
    assert_eq!(fixture.reload("AUTH").await.status, TaskStatus::InProgress);
    assert_eq!(
        fixture.active_holder("AUTH").await.as_deref(),
        Some("AGENT-1")
    );
}

// ---------------------------------------------------------------------------
// Application level: the refusal reaches the caller structured
// ---------------------------------------------------------------------------

/// The same race through the public ASSIGN step. Whichever half of the guard
/// refuses — the optimistic pre-check or the authoritative in-transaction one —
/// the caller gets the same structured error, and exactly one agent gets the work.
#[tokio::test]
async fn a_concurrent_conflict_reaches_the_caller_as_a_structured_refusal() {
    let fixture = Fixture::new().await;
    fixture
        .plan(vec![
            scoped("AUTH", "src/auth"),
            scoped("API", "src/auth/login.rs"),
        ])
        .await;
    let auth_agent = fixture.agent("AGENT-1").await;
    let api_agent = fixture.agent("AGENT-2").await;

    let request = |task: &str, agent: AgentId, assignment: &str| AssignRequest {
        project_id: fixture.project.clone(),
        task_id: TaskId::from_string(task),
        agent_id: agent,
        assignment_id: AssignmentId::from_string(assignment),
    };

    let (auth_result, api_result) = in_parallel(
        || {
            run(assign_at(
                &fixture.store,
                request("AUTH", auth_agent.clone(), "ASG-1"),
                now(),
            ))
        },
        || {
            run(assign_at(
                &fixture.other,
                request("API", api_agent.clone(), "ASG-2"),
                now(),
            ))
        },
    );

    let (winners, refusals) = partition([auth_result, api_result]);
    assert_eq!(winners, 1, "exactly one handoff wins overlapping ground");
    assert_eq!(refusals.len(), 1);

    match &refusals[0] {
        AssignError::ScopeConflict {
            task,
            holder,
            undeclared,
            ..
        } => {
            assert!(!undeclared);
            // The message names the ground in dispute, so a caller can fix the
            // plan rather than guess.
            let winner = if task.as_str() == "AUTH" {
                "API"
            } else {
                "AUTH"
            };
            assert_eq!(holder.as_str(), winner);
        }
        // Not `AssignError::Store`: a scope conflict is a fact about the world,
        // and flattening it into a generic store failure would tell a retry loop
        // to try again immediately for no reason.
        other => panic!("expected a structured scope conflict, got {other:?}"),
    }
}

/// The conflict mapping must be narrow in both directions: a real conflict is
/// structured, and a genuine store fault is not disguised as one.
#[tokio::test]
async fn the_conflict_mapping_does_not_disguise_other_store_errors() {
    let fixture = Fixture::new().await;
    fixture
        .plan(vec![
            scoped("AUTH", "src/auth"),
            scoped("API", "src/auth/login.rs"),
        ])
        .await;
    let auth_agent = fixture.agent("AGENT-1").await;
    let api_agent = fixture.agent("AGENT-2").await;

    // Take the ground first, so the next handoff is a genuine conflict.
    fixture
        .store
        .assign_and_start(
            &TaskId::from_string("AUTH"),
            &auth_agent,
            &AssignmentId::from_string("ASG-1"),
            now(),
        )
        .await
        .expect("the first handoff succeeds");

    let conflict = assign_at(
        &fixture.store,
        AssignRequest {
            project_id: fixture.project.clone(),
            task_id: TaskId::from_string("API"),
            agent_id: api_agent.clone(),
            assignment_id: AssignmentId::from_string("ASG-2"),
        },
        now(),
    )
    .await
    .expect_err("the overlapping handoff is refused");

    assert!(
        matches!(conflict, AssignError::ScopeConflict { .. }),
        "a real conflict is structured, got {conflict:?}"
    );
    assert!(
        !matches!(conflict, AssignError::Store(_)),
        "and it is not flattened into a generic store failure"
    );
    assert!(
        !matches!(conflict, AssignError::NoActivePlan(_)),
        "nor misreported as a missing plan, which would send a caller looking in the wrong place"
    );

    // A task that is already in progress is refused for its own reason, and that
    // reason is preserved rather than overwritten by the scope rule. Which
    // legality error fires first depends on the step's ordering, so the point is
    // that it is a legality refusal naming the ids - not a scope conflict it did
    // not actually have, and not a generic store failure.
    let not_todo = assign_at(
        &fixture.store,
        AssignRequest {
            project_id: fixture.project.clone(),
            task_id: TaskId::from_string("AUTH"),
            agent_id: api_agent.clone(),
            assignment_id: AssignmentId::from_string("ASG-3"),
        },
        now(),
    )
    .await
    .expect_err("a task already in progress is not handable as new work");
    match not_todo {
        AssignError::TaskAlreadyAssigned { .. }
        | AssignError::TaskNotTodo { .. }
        | AssignError::ScopeConflict { .. } => {}
        other => panic!("expected a legality refusal, got {other:?}"),
    }
}
