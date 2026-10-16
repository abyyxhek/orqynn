//! Recovery context — what a replacement agent is handed when it picks work up.
//!
//! Phase 11 proved the store alone is enough to brief a replacement mid-flight,
//! and it did it the hard way: `a_replacement_agent_can_be_given_everything_the_prior_agent_left_behind`
//! issues six unrelated repository queries and assembles the picture by hand.
//! That test was explicit about being a placeholder, and about the piece that
//! was missing — no step wrote the checkpoint it then read back.
//!
//! Phase 12 removes both halves of that gap. [`crate::checkpoint`] writes the
//! checkpoint automatically, and this module assembles the picture in one place.
//! A replacement — or a driver briefing one — asks one question and gets one
//! answer:
//!
//! ```text
//! RecoveryBuilder::new(store, task_id)
//!    ↓  .observed(git_state, git_history)   // or .from_stored_state()
//!    ↓  .without_histories()                // optional
//!    ↓  .build()
//! ContinuationPackage { task, checkpoint, tenures, verdicts, transitions, … }
//!    ↓
//! RecoveryOutcome::Built(recovery) | UnknownTask | NoCheckpoint | …
//!    ↓
//! ResumeStatus::Ready | Stale | Conflicted | Blocked | Insufficient
//! ```
//!
//! [`recovery_context`] and [`recovery_context_from_store`] are thin wrappers
//! over [`RecoveryBuilder`] that keep the single-question shape SCHEDULE already
//! has; the builder is for a caller that needs to choose the observation source,
//! skip the history queries, or see *why* nothing was built.
//!
//! ## The resume verdict
//!
//! [`Checkpoint::compare_with`] is the guard that has existed since Phase 5 and
//! had no caller. It now has one. The checkpoint records the commit and branch
//! it was taken against; the recovery reads the project's state and asks whether
//! that world still exists. [`ResumeStatus::Stale`] is not an error — it is the
//! honest answer when the project moved on, and it tells the replacement to
//! reconcile rather than trust the narrative.
//!
//! ## What is here, and what is not
//!
//! Everything in a [`ContinuationPackage`] comes from persisted records, and a
//! briefing now reconstructs both kinds of activity it previously could not:
//!
//! - [`RecentContext`] is built from the `task_actions` table, oldest first, and
//!   is bounded and compacted by [`RecentContext`] itself rather than by a second
//!   limit here. It is a record of what Orqynn actually observed.
//! - [`Blocker`](director_domain::Blocker) is read from the `blockers` table,
//!   **including cleared blockers** - the row is retained when a blocker stops
//!   blocking, so "we fixed it" stays distinguishable from "we stopped trying".
//!
//! Two honest limits remain, and neither is a missing table:
//!
//! - **No production execution path records actions yet.** The storage, ordering,
//!   bounding, and restart-durability of action history are real; nothing in the
//!   loop writes to it, because Orqynn has no agent driver to observe. An empty
//!   window is therefore reported as *unavailable history* rather than as a
//!   complete history: Orqynn cannot tell "an agent worked without recording" from
//!   "nothing happened", and it does not guess.
//! - **`ResumeStatus` derives its blocked verdict from task status, not from
//!   blockers.** A persisted open blocker does not move a task to
//!   [`TaskStatus::Blocked`], and nothing in the store or the loop ever writes
//!   that status today - REPLAN deliberately declines to, so it does not own
//!   another entity's lifecycle. A task can therefore carry an open blocker
//!   while its resume verdict reads `Ready` or `Stale`. The blockers are in the
//!   briefing either way.
//!
//! A gap is a statement about what could be reconstructed, never about the work:
//! an empty window with `recent_context_unavailable` set means nothing was
//! recorded, not that the agent did nothing. Without the flag the two read
//! identically.
//!
//! No field is invented. A fact that cannot be sourced is reported as absent, not
//! with a guess.

use director_domain::checkpoint::{Checkpoint, ContinuationPackage, ResumeStatus};
use director_domain::context::RecentContext;
use director_domain::ids::{AgentId, TaskId};
use director_domain::state::ProjectState;
use director_domain::store::StoredProjectState;
use director_domain::task::{Task, TaskStatus};
use director_domain::{ActionRepository, BlockerRepository};
use director_domain::{
    AgentRepository, AssignmentRepository, CheckpointRepository, DecisionRepository,
    ProjectStateRepository, TaskRepository, VerificationRepository,
};
use director_store::Store;

use crate::CheckpointError;

/// The recovery picture for one task, plus what the assembly could and could not
/// reconstruct.
///
/// This wraps [`ContinuationPackage`] — the domain's existing recovery type —
/// rather than replacing it, because the package is the shape a replacement
/// consumes and the domain owns it. What the wrapper adds is the honest
/// accounting around the one field the store cannot fill: a caller that wants
/// to know whether the package's recent-action window is real or a placeholder
/// asks [`Self::actions_available`], rather than guessing at a bug.
#[derive(Debug, Clone, PartialEq)]
pub struct Recovery {
    /// The domain's recovery package: task, checkpoint, tenures, verdicts,
    /// transitions, decisions, and the resume verdict.
    pub package: ContinuationPackage,
    /// What this build could not reconstruct from persisted records, and why.
    /// A gap is a statement about Orqyn's persistence, not about the work: an
    /// empty `recent_context` with `recent_context_unavailable` set means Orqyn
    /// keeps no action window, not that the agent did nothing.
    pub gaps: RecoveryGaps,
}

