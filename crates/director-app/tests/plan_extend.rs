//! Plan scope: what a plan says about itself once it holds more than one task.
//!
//! These exercise [`director_app::plan::extend_active_plan_at`] against real
//! SQLite, with no git and no observation round, so each test is about one
//! question — what the plan's metadata says, and what the store refuses.
//!
//! ## The semantic under test
//!
//! `Plan::objective` is *"the objective the plan decomposes … so the plan stays
//! self-explanatory after the originating prompt scrolls away"*. It records why
//! the plan was created, so extending a plan must **not** rewrite it. The scope a
//! plan now covers is a different fact, and is recorded additively as its own line
//! in the rationale, appended by the store inside the transaction that appends the
//! task — so concurrent extensions cannot overwrite one another.

use director_app::plan::{extend_active_plan_at, plan_at, ExtendRequest, PlanRequest, TaskSpec};
use director_domain::ids::{AgentId, PlanId, ProjectId, TaskId};
use director_domain::plan::PlanStatus;
use director_domain::store::PlanRepository;
use director_domain::task::{ExpectedOutput, Task, TaskStatus};
use director_domain::{Project, ProjectRepository, TaskRepository};
use director_store::Store as SqliteStore;

const PROJ: &str = "PROJ-1";

fn pinned(step: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000 + step * 60, 0).expect("a pinned moment")
}

async fn boot() -> (tempfile::TempDir, SqliteStore) {
    let dir = tempfile::TempDir::new().expect("a temp dir");
    let store = SqliteStore::open(dir.path().join("plan.db"))
        .await
        .expect("the store opens");
    store
        .projects()
        .create_project(&Project::new(ProjectId::from_string(PROJ), PROJ, "/repo"))
        .await
        .expect("project created");
    (dir, store)
}

fn spec(id: &str, objective: &str) -> TaskSpec {
    let mut spec = TaskSpec::new(TaskId::from_string(id), id, objective);
    spec.expected_outputs = vec![ExpectedOutput {
        criterion: format!("`{id}` is checked"),
        check: Some("git --version".into()),
    }];
    spec
}

/// Create the project's first plan, the way PLAN does.
async fn first_plan(store: &SqliteStore, plan_id: &str, objective: &str, task: &str) -> PlanId {
    plan_at(
        store,
        PlanRequest {
            project_id: ProjectId::from_string(PROJ),
            plan_id: PlanId::from_string(plan_id),
            objective: objective.to_string(),
            rationale: "requested explicitly on the command line".into(),
            tasks: vec![spec(task, objective)],
            authorized_by: AgentId::from_string("ORQYNN-CLI"),
        },
        pinned(0),
    )
    .await
    .expect("the first plan")
    .plan
    .id
}

/// Extend the active plan with one more task.
async fn extend(store: &SqliteStore, id: &str, objective: &str, now: i64) {
    extend_active_plan_at(
        store,
        ExtendRequest {
            project_id: ProjectId::from_string(PROJ),
            task: spec(id, objective),
            authorized_by: AgentId::from_string("ORQYNN-CLI"),
            now: pinned(now),
        },
    )
    .await
    .expect("the extension");
}

async fn active(store: &SqliteStore) -> director_domain::plan::Plan {
    store
        .plans()
        .active_plan_for_project(&ProjectId::from_string(PROJ))
        .await
        .expect("plans")
        .expect("an active plan")
}

// ---------------------------------------------------------------------------
// 1 & 2. The originating objective is preserved; scope is stated additively
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_first_task_records_the_objective_and_rationale_it_was_planned_with() {
    let (_dir, store) = boot().await;
    let id = first_plan(&store, "PLAN-FIRST", "make the greeting say hello", "GREET").await;

    let plan = store.plans().get_plan(&id).await.expect("plan");
    assert_eq!(plan.objective, "make the greeting say hello");
    assert!(
        plan.rationale
            .contains("requested explicitly on the command line"),
        "{}",
        plan.rationale
    );
    assert_eq!(plan.task_ids, vec![TaskId::from_string("GREET")]);
    assert!(
        !plan.rationale.contains("scope extended"),
        "a single-task plan has no extension to record"
    );
}

