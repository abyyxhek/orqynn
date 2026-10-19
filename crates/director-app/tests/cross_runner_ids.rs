//! Session and blocker ids must be unique across Runner instances and restarts.
//!
//! Both used to come from the Runner's own `IdGenerator`, whose counter resets
//! with every instance. Two Runners over one store therefore both proposed
//! `SESS-session-1` / `BLK-blocker-1` for *different* records, and because
//! `agent_sessions.id` and `blockers.id` are PRIMARY KEYs, a legitimate write was
//! refused with a bare `UNIQUE constraint failed`.
//!
//! Every test drives the real store, since the failure was a constraint violation
//! and nothing less would have caught it.

use std::path::PathBuf;
use std::process::Command;

use chrono::{DateTime, TimeZone, Utc};
use director_adapters::executor::LocalExecutor;
use director_adapters::git::GitService;
use director_adapters::InMemoryProvider;
use director_app::plan::{PlanRequest, TaskSpec};
use director_app::runner::Runner;
use director_domain::agent::{Agent, Harness};
use director_domain::blocker::{Blocker, BlockerKind, BlockerStatus};
use director_domain::capability::Capability;
use director_domain::ids::{
    AgentId, AssignmentId, BlockerId, MachineId, PlanId, ProjectId, RepositoryId, TaskId,
};
use director_domain::task::ExpectedOutput;
use director_domain::{AgentRepository, BlockerRepository, SessionRepository, TaskRepository};
use director_store::Store;

const PROJECT: &str = "PROJ-1";
const REPO: &str = "REPO-1";
const PASSES: &str = "git --version";

fn pinned() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 20, 14, 0, 0).unwrap()
}

fn later(seconds: i64) -> DateTime<Utc> {
    pinned() + chrono::Duration::seconds(seconds)
}

struct Fixture {
    store_path: PathBuf,
    store: Store,
    git: GitService,
    memory: InMemoryProvider,
    executor: LocalExecutor,
    repo_dir: PathBuf,
    project: ProjectId,
    repository: RepositoryId,
    _tmp: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("a temp dir");
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("repo dir");
        let git_in = || {
            let mut cmd = Command::new("git");
            cmd.current_dir(&repo_dir);
            cmd
        };
        git_in().args(["init", "-q"]).status().expect("git init");
        for (key, value) in [
            ("user.name", "Orqyn Tests"),
            ("user.email", "tests@orqyn.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            git_in()
                .args(["config", key, value])
                .status()
                .expect("config");
        }
        std::fs::write(repo_dir.join("README.md"), "# fixture\n").expect("write");
        git_in().args(["add", "README.md"]).status().expect("add");
        git_in()
            .args(["commit", "-q", "-m", "initial"])
            .status()
            .expect("commit");

        let state_dir = tmp.path().join("git-state");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let store_path = tmp.path().join("orqyn.db");
        let store = Store::open(&store_path).await.expect("store opens");

        Fixture {
            store_path,
            store,
            git: GitService::new(&state_dir).expect("git service"),
            memory: InMemoryProvider::new(),
            executor: LocalExecutor::default(),
            repo_dir,
            project: ProjectId::from_string(PROJECT),
            repository: RepositoryId::from_string(REPO),
            _tmp: tmp,
        }
    }

