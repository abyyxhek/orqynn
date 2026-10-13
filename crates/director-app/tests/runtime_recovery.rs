//! Phase 11 — runtime and recovery validation.
//!
//! Every step of the loop is landed and tested individually, and Phase 10's
//! end-to-end test proves the happy path reaches a `done` task with a durable
//! record behind it. What those tests do not yet prove is the property the
//! whole design exists for: that Orqyn is a *persistent* orchestration system
//! rather than an in-memory one that happens to write to disk.
//!
//! These tests are that proof, run against a real store on a real file:
//!
//! - **Restart.** Drive the loop, close the store, reopen the *same file*, and
//!   continue the orchestration — with every entity the loop produced read back
//!   from the store rather than held in memory.
//! - **Interruption.** The same, except the first store is never closed
//!   cleanly: it is abandoned the way a process that vanished would leave it.
//!   Committed state must still be recoverable, and the atomic operations must
//!   still be atomic — no task reading `in_progress` without the assignment
//!   that put it there.
//! - **Agent failure.** An agent holding work goes quiet past the lease window.
//!   MONITOR reclaims the work, the tenure is retained as history, the vanished
//!   agent stays identifiable in it, and a replacement takes the work to
//!   completion.
//! - **Replan after failure.** A task the engine judged `failed` is reworked
//!   and handed to a different agent, who passes the corrected check — with the
//!   failed judgment still on record.
//! - **Context continuity.** Everything a driver would need to hand a
//!   replacement agent the work it is inheriting is recoverable from Orqyn's
//!   own state, and the test says plainly where it is not.
//!
//! No test here pokes a status by hand to reach the state it asserts, and no
//! test constructs the final rows it then claims to verify. Every state is
//! produced by the steps that produce it in production, and every assertion
//! reloads from the store a fresh tick — or a fresh process — would read.

use std::process::Command;

use director_adapters::executor::LocalExecutor;
use director_adapters::git::GitService;
use director_adapters::InMemoryProvider;
use director_app::monitor::{monitor_at, report_done, ReportRequest};
use director_app::observe::{observe_at, Observed};
use director_app::plan::{plan, PlanRequest, TaskSpec};
use director_app::replan::{replan, Remediation, ReplanRequest, TaskDecision};
use director_app::schedule::schedule;
use director_app::verify::verify;
use director_domain::agent::{Agent, AgentStatus, Harness};
use director_domain::assignment::{AssignmentStatus, ReleaseReason};
use director_domain::capability::Capability;
use director_domain::ids::IdGenerator;
use director_domain::ids::{
    AgentId, AssignmentId, MachineId, PlanId, ProjectId, RepositoryId, TaskId,
};
use director_domain::project::Project;
use director_domain::providers::MemoryProvider;
use director_domain::task::{ExpectedOutput, TaskStatus};
use director_domain::verification::VerificationStatus;
use director_domain::{
    AgentRepository, AssignmentRepository, CheckpointRepository, DecisionRepository,
    PlanRepository, ProjectRepository, TaskRepository, VerificationRepository,
};
use director_store::Store;

/// A store open on a real SQLite file, with the project and agents a running
/// loop has after OBSERVE.
///
/// It has two constructors on purpose. [`Runtime::boot`] registers the project
/// and the agents, the way the loop's first observation does.
/// [`Runtime::restart`] opens the *same file* and reads them back out of it —
/// it registers nothing, because a restart that had to re-create its own state
/// would be proof the state was never durable in the first place.
struct Runtime {
    store: Store,
    project: ProjectId,
    working_dir: std::path::PathBuf,
    /// The git service for the working directory, so an OBSERVE round can be
    /// run against the same repository the loop would observe.
    git: GitService,
    /// The repository id the working directory is registered under.
    repository: RepositoryId,
    /// The pinned clock the failure tests move heartbeats against rather than
    /// waiting out a real lease window.
    now: chrono::DateTime<chrono::Utc>,
}

impl Runtime {
    /// The file everything in these tests survives across.
    const DB: &'static str = "orqyn.db";

    /// First boot: open the store and register the project and `agent_count`
    /// coding agents. A replacement-agent scenario needs two; a single-threaded
    /// scenario needs one, because the scheduler hands out one task per agent
    /// per round.
    async fn boot(
        dir: &std::path::Path,
        now: chrono::DateTime<chrono::Utc>,
        agent_count: usize,
    ) -> Self {
        // The git repository is created before anything else, because the
        // project's own record points at it: a project whose local path was
        // not a repository would be one OBSERVE could never observe.
        let repo = git_repo(dir);

        let store = Store::open(dir.join(Self::DB))
            .await
            .expect("the store opens on a fresh file");

        let project = ProjectId::from_string("PROJ-1");
        store
            .projects()
            .create_project(&Project::new(
                project.clone(),
                project.as_str(),
                repo.to_str().unwrap_or("/nowhere"),
            ))
            .await
            .expect("project created");

        for n in 1..=agent_count {
            let id = format!("AGENT-{n}");
            store
                .agents()
                .register_agent(&Agent::register(
                    AgentId::from_string(&id),
                    format!("claude-{n}"),
                    Harness::ClaudeCode,
                    MachineId::from_string(format!("MACH-{n}")),
                    vec![Capability::Coding],
                ))
                .await
                .expect("agent registered");
        }

        Runtime {
            store,
            project,
            working_dir: repo,
            git: GitService::new(dir.join(GIT_STATE_DIR)).expect("the git service"),
            repository: RepositoryId::from_string(REPO),
            now,
        }
    }

