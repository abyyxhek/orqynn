//! Dedicated tests for [`director_app::recovery::RecoveryBuilder`].
//!
//! These exercise the builder end to end against a real SQLite store, with the
//! observed project state supplied directly rather than read from a git
//! repository. That keeps each test about one question — what the builder
//! reconstructs, and what it reports it cannot — and lets the file build and run
//! independently of the loop's other steps: no git service, no observation
//! round, no checkpoint id generator is involved.
//!
//! Every assertion reads back through the store's own repositories, so a test
//! never compares the builder against a value the test itself hand-wrote and
//! then handed to the builder. The one exception is the observed `ProjectState`,
//! which is input to the comparison by construction.
//!
//! ## What is deliberately not tested here
//!
//! [`RecoveryOutcome::NoProject`] is unreachable through the store's own API:
//! `tasks.project_id` is `NOT NULL` and `create_task` rejects an unscoped task.
//! The builder still handles it, because `Task.project_id` is an `Option` in the
//! domain and the builder reasons over the domain's shape rather than over one
//! store's constraint. Writing a fixture for it would mean asserting on a state
//! the store refuses to hold, so the variant is covered by the builder's own
//! docs and by the fact that every other outcome below is reachable.

use director_app::recovery::{recovery_context, RecoveryBuilder, RecoveryOutcome};
use director_domain::action::{Action, ActionKind};
use director_domain::assignment::ReleaseReason;
use director_domain::blocker::{Blocker, BlockerKind, BlockerStatus};
use director_domain::checkpoint::{Checkpoint, ResumeStatus};
use director_domain::ids::{
    ActionId, AgentId, AssignmentId, BlockerId, CheckpointId, DecisionId, MachineId, ProjectId,
    SessionId, TaskId, VerificationId,
};
use director_domain::state::{ChangedFile, CommitInfo, FileChange, ProjectState, StateComparison};
use director_domain::store::StoredProjectState;
use director_domain::task::TaskStatus;
use director_domain::verification::{Evidence, EvidenceStatus, ProbeKind, VerificationStatus};
use director_domain::{
    ActionRepository, Agent, AgentAssignment, AgentRepository, AgentSession, AssignmentRepository,
    BlockerRepository, Capability, CheckpointRepository, Decision, DecisionRepository, Harness,
    Project, ProjectRepository, ProjectStateRepository, SessionRepository, Task, TaskRepository,
    TestResults, Verification, VerificationRepository,
};
use director_store::Store as SqliteStore;

/// The project every fixture builds in.
const PROJ: &str = "PROJ-1";
/// The task most fixtures put in flight.
const TASK: &str = "WORK";

/// A pinned moment `step` minutes after an arbitrary epoch, so two records that
/// are ordered by time are ordered by the test rather than by how fast it ran.
fn pinned(step: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000 + step * 60, 0).expect("a pinned moment")
}

/// A store with a project in it and nothing else.
async fn boot() -> (tempfile::TempDir, SqliteStore) {
    let dir = tempfile::TempDir::new().expect("a temp dir for the store");
    let store = SqliteStore::open(dir.path().join("recovery.db"))
        .await
        .expect("the store opens on a fresh file");
    store
        .projects()
        .create_project(&Project::new(
            ProjectId::from_string(PROJ),
            PROJ,
            "/nowhere",
        ))
        .await
        .expect("project created");
    (dir, store)
}

/// A task in `WORK`, moved to `in_progress` the way the loop's ASSIGN step would
/// leave it. Returns the task as the store wrote it, so a caller that needs the
/// current version reads it from the record rather than from a local guess.
async fn task_in_progress(store: &SqliteStore, id: &str) -> Task {
    let mut task = Task::for_project(
        ProjectId::from_string(PROJ),
        TaskId::from_string(id),
        id,
        "ship the feature",
    );
    task.status = TaskStatus::Todo;
    let stored = store
        .tasks()
        .create_task(&task)
        .await
        .expect("the task is created");

    let mut task = stored;
    task.status = TaskStatus::InProgress;
    store
        .tasks()
        .update_task(&task)
        .await
        .expect("the task moves to in_progress")
}

/// Move a task to `status`, returning the task as the store wrote it. The store
/// bumps the version and records the transition; both are what the recovery
/// reads back, so a caller never mutates a task it then trusts.
async fn move_task_to(store: &SqliteStore, task: &Task, status: TaskStatus) -> Task {
    let mut task = task.clone();
    task.status = status;
    store
        .tasks()
        .update_task(&task)
        .await
        .expect("the task moved")
}

/// A checkpoint of `task` taken at `commit` on `main`, recording the versions a
/// resume reconciles against. `test_results` are left to the caller: the
/// automatic path never sets them, because test outcomes come from verification
/// and not from observation, and the conflict tests below depend on that.
fn checkpoint(id: &str, task: &Task, commit: &str) -> Checkpoint {
    let mut cp = Checkpoint::new(
        CheckpointId::from_string(id),
        task.id.clone(),
        &task.objective,
        "auth endpoints scaffolded; middleware not yet wired",
        "the next step",
    );
    cp.branch = Some("main".into());
    cp.commit_sha = (!commit.is_empty()).then(|| commit.to_string());
    cp.project_state_version = Some(1);
    cp.task_state_version = Some(task.state_version);
    cp
}

/// Persist a checkpoint as the current one for its task, returning the stored
/// record. `expected` is the version gate the store checks inside its
/// transaction; `None` for a task's first checkpoint.
async fn write_checkpoint(
    store: &SqliteStore,
    cp: &Checkpoint,
    expected: Option<u64>,
) -> Checkpoint {
    store
        .checkpoints()
        .create_checkpoint(cp, expected)
        .await
        .expect("the checkpoint lands")
}

