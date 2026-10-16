//! Process-boundary recovery tests: proving Orqynn's durable state survives a
//! process being forcibly terminated.
//!
//! # Why this file exists
//!
//! The rest of the suite proves things about Orqynn's steps. It cannot prove
//! anything about a *process* dying, because every one of those tests keeps the
//! writer alive. A task status held in memory by a live `Store` is not evidence
//! that a restart would find it, and a checkpoint written by a store handle that
//! never got dropped is not evidence of durability.
//!
//! So these tests cross a real process boundary. A child process is spawned,
//! drives a real Orqynn workflow against a real SQLite file, and is then killed
//! with [`Child::kill`] — which on Windows is `TerminateProcess`, an immediate
//! uncatchable termination. No destructor runs, no graceful shutdown path runs,
//! and the child never gets to write anything afterwards. A *second* child
//! process is then spawned against the same database file and has to
//! reconstruct the state from what survived on disk alone.
//!
//! # The harness
//!
//! Rather than add a `[[bin]]` target or a new crate, this file re-invokes its
//! own test binary. [`std::env::current_exe`] is the `process_kill` executable,
//! and the parent runs it with `--ignored --exact child_harness`, so the child
//! entry point is libtest code that cargo never runs on its own.
//! [`child_harness`] is `#[ignore]`d, which is honest: it is not a test, it is a
//! role-playing program that only does anything when one of the environment
//! variables below is set. When cargo runs this file directly it does nothing
//! and is reported as ignored.
//!
//! The two roles are:
//!
//! - `phase` — drive the workflow to a named kill point, write a **marker** file
//!   recording the durable facts at that instant, then park until it is killed.
//! - `recover` — a *fresh* process against the same database. Reconstruct the
//!   task, the checkpoint lineage, and the recovery package; write a **report**
//!   file; then continue the workflow using the real APIs.
//!
//! # What the marker is and is not
//!
//! The marker is written by the process that is about to die, *after* its last
//! durable step, and it is that process's own claim about what it left behind.
//! It is never the source of truth. The truth is what the second process reads
//! out of the database. The marker exists so the parent can assert "the state a
//! fresh process reads back equals the state the dying process said it had
//! written" — and if a kill had lost a committed transaction, the two would
//! disagree and the test would fail.
//!
//! # Kill points
//!
//! Three, chosen from the store's actual transaction boundaries rather than from
//! convenience. Each is a point where the workflow has genuinely committed
//! something and has genuinely not yet done the next thing.
//!
//! | Kill point | Durable when the process dies | Not yet happened |
//! |---|---|---|
//! | [`KillPoint::CheckpointPublished`] | an OBSERVE round that saw a real change published a checkpoint, advancing the task's lineage | no agent progress, no completion report, no verification |
//! | [`KillPoint::AssignmentPersisted`] | a handoff: assignment active, task `in_progress`, agent holding it | the agent never began, never reported, nothing was verified |
//! | [`KillPoint::AgentReportRecorded`] | a simulated agent reported finishing: `verification_pending`, tenure released, session closed | **independent verification** |
//!
//! The third is the one that matters most. It is the state where a system that
//! trusted the agent would call the task finished. These tests assert it is not —
//! and then assert that a replacement process can finish the job properly,
//! reaching `done` through verification rather than through the claim.
//!
//! Each test asserts the marker for its own kill point matches what it claims
//! (checkpoint count, assignment count), so a scenario can never silently borrow
//! another's state and report a pass.
//!
//! # What durability is actually guaranteed here
//!
//! `connection.rs` sets `journal_mode = WAL` and `synchronous = NORMAL`. Under
//! that pairing SQLite does not fsync on commit; bytes reach the operating system
//! and the WAL, and the OS is responsible for getting them to the platter. The
//! consequence, stated precisely:
//!
//! - **A process being killed is safe.** The OS retains writes made through a
//!   terminated process's file handles, so every committed transaction is
//!   recoverable. That is the guarantee these tests demonstrate, and they
//!   demonstrate it by terminating rather than by asserting.
//! - **A machine losing power is not guaranteed.** Under `synchronous = NORMAL` a
//!   commit that has not reached the platter can be lost. SQLite's own
//!   documentation recommends `FULL` when that matters. Changing it is a
//!   production decision about write throughput and is out of scope here; this
//!   file records the boundary rather than pretending it away.
//!
//! # Boundaries deliberately not touched
//!
//! - **The concurrent-assignment race** (`docs/PHASE13-CONFLICT-DETECTION.md`
//!   §7.6) is not fixed here. These tests are strictly sequential: one writing
//!   process at a time, and the recovery process only reads and then continues
//!   after the writer is dead. The race must be resolved — the `IMMEDIATE`
//!   transaction documented in `PHASE13_AUDIT_NOTE.md` §6 — before concurrent
//!   assignment is enabled, and nothing here is evidence that it is safe.
//! - **`ContinuationPackage` now carries `scope_paths`.** The package reads the
//!   scope from the persisted task, so a recovered briefing can tell a
//!   replacement agent which paths are its own. The recovery reports below
//!   deliberately do *not* assert it: these tests fix the durability of the
//!   workflow, and scope correctness is covered where the rule lives, in
//!   `recovery_builder.rs` and the domain's serde tests.
//! - **No daemon, CLI, or MCP server.** The harness is libtest.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use director_adapters::executor::LocalExecutor;
use director_adapters::git::GitService;
use director_adapters::InMemoryProvider;
use director_app::plan::{PlanRequest, TaskSpec};
use director_app::recovery::{self, Recovery};
use director_app::runner::Runner;
use director_domain::agent::{Agent, Harness};
use director_domain::capability::Capability;
use director_domain::ids::{
    AgentId, AssignmentId, MachineId, PlanId, ProjectId, RepositoryId, TaskId,
};
use director_domain::project::Project;
use director_domain::task::{ExpectedOutput, TaskStatus};
use director_domain::{
    AgentRepository, AssignmentRepository, CheckpointRepository, ProjectRepository, StoreError,
    TaskRepository,
};
use director_store::Store;
use serde_json::{json, Value};