    /// Reopen the file after the store that wrote it is gone. The project and
    /// the registry are read back out of the store, not re-registered — each of
    /// these reads is itself an assertion, and a failure here means the state
    /// did not survive the restart.
    async fn restart(
        dir: &std::path::Path,
        now: chrono::DateTime<chrono::Utc>,
        agent_count: usize,
    ) -> Self {
        let store = Store::open(dir.join(Self::DB))
            .await
            .expect("the store reopens its own file");

        let project = store
            .projects()
            .get_project(&ProjectId::from_string("PROJ-1"))
            .await
            .expect("the project survived the restart")
            .id;
        let agents = store
            .agents()
            .list_agents()
            .await
            .expect("the registry survived the restart");
        assert_eq!(
            agents.len(),
            agent_count,
            "every agent survived the restart, not just the project"
        );

        Runtime {
            store,
            project,
            working_dir: git_repo(dir),
            git: GitService::new(dir.join(GIT_STATE_DIR)).expect("the git service reopens"),
            repository: RepositoryId::from_string(REPO),
            now,
        }
    }

    /// The task as the store now holds it, not as any step returned it.
    async fn reload(&self, id: &str) -> director_domain::task::Task {
        self.store
            .tasks()
            .get_task(&TaskId::from_string(id))
            .await
            .expect("task exists")
    }

    /// Run one SCHEDULE round and expect it to hand out exactly one task — the
    /// shape of a tick with one agent free and one ready task. Returns what it
    /// did, so a test can name the tenure it then reports against.
    async fn schedule_one(&self, ids: &mut IdGenerator) -> director_app::schedule::Scheduled {
        let report = schedule(&self.store, ids, &self.project)
            .await
            .expect("the schedule round");
        assert_eq!(report.assigned.len(), 1, "one ready task, one free agent");
        assert!(report.refused.is_empty(), "nothing was refused");
        report.assigned.into_iter().next().unwrap()
    }

    /// The holder reports finishing. The report stops at `verification_pending`
    /// by design; it never completes the work.
    async fn report_done(&self, assignment: AssignmentId) {
        report_done(
            &self.store,
            ReportRequest {
                assignment_id: assignment,
            },
        )
        .await
        .expect("the completion report");
    }

    /// Run VERIFY and let the engine judge everything currently awaiting a
    /// verdict.
    async fn verify_round(&self) -> director_app::verify::VerifyReport {
        verify(
            &self.store,
            &LocalExecutor::default(),
            &self.project,
            &self.working_dir,
        )
        .await
        .expect("the verify round")
    }

    /// Move an agent's last heartbeat `minutes` into the past, the way an agent
    /// that went quiet would. The clock stays pinned; the heartbeat moves.
    async fn quiet_for(&self, agent: &AgentId, minutes: i64) {
        let mut agent = self
            .store
            .agents()
            .get_agent(agent)
            .await
            .expect("agent exists");
        agent.last_seen = self.now - chrono::Duration::minutes(minutes);
        self.store
            .agents()
            .update_agent(&agent)
            .await
            .expect("heartbeat moved");
    }

    /// Run one OBSERVE round against the working directory, the way a loop tick
    /// would, and return what it saw — including any checkpoints it wrote. This
    /// is the production path: nothing here writes a checkpoint by hand, and a
    /// test that wants one arrives at it by observing real change.
    async fn observe_round(&self, ids: &mut IdGenerator) -> Observed {
        self.observe_round_with(ids, &InMemoryProvider::new(), self.now)
            .await
    }

    /// [`Self::observe_round`], with the memory substrate the round publishes
    /// checkpoints to supplied by the caller — so a test can hand it one that
    /// fails, and prove the checkpoint landed anyway.
    async fn observe_round_with<M: MemoryProvider>(
        &self,
        ids: &mut IdGenerator,
        memory: &M,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Observed {
        observe_at(
            &self.store,
            &self.git,
            ids,
            memory,
            &self.project,
            &self.repository,
            &self.working_dir.to_string_lossy().replace('\\', "/"),
            now,
        )
        .await
        .expect("the observation round")
    }

    /// Write a file into the working directory and commit it, returning the
    // commit's sha. This is the real, durable project change an OBSERVE round
    /// then observes — the evidence a checkpoint is derived from.
    fn commit(&self, path: &str, contents: &str, message: &str) -> String {
        commit_file(&self.working_dir, path, contents, message)
    }
}

/// Write `path` inside `repo` and commit it, returning the new `HEAD` sha.
fn commit_file(repo: &std::path::Path, path: &str, contents: &str, message: &str) -> String {
    let file = repo.join(path);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    std::fs::write(&file, contents).expect("write file");
    let status = Command::new("git")
        .current_dir(repo)
        .args(["add", path])
        .status()
        .expect("git add");
    assert!(status.success(), "git add should succeed");
    let status = Command::new("git")
        .current_dir(repo)
        .args(["commit", "-q", "-m", message])
        .status()
        .expect("git commit");
    assert!(status.success(), "git commit should succeed");
    let sha = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse HEAD");
    assert!(sha.status.success(), "rev-parse HEAD");
    String::from_utf8(sha.stdout)
        .expect("sha is utf-8")
        .trim()
        .to_string()
}

/// A git repository in `dir` with one commit, created if it does not already
/// exist. [`Runtime::boot`] makes one the first time and [`Runtime::restart`]
/// finds it again — the repository is part of the state that survives, so a
/// restart that re-created it would be proof nothing was durable.
fn git_repo(dir: &std::path::Path) -> std::path::PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).expect("the repo directory");

    // A repository that already exists is left exactly as it is: its history is
    // the project's history.
    if repo.join(".git").exists() {
        return repo;
    }

    let git = || {
        let mut cmd = Command::new("git");
        cmd.current_dir(&repo);
        cmd
    };
    // A standalone commit needs an identity; the test environment has none.
    git().args(["init", "-q"]).status().expect("git init");
    git()
        .args(["config", "user.name", "Orqyn Tests"])
        .status()
        .expect("set name");
    git()
        .args(["config", "user.email", "tests@orqyn.invalid"])
        .status()
        .expect("set email");
    git()
        .args(["config", "commit.gpgsign", "false"])
        .status()
        .expect("disable signing");

    commit_file(&repo, "README.md", "# the project\n", "the project begins");
    repo
}

