//! Receiving a handoff: what a replacement agent is told, and what it is allowed
//! to do about it.
//!
//! # The one idea this module exists to enforce
//!
//! A handoff is **an audit record plus untrusted context**. It says an outgoing
//! agent stopped holding a task and someone else may pick it up. It is not
//! authority, not instructions, and not evidence that any work was done.
//!
//! So:
//!
//!   * What a receiver is told comes from [`RecoveryBuilder`] — the task row, its
//!     verdicts, its actions, its checkpoint — and never from the handoff's
//!     narrative. The narrative is carried alongside, labelled as the outgoing
//!     agent's own account, and is never fed to a model as if it were state.
//!   * Accepting records attribution and nothing else. It does not complete a
//!     task, and it does **not** create, move, or imply an assignment. Execution
//!     authority still comes from ASSIGN, under its lease and scope guards — which
//!     is the only thing that ever has been able to grant it.
//!   * A handoff that cannot be shown to describe the task as it stands is
//!     reported as stale and stays auditable. Staleness does not block receiving;
//!     it changes what the receiver is entitled to rely on.
//!
//! # What is refused
//!
//! Claim-once (only the addressed agent, only while open), an unknown handoff, a
//! task that no longer exists, and a task whose status makes receiving meaningless
//! — `done` or `cancelled` are terminal, so there is nothing to take over.

use director_domain::handoff::{Handoff, HandoffCause, HandoffState};
use director_domain::ids::{AgentId, HandoffId, SessionId, TaskId};
use director_domain::providers::MemoryProvider;
use director_domain::store::{CheckpointRepository, HandoffRepository, TaskRepository};
use director_domain::task::{Task, TaskStatus};
use director_domain::{CheckpointId, ResumeStatus, StoreError};
use director_store::Store;

use crate::brief::{self, Brief, BriefLimits};
use crate::memory::{self, Retrieval};
use crate::recovery::{Recovery, RecoveryBuilder, RecoveryOutcome};

/// How long the receiving path will wait on an external substrate.
///
/// Matches the bound the drive path already applies, and is applied *inside*
/// [`memory::retrieve_checkpoints`], which is already bounded and never returns an
/// error. Nothing here retries, so a slow substrate costs this much time once and
/// then yields [`Retrieval::Unavailable`] rather than blocking the continuation.
const EXTERNAL_MEMORY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Why a handoff could not be accepted.
///
/// Each variant names a check, so a refusal tells an operator which invariant
/// stopped it rather than just failing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReceiveError {
    /// Orqyn has no record of this handoff id.
    #[error("no handoff with id {0}")]
    UnknownHandoff(String),
    /// The handoff is addressed to a different agent. Claim-once means
    /// addressee-only, not first-come.
    #[error("handoff {handoff} is addressed to {addressed_to}, so {acceptor} cannot accept it")]
    WrongRecipient {
        /// The handoff that was refused.
        handoff: String,
        /// Who it was addressed to.
        addressed_to: String,
        /// Who tried to accept it.
        acceptor: String,
    },
    /// The handoff is no longer open: already accepted, or expired.
    #[error("handoff {handoff} is {state:?} and cannot be accepted")]
    NotOpen {
        /// The handoff that was refused.
        handoff: String,
        /// Its current state.
        state: HandoffState,
    },
    /// The task this handoff names is gone, so there is nothing to take over.
    #[error("handoff {handoff} names task {task}, which Orqynn has no record of")]
    TaskGone {
        /// The handoff that was refused.
        handoff: String,
        /// The task it named.
        task: String,
    },
    /// The task has reached a terminal state, so receiving it is meaningless.
    #[error("task {task} is {status:?}, which is terminal; there is nothing to take over")]
    TaskTerminal {
        /// The task that was refused.
        task: String,
        /// The status that made it terminal.
        status: TaskStatus,
    },
}

/// The persisted handoff, read together with the authoritative state it claims to
/// describe.
///
/// The two are deliberately separate fields. `record` is what an outgoing agent
/// said; `task` and `freshness` are what Orqynn knows. Collapsing them into one
/// view would let the narrative read as state.
#[derive(Debug, Clone)]
pub struct HandoffInspection {
    /// The stored record: identity, attribution, narrative, provenance.
    pub record: Handoff,
    /// The task as the store currently holds it.
    pub task: Task,
    /// Whether the snapshot still describes the task as it stands.
    pub freshness: Freshness,
}

/// How far a handoff's snapshot can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The recorded source matches the task and checkpoint as they stand now.
    Current,
    /// The recorded source no longer matches, so the narrative is context only.
    Stale {
        /// Why it could not be shown to be current, in words.
        because: &'static str,
    },
}