/// Record Orqyn's normalized belief about the project's state.
async fn record_state(store: &SqliteStore, commit: &str, branch: Option<&str>, clean: bool) {
    store
        .project_state()
        .update_project_state(&StoredProjectState {
            project_id: ProjectId::from_string(PROJ),
            repository_id: None,
            branch: branch.map(str::to_string),
            head_commit: commit.into(),
            working_tree_clean: clean,
            observation_version: 1,
            last_observed_at: chrono::Utc::now(),
            state_version: 1,
        })
        .await
        .expect("project state recorded");
}

/// A freshly observed project state: `commit` at `branch`, dirty when `dirty`.
/// This is the input to [`Checkpoint::compare_with`], so it is built here rather
/// than read from anywhere.
fn observed(commit: &str, branch: &str, dirty: bool) -> ProjectState {
    ProjectState {
        branch: Some(branch.into()),
        head_commit: commit.into(),
        working_tree: if dirty {
            vec![ChangedFile {
                path: "src/middleware.rs".into(),
                change: FileChange::Modified,
            }]
        } else {
            Vec::new()
        },
        recent_commits: Vec::new(),
        test_results: None,
        observed_on: MachineId::from_string("MACH-a"),
        observed_at: chrono::Utc::now(),
    }
}

/// A commit history, oldest first — the shape a git observation hands the
/// comparison. Only the shas matter to it.
fn history(shas: &[&str]) -> Vec<CommitInfo> {
    shas.iter()
        .map(|sha| CommitInfo {
            sha: sha.to_string(),
            summary: "a commit".into(),
            author: "an author".into(),
            committed_at: pinned(0),
            parents: Vec::new(),
            committer: "a committer".into(),
            message: "a commit".into(),
        })
        .collect()
}

/// Register an agent Orqyn knows about. `id` is the full agent id — it is the
/// identifier the tenure history and the package's presence list both carry, so
/// a caller that wants to find it again passes what it expects to read back.
async fn register_agent(store: &SqliteStore, id: &str) {
    store
        .agents()
        .register_agent(&Agent::register(
            AgentId::from_string(id),
            id,
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            vec![Capability::Coding],
        ))
        .await
        .expect("agent registered");
}

/// Record one tenure on the task: active when `ended` is `None`, released with
/// that reason when it is not. Pinned to `at` so the history's order is the
/// test's, not the clock's. `agent` is the full agent id, as `register_agent`
/// takes it.
async fn tenure(
    store: &SqliteStore,
    agent: &str,
    task: &TaskId,
    ended: Option<ReleaseReason>,
    at: chrono::DateTime<chrono::Utc>,
) {
    let agent_id = AgentId::from_string(agent);
    let session_id = SessionId::from_string(format!("SESS-{agent}"));

    // The assignment carries the session doing the work, and that session row
    // has to exist before the assignment can reference it.
    store
        .sessions()
        .create_session(&AgentSession::start(
            session_id.clone(),
            Some(ProjectId::from_string(PROJ)),
            agent_id.clone(),
            MachineId::from_string("MACH-a"),
            Some(task.clone()),
        ))
        .await
        .expect("session recorded");

    let mut assignment = AgentAssignment::propose(
        AssignmentId::from_string(format!("ASG-{agent}")),
        task.clone(),
        agent_id,
    );
    assignment.assigned_at = at;
    assignment.activate(session_id);
    if let Some(reason) = ended {
        assignment.release(reason);
        assignment.released_at = Some(at + chrono::Duration::minutes(1));
    }
    store
        .assignments()
        .create_assignment(&assignment)
        .await
        .expect("tenure recorded");
}

/// Record a failed verification with its evidence, so the replacement can see
/// what already went wrong.
async fn verdict(store: &SqliteStore, task: &TaskId) -> Verification {
    store
        .verifications()
        .create_verification(&Verification::new(
            VerificationId::from_string("VER-1"),
            task.clone(),
            ProjectId::from_string(PROJ),
            None,
            VerificationStatus::Failed,
            vec![Evidence {
                kind: ProbeKind::Command,
                criterion: "the check passes".into(),
                status: EvidenceStatus::Failed,
                detail: "exit 1: git --no-such-flag".into(),
            }],
            None,
            pinned(3),
        ))
        .await
        .expect("verification recorded")
}

/// Record a decision the replacement must honor.
async fn decision(store: &SqliteStore, task: &TaskId) -> Decision {
    store
        .decisions()
        .create_decision(
            &Decision::new(
                DecisionId::from_string("DEC-1"),
                "checkpoint ids must survive a restart",
                "a re-minted id collides with the lineage it belongs to",
            )
            .in_task(task.clone()),
        )
        .await
        .expect("decision recorded")
}

// ---------------------------------------------------------------------------
// The four ways a build can produce nothing, and the one way it cannot.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_task_orqyn_never_recorded_reports_unknown_task() {
    let (_dir, store) = boot().await;

    let outcome = RecoveryBuilder::new(&store, TaskId::from_string("NOPE"))
        .build()
        .await
        .expect("the build reads the store");

    assert!(
        matches!(outcome, RecoveryOutcome::UnknownTask(ref id) if id.as_str() == "NOPE"),
        "an id Orqyn has never written is the caller's question, not the work's",
    );
    assert!(outcome.is_absent());
    assert_eq!(
        outcome.absence_reason().unwrap(),
        "Orqyn has no record of task NOPE",
    );
    assert!(
        outcome.package().is_none(),
        "`None` is the single-question answer SCHEDULE gets",
    );
}

