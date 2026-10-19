//! Driving a real agent through a real harness.
//!
//! This is the step that replaces [`crate::runner::Runner::simulate_agent_turn`]
//! with something that launches a process. Everything it does around the launch
//! uses seams that already existed and are already tested — `monitor::acknowledge`
//! and `monitor::report_done` — so the session and assignment lifecycle is not
//! redefined here, only driven from outside instead of from a simulation.
//!
//! # The invariant this step exists to preserve
//!
//! **A harness is not evidence.** A process that exits zero having changed
//! nothing produces exactly the same [`HarnessOutcome`] as one that exits zero
//! having done the work. Nothing in this module can turn the first into a
//! passing verdict, because the verdict is computed by the verification engine
//! from exit codes of the task's own checks, run separately afterwards.
//!
//! So the worst thing a misbehaving harness can do here is make a *failed* task
//! look like it was attempted. It cannot make one look finished.
//!
//! # What happens when the harness does not run
//!
//! The completion report is the load-bearing decision here, and it is drawn
//! narrowly:
//!
//! | Outcome | `report_done`? | Why |
//! |---|---|---|
//! | Harness ran and exited | yes | the agent did finish its turn, whatever it decided |
//! | Missing binary / bad config / spawn failure | **no** | nothing ran; a completion report would be a fabrication |
//! | Timeout | **no** | the agent was killed mid-turn and never reported finishing |
//!
//! In the two "no" cases the assignment stays active with its lease, the task
//! stays `in_progress`, and [`crate::monitor::monitor`] expires the tenure on a
//! later round exactly as it would for any agent that vanished. That is the
//! honest state: an agent tenure began and did not report.
//!
//! # Not exactly-once
//!
//! Orqyn does not claim an agent runs at most once per assignment. It is
//! ordinary for a drive to be retried after a failure, and the lifecycle
//! records are written to be append-only rather than once-only:
//!
//! * The session id is derived from the assignment tenure (`SESS-{assignment_id}`),
//!   and the domain already forbids a second session on one tenure
//!   ([`crate::monitor::validate_acknowledgment`]), so a retry re-uses the same
//!   session rather than creating a rival one.
//! * `report_done` cannot end the same tenure twice: the store refuses it once
//!   the assignment is no longer active, so a retried drive is reported rather
//!   than silently applied.
//! * Verification is separately protected by the `tasks.state_version` compare
//!   and set, so a retry cannot overwrite an earlier verdict.
//!
//! What is *not* guaranteed is that a harness killed mid-flight left the working
//! tree untouched. Orqyn cannot know that; it can only judge the tree by
//! running the checks, which is what verification is for.

use std::time::Duration;

use director_domain::assignment::AgentAssignment;
use director_domain::harness::{HarnessDriver, HarnessError, HarnessOutcome, HarnessRun};
use director_domain::ids::{AssignmentId, ProjectId, SessionId, TaskId};
use director_domain::providers::MemoryProvider;
use director_domain::session::AgentSession;
use director_domain::store::TaskRepository;
use director_store::Store;

use crate::brief::{self, Brief, BriefLimits};
use crate::monitor::{AcknowledgeRequest, ReportRequest};

