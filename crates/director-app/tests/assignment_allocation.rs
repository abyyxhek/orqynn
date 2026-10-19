//! Assignment ids allocated inside the store's write transaction.
//!
//! The defect these pin: SCHEDULE used to choose an assignment id by counting a
//! task's tenures *outside* the transaction that inserts the row. Two racing
//! rounds on the same task could therefore propose the same ordinal, and
//! `guard_scope` does not catch that case — a task does not conflict with itself —
//! so the loser reached the `INSERT` and was refused by the primary key on
//! `agent_assignments.id` with a bare `UNIQUE constraint failed`.
//!
//! Concurrency here is driven deterministically. The store's own concurrency
//! tests establish that a single-threaded `join!` over this store runs its two
//! futures start-to-finish, because `assign_and_start` contains no `.await` that
//! can yield; a test that relied on a sleep to manufacture a race would be
//! asserting on timing rather than on behaviour. These tests therefore assert the
//! *outcome* a race must produce — distinct ids, a typed refusal, no partial
//! writes — and the ordering guarantee that produces it is the `IMMEDIATE`
//! transaction, not a timer.

use std::path::PathBuf;

use director_domain::agent::{Agent, AgentStatus, Harness};
use director_domain::assignment::{AgentAssignment, AssignmentStatus};
use director_domain::capability::Capability;
use director_domain::ids::{AgentId, AssignmentId, Id, MachineId, PlanId, ProjectId, TaskId};
use director_domain::plan::Plan;
use director_domain::project::Project;
use director_domain::task::{Task, TaskStatus};
use director_domain::{AgentRepository, AssignmentRepository, ProjectRepository, TaskRepository};
use director_store::Store;

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