#[tokio::test]
async fn a_task_with_no_checkpoint_reports_no_checkpoint() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;

    // The observed path, not the stored one. The stored path resolves the
    // project's state before it looks for a checkpoint, and this project has
    // never been observed — so on that path the honest outcome is
    // `NoStoredProjectState`, and `NoCheckpoint` would never be reached. The
    // fresh path takes the observation as given and goes straight to the
    // checkpoint lookup, which is what isolates the question this test asks.
    let outcome = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store");

    // The task exists and is in flight; what is missing is the narrative a
    // resumption continues from. That is a data question, not an error.
    assert!(
        matches!(outcome, RecoveryOutcome::NoCheckpoint(ref id) if *id == task.id),
        "an uncheckpointed task has history but nothing to resume",
    );
    assert!(outcome.absence_reason().unwrap().contains("no checkpoint"));
}

#[tokio::test]
async fn a_task_with_no_checkpoint_and_no_observation_reports_the_missing_state() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;

    // The same task on the stored path, where the absent observation is the
    // first thing the build finds. This is the ordering dependency made
    // explicit rather than left implicit in the test above: both absences are
    // real, and which one a build reports depends on which it meets first.
    let outcome = RecoveryBuilder::new(&store, task.id.clone())
        .build()
        .await
        .expect("the build reads the store");

    assert!(
        matches!(outcome, RecoveryOutcome::NoStoredProjectState(ref id) if *id == task.id),
        "the unobserved project is reported before the missing checkpoint",
    );
}

#[tokio::test]
async fn a_task_whose_project_was_never_observed_reports_no_stored_state() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;

    // The stored-state path resolves the observation before the checkpoint, so
    // a project OBSERVE has never run is reported as the absence it is.
    let outcome = RecoveryBuilder::new(&store, task.id.clone())
        .build()
        .await
        .expect("the build reads the store");

    assert!(
        matches!(outcome, RecoveryOutcome::NoStoredProjectState(ref id) if *id == task.id),
        "no observation means no normalized state to compare against",
    );
    assert!(outcome
        .absence_reason()
        .unwrap()
        .contains("never been observed"));
}

// ---------------------------------------------------------------------------
// The resume verdict, one per comparison outcome. Each uses the same fixture so
// the only thing varying between them is what the observation saw.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_in_flight_task_on_the_checkpointed_commit_builds_ready() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    let cp = write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("an in-flight task with a checkpoint builds");

    assert_eq!(recovery.task(), &task.id);
    assert_eq!(recovery.resume_status(), ResumeStatus::Ready);
    assert_eq!(recovery.package.comparison, StateComparison::Unchanged);
    // The world the checkpoint describes is still the world, so its own next
    // step is the instruction — not something the recovery invented.
    assert_eq!(recovery.next_action(), cp.next_action);
}

/// The scope a replacement is handed must be the task's own persisted
/// declaration — read from the task row, not inferred from the checkpoint, the
/// observed files, or anything an agent said. An agent that is told the wrong
/// ground edits files another agent owns.
#[tokio::test]
async fn a_recovery_package_carries_the_task_s_scope_exactly_as_declared() {
    let (_dir, store) = boot().await;

    let mut task = Task::for_project(
        ProjectId::from_string(PROJ),
        TaskId::from_string(TASK),
        TASK,
        "ship the feature",
    );
    task.status = TaskStatus::Todo;
    // Deliberately not normalized, and deliberately out of any obvious order:
    // the package reports the declaration verbatim and leaves comparison
    // semantics to `Task::scope_overlap`.
    task.scope_paths = vec![
        "src\\auth".to_string(),
        "./src/auth/login.rs".to_string(),
        "docs/".to_string(),
    ];
    let task = store
        .tasks()
        .create_task(&task)
        .await
        .expect("the task is created");
    let task = move_task_to(&store, &task, TaskStatus::InProgress).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    // The authoritative scope, re-read from the store rather than reused from the
    // local value — so this compares the package against what is persisted.
    let authoritative = store
        .tasks()
        .get_task(&task.id)
        .await
        .expect("the task reads")
        .scope_paths;

    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("an in-flight task with a checkpoint builds");

    assert_eq!(
        recovery.package.scope_paths, authoritative,
        "the briefing's scope is the task's persisted declaration, byte for byte"
    );
    assert_eq!(
        recovery.package.scope_paths.len(),
        3,
        "no path is dropped and none is invented"
    );
    assert_eq!(
        recovery.package.scope_paths[0], "src\\auth",
        "declaration order is preserved, and Windows separators are not rewritten"
    );
}

/// A task that declared no scope must not acquire one on the way into a briefing.
/// Empty here is *undeclared*, which conflicts with everything, so silently
/// filling it in — or reporting a task's files as if they were its scope — would
/// make the briefing actively misleading.
#[tokio::test]
async fn a_task_with_no_declared_scope_briefs_as_undeclared_rather_than_empty_by_accident() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("an in-flight task with a checkpoint builds");

    assert!(
        recovery.package.scope_paths.is_empty(),
        "an undeclared scope stays undeclared; it is not back-filled from the \
         checkpoint's changed files or the observed working tree"
    );
}

#[tokio::test]
async fn a_dirty_tree_on_the_same_commit_is_stale() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    let _ = write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", true), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    // Same commit, but work happened since, so `changed_files` is stale.
    assert_eq!(
        recovery.package.comparison,
        StateComparison::WorkingTreeChanged
    );
    assert_eq!(recovery.resume_status(), ResumeStatus::Stale);
    assert!(
        recovery.next_action().contains("reconcile"),
        "a stale task is told to reconcile rather than trust the narrative",
    );
    assert!(
        recovery.next_action().contains("the next step"),
        "the checkpoint's step is referenced, never re-issued as an instruction",
    );
}

