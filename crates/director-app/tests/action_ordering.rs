//! Deterministic action ordering when a task's rounds share a timestamp.
//!
//! Every action recorded by one verification round carries that round's single
//! instant, so equal timestamps are the *normal* case, not an edge case. Ordering
//! therefore lives almost entirely in the tie-break.
//!
//! The tie-break used to be the action id, and that was wrong. An action id is
//! `ACT-{verification_id}-{ordinal}` and the counters in it are unpadded, so
//! lexical order over the id does not track time: at one instant, round ten
//! sorted ahead of round two. The same fault appeared *inside* a single round
//! once a task ran eleven or more checks, because the ordinal is unpadded too.
//!
//! The order is now the order the actions were recorded, which for an append-only
//! log is the order they happened. These tests pin that down where the two
//! mechanisms disagree, which is the only place the difference is visible.

use std::path::PathBuf;
use std::process::Command;

use chrono::{DateTime, TimeZone, Utc};
use director_adapters::executor::LocalExecutor;
use director_adapters::git::GitService;
use director_adapters::InMemoryProvider;
use director_app::plan::{PlanRequest, TaskSpec};
use director_app::replan::{Remediation, ReplanRequest, TaskDecision};
use director_app::runner::Runner;
use director_domain::action::{Action, ActionKind};
use director_domain::agent::{Agent, Harness};
use director_domain::capability::Capability;
use director_domain::ids::{ActionId, MachineId, PlanId, ProjectId, RepositoryId, TaskId};
use director_domain::task::ExpectedOutput;
use director_domain::{ActionRepository, AgentRepository};
use director_store::Store;

const PROJECT: &str = "PROJ-1";
const REPO: &str = "REPO-1";
/// Fails, so the task lands in `failed` and REPLAN can retry it — which is how
/// one task accumulates more rounds than the survey set would otherwise allow.
const FAILING: &str = "git --no-such-flag";
const PASSES: &str = "git --version";

fn pinned() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 4, 6, 9, 30, 0).unwrap()
}

fn later(seconds: i64) -> DateTime<Utc> {
    pinned() + chrono::Duration::seconds(seconds)
}

/// The verification round embedded in an action id: the `2` in
/// `ACT-VER-AUTH-2-0`.
///
/// Parsed from the right so a task id containing a hyphen cannot shift the
/// position.
fn round_of(id: &ActionId) -> usize {
    nth_from_right(id, 1)
}

/// The check index within a round: the trailing `0` in `ACT-VER-AUTH-2-0`.
fn ordinal_of(id: &ActionId) -> usize {
    nth_from_right(id, 0)
}