    fn runner(&self) -> Runner<'_, InMemoryProvider, LocalExecutor> {
        Runner::new(
            &self.store,
            &self.git,
            &self.memory,
            &self.executor,
            &self.repo_dir,
        )
    }

    fn repo_path(&self) -> String {
        self.repo_dir.to_string_lossy().replace('\\', "/")
    }

    async fn prepared(&self, tasks: &[&str]) {
        let mut runner = self.runner();
        runner
            .observe(
                &self.project,
                &self.repository,
                &self.repo_path(),
                later(-300),
            )
            .await
            .expect("the first observation");
        // One agent per task, so no round is starved of somebody to pair with.
        for n in 0..tasks.len() {
            let name = format!("AGENT-{}", n + 1);
            self.store
                .agents()
                .register_agent(&Agent::register(
                    AgentId::from_string(&name),
                    &name,
                    Harness::ClaudeCode,
                    MachineId::from_string("MACH-a"),
                    vec![Capability::Coding],
                ))
                .await
                .expect("agent registered");
        }

        // One plan holding every task: SCHEDULE works from the *active* plan, so
        // separate plans would leave the later rounds with nothing to hand out.
        let specs: Vec<TaskSpec> = tasks
            .iter()
            .map(|task| {
                let mut spec =
                    TaskSpec::new(TaskId::from_string(*task), *task, format!("do {task}"));
                spec.expected_outputs = vec![ExpectedOutput {
                    criterion: format!("{task} answers"),
                    check: Some(PASSES.into()),
                }];
                // Each task owns disjoint ground; an undeclared scope may touch
                // anything and would conflict with every other task.
                spec.scope_paths = vec![format!("src/{}", task.to_lowercase())];
                spec
            })
            .collect();
        runner
            .plan(
                PlanRequest {
                    project_id: self.project.clone(),
                    plan_id: PlanId::from_string("PLAN-1"),
                    objective: "make the fixture verifiable".into(),
                    rationale: "one plan, several tasks".into(),
                    tasks: specs,
                    authorized_by: AgentId::from_string("AGENT-planner"),
                },
                later(-290),
            )
            .await
            .expect("the plan lands");
    }

    /// Every session id on record across `tasks`.
    async fn session_ids(&self, tasks: &[&str]) -> Vec<String> {
        let mut ids = Vec::new();
        for task in tasks {
            let sessions = self
                .store
                .sessions()
                .sessions_for_task(&TaskId::from_string(*task))
                .await
                .expect("sessions read");
            ids.extend(sessions.iter().map(|s| s.id.to_string()));
        }
        ids
    }

    /// Every blocker id on record across `tasks`.
    async fn blocker_ids(&self, tasks: &[&str]) -> Vec<String> {
        let mut ids = Vec::new();
        for task in tasks {
            let blockers: Vec<Blocker> = self
                .store
                .blockers()
                .blockers_for_task(&TaskId::from_string(*task))
                .await
                .expect("blockers read");
            ids.extend(blockers.iter().map(|b| b.id.to_string()));
        }
        ids
    }
}