#[tokio::test]
async fn an_advanced_head_is_stale() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    let _ = write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    // Someone committed since; the checkpoint's commit is still reachable, so
    // this is an advance rather than a rebase.
    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("def", "main", false), history(&["def", "abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    assert_eq!(recovery.package.comparison, StateComparison::HeadAdvanced);
    assert_eq!(recovery.resume_status(), ResumeStatus::Stale);
}

#[tokio::test]
async fn a_rebased_away_commit_is_stale() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    let _ = write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    // The commit the checkpoint pinned no longer exists in history: a rebase or
    // amend invalidated its frame of reference.
    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("def", "main", false), history(&["def", "xyz"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    assert_eq!(recovery.package.comparison, StateComparison::CommitGone);
    assert_eq!(recovery.resume_status(), ResumeStatus::Stale);
}

#[tokio::test]
async fn a_branch_change_is_stale() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    let _ = write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(
            observed("def", "feature/auth", false),
            history(&["def", "abc"]),
        )
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    assert_eq!(recovery.package.comparison, StateComparison::BranchChanged);
    assert_eq!(recovery.resume_status(), ResumeStatus::Stale);
}

// ---------------------------------------------------------------------------
// The verdict's precedence over the comparison.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_blocked_task_reports_blocked_before_staleness() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    let _ = write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;
    let task = move_task_to(&store, &task, TaskStatus::Blocked).await;

    // The tree is clean and the commit is unchanged, so the comparison alone
    // would say Ready. The task's own status is the authoritative signal, and
    // `Blocker` rows are not persisted to say it with.
    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    assert_eq!(recovery.resume_status(), ResumeStatus::Blocked);
    assert!(
        recovery.next_action().contains("clear the blocker"),
        "the instruction names the blocker rather than the next step",
    );
}

#[tokio::test]
async fn a_done_task_with_no_passing_evidence_is_conflicted() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    let cp = checkpoint("CHK-1", &task, "abc");
    assert!(
        cp.test_results.is_none(),
        "a checkpoint the observation writes records no test results",
    );
    let _ = write_checkpoint(&store, &cp, None).await;
    let task = move_task_to(&store, &task, TaskStatus::Done).await;

    // Done is reachable only through verification, and a checkpoint carries no
    // test results to the contrary, so a task that reads done is a state
    // verification never granted it. The replacement is told to adjudicate
    // rather than pick a side.
    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    assert_eq!(recovery.resume_status(), ResumeStatus::Conflicted);
    assert!(recovery.next_action().contains("adjudicate"));
}

#[tokio::test]
async fn a_done_task_whose_checkpoint_records_passing_tests_is_not_conflicted() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    let mut cp = checkpoint("CHK-1", &task, "abc");
    cp.test_results = Some(TestResults {
        passed: 10,
        failed: 0,
        skipped: 0,
        failures: Vec::new(),
        run_at: pinned(2),
    });
    let _ = write_checkpoint(&store, &cp, None).await;
    let task = move_task_to(&store, &task, TaskStatus::Done).await;

    // Passing evidence on record means done and evidence agree; the verdict is
    // Insufficient because a finished task has nothing to resume, not because
    // anything disagrees.
    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    assert_eq!(recovery.resume_status(), ResumeStatus::Insufficient);
    assert!(recovery.next_action().contains("nothing to resume"));
}

// ---------------------------------------------------------------------------
// What the build reconstructs, and what it reports it cannot.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_package_carries_the_persisted_histories() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    register_agent(&store, "AGENT-1").await;
    register_agent(&store, "AGENT-2").await;
    tenure(
        &store,
        "AGENT-1",
        &task.id,
        Some(ReleaseReason::WorkComplete),
        pinned(1),
    )
    .await;
    tenure(&store, "AGENT-2", &task.id, None, pinned(2)).await;
    let _ = verdict(&store, &task.id).await;
    let _ = decision(&store, &task.id).await;
    let _ = write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    // Each history is exactly what the store answers for the same question, so
    // a driver that asks either way gets the same picture.
    assert_eq!(
        recovery.package.tenures,
        store
            .assignments()
            .assignment_history(&task.id)
            .await
            .expect("the store answers"),
        "the tenure history is what answers 'what happened to the previous agent'",
    );
    assert_eq!(
        recovery.package.verifications,
        store
            .verifications()
            .verifications_for_task(&task.id)
            .await
            .expect("the store answers"),
    );
    assert_eq!(
        recovery.package.transitions,
        store
            .tasks()
            .task_history(&task.id)
            .await
            .expect("the store answers"),
    );
    assert_eq!(
        recovery.package.decisions,
        store
            .decisions()
            .decisions_for_task(&task.id)
            .await
            .expect("the store answers"),
    );

    // The prior tenure is retained and says how it ended — the answer to the
    // question a replacement actually asks first.
    let prior = &recovery.package.tenures[0];
    assert_eq!(prior.agent_id, AgentId::from_string("AGENT-1"));
    assert_eq!(prior.release_reason, Some(ReleaseReason::WorkComplete));

    // The registry's presence question, answered from the registry.
    assert_eq!(recovery.package.active_agents.len(), 2);
}

#[tokio::test]
async fn without_histories_builds_a_verdict_with_empty_history_vectors() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    register_agent(&store, "AGENT-1").await;
    tenure(
        &store,
        "AGENT-1",
        &task.id,
        Some(ReleaseReason::WorkComplete),
        pinned(1),
    )
    .await;
    let _ = verdict(&store, &task.id).await;
    let _ = write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .without_histories()
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    // The verdict depends on the task and the checkpoint, neither of which is a
    // history collection, so skipping them changes nothing about it.
    assert_eq!(recovery.resume_status(), ResumeStatus::Ready);
    assert!(recovery.package.tenures.is_empty());
    assert!(recovery.package.verifications.is_empty());
    assert!(recovery.package.transitions.is_empty());
    assert!(recovery.package.decisions.is_empty());
    // The registry is not a history collection, so presence is still answered.
    assert_eq!(recovery.package.active_agents.len(), 1);
    // Empty by request is not reported as a gap: the store could have answered,
    // the caller did not ask. Only the structural limits are.
    assert!(!recovery.gaps().is_clean());
}