impl Recovery {
    /// The task the recovery is for.
    pub fn task(&self) -> &TaskId {
        &self.package.task_id
    }

    /// Orqyn's verdict on whether the task can be resumed as-is.
    pub fn resume_status(&self) -> ResumeStatus {
        self.package.resume_status
    }

    /// The latest checkpoint for the task.
    pub fn checkpoint(&self) -> &Checkpoint {
        &self.package.checkpoint
    }

    /// The concrete first step for the receiving agent.
    pub fn next_action(&self) -> &str {
        &self.package.next_action
    }

    /// Whether the package's recent-action window is populated from persisted
    /// actions.
    ///
    /// `true` when at least one action was reconstructed from `task_actions`;
    /// `false` for an empty window. `false` is deliberately not read as "the agent
    /// did nothing" - no production execution path records actions yet, so an
    /// empty window means Orqynn has no record either way. See [`Self::gaps`]
    /// for the full ledger.
    pub fn actions_available(&self) -> bool {
        !self.gaps.recent_context_unavailable
    }

    /// Everything the build could not reconstruct, by name. Read this before
    /// reporting an empty field to a human, so "Orqyn does not keep that" and
    /// "nothing happened" stay distinguishable.
    pub fn gaps(&self) -> &RecoveryGaps {
        &self.gaps
    }
}

/// What a recovery build could not reconstruct from persisted records.
///
/// Both records now exist - `task_actions` and `blockers` - so neither gap is a
/// limit of Orqynn's persistence. `recent_context_unavailable` is a statement
/// about **this task** (nothing was recorded against it), and
/// `blockers_unavailable` is `false` on every build, because an empty blocker
/// list is an answer rather than a gap. They are still reported per build rather
/// than assumed as constants, so a caller reading a package never has to know
/// which fields are backed to read an empty one correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoveryGaps {
    /// The briefing's action window is empty.
    ///
    /// No longer a statement about the build — actions are persisted now. It is a
    /// statement about **this task**: nothing was ever recorded against it. That
    /// is not the same as "nothing happened", and the flag exists so a caller
    /// cannot mistake the two. Orqyn records what its own steps observe, so a task
    /// driven entirely outside Orqyn legitimately has an empty window.
    pub recent_context_unavailable: bool,
    /// `Blocker` rows are persisted, so this is no longer structural. It stays
    /// in the type because a package serialized before blockers were recorded
    /// will deserialize without them, and a reader needs to be able to say so.
    pub blockers_unavailable: bool,
}

impl RecoveryGaps {
    /// The gaps that apply to a build that actually reconstructed something.
    ///
    /// Both records now exist, so a blocker list is always trustworthy and the
    /// only question left is whether *this* task has any activity on record. An
    /// empty window is reported as unavailable precisely because Orqyn cannot
    /// distinguish "an agent worked without recording" from "nothing happened",
    /// and the honest answer to that uncertainty is to say so rather than to
    /// present an empty window as a complete history.
    fn observed(recent_context: &RecentContext) -> Self {
        RecoveryGaps {
            recent_context_unavailable: recent_context.is_empty(),
            blockers_unavailable: false,
        }
    }

    /// True when nothing the build could have reconstructed was missing. False
    /// for every build today, because both gaps are structural; it exists so a
    /// caller that will be true for it later does not have to be written later.
    pub fn is_clean(&self) -> bool {
        !self.recent_context_unavailable && !self.blockers_unavailable
    }
}

/// What a [`RecoveryBuilder`] produced — including the reasons it produced
/// nothing.
///
/// [`recovery_context`] collapses every failure to assemble into a single
/// `None`, because SCHEDULE only wants "is there a briefing or not". A caller
/// that has to *explain* the absence — a driver answering "why can't I resume
/// this", or a report counting stranded tasks — needs the distinction: a task
/// with no checkpoint is a data question, a task Orqyn never heard of is a
/// caller's mistake, and a project never observed is an OBSERVE that has not
/// run. This enum is that explanation, and it is built from persisted records
/// alone.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryOutcome {
    /// A package was assembled. Its [`Recovery::gaps`] record what could not be
    /// reconstructed. Boxed because the package is the largest thing the enum
    /// holds and every other variant carries only an id.
    Built(Box<Recovery>),
    /// Orqyn has no record of this task. The id is wrong, or the task was never
    /// written.
    UnknownTask(TaskId),
    /// The task exists but has never been checkpointed, so there is no progress
    /// narrative to hand a replacement. Its history is still answerable; only
    /// resumption is not.
    NoCheckpoint(TaskId),
    /// The task is not claimed for any project, so Orqyn holds no project state
    /// to compare a checkpoint against. Reachable only on the stored-state path.
    NoProject(TaskId),
    /// The task's project has never been observed, so Orqyn holds no normalized
    /// state for it. Reachable only on the stored-state path.
    NoStoredProjectState(TaskId),
}

impl RecoveryOutcome {
    /// The assembled package, if the build produced one. The single-question
    /// shape [`recovery_context`] has always answered.
    pub fn package(self) -> Option<Recovery> {
        match self {
            RecoveryOutcome::Built(recovery) => Some(*recovery),
            _ => None,
        }
    }

    /// True when the build produced nothing at all.
    pub fn is_absent(&self) -> bool {
        !matches!(self, RecoveryOutcome::Built(_))
    }

