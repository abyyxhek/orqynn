//! The runnable integration runner: one driver that opens the store and walks
//! Orqyn's seven orchestration steps in the order they depend on each other.
//!
//! Every other module in this crate is a step. This one is the thing that calls
//! them, which is the piece [`crate`]'s own module docs say has been missing
//! since the loop landed. It owns no orchestration logic of its own: it takes
//! the injected dependencies, calls the existing step functions in sequence, and
//! reports what came back. Where a step already owns a rule - what may be
//! assigned, what counts as verified, what a scope conflict is - that rule is
//! not restated here, and a step that refuses is reported as a refusal rather
//! than retried or worked around.
//!
//! ## What this runner does and does not do
//!
//! **Verification is real.** VERIFY is driven through the injected
//! [`ExecutionProvider`], so the checks in a task's expected outputs actually
//! run in the working directory and the verdict is read from exit codes alone.
//! Nothing in this crate can manufacture a passing check.
//!
//! **Agent execution is simulated.** This is the important caveat, and it is
//! the reason [`Runner::simulate_agent_turn`] is named the way it is. Orqyn has
//! no driver that launches Claude Code, OpenCode, or any other harness, so the
//! runner cannot start a real agent either. What an agent "doing the work" means
//! here is exactly two records an agent would report about itself:
//! [`monitor::acknowledge`] (began) and [`monitor::report_done`] (finished).
//! Neither touches a repository or an external process. Any task the runner
//! moves through those two calls has had *simulated* progress recorded against
//! it.
//!
//! **A report never completes a task.** [`monitor::report_done`] moves a task to
//! `verification_pending` and no further - `TaskStatus::Done` is unreachable
//! from an agent report by construction, and it is the VERIFY step's judgment
//! alone that lands it. So a runner tick that simulates an agent reporting
//! success still leaves the task awaiting a verdict.
//!
//! ## Determinism
//!
//! Every step method here calls the step's `_at` variant with a
//! `now` the caller supplies. Nothing in the runner reads the wall clock, so a
//! test can pin one instant and get identical records every run, and a
//! production caller passes [`chrono::Utc::now`] the way the steps' own
//! non-`_at` entry points do.
//!
//! ## Example
//!
//! ```no_run
//! # use std::path::Path;
//! # use director_adapters::git::GitService;
//! # use director_adapters::{InMemoryProvider, LocalExecutor};
//! # use director_app::plan::{PlanRequest, TaskSpec};
//! # use director_app::runner::Runner;
//! # use director_domain::ids::{AgentId, PlanId, ProjectId, RepositoryId, TaskId};
//! # use director_domain::task::TaskStatus;
//! # use director_store::Store;
//! # async fn run(
//! #     store: &Store,
//! #     git: &GitService,
//! #     memory: &InMemoryProvider,
//! #     executor: &LocalExecutor,
//! #     working_dir: &Path,
//! #     now: chrono::DateTime<chrono::Utc>,
//! # ) -> Result<(), Box<dyn std::error::Error>> {
//! let project = ProjectId::from_string("PROJ-1");
//! let auth = TaskId::from_string("AUTH");
//!
//! let mut runner = Runner::new(store, git, memory, executor, working_dir);
//!
//! // OBSERVE - and the step that creates the project.
//! runner
//!     .observe(&project, &RepositoryId::from_string("REPO-1"), "/repo", now)
//!     .await?;
//!
//! // PLAN
//! runner
//!     .plan(
//!         PlanRequest {
//!             project_id: project.clone(),
//!             plan_id: PlanId::from_string("PLAN-1"),
//!             objective: "add authentication".into(),
//!             rationale: "the smallest honest path".into(),
//!             tasks: vec![TaskSpec::new(auth.clone(), "auth", "the endpoint answers")],
//!             authorized_by: AgentId::from_string("AGENT-planner"),
//!         },
//!         now,
//!     )
//!     .await?;
//!
//! // SCHEDULE - which applies its pairings through ASSIGN as it goes.
//! let scheduled = runner.schedule(&project, now).await?;
//!
//! // Simulated agent progress. Nothing external ran; the task stops at
//! // verification_pending no matter what the simulated agent "reported".
//! if let Some(assignment) = scheduled.assignment_for(&auth) {
//!     let turn = runner.simulate_agent_turn(assignment, now).await?;
//!     assert_eq!(turn.task_status, TaskStatus::VerificationPending);
//! }
//!
//! // VERIFY - the only step that can reach Done, and it runs real commands.
//! runner.verify(&project).await?;
//!
//! // What a replacement agent would be handed for this task.
//! runner.recovery(&auth).await?;
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use director_adapters::git::GitService;
use director_domain::agent::Liveness;
use director_domain::checkpoint::ResumeStatus;
use director_domain::ids::{IdGenerator, ProjectId, RepositoryId, SessionId, TaskId};
use director_domain::providers::{ExecutionProvider, MemoryProvider};
use director_domain::repository::SyncStatus;
use director_domain::task::TaskStatus;
use director_store::Store;
use serde::Serialize;
use thiserror::Error;