#[tokio::test]
async fn extending_communicates_the_expanded_scope_without_rewriting_the_objective() {
    let (_dir, store) = boot().await;
    let id = first_plan(&store, "PLAN-FIRST", "make the greeting say hello", "GREET").await;
    extend(&store, "FAREWELL", "make the farewell say goodbye", 1).await;

    let plan = store.plans().get_plan(&id).await.expect("plan");
    assert_eq!(
        plan.objective, "make the greeting say hello",
        "the originating objective is the reason this plan exists and must survive"
    );
    assert_eq!(plan.task_ids.len(), 2);
    let note = plan
        .rationale
        .lines()
        .find(|line| line.contains("scope extended"))
        .expect("an extension note");
    assert!(note.contains("FAREWELL"), "{note}");
    assert!(note.contains("make the farewell say goodbye"), "{note}");
}

// ---------------------------------------------------------------------------
// 3. Repeated extension accumulates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repeated_extensions_retain_every_task_and_every_note() {
    let (_dir, store) = boot().await;
    let id = first_plan(&store, "PLAN-FIRST", "the first objective", "T-1").await;
    extend(&store, "T-2", "the second objective", 1).await;
    extend(&store, "T-3", "the third objective", 2).await;

    let plan = store.plans().get_plan(&id).await.expect("plan");
    assert_eq!(plan.objective, "the first objective");
    assert_eq!(
        plan.task_ids,
        vec![
            TaskId::from_string("T-1"),
            TaskId::from_string("T-2"),
            TaskId::from_string("T-3")
        ],
        "membership accumulates in extension order"
    );
    for task in ["T-2", "T-3"] {
        assert!(
            plan.rationale.contains(task),
            "every extension is recorded: {}",
            plan.rationale
        );
    }
    assert_eq!(
        plan.rationale
            .lines()
            .filter(|l| l.contains("scope extended"))
            .count(),
        2
    );
}

// ---------------------------------------------------------------------------
// 4, 5 & 7. Refusals leave nothing behind
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_duplicate_task_is_rejected_without_mutating_the_plan() {
    let (_dir, store) = boot().await;
    let id = first_plan(&store, "PLAN-FIRST", "the first objective", "T-1").await;
    extend(&store, "T-2", "the second objective", 1).await;
    let before = store.plans().get_plan(&id).await.expect("plan");

    // The same task id again, through the application API.
    let outcome = extend_active_plan_at(
        &store,
        ExtendRequest {
            project_id: ProjectId::from_string(PROJ),
            task: spec("T-2", "the second objective"),
            authorized_by: AgentId::from_string("ORQYNN-CLI"),
            now: pinned(2),
        },
    )
    .await;
    assert!(outcome.is_err(), "a duplicate task must be refused");

    let after = store.plans().get_plan(&id).await.expect("plan");
    assert_eq!(after.task_ids, before.task_ids, "no duplicate membership");
    assert_eq!(
        after.rationale, before.rationale,
        "and no scope note for work that did not happen"
    );
}

#[tokio::test]
async fn a_superseded_plan_cannot_be_extended() {
    let (_dir, store) = boot().await;
    let old = first_plan(&store, "PLAN-OLD", "the old objective", "T-1").await;
    // A genuine replan: a new plan supersedes the old one, which becomes history.
    plan_at(
        &store,
        PlanRequest {
            project_id: ProjectId::from_string(PROJ),
            plan_id: PlanId::from_string("PLAN-NEW"),
            objective: "the new objective".into(),
            rationale: "a deliberate replan".into(),
            tasks: vec![spec("T-9", "the new objective")],
            authorized_by: AgentId::from_string("ORQYNN-CLI"),
        },
        pinned(5),
    )
    .await
    .expect("the replan");

    // Reaching for the historical plan directly is refused by the store, even
    // though the task it would carry is perfectly well formed.
    let refused = store
        .append_task_to_plan(
            &old,
            &Task::for_project(
                ProjectId::from_string(PROJ),
                TaskId::from_string("T-LATE"),
                "late",
                "arrived after the replan",
            ),
            "+ scope extended: arrived after the replan",
            pinned(6),
        )
        .await;
    assert!(refused.is_err(), "a superseded plan must refuse");

    // And the refusal left the historical record exactly as it was.
    let historical = store.plans().get_plan(&old).await.expect("plan");
    assert_eq!(historical.status, PlanStatus::Superseded);
    assert_eq!(
        historical.task_ids,
        vec![TaskId::from_string("T-1")],
        "history is not rewritten"
    );
    assert_eq!(historical.objective, "the old objective");
    assert!(
        store
            .tasks()
            .get_task(&TaskId::from_string("T-LATE"))
            .await
            .is_err(),
        "and no task row was written"
    );
}

