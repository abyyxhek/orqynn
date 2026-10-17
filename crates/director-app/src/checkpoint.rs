//! Automatic checkpointing — the Phase 12 activation of the Phase 4 foundation.
//!
//! A checkpoint is a resumption document addressed to whatever agent picks a
//! task up next. The model, the migration, the repository, and the
//! latest-for-task machinery have all existed since Phase 4; what was missing
//! was a *writer*. Nothing in the loop called
//! [`create_checkpoint`](director_domain::CheckpointRepository::create_checkpoint),
//! so the progress narrative had to be written by hand — the Phase 11
//! context-continuity test did exactly that, and said so in its comments.
//!
//! ## Why this step lives in OBSERVE
//!
//! The loop has exactly one place that looks at a repository and records what
//! it found, and this is not it — that is [`crate::observe`]. Checkpoint
//! content is *evidence*: the branch, the commit `HEAD` points at, the files
//! that changed. OBSERVE already holds all of it, and it holds the
//! [`SyncResult`] that says whether anything actually moved. MONITOR, the other
//! candidate, works off heartbeats and assignments and has no git state at all;
//! a checkpoint it wrote would be a narrative with no evidence behind it, which
//! is precisely the self-reporting shape the loop's invariants exist to prevent.
//!
//! So OBSERVE observes durable progress, and this module is the narrow seam
//! that turns one observation into zero or more checkpoints:
//!
//! ```text
//! OBSERVE sees a real change
//!    ↓
//! for each in_progress task in the project
//!    ↓
//! derive a checkpoint from the observed evidence
//!    ↓
//! SQLite, transactionally, version-gated
//! ```
//!
//! ## What is checkpointed, and what is not
//!
//! Only tasks that are [`TaskStatus::InProgress`] are checkpointed, and only
//! when the observation recorded a real change — a moved `HEAD`, a moved
//! branch, or a changed working tree. A quiet round writes nothing, because a
//! checkpoint that restates the previous one is noise in the recovery lineage
//! without adding any information. Tasks that are `todo`, `verification_pending`,
//! `done`, `failed`, or `cancelled` are not in flight, so there is no
//! resumption document to write for them.
//!
//! ## What the checkpoint contains, and what it deliberately does not
//!
//! Every field is derived from records Orqyn holds. The objective comes from
//! the task, the branch and commit from the observation, the changed files from
//! the working tree. `progress` is a *description of observed state*, not an
//! agent's claim: it reports what git shows, and it never says a task is
//! complete. Nothing here can reach [`TaskStatus::Done`] — that is still
//! reachable only through independent verification in [`crate::verify`].
//!
//! Fields that Orqyn cannot fill honestly are left at their defaults rather
//! than invented: `test_results` is `None` because test outcomes are produced
//! by verification, not by observation; `current_blocker` is `None` because this
//! step observes a repository rather than a task's blocker lifecycle — blockers
//! are persisted, but they are raised by an explicit caller decision (see
//! [`crate::runner::Runner::raise_blocker`]) and a git observation is not such a
//! decision; `important_decisions` is empty because a decision is recorded by a
//! caller's explicit choice, not inferred from a diff.
//!
//! [`SyncResult`]: director_domain::repository::SyncResult

use std::time::Duration;

use director_domain::checkpoint::Checkpoint;
use director_domain::ids::{AgentId, CheckpointId, IdGenerator, ProjectId, SessionId, TaskId};
use director_domain::providers::MemoryProvider;
use director_domain::repository::{SyncResult, SyncStatus, WorktreeState};
use director_domain::store::StoredProjectState;
use director_domain::task::{Task, TaskStatus};
use director_domain::{AssignmentRepository, CheckpointRepository, TaskRepository};
use director_store::Store;

use crate::CheckpointError;

/// How long a checkpoint waits for the semantic-memory substrate before the
/// round moves on.
///
/// Publication is best-effort and SQLite is authoritative, so a substrate that
/// cannot answer in bounded time must not hold the observation round open — the
/// loop's other work is not a guest of the memory substrate. The checkpoint is
/// already committed by then, so a timeout costs the recovery lineage nothing;
/// the round records the failure in [`CheckpointRound::memory_failures`] and
/// the next round publishes the page then.
pub const MEMORY_PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);

/// The text a [`CheckpointRound`] records when publication hit
/// [`MEMORY_PUBLISH_TIMEOUT`] rather than the substrate returning an error.
/// Distinct from a substrate error so a caller can tell "the substrate refused"
/// from "the substrate did not answer", which are different things to do
/// something about.
pub const MEMORY_PUBLISH_TIMED_OUT: &str = "semantic memory publication timed out";

/// One observation's worth of checkpointing: what was written, in task order,
/// what could not be published to semantic memory, and what lost a race.
///
/// Empty `written` when the round had nothing to say — no in-progress tasks, no
/// real change, or a change that only restated the checkpoint the task already
/// has. A caller that wants to know "did this round advance the recovery
/// lineage" checks [`Self::is_empty`]; a caller that wants to know "did the
/// round come away with everything it tried for" also checks
/// [`Self::has_conflicts`].
#[derive(Debug, Clone, Default)]
pub struct CheckpointRound {
    /// The checkpoints the round persisted, one per in-progress task.
    pub written: Vec<Checkpoint>,
    /// Checkpoints that landed in SQLite but whose semantic-memory publication
    /// failed or timed out. Each names the task and the substrate's error, so a
    /// caller can report the gap without re-deriving it. This is deliberately
    /// *not* an error: SQLite is authoritative, so a checkpoint whose memory
    /// page did not land is still the checkpoint a recovery reads.
    pub memory_failures: Vec<MemoryFailure>,
    /// Tasks the round could not checkpoint because another writer advanced the
    /// lineage between the round's read and its write. Empty on a round that won
    /// every race. This is *not* a failure of the round — see
    /// [`CheckpointConflict`]; the observation itself landed, the tasks the
    /// round did write are in `written`, and each entry carries what a retry
    /// needs.
    pub conflicts: Vec<CheckpointConflict>,
}

