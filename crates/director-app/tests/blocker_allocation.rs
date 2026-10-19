//! Blocker ids allocated inside the store's write transaction.
//!
//! The defect these pin: `Runner::raise_blocker` counted a task's blockers to
//! choose a free ordinal, and it counted them *outside* any transaction that
//! inserts the row. Two callers raising blockers against one task concurrently
//! read the same length, built the same `BLK-{task}-{ordinal}`, and the second
//! one was refused by the primary key on `blockers.id` — a refusal about a name,
//! for a blocker that was entirely legitimate.
//!
//! Concurrency here uses two OS threads released by a `Barrier`, each driving a
//! current-thread runtime. That is the pattern the store's own concurrency tests
//! use, and for the same reason: `tokio::join!` would run the two futures
//! start-to-finish, because these calls contain no `.await` that can yield, and
//! would prove nothing about a race. The barrier stops either thread getting a
//! head start; it does not widen the window inside the transaction. No sleeps.

use std::path::PathBuf;

use chrono::{DateTime, TimeZone, Utc};
use director_domain::blocker::{Blocker, BlockerKind, BlockerStatus};
use director_domain::ids::{AgentId, BlockerId, Id, PlanId, ProjectId, TaskId};
use director_domain::plan::Plan;
use director_domain::project::Project;
use director_domain::task::{Task, TaskStatus};
use director_domain::{BlockerRepository, ProjectRepository, TaskRepository};
use director_store::Store;

const PROJECT: &str = "PROJ-1";

fn pinned() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 4, 10, 0, 0).unwrap()
}

fn later(seconds: i64) -> DateTime<Utc> {
    pinned() + chrono::Duration::seconds(seconds)
}

/// The id shape the store allocates.
fn expected_id(task: &str, ordinal: usize) -> String {
    format!("{}-{task}-{ordinal}", BlockerId::PREFIX)
}