/// One expected output with the machine check Orqyn will run itself.
fn checked(criterion: &str, check: &str) -> ExpectedOutput {
    ExpectedOutput {
        criterion: criterion.to_string(),
        check: Some(check.to_string()),
    }
}

/// A task spec whose single criterion is the given check.
fn spec(id: &str, criterion: &str, check: &str) -> TaskSpec {
    let mut spec = TaskSpec::new(TaskId::from_string(id), id, format!("do {id}"));
    spec.expected_outputs = vec![checked(criterion, check)];
    spec
}

/// The repository id the working directory is registered under.
const REPO: &str = "REPO-1";

/// Where the git service keeps its own observation state, outside the
/// repository it observes.
const GIT_STATE_DIR: &str = "orqyn-state";

/// A pinned clock an hour before the epoch-free present, far enough from `now`
/// that a heartbeat moved 90 minutes into the past is unambiguously gone and a
/// task written at two different pinned moments gets two different timestamps.
fn pinned_now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_naive_utc_and_offset(
        chrono::NaiveDate::from_ymd_opt(2026, 10, 7)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap(),
        chrono::Utc,
    )
}

#[tokio::test]
async fn persistent_state_survives_a_process_restart_and_the_loop_continues() {
    // The milestone's central test. Four tasks, one of which fails and is
    // cancelled, driven through the loop to a state worth restarting from:
    // work done, work failed, work cancelled with a recorded decision, and
    // work still waiting. Then the store is closed, reopened on the same file,
    // and the remaining work is finished — by steps that read the recovered
    // state, not by a test that remembers it.
    let dir = tempfile::TempDir::new().expect("a temp dir for the store");
    let now = pinned_now();

    // The plan is listed in the order the topological tie-break will hand it
    // out: AUTH, then API (which depends on AUTH), then EXTRA, then TAIL (which
    // depends on API). EXTRA's check fails, so it is the task that reaches
    // `failed` and then `cancelled`; TAIL is the task that is still `todo` when
    // the process restarts.
    let before = Runtime::boot(dir.path(), now, 1).await;
    let mut ids = IdGenerator::new();
    plan(
        &before.store,
        PlanRequest {
            project_id: before.project.clone(),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "ship the feature".into(),
            rationale: "four tasks exercising every persisted entity".into(),
            tasks: vec![
                spec("AUTH", "auth answers", "git --version"),
                TaskSpec {
                    depends_on: vec![TaskId::from_string("AUTH")],
                    ..spec("API", "api answers", "git --version")
                },
                spec("EXTRA", "extra answers", "git --no-such-flag"),
                TaskSpec {
                    depends_on: vec![TaskId::from_string("API")],
                    ..spec("TAIL", "tail answers", "git --version")
                },
            ],
            authorized_by: AgentId::from_string("AGENT-1"),
        },
    )
    .await
    .expect("the plan lands");

    // AUTH: assigned, reported, and judged a pass.
    let auth = before.schedule_one(&mut ids).await;
    assert_eq!(auth.task_id, TaskId::from_string("AUTH"));
    before.report_done(auth.assignment.id.clone()).await;
    before.verify_round().await;

    // API: unblocked by AUTH's completion, assigned, reported, judged a pass.
    let api = before.schedule_one(&mut ids).await;
    assert_eq!(api.task_id, TaskId::from_string("API"));
    before.report_done(api.assignment.id.clone()).await;
    before.verify_round().await;

    // EXTRA: its check fails, so the engine judges the work `failed`.
    let extra = before.schedule_one(&mut ids).await;
    assert_eq!(extra.task_id, TaskId::from_string("EXTRA"));
    before.report_done(extra.assignment.id.clone()).await;
    let failed_round = before.verify_round().await;
    assert_eq!(failed_round.failed().count(), 1);
    assert_eq!(
        before.reload("EXTRA").await.status,
        TaskStatus::Failed,
        "the check failed, so the work is failed — not done"
    );

    // REPLAN answers the failure by cancelling the task, which is the one write
    // in the loop that records a Decision. That is what makes "why did we stop
    // pursuing this" answerable after the process that decided it is gone.
    replan(
        &before.store,
        ReplanRequest {
            project_id: before.project.clone(),
            decisions: vec![TaskDecision {
                task_id: TaskId::from_string("EXTRA"),
                remediation: Remediation::Cancel,
                reason: Some("the extra work is no longer worth doing".into()),
            }],
            decided_by: Some(AgentId::from_string("AGENT-1")),
        },
    )
    .await
    .expect("the replan round");
    assert_eq!(before.reload("EXTRA").await.status, TaskStatus::Cancelled);

    // Snapshot the versions before the restart, so the test can prove the
    // version counter is the store's, not the process's.
    let auth_version = before.reload("AUTH").await.state_version;
    let auth_tenures = before
        .store
        .assignments()
        .assignment_history(&TaskId::from_string("AUTH"))
        .await
        .expect("tenure history")
        .len();

    // --- The restart. The store is dropped, which tears the connection pool
    // down; nothing of Orqyn's remains in memory but the file. ---
    drop(before);
    let after = Runtime::restart(dir.path(), now, 1).await;

    // The task survived, identity and status intact.
    assert_eq!(after.reload("AUTH").await.status, TaskStatus::Done);
    assert_eq!(after.reload("API").await.status, TaskStatus::Done);
    assert_eq!(after.reload("EXTRA").await.status, TaskStatus::Cancelled);
    assert_eq!(after.reload("TAIL").await.status, TaskStatus::Todo);
    assert_eq!(
        after.reload("AUTH").await.state_version,
        auth_version,
        "the version counter is persisted, not restarted at one"
    );

    // The objective and the acceptance contract survived too — a restarted
    // loop hands an agent the same contract the plan named.
    let auth = after.reload("AUTH").await;
    assert_eq!(auth.objective, "do AUTH");
    assert_eq!(auth.expected_outputs.len(), 1);
    assert_eq!(auth.expected_outputs[0].criterion, "auth answers");

    // The plan survived and is still the active one, with all four tasks in it.
    let active = after
        .store
        .plans()
        .active_plan_for_project(&after.project)
        .await
        .expect("the plan survived")
        .expect("there is still an active plan");
    assert_eq!(active.id, PlanId::from_string("PLAN-1"));
    assert_eq!(
        after
            .store
            .tasks()
            .list_tasks(&after.project)
            .await
            .expect("tasks survived")
            .len(),
        4
    );

    // The tenures survived: AUTH's single tenure is still closed and retained,
    // and the count is what it was before the restart.
    let tenures = after
        .store
        .assignments()
        .assignment_history(&TaskId::from_string("AUTH"))
        .await
        .expect("tenure history survived");
    assert_eq!(tenures.len(), auth_tenures);
    assert_eq!(tenures[0].status, AssignmentStatus::Released);
    assert_eq!(tenures[0].release_reason, Some(ReleaseReason::WorkComplete));

    // The transition history survived, and reading it back tells the story a
    // fresh process could not otherwise know: AUTH went todo → in_progress →
    // verification_pending → done.
    let history = after
        .store
        .tasks()
        .task_history(&TaskId::from_string("AUTH"))
        .await
        .expect("transition history survived");
    let path: Vec<TaskStatus> = std::iter::once(TaskStatus::Todo)
        .chain(history.iter().map(|transition| transition.to))
        .collect();
    assert_eq!(
        path,
        vec![
            TaskStatus::Todo,
            TaskStatus::InProgress,
            TaskStatus::VerificationPending,
            TaskStatus::Done
        ]
    );

    // The verification records survived: two passes, and each carries the
    // evidence the engine gathered rather than a verdict alone.
    let auth_verifications = after
        .store
        .verifications()
        .verifications_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("verification history survived");
    assert_eq!(auth_verifications.len(), 1);
    assert_eq!(auth_verifications[0].status, VerificationStatus::Passed);
    assert_eq!(auth_verifications[0].task_id, TaskId::from_string("AUTH"));
    let extra_verifications = after
        .store
        .verifications()
        .verifications_for_task(&TaskId::from_string("EXTRA"))
        .await
        .expect("the failed judgment survived");
    assert_eq!(extra_verifications.len(), 1);
    assert_eq!(extra_verifications[0].status, VerificationStatus::Failed);

    // The decision survived, attached to the task it was about — a cancelled
    // task's reason outlives the task's own activity.
    let decisions = after
        .store
        .decisions()
        .decisions_for_task(&TaskId::from_string("EXTRA"))
        .await
        .expect("the decision survived");
    assert_eq!(decisions.len(), 1);
    assert!(decisions[0].rationale.contains("no longer worth doing"));

    // --- The loop continues on the recovered state. TAIL was `todo` before
    // the restart and nothing since has touched it; the recovered loop hands it
    // out and finishes it. ---
    let tail = after.schedule_one(&mut ids).await;
    assert_eq!(tail.task_id, TaskId::from_string("TAIL"));
    after.report_done(tail.assignment.id.clone()).await;
    let final_round = after.verify_round().await;
    assert_eq!(final_round.passed().count(), 1);
    assert_eq!(after.reload("TAIL").await.status, TaskStatus::Done);

    // The id of the new verification continues the sequence the store holds,
    // which is what makes a re-judgment after a restart a new row rather than a
    // collision with one.
    let tail_verifications = after
        .store
        .verifications()
        .verifications_for_task(&TaskId::from_string("TAIL"))
        .await
        .expect("the new judgment landed");
    assert_eq!(tail_verifications.len(), 1);
    assert_eq!(
        tail_verifications[0].id,
        director_domain::ids::VerificationId::from_string("VER-TAIL-1")
    );

    // The whole plan is now complete, and the store — not this test — says so.
    let remaining = after
        .store
        .tasks()
        .list_tasks(&after.project)
        .await
        .expect("tasks")
        .into_iter()
        .filter(|task| !task.status.is_terminal())
        .count();
    assert_eq!(remaining, 0, "every task reached a terminal state");
}