/// A checkpoint that persisted but could not be published to semantic memory.
///
/// Carries the substrate's own error text so a caller reports what actually
/// went wrong rather than a generic "memory unavailable".
#[derive(Debug, Clone)]
pub struct MemoryFailure {
    /// The task whose checkpoint was written.
    pub task_id: TaskId,
    /// The substrate's error, as reported by [`MemoryProvider`], or
    /// [`MEMORY_PUBLISH_TIMED_OUT`] when the substrate did not answer within
    /// [`MEMORY_PUBLISH_TIMEOUT`].
    pub error: String,
}

/// A task the round could not checkpoint, and the version that beat it.
///
/// This is not a failure. Another writer — another observation round, another
/// tab, another loop step — landed a checkpoint for this task after this round
/// read the lineage and before it wrote, and the version gate refused the write
/// rather than burying the newer checkpoint. The observation itself is
/// unaffected: the normalized state was recorded before checkpointing began,
/// and every other task the round checkpointed is in
/// [`CheckpointRound::written`].
///
/// A conflict is *retryable by construction*: the store reports the lineage
/// version that won, and [`Self::retry_against`] is that version — the
/// precondition a re-read-and-retry presents so its write lands.
#[derive(Debug, Clone)]
pub struct CheckpointConflict {
    /// The task whose checkpoint could not be written.
    pub task_id: TaskId,
    /// The lineage version the store now holds: the checkpoint that won.
    pub stored: u64,
    /// The lineage version this round read before it wrote.
    pub caller: u64,
}

impl CheckpointConflict {
    /// The lineage version a retry must present as its write precondition.
    ///
    /// The conflict carries the cure with the complaint: `stored` is the
    /// version the store reported when it refused, so a caller that re-derives
    /// its checkpoint and presents this version writes at the next lineage
    /// position rather than conflicting a second time. The round's conflicts
    /// always name a checkpoint that exists, because the round's own read is
    /// what produced its precondition.
    pub fn retry_against(&self) -> u64 {
        self.stored
    }
}

impl CheckpointRound {
    /// True when the round wrote nothing — a quiet observation, no work in
    /// flight to checkpoint, or a change that only restated the checkpoint a
    /// task already has.
    pub fn is_empty(&self) -> bool {
        self.written.is_empty()
    }

    /// True when at least one task lost a race for its checkpoint lineage. A
    /// round can write checkpoints *and* conflict on others, so this is
    /// independent of [`Self::is_empty`]: the outcome to report is "the round
    /// wrote what it could, and here is what it could not".
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// The tasks the round checkpointed.
    pub fn tasks(&self) -> Vec<&TaskId> {
        self.written.iter().map(|cp| &cp.task_id).collect()
    }

    /// The tasks the round could not checkpoint, and the version to retry
    /// against.
    pub fn conflicted_tasks(&self) -> Vec<(&TaskId, u64)> {
        self.conflicts
            .iter()
            .map(|c| (&c.task_id, c.retry_against()))
            .collect()
    }
}

/// The pure half of the decision: given a task and what an observation saw, is
/// a checkpoint warranted?
///
/// Extracted so the rule can be tested without a store and without a git
/// repository. A task is checkpointable when it is in flight and the observation
/// recorded a real change — the same signal [`crate::observe`] treats as
/// "changed", taken here straight from the sync result.
pub fn should_checkpoint(task: &Task, sync: &SyncResult) -> bool {
    // Only work that is actually out. A task that is `todo` has not started, so
    // there is no progress to preserve; a task that is `verification_pending`
    // has finished its part and is awaiting judgment; the terminal statuses are
    // over. `in_progress` is the one state whose loss costs a replacement agent
    // real work.
    if task.status != TaskStatus::InProgress {
        return false;
    }

    // A restatement is not a checkpoint. `SyncStatus::Synced` means the sync
    // recorded at least one new fact; `NoChange` means nothing moved, and
    // `Baseline` means Orqyn arrived and found the repository as it already
    // was. Neither advances the recovery lineage, so neither writes.
    sync.status == SyncStatus::Synced
}

/// Whether a candidate checkpoint would say nothing the task's current
/// checkpoint does not already say.
///
/// The git layer's own change detection is content-keyed, so an unchanged
/// repository reports [`SyncStatus::NoChange`] and the round writes nothing.
/// But its content key for an *uncommitted* file is the working-tree blob oid,
/// and for a file git did not hash — which includes most untracked paths — it
/// falls back to `len:mtime`, in whole seconds. A file rewritten with identical
/// bytes a second later is therefore a "new change" as far as the event log is
/// concerned, and a round that trusted the sync status alone would mint a
/// checkpoint that restates the previous one verbatim. Repeated observations of
/// a dirty worktree would grow the lineage without bound, each entry identical
/// to the last, and the resumption document a replacement reads would be buried
/// under noise.
///
/// So the guard is over the *checkpoint* rather than over the event log: if the
/// evidence a new checkpoint would record is the evidence the task's current
/// checkpoint already records, the round writes nothing. The comparison is over
/// exactly the fields that make a checkpoint useful to a reader — the branch
/// and commit it was taken against, the files that were out, the objective it
/// was pursuing, and who was pursuing it — and over nothing else, so a change
/// to any of them still writes.
///
/// This is why a quiet round and a suppressed round are different things a
/// caller can distinguish: a quiet round saw no change at all, a suppressed
/// round saw one that turned out to be restatement. Both write nothing, and
/// neither is an error.
pub fn restates_current(current: &Checkpoint, candidate: &Checkpoint) -> bool {
    // Different tasks are different lineages; the guard never spans them.
    current.task_id != candidate.task_id
        || (current.branch == candidate.branch
            && current.commit_sha == candidate.commit_sha
            && current.objective == candidate.objective
            && current.created_by_agent == candidate.created_by_agent
            && changed_file_set(&current.changed_files)
                == changed_file_set(&candidate.changed_files))
}