fn nth_from_right(id: &ActionId, offset: usize) -> usize {
    id.as_str()
        .rsplit('-')
        .nth(offset)
        .and_then(|part| part.parse().ok())
        .unwrap_or_else(|| panic!("{id} has no numeric part {offset} from the right"))
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

    async fn prepared(&self) {
        let mut runner = self.runner();
        runner
            .observe(
                &self.project,
                &self.repository,
                self.repo_dir.to_str().expect("a utf-8 repo path"),
                later(-100),
            )
            .await
            .expect("the first observation");
        self.store
            .agents()
            .register_agent(&Agent::register(
                director_domain::ids::AgentId::from_string("AGENT-1"),
                "AGENT-1",
                Harness::ClaudeCode,
                MachineId::from_string("MACH-a"),
                vec![Capability::Coding],
            ))
            .await
            .expect("agent registered");
    }

    /// Create the plan once. Re-planning the same plan id would be refused, and
    /// the retry loop below reuses it twelve times.
    async fn plan_once(
        &self,
        runner: &mut Runner<'_, InMemoryProvider, LocalExecutor>,
        task: &str,
        checks: &[&str],
    ) {
        let task_id = TaskId::from_string(task);
        let mut spec = TaskSpec::new(task_id.clone(), task, format!("do {task}"));
        spec.expected_outputs = checks
            .iter()
            .map(|check| ExpectedOutput {
                criterion: format!("{task} satisfies {check}"),
                check: Some((*check).to_string()),
            })
            .collect();
        runner
            .plan(
                PlanRequest {
                    project_id: self.project.clone(),
                    plan_id: PlanId::from_string("PLAN-1"),
                    objective: "make the fixture verifiable".into(),
                    rationale: "the ordering harness's smallest path".into(),
                    tasks: vec![spec],
                    authorized_by: director_domain::ids::AgentId::from_string("AGENT-planner"),
                },
                later(-90),
            )
            .await
            .expect("the plan lands");
    }

    /// Hand the task to the agent and let the simulated turn report, which is the
    /// only way to reach VERIFY without an agent driver.
    async fn drive_to_awaiting_verdict(
        &self,
        runner: &mut Runner<'_, InMemoryProvider, LocalExecutor>,
        task: &str,
    ) {
        let task_id = TaskId::from_string(task);
        let scheduled = runner
            .schedule(&self.project, later(-80))
            .await
            .expect("the scheduling round");
        let assignment = scheduled
            .assignment_for(&task_id)
            .unwrap_or_else(|| panic!("{task} was handed out; report: {scheduled:?}"))
            .clone();
        runner
            .simulate_agent_turn(&assignment, later(-70))
            .await
            .expect("the simulated turn");
    }

    async fn retry(&self, runner: &mut Runner<'_, InMemoryProvider, LocalExecutor>, task: &str) {
        runner
            .replan(
                ReplanRequest {
                    project_id: self.project.clone(),
                    decisions: vec![TaskDecision {
                        task_id: TaskId::from_string(task),
                        remediation: Remediation::Retry,
                        reason: None,
                    }],
                    decided_by: None,
                },
                later(0),
            )
            .await
            .expect("the retry lands");
    }

    async fn actions(&self, task: &str) -> Vec<Action> {
        self.store
            .actions()
            .actions_for_task(&TaskId::from_string(task))
            .await
            .expect("actions read")
    }

    async fn action_ids(&self, task: &str) -> Vec<String> {
        self.actions(task)
            .await
            .into_iter()
            .map(|a| a.id.to_string())
            .collect()
    }

    /// Run `rounds` verification rounds, each judged at the pinned instant.
    ///
    /// Assumes [`Self::plan_once`] has already run, so a test can write a row
    /// before the first round and control where it lands.
    async fn run_rounds(
        &self,
        runner: &mut Runner<'_, InMemoryProvider, LocalExecutor>,
        task: &str,
        rounds: usize,
    ) {
        for round in 1..=rounds {
            if round > 1 {
                self.retry(runner, task).await;
            }
            self.drive_to_awaiting_verdict(runner, task).await;
            runner
                .verify_at(&self.project, pinned())
                .await
                .expect("the verification round");
        }
    }
}

/// Drive one task through `rounds` verification rounds, every one of them judged
/// at the same pinned instant.
///
/// Returns the ids as they read back.
async fn rounds_at_one_instant(
    fixture: &Fixture,
    task: &str,
    checks: &[&str],
    rounds: usize,
) -> Vec<String> {
    let mut runner = fixture.runner();
    fixture.plan_once(&mut runner, task, checks).await;
    fixture.run_rounds(&mut runner, task, rounds).await;
    fixture.action_ids(task).await
}

// ---------------------------------------------------------------------------
// The 2 / 10 boundary
// ---------------------------------------------------------------------------