/// Every way driving an agent can fail.
///
/// Wraps the step errors rather than flattening them, matching the other steps
/// in this crate.
#[derive(Debug, thiserror::Error)]
pub enum AgentDriveError {
    /// The store refused one of the lifecycle writes.
    #[error("the store rejected the agent drive: {0}")]
    Store(String),
    /// The recovery context could not be assembled for the task.
    #[error("could not assemble the recovery context: {0}")]
    Recovery(String),
    /// The harness could not be driven.
    #[error(transparent)]
    Harness(#[from] HarnessError),
}

/// How one drive should be run: everything a caller can vary.
#[derive(Debug, Clone)]
pub struct DriveOptions {
    /// Repository directory the harness runs in. This is the repository root,
    /// **not** one of the task's scope paths: a scope path is a path inside the
    /// repository, and starting the process there would put the agent outside
    /// the project it is meant to change.
    pub working_dir: String,
    /// Wall-clock limit for the harness.
    pub timeout: Duration,
    /// Cap on captured bytes per stream.
    pub max_output_bytes: usize,
    /// Optional `--permission-mode`-style override.
    pub permission_mode: Option<String>,
    /// Optional model override.
    pub model: Option<String>,
    /// Additional directories the harness may reach.
    pub extra_dirs: Vec<String>,
    /// Briefing size limits.
    pub limits: BriefLimits,
    /// Bound on the external-memory retrieval that contributes to the briefing.
    pub external_memory_timeout: Duration,
}

impl DriveOptions {
    /// Options for a repository directory, with the documented defaults.
    pub fn in_dir(working_dir: impl Into<String>) -> Self {
        DriveOptions {
            working_dir: working_dir.into(),
            timeout: Duration::from_secs(900),
            max_output_bytes: 256 * 1024,
            permission_mode: None,
            model: None,
            extra_dirs: Vec::new(),
            limits: BriefLimits::default(),
            external_memory_timeout: crate::memory::EXTERNAL_MEMORY_TIMEOUT,
        }
    }
}

/// What one drive produced.
#[derive(Debug)]
pub struct AgentDrive {
    /// The project the driven task belongs to, so a caller can run verification
    /// without re-deriving it. Carried because the task row was read here.
    pub project_id: ProjectId,
    /// The assignment this drive acted on, as it stands afterwards.
    pub assignment: AgentAssignment,
    /// The session recorded for the tenure. Present whenever the drive got as
    /// far as acknowledging one.
    pub session: Option<AgentSession>,
    /// What the harness actually did. `None` when it never ran.
    pub outcome: Option<HarnessOutcome>,
    /// The briefing the harness was given, and whether it was cut to fit.
    pub brief: Brief,
    /// What external memory contributed to the briefing, and why.
    ///
    /// Carried so a caller can report a substrate outage rather than discovering
    /// it by noticing the briefing is thinner than it should be. Never an error:
    /// the authoritative sections do not depend on it.
    pub external_memory: crate::memory::Retrieval,
    /// Why the drive could not run, when it could not.
    ///
    /// A drive that never launched returns this together with an acknowledged
    /// session and no outcome, so the caller learns what happened without the
    /// lifecycle records claiming an agent finished.
    pub not_run: Option<HarnessError>,
}

impl AgentDrive {
    /// True when a harness process actually ran to completion.
    ///
    /// False for a launch that failed, and also false for a harness that ran and
    /// reported failure — this asks "did it run", never "did it succeed".
    pub fn executed(&self) -> bool {
        self.outcome.is_some()
    }