    /// A one-line, human-facing reason the build produced nothing, for a driver
    /// that has to say why a replacement cannot be briefed. `None` when the
    /// build succeeded.
    pub fn absence_reason(&self) -> Option<String> {
        match self {
            RecoveryOutcome::Built(_) => None,
            RecoveryOutcome::UnknownTask(id) => {
                Some(format!("Orqyn has no record of task {}", id.as_str()))
            }
            RecoveryOutcome::NoCheckpoint(id) => Some(format!(
                "task {} has no checkpoint to resume from",
                id.as_str()
            )),
            RecoveryOutcome::NoProject(id) => Some(format!(
                "task {} belongs to no project, so Orqyn holds no project state for it",
                id.as_str()
            )),
            RecoveryOutcome::NoStoredProjectState(id) => Some(format!(
                "task {}'s project has never been observed, so Orqyn holds no state for it",
                id.as_str()
            )),
        }
    }
}

/// Which project state a [`RecoveryBuilder`] compares the checkpoint against.
enum Observation {
    /// A state a git observation just produced, supplied by the caller with the
    /// commit history that goes with it. The accurate path. Boxed because
    /// [`ProjectState`] is the largest thing the builder holds and this enum is
    /// constructed on the stored path for every build — the indirection keeps
    /// that path from carrying a fresh-observation-sized payload it never uses.
    Fresh {
        /// The observed project state.
        state: Box<ProjectState>,
        /// The commit history the observation collected.
        history: Vec<director_domain::CommitInfo>,
    },
    /// Orqyn's own normalized belief, read from the store. Carries no working
    /// tree and no commit history, so its comparison errs conservative.
    Stored,
}

/// Assembles a [`ContinuationPackage`] for one task from persisted records
/// alone.
///
/// The loop's SCHEDULE step needs one thing from recovery — "is there a
/// briefing, and is it current" — and [`recovery_context`] answers that. This
/// builder is for a caller that needs more control over the assembly: which
/// project state to compare against, whether the history collections are worth
/// loading, and an outcome that distinguishes the reasons nothing was built.
///
/// ```text
/// RecoveryBuilder::new(store, task_id)
///    ↓
/// RecoveryOutcome::Built(recovery)   // or UnknownTask / NoCheckpoint / …
/// ```
///
/// ## The two sources of project state
///
/// [`Self::observed`] is the fresh path: the caller passes what a git observation
/// just saw, so the comparison is against reality and [`Checkpoint::compare_with`]
/// can distinguish a tree change from a head advance. [`Self::from_stored_state`]
/// is Orqyn's own normalized belief, which carries no working tree and no commit
/// history — the reconstructed state reports a clean tree and no history, so the
/// comparison errs conservative (a checkpoint taken at the same commit reads
/// [`StateComparison::Unchanged`], anything else reads as stale). A caller that
/// needs the real verdict observes first.
///
/// ## What the builder will not do
///
/// It will not invent a field it cannot source. [`RecentContext`] and
/// [`Blocker`](director_domain::Blocker) are both read from real tables now,
/// and both are populated only with what those tables hold - actions are not
/// inferred from working-tree changes, commits, or task status, and a
/// blocker that was never recorded is not invented to explain a stuck task.
pub struct RecoveryBuilder<'s> {
    store: &'s Store,
    task_id: TaskId,
    observation: Observation,
    /// Whether to load the tenure, verification, transition, and decision
    /// histories. A verdict-only caller turns this off.
    histories: bool,
}

impl<'s> RecoveryBuilder<'s> {
    /// Begin assembling a recovery for `task_id`.
    ///
    /// Defaults to the stored-state path with histories loaded: the conservative
    /// comparison and the complete briefing. Call [`Self::observed`] to compare
    /// against a fresh git observation, or [`Self::without_histories`] for a
    /// verdict-only build.
    pub fn new(store: &'s Store, task_id: TaskId) -> Self {
        RecoveryBuilder {
            store,
            task_id,
            observation: Observation::Stored,
            histories: true,
        }
    }

    /// Compare the checkpoint against a freshly observed project state and its
    /// commit history. The accurate path, for a caller that is mid-observation
    /// or has just run one.
    pub fn observed(
        mut self,
        state: ProjectState,
        history: Vec<director_domain::CommitInfo>,
    ) -> Self {
        self.observation = Observation::Fresh {
            state: Box::new(state),
            history,
        };
        self
    }

    /// Compare against the project state Orqyn last recorded. This is the
    /// default; it is expressed as a method so a caller's intent reads at the
    /// call site rather than in the absence of a call.
    pub fn from_stored_state(mut self) -> Self {
        self.observation = Observation::Stored;
        self
    }

    /// Skip the tenure, verification, transition, and decision histories.
    ///
    /// For a caller that only needs the resume verdict — a readiness check, a
    /// poll — the four history queries are work the briefing does not use. The
    /// package's history fields come back empty **by request**, which is a
    /// different fact from empty because nothing happened, so the build records
    /// no gap for them.
    pub fn without_histories(mut self) -> Self {
        self.histories = false;
        self
    }