use crate::assign::{self, AssignRequest};
use crate::monitor::{self, AcknowledgeRequest, ReportRequest};
use crate::observe::{self, Observed};
use crate::plan::{self, PlanRequest};
use crate::recovery::{self, Recovery};
use crate::replan::{self, ReplanRequest};
use crate::schedule::{self, ScheduleReport};
use crate::verify;
use crate::{
    AssignError, CheckpointError, MonitorError, ObserveError, PlanError, ReplanError,
    ScheduleError, VerifyError,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Every way a runner step can fail, in one place.
///
/// The variants are the step errors themselves, wrapped rather than flattened:
/// the runner is the layer that knows *which step* failed, and a caller
/// recovering a loop wants to branch on that before it looks at the cause. No
/// step's failure is turned into an empty result, a `None`, or a default - a
/// runner that swallowed an error would report a quiet, healthy-looking tick
/// over a store that never wrote anything.
///
/// [`CheckpointError`] appears twice over the runner's own steps: a failed
/// recovery build is reported through [`RunnerError::Checkpoint`].
#[derive(Debug, Error)]
pub enum RunnerError {
    /// The OBSERVE step failed.
    #[error("the OBSERVE step failed: {0}")]
    Observe(#[from] ObserveError),

    /// The PLAN step failed.
    #[error("the PLAN step failed: {0}")]
    Plan(#[from] PlanError),

    /// The SCHEDULE step failed.
    #[error("the SCHEDULE step failed: {0}")]
    Schedule(#[from] ScheduleError),

    /// The ASSIGN step failed.
    #[error("the ASSIGN step failed: {0}")]
    Assign(#[from] AssignError),

    /// A MONITOR step failed.
    #[error("a MONITOR step failed: {0}")]
    Monitor(#[from] MonitorError),

    /// The VERIFY step failed.
    #[error("the VERIFY step failed: {0}")]
    Verify(#[from] VerifyError),

    /// The REPLAN step failed.
    #[error("the REPLAN step failed: {0}")]
    Replan(#[from] ReplanError),

    /// Building a recovery context failed.
    #[error("building a recovery context failed: {0}")]
    Checkpoint(#[from] CheckpointError),
}

// ---------------------------------------------------------------------------
// Step records
// ---------------------------------------------------------------------------

/// What one OBSERVE round found and recorded.
#[derive(Debug, Clone, Serialize)]
pub struct ObserveStep {
    /// The project observed.
    pub project_id: ProjectId,
    /// The repository observed.
    pub repository_id: RepositoryId,
    /// Whether this round was Orqyn's first look at the project - a baseline
    /// rather than a change.
    pub first_observation: bool,
    /// The branch, or `None` under a detached HEAD.
    pub branch: Option<String>,
    /// The commit `HEAD` pointed at.
    pub head_commit: String,
    /// Whether the working tree was clean.
    pub working_tree_clean: bool,
    /// What the underlying git sync concluded.
    pub sync_status: SyncStatus,
    /// How many git events this round added that were not already known.
    pub events_created: usize,
    /// The project state version after the observation.
    pub state_version: u64,
    /// The checkpoints this round wrote for in-progress tasks. Empty on a quiet
    /// round or when nothing is in flight.
    pub checkpoints_written: usize,
}

impl ObserveStep {
    fn from_observed(
        observed: &Observed,
        project_id: &ProjectId,
        repository_id: &RepositoryId,
    ) -> Self {
        ObserveStep {
            project_id: project_id.clone(),
            repository_id: repository_id.clone(),
            first_observation: observed.first_observation,
            branch: observed.state.branch.clone(),
            head_commit: observed.state.head_commit.clone(),
            working_tree_clean: observed.state.working_tree_clean,
            sync_status: observed.sync.status,
            events_created: observed.sync.events_created,
            state_version: observed.state.state_version,
            checkpoints_written: observed.checkpoints.len(),
        }
    }
}

/// What one PLAN round produced.
#[derive(Debug, Clone, Serialize)]
pub struct PlanStep {
    /// The plan now active for the project.
    pub plan_id: String,
    /// The plan's objective, as recorded.
    pub objective: String,
    /// The tasks persisted, in execution order.
    pub tasks: Vec<PlannedTask>,
    /// The plan this one superseded, if any. The superseded plan is retained for
    /// audit rather than deleted.
    pub superseded: Option<String>,
}

/// One planned task as the runner reports it.
#[derive(Debug, Clone, Serialize)]
pub struct PlannedTask {
    /// The task id.
    pub id: TaskId,
    /// The status PLAN left it in - `todo`, since nothing has started.
    pub status: TaskStatus,
    /// The ids this task waits on.
    pub depends_on: Vec<TaskId>,
    /// What the task may touch. Empty means *undeclared*, which Phase 13 treats
    /// as conflicting with everything - see [`director_domain::task::Task::scope_overlap`].
    pub scope_paths: Vec<String>,
}

/// What one SCHEDULE round did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScheduleStep {
    /// Pairings applied. Each names the task, the agent now holding it, and the
    /// assignment id the caller will need for the MONITOR steps.
    pub assigned: Vec<ScheduledPairing>,
    /// Pairings the store refused. Nothing was written for these; the task is
    /// still `todo` and still handable.
    pub refused: Vec<RefusedPairing>,
    /// Ready work the round could not pair at all, with why.
    pub unassigned: Vec<UnassignedTask>,
}

/// A pairing SCHEDULE applied through ASSIGN.
///
/// Every field here is read back out of the step's own result. In particular
/// there is no task-status field: `schedule::Scheduled` carries the task id, the
/// agent, the assignment, and the recovery briefing, but not the task row, so the
/// runner has no status to report and does not guess one. A caller that needs
/// the status reads it from the store, where the step wrote it.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduledPairing {
    /// The task SCHEDULE handed out.
    pub task_id: TaskId,
    /// The agent holding it.
    pub agent_id: director_domain::ids::AgentId,
    /// The assignment in force, for the MONITOR steps that follow.
    pub assignment_id: director_domain::ids::AssignmentId,
    /// The briefing handed to a replacement agent resuming this task, when it
    /// had a checkpoint to resume from. `None` for work nobody has done before.
    pub recovery: Option<RecoveryStep>,
}

/// A pairing the store refused, with the reason in the step's own words.
#[derive(Debug, Clone, Serialize)]
pub struct RefusedPairing {
    /// The task the round tried to hand out.
    pub task_id: TaskId,
    /// The agent it would have gone to.
    pub agent_id: director_domain::ids::AgentId,
    /// Why the store refused, as [`AssignError`]'s own message.
    pub reason: String,
}

/// Ready work left without an agent.
#[derive(Debug, Clone, Serialize)]
pub struct UnassignedTask {
    /// The task that stayed `todo`.
    pub task_id: TaskId,
    /// The runner's rendering of [`schedule::UnassignedReason`].
    pub reason: String,
}

impl UnassignedTask {
    fn from_unassigned(unassigned: &schedule::Unassigned) -> Self {
        let reason = match &unassigned.reason {
            schedule::UnassignedReason::NoAgentsAtAll => "no agents are registered".to_string(),
            schedule::UnassignedReason::NoEligibleAgent => {
                "no available agent declares the required capabilities".to_string()
            }
            schedule::UnassignedReason::AgentTakenEarlier => {
                "an eligible agent was committed to an earlier task this round".to_string()
            }
            schedule::UnassignedReason::ScopeConflict { holder } => {
                format!("task {holder} already holds ground this task's scope overlaps")
            }
        };
        UnassignedTask {
            task_id: unassigned.task_id.clone(),
            reason,
        }
    }
}

impl ScheduleStep {
    fn from_report(report: &ScheduleReport) -> Self {
        ScheduleStep {
            assigned: report
                .assigned
                .iter()
                .map(|scheduled| ScheduledPairing {
                    task_id: scheduled.task_id.clone(),
                    agent_id: scheduled.agent_id.clone(),
                    assignment_id: scheduled.assignment.id.clone(),
                    recovery: scheduled.recovery.as_ref().map(RecoveryStep::from_recovery),
                })
                .collect(),
            refused: report
                .refused
                .iter()
                .map(|refused| RefusedPairing {
                    task_id: refused.task_id.clone(),
                    agent_id: refused.agent_id.clone(),
                    reason: refused.reason.to_string(),
                })
                .collect(),
            unassigned: report
                .unassigned
                .iter()
                .map(UnassignedTask::from_unassigned)
                .collect(),
        }
    }

    /// The assignment the runner holds for a task SCHEDULE handed out, if any.
    ///
    /// A caller running the loop needs this to name the assignment in the
    /// MONITOR steps that follow; it is a lookup rather than an index because
    /// the report is a proposal log, not a map.
    pub fn assignment_for(&self, task_id: &TaskId) -> Option<&director_domain::ids::AssignmentId> {
        self.assigned
            .iter()
            .find(|pairing| &pairing.task_id == task_id)
            .map(|pairing| &pairing.assignment_id)
    }
}

/// What one direct ASSIGN handoff produced.
#[derive(Debug, Clone, Serialize)]
pub struct AssignStep {
    /// The assignment now in force.
    pub assignment_id: director_domain::ids::AssignmentId,
    /// The agent holding the task.
    pub agent_id: director_domain::ids::AgentId,
    /// The task's status after the handoff.
    pub task_status: TaskStatus,
    /// The task's state version after the move, which a later write must present
    /// to be accepted.
    pub state_version: u64,
    /// The agent this handoff displaced, if the task was already held. The
    /// departed tenure is retained rather than deleted.
    pub displaced: Option<director_domain::ids::AgentId>,
}

/// What one MONITOR survey found.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MonitorStep {
    /// Every in-flight task the round looked at.
    pub in_flight: Vec<InFlightTask>,
    /// True when the round reclaimed nothing and found no anomalies.
    pub quiet: bool,
}

/// One in-flight task as MONITOR sees it.
#[derive(Debug, Clone, Serialize)]
pub struct InFlightTask {
    /// The task.
    pub task_id: TaskId,
    /// The runner's rendering of the lease's condition.
    pub condition: InFlightCondition,
}

/// What MONITOR concluded about one in-flight lease.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InFlightCondition {
    /// Held, and the holder is presumed to still be working.
    Live {
        /// The agent holding it.
        agent_id: director_domain::ids::AgentId,
        /// What the heartbeats said.
        liveness: Liveness,
    },
    /// The holder went past the stale window and the round reclaimed the lease.
    /// The task is `todo` and handable again.
    Expired {
        /// The agent whose tenure ended.
        agent_id: director_domain::ids::AgentId,
    },
    /// `in_progress` with no active assignment holding it. MONITOR reports this
    /// and writes nothing - deciding what to do with tenantless work is REPLAN's
    /// job.
    Orphaned,
}

/// What one simulated agent turn recorded.
///
/// **This is simulated execution.** See the module docs: the two records below
/// are what an agent would say about its own progress, and nothing ran. The
/// task this turn covers ends at `verification_pending` no matter what the
/// simulated agent "reported" - the field below is the store's answer, not the
/// agent's claim.
#[derive(Debug, Clone, Serialize)]
pub struct AgentTurn {
    /// The assignment the turn was for.
    pub assignment_id: director_domain::ids::AssignmentId,
    /// The session recorded against the tenure.
    pub session_id: Option<SessionId>,
    /// The status the task holds after the simulated report.
    pub task_status: TaskStatus,
    /// Always true. Present so a consumer reading a serialized run cannot
    /// mistake this record for genuine agent execution.
    pub simulated: bool,
}

/// What one VERIFY round concluded.
///
/// A task reaches `Done` through this record and through nothing else in the
/// runner. The verdicts are read from exit codes the executor observed; the
/// runner has no way to assert one.
#[derive(Debug, Clone, Default, Serialize)]
pub struct VerifyStep {
    /// Every task the round judged, in the store's order.
    pub judged: Vec<JudgedTask>,
}

impl VerifyStep {
    /// True when every task the round surveyed passed.
    pub fn all_passed(&self) -> bool {
        !self.judged.is_empty() && self.judged.iter().all(|judged| judged.passed)
    }
}

/// One task's verdict.
#[derive(Debug, Clone, Serialize)]
pub struct JudgedTask {
    /// The task judged.
    pub task_id: TaskId,
    /// The runner's rendering of [`verify::Verdict`].
    pub verdict: String,
    /// True only for a passing verdict.
    pub passed: bool,
    /// What the round did about each expected output.
    pub checks: Vec<CheckOutcome>,
    /// The status the round wrote, if it wrote one. `None` for a task the round
    /// could not judge, which stays `verification_pending` and is surveyed
    /// again next round.
    pub outcome: Option<TaskStatus>,
}

/// What VERIFY did about one expected output.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    /// The check ran and exited zero.
    Passed {
        /// The criterion it was for.
        criterion: String,
        /// The command that ran.
        command: String,
    },
    /// The check ran and did not exit zero, or it exceeded the executor's
    /// timeout.
    Failed {
        /// The criterion it was for.
        criterion: String,
        /// The command that ran.
        command: String,
        /// The process exit code; `-1` if unavailable.
        exit_code: i32,
        /// True if the executor killed the command for exceeding its limit.
        timed_out: bool,
        /// What the command wrote to stderr, trimmed.
        stderr: String,
    },
    /// The check could not be run at all - a broken environment, not a verdict
    /// on the work. The task stays `verification_pending`.
    Unrunnable {
        /// The criterion it was for.
        criterion: String,
        /// The check as written.
        command: String,
        /// Why it could not run, in the executor's own terms.
        error: String,
    },
    /// The criterion has no machine-checkable form. Reported, never blocking.
    Unchecked {
        /// The criterion Orqyn has no check for.
        criterion: String,
    },
}

/// What one REPLAN round did about the tasks the loop stopped on.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReplanStep {
    /// Tasks put back to `todo`.
    pub retried: Vec<TaskId>,
    /// Tasks put back to `todo` with an amended contract.
    pub reworked: Vec<TaskId>,
    /// Tasks made terminal.
    pub cancelled: Vec<TaskId>,
    /// Tasks that need a decision and did not get one. Left exactly as the
    /// survey found them.
    pub undecided: Vec<UndecidedTask>,
    /// Tasks whose condition the caller named for a task that is not in one of
    /// those conditions. The decision was refused and nothing was written.
    pub refused: Vec<RefusedDecision>,
}

/// A task REPLAN needs a decision for and did not get one.
#[derive(Debug, Clone, Serialize)]
pub struct UndecidedTask {
    /// The task.
    pub task_id: TaskId,
    /// The runner's rendering of [`replan::Condition`].
    pub condition: String,
}

/// A decision REPLAN refused to apply.
#[derive(Debug, Clone, Serialize)]
pub struct RefusedDecision {
    /// The task the decision named.
    pub task_id: TaskId,
    /// Why it was refused, as [`replan::RemediationError`]'s own message.
    pub reason: String,
}

/// The recovery briefing for a task, as the runner reports it.
///
/// Built entirely from persisted records by
/// [`recovery::recovery_context_from_store`], so it is reconstructible after a
/// restart and is safe to hand to a replacement agent.
#[derive(Debug, Clone, Serialize)]
pub struct RecoveryStep {
    /// The task being resumed.
    pub task_id: TaskId,
    /// The resume verdict.
    pub resume_status: ResumeStatus,
    /// The concrete first step for the receiving agent.
    pub next_action: String,
    /// Whether the replacement has anything concrete it can do. `false` today,
    /// because `RecentContext` has no persistence backing.
    pub actions_available: bool,
    /// Whether every field was reconstructed. `false` today for every build,
    /// because two gaps are structural limits of Orqyn's persistence.
    pub gaps_clean: bool,
    /// How many tenures the briefing carries.
    pub tenures: usize,
    /// How many verification records with evidence the briefing carries.
    pub verifications: usize,
    /// What this task may touch, as the task declared it.
    ///
    /// Carried so a replacement agent is told which paths are its own instead of
    /// inferring that from whatever is nearest. Empty means **undeclared**, which
    /// conflicts with everything — see
    /// [`director_domain::task::Task::scope_overlap`] — so an empty list is a
    /// conservative signal, not permission to roam.
    pub scope_paths: Vec<String>,
}

impl RecoveryStep {
    fn from_recovery(recovery: &Recovery) -> Self {
        RecoveryStep {
            task_id: recovery.task().clone(),
            resume_status: recovery.resume_status(),
            next_action: recovery.next_action().to_string(),
            actions_available: recovery.actions_available(),
            gaps_clean: recovery.gaps.is_clean(),
            tenures: recovery.package.tenures.len(),
            verifications: recovery.package.verifications.len(),
            // Copied as declared, not normalized: the briefing reports what
            // the task claims, and comparison semantics belong to
            // `Task::scope_overlap` at enforcement time.
            scope_paths: recovery.package.scope_paths.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

/// The driver that walks the loop.
///
/// Holds the injected dependencies and the [`IdGenerator`] that mints assignment
/// and session ids, and exposes one method per step. It is constructed once and
/// used across ticks: the `&mut self` on the id-minting steps is the only
/// interior state, and it is deliberately the only state the runner keeps — the
/// store is the record of what happened.
///
/// Nothing is read from the wall clock. Each step method takes the instant it
/// should act as though it is running at, which is what makes a run reproducible
/// and is why the `_at` variants are used throughout.
pub struct Runner<'s, M, E> {
    store: &'s Store,
    git: &'s GitService,
    memory: &'s M,
    executor: &'s E,
    working_dir: PathBuf,
    ids: IdGenerator,
}

impl<'s, M, E> Runner<'s, M, E>
where
    M: MemoryProvider,
    E: ExecutionProvider,
{
    /// Wire a runner to its dependencies.
    ///
    /// `working_dir` is the directory VERIFY runs its checks in — the checkout
    /// the loop is orchestrating. It is held rather than passed per call because
    /// every verification in a run belongs to the same tree.
    pub fn new(
        store: &'s Store,
        git: &'s GitService,
        memory: &'s M,
        executor: &'s E,
        working_dir: impl AsRef<Path>,
    ) -> Self {
        Runner {
            store,
            git,
            memory,
            executor,
            working_dir: working_dir.as_ref().to_path_buf(),
            ids: IdGenerator::new(),
        }
    }

    /// The store this runner persists through.
    pub fn store(&self) -> &'s Store {
        self.store
    }

    /// The directory VERIFY runs checks in.
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    /// Run one OBSERVE round: look at the repository and record what it found.
    ///
    /// Idempotent by construction — the step registers the project and
    /// repository only if they are not already known — which is what makes it
    /// safe to call on every tick. This is also the step that creates the
    /// project, so a run that plans anything has to observe first.
    pub async fn observe(
        &mut self,
        project_id: &ProjectId,
        repository_id: &RepositoryId,
        local_path: &str,
        now: DateTime<Utc>,
    ) -> Result<ObserveStep, RunnerError> {
        let observed = observe::observe_at(
            self.store,
            self.git,
            &mut self.ids,
            self.memory,
            project_id,
            repository_id,
            local_path,
            now,
        )
        .await?;
        Ok(ObserveStep::from_observed(
            &observed,
            project_id,
            repository_id,
        ))
    }

    /// Run one PLAN round: turn a request into a persisted, active plan.
    ///
    /// The plan, its tasks, and the plan's activation are one transaction inside
    /// the step, so a failure here leaves the previous plan active and nothing
    /// written.
    pub async fn plan(
        &mut self,
        request: PlanRequest,
        now: DateTime<Utc>,
    ) -> Result<PlanStep, RunnerError> {
        let planned = plan::plan_at(self.store, request, now).await?;
        Ok(PlanStep {
            plan_id: planned.plan.id.to_string(),
            objective: planned.plan.objective.clone(),
            tasks: planned
                .tasks
                .iter()
                .map(|task| PlannedTask {
                    id: task.id.clone(),
                    status: task.status,
                    depends_on: task.dependencies.clone(),
                    scope_paths: task.scope_paths.clone(),
                })
                .collect(),
            superseded: planned.superseded.map(|id| id.to_string()),
        })
    }

    /// Run one SCHEDULE round: hand every ready task to the best available
    /// agent.
    ///
    /// SCHEDULE applies its own pairings through ASSIGN, so this is the step
    /// that both decides and assigns. Pairings the live store refuses land in
    /// [`ScheduleStep::refused`] with the reason rather than failing the round:
    /// a refusal is a fact about the world the round read, not a failure of the
    /// round.
    pub async fn schedule(
        &mut self,
        project_id: &ProjectId,
        now: DateTime<Utc>,
    ) -> Result<ScheduleStep, RunnerError> {
        let report = schedule::schedule_at(self.store, &mut self.ids, project_id, now).await?;
        Ok(ScheduleStep::from_report(&report))
    }

    /// Run one ASSIGN handoff directly: name the task and the agent.
    ///
    /// SCHEDULE already assigns what it pairs, so a caller reaching for this is
    /// making a deliberate handoff the scheduler did not choose — which is
    /// exactly what the step's own docs describe as an auditable caller decision.
    /// It re-validates against the live store, so a stale request is refused
    /// rather than applied.
    pub async fn assign(
        &mut self,
        request: AssignRequest,
        now: DateTime<Utc>,
    ) -> Result<AssignStep, RunnerError> {
        let assigned = assign::assign_at(self.store, request, now).await?;
        Ok(AssignStep {
            assignment_id: assigned.assignment.id.clone(),
            agent_id: assigned.assignment.agent_id.clone(),
            task_status: assigned.task.status,
            state_version: assigned.task.state_version,
            displaced: assigned.displaced,
        })
    }

    /// Run one MONITOR survey: look at the work in flight and reclaim the
    /// leases that have run out.
    ///
    /// Idempotent — a task whose lease this round already expired is `todo`, so
    /// the next survey does not see it, and a holder that is quiet but not gone
    /// is reported every round and written never.
    pub async fn monitor(
        &mut self,
        project_id: &ProjectId,
        now: DateTime<Utc>,
    ) -> Result<MonitorStep, RunnerError> {
        let report = monitor::monitor_at(self.store, project_id, now).await?;
        let in_flight = report
            .in_flight
            .iter()
            .map(|entry| match entry {
                monitor::InFlight::Live {
                    task,
                    agent,
                    liveness,
                } => InFlightTask {
                    task_id: task.clone(),
                    condition: InFlightCondition::Live {
                        agent_id: agent.clone(),
                        liveness: *liveness,
                    },
                },
                monitor::InFlight::Expired(lease) => InFlightTask {
                    task_id: lease.task.id.clone(),
                    condition: InFlightCondition::Expired {
                        agent_id: lease.assignment.agent_id.clone(),
                    },
                },
                monitor::InFlight::Orphaned { task } => InFlightTask {
                    task_id: task.clone(),
                    condition: InFlightCondition::Orphaned,
                },
            })
            .collect();
        Ok(MonitorStep {
            quiet: report.is_quiet(),
            in_flight,
        })
    }

    /// Record that an agent began the work it was handed.
    ///
    /// Until this runs, an assignment is a handoff Orqyn recorded with no
    /// evidence the work ever began. A tenure has one invocation, so a second
    /// acknowledgment is refused rather than recorded.
    pub async fn acknowledge(
        &mut self,
        assignment_id: &director_domain::ids::AssignmentId,
        session_id: SessionId,
        now: DateTime<Utc>,
    ) -> Result<AcknowledgedStep, RunnerError> {
        let acknowledged = monitor::acknowledge_at(
            self.store,
            AcknowledgeRequest {
                assignment_id: assignment_id.clone(),
                session_id,
            },
            now,
        )
        .await?;
        Ok(AcknowledgedStep {
            assignment_id: acknowledged.assignment.id.clone(),
            session_id: acknowledged.session.id.clone(),
            agent_id: acknowledged.session.agent_id.clone(),
        })
    }

    /// Record that an agent finished the work it was handed.
    ///
    /// The task moves to `verification_pending` and no further: this step ends
    /// the agent's part and hands the judgment to VERIFY. `TaskStatus::Done` is
    /// not reachable from here by construction.
    pub async fn report_done(
        &mut self,
        assignment_id: &director_domain::ids::AssignmentId,
        now: DateTime<Utc>,
    ) -> Result<ReportedStep, RunnerError> {
        let reported = monitor::report_done_at(
            self.store,
            ReportRequest {
                assignment_id: assignment_id.clone(),
            },
            now,
        )
        .await?;
        Ok(ReportedStep {
            assignment_id: reported.assignment.id.clone(),
            task_id: reported.task.id.clone(),
            task_status: reported.task.status,
            session_closed_cleanly: reported.session.is_some(),
        })
    }

    /// Simulate one agent's whole turn: acknowledge, then report done.
    ///
    /// **This is simulated execution and is labelled as such in its own output.**
    /// Orqyn has no driver that launches an external agent, so nothing here runs
    /// a model, touches a repository, or makes the work real. What it records is
    /// the two reports an agent would make about itself — that it began, and
    /// that it finished — through the same MONITOR steps a real agent's reports
    /// would arrive through.
    ///
    /// The task it covers ends at `verification_pending` regardless of what was
    /// simulated. Nothing in this method, and nothing in this crate, can move a
    /// task to `Done`; only [`Runner::verify`] does, and only from exit codes the
    /// executor observed.
    ///
    /// The session id is minted from the runner's generator so a simulated turn
    /// is as auditable as a real one: "which invocation of which agent did this
    /// task" stays answerable.
    pub async fn simulate_agent_turn(
        &mut self,
        assignment_id: &director_domain::ids::AssignmentId,
        now: DateTime<Utc>,
    ) -> Result<AgentTurn, RunnerError> {
        let session_id = self.ids.next::<SessionId>("session");
        self.acknowledge(assignment_id, session_id.clone(), now)
            .await?;
        let reported = self.report_done(assignment_id, now).await?;
        Ok(AgentTurn {
            assignment_id: assignment_id.clone(),
            session_id: Some(session_id),
            task_status: reported.task_status,
            simulated: true,
        })
    }

    /// Run one VERIFY round: judge every task awaiting a verdict.
    ///
    /// The only step that runs commands, and the only one that can move a task
    /// to `Done`. It reads exit codes and nothing the simulated agent reported,
    /// so a task whose "work" was never done still fails here unless the
    /// repository genuinely satisfies its checks.
    ///
    /// A task that cannot be judged stays `verification_pending` and is
    /// surveyed again next round — which is safe because the round wrote nothing
    /// about it that would have to be unwound.
    ///
    /// The one step that does not take an instant: `verify::verify` has no `_at`
    /// variant, because the engine it delegates to timestamps its own attempt
    /// internally. Every other step here is caller-pinned.
    pub async fn verify(&mut self, project_id: &ProjectId) -> Result<VerifyStep, RunnerError> {
        let report =
            verify::verify(self.store, self.executor, project_id, &self.working_dir).await?;
        Ok(VerifyStep {
            judged: report
                .judged
                .iter()
                .map(|judged| JudgedTask {
                    task_id: judged.task.clone(),
                    verdict: render_verdict(&judged.verdict),
                    passed: matches!(judged.verdict, verify::Verdict::Passed),
                    checks: judged.checks.iter().map(render_check).collect(),
                    outcome: judged.outcome,
                })
                .collect(),
        })
    }

    /// Run one REPLAN round: apply the caller's decisions about the tasks the
    /// loop stopped on.
    ///
    /// Every decision is validated before any is applied, so a round that
    /// rejects one writes nothing at all. Tasks the survey finds that the caller
    /// does not decide are reported in [`ReplanStep::undecided`] and left alone.
    pub async fn replan(
        &mut self,
        request: ReplanRequest,
        now: DateTime<Utc>,
    ) -> Result<ReplanStep, RunnerError> {
        let report = replan::replan_at(self.store, request, now).await?;

        // Built from `surveyed` rather than from the convenience accessors,
        // because `ReplanReport::retried` folds `Rework` in with `Retry` and a
        // caller deciding what to do next needs to tell an amended contract from
        // a plain re-offer.
        let mut step = ReplanStep::default();
        for entry in &report.surveyed {
            match &entry.outcome {
                replan::Outcome::Applied {
                    remediation: replan::Remediation::Retry,
                    ..
                } => step.retried.push(entry.task.clone()),
                replan::Outcome::Applied {
                    remediation: replan::Remediation::Rework { .. },
                    ..
                } => step.reworked.push(entry.task.clone()),
                replan::Outcome::Applied {
                    remediation: replan::Remediation::Cancel,
                    ..
                } => step.cancelled.push(entry.task.clone()),
                replan::Outcome::Refused { error } => {
                    step.refused.push(RefusedDecision {
                        task_id: entry.task.clone(),
                        // `RemediationError` is `Debug` but not `Display`, so its
                        // own rendering is the stable name of the variant.
                        reason: format!("{error:?}"),
                    })
                }
                replan::Outcome::NotDecided => step.undecided.push(UndecidedTask {
                    task_id: entry.task.clone(),
                    condition: entry
                        .condition
                        .as_ref()
                        .map(render_condition)
                        .unwrap_or_else(|| "no condition recorded".to_string()),
                }),
                replan::Outcome::NothingNeeded => {}
            }
        }
        Ok(step)
    }

    /// Build the recovery briefing for a task from persisted state alone.
    ///
    /// Returns `None` when there is nothing to hand a replacement — the task
    /// does not exist, has no project, or has no checkpoint to resume from.
    /// That is deliberately a distinct answer from "recovered, with gaps":
    /// `None` means the absence was structural, and
    /// [`RecoveryOutcome`](recovery::RecoveryOutcome) is where a caller who needs
    /// to tell *why* builds `RecoveryBuilder` directly.
    ///
    /// The comparison is against the state Orqyn last recorded, which can be
    /// older than git. A caller that needs a current verdict observes first and
    /// uses [`recovery::recovery_context`].
    pub async fn recovery(
        &mut self,
        task_id: &TaskId,
    ) -> Result<Option<RecoveryStep>, RunnerError> {
        let recovery = recovery::recovery_context_from_store(self.store, task_id).await?;
        Ok(recovery.as_ref().map(RecoveryStep::from_recovery))
    }
}

/// What an acknowledgment recorded.
#[derive(Debug, Clone, Serialize)]
pub struct AcknowledgedStep {
    /// The tenure the session was attached to.
    pub assignment_id: director_domain::ids::AssignmentId,
    /// The invocation behind it.
    pub session_id: SessionId,
    /// The agent running it.
    pub agent_id: director_domain::ids::AgentId,
}

/// What a completion report recorded.
#[derive(Debug, Clone, Serialize)]
pub struct ReportedStep {
    /// The tenure that ended.
    pub assignment_id: director_domain::ids::AssignmentId,
    /// The task the report was about.
    pub task_id: TaskId,
    /// Where the report left the task. `verification_pending` by construction.
    pub task_status: TaskStatus,
    /// Whether the session closed cleanly. `false` when the report arrived
    /// before Orqyn learned which invocation was doing the work.
    pub session_closed_cleanly: bool,
}

/// Render a verdict as the stable string a serialized run carries.
fn render_verdict(verdict: &verify::Verdict) -> String {
    match verdict {
        verify::Verdict::Passed => "passed".to_string(),
        verify::Verdict::Failed => "failed".to_string(),
        verify::Verdict::Unverifiable(verify::UnverifiableReason::NoChecks) => {
            "unverifiable: no expected output names a check".to_string()
        }
        verify::Verdict::Unverifiable(verify::UnverifiableReason::EvidenceMissing) => {
            "unverifiable: the evidence is missing".to_string()
        }
    }
}

/// Render what a round did about one expected output.
fn render_check(check: &verify::CheckResult) -> CheckOutcome {
    match check {
        verify::CheckResult::Passed { criterion, command } => CheckOutcome::Passed {
            criterion: criterion.clone(),
            command: command.clone(),
        },
        verify::CheckResult::Failed(failed) => CheckOutcome::Failed {
            criterion: failed.criterion.clone(),
            command: failed.command.clone(),
            exit_code: failed.exit_code,
            timed_out: failed.timed_out,
            stderr: failed.stderr.trim().to_string(),
        },
        verify::CheckResult::Unrunnable {
            criterion,
            command,
            error,
        } => CheckOutcome::Unrunnable {
            criterion: criterion.clone(),
            command: command.clone(),
            error: error.clone(),
        },
        verify::CheckResult::Unchecked { criterion } => CheckOutcome::Unchecked {
            criterion: criterion.clone(),
        },
    }
}

/// Render a survey condition as the stable string a serialized run carries.
fn render_condition(condition: &replan::Condition) -> String {
    match condition {
        replan::Condition::Failed => "verification failed".to_string(),
        replan::Condition::Orphaned => {
            "in progress with no active assignment holding it".to_string()
        }
        replan::Condition::PremiseBroken {
            dependency,
            dependency_status,
        } => {
            format!("dependency {dependency} reached {dependency_status:?} and will never be done")
        }
    }
}