#[tokio::test]
async fn an_abandoned_process_leaves_committed_state_intact_and_recoverable() {
    // The crash equivalent. A true SIGKILL test needs a second process, and
    // the loop is a library with no binary yet — there is no process boundary
    // to kill. What *is* testable, deterministically and portably, is the
    // property a crash would expose: the first store is never closed cleanly,
    // it is simply forgotten, exactly as a process that vanished would leave
    // it. A second store then opens the same file and must recover every
    // committed fact, and must find no half-applied one.
    let dir = tempfile::TempDir::new().expect("a temp dir for the store");
    let now = pinned_now();

    let first = Runtime::boot(dir.path(), now, 1).await;
    let mut ids = IdGenerator::new();
    plan(
        &first.store,
        PlanRequest {
            project_id: first.project.clone(),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "ship the feature".into(),
            rationale: "interruption at the most dangerous moment".into(),
            tasks: vec![spec("WORK", "the work answers", "git --version")],
            authorized_by: AgentId::from_string("AGENT-1"),
        },
    )
    .await
    .expect("the plan lands");

    // The task is in flight — the state that lives in two tables at once, and
    // the one a non-atomic write would leave inconsistent across a crash.
    let _work = first.schedule_one(&mut ids).await;
    assert_eq!(first.reload("WORK").await.status, TaskStatus::InProgress);

    // The first store is *abandoned*, not dropped: it stays open and holds the
    // file, the way a killed process's connection would still be held by the
    // OS until the kernel reaps it. Nothing is flushed by a destructor here.
    let db = dir.path().join(Runtime::DB);
    let recovered = Store::open(&db)
        .await
        .expect("a second store recovers the file");

    // The committed state is all there.
    assert_eq!(
        recovered
            .tasks()
            .get_task(&TaskId::from_string("WORK"))
            .await
            .expect("the task survived")
            .status,
        TaskStatus::InProgress
    );

    // And it is *consistent*: the atomic handoff survived the interruption, so
    // the task is in flight *because* an assignment is still behind it. A write
    // that landed the status without the tenure is the state this test exists
    // to rule out.
    let assignment = recovered
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("WORK"))
        .await
        .expect("the assignment survived")
        .expect("an in-progress task has an active assignment behind it");
    assert_eq!(assignment.agent_id, AgentId::from_string("AGENT-1"));
    assert_eq!(
        recovered
            .agents()
            .get_agent(&AgentId::from_string("AGENT-1"))
            .await
            .expect("the agent survived")
            .current_task,
        Some(TaskId::from_string("WORK"))
    );

    // The recovered store can finish the work the abandoned one started. The
    // completion report and the verdict below are the second store's writes,
    // and they apply cleanly against state the first store committed.
    report_done(
        &recovered,
        ReportRequest {
            assignment_id: assignment.id,
        },
    )
    .await
    .expect("the recovered store accepts the report");

    // The first store is now let go, after the second one has already taken
    // over the file — the messiest possible handoff, and it still lands.
    drop(first);

    let report = verify(
        &recovered,
        &LocalExecutor::default(),
        &recovered_project(&recovered).await,
        dir.path(),
    )
    .await
    .expect("the recovered store judges the work");
    assert_eq!(report.passed().count(), 1);
    assert_eq!(
        recovered
            .tasks()
            .get_task(&TaskId::from_string("WORK"))
            .await
            .expect("the task survived")
            .status,
        TaskStatus::Done
    );
}