const ROLE: &str = "ORQYNN_PROCESS_KILL_ROLE";
const DB_PATH: &str = "ORQYNN_PROCESS_KILL_DB";
const REPO_DIR: &str = "ORQYNN_PROCESS_KILL_REPO";
const STATE_DIR: &str = "ORQYNN_PROCESS_KILL_STATE";
const MARKER_PATH: &str = "ORQYNN_PROCESS_KILL_MARKER";
const REPORT_PATH: &str = "ORQYNN_PROCESS_KILL_REPORT";
const KILL_POINT: &str = "ORQYNN_PROCESS_KILL_POINT";

/// Environment for the `assign` role: which task, which agent, which assignment
/// id, where to record the outcome, and where the cross-process rendezvous lives.
///
/// These reuse the same `ORQYNN_PROCESS_KILL_` prefix rather than inventing a
/// second namespace, so there is one place to look for what a child process was
/// asked to do.
const CHILD_TASK: &str = "ORQYNN_PROCESS_KILL_TASK";
const CHILD_AGENT: &str = "ORQYNN_PROCESS_KILL_AGENT";
const CHILD_ASSIGNMENT: &str = "ORQYNN_PROCESS_KILL_ASSIGNMENT";
const CHILD_RESULT: &str = "ORQYNN_PROCESS_KILL_RESULT";
const CHILD_GATE: &str = "ORQYNN_PROCESS_KILL_GATE";

/// How long a racing child waits for its partner to reach the rendezvous, and
/// how long the parent waits for a child to finish.
const GATE_TIMEOUT: Duration = Duration::from_secs(60);
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

/// A child that gave up waiting for its partner, so the parent can tell a
/// synchronization failure apart from a genuine assignment outcome.
const GATE_TIMEOUT_EXIT_CODE: i32 = 4;
const TASK_ID: &str = "AUTH";

const PROJECT: &str = "PROJ-1";
const REPO: &str = "REPO-1";

/// A check that passes on any machine with git installed.
const PASSES: &str = "git --version";

/// How long the parent waits for a child to reach its kill point. Generous,
/// because a cold run may be competing for the disk.
const REACH_TIMEOUT: Duration = Duration::from_secs(90);

/// The child parks until it is killed, but not forever: if a parent ever fails
/// to kill it, the child exits with a distinctive code instead of hanging a test
/// run indefinitely. A parent that sees this code fails loudly.
const PARK_LIMIT: Duration = Duration::from_secs(120);
const PARK_EXIT_CODE: i32 = 3;

/// Where in the workflow the process is terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillPoint {
    /// An OBSERVE round published a checkpoint for an in-flight task.
    CheckpointPublished,
    /// A handoff is durable: assignment active, task in progress.
    AssignmentPersisted,
    /// A simulated agent reported finishing; verification has not run.
    AgentReportRecorded,
}