#[tokio::test]
async fn unpersisted_context_is_reported_as_unavailable_not_as_empty() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    let _ = write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    // The action window is empty for this task, and is reported by name so an
    // empty field is never read as "the agent did nothing". Blockers are
    // different: they are persisted, so an empty blocker list is an *answer*,
    // not a gap — and saying otherwise would tell a caller Orqyn cannot know,
    // when in fact it can.
    assert!(recovery.gaps().recent_context_unavailable);
    assert!(!recovery.gaps().blockers_unavailable);
    assert!(!recovery.actions_available());
    assert!(!recovery.gaps().is_clean());
    assert!(
        recovery.package.recent_context.is_empty(),
        "the window is empty because nothing was recorded, and is reported as such",
    );
    assert_eq!(recovery.package.recent_context.total_recorded, 0);
    assert!(recovery.package.blockers.is_empty());
    // The task is ready despite both gaps: availability of context and
    // resumability are different questions.
    assert_eq!(recovery.resume_status(), ResumeStatus::Ready);
}

#[tokio::test]
async fn the_package_reports_the_task_version_it_was_built_against() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    let _ = write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    // A replacement presents this version when it writes back; a store that
    // disagrees refuses rather than clobbering. It has to be the version the
    // store holds when the package is read, or the first write-back fails.
    let read = store
        .tasks()
        .get_task(&task.id)
        .await
        .expect("the task reads back");
    assert_eq!(
        recovery.package.task_state_version, read.state_version,
        "the version in the package is the version in the store",
    );
}

// ---------------------------------------------------------------------------
// The two observation sources, and the wrapper SCHEDULE keeps using.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_stored_state_path_builds_a_conservative_verdict() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;
    let _ = write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;
    record_state(&store, "abc", Some("main"), true).await;

    // The store's state carries no working tree and no commit history, so a
    // checkpoint taken at the same commit reads unchanged and stays ready.
    let same = RecoveryBuilder::new(&store, task.id.clone())
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");
    assert_eq!(same.package.comparison, StateComparison::Unchanged);
    assert_eq!(same.resume_status(), ResumeStatus::Ready);

    // ...and anything else errs stale, which is the safe direction: with no
    // history to consult, the comparison cannot see that the old commit is
    // still reachable, so it reports the checkpoint's frame of reference as
    // gone rather than assuming it moved.
    record_state(&store, "def", Some("main"), true).await;
    let moved = RecoveryBuilder::new(&store, task.id.clone())
        .from_stored_state()
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");
    assert_eq!(moved.package.comparison, StateComparison::CommitGone);
    assert_eq!(moved.resume_status(), ResumeStatus::Stale);
}

#[tokio::test]
async fn the_recovery_context_wrapper_still_answers_a_single_question() {
    let (_dir, store) = boot().await;
    let task = task_in_progress(&store, TASK).await;

    // No checkpoint: nothing to hand a replacement, reported as `None` the way
    // SCHEDULE expects.
    assert!(
        recovery_context(
            &store,
            &task.id,
            &observed("abc", "main", false),
            &history(&["abc"])
        )
        .await
        .expect("the build reads the store")
        .is_none(),
        "a task with no checkpoint yields no briefing",
    );

    let _ = write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;
    let recovery = recovery_context(
        &store,
        &task.id,
        &observed("abc", "main", false),
        &history(&["abc"]),
    )
    .await
    .expect("the build reads the store")
    .expect("an in-flight task with a checkpoint builds");

    // The wrapper is the same assembly path, so it reports the same gaps.
    assert_eq!(recovery.resume_status(), ResumeStatus::Ready);
    assert!(!recovery.actions_available());
}

// ---------------------------------------------------------------------------
// Durable recovery context: actions and blockers
// ---------------------------------------------------------------------------
//
// The two things a briefing could not reconstruct before, because nothing
// recorded them. Both are read back from the store rather than inferred, and
// the durability tests close the store and reopen it so nothing can pass on
// in-memory state left behind by the writer.

/// A store, and the path it lives at, so a test can drop the handle and reopen.
async fn boot_at() -> (tempfile::TempDir, std::path::PathBuf, SqliteStore) {
    let dir = tempfile::TempDir::new().expect("a temp dir for the store");
    let path = dir.path().join("recovery.db");
    let store = SqliteStore::open(&path)
        .await
        .expect("the store opens on a fresh file");
    store
        .projects()
        .create_project(&Project::new(
            ProjectId::from_string(PROJ),
            PROJ,
            "/nowhere",
        ))
        .await
        .expect("project created");
    (dir, path, store)
}

/// Record one action against `task`, as a running loop would.
async fn record_action(store: &SqliteStore, task: &TaskId, id: &str, summary: &str) {
    let mut action = Action::new(ActionId::from_string(id), ActionKind::Command, summary);
    action.task_id = Some(task.clone());
    store
        .actions()
        .record_action(&action)
        .await
        .expect("the action lands");
}

/// Record one blocker against `task`.
async fn record_blocker(
    store: &SqliteStore,
    task: &TaskId,
    id: &str,
    description: &str,
) -> Blocker {
    let blocker = Blocker::raise(
        BlockerId::from_string(id),
        task.clone(),
        BlockerKind::External,
        description,
    );
    store
        .blockers()
        .create_blocker(&blocker)
        .await
        .expect("the blocker lands")
}