/// The project id as the store holds it, for a store whose `Runtime` wrapper is
/// out of scope.
async fn recovered_project(store: &Store) -> ProjectId {
    store
        .projects()
        .list_projects()
        .await
        .expect("projects survived")
        .into_iter()
        .map(|project| project.id)
        .next()
        .expect("there is a project")
}

#[tokio::test]
async fn an_agent_that_vanishes_loses_its_lease_and_a_replacement_finishes_the_work() {
    // The recovery scenario the whole model is shaped for: the agent holding
    // the work disappears, the lease runs out, the work comes back, and a
    // different agent picks it up — with the vanished agent still identifiable
    // in the history and the task's identity unchanged throughout.
    let dir = tempfile::TempDir::new().expect("a temp dir for the store");
    let now = pinned_now();
    let runtime = Runtime::boot(dir.path(), now, 2).await;

    let mut ids = IdGenerator::new();
    plan(
        &runtime.store,
        PlanRequest {
            project_id: runtime.project.clone(),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "ship the feature".into(),
            rationale: "a task that outlives its first agent".into(),
            tasks: vec![spec("WORK", "the work answers", "git --version")],
            authorized_by: AgentId::from_string("AGENT-1"),
        },
    )
    .await
    .expect("the plan lands");

    let first = runtime.schedule_one(&mut ids).await;
    assert_eq!(first.agent_id, AgentId::from_string("AGENT-1"));
    let first_assignment = first.assignment.id.clone();

    // AGENT-1 goes quiet: not stale, but past the stale window, so the lease is
    // over. The heartbeat moves and the clock does not.
    runtime
        .quiet_for(&AgentId::from_string("AGENT-1"), 90)
        .await;

    let report = monitor_at(&runtime.store, &runtime.project, runtime.now)
        .await
        .expect("the monitor round");

    // MONITOR reclaimed exactly one lease, and reported it as expired rather
    // than silently dropping it.
    assert_eq!(report.in_flight.len(), 1);
    let expired = match &report.in_flight[0] {
        director_app::monitor::InFlight::Expired(expired) => expired,
        other => panic!("expected an expired lease, got {other:?}"),
    };
    assert_eq!(expired.task.id, TaskId::from_string("WORK"));

    // The task is not lost and not done: it is back where a plan can hand it
    // out again. This is the recovery state, and it is a *task* state, not an
    // agent-shaped one.
    assert_eq!(
        runtime.reload("WORK").await.status,
        TaskStatus::Todo,
        "the lease expired, so the work is handable again"
    );
    assert_ne!(
        runtime.reload("WORK").await.status,
        TaskStatus::Done,
        "an agent vanishing never completes the work"
    );

    // The tenure is retained, not deleted, and it names who held the work —
    // "which agent had this when it went wrong" stays answerable.
    let tenures = runtime
        .store
        .assignments()
        .assignment_history(&TaskId::from_string("WORK"))
        .await
        .expect("tenure history");
    assert_eq!(tenures.len(), 1, "the vanished agent's tenure is retained");
    assert_eq!(tenures[0].agent_id, AgentId::from_string("AGENT-1"));
    assert_eq!(tenures[0].status, AssignmentStatus::Released);
    assert_eq!(tenures[0].release_reason, Some(ReleaseReason::LeaseExpired));

    // The agent is recorded as disconnected, so the loop does not hand the work
    // straight back to the agent that lost it.
    assert_eq!(
        runtime
            .store
            .agents()
            .get_agent(&AgentId::from_string("AGENT-1"))
            .await
            .expect("agent exists")
            .status,
        AgentStatus::Disconnected
    );

    // Nothing is in flight anymore, so a second monitor round is quiet — the
    // step is safe to run on every tick.
    let second = monitor_at(&runtime.store, &runtime.project, runtime.now)
        .await
        .expect("the second monitor round");
    assert!(second.is_quiet(), "a task back to `todo` is not in flight");

    // REPLAN is not needed here, and saying so is the assertion: a lease that
    // ran out is back to `todo`, which is handable, so the replan survey finds
    // nothing stopped. MONITOR and REPLAN do not overlap, by design.
    let replanned = replan(
        &runtime.store,
        ReplanRequest {
            project_id: runtime.project.clone(),
            decisions: vec![],
            decided_by: None,
        },
    )
    .await
    .expect("the replan round");
    assert!(
        replanned.is_settled(),
        "an expired lease is not a replan condition"
    );

    // The replacement: AGENT-1 is disconnected and held the task once, so the
    // scheduler's eligibility and fresh-eyes rules both point at AGENT-2.
    let replacement = runtime.schedule_one(&mut ids).await;
    assert_eq!(replacement.task_id, TaskId::from_string("WORK"));
    assert_eq!(
        replacement.agent_id,
        AgentId::from_string("AGENT-2"),
        "the replacement is a different agent, not the one that vanished"
    );

    // The task's identity never changed — the same id that was planned is the
    // one now held by AGENT-2, and the two tenures sit side by side in one
    // history.
    assert_eq!(runtime.reload("WORK").await.status, TaskStatus::InProgress);
    let tenures = runtime
        .store
        .assignments()
        .assignment_history(&TaskId::from_string("WORK"))
        .await
        .expect("tenure history");
    assert_eq!(tenures.len(), 2);
    // The history is chronological, so the vanished agent's tenure comes first
    // and the replacement's second — the order a driver reading the trail
    // would tell the story in.
    assert_eq!(tenures[0].agent_id, AgentId::from_string("AGENT-1"));
    assert_eq!(tenures[1].agent_id, AgentId::from_string("AGENT-2"));
    assert_ne!(first_assignment, replacement.assignment.id);

    // The replacement finishes, and the verdict is the engine's — never the
    // agent's.
    runtime.report_done(replacement.assignment.id.clone()).await;
    let judged = runtime.verify_round().await;
    assert_eq!(judged.passed().count(), 1);
    assert_eq!(runtime.reload("WORK").await.status, TaskStatus::Done);
}