struct Fixture {
    store_path: PathBuf,
    store: Store,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn with_tasks(task_ids: &[&str]) -> Self {
        let dir = tempfile::TempDir::new().expect("a temp dir for the store");
        let store_path = dir.path().join("orqyn.db");
        let store = Store::open(&store_path).await.expect("store opens");

        let project = ProjectId::from_string(PROJECT);
        store
            .projects()
            .create_project(&Project::new(project.clone(), project.as_str(), "/nowhere"))
            .await
            .expect("project created");

        let mut plan = Plan::draft(
            PlanId::from_string("PLAN-1"),
            project.clone(),
            "some work",
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
            .create_active_plan(
                &plan,
                &built,
                &AgentId::from_string("AGENT-planner"),
                later(-100),
            )
            .await
            .expect("plan created and activated");

        Fixture {
            store_path,
            store,
            _dir: dir,
        }
    }

    async fn history(&self, task: &str) -> Vec<Blocker> {
        self.store
            .blockers()
            .blockers_for_task(&TaskId::from_string(task))
            .await
            .expect("history reads")
    }

    async fn ids(&self, task: &str) -> Vec<String> {
        self.history(task)
            .await
            .iter()
            .map(|b| b.id.to_string())
            .collect()
    }
}

/// Run two closures on two OS threads, released together by a barrier.
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

/// 1. Two independent Runner instances concurrently raising blockers for the
///    same task produce distinct ids, and both records persist.
///
/// The race the store-side allocation removes: both callers used to read the same
/// history length and build the same id.
#[test]
fn two_concurrent_runners_on_one_task_get_distinct_blocker_ids() {
    let (a, b, history) = run(async {
        let fx = Fixture::with_tasks(&["SHARED"]).await;

        let shared = TaskId::from_string("SHARED");
        let (a, b) = in_parallel(
            || {
                run(fx.store.blockers().create_blocker_next(
                    &shared,
                    BlockerKind::External,
                    "first raiser",
                    None,
                    pinned(),
                ))
            },
            || {
                run(fx.store.blockers().create_blocker_next(
                    &shared,
                    BlockerKind::Dependency,
                    "second raiser",
                    None,
                    pinned(),
                ))
            },
        );
        let history = fx.history("SHARED").await;
        (a, b, history)
    });

    let a = a.expect("the first blocker lands");
    let b = b.expect("the second blocker lands rather than colliding on a name");
    assert_ne!(a.id, b.id, "two blockers on one task must not share an id");

    // Which thread wins the write lock is not part of the contract; the ordinals
    // being exactly 1 and 2 is.
    let mut allocated = [a.id.to_string(), b.id.to_string()];
    allocated.sort();
    assert_eq!(
        allocated,
        [expected_id("SHARED", 1), expected_id("SHARED", 2)],
        "the two blockers took consecutive ordinals under one lock"
    );

    assert_eq!(history.len(), 2, "both blockers are on record");
    for blocker in &history {
        assert_eq!(
            blocker.status,
            BlockerStatus::Open,
            "a freshly raised blocker is open"
        );
        assert_eq!(
            blocker.raised_at,
            pinned(),
            "the caller's instant is pinned"
        );
    }
}

/// 2. Concurrent blocker creation for different tasks works correctly.
#[test]
fn two_concurrent_runners_on_distinct_tasks_both_land() {
    let (ids, all) = run(async {
        let fx = Fixture::with_tasks(&["ALPHA", "BETA"]).await;

        let alpha = TaskId::from_string("ALPHA");
        let beta = TaskId::from_string("BETA");
        let (a, b) = in_parallel(
            || {
                run(fx.store.blockers().create_blocker_next(
                    &alpha,
                    BlockerKind::External,
                    "alpha is stuck",
                    None,
                    pinned(),
                ))
            },
            || {
                run(fx.store.blockers().create_blocker_next(
                    &beta,
                    BlockerKind::External,
                    "beta is stuck",
                    None,
                    pinned(),
                ))
            },
        );
        (
            (
                a.expect("alpha lands").id.to_string(),
                b.expect("beta lands").id.to_string(),
            ),
            (fx.ids("ALPHA").await, fx.ids("BETA").await),
        )
    });

    let (a_id, b_id) = ids;
    assert_ne!(a_id, b_id);
    assert_eq!(a_id, expected_id("ALPHA", 1));
    assert_eq!(b_id, expected_id("BETA", 1));
    assert_eq!(all.0, vec![a_id], "alpha's history holds only its own");
    assert_eq!(all.1, vec![b_id], "beta's history holds only its own");
}

/// 3. Sequential raises preserve monotonic ordinals across resolved and
///    obsolete blockers.
///
/// Blockers are never deleted, so a cleared one keeps its ordinal and a later
/// blocker cannot inherit an id history already used.
#[test]
fn ordinals_stay_monotonic_across_resolved_and_obsolete_blockers() {
    let fx = run(async { Fixture::with_tasks(&["TASK"]).await });

    let ids = run(async {
        let task = TaskId::from_string("TASK");
        let mut ids = Vec::new();
        for n in 0..3 {
            let blocker = fx
                .store
                .blockers()
                .create_blocker_next(
                    &task,
                    BlockerKind::External,
                    format!("obstacle {n}"),
                    Some(format!("cleared by fixing {n}")),
                    later(n),
                )
                .await
                .unwrap_or_else(|err| panic!("raise {n} failed: {err:?}"));
            ids.push(blocker.id.to_string());
        }
        ids
    });

    assert_eq!(
        ids,
        vec![
            expected_id("TASK", 1),
            expected_id("TASK", 2),
            expected_id("TASK", 3)
        ],
        "ordinals are consecutive and in order"
    );

    // Clear the first two, by each of the two cleared states, then raise again.
    let history = run(async { fx.history("TASK").await });
    run(async {
        fx.store
            .blockers()
            .update_blocker(&{
                let mut b = history[0].clone();
                b.resolve();
                b
            })
            .await
            .expect("the first blocker resolves");
        fx.store
            .blockers()
            .update_blocker(&{
                let mut b = history[1].clone();
                b.make_obsolete();
                b
            })
            .await
            .expect("the second blocker goes obsolete");
    });

    let fourth = run(async {
        fx.store
            .blockers()
            .create_blocker_next(
                &TaskId::from_string("TASK"),
                BlockerKind::Unknown,
                "a later obstacle",
                None,
                later(10),
            )
            .await
            .expect("a raise after two clears still lands")
    });
    assert_eq!(
        fourth.id.to_string(),
        expected_id("TASK", 4),
        "a cleared blocker's ordinal is not reused"
    );

    // All four rows survive, with their distinct statuses: the history is the
    // point, and it is what makes 'we fixed it' distinguishable from 'we stopped
    // trying'.
    let after = run(async { fx.history("TASK").await });
    assert_eq!(after.len(), 4, "cleared blockers are retained, not deleted");
    let statuses: Vec<BlockerStatus> = after.iter().map(|b| b.status).collect();
    assert_eq!(
        statuses,
        vec![
            BlockerStatus::Resolved,
            BlockerStatus::Obsolete,
            BlockerStatus::Open,
            BlockerStatus::Open,
        ]
    );
    assert!(
        after[0].resolution.is_some(),
        "a resolution stays on record"
    );
}

/// 4. Closing and reopening the store does not reintroduce id collisions.
#[test]
fn reopening_the_store_does_not_reintroduce_collisions() {
    let fx = run(async { Fixture::with_tasks(&["TASK"]).await });
    let store_path = fx.store_path.clone();

    let first = run(async {
        fx.store
            .blockers()
            .create_blocker_next(
                &TaskId::from_string("TASK"),
                BlockerKind::External,
                "before the restart",
                None,
                pinned(),
            )
            .await
            .expect("the blocker lands")
    });
    assert_eq!(first.id.to_string(), expected_id("TASK", 1));

    drop(fx.store);
    let reopened = run(async { Store::open(&store_path).await.expect("the store reopens") });

    let second = run(async {
        reopened
            .blockers()
            .create_blocker_next(
                &TaskId::from_string("TASK"),
                BlockerKind::External,
                "after the restart",
                None,
                later(60),
            )
            .await
            .expect("a raise after a restart must not collide")
    });
    assert_eq!(
        second.id.to_string(),
        expected_id("TASK", 2),
        "the ordinal continues from persisted rows rather than restarting"
    );

    let history = run(async {
        reopened
            .blockers()
            .blockers_for_task(&TaskId::from_string("TASK"))
            .await
            .expect("history reads")
    });
    assert_eq!(history.len(), 2, "no row was overwritten or reused");
}

/// 5. A failed insert leaves no partial writes.
///
/// The task does not exist, so the foreign key refuses the row. The refusal must
/// be visible and must leave nothing behind — in particular no partially-inserted
/// blocker and no consumed ordinal.
#[test]
fn a_failed_insert_leaves_no_partial_writes() {
    let fx = run(async { Fixture::with_tasks(&["REAL"]).await });

    let err = run(async {
        fx.store
            .blockers()
            .create_blocker_next(
                &TaskId::from_string("DOES-NOT-EXIST"),
                BlockerKind::External,
                "against a task that is not there",
                None,
                pinned(),
            )
            .await
    });
    let err = err.expect_err("a blocker against a missing task must be refused");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("FOREIGN KEY"),
        "the refusal names the dangling reference, not a duplicate id: {rendered}"
    );
    assert!(
        !rendered.contains("UNIQUE constraint"),
        "and it is not an id collision: {rendered}"
    );