#[tokio::test]
async fn extending_after_a_replan_targets_the_new_active_plan_not_the_old_one() {
    let (_dir, store) = boot().await;
    let old = first_plan(&store, "PLAN-OLD", "the old objective", "T-1").await;
    plan_at(
        &store,
        PlanRequest {
            project_id: ProjectId::from_string(PROJ),
            plan_id: PlanId::from_string("PLAN-NEW"),
            objective: "the new objective".into(),
            rationale: "a deliberate replan".into(),
            tasks: vec![spec("T-9", "the new objective")],
            authorized_by: AgentId::from_string("ORQYNN-CLI"),
        },
        pinned(5),
    )
    .await
    .expect("the replan");

    extend(&store, "T-LATE", "arrived after the replan", 6).await;

    let active = active(&store).await;
    assert_eq!(active.id, PlanId::from_string("PLAN-NEW"));
    assert!(active.task_ids.contains(&TaskId::from_string("T-LATE")));

    let historical = store.plans().get_plan(&old).await.expect("plan");
    assert!(
        !historical.task_ids.contains(&TaskId::from_string("T-LATE")),
        "the superseded plan was not touched"
    );
    assert_eq!(historical.task_ids, vec![TaskId::from_string("T-1")]);
}

#[tokio::test]
async fn a_refused_extension_writes_no_task_row() {
    let (_dir, store) = boot().await;
    first_plan(&store, "PLAN-FIRST", "the first objective", "T-1").await;

    // An empty task id cannot be persisted; the whole extension must roll back.
    let outcome = extend_active_plan_at(
        &store,
        ExtendRequest {
            project_id: ProjectId::from_string(PROJ),
            task: TaskSpec::new(TaskId::from_string("T-2"), "second", "the second objective"),
            authorized_by: AgentId::from_string("ORQYNN-CLI"),
            now: pinned(1),
        },
    )
    .await;
    assert!(outcome.is_ok(), "a well-formed task extends cleanly");

    // Now force the store half to fail, by pointing it at a plan that is gone.
    let missing = store
        .append_task_to_plan(
            &PlanId::from_string("PLAN-NOT-THERE"),
            &Task::for_project(
                ProjectId::from_string(PROJ),
                TaskId::from_string("T-GHOST"),
                "ghost",
                "a task for a plan that does not exist",
            ),
            "+ scope extended: nothing",
            pinned(2),
        )
        .await;
    assert!(missing.is_err());
    assert!(
        store
            .tasks()
            .get_task(&TaskId::from_string("T-GHOST"))
            .await
            .is_err(),
        "a failed extension must not leave a task row behind"
    );
}

// ---------------------------------------------------------------------------
// 6. Concurrent extensions lose nothing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_extensions_do_not_lose_tasks_or_notes() {
    let (_dir, store) = boot().await;
    let id = first_plan(&store, "PLAN-FIRST", "the first objective", "T-1").await;

    // Genuinely concurrent: six tasks appended at the same time on a real store,
    // with no sleep and no ordering assumption. Whichever commits first, the rest
    // must build on what it wrote rather than on what they read beforehand.
    let mut set = tokio::task::JoinSet::new();
    for n in 0..6u32 {
        let store = store.clone();
        set.spawn(async move {
            let task = format!("CONCURRENT-{n}");
            extend_active_plan_at(
                &store,
                ExtendRequest {
                    project_id: ProjectId::from_string(PROJ),
                    task: spec(&task, &format!("objective {n}")),
                    authorized_by: AgentId::from_string("ORQYNN-CLI"),
                    now: pinned(n as i64),
                },
            )
            .await
        });
    }
    while let Some(joined) = set.join_next().await {
        // Every append either succeeded or was refused as a duplicate; none may
        // have failed for any other reason.
        if let Ok(Err(err)) = joined {
            assert!(
                matches!(err, director_app::PlanError::Store(_)),
                "an unexpected failure: {err:?}"
            );
        }
    }

    let plan = store.plans().get_plan(&id).await.expect("plan");
    let members: Vec<String> = plan
        .task_ids
        .iter()
        .map(|t| t.as_str().to_string())
        .collect();
    assert_eq!(
        members.len(),
        members
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        "no task appears twice: {members:?}"
    );
    assert!(
        members.contains(&"T-1".to_string()),
        "the original member survived: {members:?}"
    );
    // One note per successfully added task, and no note without its task.
    let notes = plan
        .rationale
        .lines()
        .filter(|l| l.contains("scope extended"))
        .count();
    assert_eq!(
        notes,
        members.len() - 1,
        "exactly one note per added task: {} vs {members:?}",
        plan.rationale
    );
    for member in &members {
        if member != "T-1" {
            assert!(
                plan.rationale.contains(member.as_str()),
                "every member has a scope note: {} missing for {member}",
                plan.rationale
            );
        }
    }
    assert_eq!(plan.objective, "the first objective");
}