#[tokio::test]
async fn a_failed_task_can_be_reworked_for_a_replacement_agent_who_then_passes() {
    // The REPLAN path, end to end: the work is judged `failed`, the caller
    // decides the *criterion* was wrong rather than the implementation, the
    // reworked task goes back to a different agent, and the corrected check
    // passes — with both judgments on record.
    let dir = tempfile::TempDir::new().expect("a temp dir for the store");
    let now = pinned_now();
    let runtime = Runtime::boot(dir.path(), now, 2).await;

    let mut ids = IdGenerator::new();
    plan(
        &runtime.store,
        PlanRequest {
            project_id: runtime.project.clone(),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "ship the feature".into(),
            rationale: "a task whose first check tested the wrong thing".into(),
            tasks: vec![spec(
                "WORK",
                "the work answers",
                // A criterion that reads sensibly but names a check the work
                // cannot pass — the failure is in the contract, not the code.
                "git --no-such-flag",
            )],
            authorized_by: AgentId::from_string("AGENT-1"),
        },
    )
    .await
    .expect("the plan lands");

    let first = runtime.schedule_one(&mut ids).await;
    assert_eq!(first.agent_id, AgentId::from_string("AGENT-1"));
    runtime.report_done(first.assignment.id.clone()).await;
    runtime.verify_round().await;
    assert_eq!(
        runtime.reload("WORK").await.status,
        TaskStatus::Failed,
        "the check failed, so the work is failed and nothing completed it"
    );

    // The decision to rework, amending the criterion. A retry would run the
    // same wrong check again; a rework is the remediation for a contract that
    // was wrong.
    replan(
        &runtime.store,
        ReplanRequest {
            project_id: runtime.project.clone(),
            decisions: vec![TaskDecision {
                task_id: TaskId::from_string("WORK"),
                remediation: Remediation::Rework {
                    objective: None,
                    expected_outputs: Some(vec![checked("the work answers", "git --version")]),
                },
                reason: Some("the check tested the wrong thing".into()),
            }],
            decided_by: Some(AgentId::from_string("AGENT-1")),
        },
    )
    .await
    .expect("the rework lands");
    assert_eq!(
        runtime.reload("WORK").await.status,
        TaskStatus::Todo,
        "a reworked task is handable again"
    );
    let reworked = runtime.reload("WORK").await;
    assert_eq!(
        reworked.expected_outputs[0].check.as_deref(),
        Some("git --version"),
        "the reworked task carries the amended contract to the next agent"
    );

    // The transition history records that this was a decision, not a fresh
    // start: the task went `failed` → `todo`.
    let history = runtime
        .store
        .tasks()
        .task_history(&TaskId::from_string("WORK"))
        .await
        .expect("history");
    assert_eq!(
        history.last().expect("there is a transition").to,
        TaskStatus::Todo
    );

    // A different agent takes the reworked task — AGENT-1 held it, so fresh
    // eyes rank AGENT-2 first.
    let replacement = runtime.schedule_one(&mut ids).await;
    assert_eq!(replacement.agent_id, AgentId::from_string("AGENT-2"));
    runtime.report_done(replacement.assignment.id.clone()).await;
    let judged = runtime.verify_round().await;
    assert_eq!(judged.passed().count(), 1);
    assert_eq!(runtime.reload("WORK").await.status, TaskStatus::Done);

    // Both judgments are on record: the failure is not erased by the pass, and
    // the pass is not assumed by the failure.
    let history = runtime
        .store
        .verifications()
        .verifications_for_task(&TaskId::from_string("WORK"))
        .await
        .expect("verification history");
    assert_eq!(
        history.len(),
        2,
        "the failure and the pass are both on record"
    );
    assert_eq!(history[0].status, VerificationStatus::Passed);
    assert_eq!(history[1].status, VerificationStatus::Failed);
    assert_ne!(history[0].id, history[1].id);
}