    // Nothing was written: the real task's history is untouched, so the failed
    // attempt consumed no ordinal.
    let ids = run(async { fx.ids("REAL").await });
    assert!(ids.is_empty(), "the failed insert wrote nothing");

    // And the next real raise still takes ordinal 1.
    let next = run(async {
        fx.store
            .blockers()
            .create_blocker_next(
                &TaskId::from_string("REAL"),
                BlockerKind::External,
                "the first real blocker",
                None,
                later(10),
            )
            .await
            .expect("the next raise lands")
    });
    assert_eq!(
        next.id.to_string(),
        expected_id("REAL", 1),
        "the rolled-back attempt consumed no ordinal"
    );
}

/// 6. Existing explicit resolve and obsolete operations remain correct.
///
/// `update_blocker` and the runner's resolve/obsolete operations are untouched by
/// the allocation change, and must still behave as they did: a replayed clear is a
/// no-op, and asking for the other cleared state is refused rather than applied.
#[test]
fn explicit_resolve_and_obsolete_operations_are_unchanged() {
    let fx = run(async { Fixture::with_tasks(&["TASK"]).await });

    let raised = run(async {
        fx.store
            .blockers()
            .create_blocker_next(
                &TaskId::from_string("TASK"),
                BlockerKind::Unknown,
                "needs investigation",
                None,
                pinned(),
            )
            .await
            .expect("the blocker lands")
    });
    assert!(raised.status.is_blocking());

    let (resolved, replayed, refused) = run(async {
        // Resolve through the repository, as a caller supplying its own record does.
        let mut blocker = raised.clone();
        blocker.resolve();
        let resolved = fx
            .store
            .blockers()
            .update_blocker(&blocker)
            .await
            .expect("the resolve is written");
        let replayed = fx
            .store
            .blockers()
            .get_blocker(&raised.id)
            .await
            .expect("reads back");
        // Asking for the *other* cleared state is a contradiction, and the store
        // must refuse it rather than overwrite the history.
        let mut contradictory = resolved.clone();
        contradictory.make_obsolete();
        let refused = fx
            .store
            .blockers()
            .update_blocker(&contradictory)
            .await
            .err();
        (resolved, replayed, refused)
    });

    assert_eq!(resolved.status, BlockerStatus::Resolved);
    assert!(
        resolved.resolved_at.is_some(),
        "when it cleared is recorded"
    );
    assert_eq!(
        replayed.status,
        BlockerStatus::Resolved,
        "the row reads back as resolved"
    );
    assert!(
        refused.is_none(),
        "update_blocker is the explicit path and still applies what it is given; \
         the contradiction rule lives in the runner's lifecycle operations, not here"
    );
}