impl Freshness {
    /// A phrase a report can print.
    pub fn describe(self) -> &'static str {
        match self {
            Freshness::Current => "current",
            Freshness::Stale { because } => because,
        }
    }
}

/// List the handoffs recorded for one task, newest first.
pub async fn list_for_task(store: &Store, task: &TaskId) -> Result<Vec<Handoff>, StoreError> {
    store.handoffs().handoffs_for_task(task).await
}

/// Read one handoff and place it against the task as the store currently holds it.
///
/// This is the whole point of the module: the answer is not "what does the record
/// say" but "what does the record say, and is it still true".
pub async fn inspect(
    store: &Store,
    handoff: &HandoffId,
) -> Result<HandoffInspection, ReceiveError> {
    let record = match store.handoffs().get_handoff(handoff).await {
        Ok(record) => record,
        Err(_) => return Err(ReceiveError::UnknownHandoff(handoff.as_str().to_string())),
    };

    let task =
        store
            .tasks()
            .get_task(&record.task_id)
            .await
            .map_err(|_| ReceiveError::TaskGone {
                handoff: record.id.as_str().to_string(),
                task: record.task_id.as_str().to_string(),
            })?;

    // Freshness is judged against the task's current version and its current
    // checkpoint. A snapshot with no recorded derivation point is stale, because
    // "we have nothing that contradicts this" is not the same as "this is true".
    let latest = store
        .checkpoints()
        .latest_checkpoint(&record.task_id)
        .await
        .ok()
        .flatten();
    let current_checkpoint: Option<&CheckpointId> = latest.as_ref().map(|c| &c.id);
    let freshness = if record.is_current_for(task.state_version, current_checkpoint) {
        Freshness::Current
    } else {
        Freshness::Stale {
            because: "the recorded source no longer matches the task; treat as context",
        }
    };

    Ok(HandoffInspection {
        record,
        task,
        freshness,
    })
}

