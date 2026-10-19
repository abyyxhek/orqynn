//! The one lifecycle the CLI runs, built entirely from existing application APIs.
//!
//! # Nothing here reimplements the loop
//!
//! Scheduling, assignment, checkpointing, briefing, execution, verification and
//! recovery all belong to `director-app`. This module only decides *which* of them
//! to call, in what order, and what to print. Every state change goes through a
//! function another crate already owns and tests, so a fix made there is fixed
//! here too.
//!
//! # The invariants, and where each one is kept
//!
//! | Invariant | Kept by |
//! |---|---|
//! | SQLite is the authoritative task-state store | [`Store`], used for every read and write below |
//! | An agent's exit status does not establish success | the exit code comes from `classify`, which reads the *verdict* |
//! | Only verification determines the verdict | `verify::verify_at`, reached through the runner |
//! | Verdict and actions persist atomically | the store's single transaction, not this module |
//! | Scope guards are unchanged | `schedule`/`assign`, called untouched |
//! | External memory is advisory | `memory::Retrieval`, rendered as an external copy |
//! | A failure is never reported as success | [`classify`], which cannot return `Success` without a passing verdict |

use std::path::{Path, PathBuf};

use director_adapters::aimemory::adapter::{AiMemoryAdapter, AiMemoryAdapterError};
use director_adapters::aimemory::config::AiMemoryConfig;
use director_adapters::executor::LocalExecutor;
use director_adapters::git::GitService;
use director_adapters::harness::{ClaudeCodeConfig, ClaudeCodeDriver};
use director_adapters::InMemoryProvider;
use director_app::agent::{AgentDrive, DriveOptions};
use director_app::engine;
use director_app::recovery::{RecoveryBuilder, RecoveryOutcome};
use director_app::replan::{self, Condition, ReplanRequest, TaskDecision};
use director_app::runner::{CheckOutcome, DrivenStep, Runner, RunnerError, VerifyStep};
use director_domain::agent::{Agent, Harness};
use director_domain::capability::Capability;
use director_domain::ids::{AgentId, MachineId, PlanId, ProjectId, RepositoryId, TaskId};
use director_domain::providers::{Memory, MemoryProvider, MemoryQuery, Provider};
use director_domain::store::{
    AgentRepository, AssignmentRepository, BlockerRepository, CheckpointRepository,
    HandoffRepository, PlanRepository, ProjectRepository, TaskRepository, VerificationRepository,
};
use director_domain::task::{ExpectedOutput, TaskStatus};
use director_domain::ResumeStatus;
use director_store::Store;

use crate::config::{ConfigError, Remediation as RequestedRemediation, RunArgs};

/// Fixed identities, so a retry addresses the same records rather than making
/// new ones. Not configurable: a second project or agent would be a different
/// system, not a different run.
const PROJECT_ID: &str = "PROJ-1";
const REPOSITORY_ID: &str = "REPO-1";
const AGENT_ID: &str = "AGENT-1";
const MACHINE_ID: &str = "MACH-local";

/// The plan a task is filed under.
///
/// Derived from the task id rather than fixed, for two reasons. A retry of the
/// same task must address the same plan, or it would silently plan the work twice;
/// and a *different* task must get a different plan, or the second task in a
/// database would be refused as if it were a retry of the first.
fn plan_id(task_id: &TaskId) -> PlanId {
    PlanId::from_string(format!("PLAN-{}", task_id.as_str()))
}

/// Exit codes, so a script can tell outcomes apart without parsing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    /// Everything ran and verification passed.
    Success = 0,
    /// The invocation was wrong: a missing flag, a bad path, no check given.
    Configuration = 2,
    /// A step of the loop failed: observation, planning, scheduling, or storage.
    Orchestration = 3,
    /// The agent process could not be started, or did not finish.
    Execution = 4,
    /// Verification ran and did not pass. The task is *not* done.
    VerificationFailed = 5,
    /// The agent ran but Orqyn could not judge the result.
    Unverifiable = 6,
    /// A dry run, which executed nothing and verified nothing.
    DryRun = 7,
    /// The task's state does not permit a safe resume. Distinct from every
    /// failure above on purpose: nothing was attempted, nothing was launched, and
    /// nothing about the agent went wrong. Conflating a refusal with an execution
    /// failure would tell an operator to go and debug their agent when the agent
    /// was never involved.
    UnsafeState = 8,
}

impl ExitCode {
    /// The value a process exits with.
    pub fn status(self) -> i32 {
        self as i32
    }
}

/// Anything that can stop the run.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// Bad invocation.
    #[error("{0}")]
    Config(#[from] ConfigError),
    /// A step of the loop failed.
    #[error("the {step} step failed: {reason}")]
    Orchestration {
        /// Which step.
        step: &'static str,
        /// What it said.
        reason: String,
    },
    /// The store or a service could not be opened.
    #[error("{0}")]
    Setup(String),
}

/// What a run did, for printing.
#[derive(Debug)]
pub struct RunReport {
    /// The exit code the run earned.
    pub exit_code: ExitCode,
    /// Lines describing what happened, in order.
    pub lines: Vec<String>,
}

/// The memory substrate a run uses, chosen once at startup.
///
/// `Runner` is generic over its provider, so "memory on" and "memory off" are two
/// monomorphisations rather than a flag threaded through every call. This enum is
/// the dispatch, and the only place that knows both exist.
///
/// The live adapter is boxed because it is far larger than the in-memory one, and
/// an unboxed pair would make every `CliMemory` — including the local case that
/// holds an empty map — as big as the case nobody uses.
pub enum CliMemory {
    /// No substrate: checkpoints go nowhere and retrieval finds nothing.
    Local(InMemoryProvider),
    /// A live substrate.
    External(Box<AiMemoryAdapter>),
}

/// Errors from either substrate, collapsed so `MemoryProvider` needs one type.
///
/// Deliberately stringly-typed: the CLI reports these and nothing branches on
/// them, and flattening two unrelated error taxonomies into one would be a
/// larger claim than this program can support.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MemoryFailure(pub String);

#[async_trait::async_trait]
impl Provider for CliMemory {
    type Error = MemoryFailure;
}

#[async_trait::async_trait]
impl MemoryProvider for CliMemory {
    async fn save_memory(&self, memory: Memory) -> Result<Memory, Self::Error> {
        match self {
            CliMemory::Local(provider) => provider
                .save_memory(memory)
                .await
                .map_err(|err| MemoryFailure(err.to_string())),
            CliMemory::External(provider) => provider
                .save_memory(memory)
                .await
                .map_err(|err: AiMemoryAdapterError| MemoryFailure(err.to_string())),
        }
    }