    /// Assemble the package, or report precisely why it could not be assembled.
    ///
    /// Everything the assembled package states comes from a record the store
    /// returned. Nothing is derived from a caller's claim about the world, and
    /// nothing that Orqyn does not persist is filled in by guesswork — see
    /// [`RecoveryGaps`].
    pub async fn build(self) -> Result<RecoveryOutcome, CheckpointError> {
        // A task Orqyn has no record of has nothing to hand a replacement. This
        // is the `Insufficient` case in the domain's vocabulary, reported as a
        // distinct outcome rather than as a package full of `None` so "cannot
        // recover" stays a different answer from "recovered, with gaps".
        let Some(task) = read_task(self.store, &self.task_id).await? else {
            return Ok(RecoveryOutcome::UnknownTask(self.task_id));
        };

        // The observation the checkpoint is compared against. The fresh path
        // takes the caller's; the stored path has to find the project and its
        // state first, and either being absent is an outcome rather than an
        // error — a task with no project was never claimed for one, and a
        // project with no state was never observed.
        let (observed, history) = match self.observation {
            Observation::Fresh { state, history } => (*state, history),
            Observation::Stored => {
                let Some(project_id) = task.project_id.clone() else {
                    return Ok(RecoveryOutcome::NoProject(self.task_id));
                };

                let Some(state) = self
                    .store
                    .project_state()
                    .get_project_state(&project_id)
                    .await?
                else {
                    return Ok(RecoveryOutcome::NoStoredProjectState(self.task_id));
                };

                // The store's state carries no working tree and no commit
                // history — those live in the git layer — so the reconstructed
                // state reports a clean tree and no history. The comparison is
                // therefore conservative in the safe direction.
                (project_state_from_stored(&state), Vec::new())
            }
        };

        // The checkpoint is the thing a resumption continues from. Without one
        // the task's history is still answerable, but there is no progress
        // narrative to hand over — which is the `Insufficient` case the domain
        // names.
        let Some(checkpoint) = self
            .store
            .checkpoints()
            .latest_checkpoint(&self.task_id)
            .await?
        else {
            return Ok(RecoveryOutcome::NoCheckpoint(self.task_id));
        };

        // The histories a briefing carries, unless the caller asked for a
        // verdict-only build. Read here — after the checkpoint check — so a
        // task with nothing to resume does not cost four queries.
        let (tenures, verifications, transitions, decisions) = if self.histories {
            (
                self.store
                    .assignments()
                    .assignment_history(&self.task_id)
                    .await?,
                self.store
                    .verifications()
                    .verifications_for_task(&self.task_id)
                    .await?,
                self.store.tasks().task_history(&self.task_id).await?,
                self.store
                    .decisions()
                    .decisions_for_task(&self.task_id)
                    .await?,
            )
        } else {
            (Vec::new(), Vec::new(), Vec::new(), Vec::new())
        };

        // Agents currently known to Orqyn, so a replacement knows who else is
        // in the room. Derived from the registry rather than the assignments,
        // because the question is about presence, not tenures.
        let agents = self.store.agents().list_agents().await?;
        let active_agents: Vec<AgentId> = agents.into_iter().map(|agent| agent.id).collect();

        // The two things a briefing could not reconstruct before, because nothing
        // recorded them. Both are read from the store rather than inferred: an
        // absent action is reported as an absent action, never as one invented
        // from a status change or a changed file.
        let blockers = self.store.blockers().blockers_for_task(&task.id).await?;
        let actions = self.store.actions().actions_for_task(&task.id).await?;

        // The bounded window is built by pushing into `RecentContext`, so the
        // bound and the compaction rule are enforced by the same code that owns
        // them, rather than by a second limit written here.
        let mut recent_context = RecentContext::for_task(task.id.clone());
        for action in &actions {
            recent_context.push(action.clone());
        }

        // The resume verdict: the checkpoint's recorded world against the world
        // as it now stands. `resume_status_for` computes the comparison itself
        // rather than taking the one below, so a caller cannot hand it a state
        // and a comparison that disagree — see its own docs.
        let comparison = checkpoint.compare_with(&observed, &history);
        let resume_status = resume_status_for(&task, &checkpoint, &observed, &history);

        // The next action comes from the checkpoint — it was written by the
        // observation that saw the work, so it is derived rather than guessed
        // here. A task that cannot be resumed tells the replacement what to do
        // about that instead of repeating a stale instruction.
        let next_action = next_action_for(&task, &checkpoint, resume_status);

        let gaps = RecoveryGaps::observed(&recent_context);
        let package = ContinuationPackage {
            task_id: task.id.clone(),
            task_state_version: task.state_version,
            // The scope comes from the task row this builder already loaded from
            // the store, so it is the authoritative persisted declaration and not
            // something reconstructed from a checkpoint, a message, or the files a
            // commit happened to touch. Copied as written: normalization and
            // comparison happen at enforcement time, not here.
            scope_paths: task.scope_paths.clone(),
            checkpoint,
            tenures,
            verifications,
            transitions,
            decisions,
            recent_context,
            observed_state: observed,
            comparison,
            active_agents,
            blockers: blockers.iter().map(|blocker| blocker.id.clone()).collect(),
            blocker_details: blockers,
            next_action,
            resume_status,
        };

        Ok(RecoveryOutcome::Built(Box::new(Recovery { gaps, package })))
    }
}

impl<'s> std::fmt::Debug for RecoveryBuilder<'s> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The store is not Debug by design — it is a pool, not a value — so the
        // builder reports what it was asked for rather than what it is reading.
        f.debug_struct("RecoveryBuilder")
            .field("task_id", &self.task_id)
            .field(
                "observation",
                &match self.observation {
                    Observation::Fresh { .. } => "fresh",
                    Observation::Stored => "stored",
                },
            )
            .field("histories", &self.histories)
            .finish()
    }
}