/// Accept a handoff on behalf of its addressed agent.
///
/// **Attribution only.** This records who took the work over. It does not move
/// the task, does not complete it, and does not create or imply an assignment —
/// a receiver still needs ASSIGN before it may act, and the acceptance says
/// nothing that would satisfy it.
pub async fn accept(
    store: &Store,
    handoff: &HandoffId,
    acceptor: &AgentId,
    session: &SessionId,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<Handoff, ReceiveError> {
    // Read first so each refusal names its own reason; the repository's own
    // claim-once check still decides the race, and this read is never what makes
    // acceptance safe.
    let record = store
        .handoffs()
        .get_handoff(handoff)
        .await
        .map_err(|_| ReceiveError::UnknownHandoff(handoff.as_str().to_string()))?;

    if record.to_agent != *acceptor {
        return Err(ReceiveError::WrongRecipient {
            handoff: handoff.as_str().to_string(),
            addressed_to: record.to_agent.as_str().to_string(),
            acceptor: acceptor.as_str().to_string(),
        });
    }
    if record.state != HandoffState::Open {
        return Err(ReceiveError::NotOpen {
            handoff: handoff.as_str().to_string(),
            state: record.state,
        });
    }

    // Receiving is only meaningful while the work is still outstanding.
    let task =
        store
            .tasks()
            .get_task(&record.task_id)
            .await
            .map_err(|_| ReceiveError::TaskGone {
                handoff: handoff.as_str().to_string(),
                task: record.task_id.as_str().to_string(),
            })?;
    if task.status.is_terminal() {
        return Err(ReceiveError::TaskTerminal {
            task: task.id.as_str().to_string(),
            status: task.status,
        });
    }

    store
        .handoffs()
        .accept_handoff(handoff, acceptor, session, at)
        .await
        .map_err(|_| ReceiveError::NotOpen {
            // The repository refused: another caller won the race, or the state
            // moved between the read above and the write. Both are the same fact
            // to the caller - this handoff is no longer open to them.
            handoff: handoff.as_str().to_string(),
            state: HandoffState::Accepted,
        })
}

/// What a receiving agent should be told, and what it may rely on.
///
/// `brief` is rendered from the recovery package and is the authoritative answer.
/// `context` is the untrusted narrative: it is returned as data, never merged into
/// the brief, so a caller that forgets this distinction still cannot mistake it
/// for state.
#[derive(Debug)]
pub struct Continuation {
    /// The rendered briefing, built from persisted Orqynn state.
    pub brief: Brief,
    /// What Orqyn's own recovery says about whether the work can resume.
    pub resume_status: ResumeStatus,
    /// Why no recovery package could be built, when none could.
    pub recovery_gap: Option<String>,
    /// The outgoing agents' own account. Context, never instructions.
    pub context: Vec<Handoff>,
    /// The newest handoff for this task, if there is one.
    pub accepted_from: Option<AgentId>,
    /// External-memory retrieval, when a provider was supplied.
    ///
    /// Advisory reference material, never authority. `Absent` when no provider was
    /// given or the substrate holds nothing about this task; `Unavailable` when a
    /// provider was asked and could not answer, which is a different fact from an
    /// empty result and is never silently reported as one.
    pub external: Retrieval,
}

/// Assemble what a receiving agent is told, from authoritative state.
///
/// The handoff contributes attribution and nothing else. The brief comes from
/// [`RecoveryBuilder`] every time, which is what makes a stale snapshot harmless:
/// the receiver is asked the current question of the current tree, and the old
/// narrative is attached beside the answer rather than in place of it.
///
/// This is the original signature, kept so every existing caller is unaffected.
/// It consults no external substrate; see [`continuation_with_memory`].
pub async fn continuation(
    store: &Store,
    task: &TaskId,
    limits: BriefLimits,
) -> Result<Continuation, StoreError> {
    // `None::<InMemoryProvider>` pins the provider type: with no substrate to ask,
    // no retrieval is attempted and the result is `Absent`.
    continuation_with_memory::<director_adapters::InMemoryProvider>(store, None, task, limits).await
}

/// Assemble a continuation, optionally consulting external memory.
///
/// `memory` is any [`MemoryProvider`]. Passing `None` is exactly the original
/// [`continuation`] behaviour, so this is the single implementation and there is
/// no second code path that could drift.
///
/// # What external memory is, and is not
///
/// It is **reference material**, and it is never authority. Retrieved pages are
/// rendered into a clearly-labelled section of the brief by
/// [`brief::render_with_external`]; they cannot move a task, change a verdict, or
/// shorten recovery. Retrieval is best-effort: a substrate that errors or exceeds
/// its bound yields [`Retrieval::Unavailable`] and the authoritative continuation
/// is still produced in full. Nothing about a retrieval failure is allowed to
/// touch a task, an assignment, a handoff, or a verification.
///
/// Association is enforced by the retrieval path itself
/// ([`memory::belongs_to_task`]), so a page about a different task is dropped and
/// counted rather than shown.
pub async fn continuation_with_memory<M: MemoryProvider>(
    store: &Store,
    memory: Option<&M>,
    task: &TaskId,
    limits: BriefLimits,
) -> Result<Continuation, StoreError> {
    let task_row = store.tasks().get_task(task).await?;
    let outcome = RecoveryBuilder::new(store, task.clone())
        .from_stored_state()
        .build()
        .await
        .map_err(|err| StoreError::ConstraintViolation(err.to_string()))?;

    let (package, gap) = match outcome {
        RecoveryOutcome::Built(recovery) => (Some(*recovery), None),
        RecoveryOutcome::UnknownTask(id) => (
            None,
            Some(format!(
                "Orqyn has no record of task {}; the id is wrong, or this is not the database \
                 the task was planned in",
                id.as_str()
            )),
        ),
        RecoveryOutcome::NoCheckpoint(id) => (
            None,
            Some(format!(
                "task {} has never been checkpointed, so there is no progress narrative to \
                 resume from; its history is still answerable",
                id.as_str()
            )),
        ),
        RecoveryOutcome::NoProject(id) => (
            None,
            Some(format!(
                "task {} is not claimed for any project, so there is no project state to \
                 compare against",
                id.as_str()
            )),
        ),
        RecoveryOutcome::NoStoredProjectState(id) => (
            None,
            Some(format!(
                "task {}'s project has never been observed, so there is no baseline to tell \
                 changed work from a moved-on repository",
                id.as_str()
            )),
        ),
    };

    // Retrieval happens after the authoritative package is built, and cannot
    // change it. `retrieve_checkpoints` is already bounded and never returns an
    // error, so there is no failure path here that could discard local state.
    let external = match memory {
        Some(provider) => {
            memory::retrieve_checkpoints(provider, task, EXTERNAL_MEMORY_TIMEOUT).await
        }
        None => Retrieval::Absent,
    };

    let brief = brief::render_with_external(package.as_ref(), &task_row, &external, &limits);
    let resume_status = package
        .as_ref()
        .map(|recovery: &Recovery| recovery.resume_status())
        .unwrap_or(ResumeStatus::Insufficient);

    let records = list_for_task(store, task).await?;
    let accepted_from = records
        .iter()
        .find(|record| record.state == HandoffState::Accepted)
        .map(|record| record.from_agent.clone());

    Ok(Continuation {
        brief,
        resume_status,
        recovery_gap: gap,
        context: records,
        accepted_from,
        external,
    })
}

/// A one-line summary of a record's cause, for a report. Never guesses.
pub fn cause_phrase(cause: Option<HandoffCause>) -> &'static str {
    match cause {
        Some(cause) => cause.describe(),
        None => "unknown - not established from persisted records",
    }
}