/// Twelve rounds judged at one instant come back in the order they happened.
///
/// This is the test that would have caught the lexical tie-break. The ids here
/// run `ACT-VER-AUTH-2-0` through `ACT-VER-AUTH-12-0`, and sorted as text that is
/// `1, 10, 11, 12, 2, 3, …` — round ten before round two, and the sequence is
/// not even monotonic.
#[tokio::test]
async fn twelve_rounds_at_one_instant_read_back_in_verification_order() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;

    let ids = rounds_at_one_instant(&fixture, "AUTH", &[FAILING], 12).await;
    assert_eq!(ids.len(), 12, "one action per round");

    let actions = fixture.actions("AUTH").await;
    assert!(
        actions.iter().all(|a| a.at == pinned()),
        "the premise: every round really was judged at the same instant"
    );

    let rounds: Vec<usize> = actions.iter().map(|a| round_of(&a.id)).collect();
    assert_eq!(
        rounds,
        (1..=12).collect::<Vec<usize>>(),
        "rounds read back in the order they were judged"
    );

    // The boundary itself, stated on its own so a failure says which half broke.
    assert_eq!(round_of(&actions[1].id), 2, "round two is the second row");
    assert_eq!(
        round_of(&actions[9].id),
        10,
        "round ten is the tenth row, not the second"
    );
}

/// The ordering test is not vacuous: lexical order really would disagree.
///
/// Without this the test above could pass for the wrong reason — for instance if
/// the ids were zero-padded, or if the fixture only ever produced one round.
#[tokio::test]
async fn lexical_id_order_would_have_been_wrong() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;

    let ids = rounds_at_one_instant(&fixture, "AUTH", &[FAILING], 12).await;

    let mut lexical = ids.clone();
    lexical.sort();
    assert_ne!(
        ids, lexical,
        "the recorded order and the lexical order agree, so this fixture no longer \
         distinguishes them and the ordering tests prove nothing"
    );
    assert!(
        lexical.starts_with(&ids[..1]) && lexical[1].contains("-10-"),
        "the lexical order is expected to place round ten immediately after round one"
    );
}

// ---------------------------------------------------------------------------
// Stability
// ---------------------------------------------------------------------------

/// Two reads of the same rows give the same order.
///
/// Determinism is the whole contract. It does not matter *why* an order is
/// correct if a briefing can disagree with itself between two reads.
#[tokio::test]
async fn the_order_is_the_same_on_every_read() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;

    let ids = rounds_at_one_instant(&fixture, "AUTH", &[FAILING], 12).await;

    for _ in 0..5 {
        assert_eq!(
            fixture.action_ids("AUTH").await,
            ids,
            "the same rows read twice gave two different orders"
        );
    }
}

/// The order is stored, not remembered.
///
/// The order comes from the database, so closing the store and opening the same
/// file again must reproduce it exactly. A tie-break held in writer memory would
/// satisfy the previous test and fail this one.
#[tokio::test]
async fn the_order_survives_closing_and_reopening_the_store() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;

    let ids = rounds_at_one_instant(&fixture, "AUTH", &[FAILING], 12).await;

    let reopened = Store::open(&fixture.store_path)
        .await
        .expect("store reopens");
    let after = reopened
        .actions()
        .actions_for_task(&TaskId::from_string("AUTH"))
        .await
        .expect("actions read");
    let after: Vec<String> = after.into_iter().map(|a| a.id.to_string()).collect();

    assert_eq!(
        after, ids,
        "the order changed across a reopen, so it was not persisted"
    );
}

// ---------------------------------------------------------------------------
// Within one round
// ---------------------------------------------------------------------------