/// Assemble everything a replacement agent needs for one task, from persisted
/// records alone.
///
/// This is the function the Phase 11 test did by hand. Given a task id it reads
/// the task, its current checkpoint, its tenure history, its verification
/// history, its transition history, its decisions, and the project's observed
/// state, then folds them into a [`ContinuationPackage`] with a resume verdict
/// from [`Checkpoint::compare_with`].
///
/// Returns `None` when there is nothing to hand a replacement — the task does
/// not exist, or it has no checkpoint to resume from. Both are
/// [`ResumeStatus::Insufficient`] in the domain's vocabulary; reporting them as
/// `None` rather than as a package full of `None` keeps "cannot recover" a
/// distinct answer from "recovered, with gaps".
pub async fn recovery_context(
    store: &Store,
    task_id: &TaskId,
    observed: &ProjectState,
    history: &[director_domain::CommitInfo],
) -> Result<Option<Recovery>, CheckpointError> {
    // The fresh-observation entry point over [`RecoveryBuilder`]. A caller that
    // needs to distinguish *why* nothing was built — rather than just that
    // nothing was — builds directly and matches on [`RecoveryOutcome`].
    let outcome = RecoveryBuilder::new(store, task_id.clone())
        .observed(observed.clone(), history.to_vec())
        .build()
        .await?;

    Ok(outcome.package())
}

/// Assemble a recovery context from the store's normalized project state rather
/// than a freshly observed one.
///
/// Use this when the caller is not in the middle of an observation and wants the
/// recovery picture against the state Orqyn last recorded. The comparison is
/// against that stored state, so it can be older than git — a caller that needs
/// a current verdict observes first and calls [`recovery_context`].
///
/// Returns `None` when the task does not exist, has no project, has no recorded
/// project state, or has no checkpoint — the same `Insufficient` answer
/// [`recovery_context`] gives, for the same reasons.
pub async fn recovery_context_from_store(
    store: &Store,
    task_id: &TaskId,
) -> Result<Option<Recovery>, CheckpointError> {
    // The stored-state entry point over [`RecoveryBuilder`]. The comparison is
    // against the state Orqyn last recorded, which can be older than git — the
    // defaults of [`RecoveryBuilder::new`].
    let outcome = RecoveryBuilder::new(store, task_id.clone()).build().await?;

    Ok(outcome.package())
}

/// Read a task, treating its absence as `None` rather than as an error. The
/// store reports a missing task as [`StoreError::NotFound`], which is a
/// caller's ordinary question about an id, not a failure of the recovery.
async fn read_task(store: &Store, task_id: &TaskId) -> Result<Option<Task>, CheckpointError> {
    match store.tasks().get_task(task_id).await {
        Ok(task) => Ok(Some(task)),
        Err(director_domain::StoreError::NotFound(_)) => Ok(None),
        Err(err) => Err(CheckpointError::from(err)),
    }
}

/// The resume verdict, decided from the task's state, the checkpoint's
/// staleness, and the observed project.
///
/// This computes the comparison itself — the checkpoint against the observed
/// state and history — rather than accepting a precomputed one, so a caller
/// cannot pass a [`StateComparison`] derived from a *different* project state
/// than the `observed` one it also passes. The two can drift apart silently, and
/// a resume verdict built on a mismatched pair is worse than none.
///
/// The order of the checks is the order of the questions a replacement asks, and
/// each one wins outright:
///
/// 1. **Conflicted** — the task and its latest verdict disagree. A task that
///    reads `done` with no passing verdict, or `failed` with one that passed,
///    is in a state two sources disagree about; a replacement must not pick a
///    side, so this beats everything below it.
/// 2. **Blocked** — the task itself is `blocked`. The blocker list is not
///    persisted, but the status is, and it is the authoritative signal.
/// 3. **Insufficient** — the task is not in flight. Resumption is a question
///    about work in progress; a task that is `todo`, terminal, or awaiting
///    verification has nothing to resume.
/// 4. **Stale** — the checkpoint describes a world that moved on, per
///    [`Checkpoint::compare_with`]. The replacement reconciles rather than
///    trusts.
/// 5. **Ready** — the checkpoint is current and the task is in flight.
pub fn resume_status_for(
    task: &Task,
    checkpoint: &Checkpoint,
    observed: &ProjectState,
    history: &[director_domain::CommitInfo],
) -> ResumeStatus {
    use director_domain::StateComparison;

    let comparison = checkpoint.compare_with(observed, history);

    // 1. A state two sources disagree about. The verdicts on record are the
    //    independent judgment; the task status is what the loop concluded. If
    //    the latest verdict passed but the task is not on its way to done, or
    //    the task claims done without a passing verdict, the recovery reports
    //    the disagreement instead of resolving it.
    if task_conflicts_with_verdicts(task, checkpoint) {
        return ResumeStatus::Conflicted;
    }

    // 2. The task is blocked. This verdict comes from the *status*, not from the
    //    persisted blocker list: nothing in the store or the loop ever writes
    //    `TaskStatus::Blocked` (REPLAN declines to, so it does not own another
    //    entity's lifecycle), so an open blocker can coexist with a non-blocked
    //    status. The blockers are still in the briefing - the caller sees them -
    //    but they do not decide this verdict.
    if task.status == TaskStatus::Blocked {
        return ResumeStatus::Blocked;
    }

    // 3. Nothing in flight to resume.
    if !matches!(task.status, TaskStatus::InProgress) {
        return ResumeStatus::Insufficient;
    }

    // 4. and 5. The checkpoint's world against the observed one. A checkpoint
    //    that cannot be compared — no recorded commit — is treated as needing
    //    reconciliation rather than trusted.
    match comparison {
        StateComparison::Unchanged if !observed.is_dirty() => ResumeStatus::Ready,
        StateComparison::Unchanged => ResumeStatus::Stale,
        StateComparison::WorkingTreeChanged
        | StateComparison::HeadAdvanced
        | StateComparison::BranchChanged
        | StateComparison::CommitGone => ResumeStatus::Stale,
    }
}