#[tokio::test]
async fn a_replacement_agent_can_be_given_everything_the_prior_agent_left_behind() {
    // Context continuity, Phase 12 edition. The loop has no "hand a replacement
    // agent its context" step; what it has is the store plus a checkpoint that
    // the OBSERVE round writes automatically. Phase 11 proved the store alone
    // was enough to reconstruct the picture, but it had to write the checkpoint
    // by hand to do it — "the loop has no step that writes checkpoints yet" was
    // the caveat the test itself recorded.
    //
    // That caveat is gone. The checkpoint below is produced by the production
    // path: an OBSERVE round sees real change on an in-flight task and writes
    // one. No test in this function calls `create_checkpoint`.
    //
    // The scenario still stops mid-flight on purpose: AGENT-1 has failed and
    // been reworked, AGENT-2 holds the work but has not reported, and the driver
    // must brief AGENT-2 from Orqyn's state alone.
    let dir = tempfile::TempDir::new().expect("a temp dir for the store");
    let now = pinned_now();
    let runtime = Runtime::boot(dir.path(), now, 2).await;

    let mut ids = IdGenerator::new();
    plan(
        &runtime.store,
        PlanRequest {
            project_id: runtime.project.clone(),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "ship the feature".into(),
            rationale: "a task inherited mid-flight".into(),
            tasks: vec![spec(
                "WORK",
                "the work answers",
                // The first check cannot pass — the failure is what makes this
                // an inheritance rather than a fresh start.
                "git --no-such-flag",
            )],
            authorized_by: AgentId::from_string("AGENT-1"),
        },
    )
    .await
    .expect("the plan lands");

    // The first observation registers the repository and takes its baseline.
    // The task is still `todo`, so even a change here would checkpoint nothing:
    // only work that is actually out is worth a resumption document.
    let baseline = runtime.observe_round(&mut ids).await;
    assert!(
        baseline.checkpoints.is_empty(),
        "nothing is in flight to checkpoint"
    );

    // AGENT-1 takes the work and gets partway through it: one change committed,
    // one left uncommitted. Both are real, durable project state — and neither
    // is a completion claim.
    let first = runtime.schedule_one(&mut ids).await;
    assert_eq!(first.agent_id, AgentId::from_string("AGENT-1"));
    let sha = runtime.commit(
        "src/auth.rs",
        "pub fn authenticate() -> bool { true }\n",
        "scaffold the auth endpoint",
    );

    // The next observation sees the change and writes the checkpoint — derived
    // from the git evidence the round just collected, not from anything an
    // agent reported. This is the Phase 12 activation, and the assertions below
    // are what prove it fired.
    std::fs::write(
        runtime.working_dir.join("src/middleware.rs"),
        "// not wired into the router yet\n",
    )
    .expect("an uncommitted change");
    let observed = runtime.observe_round(&mut ids).await;
    assert_eq!(
        observed.checkpoints.len(),
        1,
        "one in-flight task with real change produces one checkpoint"
    );
    let written = observed.checkpoints[0].clone();
    assert_eq!(written.task_id, TaskId::from_string("WORK"));
    assert_eq!(
        written.created_by_agent,
        Some(AgentId::from_string("AGENT-1"))
    );

    // The checkpoint carries the evidence it was derived from rather than a
    // narrative somebody typed: the commit the round observed, and the
    // uncommitted file it found in the tree.
    assert_eq!(written.commit_sha.as_deref(), Some(sha.as_str()));
    assert!(written
        .changed_files
        .iter()
        .any(|path| path == "src/middleware.rs"));
    assert!(
        written.progress.contains("src/middleware.rs"),
        "the narrative describes the observed tree"
    );
    // And it is a description of state, not a claim the work is finished —
    // nothing here can make the task done.
    assert!(!written.progress.to_lowercase().contains("done"));

    // AGENT-1 fails the check, the caller reworks the contract, and AGENT-2
    // inherits the task.
    runtime.report_done(first.assignment.id.clone()).await;
    runtime.verify_round().await;
    assert_eq!(runtime.reload("WORK").await.status, TaskStatus::Failed);
    replan(
        &runtime.store,
        ReplanRequest {
            project_id: runtime.project.clone(),
            decisions: vec![TaskDecision {
                task_id: TaskId::from_string("WORK"),
                remediation: Remediation::Rework {
                    objective: Some("ship the feature, with the corrected check".into()),
                    expected_outputs: Some(vec![checked("the work answers", "git --version")]),
                },
                reason: Some("the check tested the wrong thing".into()),
            }],
            decided_by: Some(AgentId::from_string("AGENT-1")),
        },
    )
    .await
    .expect("the rework lands");

    // The replacement is scheduled, and the scheduler hands it the recovery
    // context it just became responsible for — built from the persisted state,
    // not by the replacement asking six unrelated repository questions.
    let replacement = runtime.schedule_one(&mut ids).await;
    assert_eq!(replacement.agent_id, AgentId::from_string("AGENT-2"));
    let recovery = replacement
        .recovery
        .as_ref()
        .expect("a replacement inheriting checkpointed work gets a recovery context");

    // --- Everything a driver can brief the replacement on, read out of the
    // store the way a caller would. ---
    let task = runtime.reload("WORK").await;

    // The contract as it now stands: the amended objective and the corrected
    // acceptance criterion, not the ones the first agent failed against.
    assert_eq!(task.objective, "ship the feature, with the corrected check");
    assert_eq!(
        task.expected_outputs[0].check.as_deref(),
        Some("git --version")
    );

    // The current state and the version it was read at, so a driver can tell
    // the work is in flight and present the right version when it writes back.
    assert_eq!(task.status, TaskStatus::InProgress);
    let read_version = task.state_version;
    // The package is built *before* the assignment that hands out the tenure,
    // so it describes the work as the replacement inherits it: the assignment
    // is the write that moves the task to `in_progress`, and it bumps the
    // version by exactly one. A package that matched the post-assignment read
    // would be describing a version that did not exist when it was built.
    assert_eq!(
        recovery.package.task_state_version,
        read_version - 1,
        "the package reports the pre-tenure version it was built against"
    );

    // Who held it before, and why they are not holding it now.
    let tenures = runtime
        .store
        .assignments()
        .assignment_history(&TaskId::from_string("WORK"))
        .await
        .expect("tenure history");
    let prior = tenures
        .iter()
        .find(|tenure| tenure.status == AssignmentStatus::Released)
        .expect("the prior tenure is retained");
    assert_eq!(prior.agent_id, AgentId::from_string("AGENT-1"));
    assert_eq!(prior.release_reason, Some(ReleaseReason::WorkComplete));
    // The package carries the tenure history as it stood when the replacement
    // was briefed — which is every tenure but its own. The replacement's
    // assignment is the write that created it, and the package is built before
    // that write lands, so the history it carries answers "what happened to
    // the previous agent" without including a tenure that had not started.
    assert_eq!(
        recovery.package.tenures,
        tenures
            .iter()
            .filter(|tenure| tenure.agent_id != AgentId::from_string("AGENT-2"))
            .cloned()
            .collect::<Vec<_>>(),
        "the package carries the history it was built against"
    );
    assert_eq!(recovery.package.tenures.len(), 1);

    // What was judged, and what the evidence said — so the replacement knows
    // what already failed and why, rather than repeating it.
    let judgments = runtime
        .store
        .verifications()
        .verifications_for_task(&TaskId::from_string("WORK"))
        .await
        .expect("verification history");
    let failed_judgment = judgments
        .iter()
        .find(|judgment| judgment.status == VerificationStatus::Failed)
        .expect("the failed judgment is on record");
    assert!(failed_judgment.evidence[0]
        .detail
        .contains("git --no-such-flag"));
    assert_eq!(recovery.package.verifications, judgments);

    // Where the prior agent got to, and what the next action is — the
    // checkpoint a resumption continues from, written by the observation round
    // rather than by this test.
    let checkpoint = runtime
        .store
        .checkpoints()
        .latest_checkpoint(&TaskId::from_string("WORK"))
        .await
        .expect("the checkpoint survived")
        .expect("there is a checkpoint to resume from");
    assert_eq!(checkpoint, written);
    assert_eq!(
        runtime
            .store
            .checkpoints()
            .checkpoints_for_task(&TaskId::from_string("WORK"))
            .await
            .expect("checkpoint history")
            .len(),
        1,
        "one observation with change, one checkpoint in the lineage"
    );
    // A checkpoint taken against a dirty tree tells the replacement to
    // reconcile it before reporting — the observed state, not a guess.
    assert!(checkpoint
        .next_action
        .contains("uncommitted changes to reconcile"));

    // The latest transition, so a driver can say what changed most recently.
    let history = runtime
        .store
        .tasks()
        .task_history(&TaskId::from_string("WORK"))
        .await
        .expect("history");
    let latest = history.last().expect("there is a transition");
    assert_eq!(latest.to, TaskStatus::InProgress);
    // The package's transitions stop one short of the live history for the same
    // reason its tenures do: the final `Todo → InProgress` *is* the
    // replacement's assignment, written after the package was built. What the
    // package carries is everything that led to the handoff — the state a
    // driver briefs the replacement *on*.
    assert_eq!(
        recovery.package.transitions,
        history[..history.len() - 1],
        "the package carries the transitions that preceded the handoff"
    );
    assert_eq!(
        history[history.len() - 2].to,
        TaskStatus::Todo,
        "the last transition the package saw was the rework returning the task"
    );

    // The replacement agent's own record points at the work, so a driver
    // handing out the briefing is consistent with the registry.
    assert_eq!(
        runtime
            .store
            .agents()
            .get_agent(&AgentId::from_string("AGENT-2"))
            .await
            .expect("agent exists")
            .current_task,
        Some(TaskId::from_string("WORK"))
    );

    // And the briefing is load-bearing rather than decorative: the replacement
    // presents the version the store is actually on, so its report lands.
    report_done(
        &runtime.store,
        ReportRequest {
            assignment_id: replacement.assignment.id,
        },
    )
    .await
    .expect("the replacement's report lands");
    assert_eq!(
        runtime.reload("WORK").await.status,
        TaskStatus::VerificationPending
    );
    assert!(
        read_version < runtime.reload("WORK").await.state_version,
        "the write advanced the version, so the read the briefing was built on was live"
    );
}