/// Recovery reconstructs the action window from durable records, and only after
/// a real close-and-reopen — so nothing here can be served from the writer's
/// memory.
#[tokio::test]
async fn recovery_reconstructs_recent_actions_after_the_store_is_reopened() {
    let (_dir, path, store) = boot_at().await;
    let task = task_in_progress(&store, TASK).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    for (id, summary) in [
        ("ACT-1", "ran the unit tests"),
        ("ACT-2", "edited login.rs"),
    ] {
        record_action(&store, &task.id, id, summary).await;
    }

    // Close the writer entirely and open a fresh handle on the same file.
    drop(store);
    let reopened = SqliteStore::open(&path).await.expect("the store reopens");

    let recovery = RecoveryBuilder::new(&reopened, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("an in-flight task with a checkpoint builds");

    let summaries: Vec<&str> = recovery
        .package
        .recent_context
        .actions
        .iter()
        .map(|action| action.summary.as_str())
        .collect();
    assert_eq!(
        summaries,
        vec!["ran the unit tests", "edited login.rs"],
        "the window is reconstructed from durable records, oldest first"
    );
    assert_eq!(recovery.package.recent_context.total_recorded, 2);
    assert!(
        recovery.actions_available(),
        "a window with recorded actions is a real record, not an absence"
    );
    assert!(!recovery.gaps().recent_context_unavailable);
}

/// Blockers survive a restart, and the package carries the records rather than
/// only ids — ids cannot answer "what is blocking, and what would clear it".
#[tokio::test]
async fn blockers_survive_a_restart_and_reach_the_briefing() {
    let (_dir, path, store) = boot_at().await;
    let task = task_in_progress(&store, TASK).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    let blocker = record_blocker(&store, &task.id, "BLK-1", "waiting on an API key").await;
    assert!(blocker.status.is_blocking());

    drop(store);
    let reopened = SqliteStore::open(&path).await.expect("the store reopens");

    let recovery = RecoveryBuilder::new(&reopened, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    assert_eq!(
        recovery.package.blockers,
        vec![BlockerId::from_string("BLK-1")]
    );
    assert_eq!(recovery.package.blocker_details.len(), 1);
    let restored = &recovery.package.blocker_details[0];
    assert_eq!(restored.description, "waiting on an API key");
    assert_eq!(restored.kind, BlockerKind::External);
    assert!(restored.status.is_blocking());
    assert!(
        !recovery.gaps().blockers_unavailable,
        "blockers are persisted, so an empty-or-not list is an answer rather than a gap"
    );
}

/// Resolution history is preserved, and "we fixed it" stays distinguishable from
/// "we stopped trying". A cleared blocker is retained, not deleted.
#[tokio::test]
async fn blocker_status_changes_and_resolution_history_are_preserved() {
    let (_dir, path, store) = boot_at().await;
    let task = task_in_progress(&store, TASK).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    let mut blocker = record_blocker(&store, &task.id, "BLK-1", "waiting on review").await;
    blocker = blocker.resolvable_by("a reviewer approves");
    store
        .blockers()
        .update_blocker(&blocker)
        .await
        .expect("the resolution is recorded before the blocker clears");

    blocker.resolve();
    store
        .blockers()
        .update_blocker(&blocker)
        .await
        .expect("the blocker clears");

    drop(store);
    let reopened = SqliteStore::open(&path).await.expect("the store reopens");

    let stored = reopened
        .blockers()
        .blockers_for_task(&task.id)
        .await
        .expect("the blocker history reads");
    assert_eq!(
        stored.len(),
        1,
        "a cleared blocker is retained, not deleted"
    );
    let restored = &stored[0];
    assert!(!restored.status.is_blocking(), "it no longer blocks");
    assert_eq!(
        restored.resolution.as_deref(),
        Some("a reviewer approves"),
        "what would have cleared it is still on record"
    );
    assert!(
        restored.resolved_at.is_some(),
        "when it cleared survived the restart"
    );
    assert_eq!(
        restored.raised_at.timestamp(),
        blocker.raised_at.timestamp(),
        "when it was raised is not rewritten by a later update"
    );

    // And a replacement reading the briefing sees the history, not just the id.
    let recovery = RecoveryBuilder::new(&reopened, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");
    assert_eq!(
        recovery.package.blocker_details[0].resolution.as_deref(),
        Some("a reviewer approves")
    );
}

/// Both open and cleared blockers reach the briefing, oldest first — the list a
/// replacement needs is "what has ever stopped this", not "what is stopping it".
#[tokio::test]
async fn the_briefing_carries_both_open_and_cleared_blockers_in_order() {
    let (_dir, _path, store) = boot_at().await;
    let task = task_in_progress(&store, TASK).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    let mut first = record_blocker(&store, &task.id, "BLK-1", "first").await;
    record_blocker(&store, &task.id, "BLK-2", "second").await;
    first.resolve();
    store
        .blockers()
        .update_blocker(&first)
        .await
        .expect("the first clears");

    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    let ids: Vec<String> = recovery
        .package
        .blocker_details
        .iter()
        .map(|b| b.id.to_string())
        .collect();
    assert_eq!(ids, vec!["BLK-1", "BLK-2"], "oldest first");
    assert!(
        !recovery.package.blocker_details[0].status.is_blocking(),
        "the cleared one is still reported, marked as cleared"
    );
    assert!(recovery.package.blocker_details[1].status.is_blocking());
}

/// Missing history is reported as missing, and never invented. A task with
/// nothing on record gets an empty window and an explicit gap — not a
/// reconstruction guessed from status changes or changed files.
#[tokio::test]
async fn missing_history_is_reported_honestly_and_never_invented() {
    let (_dir, _path, store) = boot_at().await;
    let task = task_in_progress(&store, TASK).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    // The repository genuinely moved between the checkpoint and now, and the
    // working tree has a change — the tempting raw material for a fabricated
    // action window. None of it may be used.
    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(
            observed("def456", "main", true),
            history(&["abc", "def456"]),
        )
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    assert!(
        recovery.package.recent_context.is_empty(),
        "no actions were recorded, so none are reconstructed from commits or file changes"
    );
    assert_eq!(recovery.package.recent_context.total_recorded, 0);
    assert!(
        recovery.gaps().recent_context_unavailable,
        "the absence is named rather than presented as a complete history"
    );
    assert!(!recovery.actions_available());
    assert!(
        recovery.package.blocker_details.is_empty(),
        "and no blocker is invented either"
    );
    assert_eq!(recovery.package.blockers, Vec::<BlockerId>::new());
}

/// The window stays bounded no matter how much is on record: the store holds the
/// log, `RecentContext` enforces the limit the briefing hands over.
#[tokio::test]
async fn the_reconstructed_window_stays_bounded_however_much_is_recorded() {
    let (_dir, _path, store) = boot_at().await;
    let task = task_in_progress(&store, TASK).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    for n in 0..80 {
        record_action(&store, &task.id, &format!("ACT-{n}"), &format!("step {n}")).await;
    }
    // The log itself is complete.
    assert_eq!(
        store
            .actions()
            .actions_for_task(&task.id)
            .await
            .expect("the log reads")
            .len(),
        80,
        "the durable log is not truncated"
    );

    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    let window = &recovery.package.recent_context;
    assert!(
        window.len() <= window.max_actions,
        "the briefing never carries more than the bound"
    );
    assert!(
        window.respects_bound(),
        "and the window respects the invariant the domain states"
    );
    assert_eq!(
        window.total_recorded, 80,
        "compaction is lossy but accountable: the count still tells the truth"
    );
}

/// The earlier continuation packages stay readable, and scope is untouched by
/// any of this.
#[tokio::test]
async fn scope_survives_alongside_the_new_context() {
    let (_dir, _path, store) = boot_at().await;

    let mut scoped = Task::for_project(
        ProjectId::from_string(PROJ),
        TaskId::from_string(TASK),
        TASK,
        "ship it",
    );
    scoped.scope_paths = vec!["src/auth".to_string()];
    let task = store
        .tasks()
        .create_task(&scoped)
        .await
        .expect("the task is created");
    let task = move_task_to(&store, &task, TaskStatus::InProgress).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;
    record_action(&store, &task.id, "ACT-1", "did a thing").await;

    let recovery = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");

    assert_eq!(
        recovery.package.scope_paths,
        vec!["src/auth".to_string()],
        "scope semantics are unchanged by this work"
    );
    assert_eq!(recovery.package.recent_context.actions.len(), 1);

    // A package serialized before `blocker_details` existed still deserializes.
    let mut legacy = serde_json::to_value(&recovery.package).expect("the package serializes");
    legacy
        .as_object_mut()
        .expect("an object")
        .remove("blocker_details");
    let parsed = serde_json::from_value::<director_domain::ContinuationPackage>(legacy)
        .expect("an older package still reads");
    assert!(
        parsed.blocker_details.is_empty(),
        "the new field defaults rather than failing deserialization"
    );
    assert_eq!(
        parsed.scope_paths,
        vec!["src/auth".to_string()],
        "and everything earlier in the payload is intact"
    );
}

// ---------------------------------------------------------------------------
// Blockers decide the resume verdict: Ready must never mean "something is in
// the way". These go through the real store, so they cover the query as well as
// the verdict.
// ---------------------------------------------------------------------------

/// Build a briefing for `task` against a clean, unchanged `abc`.
async fn verdict_for(store: &SqliteStore, task: &TaskId) -> director_app::recovery::Recovery {
    RecoveryBuilder::new(store, task.clone())
        .observed(observed("abc", "main", false), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("an in-flight task with a checkpoint builds")
}

/// An in-flight task on a current checkpoint with no blockers is `Ready` — the
/// baseline the blocker tests below are only meaningful against.
#[tokio::test]
async fn an_unblocked_task_is_still_ready() {
    let (_dir, _path, store) = boot_at().await;
    let task = task_in_progress(&store, TASK).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    let package = verdict_for(&store, &task.id).await;
    assert_eq!(
        package.resume_status(),
        ResumeStatus::Ready,
        "the fix must not block a task that has nothing blocking it"
    );
}

/// 1. A ready task with an open blocker cannot receive a `Ready` verdict.
#[tokio::test]
async fn an_open_blocker_stops_a_ready_verdict() {
    let (_dir, _path, store) = boot_at().await;
    let task = task_in_progress(&store, TASK).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    let blocker = record_blocker(&store, &task.id, "BLK-1", "waiting on an API key").await;
    assert!(
        blocker.status.is_blocking(),
        "the precondition: the blocker really is open"
    );

    let package = verdict_for(&store, &task.id).await;
    assert_eq!(
        package.resume_status(),
        ResumeStatus::Blocked,
        "an open blocker must not be reported as a task that can go"
    );
    // And the instruction has to match the verdict.
    assert!(
        package.package.next_action.contains("blocked"),
        "a blocked verdict that tells the replacement to proceed anyway is worse than no verdict; got: {}",
        package.package.next_action
    );
    // The blocker is still reported, not just the verdict.
    assert_eq!(package.package.blocker_details.len(), 1);
}

/// 2. A cleared blocker alone does not block resumption — in either cleared
/// state, since "we fixed it" and "we stopped trying" are different history but
/// the same answer here.
#[tokio::test]
async fn a_cleared_blocker_alone_does_not_block_resumption() {
    for (label, clear) in [
        ("resolved", BlockerStatus::Resolved),
        ("obsolete", BlockerStatus::Obsolete),
    ] {
        let (_dir, _path, store) = boot_at().await;
        let task = task_in_progress(&store, TASK).await;
        write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

        let mut blocker = record_blocker(&store, &task.id, "BLK-1", "waiting on review").await;
        blocker.status = clear;
        blocker.resolved_at = Some(chrono::Utc::now());
        store
            .blockers()
            .update_blocker(&blocker)
            .await
            .expect("the blocker clears");

        let package = verdict_for(&store, &task.id).await;
        assert_eq!(
            package.resume_status(),
            ResumeStatus::Ready,
            "a {label} blocker is history, not a hold on the work"
        );
        assert_eq!(
            package.package.blocker_details.len(),
            1,
            "and the history is still carried in the briefing"
        );
    }
}

/// 3. An explicitly blocked task remains blocked — the task's own status decides
/// that one, and no blocker row is needed for it to hold.
#[tokio::test]
async fn an_explicitly_blocked_task_remains_blocked() {
    let (_dir, _path, store) = boot_at().await;
    let task = task_in_progress(&store, TASK).await;
    let task = move_task_to(&store, &task, TaskStatus::Blocked).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    // Deliberately no blocker rows: the status alone must decide this.
    let package = verdict_for(&store, &task.id).await;
    assert_eq!(
        package.resume_status(),
        ResumeStatus::Blocked,
        "a blocked task is blocked by its own status"
    );
    assert!(package.package.blocker_details.is_empty());
}

/// 4. Stale and dirty-tree behaviour is untouched by blockers: a blocker is
/// consulted only on the path to `Ready`, so it cannot mask staleness.
#[tokio::test]
async fn staleness_is_unaffected_by_an_open_blocker() {
    let (_dir, _path, store) = boot_at().await;
    let task = task_in_progress(&store, TASK).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;
    record_blocker(&store, &task.id, "BLK-1", "waiting on review").await;

    // A dirty tree on the same commit: stale without the blocker ever being read.
    let package = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("abc", "main", true), history(&["abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");
    assert_eq!(
        package.resume_status(),
        ResumeStatus::Stale,
        "a dirty tree still reads stale; an open blocker must not reorder that"
    );

    // An advanced head, likewise.
    let package = RecoveryBuilder::new(&store, task.id.clone())
        .observed(observed("def", "main", false), history(&["def", "abc"]))
        .build()
        .await
        .expect("the build reads the store")
        .package()
        .expect("built");
    assert_eq!(package.resume_status(), ResumeStatus::Stale);

    // And a task that is not in flight is still insufficient, blocker or not.
    let done = move_task_to(&store, &task, TaskStatus::Cancelled).await;
    let package = verdict_for(&store, &done.id).await;
    assert_eq!(
        package.resume_status(),
        ResumeStatus::Insufficient,
        "a cancelled task has nothing to resume either way"
    );
}

/// 5. Multiple blockers are handled by whether any one is still open, regardless
/// of the order the store returned them in.
#[tokio::test]
async fn multiple_blockers_are_judged_by_whether_any_is_open() {
    let (_dir, _path, store) = boot_at().await;
    let task = task_in_progress(&store, TASK).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;

    // All cleared: nothing in the way.
    for (id, status) in [
        ("BLK-1", BlockerStatus::Resolved),
        ("BLK-2", BlockerStatus::Obsolete),
        ("BLK-3", BlockerStatus::Resolved),
    ] {
        let mut blocker = record_blocker(&store, &task.id, id, "something").await;
        blocker.status = status;
        blocker.resolved_at = Some(chrono::Utc::now());
        store
            .blockers()
            .update_blocker(&blocker)
            .await
            .expect("clears");
    }
    assert_eq!(
        verdict_for(&store, &task.id).await.resume_status(),
        ResumeStatus::Ready
    );

    // One reopened among them is enough.
    let mut reopened = store
        .blockers()
        .get_blocker(&BlockerId::from_string("BLK-2"))
        .await
        .expect("reads back");
    reopened.status = BlockerStatus::Open;
    reopened.resolved_at = None;
    store
        .blockers()
        .update_blocker(&reopened)
        .await
        .expect("reopens");

    let package = verdict_for(&store, &task.id).await;
    assert_eq!(
        package.resume_status(),
        ResumeStatus::Blocked,
        "one open blocker among cleared ones still holds"
    );
    assert_eq!(
        package.package.blocker_details.len(),
        3,
        "all three stay on record — the verdict reads the list, it does not trim it"
    );
}

/// 6. Recovery after reopening the store produces the same verdict: the blocker
/// that decides it is a durable row, not writer memory.
#[tokio::test]
async fn the_verdict_survives_reopening_the_store() {
    let (_dir, path, store) = boot_at().await;
    let task = task_in_progress(&store, TASK).await;
    write_checkpoint(&store, &checkpoint("CHK-1", &task, "abc"), None).await;
    record_blocker(&store, &task.id, "BLK-1", "waiting on an API key").await;

    let before = verdict_for(&store, &task.id).await;
    assert_eq!(before.resume_status(), ResumeStatus::Blocked);

    drop(store);
    let reopened = SqliteStore::open(&path).await.expect("the store reopens");
    let after = verdict_for(&reopened, &task.id).await;

    assert_eq!(
        after.resume_status(),
        before.resume_status(),
        "the verdict is rebuilt from durable rows, so it must not drift across a restart"
    );
    assert_eq!(after.resume_status(), ResumeStatus::Blocked);

    // And clearing it durably changes the verdict on the next read, too.
    let mut blocker = reopened
        .blockers()
        .get_blocker(&BlockerId::from_string("BLK-1"))
        .await
        .expect("reads back");
    blocker.resolve();
    reopened
        .blockers()
        .update_blocker(&blocker)
        .await
        .expect("clears");
    assert_eq!(
        verdict_for(&reopened, &task.id).await.resume_status(),
        ResumeStatus::Ready,
        "clearing a blocker durably releases the verdict"
    );
}