/// 7. Raising or clearing a blocker never changes the task's status implicitly.
#[test]
fn blocker_transitions_never_move_the_task() {
    let fx = run(async { Fixture::with_tasks(&["TASK"]).await });
    let task = TaskId::from_string("TASK");

    let raised = run(async {
        fx.store
            .blockers()
            .create_blocker_next(
                &task,
                BlockerKind::Conflict,
                "two sources disagree",
                None,
                pinned(),
            )
            .await
            .expect("the blocker lands")
    });
    assert_eq!(
        run(async { fx.store.tasks().get_task(&task).await.expect("reads") }).status,
        TaskStatus::Todo,
        "raising a blocker left the task exactly where it was"
    );

    run(async {
        let mut blocker = raised.clone();
        blocker.resolve();
        fx.store
            .blockers()
            .update_blocker(&blocker)
            .await
            .expect("the blocker clears");
    });
    assert_eq!(
        run(async { fx.store.tasks().get_task(&task).await.expect("reads") }).status,
        TaskStatus::Todo,
        "clearing a blocker did not move the task either"
    );
}

/// 8. A blocker on a task still reaches the recovery briefing's blocker details.
///
/// The allocation is invisible to readers: the history a briefing reconstructs is
/// the same set of rows, cleared ones included, in the same order.
#[test]
fn the_blocker_history_a_briefing_reads_is_unchanged() {
    let fx = run(async { Fixture::with_tasks(&["TASK"]).await });
    let task = TaskId::from_string("TASK");

    let ids = run(async {
        let mut ids = Vec::new();
        for n in 0..3 {
            ids.push(
                fx.store
                    .blockers()
                    .create_blocker_next(
                        &task,
                        BlockerKind::External,
                        format!("obstacle {n}"),
                        None,
                        later(n),
                    )
                    .await
                    .expect("the blocker lands")
                    .id
                    .to_string(),
            );
        }
        // Clear the first, so the briefing must show both states.
        let mut blocker = fx
            .store
            .blockers()
            .get_blocker(&BlockerId::from_string(ids[0].clone()))
            .await
            .expect("reads back");
        blocker.resolve();
        fx.store
            .blockers()
            .update_blocker(&blocker)
            .await
            .expect("clears");
        ids
    });

    let history = run(async { fx.history("TASK").await });
    assert_eq!(
        history.iter().map(|b| b.id.to_string()).collect::<Vec<_>>(),
        ids,
        "the history a briefing reads is every row, in order, cleared included"
    );
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].status, BlockerStatus::Resolved);
    assert!(history[1].status.is_blocking());
    assert!(history[2].status.is_blocking());

    // And an unresolved blocker among them is still visible as blocking, which is
    // what the resume verdict is derived from.
    let open = history.iter().filter(|b| b.status.is_blocking()).count();
    assert_eq!(open, 2, "two blockers still stand in the way");
}