/// Whether the task's status and its latest verdict disagree.
///
/// [`TaskStatus::Done`] is reachable only through a passing verdict; a task that
/// reads `done` without one reached a state verification never granted it. The
/// reverse disagreement — a passing verdict on a task that is not `done` — is
/// the same shape from the other direction. Either way a replacement must not
/// pick a side, so the recovery reports [`ResumeStatus::Conflicted`] and lets a
/// caller adjudicate.
fn task_conflicts_with_verdicts(task: &Task, checkpoint: &Checkpoint) -> bool {
    let passed = checkpoint_says_passed(checkpoint);

    match task.status {
        // A task that reads done without a passing verdict on record: done is
        // reachable only through verification, so this is a state nothing
        // should have produced.
        TaskStatus::Done if !passed => true,
        // A task that verification passed, sitting anywhere other than done or
        // on its way there: the verdict and the lifecycle disagree.
        TaskStatus::Backlog
        | TaskStatus::Todo
        | TaskStatus::Blocked
        | TaskStatus::Cancelled
        | TaskStatus::Failed
            if passed =>
        {
            true
        }
        _ => false,
    }
}

/// Whether the checkpoint's own evidence indicates verification passed.
///
/// A checkpoint records [`TestResults`] from the observation that took it, when
/// it records any. This is deliberately a *signal about agreement*, not a route
/// to `done`: nothing here can make a task complete, and the real verdict is the
/// verification history the package also carries. A checkpoint with no test
/// results reports `false`, which is "no evidence of a pass", not "evidence of a
/// failure".
fn checkpoint_says_passed(checkpoint: &Checkpoint) -> bool {
    checkpoint
        .test_results
        .as_ref()
        .map(|results| results.failed == 0 && results.passed > 0)
        .unwrap_or(false)
}

/// What the replacement should do first.
///
/// For a resumable task this is the checkpoint's own next action — derived from
/// the observation that saw the work, so it tracks the evidence. For a task that
/// cannot be resumed as-is, the instruction names *that*: reconcile, clear the
/// blocker, or let verification judge. A stale checkpoint's instruction is
/// repeated only as a reference point, never as an instruction to trust.
pub fn next_action_for(task: &Task, checkpoint: &Checkpoint, status: ResumeStatus) -> String {
    match status {
        ResumeStatus::Ready => checkpoint.next_action.clone(),
        ResumeStatus::Stale => format!(
            "the project moved on since this checkpoint was taken — reconcile {} against the current state before continuing. The checkpoint's own next step was: {}",
            task.title, checkpoint.next_action
        ),
        ResumeStatus::Conflicted => format!(
            "{}'s status and its verification history disagree — a caller must adjudicate before this work continues",
            task.title
        ),
        ResumeStatus::Blocked => format!(
            "{} is blocked; clear the blocker before resuming", task.title
        ),
        ResumeStatus::Insufficient => match task.status {
            TaskStatus::VerificationPending => format!(
                "{} is awaiting independent verification — nothing to resume until it is judged", task.title
            ),
            TaskStatus::Done => format!("{} is complete; there is nothing to resume", task.title),
            TaskStatus::Failed => format!(
                "{} failed verification; rework the contract before assigning it again", task.title
            ),
            TaskStatus::Cancelled => format!("{} was cancelled; it is not resumable", task.title),
            _ => format!("{} is not in progress; there is nothing to resume", task.title),
        },
    }
}