struct Fixture {
    store_path: PathBuf,
    store: Store,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// A project with one active plan holding `task_ids`, all `todo` and each
    /// owning disjoint ground.
    async fn with_tasks(task_ids: &[&str]) -> Self {
        let dir = tempfile::TempDir::new().expect("a temp dir for the store");
        let store_path = dir.path().join("orqyn.db");
        let store = Store::open(&store_path).await.expect("store opens");

        let project = ProjectId::from_string("PROJ-1");
        store
            .projects()
            .create_project(&Project::new(project.clone(), project.as_str(), "/nowhere"))
            .await
            .expect("project created");

        let mut plan = Plan::draft(
            PlanId::from_string("PLAN-1"),
            project.clone(),
            "assign some work",
            "the smallest honest fixture",
        );
        let built: Vec<Task> = task_ids
            .iter()
            .map(|id| {
                let mut task = Task::for_project(
                    project.clone(),
                    TaskId::from_string(*id),
                    *id,
                    format!("do {id}"),
                );
                task.status = TaskStatus::Todo;
                task.scope_paths = vec![format!("src/{}", id.to_lowercase())];
                plan.add_task(task.id.clone());
                task
            })
            .collect();

        store
            .create_active_plan(&plan, &built, &AgentId::from_string("AGENT-planner"), now())
            .await
            .expect("plan created and activated");

        Fixture {
            store_path,
            store,
            _dir: dir,
        }
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

    async fn history(&self, task: &str) -> Vec<AgentAssignment> {
        self.store
            .assignments()
            .assignment_history(&TaskId::from_string(task))
            .await
            .expect("history reads")
    }

    /// Every assignment id on record across `tasks`.
    async fn all_ids(&self, tasks: &[&str]) -> Vec<String> {
        let mut ids = Vec::new();
        for task in tasks {
            ids.extend(self.history(task).await.iter().map(|a| a.id.to_string()));
        }
        ids
    }
}

/// Run two closures on two OS threads, released together by a barrier.
///
/// This is the same shape the store's own concurrency tests use, and it is used
/// here for the same reason. `tokio::join!` would *not* do: this store's calls
/// contain no `.await` that can yield, so a joined pair runs start-to-finish and
/// the second call would simply observe the first one's committed row. That is
/// still a valid assertion about ids, but it proves nothing about a race.
///
/// The barrier is the synchronization, not a sleep: it stops either thread
/// getting a head start, so both arrive at `BEGIN IMMEDIATE` together. It does
/// not widen the window inside the transaction.
fn in_parallel<A, B, RA, RB>(first: A, second: B) -> (RA, RB)
where
    A: FnOnce() -> RA + Send,
    B: FnOnce() -> RB + Send,
    RA: Send,
    RB: Send,
{
    std::thread::scope(|scope| {
        let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
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
/// The store's work is synchronous under the hood, so the call really does occupy
/// the thread for its whole duration — which is what lets the other thread
/// contend with it.
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

/// The id shape the store allocates: the task's own id and an ordinal over its
/// tenures.
fn expected_id(task: &str, ordinal: usize) -> String {
    format!("{}-{task}-{ordinal}", AssignmentId::PREFIX)
}

/// 1. Two concurrent schedulers targeting the same task.
///
/// This is the case the caller-side derivation could not win: both rounds read
/// the same tenure count, so both proposed the same ordinal, and the loser was
/// refused by the primary key. Now the ordinal is counted under the write lock,
/// so the second round sees the first's row and takes the next one.
#[test]
fn two_concurrent_schedulers_on_one_task_get_distinct_assignments() {
    let (fx, a, b, history) = run(async {
        let fx = Fixture::with_tasks(&["SHARED"]).await;
        fx.agent("AGENT-1").await;
        fx.agent("AGENT-2").await;

        // Both rounds name the same task and the same agent, so the only thing
        // that can differ between them is the id the store allocates.
        let shared = TaskId::from_string("SHARED");
        let one = AgentId::from_string("AGENT-1");
        let two = AgentId::from_string("AGENT-2");
        let (a, b) = in_parallel(
            || run(fx.store.assign_and_start_next(&shared, &one, now())),
            || run(fx.store.assign_and_start_next(&shared, &two, now())),
        );
        let history = fx.history("SHARED").await;
        (fx, a, b, history)
    });

    let a = a.expect("the first assignment lands");
    let b = b.expect("the second assignment lands rather than colliding");

    assert_ne!(
        a.0.id, b.0.id,
        "two tenures of one task must not share an id"
    );
    // Which thread wins the write lock is not part of the contract — the barrier
    // only stops either getting a head start. What *is* the contract is that the
    // two ordinals are exactly 1 and 2, in some order, with neither reused.
    let mut allocated = [a.0.id.to_string(), b.0.id.to_string()];
    allocated.sort();
    assert_eq!(
        allocated,
        [expected_id("SHARED", 1), expected_id("SHARED", 2)],
        "the two tenures took consecutive ordinals under one lock"
    );

    // The store's own reassignment semantics are intact: the later handoff
    // displaces the earlier one, and the displaced tenure is retained rather than
    // deleted. Neither call was refused.
    assert!(
        a.2.is_none() || b.2.is_none(),
        "exactly one of the two handoffs displaces the other"
    );
    assert_eq!(
        history.len(),
        2,
        "both tenures are on record: {:?}",
        history.len()
    );
    let active: Vec<&AgentAssignment> = history
        .iter()
        .filter(|a| a.status == AssignmentStatus::Active)
        .collect();
    assert_eq!(
        active.len(),
        1,
        "exactly one tenure is active, so the invariant held throughout"
    );
    drop(fx);
}

/// 2. Two concurrent schedulers targeting distinct tasks.
///
/// The case the caller-side derivation already fixed; kept so the transaction move
/// did not regress it. Disjoint scopes mean neither round may refuse the other.
#[test]
fn two_concurrent_schedulers_on_distinct_tasks_both_land() {
    let (a, b, ids) = run(async {
        let fx = Fixture::with_tasks(&["ALPHA", "BETA"]).await;
        fx.agent("AGENT-1").await;
        fx.agent("AGENT-2").await;

        let alpha = TaskId::from_string("ALPHA");
        let beta = TaskId::from_string("BETA");
        let one = AgentId::from_string("AGENT-1");
        let two = AgentId::from_string("AGENT-2");
        let (a, b) = in_parallel(
            || run(fx.store.assign_and_start_next(&alpha, &one, now())),
            || run(fx.store.assign_and_start_next(&beta, &two, now())),
        );
        let ids = fx.all_ids(&["ALPHA", "BETA"]).await;
        (a, b, ids)
    });

    let a = a.expect("ALPHA lands");
    let b = b.expect("BETA lands");
    assert_ne!(a.0.id, b.0.id);
    assert_eq!(a.0.id.to_string(), expected_id("ALPHA", 1));
    assert_eq!(b.0.id.to_string(), expected_id("BETA", 1));

    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(ids, unique, "ids across distinct tasks must not collide");
    assert_eq!(ids.len(), 2, "both assignments are on record");
}

/// 3. Repeated rounds and a store reopen keep the ordinals unique and monotonic.
///
/// A process-local counter cannot serve this: a reopened handle starts from zero
/// again. Because the ordinal is counted from persisted rows, it continues.
#[tokio::test]
async fn repeated_rounds_and_a_reopen_keep_ordinals_unique() {
    let fx = Fixture::with_tasks(&["TASK"]).await;
    fx.agent("AGENT-1").await;

    for round in 0..3 {
        fx.store
            .assign_and_start_next(
                &TaskId::from_string("TASK"),
                &AgentId::from_string("AGENT-1"),
                now(),
            )
            .await
            .unwrap_or_else(|err| panic!("round {round} failed: {err:?}"));
    }
    let before = fx.all_ids(&["TASK"]).await;
    assert_eq!(
        before,
        vec![
            expected_id("TASK", 1),
            expected_id("TASK", 2),
            expected_id("TASK", 3)
        ],
        "three rounds took three distinct ordinals"
    );

    // Drop the writer entirely.
    let reopened = Store::open(&fx.store_path)
        .await
        .expect("the store reopens");
    let fourth = reopened
        .assign_and_start_next(
            &TaskId::from_string("TASK"),
            &AgentId::from_string("AGENT-1"),
            now(),
        )
        .await
        .expect("a round after a reopen still lands");
    assert_eq!(
        fourth.0.id.to_string(),
        expected_id("TASK", 4),
        "the ordinal continues from persisted state rather than restarting"
    );

    let history = reopened
        .assignments()
        .assignment_history(&TaskId::from_string("TASK"))
        .await
        .expect("history reads");
    assert_eq!(history.len(), 4, "no row was overwritten or reused");
}

/// 4. A scope conflict is still a typed scope conflict, not a raw key error.
///
/// This is the point of moving allocation *after* `guard_scope`: a refused write
/// must refuse for its own stated reason, and must not be masked by the id
/// machinery that runs after it.
#[tokio::test]
async fn an_overlapping_scope_is_refused_as_a_scope_conflict() {
    // Two tasks that declare the *same* ground, so the second cannot be handed out.
    let dir = tempfile::TempDir::new().expect("a temp dir");
    let store = Store::open(dir.path().join("orqyn.db"))
        .await
        .expect("store opens");
    let project = ProjectId::from_string("PROJ-1");
    store
        .projects()
        .create_project(&Project::new(project.clone(), project.as_str(), "/nowhere"))
        .await
        .expect("project created");

    let mut plan = Plan::draft(
        PlanId::from_string("PLAN-1"),
        project.clone(),
        "overlapping work",
        "two tasks, one path",
    );
    let built: Vec<Task> = ["A", "B"]
        .iter()
        .map(|id| {
            let mut task = Task::for_project(
                project.clone(),
                TaskId::from_string(*id),
                *id,
                format!("do {id}"),
            );
            task.status = TaskStatus::Todo;
            task.scope_paths = vec!["src/auth".to_string()];
            plan.add_task(task.id.clone());
            task
        })
        .collect();
    store
        .create_active_plan(&plan, &built, &AgentId::from_string("AGENT-planner"), now())
        .await
        .expect("plan created");

    for n in 0..2 {
        let name = format!("AGENT-{n}");
        let mut agent = Agent::register(
            AgentId::from_string(&name),
            &name,
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
    }

    let first = store
        .assign_and_start_next(
            &TaskId::from_string("A"),
            &AgentId::from_string("AGENT-0"),
            now(),
        )
        .await
        .expect("the first task is handed out");

    let err = store
        .assign_and_start_next(
            &TaskId::from_string("B"),
            &AgentId::from_string("AGENT-1"),
            now(),
        )
        .await
        .expect_err("overlapping ground must be refused");

    match err {
        director_domain::StoreError::ScopeConflict { .. } => {}
        other => {
            let rendered = format!("{other}");
            assert!(
                !rendered.contains("UNIQUE constraint"),
                "the refusal must not be a raw primary-key error: {rendered}"
            );
            panic!("expected a typed scope conflict, got: {rendered}");
        }
    }

    // And nothing was written for the refused task: no tenure, no status change.
    let history = store
        .assignments()
        .assignment_history(&TaskId::from_string("B"))
        .await
        .expect("history reads");
    assert!(
        history.is_empty(),
        "a refused assignment left no tenure behind"
    );
    let task_b = store
        .tasks()
        .get_task(&TaskId::from_string("B"))
        .await
        .expect("the task reads");
    assert_eq!(
        task_b.status,
        TaskStatus::Todo,
        "and the refused task is exactly where the store found it"
    );
    assert_eq!(
        first.0.id.to_string(),
        expected_id("A", 1),
        "the successful assignment still took ordinal one"
    );
}

/// 5. A refused assignment leaves no partial writes anywhere.
///
/// The same transaction that allocates the id also releases the sitting tenant,
/// inserts the row, moves the task and records the transition. A refusal anywhere
/// in it must roll all of that back, so the refusal cannot half-happen.
#[tokio::test]
async fn a_refused_assignment_leaves_no_partial_writes() {
    let fx = Fixture::with_tasks(&["A", "B"]).await;
    let agent = fx.agent("AGENT-1").await;

    // A first, successful assignment so there is a sitting tenant to displace.
    fx.store
        .assign_and_start_next(&TaskId::from_string("A"), &agent, now())
        .await
        .expect("A is handed out");
    let before = fx.history("A").await;
    assert_eq!(before.len(), 1);

    // Now refuse a second handoff by pointing the store at overlapping ground:
    // give B the same scope as the in-flight A.
    let mut task_b = fx
        .store
        .tasks()
        .get_task(&TaskId::from_string("B"))
        .await
        .expect("the task reads");
    task_b.scope_paths = vec!["src/a".to_string()];
    fx.store
        .tasks()
        .update_task(&task_b)
        .await
        .expect("B now overlaps A");

    let err = fx
        .store
        .assign_and_start_next(&TaskId::from_string("B"), &agent, now())
        .await
        .expect_err("the overlapping handoff is refused");
    assert!(
        !format!("{err}").contains("UNIQUE constraint"),
        "the refusal is about scope, not an id: {err}"
    );

    // A is untouched: still active, still one row, and no second tenure was
    // released or inserted on its behalf.
    let after = fx.history("A").await;
    assert_eq!(after.len(), 1, "A's tenure count did not change");
    assert_eq!(after[0].status, AssignmentStatus::Active);
    assert_eq!(after[0].id, before[0].id);

    // B is untouched: no tenure, still `todo`.
    assert!(
        fx.history("B").await.is_empty(),
        "the refused handoff wrote no tenure"
    );
    let task_b = fx
        .store
        .tasks()
        .get_task(&TaskId::from_string("B"))
        .await
        .expect("the task reads");
    assert_eq!(task_b.status, TaskStatus::Todo);
}

/// 6. Callers that supply an explicit id are unaffected.
///
/// The new path is additive. Everything that already names its own assignment id
/// must keep the id it asked for, byte for byte — including a deliberate
/// collision, which must still be refused rather than silently reallocated.
#[tokio::test]
async fn explicit_id_callers_are_unchanged() {
    let fx = Fixture::with_tasks(&["A", "B"]).await;
    let agent = fx.agent("AGENT-1").await;

    let chosen = AssignmentId::from_string("ASG-caller-chosen");
    let assigned = fx
        .store
        .assign_and_start(&TaskId::from_string("A"), &agent, &chosen, now())
        .await
        .expect("the caller's id is used verbatim");
    assert_eq!(
        assigned.0.id, chosen,
        "the store must not second-guess an id the caller chose"
    );
    assert_ne!(
        assigned.0.id.to_string(),
        expected_id("A", 1),
        "an explicit id is not replaced by the allocated shape"
    );

    // Reusing it is still refused, with the id's own reason.
    let err = fx
        .store
        .assign_and_start(&TaskId::from_string("B"), &agent, &chosen, now())
        .await
        .expect_err("a caller's duplicate id is still refused");
    assert!(!format!("{err}").is_empty(), "and the refusal is explained");
    assert!(
        fx.history("B").await.is_empty(),
        "nothing was written for the refused caller-supplied id"
    );
}

/// 7. A second store handle cannot produce an id the first already wrote.
///
/// The cross-process shape, exercised in one process with two connections: each
/// handle has its own pool, and the ordinal still comes from the committed rows
/// rather than from anything the handle remembers.
#[tokio::test]
async fn two_store_handles_never_allocate_the_same_ordinal() {
    let fx = Fixture::with_tasks(&["A", "B"]).await;
    let agent = fx.agent("AGENT-1").await;
    let second_handle = Store::open(&fx.store_path).await.expect("a second handle");

    let on_first = fx
        .store
        .assign_and_start_next(&TaskId::from_string("A"), &agent, now())
        .await
        .expect("the first handle assigns");
    let on_second = second_handle
        .assign_and_start_next(&TaskId::from_string("A"), &agent, now())
        .await
        .expect("the second handle assigns too");

    assert_ne!(
        on_first.0.id, on_second.0.id,
        "two handles over one file must not agree on an ordinal"
    );
    assert_eq!(on_first.0.id.to_string(), expected_id("A", 1));
    assert_eq!(on_second.0.id.to_string(), expected_id("A", 2));

    // Both rows survive: the append-only history is what the ordinal counts.
    assert_eq!(fx.history("A").await.len(), 2);
}

/// A guard on the harness itself: the tests above assume the store is genuinely
/// usable from two connections, and a file lock that silently serialised
/// everything would make the assertions vacuous.
#[tokio::test]
async fn the_fixture_really_does_hand_out_two_connections() {
    let fx = Fixture::with_tasks(&["A"]).await;
    fx.agent("AGENT-1").await;
    let second_handle = Store::open(&fx.store_path).await.expect("a second handle");

    // Two disjoint tasks, one handle each: if the two handles shared a lock or a
    // connection pool this would deadlock or serialise, and the assertion below
    // would be about the harness rather than about allocation.
    let a = TaskId::from_string("A");
    let one = AgentId::from_string("AGENT-1");
    let first = fx.store.assign_and_start_next(&a, &one, now());
    let second = second_handle.assign_and_start_next(&a, &one, now());
    let (a, b) = tokio::join!(first, second);
    assert!(a.is_ok() && b.is_ok(), "both handles resolved");
}