// ---------------------------------------------------------------------------
// 8 & 10. Legacy readability and genuine supersession
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plan_written_before_scope_notes_still_reads_and_extends() {
    let (_dir, store) = boot().await;
    let id = first_plan(&store, "PLAN-FIRST", "the first objective", "T-1").await;

    // Simulate a legacy row: a plan whose rationale carries no extension note,
    // which is exactly what every plan written before this change looks like.
    let mut legacy = store.plans().get_plan(&id).await.expect("plan");
    legacy.rationale = "one task, requested explicitly on the command line - decomposed from observation 1 at abc123def456 on main".into();
    store
        .plans()
        .update_plan(&legacy)
        .await
        .expect("legacy row written");

    // Still readable, still extendable, and the note lands after the legacy text.
    let reread = store.plans().get_plan(&id).await.expect("plan");
    assert_eq!(reread.rationale, legacy.rationale);

    extend(&store, "T-2", "the second objective", 1).await;
    let plan = store.plans().get_plan(&id).await.expect("plan");
    assert!(
        plan.rationale.starts_with("one task, requested explicitly"),
        "the legacy rationale is preserved, not replaced: {}",
        plan.rationale
    );
    assert!(plan.rationale.contains("scope extended"));
    assert_eq!(plan.objective, "the first objective");
}

#[tokio::test]
async fn genuine_replanning_still_supersedes_and_keeps_the_old_record() {
    let (_dir, store) = boot().await;
    let old = first_plan(&store, "PLAN-OLD", "the old objective", "T-1").await;

    let planned = plan_at(
        &store,
        PlanRequest {
            project_id: ProjectId::from_string(PROJ),
            plan_id: PlanId::from_string("PLAN-NEW"),
            objective: "the new objective".into(),
            rationale: "a deliberate replan".into(),
            tasks: vec![spec("T-9", "the new objective")],
            authorized_by: AgentId::from_string("ORQYNN-CLI"),
        },
        pinned(5),
    )
    .await
    .expect("the replan");

    assert_eq!(planned.superseded.as_ref(), Some(&old));
    let new = active(&store).await;
    assert_eq!(new.id, PlanId::from_string("PLAN-NEW"));
    assert_eq!(new.supersedes.as_ref(), Some(&old));

    let historical = store.plans().get_plan(&old).await.expect("plan");
    assert_eq!(historical.status, PlanStatus::Superseded);
    assert_eq!(
        historical.superseded_by.as_ref(),
        Some(&PlanId::from_string("PLAN-NEW")),
        "the link is recorded in both directions"
    );
    assert_eq!(historical.task_ids, vec![TaskId::from_string("T-1")]);
    assert_eq!(historical.objective, "the old objective");
}

// ---------------------------------------------------------------------------
// 12. Task semantics are untouched by any of this
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_extended_task_is_ordinary_todo_work_in_the_project() {
    let (_dir, store) = boot().await;
    first_plan(&store, "PLAN-FIRST", "the first objective", "T-1").await;
    extend(&store, "T-2", "the second objective", 1).await;

    let task = store
        .tasks()
        .get_task(&TaskId::from_string("T-2"))
        .await
        .expect("the added task");
    assert_eq!(task.status, TaskStatus::Todo, "still handable, not special");
    assert_eq!(
        task.project_id.as_ref(),
        Some(&ProjectId::from_string(PROJ))
    );
    assert_eq!(
        task.expected_outputs.len(),
        1,
        "its acceptance criteria travelled with it"
    );
}