/// Hand every ready task to an agent, then drive one turn per tenure using a
/// *separate* Runner for each — the shape that collided, because each Runner's
/// generator starts from zero again.
///
/// Returns `(task_id, assignment_id, session_id)` per turn, in the order SCHEDULE
/// paired them.
async fn drive_turns_with_separate_runners(fixture: &Fixture) -> Vec<(String, String, String)> {
    // One round pairs every task with an available agent.
    let mut scheduler = fixture.runner();
    let scheduled = scheduler
        .schedule(&fixture.project, later(-200))
        .await
        .expect("the scheduling round");
    assert!(
        !scheduled.assigned.is_empty(),
        "the round paired nothing: {scheduled:?}"
    );
    let pairings: Vec<(String, String)> = scheduled
        .assigned
        .iter()
        .map(|p| (p.task_id.to_string(), p.assignment_id.to_string()))
        .collect();

    let mut out = Vec::new();
    for (n, (task, assignment_id)) in pairings.iter().enumerate() {
        // A brand new Runner per turn: its generator starts from zero again, which
        // is the whole point of the exercise.
        let mut runner = fixture.runner();
        let turn = runner
            .simulate_agent_turn(
                &AssignmentId::from_string(assignment_id.clone()),
                later(-100 + n as i64),
            )
            .await
            .unwrap_or_else(|err| panic!("{task}'s turn failed: {err:?}"));
        out.push((
            task.clone(),
            assignment_id.clone(),
            turn.session_id
                .expect("a simulated turn records a session")
                .to_string(),
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// SessionId
// ---------------------------------------------------------------------------

/// 1. Separate Runner instances mint distinct session ids.
///
/// Before the fix every one of these runners proposed `SESS-session-1`, and the
/// second write was refused for colliding on the primary key.
#[tokio::test]
async fn separate_runners_mint_distinct_session_ids() {
    let tasks = ["ALPHA", "BETA", "GAMMA"];
    let fx = Fixture::new().await;
    fx.prepared(&tasks).await;

    drive_turns_with_separate_runners(&fx).await;

    let ids = fx.session_ids(&tasks).await;
    assert_eq!(
        ids.len(),
        3,
        "all three turns recorded a session, so none was refused: {ids:?}"
    );
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        ids, unique,
        "two sessions on record share an id, which is the defect in another form"
    );
}

/// 2. Reopening the store does not reintroduce a session collision.
///
/// A session id is derived from the tenure it belongs to, so it has to be stable
/// across the writer going away entirely.
#[tokio::test]
async fn reopening_the_store_does_not_reintroduce_a_session_collision() {
    let tasks = ["ALPHA", "BETA"];
    let fx = Fixture::new().await;
    fx.prepared(&tasks).await;
    drive_turns_with_separate_runners(&fx).await;

    let before = fx.session_ids(&tasks).await;
    assert_eq!(before.len(), 2);

    // A cold runner over a freshly opened handle — a restart, in effect.
    let reopened = Store::open(&fx.store_path)
        .await
        .expect("the store reopens");
    let sessions = reopened
        .sessions()
        .sessions_for_task(&TaskId::from_string("ALPHA"))
        .await
        .expect("sessions read");
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].id.to_string(),
        before[0],
        "the session survived the restart under the same identity"
    );
}

/// 3. One session per tenure is still refused — the rule the id relies on.
///
/// The new id is sound because MONITOR forbids a second session on one
/// assignment. That guarantee must not have been weakened.
#[tokio::test]
async fn a_second_session_on_one_tenure_is_still_refused() {
    let fx = Fixture::new().await;
    fx.prepared(&["ALPHA"]).await;
    fx.register_helper_agent("AGENT-2").await;

    let mut runner = fx.runner();
    let scheduled = runner
        .schedule(&fx.project, later(-200))
        .await
        .expect("the scheduling round");
    let assignment = scheduled
        .assignment_for(&TaskId::from_string("ALPHA"))
        .expect("handed out")
        .clone();

    // First acknowledgment succeeds.
    runner
        .acknowledge(
            &assignment,
            director_domain::ids::SessionId::from_string("SESS-manual-1"),
            later(-100),
        )
        .await
        .expect("the first acknowledgment lands");

    // A second, with a different id, is refused because the tenure already has a
    // session — not because an id collided.
    let err = runner
        .acknowledge(
            &assignment,
            director_domain::ids::SessionId::from_string("SESS-manual-2"),
            later(-90),
        )
        .await
        .expect_err("a second session on one tenure must be refused");
    let rendered = format!("{err}");
    assert!(
        rendered.to_lowercase().contains("already")
            || rendered.to_lowercase().contains("acknowledged"),
        "the refusal is about the existing session, not an id collision: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// BlockerId
// ---------------------------------------------------------------------------

/// 4. Separate Runner instances mint distinct blocker ids.
///
/// Before the fix, two Runners both proposed `BLK-blocker-1`, and the second
/// `create_blocker` failed on the primary key — so the operator's second
/// legitimate blocker was lost.
#[tokio::test]
async fn separate_runners_mint_distinct_blocker_ids() {
    let tasks = ["ALPHA", "BETA", "GAMMA"];
    let fx = Fixture::new().await;
    fx.prepared(&tasks).await;

    // Three separate Runners, one blocker each, against different tasks.
    for (n, task) in tasks.iter().enumerate() {
        let mut runner = fx.runner();
        let raised = runner
            .raise_blocker(
                &TaskId::from_string(*task),
                BlockerKind::External,
                format!("waiting on something for {task}"),
                None,
                later(n as i64),
            )
            .await
            .unwrap_or_else(|err| panic!("{task}'s blocker was refused: {err:?}"));
        assert_eq!(
            raised.status,
            BlockerStatus::Open,
            "the blocker really is open"
        );
    }

    let ids = fx.blocker_ids(&tasks).await;
    assert_eq!(
        ids.len(),
        3,
        "all three blockers landed, so none was refused for a duplicate id: {ids:?}"
    );
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(ids, unique, "two blockers on record share an id");
}

/// 5. Several blockers on one task get distinct ids, and clearing does not free
/// an id for reuse.
///
/// Blockers are never deleted, so the ordinal only ever grows: a cleared blocker
/// keeps its row and its number, which is what stops a later blocker from
/// inheriting an id that history already used.
#[tokio::test]
async fn several_blockers_on_one_task_never_reuse_an_id() {
    let fx = Fixture::new().await;
    fx.prepared(&["ALPHA"]).await;
    let task = TaskId::from_string("ALPHA");

    let mut ids = Vec::new();
    for n in 0..3 {
        // A separate Runner per raise: the shape that collided.
        let mut runner = fx.runner();
        let raised = runner
            .raise_blocker(
                &task,
                BlockerKind::External,
                format!("obstacle {n}"),
                None,
                later(n),
            )
            .await
            .unwrap_or_else(|err| panic!("raise {n} was refused: {err:?}"));
        ids.push(raised.id.to_string());
    }
    assert_eq!(ids.len(), 3, "three distinct blockers landed");

    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(ids, unique, "ids must be distinct within one task");

    // Clear the first one, then raise again: the new blocker must not reuse it.
    let mut runner = fx.runner();
    runner
        .resolve_blocker(&BlockerId::from_string(ids[0].clone()), later(50))
        .await
        .expect("the first blocker clears");

    let mut runner = fx.runner();
    let fourth = runner
        .raise_blocker(
            &task,
            BlockerKind::Unknown,
            "a later obstacle",
            None,
            later(60),
        )
        .await
        .expect("a raise after a clear still lands");
    assert!(
        !ids.contains(&fourth.id.to_string()),
        "a cleared blocker's id was handed out again: {}",
        fourth.id
    );

    // And the history is intact: all four rows, open and cleared alike.
    let history: Vec<Blocker> = fx
        .store
        .blockers()
        .blockers_for_task(&task)
        .await
        .expect("history reads");
    assert_eq!(
        history.len(),
        4,
        "cleared blockers are retained, not deleted"
    );
}

/// 6. Reopening the store does not reintroduce a blocker collision.
#[tokio::test]
async fn reopening_the_store_does_not_reintroduce_a_blocker_collision() {
    let tasks = ["ALPHA", "BETA"];
    let fx = Fixture::new().await;
    fx.prepared(&tasks).await;

    for (n, task) in tasks.iter().enumerate() {
        let mut runner = fx.runner();
        runner
            .raise_blocker(
                &TaskId::from_string(*task),
                BlockerKind::Conflict,
                format!("two sources disagree on {task}"),
                None,
                later(n as i64),
            )
            .await
            .expect("the blocker lands");
    }

    let before = fx.blocker_ids(&tasks).await;
    assert_eq!(before.len(), 2);

    let reopened = Store::open(&fx.store_path)
        .await
        .expect("the store reopens");
    let blockers: Vec<Blocker> = reopened
        .blockers()
        .blockers_for_task(&TaskId::from_string("ALPHA"))
        .await
        .expect("blockers read");
    assert_eq!(blockers.len(), 1);
    assert_eq!(
        blockers[0].id.to_string(),
        before[0],
        "the blocker survived the restart under the same identity"
    );

    // A raise after the restart must still find a free ordinal.
    let mut cold = Runner::new(&reopened, &fx.git, &fx.memory, &fx.executor, &fx.repo_dir);
    cold.raise_blocker(
        &TaskId::from_string("ALPHA"),
        BlockerKind::External,
        "raised after a restart",
        None,
        later(70),
    )
    .await
    .expect("a raise after a restart must not collide with a stored blocker");
}

/// 7. Raising a blocker leaves the task's lifecycle alone.
///
/// The fix touches only how an id is named. Blocker lifecycle and task lifecycle
/// stay separate, which is the whole reason REPLAN does not own blockers.
#[tokio::test]
async fn raising_a_blocker_does_not_move_the_task() {
    let fx = Fixture::new().await;
    fx.prepared(&["ALPHA"]).await;
    let task = TaskId::from_string("ALPHA");

    let mut runner = fx.runner();
    let scheduled = runner
        .schedule(&fx.project, later(-200))
        .await
        .expect("the scheduling round");
    assert!(scheduled.assignment_for(&task).is_some());

    runner
        .raise_blocker(
            &task,
            BlockerKind::Conflict,
            "sources disagree",
            None,
            later(-100),
        )
        .await
        .expect("the blocker lands");

    let stored = fx
        .store
        .tasks()
        .get_task(&task)
        .await
        .expect("the task reads");
    assert_eq!(
        stored.status,
        director_domain::task::TaskStatus::InProgress,
        "nothing here writes TaskStatus::Blocked"
    );
}

impl Fixture {
    async fn register_helper_agent(&self, id: &str) {
        self.store
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
}