/// Build a [`ProjectState`] from the store's normalized state, for the recovery
/// path that is not mid-observation.
///
/// The store's state does not carry the working tree or the recent commits —
/// those live in the git layer — so the reconstructed state reports a clean tree
/// and no commit history. That makes its comparison *conservative*: a checkpoint
/// taken at the same commit reads as [`director_domain::StateComparison::Unchanged`]
/// and anything else reads as needing reconciliation, which is the safe
/// direction to err.
fn project_state_from_stored(state: &StoredProjectState) -> ProjectState {
    ProjectState {
        branch: state.branch.clone(),
        head_commit: state.head_commit.clone(),
        working_tree: Vec::new(),
        recent_commits: Vec::new(),
        test_results: None,
        observed_on: director_domain::ids::MachineId::from_string("unknown"),
        observed_at: state.last_observed_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use director_domain::checkpoint::{Checkpoint, ResumeStatus};
    use director_domain::ids::{CheckpointId, MachineId};
    use director_domain::state::{ChangedFile, CommitInfo, FileChange};
    use director_domain::task::Task;

    fn task(status: TaskStatus) -> Task {
        let mut task = Task::new(TaskId::from_string("AUTH"), "auth", "the objective");
        task.status = status;
        task
    }

    fn checkpoint(commit: Option<&str>) -> Checkpoint {
        let mut cp = Checkpoint::new(
            CheckpointId::from_string("CHK-1"),
            TaskId::from_string("AUTH"),
            "the objective",
            "progress so far",
            "the next step",
        );
        cp.commit_sha = commit.map(str::to_string);
        cp.branch = Some("main".into());
        cp
    }

    fn observed(commit: &str) -> ProjectState {
        ProjectState {
            branch: Some("main".into()),
            head_commit: commit.into(),
            working_tree: Vec::new(),
            recent_commits: Vec::new(),
            test_results: None,
            observed_on: MachineId::from_string("MACH-a"),
            observed_at: chrono::Utc::now(),
        }
    }

    fn history(shas: &[&str]) -> Vec<CommitInfo> {
        shas.iter()
            .map(|sha| CommitInfo {
                sha: sha.to_string(),
                summary: "a commit".into(),
                author: "an author".into(),
                committed_at: chrono::Utc::now(),
                parents: Vec::new(),
                committer: "a committer".into(),
                message: "a commit".into(),
            })
            .collect()
    }

    fn dirty(commit: &str) -> ProjectState {
        let mut state = observed(commit);
        state.working_tree.push(ChangedFile {
            path: "src/lib.rs".into(),
            change: FileChange::Modified,
        });
        state
    }

    #[test]
    fn an_in_flight_task_on_the_checkpointed_commit_is_ready() {
        let cp = checkpoint(Some("abc"));
        let status = resume_status_for(
            &task(TaskStatus::InProgress),
            &cp,
            &observed("abc"),
            &history(&["abc"]),
        );
        assert_eq!(status, ResumeStatus::Ready);
    }

    #[test]
    fn a_dirty_tree_on_the_same_commit_is_stale_not_ready() {
        let cp = checkpoint(Some("abc"));
        let status = resume_status_for(
            &task(TaskStatus::InProgress),
            &cp,
            &dirty("abc"),
            &history(&["abc"]),
        );
        assert_eq!(status, ResumeStatus::Stale);
    }

    #[test]
    fn an_advanced_head_is_stale() {
        let cp = checkpoint(Some("abc"));
        let status = resume_status_for(
            &task(TaskStatus::InProgress),
            &cp,
            &observed("def"),
            &history(&["def", "abc"]),
        );
        assert_eq!(status, ResumeStatus::Stale);
    }

    #[test]
    fn a_rebased_away_commit_is_stale() {
        let cp = checkpoint(Some("abc"));
        let status = resume_status_for(
            &task(TaskStatus::InProgress),
            &cp,
            &observed("def"),
            &history(&["def"]),
        );
        assert_eq!(status, ResumeStatus::Stale);
    }

    #[test]
    fn a_branch_change_is_stale() {
        let cp = checkpoint(Some("abc"));
        let mut state = observed("def");
        state.branch = Some("feature/auth".into());
        let status = resume_status_for(
            &task(TaskStatus::InProgress),
            &cp,
            &state,
            &history(&["def", "abc"]),
        );
        assert_eq!(status, ResumeStatus::Stale);
    }

    #[test]
    fn a_blocked_task_reports_blocked_before_staleness() {
        let cp = checkpoint(Some("abc"));
        let status = resume_status_for(
            &task(TaskStatus::Blocked),
            &cp,
            &observed("abc"),
            &history(&["abc"]),
        );
        assert_eq!(status, ResumeStatus::Blocked);
    }

    #[test]
    fn a_task_that_is_not_in_flight_is_insufficient() {
        let cp = checkpoint(Some("abc"));
        // `Done` is deliberately absent: the conflict check runs first, and a
        // checkpoint written by the automatic path records no test results, so a
        // task that reads done has no evidence of a pass on record and reports
        // `Conflicted` rather than `Insufficient`. That case is covered by
        // `a_done_task_with_no_passing_evidence_is_conflicted` below — the two
        // answers are different facts, and listing `Done` here would assert the
        // precedence this module documents the other way around.
        for status in [
            TaskStatus::Todo,
            TaskStatus::Backlog,
            TaskStatus::VerificationPending,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert_eq!(
                resume_status_for(&task(status), &cp, &observed("abc"), &history(&["abc"])),
                ResumeStatus::Insufficient,
                "{status:?} is not resumable"
            );
        }
    }

    #[test]
    fn a_done_task_with_no_passing_evidence_is_conflicted() {
        // Done is reachable only through verification. A checkpoint with no test
        // results reports no evidence of a pass, so a task that reads done is a
        // state verification never granted it.
        let cp = checkpoint(Some("abc"));
        let status = resume_status_for(
            &task(TaskStatus::Done),
            &cp,
            &observed("abc"),
            &history(&["abc"]),
        );
        assert_eq!(status, ResumeStatus::Conflicted);
    }

    #[test]
    fn a_conflict_wins_over_staleness_for_a_done_task() {
        // Conflicted is checked first, so a task whose status and evidence
        // disagree reports the disagreement even when the checkpoint is also
        // stale. Blocked is a different matter: a blocked task with no
        // evidence of a pass is not conflicted, so its own status wins — see
        // `a_blocked_task_reports_blocked_before_staleness`.
        let cp = checkpoint(Some("abc"));
        let status = resume_status_for(
            &task(TaskStatus::Done),
            &cp,
            &observed("def"),
            &history(&["def", "abc"]),
        );
        assert_eq!(status, ResumeStatus::Conflicted);
    }

    #[test]
    fn a_checkpoint_with_passing_tests_on_a_done_task_is_not_conflicted() {
        use director_domain::state::TestResults;
        let mut cp = checkpoint(Some("abc"));
        cp.test_results = Some(TestResults {
            passed: 10,
            failed: 0,
            skipped: 0,
            failures: Vec::new(),
            run_at: chrono::Utc::now(),
        });
        let status = resume_status_for(
            &task(TaskStatus::Done),
            &cp,
            &observed("abc"),
            &history(&["abc"]),
        );
        assert_eq!(status, ResumeStatus::Insufficient, "done and not in flight");
    }

    #[test]
    fn a_ready_task_repeats_the_checkpoints_next_action() {
        let cp = checkpoint(Some("abc"));
        let action = next_action_for(&task(TaskStatus::InProgress), &cp, ResumeStatus::Ready);
        assert_eq!(action, "the next step");
    }

    #[test]
    fn a_stale_task_is_told_to_reconcile_not_trust() {
        let cp = checkpoint(Some("abc"));
        let action = next_action_for(&task(TaskStatus::InProgress), &cp, ResumeStatus::Stale);
        assert!(action.contains("reconcile"));
        assert!(
            action.contains("the next step"),
            "the stale step is referenced, not trusted"
        );
    }

    #[test]
    fn a_conflicted_task_is_told_to_adjudicate() {
        let cp = checkpoint(Some("abc"));
        let action = next_action_for(&task(TaskStatus::Done), &cp, ResumeStatus::Conflicted);
        assert!(action.contains("adjudicate"));
    }

    #[test]
    fn a_verification_pending_task_is_told_to_wait_for_judgment() {
        let cp = checkpoint(Some("abc"));
        let action = next_action_for(
            &task(TaskStatus::VerificationPending),
            &cp,
            ResumeStatus::Insufficient,
        );
        assert!(action.contains("awaiting independent verification"));
    }

    #[test]
    fn a_failed_task_is_told_to_rework_the_contract() {
        let cp = checkpoint(Some("abc"));
        let action = next_action_for(&task(TaskStatus::Failed), &cp, ResumeStatus::Insufficient);
        assert!(action.contains("rework"));
    }

    #[test]
    fn a_cancelled_task_is_told_it_is_not_resumable() {
        let cp = checkpoint(Some("abc"));
        let action = next_action_for(
            &task(TaskStatus::Cancelled),
            &cp,
            ResumeStatus::Insufficient,
        );
        assert!(action.contains("not resumable"));
    }

    #[test]
    fn an_empty_action_window_is_reported_as_unavailable_rather_than_as_a_history() {
        // The same concern the old `blocker_list_is_empty` test covered, restated for
        // the case that is now real: a task with nothing on record must not present an
        // empty window as though it were a complete history.
        let mut empty = RecentContext::for_task(TaskId::from_string("TASK-1"));
        let gaps = RecoveryGaps::observed(&empty);
        assert!(
            gaps.recent_context_unavailable,
            "no recorded actions means the briefing cannot say what happened"
        );
        assert!(
            !gaps.blockers_unavailable,
            "blockers are persisted now, so an empty blocker list is an answer, not a gap"
        );
        assert!(
            !gaps.is_clean(),
            "a window with nothing in it is not a clean build"
        );

        // One recorded action flips it, which is what makes the flag mean something.
        let mut recorded = director_domain::action::Action::new(
            director_domain::ids::ActionId::from_string("ACT-1"),
            director_domain::action::ActionKind::Command,
            "ran the tests",
        );
        recorded.task_id = Some(TaskId::from_string("TASK-1"));
        empty.push(recorded);
        let gaps = RecoveryGaps::observed(&empty);
        assert!(!gaps.recent_context_unavailable);
        assert!(
            gaps.is_clean(),
            "with both records readable there is nothing to report"
        );
    }

    #[test]
    fn a_stored_state_rebuilds_as_a_clean_project_state() {
        // The conservative reconstruction: no working tree, no commit history,
        // so a same-commit checkpoint reads unchanged and anything else reads
        // stale. Verified directly because the recovery path depends on the
        // direction of that error.
        let state = StoredProjectState {
            project_id: director_domain::ids::ProjectId::from_string("PROJ-1"),
            repository_id: None,
            branch: Some("main".into()),
            head_commit: "abc".into(),
            working_tree_clean: true,
            observation_version: 1,
            last_observed_at: chrono::Utc::now(),
            state_version: 1,
        };
        let observed = project_state_from_stored(&state);
        assert_eq!(observed.head_commit, "abc");
        assert!(observed.working_tree.is_empty());
        assert!(observed.recent_commits.is_empty());
        assert!(!observed.is_dirty());
    }

    #[test]
    fn a_state_with_no_test_results_reports_no_evidence_of_a_pass() {
        // Not evidence of a failure — evidence of nothing. This is what keeps
        // the conflict check from firing on every checkpoint taken before
        // verification ran.
        assert!(!checkpoint_says_passed(&checkpoint(Some("abc"))));
    }
}