    async fn query_memories(&self, query: &MemoryQuery) -> Result<Vec<Memory>, Self::Error> {
        match self {
            CliMemory::Local(provider) => provider
                .query_memories(query)
                .await
                .map_err(|err| MemoryFailure(err.to_string())),
            CliMemory::External(provider) => provider
                .query_memories(query)
                .await
                .map_err(|err: AiMemoryAdapterError| MemoryFailure(err.to_string())),
        }
    }

    async fn recent_memories(&self, limit: u32) -> Result<Vec<Memory>, Self::Error> {
        match self {
            CliMemory::Local(provider) => provider
                .recent_memories(limit)
                .await
                .map_err(|err| MemoryFailure(err.to_string())),
            CliMemory::External(provider) => provider
                .recent_memories(limit)
                .await
                .map_err(|err: AiMemoryAdapterError| MemoryFailure(err.to_string())),
        }
    }
}

/// Everything the lifecycle needs, assembled once.
pub struct Runtime {
    /// The authoritative store.
    pub store: Store,
    /// Git observation.
    pub git: GitService,
    /// Where git state is persisted.
    pub state_dir: PathBuf,
    /// The repository being worked in, canonicalized.
    pub repo: PathBuf,
    /// The same repository as the string the git service takes.
    pub repo_str: String,
}

/// Open the store and the git service.
///
/// Orqyn's own state lives under `<db parent>/.orqynn`-style directories that are
/// created here, because refusing to run on a fresh clone would be a poor first
/// impression for no safety gain. The repository itself is never written to
/// except by the agent.
///
/// Async because `Store::open` is. An earlier version opened it through a private
/// `block_on`, which panicked outright when this ran inside an already-running
/// runtime — so it is called through the caller's runtime like everything else.
pub async fn open_runtime(args: &RunArgs) -> Result<Runtime, RunError> {
    let db_parent = args
        .database
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| args.repo.clone());
    std::fs::create_dir_all(&db_parent).map_err(|err| {
        RunError::Setup(format!("could not create {}: {err}", db_parent.display()))
    })?;
    let state_dir = db_parent.join("git-state");
    std::fs::create_dir_all(&state_dir).map_err(|err| {
        RunError::Setup(format!("could not create the git state directory: {err}"))
    })?;

    let git = GitService::new(&state_dir)
        .map_err(|err| RunError::Setup(format!("could not open the git service: {err}")))?;
    let repo = args
        .repo
        .canonicalize()
        .unwrap_or_else(|_| args.repo.clone());
    let repo_str = repo.to_string_lossy().into_owned();
    let store = Store::open(&args.database)
        .await
        .map_err(|err| RunError::Setup(format!("could not open the store: {err}")))?;

    Ok(Runtime {
        store,
        git,
        state_dir,
        repo,
        repo_str,
    })
}

/// Connect the memory substrate, or explain why it cannot be used.
///
/// Refused *before* anything runs: a run that would silently lose its checkpoint
/// publication is worse than one that does not start.
pub async fn connect_memory(args: &RunArgs) -> Result<Option<Box<AiMemoryAdapter>>, RunError> {
    if !args.memory {
        return Ok(None);
    }
    let config = AiMemoryConfig::from_env()
        .map_err(|_| RunError::Config(ConfigError::MemoryNotConfigured))?;
    let adapter = config
        .connect()
        .await
        .map_err(|err| RunError::Setup(format!("could not reach the memory substrate: {err}")))?;
    Ok(Some(Box::new(adapter)))
}