/// A single round's checks stay in the order the round ran them.
///
/// The id tie-break had this fault too, and it needs no retries to expose: the
/// ordinal is unpadded, so a round running twelve checks sorted as
/// `0, 1, 10, 11, 2, 3, …`.
#[tokio::test]
async fn checks_within_one_round_keep_their_ordinal_order() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;

    let checks: Vec<&str> = std::iter::repeat_n(PASSES, 12).collect();
    let mut runner = fixture.runner();
    fixture.plan_once(&mut runner, "MULTI", &checks).await;
    fixture
        .drive_to_awaiting_verdict(&mut runner, "MULTI")
        .await;
    runner
        .verify_at(&fixture.project, pinned())
        .await
        .expect("the verification round");

    let actions = fixture.actions("MULTI").await;
    assert_eq!(actions.len(), 12, "all twelve checks ran");
    assert!(
        actions.iter().all(|a| a.at == pinned()),
        "the premise: one round, so one instant for every action"
    );

    let ordinals: Vec<usize> = actions.iter().map(|a| ordinal_of(&a.id)).collect();
    assert_eq!(
        ordinals,
        (0..12).collect::<Vec<usize>>(),
        "the round's checks read back in the order the round ran them"
    );

    let ids: Vec<String> = actions.iter().map(|a| a.id.to_string()).collect();
    let mut lexical = ids.clone();
    lexical.sort();
    assert_ne!(
        ids, lexical,
        "lexical order agrees, so twelve checks is no longer enough to tell the \
         two mechanisms apart"
    );
}

// ---------------------------------------------------------------------------
// Legacy and taskless rows
// ---------------------------------------------------------------------------

/// Rows this change knows nothing about stay readable and keep a defined place.
///
/// Two shapes matter. A *legacy* row carries an id from before the derived
/// format, so it has no round and no ordinal to parse. A *taskless* row has no
/// task at all, so it never appears in a task's history and nothing orders it.
///
/// The claim being made is deliberately modest: a legacy row is placed by the
/// order it was written, not by a time anyone established independently.
#[tokio::test]
async fn legacy_and_taskless_rows_stay_readable_and_ordered() {
    let fixture = Fixture::new().await;
    fixture.prepared().await;

    // The task must exist before an action can reference it: `task_actions`
    // carries a foreign key to `tasks`. So the plan lands first, then the row,
    // then the rounds.
    let mut runner = fixture.runner();
    fixture.plan_once(&mut runner, "AUTH", &[FAILING]).await;

    // Written before any round runs, so at equal `at` it precedes them all.
    let legacy = Action {
        task_id: Some(TaskId::from_string("AUTH")),
        at: pinned(),
        ..Action::new(
            ActionId::from_string("ACT-manual-1"),
            ActionKind::ToolCall,
            "a row written before ids were derived",
        )
    };
    fixture
        .store
        .actions()
        .record_action(&legacy)
        .await
        .expect("the legacy row records");

    // No task, so no history and no ordering claim.
    let taskless = Action {
        task_id: None,
        at: pinned(),
        ..Action::new(
            ActionId::from_string("ACT-exploration-1"),
            ActionKind::ToolCall,
            "recorded outside any task",
        )
    };
    fixture
        .store
        .actions()
        .record_action(&taskless)
        .await
        .expect("the taskless row records");

    fixture.run_rounds(&mut runner, "AUTH", 12).await;
    let ids = fixture.action_ids("AUTH").await;

    // Readable by id, unchanged.
    let fetched = fixture
        .store
        .actions()
        .get_action(&ActionId::from_string("ACT-manual-1"))
        .await
        .expect("the legacy row is still readable");
    assert_eq!(
        fetched.summary, legacy.summary,
        "the legacy row kept its content"
    );

    // The taskless row is not part of any task's history.
    assert!(
        !ids.contains(&"ACT-exploration-1".to_string()),
        "a taskless action must not appear in a task's history"
    );

    // The legacy row is placed by when it was written: first among the rows that
    // share this instant. Not by parsing, and not claimed to be a timestamp.
    assert_eq!(
        ids[0], "ACT-manual-1",
        "a row written before the rounds, sharing their instant, reads back first"
    );
    let rounds: Vec<usize> = ids[1..]
        .iter()
        .map(|id| round_of(&ActionId::from_string(id)))
        .collect();
    assert_eq!(
        rounds,
        (1..=12).collect::<Vec<usize>>(),
        "the rounds after it are still in order"
    );
}