impl KillPoint {
    fn name(self) -> &'static str {
        match self {
            KillPoint::CheckpointPublished => "after_checkpoint",
            KillPoint::AssignmentPersisted => "after_assignment",
            KillPoint::AgentReportRecorded => "after_agent_report",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw {
            "after_checkpoint" => KillPoint::CheckpointPublished,
            "after_assignment" => KillPoint::AssignmentPersisted,
            "after_agent_report" => KillPoint::AgentReportRecorded,
            other => panic!("unknown kill point {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The child harness
// ---------------------------------------------------------------------------

/// The child process's entry point.
///
/// Ignored by cargo, because on its own it does nothing: it waits for
/// `ORQYNN_PROCESS_KILL_ROLE` and returns. The parent spawns this binary with
/// `--ignored --exact child_harness` and the role set, which is what turns it
/// into a program.
///
/// Roles are distinguished by an environment variable rather than by argv so a
/// stray manual invocation cannot accidentally start writing to a database.
#[test]
#[ignore = "child-process harness, spawned explicitly by the parent tests"]
fn child_harness() {
    let Ok(role) = std::env::var(ROLE) else {
        // Reached when cargo runs this file directly. Nothing to do.
        return;
    };
    match role.as_str() {
        "phase" => run_phase_child(),
        "recover" => run_recover_child(),
        "assign" => run_assign_child(),
        other => panic!("unknown role {other:?}"),
    }
}

/// A process attempting exactly one assignment, against a database another
/// process is contending for.
///
/// Unlike the `phase` role this one does not park to be killed: it takes part in
/// a race, records what happened, and exits on its own. The parent enforces the
/// timeout instead.
///
/// The rendezvous is a directory, not a timer. Each child drops a `ready-*` file
/// and then waits until both are present, so neither process can be so far ahead
/// that the other arrives after the transaction has already committed. That makes
/// the two `BEGIN IMMEDIATE` calls land close together without anyone sleeping for
/// an arbitrary duration and hoping.
fn run_assign_child() {
    let db = PathBuf::from(required_env(DB_PATH));
    let task = TaskId::from_string(required_env(CHILD_TASK));
    let agent = AgentId::from_string(required_env(CHILD_AGENT));
    let assignment = AssignmentId::from_string(required_env(CHILD_ASSIGNMENT));
    let result_path = PathBuf::from(required_env(CHILD_RESULT));
    let gate_dir = PathBuf::from(required_env(CHILD_GATE));

    // Announce arrival, then wait for the partner. If the partner never comes the
    // outcome is recorded as a synchronization failure, so a broken rendezvous can
    // never be mistaken for "the guard refused the loser".
    std::fs::write(gate_dir.join(format!("ready-{assignment}")), b"ready")
        .expect("the ready marker is written");

    let deadline = Instant::now() + GATE_TIMEOUT;
    while ready_markers(&gate_dir) < 2 {
        if Instant::now() > deadline {
            let _ = std::fs::write(&result_path, json!({"outcome": "gate_timeout"}).to_string());
            std::process::exit(GATE_TIMEOUT_EXIT_CODE);
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");

    let outcome = runtime.block_on(async move {
        let store = Store::open(&db).await.expect("the store opens");
        match store
            .assign_and_start(&task, &agent, &assignment, stamp(0))
            .await
        {
            Ok(_) => json!({"outcome": "ok"}),
            Err(StoreError::ScopeConflict {
                task,
                holder,
                overlap,
                undeclared,
            }) => json!({
                "outcome": "scope_conflict",
                "task": task.to_string(),
                "holder": holder.to_string(),
                "overlap": overlap,
                "undeclared": undeclared,
            }),
            // Anything else is recorded verbatim so the parent can tell a
            // structured refusal from a lock error or a missing row. A child that
            // lost the race should never report this arm; if it does, the test
            // fails loudly rather than quietly counting it as a refusal.
            Err(other) => json!({"outcome": "other", "error": format!("{other:?}")}),
        }
    });

    std::fs::write(&result_path, outcome.to_string()).expect("the result is written");
}

/// How many children have announced themselves at the rendezvous.
fn ready_markers(gate_dir: &Path) -> usize {
    std::fs::read_dir(gate_dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("ready-"))
                .count()
        })
        .unwrap_or(0)
}

/// The dying process: drive the workflow to the kill point, announce what it
/// left behind, and wait to be terminated.
fn run_phase_child() {
    let db = PathBuf::from(required_env(DB_PATH));
    let repo_dir = PathBuf::from(required_env(REPO_DIR));
    let state_dir = PathBuf::from(required_env(STATE_DIR));
    let marker_path = PathBuf::from(required_env(MARKER_PATH));
    let kill_point = KillPoint::parse(&required_env(KILL_POINT));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");

    let marker = runtime.block_on(async move {
        let store = Store::open(&db).await.expect("the store opens");
        let git = GitService::new(&state_dir).expect("the git service");
        let memory = InMemoryProvider::new();
        let executor = LocalExecutor::default();

        store
            .agents()
            .register_agent(&Agent::register(
                AgentId::from_string("AGENT-1"),
                "AGENT-1",
                Harness::ClaudeCode,
                MachineId::from_string("MACH-1"),
                vec![Capability::Coding],
            ))
            .await
            .expect("the agent registers");

        let project = ProjectId::from_string(PROJECT);
        let repository = RepositoryId::from_string(REPO);
        let task = TaskId::from_string(TASK_ID);
        let repo_path = path_string(&repo_dir);

        let mut runner = Runner::new(&store, &git, &memory, &executor, &repo_dir);

        runner
            .observe(&project, &repository, &repo_path, stamp(1))
            .await
            .expect("the first observation");

        runner
            .plan(
                PlanRequest {
                    project_id: project.clone(),
                    plan_id: PlanId::from_string("PLAN-1"),
                    objective: "make the fixture verifiable".into(),
                    rationale: "the process-kill harness's smallest path".into(),
                    tasks: vec![{
                        let mut spec =
                            TaskSpec::new(task.clone(), "auth", "the endpoint answers");
                        spec.expected_outputs = vec![ExpectedOutput {
                            criterion: "the endpoint answers".into(),
                            check: Some(PASSES.into()),
                        }];
                        spec
                    }],
                    authorized_by: AgentId::from_string("AGENT-planner"),
                },
                stamp(2),
            )
            .await
            .expect("the plan lands");

        let scheduled = runner
            .schedule(&project, stamp(3))
            .await
            .expect("the scheduling round");
        let assignment = scheduled
            .assignment_for(&task)
            .expect("the task was handed out")
            .clone();

        // Stop at exactly the boundary named in the module docs, and nothing
        // further.
        if kill_point == KillPoint::CheckpointPublished {
            // A checkpoint is published only when OBSERVE records a *real
            // change* while work is in flight, so the repository has to actually
            // move between two observations.
            commit(
                &repo_dir,
                "src/auth.rs",
                "// work in progress\n",
                "start the work",
            );
            let observed = runner
                .observe(&project, &repository, &repo_path, stamp(4))
                .await
                .expect("an observation that records a change");
            assert_eq!(
                observed.checkpoints_written, 1,
                "the kill point is defined by a published checkpoint and there is not one"
            );
        }

        if kill_point == KillPoint::AgentReportRecorded {
            runner
                .simulate_agent_turn(&assignment, stamp(5))
                .await
                .expect("the simulated agent turn");
        }

        // Read the durable facts back out of the store: this is the dying
        // process's own account of what it committed.
        let lineage = store
            .checkpoints()
            .checkpoints_for_task(&task)
            .await
            .expect("the checkpoint lineage reads");
        let stored = store
            .tasks()
            .get_task(&task)
            .await
            .expect("the task reads");
        let history = store
            .assignments()
            .assignment_history(&task)
            .await
            .expect("the assignment history reads");

        json!({
            "kill_point": kill_point.name(),
            "task_id": TASK_ID,
            "task_status": status_json(stored.status),
            "task_state_version": stored.state_version,
            "checkpoint_count": lineage.len(),
            "checkpoint_state_versions": lineage.iter().map(|c| c.state_version).collect::<Vec<_>>(),
            "checkpoint_statuses": lineage.iter().map(|c| to_value(&c.status)).collect::<Vec<_>>(),
            "assignment_history_len": history.len(),
            "assignment_id": assignment.to_string(),
        })
    });

    // Written only after the last durable step has returned. The parent polls
    // for this file, so its existence is the proof the kill point was reached.
    std::fs::write(&marker_path, marker.to_string()).expect("the marker is written");

    // Park. The parent terminates this process; nothing below ever runs.
    let deadline = Instant::now() + PARK_LIMIT;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::process::exit(PARK_EXIT_CODE);
}

/// The replacement process: a fresh handle on the same database, reconstructing
/// everything from what survived.
fn run_recover_child() {
    let db = PathBuf::from(required_env(DB_PATH));
    let repo_dir = PathBuf::from(required_env(REPO_DIR));
    let state_dir = PathBuf::from(required_env(STATE_DIR));
    let report_path = PathBuf::from(required_env(REPORT_PATH));
    let kill_point = KillPoint::parse(&required_env(KILL_POINT));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");

    let report = runtime.block_on(async move {
        let store = Store::open(&db).await.expect("the store opens");
        let git = GitService::new(&state_dir).expect("the git service");
        let memory = InMemoryProvider::new();
        let executor = LocalExecutor::default();
        let task = TaskId::from_string(TASK_ID);
        let project = ProjectId::from_string(PROJECT);

        // 1. The task, read from persisted state. Nothing in memory survived.
        let stored = store
            .tasks()
            .get_task(&task)
            .await
            .expect("the task survived");

        // 2. The checkpoint lineage, oldest first.
        let lineage = store
            .checkpoints()
            .checkpoints_for_task(&task)
            .await
            .expect("the checkpoint lineage survived");

        // 3. The recovery package, through the production API.
        let package =
            recovery::recovery_context_from_store(&store, &task).await.expect("recovery builds");

        // 4. Recovery again. `recovery_context_from_store` is read-only, so a
        //    second build must describe identical state: re-running recovery
        //    must not advance, duplicate, or corrupt the lineage.
        let rebuilt =
            recovery::recovery_context_from_store(&store, &task).await.expect("recovery rebuilds");
        let stable = packages_agree(package.as_ref(), rebuilt.as_ref());

        let history = store
            .assignments()
            .assignment_history(&task)
            .await
            .expect("the assignment history reads");

        // 5. Continue the workflow on the recovered state, through the real
        //    APIs. This is what separates "the file still opens" from "the loop
        //    can pick up where it left off".
        let mut runner = Runner::new(&store, &git, &memory, &executor, &repo_dir);
        let verified = runner
            .verify(&project)
            .await
            .expect("a verification round on the recovered state");
        let status_after_verification = store
            .tasks()
            .get_task(&task)
            .await
            .expect("the task reads after continuing")
            .status;

        json!({
            "kill_point": kill_point.name(),
            "task_id": TASK_ID,
            "task_status": status_json(stored.status),
            "task_state_version": stored.state_version,
            "checkpoint_count": lineage.len(),
            "checkpoint_state_versions": lineage.iter().map(|c| c.state_version).collect::<Vec<_>>(),
            "checkpoint_statuses": lineage.iter().map(|c| to_value(&c.status)).collect::<Vec<_>>(),
            "latest_next_action": lineage.last().map(|c| c.next_action.clone()),
            "latest_progress": lineage.last().map(|c| c.progress.clone()),
            "recovery_present": package.is_some(),
            "resume_status": package.as_ref().map(|p| to_value(&p.resume_status())),
            "recovery_next_action": package.as_ref().map(|p| p.next_action().to_string()),
            "recovery_tenures": package.as_ref().map(|p| p.package.tenures.len()).unwrap_or(0),
            "recovery_verifications": package.as_ref().map(|p| p.package.verifications.len()).unwrap_or(0),
            "recovery_transitions": package.as_ref().map(|p| p.package.transitions.len()).unwrap_or(0),
            "recovery_decisions": package.as_ref().map(|p| p.package.decisions.len()).unwrap_or(0),
            "recovery_actions_available": package.as_ref().map(|p| p.actions_available()).unwrap_or(false),
            "recovery_gaps_clean": package.as_ref().map(|p| p.gaps.is_clean()).unwrap_or(false),
            "recovery_stable_across_rebuilds": stable,
            "assignment_history_len": history.len(),
            "judged_on_continue": verified.judged.len(),
            "verdicts": verified.judged.iter().map(|j| j.verdict.clone()).collect::<Vec<_>>(),
            "status_after_verification": status_json(status_after_verification),
        })
    });

    std::fs::write(&report_path, report.to_string()).expect("the report is written");
}

/// Whether two independently built recovery packages describe the same state.
fn packages_agree(first: Option<&Recovery>, second: Option<&Recovery>) -> bool {
    match (first, second) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.resume_status() == b.resume_status()
                && a.next_action() == b.next_action()
                && a.checkpoint().state_version == b.checkpoint().state_version
                && a.package.task_state_version == b.package.task_state_version
                && a.package.tenures.len() == b.package.tenures.len()
                && a.package.verifications.len() == b.package.verifications.len()
                && a.package.transitions.len() == b.package.transitions.len()
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Parent-side harness
// ---------------------------------------------------------------------------

/// The base command for any child harness: this binary, the `child_harness` test,
/// and the environment every role shares.
fn harness_command(role: &str, db: &Path, repo_dir: &Path, state_dir: &Path) -> Command {
    let mut command = Command::new(current_exe());
    command
        .arg("--ignored")
        .arg("--exact")
        .arg("child_harness")
        .arg("--nocapture")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env(ROLE, role)
        .env(DB_PATH, path_string(db))
        .env(REPO_DIR, path_string(repo_dir))
        .env(STATE_DIR, path_string(state_dir));
    command
}

/// Wait for a child, killing it if it overruns.
///
/// The `assign` role exits on its own, so this is a safety net rather than the
/// normal path. Without it a wedged child would hang the whole test run instead
/// of failing it — and a killed child is reported as such, never as a pass.
fn wait_with_timeout(mut child: Child, label: &str) -> std::process::ExitStatus {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) => {}
            Err(err) => panic!("waiting for the {label} child failed: {err}"),
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the {label} child did not finish within {CHILD_TIMEOUT:?} and was killed");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Everything a child process needs, assembled in a temporary directory.
struct KillHarness {
    db: PathBuf,
    repo_dir: PathBuf,
    state_dir: PathBuf,
    marker: PathBuf,
    report: PathBuf,
    _tmp: tempfile::TempDir,
}

impl KillHarness {
    fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("a temp dir");
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("repo dir");
        let state_dir = tmp.path().join("git-state");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let marker = tmp.path().join("marker.json");
        let report = tmp.path().join("report.json");

        init_repo(&repo_dir);
        commit(&repo_dir, "README.md", "# fixture\n", "initial");

        KillHarness {
            db: tmp.path().join("orqyn.db"),
            repo_dir,
            state_dir,
            marker,
            report,
            _tmp: tmp,
        }
    }

    /// Spawn the child harness in the given role, with extra environment.
    fn spawn(&self, role: &str, kill_point: KillPoint, extra: &[(&str, &str)]) -> Child {
        let mut command = harness_command(role, &self.db, &self.repo_dir, &self.state_dir);
        command.env(MARKER_PATH, path_string(&self.marker));
        command.env(REPORT_PATH, path_string(&self.report));
        command.env(KILL_POINT, kill_point.name());
        for (key, value) in extra {
            command.env(key, value);
        }
        command.spawn().expect("the child process spawns")
    }

    /// Run the dying process to `kill_point`, then terminate it forcibly.
    ///
    /// Returns the marker the child wrote at the kill point. Panics rather than
    /// returning if the child never reached the point or died early — a kill
    /// point that was not reached must never be reported as tested.
    fn kill_at(&self, kill_point: KillPoint) -> Value {
        let mut child = self.spawn("phase", kill_point, &[]);

        let deadline = Instant::now() + REACH_TIMEOUT;
        let mut reached = false;
        while Instant::now() < deadline {
            if self.marker.exists() {
                reached = true;
                break;
            }
            if let Some(status) = child.try_wait().expect("the child is polled") {
                panic!(
                    "the child exited before reaching the {kill_point:?} kill point \
                     (status {status}); that kill point was never reached, so nothing \
                     about it has been tested"
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            reached,
            "the child never reached the {kill_point:?} kill point within {REACH_TIMEOUT:?}; \
             this kill point is untested, not passing"
        );

        // The real thing: an immediate, uncatchable termination. No destructor,
        // no shutdown hook, no graceful path.
        child.kill().expect("the child is terminated");
        let status = child.wait().expect("the child is reaped");
        assert!(
            !status.success(),
            "the child exited cleanly ({status}) instead of dying from the kill, so the \
             process ran to completion and this is not a kill test"
        );
        assert_ne!(
            status.code(),
            Some(PARK_EXIT_CODE),
            "the child reached its park limit and exited on its own; the parent did not kill it"
        );

        serde_json::from_str(&std::fs::read_to_string(&self.marker).expect("the marker reads"))
            .expect("the marker is valid JSON")
    }

    /// Spawn a *fresh* process against the same database and read its report.
    fn recover_in_new_process(&self, kill_point: KillPoint) -> Value {
        assert!(
            !self.report.exists(),
            "a report exists before the recovery process ran, so this is not measuring recovery"
        );
        let mut child = self.spawn("recover", kill_point, &[]);
        let status = child.wait().expect("the recovery child is reaped");
        assert!(
            status.success(),
            "the recovery process failed ({status}); its output was discarded by the harness"
        );
        serde_json::from_str(&std::fs::read_to_string(&self.report).expect("the report exists"))
            .expect("the report is valid JSON")
    }
}

fn current_exe() -> PathBuf {
    std::env::current_exe().expect("this test binary's path")
}

fn required_env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("{key} must be set for the child harness"))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// A deterministic instant, so the child's writes are reproducible.
fn stamp(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_760_000_000 + seconds, 0).expect("a fixed instant")
}

fn status_json(status: TaskStatus) -> Value {
    to_value(&status)
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("the value serializes")
}

/// Create a git repository with an identity, so commits can be made at all.
fn init_repo(dir: &Path) {
    let git = || {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir);
        cmd
    };
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
}

/// Write a file and commit it.
fn commit(dir: &Path, path: &str, contents: &str, message: &str) {
    let file = dir.join(path);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    std::fs::write(&file, contents).expect("write file");
    let git = || {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir);
        cmd
    };
    git().args(["add", path]).status().expect("git add");
    let status = git()
        .args(["commit", "-q", "-m", message])
        .status()
        .expect("git commit");
    assert!(status.success(), "git commit should succeed");
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

#[test]
fn a_kill_after_a_checkpoint_publish_preserves_the_lineage_and_the_work() {
    let harness = KillHarness::new();

    // The dying process.
    let marker = harness.kill_at(KillPoint::CheckpointPublished);
    assert_eq!(marker["kill_point"], "after_checkpoint");
    assert_eq!(
        marker["task_status"], "in_progress",
        "the task was mid-work when the process died"
    );
    assert_eq!(
        marker["checkpoint_count"], 1,
        "this kill point is defined by a published checkpoint"
    );

    // A brand new process, reconstructing from the same file.
    let report = harness.recover_in_new_process(KillPoint::CheckpointPublished);

    // The dying process's account and the fresh process's reading must agree on
    // everything durable. This is the assertion that a committed transaction
    // survived an uncatchable termination.
    for field in [
        "task_status",
        "task_state_version",
        "checkpoint_count",
        "checkpoint_state_versions",
        "checkpoint_statuses",
        "assignment_history_len",
    ] {
        assert_eq!(
            report[field], marker[field],
            "{field} differs between what the dying process committed and what a fresh \
             process read back, so a committed transaction did not survive the kill"
        );
    }

    assert_eq!(report["task_status"], "in_progress");
    assert_eq!(
        report["checkpoint_count"], 1,
        "the lineage survived and was not duplicated"
    );
    assert_eq!(
        report["checkpoint_statuses"],
        json!(["current"]),
        "the published checkpoint is on record"
    );
    assert_eq!(
        report["recovery_present"], true,
        "an in-flight task with a checkpoint has a briefing to hand a replacement"
    );
    assert_eq!(
        report["recovery_tenures"], 1,
        "the briefing carries the tenure the dead process left behind"
    );
    assert_eq!(
        report["recovery_verifications"], 0,
        "nothing was verified, so nothing is inferred as verified"
    );
    assert!(
        (report["recovery_transitions"].as_u64().unwrap_or(0)) >= 1,
        "the briefing carries the task's lifecycle transitions, so a replacement can see \
         how the task got to where it is"
    );
    assert_eq!(
        report["recovery_stable_across_rebuilds"], true,
        "re-running recovery must not advance or corrupt the lineage"
    );
    assert!(
        !report["latest_next_action"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "the checkpoint carried a next action and it survived"
    );

    // Continuing from recovered state: verification has nothing to judge, so
    // the task is left exactly where the kill found it.
    assert_eq!(
        report["judged_on_continue"], 0,
        "an in-progress task is not awaiting a verdict"
    );
    assert_eq!(report["status_after_verification"], "in_progress");
}

#[test]
fn a_kill_after_an_assignment_preserves_the_handoff_without_inventing_progress() {
    let harness = KillHarness::new();

    let marker = harness.kill_at(KillPoint::AssignmentPersisted);
    assert_eq!(marker["task_status"], "in_progress");
    assert_eq!(
        marker["assignment_history_len"], 1,
        "the handoff is what this kill point is about"
    );
    assert_eq!(
        marker["checkpoint_count"], 0,
        "no observation recorded a change, so no checkpoint exists to claim"
    );

    let report = harness.recover_in_new_process(KillPoint::AssignmentPersisted);

    for field in [
        "task_status",
        "task_state_version",
        "assignment_history_len",
        "checkpoint_count",
    ] {
        assert_eq!(
            report[field], marker[field],
            "{field} did not survive the kill"
        );
    }

    assert_eq!(
        report["task_status"], "in_progress",
        "the task was still in progress when the process died"
    );
    assert_eq!(
        report["assignment_history_len"], 1,
        "the assignment the dead process wrote is still on record"
    );

    // What must NOT be inferred: the agent never began. A handoff is not work.
    assert_eq!(
        report["recovery_verifications"], 0,
        "a handoff produced no verdict and none is invented"
    );
    assert_eq!(
        report["recovery_present"], false,
        "with no checkpoint there is nothing to resume from, and that is None rather \
         than an empty package"
    );
    assert_eq!(report["judged_on_continue"], 0);
    assert_eq!(report["status_after_verification"], "in_progress");
}

#[test]
fn a_kill_after_an_agent_report_does_not_become_verified_completion() {
    let harness = KillHarness::new();

    let marker = harness.kill_at(KillPoint::AgentReportRecorded);
    assert_eq!(
        marker["task_status"], "verification_pending",
        "the agent reported finishing and the task stopped exactly there"
    );
    assert_eq!(
        marker["checkpoint_count"], 0,
        "this kill point publishes no checkpoint"
    );

    let report = harness.recover_in_new_process(KillPoint::AgentReportRecorded);

    assert_eq!(
        report["task_status"], marker["task_status"],
        "the recovered status matches what the dying process committed"
    );

    // The central claim of this file. The agent said it was finished, the
    // process was killed, and a fresh process must not conclude the work is
    // done.
    assert_eq!(
        report["task_status"], "verification_pending",
        "an agent report is not a completion, and a restart must not promote it to one"
    );
    assert_eq!(
        report["recovery_verifications"], 0,
        "nothing was verified before the kill, and a restart does not invent a verdict"
    );
    assert_eq!(
        report["judged_on_continue"], 1,
        "the recovered process finds exactly one task awaiting a verdict"
    );
    assert_eq!(
        report["verdicts"],
        json!(["passed"]),
        "and it reaches that verdict by running the check, not by trusting the agent"
    );

    // Continuation: the replacement process finishes the job the dead one
    // started, through the real APIs, and only verification is what gets there.
    assert_eq!(
        report["status_after_verification"], "done",
        "a replacement process can pick the workflow up and verify it to done"
    );
}

#[test]
fn a_killed_process_does_not_reach_its_own_recovery_path() {
    // A guard on the harness itself, because the whole file is worthless if the
    // child quietly finished its work before being killed.
    let harness = KillHarness::new();
    let marker = harness.kill_at(KillPoint::AgentReportRecorded);

    assert!(
        !harness.report.exists(),
        "the recovery report exists even though no recovery process was spawned, which \
         would mean the killed child ran recovery itself"
    );
    assert_eq!(
        marker["task_status"], "verification_pending",
        "the kill point is reached before verification, not after"
    );
    assert!(
        marker.get("recovery_present").is_none(),
        "the marker has no recovery field at all, which is the point: the dying process \
         never built a recovery package, so nothing it reports can be a recovered value"
    );
    assert_eq!(
        marker["assignment_history_len"], 1,
        "the tenure the dead process wrote is on record and nothing was cleaned up"
    );
}

#[test]
fn the_harness_actually_terminates_the_child() {
    // Proves the kill mechanism itself, isolated from any workflow claim: a
    // child is terminated and must not report success, or no kill test in this
    // file means anything.
    let harness = KillHarness::new();
    let mut child = harness.spawn("phase", KillPoint::AssignmentPersisted, &[]);
    std::thread::sleep(Duration::from_millis(100));

    let terminated = child.kill().is_ok();
    let status = child.wait().expect("the child is reaped");
    assert!(
        terminated || !status.success(),
        "the child could not be terminated and did not fail on its own"
    );
    assert!(
        !status.success(),
        "a killed child must not report success, or the kill proves nothing"
    );
}

// ---------------------------------------------------------------------------
// Cross-process scope-conflict safety
// ---------------------------------------------------------------------------

/// A parent-side fixture for the cross-process assignment race: one shared
/// SQLite file, two agents, and a rendezvous directory the two children meet in.
///
/// This reuses the child-process machinery above — the same binary, the same
/// `--ignored --exact child_harness` entry point, the same environment-driven
/// role dispatch — rather than standing up a second harness. What is new here is
/// only the fixture: no git repository, because the race is about the assignment
/// transaction and has nothing to do with OBSERVE.
struct RaceHarness {
    db: PathBuf,
    gate_dir: PathBuf,
    spare_dir: PathBuf,
    project: ProjectId,
    _tmp: tempfile::TempDir,
}

impl RaceHarness {
    async fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("a temp dir");
        let db = tmp.path().join("orqyn.db");
        let gate_dir = tmp.path().join("gate");
        let spare_dir = tmp.path().join("spare");
        std::fs::create_dir_all(&gate_dir).expect("gate dir");
        std::fs::create_dir_all(&spare_dir).expect("spare dir");

        let store = Store::open(&db).await.expect("store opens");
        let project = ProjectId::from_string("PROJ-RACE");
        store
            .projects()
            .create_project(&Project::new(project.clone(), "proj", "/nowhere"))
            .await
            .expect("project created");

        RaceHarness {
            db,
            gate_dir,
            spare_dir,
            project,
            _tmp: tmp,
        }
    }

    /// Register one agent that can take work.
    async fn agent(&self, store: &Store, id: &str) -> AgentId {
        let agent_id = AgentId::from_string(id);
        let mut agent = Agent::register(
            agent_id.clone(),
            id,
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            vec![Capability::Coding],
        );
        agent.status = director_domain::agent::AgentStatus::Available;
        store
            .agents()
            .register_agent(&agent)
            .await
            .expect("agent registers");
        agent_id
    }

    /// Plan `specs` through the real PLAN step so scopes are persisted, not
    /// injected.
    async fn plan(&self, store: &Store, plan_id: &str, specs: Vec<TaskSpec>) {
        director_app::plan::plan(
            store,
            PlanRequest {
                project_id: self.project.clone(),
                plan_id: PlanId::from_string(plan_id),
                objective: "race the assignment transaction".into(),
                rationale: "the cross-process tests need real scoped tasks".into(),
                tasks: specs,
                authorized_by: AgentId::from_string("AGENT-planner"),
            },
        )
        .await
        .expect("plan succeeds");
    }

    /// Start one `assign` child. Both are started before either is waited on,
    /// which is what makes them concurrent rather than merely adjacent.
    ///
    /// Both children rendezvous in the *same* gate directory, distinguished by
    /// the assignment id in their marker filename. Giving each its own directory
    /// would mean each waits forever for a partner that can never appear.
    fn start_child(&self, label: &str, task: &str, agent: &str, assignment: &str) -> Child {
        let result = self.spare_dir.join(format!("result-{label}.json"));
        let _ = std::fs::remove_file(&result);

        let mut command = harness_command("assign", &self.db, &self.spare_dir, &self.spare_dir);
        command
            .env(CHILD_TASK, task)
            .env(CHILD_AGENT, agent)
            .env(CHILD_ASSIGNMENT, assignment)
            .env(CHILD_RESULT, path_string(&result))
            .env(CHILD_GATE, path_string(&self.gate_dir));
        command.spawn().expect("the assign child spawns")
    }

    /// Clear the rendezvous, so a marker left by an earlier round cannot let a
    /// child skip the wait and race ahead on its own.
    fn reset_gate(&self) {
        let Ok(entries) = std::fs::read_dir(&self.gate_dir) else {
            return;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            if entry.file_name().to_string_lossy().starts_with("ready-") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Read one child's recorded outcome, failing if it never recorded one.
    fn outcome(&self, label: &str) -> Value {
        let path = self.spare_dir.join(format!("result-{label}.json"));
        let deadline = Instant::now() + CHILD_TIMEOUT;
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "the {label} child recorded no outcome within {CHILD_TIMEOUT:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        serde_json::from_str(&std::fs::read_to_string(&path).expect("result readable"))
            .expect("result is valid JSON")
    }
}

/// One task spec with a declared scope.
fn race_task(id: &str, scope: &str) -> TaskSpec {
    TaskSpec::new(TaskId::from_string(id), id, format!("do {id}")).scopes([scope])
}

/// The decisive cross-process proof.
///
/// Two independently spawned OS processes, two connection pools, one SQLite file.
/// Both attempt to take overlapping ground in the same project at the same moment,
/// and the invariant is that **at most one** can win — with the loser receiving a
/// structured scope conflict rather than a lock error or a silent double grant.
///
/// Three rounds with distinct scope paths run in one test, so the property is
/// exercised repeatedly without the rounds interfering: each round's ground is
/// disjoint from every earlier round's, which is still in flight.
#[tokio::test]
async fn two_processes_cannot_both_acquire_overlapping_task_scopes() {
    let fixture = RaceHarness::new().await;
    let store = Store::open(&fixture.db).await.expect("store opens");

    let mut specs = Vec::new();
    for round in 1..=3 {
        specs.push(race_task(
            &format!("R{round}A"),
            &format!("src/round{round}/a"),
        ));
        specs.push(race_task(
            &format!("R{round}B"),
            &format!("src/round{round}/a/inner"),
        ));
    }
    fixture.plan(&store, "PLAN-RACE", specs).await;
    for round in 1..=3 {
        fixture.agent(&store, &format!("AGENT-{round}A")).await;
        fixture.agent(&store, &format!("AGENT-{round}B")).await;
    }

    for round in 1..=3 {
        let label_a = format!("r{round}a");
        let label_b = format!("r{round}b");

        // Both processes are started before either is waited on.
        fixture.reset_gate();
        let first = fixture.start_child(
            &label_a,
            &format!("R{round}A"),
            &format!("AGENT-{round}A"),
            &format!("ASG-{round}-A"),
        );
        let second = fixture.start_child(
            &label_b,
            &format!("R{round}B"),
            &format!("AGENT-{round}B"),
            &format!("ASG-{round}-B"),
        );
        let first_status = wait_with_timeout(first, &label_a);
        let second_status = wait_with_timeout(second, &label_b);

        for (label, status) in [(&label_a, first_status), (&label_b, second_status)] {
            assert_ne!(
                status.code(),
                Some(GATE_TIMEOUT_EXIT_CODE),
                "the {label} child never met its partner at the rendezvous, so this round \
                 proves nothing about the guard"
            );
            assert!(
                status.success(),
                "the {label} child exited with {status}; a child that cannot do its job \
                 is not evidence either way"
            );
        }

        let outcomes = [fixture.outcome(&label_a), fixture.outcome(&label_b)];
        let wins = outcomes.iter().filter(|o| o["outcome"] == "ok").count();
        let conflicts = outcomes
            .iter()
            .filter(|o| o["outcome"] == "scope_conflict")
            .count();

        assert_eq!(
            wins, 1,
            "round {round}: exactly one process may take overlapping ground, got {outcomes:?}"
        );
        assert_eq!(
            conflicts, 1,
            "round {round}: the loser must receive a structured scope conflict, got {outcomes:?}"
        );

        // The conflict names the task that already held the ground, which is the
        // winner. So the holder is the winner and the other task is the loser -
        // the two processes agree about who won rather than one guessing.
        let holder = outcomes
            .iter()
            .find(|o| o["outcome"] == "scope_conflict")
            .map(|o| o["holder"].as_str().unwrap_or_default().to_string())
            .expect("a conflict names its holder");
        assert!(
            !holder.is_empty(),
            "round {round}: the conflict names the holding task"
        );
        let winner = holder;
        let loser_task = if winner.ends_with('A') {
            format!("R{round}B")
        } else {
            format!("R{round}A")
        };
        assert_eq!(
            store
                .tasks()
                .get_task(&TaskId::from_string(&winner))
                .await
                .expect("winner reads")
                .status,
            TaskStatus::InProgress,
            "round {round}: the winner is in flight"
        );
        assert_eq!(
            store
                .tasks()
                .get_task(&TaskId::from_string(&loser_task))
                .await
                .expect("loser reads")
                .status,
            TaskStatus::Todo,
            "round {round}: the loser never started"
        );
        assert!(
            store
                .assignments()
                .active_assignment_for_task(&TaskId::from_string(&loser_task))
                .await
                .expect("loser assignment lookup")
                .is_none(),
            "round {round}: the rejected handoff wrote no assignment row"
        );
        assert_eq!(
            store
                .assignments()
                .assignment_history(&TaskId::from_string(&loser_task))
                .await
                .expect("loser history reads")
                .len(),
            0,
            "round {round}: the rejected handoff left no assignment history at all"
        );
    }
}

/// The control that keeps the guard honest: disjoint scopes across two processes
/// must both succeed. Without this, a test proving "only one wins" would also
/// pass if the guard simply refused everything.
#[tokio::test]
async fn two_processes_can_both_acquire_disjoint_task_scopes() {
    let fixture = RaceHarness::new().await;
    let store = Store::open(&fixture.db).await.expect("store opens");

    fixture
        .plan(
            &store,
            "PLAN-RACE",
            vec![
                race_task("AUTH", "src/auth"),
                race_task("BILLING", "src/billing"),
            ],
        )
        .await;
    fixture.agent(&store, "AGENT-A").await;
    fixture.agent(&store, "AGENT-B").await;

    fixture.reset_gate();
    let first = fixture.start_child("da", "AUTH", "AGENT-A", "ASG-D-A");
    let second = fixture.start_child("db", "BILLING", "AGENT-B", "ASG-D-B");
    let first_status = wait_with_timeout(first, "da");
    let second_status = wait_with_timeout(second, "db");

    for (label, status) in [("da", first_status), ("db", second_status)] {
        assert_ne!(
            status.code(),
            Some(GATE_TIMEOUT_EXIT_CODE),
            "the {label} child never met its partner at the rendezvous"
        );
        assert!(status.success(), "the {label} child exited with {status}");
    }

    let outcomes = [fixture.outcome("da"), fixture.outcome("db")];
    for outcome in &outcomes {
        assert_eq!(
            outcome["outcome"], "ok",
            "disjoint scopes are the case the guard must let through across processes too, \
             got {outcomes:?}"
        );
    }

    for (task, agent) in [("AUTH", "AGENT-A"), ("BILLING", "AGENT-B")] {
        assert_eq!(
            store
                .tasks()
                .get_task(&TaskId::from_string(task))
                .await
                .expect("task reads")
                .status,
            TaskStatus::InProgress
        );
        assert_eq!(
            store
                .assignments()
                .active_assignment_for_task(&TaskId::from_string(task))
                .await
                .expect("assignment lookup")
                .map(|a| a.agent_id.to_string())
                .as_deref(),
            Some(agent),
            "{task} is held by {agent} and nobody else"
        );
    }
}