/// The lifecycle, generic over the harness driver and the memory provider.
///
/// `D` is the harness driver: production passes [`ClaudeCodeDriver`], and the
/// CLI's own tests pass the deterministic local fixture. That substitution is what
/// lets the orchestration be tested honestly — a real process runs, and nothing
/// about it pretends to be a real agent.
pub async fn run_lifecycle<D, M>(
    runtime: &Runtime,
    memory: &M,
    driver: &D,
    executor: &LocalExecutor,
    args: &RunArgs,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<RunReport, RunError>
where
    D: director_domain::harness::HarnessDriver,
    M: MemoryProvider + Send + Sync,
{
    let mut lines: Vec<String> = Vec::new();
    let project = project_id();
    let repository = RepositoryId::from_string(REPOSITORY_ID);
    let task_id = TaskId::from_string(args.task_id());

    let mut runner = Runner::new(
        &runtime.store,
        &runtime.git,
        memory,
        executor,
        &runtime.repo,
    );

    // 1. OBSERVE. Registers the project and repository from real git state, and
    //    does the checkpointing half — which is where memory publication happens.
    let observed = runner
        .observe(&project, &repository, &runtime.repo_str, now)
        .await
        .map_err(|err| RunError::Orchestration {
            step: "OBSERVE",
            reason: err.to_string(),
        })?;
    lines.push(format!(
        "observed {} on branch {} at {} ({} checkpoint(s) written)",
        runtime.repo.display(),
        observed.branch.as_deref().unwrap_or("(detached)"),
        &observed.head_commit[..observed.head_commit.len().min(12)],
        observed.checkpoints_written
    ));

    // 2. Register the agent. A duplicate is reported, not fatal: a retry after an
    //    interrupted run finds the agent already there, which is correct.
    let agent = Agent::register(
        AgentId::from_string(AGENT_ID),
        "orqynn-cli",
        Harness::ClaudeCode,
        MachineId::from_string(MACHINE_ID),
        vec![Capability::Coding, Capability::Testing],
    );
    match runtime.store.agents().register_agent(&agent).await {
        Ok(_) => lines.push("registered the agent".into()),
        Err(err) => lines.push(format!(
            "agent {AGENT_ID} already registered ({err}); reusing it"
        )),
    }

    // 3. PLAN. The task, and the checks that will judge it.
    //
    //    A task id that already exists is refused here, before the plan is
    //    attempted. Letting the store reject it would surface a SQLite
    //    constraint message that says nothing about what to do instead, and
    //    planning again would be a no-op at best.
    if runtime.store.tasks().get_task(&task_id).await.is_ok() {
        return Err(RunError::Orchestration {
            step: "PLAN",
            reason: format!(
                "task {} already exists in this store, so it has already been planned and run. \
                 Re-running the same --task-id will not plan it a second time and nothing was \
                 duplicated; pass a different --task-id for a new task, or point --db at a \
                 fresh database to start over.",
                task_id.as_str()
            ),
        });
    }

    let mut spec =
        director_app::plan::TaskSpec::new(task_id.clone(), args.task_id(), args.task.clone());
    spec.expected_outputs = args
        .checks
        .iter()
        .map(|check| ExpectedOutput {
            criterion: format!("`{check}` exits zero"),
            check: Some(check.clone()),
        })
        .collect();

    // If the project is already executing a plan, this task joins it. Creating a
    // new plan here instead would supersede the sitting one, and a superseded
    // plan's tasks are ineligible for assignment — so asking for a second task
    // would quietly make the first one unrecoverable.
    let existing = runtime
        .store
        .plans()
        .active_plan_for_project(&project)
        .await
        .map_err(|err| RunError::Orchestration {
            step: "PLAN",
            reason: format!("could not read the project's active plan: {err}"),
        })?;

    if existing.is_some() {
        let extended = director_app::plan::extend_active_plan_at(
            &runtime.store,
            director_app::plan::ExtendRequest {
                project_id: project.clone(),
                task: spec,
                authorized_by: AgentId::from_string("ORQYNN-CLI"),
                now,
            },
        )
        .await
        .map_err(|err| RunError::Orchestration {
            step: "PLAN",
            reason: err.to_string(),
        })?;
        lines.push(format!(
            "added the task to the project's existing active plan {}; no plan was superseded. \
             That plan still records its originating objective (\"{}\") and now covers {} \
             requested task(s).",
            extended.plan.id.as_str(),
            extended.plan.objective.trim(),
            extended.plan.task_ids.len()
        ));
        for id in &extended.plan.task_ids {
            lines.push(format!("  plan member: {}", id.as_str()));
        }
    } else {
        let planned = runner
            .plan(
                director_app::plan::PlanRequest {
                    project_id: project.clone(),
                    plan_id: plan_id(&task_id),
                    objective: args.task.clone(),
                    rationale: "one task, requested explicitly on the command line".into(),
                    tasks: vec![spec],
                    authorized_by: AgentId::from_string("ORQYNN-CLI"),
                },
                now,
            )
            .await
            .map_err(|err| RunError::Orchestration {
                step: "PLAN",
                reason: match err {
                    // The step name is added once, by the wrapper below, so the inner
                    // error is unwrapped rather than repeated.
                    RunnerError::Plan(inner) => inner.to_string(),
                    other => other.to_string(),
                },
            })?;
        lines.push(format!(
            "created the project's first plan {} for this objective",
            planned.plan_id.as_str()
        ));
        lines.push(format!("planned {} task(s)", planned.tasks.len()));
    }

    // 4. SCHEDULE, which applies its pairings through ASSIGN.
    let scheduled =
        runner
            .schedule(&project, now)
            .await
            .map_err(|err| RunError::Orchestration {
                step: "SCHEDULE",
                reason: err.to_string(),
            })?;
    let pairing = scheduled
        .assigned
        .iter()
        .find(|pairing| pairing.task_id == task_id)
        .ok_or_else(|| {
            let unassigned: Vec<String> = scheduled
                .unassigned
                .iter()
                .map(|task| format!("{}: {}", task.task_id.as_str(), task.reason))
                .collect();
            let refused: Vec<String> = scheduled
                .refused
                .iter()
                .map(|pairing| format!("{}: {}", pairing.task_id.as_str(), pairing.reason))
                .collect();
            let mut reasons = Vec::new();
            if !unassigned.is_empty() {
                reasons.push(format!("unassigned: {}", unassigned.join("; ")));
            }
            if !refused.is_empty() {
                reasons.push(format!("refused: {}", refused.join("; ")));
            }
            RunError::Orchestration {
                step: "SCHEDULE",
                reason: format!(
                    "task {} was not handed out.{}",
                    task_id.as_str(),
                    if reasons.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", reasons.join(". "))
                    }
                ),
            }
        })?;
    let assignment = pairing.assignment_id.clone();
    lines.push(format!(
        "assigned {} to {} via {}",
        task_id.as_str(),
        pairing.agent_id.as_str(),
        assignment.as_str()
    ));

    // 5. Execute the agent and verify, through the existing drive path. The
    //    verdict comes back inside `DrivenStep`; nothing here decides it.
    let mut options = DriveOptions::in_dir(runtime.repo.to_string_lossy().into_owned());
    options.timeout = args.agent_timeout;
    options.external_memory_timeout = args.memory_timeout;
    options.permission_mode = args.permission_mode.clone();
    options.model = args.model.clone();
    options.extra_dirs = args.extra_dirs.clone();

    let driven = runner
        .drive_agent_with_memory_at(driver, &task_id, &assignment, &options, now)
        .await
        .map_err(|err| RunError::Orchestration {
            step: "AGENT DRIVE",
            reason: err.to_string(),
        })?;

    lines.push(describe_drive(&driven.drive));

    // 6. Decide the exit code from the verdict, never from the agent's own view.
    let (exit_code, verdict_lines) = classify(&driven);
    lines.extend(verdict_lines);

    Ok(RunReport { exit_code, lines })
}

/// Report what the agent actually did, without implying it succeeded.
fn describe_drive(drive: &AgentDrive) -> String {
    let mut text = match drive.outcome.as_ref() {
        Some(outcome) => {
            let mut t = format!(
                "agent ran: exit {:?}, {} bytes of stdout, {} bytes of stderr",
                outcome.exit_code,
                outcome.stdout.len(),
                outcome.stderr.len()
            );
            if outcome.truncated {
                t.push_str(" (output clipped at the configured limit)");
            }
            if outcome.reported_error {
                t.push_str("; the harness reported an error");
            }
            t.push('.');
            t
        }
        None => {
            let mut t = "the agent did not run".to_string();
            if let Some(reason) = &drive.not_run {
                t.push_str(&format!(": {reason}"));
            }
            t.push('.');
            t
        }
    };
    text.push_str(&format!(
        " External memory: {}.",
        drive.external_memory.summary()
    ));
    text
}