    /// True when the harness process ran and exited cleanly.
    ///
    /// Still not a statement about the task: see the module docs.
    pub fn harness_succeeded(&self) -> bool {
        self.outcome
            .as_ref()
            .map(HarnessOutcome::succeeded)
            .unwrap_or(false)
    }
}

/// Drive one assignment's task through a real harness.
///
/// The call order is fixed and each step is a precondition for the next:
///
/// 1. Assemble recovery context from persisted records.
/// 2. Render the briefing.
/// 3. Acknowledge the assignment, recording the session for this tenure.
/// 4. Launch the harness and observe it.
/// 5. Report completion **only if the harness ran**.
///
/// Verification is deliberately *not* run here. [`crate::runner::Runner`]
/// sequences it after this returns, so a caller can inspect the execution
/// evidence before any verdict exists.
pub async fn drive_at<D: HarnessDriver>(
    store: &Store,
    driver: &D,
    task_id: &TaskId,
    assignment_id: &AssignmentId,
    options: &DriveOptions,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<AgentDrive, AgentDriveError> {
    // No substrate configured, so the briefing carries no external copy and says
    // so. An existing caller is unaffected by any of this.
    drive_inner(
        store,
        driver,
        task_id,
        assignment_id,
        options,
        crate::memory::Retrieval::Absent,
        now,
    )
    .await
}

/// [`drive_at`], with an external memory substrate consulted for the briefing.
///
/// Retrieval happens here, in the orchestration layer, rather than inside the
/// renderer — [`crate::brief::render_with_external`] is pure and takes the outcome
/// as data. The retrieval is bounded by
/// [`DriveOptions::external_memory_timeout`], and a failure or timeout yields
/// [`crate::memory::Retrieval::Unavailable`], which the briefing renders as
/// "could not be reached" rather than as an empty result.
///
/// SQLite state is loaded and committed independently of this call, so an
/// unreachable substrate costs the agent one clearly-labelled section and
/// nothing else. The drive itself proceeds.
pub async fn drive_at_with_memory<D: HarnessDriver, M: MemoryProvider>(
    store: &Store,
    driver: &D,
    task_id: &TaskId,
    assignment_id: &AssignmentId,
    options: &DriveOptions,
    memory: &M,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<AgentDrive, AgentDriveError> {
    // Consulted after the authoritative state is loaded and before the briefing is
    // rendered, so a retrieved page can never be mistaken for the record.
    let retrieval =
        crate::memory::retrieve_checkpoints(memory, task_id, options.external_memory_timeout).await;
    drive_inner(
        store,
        driver,
        task_id,
        assignment_id,
        options,
        retrieval,
        now,
    )
    .await
}

/// The drive itself, given an already-computed retrieval outcome.
///
/// Split so [`drive_at`] and [`drive_at_with_memory`] share one implementation and
/// neither has to name the other's type parameter.
async fn drive_inner<D: HarnessDriver>(
    store: &Store,
    driver: &D,
    task_id: &TaskId,
    assignment_id: &AssignmentId,
    options: &DriveOptions,
    external: crate::memory::Retrieval,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<AgentDrive, AgentDriveError> {
    // 1. Recovery context. A task Orqyn has never checkpointed yields no package,
    //    which is the ordinary state for a first agent rather than a failure, so
    //    the briefing is built from the task alone and says outright that there
    //    is no prior history. Refusing here would make the driver useless for the
    //    one case it exists for.
    let recovery = crate::recovery::recovery_context_from_store(store, task_id)
        .await
        .map_err(|err| AgentDriveError::Recovery(err.to_string()))?;

    // The task row is read rather than assumed: the package carries a task id and
    // a scope, but the objective and expected outputs a briefing needs live on the
    // task itself.
    let task = store
        .tasks()
        .get_task(task_id)
        .await
        .map_err(|err| AgentDriveError::Recovery(err.to_string()))?;

    // 2. Briefing.
    let project_id = task.project_id.clone().ok_or_else(|| {
        AgentDriveError::Recovery(format!(
            "task {task_id} names no project, so there is nothing to verify against"
        ))
    })?;
    let external_memory = external;
    tracing::debug!(
        task = %task_id.as_str(),
        outcome = %external_memory.summary(),
        "external memory consulted for the briefing"
    );

    let rendered =
        brief::render_with_external(recovery.as_ref(), &task, &external_memory, &options.limits);

    // 3. Acknowledge. The session id is derived from the tenure, matching the
    //    rule `simulate_agent_turn` already uses, so the simulated and real
    //    paths cannot produce competing sessions for one assignment.
    let session_id = SessionId::from_string(format!("SESS-{assignment_id}"));
    let acknowledged = crate::monitor::acknowledge_at(
        store,
        AcknowledgeRequest {
            assignment_id: assignment_id.clone(),
            session_id,
        },
        now,
    )
    .await
    .map_err(|err| AgentDriveError::Store(err.to_string()))?;

    let mut run = HarnessRun::new(options.working_dir.clone(), rendered.text.clone());
    run.timeout = options.timeout;
    run.max_output_bytes = options.max_output_bytes;
    run.permission_mode = options.permission_mode.clone();
    run.model = options.model.clone();
    run.extra_dirs = options.extra_dirs.clone();

    // 4. Launch and observe.
    let driven = driver.drive(&run).await;

    let outcome = match driven {
        Ok(outcome) => outcome,
        Err(err) => {
            // 5, negative case. No completion report: the harness did not run to
            //    a finish. The acknowledged session stays on record and the lease
            //    is left for MONITOR to expire, which is the honest state for an
            //    agent that began and vanished.
            return Ok(AgentDrive {
                project_id,
                assignment: acknowledged.assignment,
                session: Some(acknowledged.session),
                outcome: None,
                brief: rendered,
                external_memory,
                not_run: Some(err),
            });
        }
    };

    // 5. The harness ran and finished, so the tenure ends here. A non-zero exit
    //    is reported the same way a zero exit is: the agent finished its turn.
    //    Whether the work was any good is verification's question, not this
    //    step's.
    let reported = crate::monitor::report_done_at(
        store,
        ReportRequest {
            assignment_id: assignment_id.clone(),
        },
        now,
    )
    .await
    .map_err(|err| AgentDriveError::Store(err.to_string()))?;

    Ok(AgentDrive {
        project_id,
        assignment: reported.assignment,
        session: Some(acknowledged.session),
        outcome: Some(outcome),
        brief: rendered,
        external_memory,
        not_run: None,
    })
}