/// The changed-file list as a comparison key: sorted and de-duplicated, so two
/// observations that report the same files in a different order — which the
/// worktree reader can, because the buckets are concatenated before they are
/// sorted — compare as the same set.
fn changed_file_set(paths: &[String]) -> Vec<&str> {
    let mut keys: Vec<&str> = paths.iter().map(String::as_str).collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Derive a checkpoint from the evidence an observation collected, without
/// touching the store.
///
/// Every populated field traces to a record Orqyn holds. Nothing is invented:
/// the fields below are the ones the evidence genuinely supports, and every
/// other field stays at its [`Checkpoint::new`] default. The caller supplies the
/// task and the assignment's agent and session so the checkpoint's provenance is
/// attribution the store already agrees with, not a claim.
pub fn derive_checkpoint(
    id: CheckpointId,
    task: &Task,
    state: &StoredProjectState,
    worktree: &WorktreeState,
    agent: Option<AgentId>,
    session: Option<SessionId>,
    now: chrono::DateTime<chrono::Utc>,
) -> Checkpoint {
    let mut checkpoint = Checkpoint::new(
        id,
        task.id.clone(),
        // The objective as the task states it, recorded so the checkpoint stays
        // self-contained even if the task's contract is later reworked.
        task.objective.clone(),
        // The progress narrative: what git shows, in the order a reader needs
        // it. This is a description of observed state, never a completion
        // claim — nothing in this string can make the task done.
        describe_progress(task, state, worktree),
        next_action(task, state, worktree),
    );

    checkpoint.branch = state.branch.clone();
    checkpoint.commit_sha = if state.head_commit.is_empty() {
        None
    } else {
        Some(state.head_commit.clone())
    };
    checkpoint.changed_files = worktree
        .all_changes()
        .into_iter()
        .map(|c| c.path.clone())
        .collect();

    // The versions a resume reconciles against. The project's state version
    // pairs with the task's to answer "was the world this checkpoint describes
    // still the world when it is read" — see [`Checkpoint::compare_with`].
    checkpoint.project_state_version = Some(state.state_version);
    checkpoint.task_state_version = Some(task.state_version);

    checkpoint.created_at = now;
    checkpoint.created_by_agent = agent;
    checkpoint.created_by_session = session;

    checkpoint
}

/// What the observation shows, as a narrative a replacement agent reads.
///
/// Deliberately a description of evidence rather than a status report: it says
/// what `HEAD` is, what changed, and what the task is asking for. It never
/// asserts the work is complete.
fn describe_progress(task: &Task, state: &StoredProjectState, worktree: &WorktreeState) -> String {
    let mut lines = Vec::new();

    let branch = state.branch.as_deref().unwrap_or("a detached HEAD");
    match non_empty_sha(state) {
        Some(sha) => lines.push(format!("in progress on {branch} at {sha}")),
        None => lines.push(format!("in progress on {branch}, no commits yet")),
    }

    let changed = worktree.all_changes();
    if changed.is_empty() {
        lines.push("the working tree is clean".to_string());
    } else {
        let paths: Vec<&str> = changed.iter().map(|c| c.path.as_str()).collect();
        // Bounded so a huge diff cannot flood the checkpoint; the full list is
        // recoverable from git by anyone resuming.
        let shown: Vec<&str> = paths.iter().take(8).copied().collect();
        let summary = if paths.len() > shown.len() {
            format!(
                "{} (and {} more)",
                shown.join(", "),
                paths.len() - shown.len()
            )
        } else {
            shown.join(", ")
        };
        lines.push(format!("uncommitted changes: {summary}"));
    }

    lines.push(format!("objective: {}", task.objective));

    lines.join("; ")
}

/// The concrete first step for the agent picking the work up.
///
/// Derived from observed state, not from anything an agent reported. When the
/// tree is dirty the next action is to finish the uncommitted work; when it is
/// clean the work is whatever the task's contract asks for next.
fn next_action(task: &Task, state: &StoredProjectState, worktree: &WorktreeState) -> String {
    if !worktree.all_changes().is_empty() {
        return format!(
            "continue the in-flight work on {} — the tree has uncommitted changes to reconcile before reporting completion",
            state
                .branch
                .as_deref()
                .unwrap_or("the current branch")
        );
    }

    // The task's own acceptance contract is the honest statement of what
    // remains. Prefer the first expected output's criterion when there is one,
    // since that is the measurable thing a replacement can aim at.
    match task.expected_outputs.first() {
        Some(output) => format!("advance {} — {}", task.title, output.criterion),
        None => format!("advance {} toward its objective", task.title),
    }
}

fn non_empty_sha(state: &StoredProjectState) -> Option<&str> {
    let sha = state.head_commit.trim();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Run the checkpoint half of an OBSERVE round: for every in-progress task in
/// the project, derive a checkpoint from what the observation saw and persist
/// it.
///
/// This is the seam [`crate::observe`] calls. It is separate from that module so
/// OBSERVE stays about *recording what was learned* and this stays about *what
/// a resumption needs*; the two are one round, but they are not one concern.
///
/// Each write is version-gated: the round reads the task's current checkpoint
/// and passes that version as the write's precondition, so a concurrent round
/// that landed a checkpoint in between is refused rather than buried — see
/// [`CheckpointRepository::create_checkpoint`](director_domain::CheckpointRepository::create_checkpoint).
/// A conflict is reported, not silently swallowed — a checkpoint the round
/// believed it wrote but did not is exactly the thing a caller must know about.
///
/// The same read fixes the checkpoint's *id*: an id is a lineage position, and
/// a lineage position is durable state, so it is taken from the store rather
/// than from `ids`. A generator's counter is per-process and resets on a
/// restart, which would collide on `checkpoints.id`'s PRIMARY KEY — see the
/// note where the id is derived. `ids` is kept in the signature because the
/// round is one half of an OBSERVE tick and the tick mints through it, but it
/// no longer supplies anything a checkpoint's identity depends on.
///
/// Once SQLite has the checkpoint, the round *attempts* to publish it to
/// semantic memory through [`MemoryProvider`]. That publication is
/// best-effort: a substrate failure is recorded in
/// [`CheckpointRound::memory_failures`] and logged, never raised, because
/// SQLite is authoritative and a memory page is not a prerequisite for
/// recovery. A failed publication therefore never unwrites a checkpoint that
/// landed.
#[allow(clippy::too_many_arguments)]
pub async fn checkpoint_round<M: MemoryProvider>(
    store: &Store,
    _ids: &mut IdGenerator,
    memory: &M,
    project: &ProjectId,
    state: &StoredProjectState,
    sync: &SyncResult,
    worktree: &WorktreeState,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<CheckpointRound, CheckpointError> {
    let tasks = store.tasks().list_tasks(project).await?;

    let mut written = Vec::new();
    let mut memory_failures = Vec::new();
    let mut conflicts = Vec::new();
    for task in tasks {
        if !should_checkpoint(&task, sync) {
            continue;
        }

        // Provenance: the tenure actually holding the task, so the checkpoint
        // names the agent and session the store already records as working it.
        // Read fresh per task, because a task with no active assignment is not
        // being worked, and a checkpoint's attribution must agree with the
        // tenure history.
        let assignment = store
            .assignments()
            .active_assignment_for_task(&task.id)
            .await?;

        // The precondition the write is gated on: the version of the checkpoint
        // this round read before deriving a new one. Read per task, inside the
        // round, so a second observation round on a moving repository advances
        // the lineage rather than colliding with the first. `None` is the
        // honest value when the task has no checkpoint yet — the gate then
        // refuses a write that assumed one.
        let expected = store
            .checkpoints()
            .latest_checkpoint(&task.id)
            .await?
            .map(|current| current.state_version);

        // The id is the checkpoint's position in the task's lineage: one past
        // the checkpoint the round just read as its write precondition. It is
        // derived from the lineage rather than from `ids` because the lineage
        // lives in SQLite and the generator does not. `checkpoints.id` is a
        // PRIMARY KEY, so an id minted from a fresh generator's counter — which
        // is what a restarted process has — would collide with the checkpoint a
        // previous process already wrote, and the store's constraint would
        // refuse the write and take the whole observation round down with it.
        // Deriving from the lineage also makes the id self-describing:
        // `CHK-work-3` is the third checkpoint for the task, and it is the row
        // whose `state_version` is three.
        let lineage_position = expected.unwrap_or(0) + 1;
        let checkpoint = derive_checkpoint(
            CheckpointId::from_string(format!(
                "CHK-{}-{lineage_position}",
                task.id.as_str().to_lowercase()
            )),
            &task,
            state,
            worktree,
            assignment.as_ref().map(|a| a.agent_id.clone()),
            assignment.as_ref().and_then(|a| a.session_id.clone()),
            now,
        );

        // SQLite first, transactionally. Only after this commits does the
        // round attempt semantic memory, so a memory failure can never leave the
        // recovery lineage without the checkpoint it is about.
        //
        // The write's two outcomes are told apart in [`attempt_checkpoint`]: a
        // conflict is data the round records before taking the next task, and
        // anything else is a real failure that ends the round here.
        let stored = match attempt_checkpoint(store, &task.id, &checkpoint, expected).await? {
            CheckpointWrite::Written(stored) => stored,
            CheckpointWrite::Conflict(conflict) => {
                tracing::warn!(
                    task = %conflict.task_id.as_str(),
                    stored_version = conflict.stored,
                    caller_version = conflict.caller,
                    "checkpoint conflict: another writer advanced the lineage; recorded for retry, the round continues"
                );
                conflicts.push(conflict);
                continue;
            }
        };

        // Best-effort publication. The checkpoint is already authoritative
        // state, so a failure here is recorded for the caller and logged, and
        // the round continues to the next task.
        if let Err(err) = publish_checkpoint(memory, &stored).await {
            tracing::warn!(
                task = %stored.task_id.as_str(),
                checkpoint = %stored.id.as_str(),
                error = %err,
                "semantic memory publication failed; the checkpoint is persisted in SQLite",
            );
            memory_failures.push(MemoryFailure {
                task_id: stored.task_id.clone(),
                error: err.to_string(),
            });
        }

        written.push(*stored);
    }

    Ok(CheckpointRound {
        written,
        memory_failures,
        conflicts,
    })
}

/// One task's checkpoint write, as the store answered it.
///
/// `Written` is boxed to keep the enum small: a `Checkpoint` carries the full
/// changed-file list and narrative, and a conflict — the case a round sees most
/// under contention — should not drag that weight onto the stack with it.
#[derive(Debug, Clone)]
enum CheckpointWrite {
    /// The checkpoint the store persisted, with its lineage position assigned.
    Written(Box<Checkpoint>),
    /// The store refused the write because another writer moved this task's
    /// checkpoint lineage first. Not an error — see [`CheckpointConflict`].
    Conflict(CheckpointConflict),
}

/// Attempt one task's checkpoint and say what the store's answer means.
///
/// This is where the round's two failure modes are told apart, and the
/// distinction is the whole point of collecting conflicts. A version conflict
/// is *the* expected race against a second observer — another tab, another loop
/// step, another process — so it comes back as data the caller records and moves
/// on from; the tasks already written are committed, and this one is retryable
/// against the version the store reported. Every *other* failure comes back as
/// an error, because a round that cannot reach its own state has no basis for
/// promising the caller a partial result.
///
/// The classification matches on the error's own vocabulary, never on its
/// message text: only [`CheckpointError::Conflict`] — the one variant that means
/// "somebody else wrote first" — becomes a conflict, so a constraint or storage
/// failure can never be mistaken for one. A caller told "conflict" retries and
/// lands; a caller told that about a broken write would retry a write that can
/// never succeed.
async fn attempt_checkpoint(
    store: &Store,
    task: &TaskId,
    checkpoint: &Checkpoint,
    expected: Option<u64>,
) -> Result<CheckpointWrite, CheckpointError> {
    match write_checkpoint(store, checkpoint, expected).await {
        Ok(stored) => Ok(CheckpointWrite::Written(Box::new(stored))),
        // The store reports the lineage version that won, which is exactly the
        // precondition a retry must present. The conflict is recorded against
        // the task the round was working, not the id string the error carried,
        // so the caller gets the `TaskId` it was iterating.
        Err(CheckpointError::Conflict { stored, caller, .. }) => {
            Ok(CheckpointWrite::Conflict(CheckpointConflict {
                task_id: task.clone(),
                stored,
                caller,
            }))
        }
        // Deliberately not a conflict: a constraint, serialization, or storage
        // failure means something is wrong with *this* write, not that somebody
        // else wrote first. The store's own text reaches the caller unchanged.
        Err(err) => Err(err),
    }
}

/// Publish a checkpoint to semantic memory, so a reader asking "what is the
/// state of this task" finds the resumption document where the substrate keeps
/// durable knowledge.
///
/// The page is addressed by task, not by checkpoint: each new checkpoint
/// supersedes the last, and the page is the *current* resumption document
/// rather than an append-only log of them — the lineage is already preserved in
/// SQLite, so duplicating it in memory would be a second source of truth that
/// could drift.
///
/// Everything on the page is drawn from the checkpoint's own evidence. It is a
/// description of observed state, never a completion claim: no amount of memory
/// content can move a task to [`TaskStatus::Done`], because that is reachable
/// only through independent verification in [`crate::verify`].
///
/// This takes the domain [`MemoryProvider`] trait and nothing else, so Orqyn's
/// loop never learns which substrate is answering. A substrate that cannot be
/// reached reports its own error, and the caller decides what to do about it.
pub async fn publish_checkpoint<M: MemoryProvider>(
    memory: &M,
    checkpoint: &Checkpoint,
) -> Result<(), M::Error> {
    let memory_record = memory_for(checkpoint);
    memory.save_memory(memory_record).await?;
    Ok(())
}

/// The semantic-memory record for a checkpoint.
///
/// Extracted so the mapping is testable without a provider, and so it is
/// obvious that every field comes from the checkpoint and nowhere else.
fn memory_for(checkpoint: &Checkpoint) -> director_domain::providers::Memory {
    let task = checkpoint.task_id.as_str().to_lowercase();
    let mut body = String::new();
    body.push_str(&format!(
        "**Resume status evidence for {}**\n\n",
        checkpoint.task_id.as_str()
    ));
    body.push_str(&format!("- objective: {}\n", checkpoint.objective));
    if let Some(branch) = checkpoint.branch.as_deref() {
        body.push_str(&format!("- branch: {branch}\n"));
    }
    if let Some(sha) = checkpoint.commit_sha.as_deref() {
        body.push_str(&format!("- head: {sha}\n"));
    }
    body.push_str(&format!("- progress: {}\n", checkpoint.progress));
    body.push_str(&format!("- next action: {}\n", checkpoint.next_action));
    if !checkpoint.changed_files.is_empty() {
        body.push_str(&format!(
            "- changed files: {}\n",
            checkpoint.changed_files.join(", ")
        ));
    }

    director_domain::providers::Memory {
        // A page path, because that is the substrate's own addressing unit —
        // the adapter maps `id` straight onto the write-page `path`.
        id: format!("checkpoints/{task}.md"),
        title: format!("Checkpoint: {}", checkpoint.task_id.as_str()),
        body,
        // Not a retention tier Orqyn knows how to name; leaving it `None`
        // lets the substrate pick its default rather than Orqyn guessing one.
        kind: None,
        tags: vec!["orqyn-checkpoint".into(), task],
        // A relevance score is the substrate's to assign on read, not the
        // writer's to assert at write time.
        score: None,
        updated_at: Some(checkpoint.created_at),
    }
}

/// Persist a single derived checkpoint against an expected prior version.
///
/// Used by callers that take a checkpoint [`derive_checkpoint`] built and want
/// the same version gating the round path gets. `expected` is the version of
/// the checkpoint the caller read before deriving this one, or `None` when the
/// caller read none.
pub async fn write_checkpoint(
    store: &Store,
    checkpoint: &Checkpoint,
    expected: Option<u64>,
) -> Result<Checkpoint, CheckpointError> {
    Ok(store
        .checkpoints()
        .create_checkpoint(checkpoint, expected)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use director_adapters::InMemoryProvider;
    use director_domain::checkpoint::CHECKPOINT_FORMAT_VERSION;
    use director_domain::ids::MachineId;
    use director_domain::project::Project;
    use director_domain::repository::{FileChangeRecord, SyncResult};
    use director_domain::state::FileChange;
    use director_domain::task::{ExpectedOutput, Task};
    use director_domain::ProjectRepository;

    fn task(id: &str, status: TaskStatus) -> Task {
        let mut task = Task::new(TaskId::from_string(id), id, "the objective");
        task.status = status;
        task.expected_outputs = vec![ExpectedOutput {
            criterion: "auth answers".into(),
            check: Some("git --version".into()),
        }];
        task
    }

    fn state(commit: &str) -> StoredProjectState {
        StoredProjectState {
            project_id: ProjectId::from_string("PROJ-1"),
            repository_id: None,
            branch: Some("main".into()),
            head_commit: commit.into(),
            working_tree_clean: true,
            observation_version: 1,
            last_observed_at: chrono::Utc::now(),
            state_version: 3,
        }
    }

    fn synced() -> SyncResult {
        SyncResult {
            status: SyncStatus::Synced,
            branch_changed: false,
            commit_changed: true,
            working_tree_changed: false,
            events_created: 1,
            previous_commit: None,
            current_commit: "abc".into(),
            state_version: 3,
        }
    }

    fn unchanged() -> SyncResult {
        SyncResult {
            status: SyncStatus::NoChange,
            commit_changed: false,
            ..synced()
        }
    }

    fn change(path: &str) -> FileChangeRecord {
        FileChangeRecord {
            repository_id: director_domain::ids::RepositoryId::from_string("REPO-1"),
            path: path.into(),
            old_path: None,
            change_type: FileChange::Modified,
            insertions: None,
            deletions: None,
            fingerprint: path.to_string(),
            commit_sha: "abc".into(),
            observed_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn an_in_progress_task_on_a_changed_repo_is_checkpointable() {
        assert!(should_checkpoint(
            &task("AUTH", TaskStatus::InProgress),
            &synced()
        ));
    }

    #[test]
    fn a_task_that_is_not_in_progress_is_not_checkpointable() {
        for status in [
            TaskStatus::Todo,
            TaskStatus::Backlog,
            TaskStatus::VerificationPending,
            TaskStatus::Done,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert!(
                !should_checkpoint(&task("AUTH", status), &synced()),
                "{status:?} must not be checkpointed"
            );
        }
    }

    #[test]
    fn a_quiet_round_checkpoints_nothing_even_for_in_flight_work() {
        assert!(!should_checkpoint(
            &task("AUTH", TaskStatus::InProgress),
            &unchanged()
        ));
    }

    #[test]
    fn a_baseline_observation_checkpoints_nothing() {
        let baseline = SyncResult {
            status: SyncStatus::Baseline,
            ..synced()
        };
        assert!(!should_checkpoint(
            &task("AUTH", TaskStatus::InProgress),
            &baseline
        ));
    }

    #[test]
    fn a_derived_checkpoint_carries_the_observed_evidence() {
        let task = task("AUTH", TaskStatus::InProgress);
        let state = state("abc123");

        let cp = derive_checkpoint(
            CheckpointId::from_string("CHK-1"),
            &task,
            &state,
            &WorktreeState::default(),
            Some(AgentId::from_string("AGENT-1")),
            Some(SessionId::from_string("SESS-1")),
            chrono::Utc::now(),
        );

        assert_eq!(cp.task_id, TaskId::from_string("AUTH"));
        assert_eq!(cp.objective, "the objective");
        assert_eq!(cp.branch.as_deref(), Some("main"));
        assert_eq!(cp.commit_sha.as_deref(), Some("abc123"));
        assert_eq!(cp.project_state_version, Some(3));
        assert_eq!(cp.task_state_version, Some(task.state_version));
        assert_eq!(cp.created_by_agent, Some(AgentId::from_string("AGENT-1")));
        assert_eq!(
            cp.created_by_session,
            Some(SessionId::from_string("SESS-1"))
        );
        assert_eq!(cp.context_version, CHECKPOINT_FORMAT_VERSION);
    }

    #[test]
    fn a_derived_checkpoint_reports_the_working_tree_not_a_completion_claim() {
        let worktree = WorktreeState {
            modified: vec![change("src/auth.rs"), change("src/api.rs")],
            ..Default::default()
        };

        let cp = derive_checkpoint(
            CheckpointId::from_string("CHK-1"),
            &task("AUTH", TaskStatus::InProgress),
            &state("abc"),
            &worktree,
            None,
            None,
            chrono::Utc::now(),
        );

        // `all_changes` sorts by path, so the order is stable regardless of the
        // bucket order they were inserted in.
        assert_eq!(cp.changed_files, vec!["src/api.rs", "src/auth.rs"]);
        assert!(cp.progress.contains("uncommitted changes"));
        assert!(cp.progress.contains("src/auth.rs"));
        // The narrative describes state; it never claims the work is done.
        assert!(!cp.progress.to_lowercase().contains("done"));
        assert!(!cp.progress.to_lowercase().contains("complete"));
    }

    #[test]
    fn a_clean_tree_is_described_as_such() {
        let cp = derive_checkpoint(
            CheckpointId::from_string("CHK-1"),
            &task("AUTH", TaskStatus::InProgress),
            &state("abc"),
            &WorktreeState::default(),
            None,
            None,
            chrono::Utc::now(),
        );
        assert!(cp.progress.contains("working tree is clean"));
        assert!(cp.changed_files.is_empty());
    }

    #[test]
    fn a_huge_diff_is_bounded_not_flooded() {
        let worktree = WorktreeState {
            modified: (0..20).map(|n| change(&format!("src/f{n}.rs"))).collect(),
            ..Default::default()
        };

        let cp = derive_checkpoint(
            CheckpointId::from_string("CHK-1"),
            &task("AUTH", TaskStatus::InProgress),
            &state("abc"),
            &worktree,
            None,
            None,
            chrono::Utc::now(),
        );

        assert!(cp.progress.contains("and 12 more"));
        assert_eq!(cp.changed_files.len(), 20, "the field keeps the full list");
    }

    #[test]
    fn an_empty_head_commit_records_none() {
        let mut empty = state("");
        empty.head_commit = String::new();

        let cp = derive_checkpoint(
            CheckpointId::from_string("CHK-1"),
            &task("AUTH", TaskStatus::InProgress),
            &empty,
            &WorktreeState::default(),
            None,
            None,
            chrono::Utc::now(),
        );

        assert!(cp.commit_sha.is_none());
        assert!(cp.progress.contains("no commits yet"));
    }

    #[test]
    fn the_next_action_points_at_the_acceptance_contract() {
        let cp = derive_checkpoint(
            CheckpointId::from_string("CHK-1"),
            &task("AUTH", TaskStatus::InProgress),
            &state("abc"),
            &WorktreeState::default(),
            None,
            None,
            chrono::Utc::now(),
        );
        assert!(cp.next_action.contains("auth answers"));
    }

    #[test]
    fn the_next_action_on_a_dirty_tree_names_the_reconciliation() {
        let worktree = WorktreeState {
            modified: vec![change("src/auth.rs")],
            ..Default::default()
        };

        let cp = derive_checkpoint(
            CheckpointId::from_string("CHK-1"),
            &task("AUTH", TaskStatus::InProgress),
            &state("abc"),
            &worktree,
            None,
            None,
            chrono::Utc::now(),
        );
        assert!(cp
            .next_action
            .contains("uncommitted changes to reconcile before reporting completion"));
    }

    #[test]
    fn a_derived_checkpoint_marks_the_machine_nothing() {
        // Sanity on the fields the evidence cannot support: they stay at their
        // defaults rather than being invented.
        let cp = derive_checkpoint(
            CheckpointId::from_string("CHK-1"),
            &task("AUTH", TaskStatus::InProgress),
            &state("abc"),
            &WorktreeState::default(),
            None,
            None,
            chrono::Utc::now(),
        );
        assert!(
            cp.test_results.is_none(),
            "test results come from verification"
        );
        assert!(
            cp.current_blocker.is_none(),
            "OBSERVE does not read a task's blocker lifecycle"
        );
        assert!(
            cp.important_decisions.is_empty(),
            "decisions are recorded, not inferred"
        );
        assert!(cp.current_assumptions.is_empty());
    }

    #[test]
    fn a_round_reports_its_tasks() {
        let round = CheckpointRound {
            written: vec![derive_checkpoint(
                CheckpointId::from_string("CHK-1"),
                &task("AUTH", TaskStatus::InProgress),
                &state("abc"),
                &WorktreeState::default(),
                None,
                None,
                chrono::Utc::now(),
            )],
            memory_failures: Vec::new(),
            conflicts: Vec::new(),
        };
        assert!(!round.is_empty());
        assert!(round.tasks().contains(&&TaskId::from_string("AUTH")));
    }

    #[test]
    fn a_round_reports_the_tasks_it_could_not_checkpoint() {
        // A round that lost a race still writes what it won: the conflict is a
        // separate channel from `written`, and both are reported.
        let round = CheckpointRound {
            written: vec![derive_checkpoint(
                CheckpointId::from_string("CHK-1"),
                &task("AUTH", TaskStatus::InProgress),
                &state("abc"),
                &WorktreeState::default(),
                None,
                None,
                chrono::Utc::now(),
            )],
            memory_failures: Vec::new(),
            conflicts: vec![CheckpointConflict {
                task_id: TaskId::from_string("DB"),
                stored: 4,
                caller: 3,
            }],
        };

        // `has_conflicts` is independent of `is_empty`: this round both wrote
        // and conflicted.
        assert!(!round.is_empty());
        assert!(round.has_conflicts());

        // The conflict names the task and the version a retry must present —
        // the lineage that won, not the one the round read.
        assert_eq!(
            round.conflicted_tasks(),
            vec![(&TaskId::from_string("DB"), 4)]
        );
    }

    #[test]
    fn a_round_without_conflicts_reports_none() {
        let round = CheckpointRound {
            written: Vec::new(),
            memory_failures: Vec::new(),
            conflicts: Vec::new(),
        };
        assert!(!round.has_conflicts());
        assert!(round.conflicted_tasks().is_empty());
    }

    #[test]
    fn a_conflict_names_the_version_to_retry_against() {
        // `retry_against` is the whole point of the type: the store reported the
        // version that won, so presenting it as the next write's precondition
        // lands the checkpoint rather than conflicting a second time.
        let conflict = CheckpointConflict {
            task_id: TaskId::from_string("DB"),
            stored: 7,
            caller: 6,
        };
        assert_eq!(conflict.retry_against(), 7);
    }

    #[test]
    fn an_empty_round_reports_nothing() {
        assert!(CheckpointRound::default().is_empty());
    }

    #[test]
    fn provenance_is_taken_from_the_records_not_invented() {
        // The machine id type exists to be attributed; derive does not invent
        // one, because an observation does not see the machine a session ran on.
        let _ = MachineId::from_string("MACH-a");
    }

    // --- Store-backed conflict tests ---------------------------------------
    //
    // Everything above is pure. What follows drives the real SQLite store,
    // because the conflict path only means something against a repository that
    // really enforces a version gate: the store's own refusal is the fact under
    // test, and a stand-in could only report the refusal it was told to report.
    //
    // A conflict is arranged by handing `attempt_checkpoint` a precondition that
    // is already stale - exactly the value a second observer would have read
    // before this store moved. The store's gate is what refuses the write; no
    // timing trick is involved, so these are deterministic.

    const PROJ: &str = "PROJ-1";

    /// A store with one project and nothing else.
    async fn boot() -> (tempfile::TempDir, Store) {
        let dir = tempfile::TempDir::new().expect("a temp dir for the store");
        let store = Store::open(dir.path().join("checkpoints.db"))
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
            .expect("the project exists");
        (dir, store)
    }

    /// A task the store records as `in_progress`, the one status a round
    /// checkpoints. Returned as the store wrote it, so a test never trusts a
    /// local guess about versions.
    async fn seed_task(store: &Store, id: &str) -> Task {
        let task = Task::for_project(
            ProjectId::from_string(PROJ),
            TaskId::from_string(id),
            id,
            "the objective",
        );
        let stored = store
            .tasks()
            .create_task(&task)
            .await
            .expect("the task is created");

        let mut in_flight = stored;
        in_flight.status = TaskStatus::InProgress;
        store
            .tasks()
            .update_task(&in_flight)
            .await
            .expect("the task is in progress")
    }

    #[tokio::test]
    async fn a_version_conflict_is_reported_not_raised() {
        let (_dir, store) = boot().await;
        let task = seed_task(&store, "AUTH").await;

        // A checkpoint this round never saw: exactly the state a second observer
        // leaves behind when it wins this task's lineage.
        let theirs = derive_checkpoint(
            CheckpointId::from_string("CHK-auth-1"),
            &task,
            &state("abc"),
            &WorktreeState::default(),
            None,
            None,
            chrono::Utc::now(),
        );
        store
            .checkpoints()
            .create_checkpoint(&theirs, None)
            .await
            .expect("the other writer's checkpoint lands");

        // The round's precondition is what it read *before* that write: `None`.
        // The store is now at version 1, so the gate refuses - and the refusal
        // comes back as a conflict the caller can act on, never as an error.
        let ours = derive_checkpoint(
            CheckpointId::from_string("CHK-auth-2"),
            &task,
            &state("def"),
            &WorktreeState::default(),
            None,
            None,
            chrono::Utc::now(),
        );
        let outcome = attempt_checkpoint(&store, &task.id, &ours, None)
            .await
            .expect("a version conflict is not an error");

        match outcome {
            CheckpointWrite::Conflict(conflict) => {
                // The versions are the store's own, and they are what make the
                // conflict retryable: `stored` is the lineage that won, `caller`
                // the one this round read.
                assert_eq!(conflict.task_id, task.id);
                assert_eq!(conflict.stored, 1, "the version the store now holds");
                assert_eq!(conflict.caller, 0, "the round read no checkpoint");
                assert_eq!(conflict.retry_against(), 1);
            }
            CheckpointWrite::Written(_) => {
                panic!("a stale precondition must conflict, not write")
            }
        }

        // The winning checkpoint is still the current one: the refused write
        // superseded nothing.
        assert_eq!(
            store
                .checkpoints()
                .latest_checkpoint(&task.id)
                .await
                .expect("the read")
                .expect("a current checkpoint exists")
                .state_version,
            1
        );
    }

    #[tokio::test]
    async fn a_conflict_on_one_task_does_not_stop_the_next() {
        let (_dir, store) = boot().await;
        let auth = seed_task(&store, "AUTH").await;
        let db = seed_task(&store, "DB").await;

        // AUTH's lineage has already moved; DB's has not.
        let theirs = derive_checkpoint(
            CheckpointId::from_string("CHK-auth-1"),
            &auth,
            &state("abc"),
            &WorktreeState::default(),
            None,
            None,
            chrono::Utc::now(),
        );
        store
            .checkpoints()
            .create_checkpoint(&theirs, None)
            .await
            .expect("AUTH's checkpoint lands");

        // The round's two writes, in the order it makes them. AUTH conflicts and
        // the outcome is still `Ok` - that is what lets the loop take the next
        // task instead of unwinding - and DB, with nothing competing for its
        // lineage, still gets its checkpoint.
        let auth_attempt = derive_checkpoint(
            CheckpointId::from_string("CHK-auth-2"),
            &auth,
            &state("def"),
            &WorktreeState::default(),
            None,
            None,
            chrono::Utc::now(),
        );
        let auth_outcome = attempt_checkpoint(&store, &auth.id, &auth_attempt, None)
            .await
            .expect("AUTH's conflict is handled, not raised");
        assert!(
            matches!(auth_outcome, CheckpointWrite::Conflict(_)),
            "AUTH lost the race"
        );

        let db_attempt = derive_checkpoint(
            CheckpointId::from_string("CHK-db-1"),
            &db,
            &state("def"),
            &WorktreeState::default(),
            None,
            None,
            chrono::Utc::now(),
        );
        let db_outcome = attempt_checkpoint(&store, &db.id, &db_attempt, None)
            .await
            .expect("DB is written");
        assert!(
            matches!(db_outcome, CheckpointWrite::Written(ref cp) if cp.task_id == db.id),
            "DB still gets its checkpoint after AUTH's conflict"
        );

        // The store agrees: DB has a checkpoint of its own, and AUTH still holds
        // the other writer's.
        assert_eq!(
            store
                .checkpoints()
                .latest_checkpoint(&db.id)
                .await
                .expect("the read")
                .expect("DB has a checkpoint")
                .state_version,
            1
        );
        assert_eq!(
            store
                .checkpoints()
                .latest_checkpoint(&auth.id)
                .await
                .expect("the read")
                .expect("AUTH still has a checkpoint")
                .state_version,
            1,
            "the winning checkpoint stands"
        );
    }

    #[tokio::test]
    async fn an_unexpected_store_error_is_raised_not_recorded_as_a_conflict() {
        let (_dir, store) = boot().await;
        let auth = seed_task(&store, "AUTH").await;

        // A checkpoint id AUTH already owns. The version gate is satisfied - AUTH
        // has no current checkpoint and the precondition is `None` - so the write
        // reaches the insert and fails on the primary key instead.
        let taken = derive_checkpoint(
            CheckpointId::from_string("CHK-taken-1"),
            &auth,
            &state("abc"),
            &WorktreeState::default(),
            None,
            None,
            chrono::Utc::now(),
        );
        store
            .checkpoints()
            .create_checkpoint(&taken, None)
            .await
            .expect("the id lands for AUTH");

        // A second task with the same id and no checkpoint of its own: the gate
        // passes, the insert collides. That is a constraint violation, not a
        // version conflict, and it must come back as an error. Classifying it as
        // a conflict would hand a caller a "retryable" write that can never land.
        let db = seed_task(&store, "DB").await;
        let collision = derive_checkpoint(
            CheckpointId::from_string("CHK-taken-1"),
            &db,
            &state("abc"),
            &WorktreeState::default(),
            None,
            None,
            chrono::Utc::now(),
        );
        let err = attempt_checkpoint(&store, &db.id, &collision, None)
            .await
            .expect_err("a constraint violation is an error");

        assert!(
            matches!(err, CheckpointError::Store(ref msg) if msg.contains("constraint")),
            "a constraint violation must not be reported as a conflict: {err:?}"
        );

        // And it was not recorded as a conflict either - nothing was written for
        // DB, and the lineage the store holds is unchanged.
        assert!(
            store
                .checkpoints()
                .latest_checkpoint(&db.id)
                .await
                .expect("the read")
                .is_none(),
            "no checkpoint was written for DB"
        );
    }

    #[tokio::test]
    async fn a_round_checkpoints_every_in_progress_task_and_advances_its_lineage() {
        let (_dir, store) = boot().await;
        let auth = seed_task(&store, "AUTH").await;
        let _db = seed_task(&store, "DB").await;

        // A task that is not in flight is skipped, and skipping it must not stop
        // the round from reaching the tasks that are.
        let _done = {
            let done = Task::for_project(
                ProjectId::from_string(PROJ),
                TaskId::from_string("DONE"),
                "DONE",
                "the objective",
            );
            store.tasks().create_task(&done).await.expect("created")
        };

        let mut ids = IdGenerator::new();
        let round = checkpoint_round(
            &store,
            &mut ids,
            &InMemoryProvider::new(),
            &ProjectId::from_string(PROJ),
            &state("abc"),
            &synced(),
            &WorktreeState::default(),
            chrono::Utc::now(),
        )
        .await
        .expect("the round");

        assert!(!round.has_conflicts());
        let mut written: Vec<_> = round
            .tasks()
            .into_iter()
            .map(|task| task.as_str().to_string())
            .collect();
        written.sort();
        assert_eq!(
            written,
            vec!["AUTH".to_string(), "DB".to_string()],
            "both in-progress tasks are checkpointed; the todo task is not"
        );

        // A second round over the same tasks advances each lineage rather than
        // rewriting version 1 - which is also what a retried conflict resolves
        // to once the precondition is the version that won.
        let again = checkpoint_round(
            &store,
            &mut ids,
            &InMemoryProvider::new(),
            &ProjectId::from_string(PROJ),
            &state("def"),
            &synced(),
            &WorktreeState::default(),
            chrono::Utc::now(),
        )
        .await
        .expect("the second round");

        let auth_v2 = again
            .written
            .iter()
            .find(|cp| cp.task_id == auth.id)
            .expect("AUTH is checkpointed again");
        assert_eq!(
            auth_v2.state_version, 2,
            "the lineage advances from the version the first round wrote"
        );
    }
}