/// Turn a verdict into an exit code, plus the lines explaining it.
///
/// The only path to [`ExitCode::Success`] is a verdict that passed, which only the
/// verification engine produces, *and* an agent that demonstrably ran without
/// reporting an error. An agent's own exit code is never on its own a reason to
/// succeed, and is never on its own a reason to fail.
pub fn classify(driven: &DrivenStep) -> (ExitCode, Vec<String>) {
    let Some(verify) = driven.verify.as_ref() else {
        let reason = driven
            .drive
            .not_run
            .as_ref()
            .map(|err| format!(": {err}"))
            .unwrap_or_default();
        return (
            ExitCode::Execution,
            vec![format!(
                "no verdict: the agent did not run{reason}, so there was nothing to verify"
            )],
        );
    };

    // The verdict lines are computed first, so they can still be reported when the
    // agent's own behaviour disqualifies the run. They are diagnostics, not a
    // decision: the exit code below is what Orqyn claims.
    let check_lines = check_lines(verify);

    // A harness that reported an error in its own output has told us it did not do
    // the task. Passing checks do not make that task done, so this cannot succeed —
    // and it is not a verification failure either, because the verdict may well
    // have passed. It is an execution failure.
    let harness_failed = driven
        .drive
        .outcome
        .as_ref()
        .map(|outcome| outcome.reported_error)
        .unwrap_or(true);
    if harness_failed {
        let mut lines = vec![
            "the harness reported an error in its own output: the agent did not do the \
             task."
                .to_string(),
        ];
        lines.extend(check_lines);
        lines.push(
            "whatever the checks said, the task is NOT done: the agent itself reported a \
             failure."
                .to_string(),
        );
        return (ExitCode::Execution, lines);
    }

    let mut failed = false;
    let mut unrunnable = false;
    for judged in &verify.judged {
        for check in &judged.checks {
            match check {
                CheckOutcome::Passed { .. } => {}
                CheckOutcome::Failed { .. } => failed = true,
                CheckOutcome::Unrunnable { .. } => unrunnable = true,
                CheckOutcome::Unchecked { .. } => {}
            }
        }
    }

    let mut lines = vec![format!(
        "verification judged {} task(s)",
        verify.judged.len()
    )];
    lines.extend(check_lines);

    if failed {
        lines.push("a check failed: the task is NOT done, whatever the agent reported".to_string());
        return (ExitCode::VerificationFailed, lines);
    }
    if unrunnable {
        lines.push(
            "Orqyn could not run at least one check, so no verdict was reached; the task \
             stays awaiting verification"
                .to_string(),
        );
        return (ExitCode::Unverifiable, lines);
    }
    if verify.all_passed() {
        lines.push("every check exited zero: the task is done".to_string());
        return (ExitCode::Success, lines);
    }
    lines.push("no verdict was reached; the task stays awaiting verification".to_string());
    (ExitCode::Unverifiable, lines)
}

/// One line per check, for printing. Decides nothing.
fn check_lines(verify: &VerifyStep) -> Vec<String> {
    let mut lines = Vec::new();
    for judged in &verify.judged {
        lines.push(format!("  {}: {}", judged.task_id.as_str(), judged.verdict));
        for check in &judged.checks {
            match check {
                CheckOutcome::Passed { criterion, command } => {
                    lines.push(format!("    PASS  {criterion}  (`{command}`)"))
                }
                CheckOutcome::Failed {
                    criterion,
                    command,
                    exit_code,
                    timed_out,
                    ..
                } => lines.push(format!(
                    "    FAIL  {criterion}  (`{command}` exited {exit_code}{})",
                    if *timed_out { ", timed out" } else { "" }
                )),
                CheckOutcome::Unrunnable { criterion, .. } => lines.push(format!(
                    "    SKIP  {criterion}  (the command could not run)"
                )),
                CheckOutcome::Unchecked { criterion } => {
                    lines.push(format!("    ??    {criterion}  (not checked)"))
                }
            }
        }
    }
    lines
}

/// Print what a dry run would do, built from the same resolved arguments.
///
/// It starts no process and touches no store, and says so on its first line, so
/// nobody can mistake it for a run.
pub fn dry_run_report(args: &RunArgs) -> Vec<String> {
    let mut lines = vec![
        "DRY RUN — nothing was executed, nothing was written, nothing was verified.".to_string(),
        "A real run of this command would:".to_string(),
    ];
    for line in args.provenance() {
        lines.push(format!("  {line}"));
    }
    lines.extend([
        "  then:".to_string(),
        "    1. open the store and observe the repository's real git state".to_string(),
        "    2. persist one task whose acceptance criteria are the checks above".to_string(),
        "    3. schedule and assign it to the registered agent".to_string(),
        "    4. launch the agent binary in the repository with the task briefing".to_string(),
        format!(
            "    5. run each command and read its exit code: {}",
            if args.checks.is_empty() {
                "(none given)".to_string()
            } else {
                args.checks.join(", ")
            }
        ),
        "    6. report the task done only if every check exits zero".to_string(),
    ]);
    if args.permission_mode.is_none() {
        lines.push(
            "  note: no --permission-mode was given, so the agent's own default applies. \
             Orqyn will not pick a permissive one for you; a harness that waits for \
             approval will hit the timeout rather than prompt."
                .to_string(),
        );
    }
    if !args.memory {
        lines.push(
            "  note: --memory was not given, so checkpoints stay in SQLite and no external \
             memory is read or written."
                .to_string(),
        );
    }
    lines
}

/// Build the agent backend configuration from the arguments.
///
/// No permissive default is chosen: with no `--permission-mode`, `None` is passed
/// through and the harness applies its own.
pub fn harness_config(args: &RunArgs) -> ClaudeCodeConfig {
    let mut config = ClaudeCodeConfig::new(args.agent_binary.clone());
    config.permission_mode = args.permission_mode.clone();
    config.model = args.model.clone();
    config.default_timeout = args.agent_timeout;
    // A command transcript should not be able to consume the machine. Verification
    // commands have their own bound in the engine.
    config.max_output_bytes = 4 * 1024 * 1024;
    config
}

/// The real Claude Code driver for this invocation.
pub fn claude_driver(args: &RunArgs) -> ClaudeCodeDriver {
    ClaudeCodeDriver::new(harness_config(args))
}

/// The project this CLI uses.
pub fn project_id() -> ProjectId {
    ProjectId::from_string(PROJECT_ID)
}

/// Whether the store already knows the project, for a resume diagnostic.
pub async fn project_is_known(store: &Store) -> bool {
    store.projects().get_project(&project_id()).await.is_ok()
}

// ---------------------------------------------------------------------------
// Resume
// ---------------------------------------------------------------------------

/// What a resume decided to do.
///
/// Distinct from the exit code on purpose: the exit code says whether the run
/// succeeded, this says what was *attempted*, which is the question an operator
/// asking "why did resume do nothing?" needs answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeAction {
    /// Nothing was launched and nothing was written. The task's state does not
    /// permit a safe resume, and the report says why.
    Refused,
    /// Verification ran again against the current working tree. No agent was
    /// launched, because none was needed and none was authorised.
    Reverified,
    /// A remediation was applied, a new assignment created, and the agent driven.
    Redriven,
}

/// What a resume did, for printing.
#[derive(Debug)]
pub struct ResumeReport {
    /// What was attempted.
    pub action: ResumeAction,
    /// The exit code the resume earned.
    pub exit_code: ExitCode,
    /// Lines describing the decision and its evidence, in order.
    pub lines: Vec<String>,
}

/// Recover one task from persisted state, and act only as far as that state allows.
///
/// The decision is made entirely from what SQLite holds: the task row, its
/// assignments, its blockers, its latest verification, and the recovery package
/// the domain builds from all of them. Nothing here infers that an agent ran
/// because an assignment exists, and nothing here writes a task status directly -
/// every state change goes through `director-app`, so the same rules that govern
/// the loop govern a resume.
///
/// `D` is the harness driver. It is used only on the re-drive path; a resume that
/// re-verifies never touches it, and that is why its tests can assert that no
/// process was launched rather than merely that none was observed.
pub async fn resume_lifecycle<D, M>(
    runtime: &Runtime,
    memory: &M,
    driver: &D,
    executor: &LocalExecutor,
    args: &RunArgs,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<ResumeReport, RunError>
where
    D: director_domain::harness::HarnessDriver,
    M: MemoryProvider + Send + Sync,
{
    let mut lines: Vec<String> = Vec::new();
    let task_id = TaskId::from_string(args.task_id());

    // 1. The task must exist. A resume that cannot find what it was asked to
    //    recover must say so, not fall back to creating a new task.
    let task = runtime
        .store
        .tasks()
        .get_task(&task_id)
        .await
        .map_err(|err| RunError::Orchestration {
            step: "LOAD",
            reason: format!("could not read task {}: {err}", task_id.as_str()),
        })?;

    let Some(project_id) = task.project_id.clone() else {
        return Ok(refuse(
            lines,
            format!(
                "task {} is not claimed for any project, so Orqyn holds no state to recover it \
                 from. It cannot be resumed safely.",
                task_id.as_str()
            ),
        ));
    };

    // 2. The evidence. Loaded before any decision, so the decision can cite it.
    let blockers = runtime
        .store
        .blockers()
        .blockers_for_task(&task_id)
        .await
        .map_err(|err| RunError::Orchestration {
            step: "LOAD",
            reason: format!("could not read the task's blockers: {err}"),
        })?;
    let open_blockers: Vec<_> = blockers
        .iter()
        .filter(|blocker| blocker.status.is_blocking())
        .collect();

    let active = runtime
        .store
        .assignments()
        .active_assignment_for_task(&task_id)
        .await
        .map_err(|err| RunError::Orchestration {
            step: "LOAD",
            reason: format!("could not read the task's assignments: {err}"),
        })?;

    let latest = runtime
        .store
        .verifications()
        .latest_verification(&task_id)
        .await
        .map_err(|err| RunError::Orchestration {
            step: "LOAD",
            reason: format!("could not read the task's verification history: {err}"),
        })?;

    let recovery = RecoveryBuilder::new(&runtime.store, task_id.clone())
        .from_stored_state()
        .build()
        .await
        .map_err(|err| RunError::Orchestration {
            step: "RECOVER",
            reason: format!("could not assemble recovery context: {err}"),
        })?;

    lines.push(describe_resume_state(
        &task,
        &blockers,
        active.as_ref(),
        latest.as_ref(),
    ));
    lines.push(describe_recovery(&recovery, &task_id));

    // 3. The refusals, in the order the table gives them. Each returns without
    //    writing anything, so a refused resume is indistinguishable from a resume
    //    that was never attempted.
    if let Some(decision) = terminal_refusal(&task, &task_id) {
        return Ok(refuse(lines, decision));
    }
    if task.status == TaskStatus::Blocked {
        return Ok(refuse(
            lines,
            format!(
                "task {} is blocked. Clearing the blocker is a decision about the work, and \
                 Orqyn will not make it on the caller's behalf; resolve the blocker through the \
                 blocker lifecycle first.",
                task_id.as_str()
            ),
        ));
    }
    if let Some(blocker) = open_blockers.first() {
        return Ok(refuse(
            lines,
            format!(
                "task {} has {} open blocker(s) (for example {}: {}). Work cannot resume while \
                 one stands.",
                task_id.as_str(),
                open_blockers.len(),
                blocker.id.as_str(),
                blocker.description
            ),
        ));
    }
    if let Some(decision) = recovery_refusal(&recovery, &task, &task_id) {
        return Ok(refuse(lines, decision));
    }
    if let Some(assignment) = active.as_ref() {
        return Ok(refuse(
            lines,
            format!(
                "task {} is held by an active assignment ({}), so an agent is already working \
                 on it. Resuming would create a second tenure over the same ground, so nothing \
                 was done.",
                task_id.as_str(),
                assignment.id.as_str()
            ),
        ));
    }

    // 4. Re-verification is the one thing a resume may do without being told to.
    //    It runs the task's *persisted* criteria against the *current* working
    //    tree, and it launches nothing.
    if task.status == TaskStatus::VerificationPending {
        return reverify(runtime, executor, args, task, task_id, lines, now).await;
    }

    // 5. Anything else that wants a new agent tenure needs an explicit, valid
    //    remediation. This is the difference between `resume` and `run`: nothing
    //    is re-driven here without the operator naming what should happen.
    let Some(requested) = args.remediate else {
        return Ok(refuse(
            lines,
            format!(
                "task {} is {} with no active assignment, so the work stopped without a verdict \
                 and nothing can be re-driven without being told how. Pass --remediate retry to \
                 hand it out again, --remediate cancel to abandon it, or nothing at all to leave \
                 it as it is.",
                task_id.as_str(),
                status_word(task.status)
            ),
        ));
    };

    if !requested.re_drives() {
        // Cancel is terminal and needs no agent, so it is applied and reported.
        return apply_remediation(
            runtime,
            executor,
            args,
            &project_id,
            task_id,
            requested,
            lines,
            now,
            memory,
            driver,
        )
        .await;
    }

    // 6. Survey, validate, apply. The domain owns these rules; resume only chooses
    //    which remediation to ask for and reports what the round decided.
    let decision = TaskDecision {
        task_id: task_id.clone(),
        remediation: requested.to_app(),
        reason: Some(format!(
            "orqynn resume: the operator authorised {:?} for a task with no active assignment",
            requested
        )),
    };
    let held = std::collections::HashMap::from([(task_id.clone(), false)]);
    let survey = replan::survey(std::slice::from_ref(&task), &held);
    let condition = survey.get(&task_id).cloned();
    lines.push(format!(
        "replan survey: {}",
        match &condition {
            None => "nothing to decide; the task is in none of the states REPLAN answers".into(),
            Some(found) => match found {
                Condition::Failed => "the task failed verification".into(),
                Condition::Orphaned => {
                    "the task is in flight with no active assignment".into()
                }
                Condition::PremiseBroken { dependency, .. } => format!(
                    "a dependency ({}) reached a dead end, so the task's premise is broken",
                    dependency.as_str()
                ),
            },
        }
    ));
    if let Some(found) = condition.as_ref() {
        if let Err(error) = replan::validate_decision(&task, found, &decision) {
            return Ok(refuse(
                lines,
                format!(
                    "the requested remediation is not valid for this task: {error:?}. Orqyn will \
                     not apply a decision its own rules refuse, and nothing was written."
                ),
            ));
        }
    }

    // The agent binary is validated only now, on the one path that launches one.
    if let Err(err) = args.validate_agent_binary() {
        return Err(RunError::Config(err));
    }

    apply_remediation(
        runtime,
        executor,
        args,
        &project_id,
        task_id,
        requested,
        lines,
        now,
        memory,
        driver,
    )
    .await
}

/// Render a refusal. Nothing was written, so there is no state to report beyond
/// the reason.
fn refuse(mut lines: Vec<String>, reason: impl Into<String>) -> ResumeReport {
    let reason = reason.into();
    lines.push(format!("resume refused: {reason}"));
    lines.push(
        "nothing was launched and nothing was written; the task is exactly as it was".to_string(),
    );
    ResumeReport {
        action: ResumeAction::Refused,
        exit_code: ExitCode::UnsafeState,
        lines,
    }
}

/// The status, in words a report can use.
fn status_word(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Backlog => "in the backlog",
        TaskStatus::Todo => "waiting to be handed out",
        TaskStatus::InProgress => "in progress with no active assignment",
        TaskStatus::Blocked => "blocked",
        TaskStatus::VerificationPending => "awaiting verification",
        TaskStatus::Done => "done",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

/// One line describing the persisted state the decision rests on.
fn describe_resume_state(
    task: &director_domain::task::Task,
    blockers: &[director_domain::blocker::Blocker],
    active: Option<&director_domain::assignment::AgentAssignment>,
    latest: Option<&director_domain::verification::Verification>,
) -> String {
    format!(
        "task {} is {}; {} open blocker(s); {}; latest verdict {}",
        task.id.as_str(),
        status_word(task.status),
        blockers.iter().filter(|b| b.status.is_blocking()).count(),
        match active {
            Some(assignment) => format!("held by active assignment {}", assignment.id.as_str()),
            None => "no active assignment".to_string(),
        },
        match latest {
            None => "none on record".to_string(),
            Some(verification) => format!("{:?}", verification.status),
        }
    )
}

/// The recovery package's own verdict, as one line of evidence.
fn describe_recovery(recovery: &RecoveryOutcome, task_id: &TaskId) -> String {
    recovery_reason(recovery, task_id)
}

/// Why the recovery package forbids acting, if it does.
///
/// The domain's `ResumeStatus` answers one question — *may this work be handed
/// straight back to an agent?* — and answers `Insufficient` for anything that is
/// not `in_progress`, including a task awaiting verification and a failed one.
/// Those two are not refusals: the first is answered by re-running the checks,
/// the second by REPLAN. So this gate only refuses where the package positively
/// says the world has moved or the record disagrees with itself.
fn recovery_refusal(
    recovery: &RecoveryOutcome,
    task: &director_domain::task::Task,
    task_id: &TaskId,
) -> Option<String> {
    if task.status == TaskStatus::VerificationPending {
        // Re-verification reads the task's own persisted criteria and runs them
        // against the current tree, so it needs no checkpoint and no agreement
        // between the package and the task row.
        return None;
    }
    match recovery {
        RecoveryOutcome::UnknownTask(_)
        | RecoveryOutcome::NoProject(_)
        | RecoveryOutcome::NoStoredProjectState(_) => Some(recovery_reason(recovery, task_id)),
        RecoveryOutcome::NoCheckpoint(_) => {
            // A missing checkpoint means there is no narrative to hand a
            // *replacement* agent, which is exactly the orphaned case. It is not a
            // reason to refuse a retry of a task that already reached a verdict:
            // that task's status, verifications and actions are the authoritative
            // record, and REPLAN gates the retry on them. Blocking here would
            // leave every failed task unresumable, because a task that was
            // observed before it was ever in progress is never checkpointed.
            if task.status == TaskStatus::InProgress {
                Some(recovery_reason(recovery, task_id))
            } else {
                None
            }
        }
        RecoveryOutcome::Built(package) => match package.resume_status() {
            ResumeStatus::Conflicted | ResumeStatus::Stale | ResumeStatus::Blocked => {
                Some(recovery_reason(recovery, task_id))
            }
            // `Ready` is permission, and `Insufficient` is not a verdict this
            // gate may act on: REPLAN owns the failed and orphaned cases.
            ResumeStatus::Ready | ResumeStatus::Insufficient => None,
        },
    }
}

fn recovery_reason(recovery: &RecoveryOutcome, task_id: &TaskId) -> String {
    match recovery {
        RecoveryOutcome::UnknownTask(id) => format!(
            "Orqyn has no record of task {}. The id is wrong, or this is not the database the \
             task was planned in.",
            id.as_str()
        ),
        RecoveryOutcome::NoCheckpoint(id) => format!(
            "task {} has never been checkpointed, so there is no progress narrative to hand a \
             replacement agent. Its history is still answerable, but resuming the work is not; \
             re-plan it as a new task instead.",
            id.as_str()
        ),
        RecoveryOutcome::NoProject(id) => format!(
            "task {} is not claimed for any project, so Orqyn holds no project state to compare \
             a checkpoint against.",
            id.as_str()
        ),
        RecoveryOutcome::NoStoredProjectState(id) => format!(
            "the project task {} belongs to has never been observed, so there is no baseline to \
             tell changed work from a moved-on repository.",
            id.as_str()
        ),
        RecoveryOutcome::Built(package) => match package.resume_status() {
            ResumeStatus::Ready => format!(
                "recovery: task {} is ready; the recorded world still matches what is on disk",
                task_id.as_str()
            ),
            ResumeStatus::Stale => format!(
                "recovery: task {} is stale; the repository has moved on since the last \
                 checkpoint, so resuming would continue against a world the checkpoint does not \
                 describe. Reconcile the tree first.",
                task_id.as_str()
            ),
            ResumeStatus::Conflicted => format!(
                "recovery: task {} is conflicted; the task's status and its verdicts on record \
                 disagree, so Orqyn will not pick a side. This needs adjudication, not a retry.",
                task_id.as_str()
            ),
            ResumeStatus::Blocked => format!(
                "recovery: task {} cannot proceed; recovery reports something blocking it",
                task_id.as_str()
            ),
            ResumeStatus::Insufficient => format!(
                "recovery: task {} has too little recorded state to resume safely",
                task_id.as_str()
            ),
        },
    }
}

/// Refuse the terminal states. Re-opening them is never this command's call.
fn terminal_refusal(task: &director_domain::task::Task, task_id: &TaskId) -> Option<String> {
    match task.status {
        TaskStatus::Done => Some(format!(
            "task {} is already done. Its verdict passed when it was reached; re-running the \
             agent would not re-decide it, and silently reopening a completed task is not \
             something a resume may do.",
            task_id.as_str()
        )),
        TaskStatus::Cancelled => Some(format!(
            "task {} is cancelled, which is terminal. Re-driving it would contradict the \
             decision that cancelled it.",
            task_id.as_str()
        )),
        _ => None,
    }
}

/// Re-run verification for a task awaiting one, against the current tree.
///
/// Launches nothing. The criteria are the task's own persisted `ExpectedOutput`
/// rows, read by the engine, so a resume cannot widen or narrow what the task is
/// judged by — it can only re-ask the same question of the working tree as it
/// stands now. This is what makes a passing verdict from an earlier round
/// worthless on its own: the checks are not copied, they are re-run.
async fn reverify(
    runtime: &Runtime,
    executor: &LocalExecutor,
    args: &RunArgs,
    task: director_domain::task::Task,
    task_id: TaskId,
    mut lines: Vec<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<ResumeReport, RunError> {
    let stored: Vec<String> = engine::probes_for(&task)
        .into_iter()
        .filter_map(|probe| match probe {
            director_domain::verification::Probe::Command { command, .. } => Some(command),
            _ => None,
        })
        .collect();

    if stored.is_empty() {
        return Ok(refuse(
            lines,
            format!(
                "task {} records no checkable acceptance criterion, so Orqyn has no question to \
                 re-ask. Resume with --check so the criteria are recorded first.",
                task_id.as_str()
            ),
        ));
    }

    // A supplied `--check` asserts what the task should be judged by. If it
    // disagrees with what is persisted the two cannot both be right, and
    // silently preferring either would be worse than refusing.
    if !args.checks.is_empty() && args.checks != stored {
        return Ok(refuse(
            lines,
            format!(
                "the --check given does not match the criteria task {} is recorded \
                 against.\n    persisted: {}\n    supplied:  {}\nResume verifies against the \
                 persisted criteria; pass matching --check, or omit it. The recorded criteria \
                 were not changed.",
                task_id.as_str(),
                stored.join("; "),
                args.checks.join("; ")
            ),
        ));
    }

    lines.push(format!(
        "re-verifying against the current working tree with the persisted criteria: {}",
        stored.join("; ")
    ));
    lines.push("no agent is launched for re-verification".to_string());

    let round = engine::verify_task(&runtime.store, executor, &task, &runtime.repo, now)
        .await
        .map_err(|err| RunError::Orchestration {
            step: "VERIFY",
            reason: format!("re-verification failed: {err}"),
        })?;

    for outcome in &round.outcomes {
        lines.push(format!(
            "  {} {}  ({})",
            outcome_word(outcome.status),
            outcome.criterion,
            match &outcome.command {
                Some(command) => format!("`{command}` exited {}", outcome.exit_code),
                None => outcome.note.clone(),
            }
        ));
    }

    let (exit_code, verdict) = classify_engine(&round);
    lines.push(verdict);
    Ok(ResumeReport {
        action: ResumeAction::Reverified,
        exit_code,
        lines,
    })
}

/// Render one probe outcome.
fn outcome_word(status: director_domain::verification::EvidenceStatus) -> &'static str {
    use director_domain::verification::EvidenceStatus as S;
    match status {
        S::Passed => "PASS",
        S::Failed => "FAIL",
        S::Unverifiable => "SKIP",
        _ => "??  ",
    }
}

/// The exit code for a re-verification round.
///
/// Reached only through the engine's own verdict. No agent was involved, so the
/// execution-failure paths cannot arise here.
fn classify_engine(round: &engine::EngineRound) -> (ExitCode, String) {
    use director_domain::verification::EvidenceStatus as S;
    let mut failed = false;
    let mut unrunnable = false;
    for outcome in &round.outcomes {
        match outcome.status {
            S::Failed => failed = true,
            S::Unverifiable => unrunnable = true,
            _ => {}
        }
    }
    if failed {
        return (
            ExitCode::VerificationFailed,
            "re-verification failed: the task is NOT done, and the working tree was left exactly \
             as it was found"
                .to_string(),
        );
    }
    if unrunnable || round.outcomes.is_empty() {
        return (
            ExitCode::Unverifiable,
            "a check could not be run, so no verdict was reached; the task stays awaiting \
             verification"
                .to_string(),
        );
    }
    (
        ExitCode::Success,
        "every check exited zero against the current tree: the task is done".to_string(),
    )
}

/// Apply the authorised remediation and, if it re-drives the work, hand the task
/// out and drive it through the same path `run` uses.
#[allow(clippy::too_many_arguments)]
async fn apply_remediation<D, M>(
    runtime: &Runtime,
    executor: &LocalExecutor,
    args: &RunArgs,
    project_id: &ProjectId,
    task_id: TaskId,
    requested: RequestedRemediation,
    mut lines: Vec<String>,
    now: chrono::DateTime<chrono::Utc>,
    memory: &M,
    driver: &D,
) -> Result<ResumeReport, RunError>
where
    D: director_domain::harness::HarnessDriver,
    M: MemoryProvider + Send + Sync,
{
    let report = replan::replan_at(
        &runtime.store,
        ReplanRequest {
            project_id: project_id.clone(),
            decisions: vec![TaskDecision {
                task_id: task_id.clone(),
                remediation: requested.to_app(),
                reason: Some("orqynn resume".to_string()),
            }],
            decided_by: None,
        },
        now,
    )
    .await
    .map_err(|err| RunError::Orchestration {
        step: "REPLAN",
        reason: format!("the remediation round failed: {err}"),
    })?;

    for entry in &report.surveyed {
        lines.push(format!(
            "replan: {} -> {:?}",
            entry.task.as_str(),
            entry.outcome
        ));
    }

    let after = runtime
        .store
        .tasks()
        .get_task(&task_id)
        .await
        .map_err(|err| RunError::Orchestration {
            step: "REPLAN",
            reason: format!("could not re-read the task after remediation: {err}"),
        })?;
    lines.push(format!(
        "task {} is now {}",
        task_id.as_str(),
        status_word(after.status)
    ));

    if !requested.re_drives() {
        return Ok(ResumeReport {
            action: ResumeAction::Redriven,
            exit_code: ExitCode::Success,
            lines,
        });
    }

    // A retry that leaves the task somewhere it cannot be handed out from is not
    // a retry; say so rather than driving an agent against a task in the wrong
    // state.
    if after.status != TaskStatus::Todo {
        return Ok(refuse(
            lines,
            format!(
                "the remediation left task {} as {}, which is not a state Orqyn hands work out \
                 from. Nothing was launched.",
                task_id.as_str(),
                status_word(after.status)
            ),
        ));
    }

    // From here the work is driven exactly as `run` drives it: schedule, assign,
    // brief, drive, verify. No step is re-implemented for resume.
    let mut runner = Runner::new(
        &runtime.store,
        &runtime.git,
        memory,
        executor,
        &runtime.repo,
    );
    let scheduled =
        runner
            .schedule(project_id, now)
            .await
            .map_err(|err| RunError::Orchestration {
                step: "SCHEDULE",
                reason: err.to_string(),
            })?;
    let pairing = scheduled
        .assigned
        .iter()
        .find(|pairing| pairing.task_id == task_id)
        .ok_or_else(|| {
            // The scheduler's own reasons, quoted. Without them a refused handout
            // says only that it did not happen, which is the least useful possible
            // thing to tell an operator who just authorised a retry.
            let mut reasons: Vec<String> = scheduled
                .unassigned
                .iter()
                .filter(|task| task.task_id == task_id)
                .map(|task| format!("unassigned: {}", task.reason))
                .chain(
                    scheduled
                        .refused
                        .iter()
                        .filter(|pairing| pairing.task_id == task_id)
                        .map(|pairing| format!("refused: {}", pairing.reason)),
                )
                .collect();
            if reasons.is_empty() {
                reasons.push(format!(
                    "{} is not named by this project's active plan. That is Orqyn's rule, not an \
                     oversight: handing out work a superseded plan abandoned is how abandoned work \
                     gets restarted. Each `orqynn run` activates a plan of its own, so only the \
                     most recent task in a database can be re-driven; re-plan the work with \
                     `orqynn run` to pick it up again.",
                    task_id.as_str()
                ));
            }
            RunError::Orchestration {
                step: "SCHEDULE",
                reason: format!(
                    "the remediated task {} was not handed out. {}",
                    task_id.as_str(),
                    reasons.join(". ")
                ),
            }
        })?;
    lines.push(format!(
        "assigned {} to {} via {}",
        task_id.as_str(),
        pairing.agent_id.as_str(),
        pairing.assignment_id.as_str()
    ));

    let mut options = DriveOptions::in_dir(runtime.repo.to_string_lossy().into_owned());
    options.timeout = args.agent_timeout;
    options.external_memory_timeout = args.memory_timeout;
    options.permission_mode = args.permission_mode.clone();
    options.model = args.model.clone();
    options.extra_dirs = args.extra_dirs.clone();

    let driven = runner
        .drive_agent_with_memory_at(driver, &task_id, &pairing.assignment_id, &options, now)
        .await
        .map_err(|err| RunError::Orchestration {
            step: "AGENT DRIVE",
            reason: err.to_string(),
        })?;

    lines.push(describe_drive(&driven.drive));
    let (exit_code, verdict_lines) = classify(&driven);
    lines.extend(verdict_lines);
    Ok(ResumeReport {
        action: ResumeAction::Redriven,
        exit_code,
        lines,
    })
}

// ---------------------------------------------------------------------------
// Handoffs (read-only)
// ---------------------------------------------------------------------------

/// List the persisted handoff records for one task.
///
/// **Reads and nothing else.** No assignment is created, no task status moves,
/// and no record is written or mutated - not even a stale one, because a stale
/// record that is rewritten when observed is no longer an audit trail.
///
/// The listing deliberately does not print the continuation narrative by
/// default. The narrative is the outgoing agent's own account of the work, it can
/// be long, and an audit reader asked "who took this over and why" does not need
/// it. `next_steps` is summarised by count for the same reason.
pub async fn list_handoffs(runtime: &Runtime, args: &RunArgs) -> Result<Vec<String>, RunError> {
    let task_id = director_domain::ids::TaskId::from_string(args.task_id());

    // The task must exist: listing handoffs for an id Orqynn has never seen
    // would otherwise print an empty list indistinguishable from "none happened".
    let task = runtime
        .store
        .tasks()
        .get_task(&task_id)
        .await
        .map_err(|err| RunError::Orchestration {
            step: "LOAD",
            reason: format!("could not read task {}: {err}", task_id.as_str()),
        })?;
    let _ = &task;

    let records = runtime
        .store
        .handoffs()
        .handoffs_for_task(&task_id)
        .await
        .map_err(|err| RunError::Orchestration {
            step: "LOAD",
            reason: format!("could not read the task's handoffs: {err}"),
        })?;

    if records.is_empty() {
        return Ok(vec![format!(
            "no handoffs are recorded for task {}",
            task_id.as_str()
        )]);
    }

    let mut lines = vec![format!(
        "{} handoff record(s) for task {}{}",
        records.len(),
        task_id.as_str(),
        match task.project_id.as_ref() {
            Some(project) => format!(" in project {}", project.as_str()),
            None => " (no project recorded)".to_string(),
        }
    )];

    for record in &records {
        lines.push(String::new());
        lines.push(format!("  {}", record.id.as_str()));
        lines.push(format!(
            "    from {}  ->  to {}",
            record.from_agent.as_str(),
            record.to_agent.as_str()
        ));
        // A cause is stated only when the store proved one. "unknown" is a real
        // answer and is never softened into something more flattering.
        lines.push(format!(
            "    cause: {}",
            match record.cause {
                Some(cause) => cause.describe().to_string(),
                None => "unknown - not established from persisted records".to_string(),
            }
        ));
        lines.push(format!("    state: {:?}", record.state));
        lines.push(format!("    created: {}", record.created_at.to_rfc3339()));
        match (record.accepted_at, record.accepted_by_session.as_ref()) {
            (Some(at), Some(session)) => lines.push(format!(
                "    accepted: {} by session {}",
                at.to_rfc3339(),
                session.as_str()
            )),
            _ => lines.push("    accepted: not yet accepted".to_string()),
        }
        // Provenance, marked when absent. A missing checkpoint is not a failure;
        // it means the snapshot cannot be shown to describe the task as it is now.
        let provenance = match (
            record.source_checkpoint.as_ref(),
            record.source_task_version,
        ) {
            (Some(checkpoint), Some(version)) => {
                format!(
                    "checkpoint {} at task version {version}",
                    checkpoint.as_str()
                )
            }
            (None, Some(version)) => format!(
                "task version {version}, but NO checkpoint was recorded - provenance unavailable"
            ),
            (None, None) => "no provenance recorded".to_string(),
            (Some(checkpoint), None) => {
                format!(
                    "checkpoint {}, but no task version recorded",
                    checkpoint.as_str()
                )
            }
        };
        lines.push(format!("    source: {provenance}"));

        // Current-ness, judged against the task as it stands now.
        let current_task = runtime
            .store
            .tasks()
            .get_task(&task_id)
            .await
            .map_err(|err| RunError::Orchestration {
                step: "LOAD",
                reason: format!("could not re-read the task: {err}"),
            })?;
        let latest = runtime
            .store
            .checkpoints()
            .latest_checkpoint(&task_id)
            .await
            .ok()
            .flatten();
        let current = record.is_current_for(
            current_task.state_version,
            latest.as_ref().map(|checkpoint| &checkpoint.id),
        );
        lines.push(format!(
            "    snapshot: {}",
            if current {
                "matches the task as it stands now"
            } else {
                "STALE - context only; current state comes from the recovery package"
            }
        ));
        lines.push(format!(
            "    narrative: {} open question(s), {} next step(s), {} file(s) touched (not shown)",
            record.open_questions.len(),
            record.next_steps.len(),
            record.files_touched.len()
        ));
    }
    Ok(lines)
}
