//! [Repository] — a git working tree Director observes (Phase 2).
//!
//! Phase 1 established that project state is **observed, never remembered**;
//! [`crate::state::ProjectState`] is the shape of one observation. Phase 2
//! builds the layer that produces those observations and, crucially, the layer
//! that decides *what changed between two of them*.
//!
//! ## The separation this module enforces
//!
//! > Git observation answers "what happened in git?". It does not decide what
//! > to do about it.
//!
//! Everything here is noun and evidence. [`ProjectStateSnapshot`] is a
//! photograph; [`StateChangeKind`] is the difference between two photographs;
//! [`ObservationEvent`] is that difference written down. No assignment, no
//! planning, no "which agent should fix this" — those are later phases and
//! they consume these types; they do not live here.
//!
//! ## Determinism
//!
//! [`ProjectStateSnapshot::state_changes`] and [`ObservationEvent::key_for`]
//! are pure functions. Change detection is **never** delegated to an LLM: two
//! snapshots compared twice produce the same answer, and the same git state
//! observed three times produces one logical event, not three.

use serde::{Deserialize, Serialize};

use crate::ids::{EventId, ProjectId, RepositoryId};
use crate::state::{CommitInfo, FileChange};

/// The SHA length git reports for a full commit id. Used only to abbreviate for
/// display; storage always keeps the full 40 characters.
pub const SHORT_SHA_LEN: usize = 7;

/// Every event names its origin. Git observation is the only source in this
/// phase; later phases may add filesystem or verification sources.
pub const EVENT_SOURCE_GIT: &str = "git";

/// A git working tree that Director observes.
///
/// Deliberately a *record of what was seen*, not a live handle to git: the
/// adapter owns the git2 handle, the domain owns the shape of the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repository {
    /// This repository's identifier.
    pub id: RepositoryId,
    /// The project this repository belongs to. A project has one observed
    /// working tree; the back-reference lets events name both.
    pub project_id: ProjectId,
    /// Absolute, canonical path to the working tree. Never relative, never
    /// containing a `..` component — see [`Repository::validate_path`].
    pub local_path: String,
    /// The remote's URL, when the repository has one. Many repositories have
    /// none; `None` is normal, not a failure.
    pub remote_url: Option<String>,
    /// The branch the repository treats as its trunk, when it can be
    /// determined. Not assumed to be `main`: a repository on `develop` or
    /// `trunk` reports its own.
    pub default_branch: Option<String>,
    /// The branch currently checked out, or `None` under a detached HEAD.
    pub current_branch: Option<String>,
    /// The commit `HEAD` pointed at when this record was last refreshed.
    pub current_commit: String,
    /// Coarse state of the repository itself. See [`RepositoryStatus`].
    pub status: RepositoryStatus,
    /// When Director last successfully read this repository.
    pub last_observed_at: chrono::DateTime<chrono::Utc>,
    /// The last commit Director finished processing into events. During
    /// incremental sync this is the anchor for the next range walk; it lags
    /// `current_commit` only if a walk was bounded or interrupted.
    pub last_observed_commit: Option<String>,
    /// How many times Director has observed this repository. Monotonic; see
    /// [`Repository::bump_observation_version`].
    pub observation_version: u64,
    /// When Director registered the repository.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When this record last changed.
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Coarse state a repository can be in after a *successful* read.
///
/// Failure states are errors, not statuses: a path that is not a git repository
/// is reported as [`RepositoryError::NotARepository`] rather than silently
/// recorded here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryStatus {
    /// `HEAD` points at a branch. The ordinary case.
    OnBranch,
    /// `HEAD` points directly at a commit. There is no branch name to report,
    /// which is legitimate — but a snapshot's `branch` will be `None`.
    DetachedHead,
    /// The repository has no commits yet. `current_commit` is empty.
    Empty,
}

/// The normalized working-tree state at one moment.
///
/// Git's own status report is a flat list of `(xy-code, path)` pairs; this
/// structure is the same information bucketed by the categories Director
/// reasons about. Nothing is lost in normalization: every entry retains the
/// [`FileChange`] kind git reported, and renames keep both paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeState {
    /// True when every bucket below is empty — no uncommitted change of any
    /// kind, including untracked files.
    pub clean: bool,
    /// Files git already tracks that changed in the working tree.
    pub modified: Vec<FileChangeRecord>,
    /// Files staged or created in the tree that git did not track before.
    pub added: Vec<FileChangeRecord>,
    /// Files removed from the working tree.
    pub deleted: Vec<FileChangeRecord>,
    /// Files whose path changed; each carries its [`FileChangeRecord::old_path`].
    pub renamed: Vec<FileChangeRecord>,
    /// Files copied to a new path; only reported when git's copy detection is
    /// enabled, and each carries its source as `old_path`.
    pub copied: Vec<FileChangeRecord>,
    /// Files present in the tree that git is not tracking at all.
    pub untracked: Vec<FileChangeRecord>,
}

impl WorktreeState {
    /// Build the normalized state from raw change records.
    ///
    /// The records are bucketed by their git-reported kind. `clean` is derived
    /// rather than passed in, so it can never disagree with the buckets.
    pub fn from_changes(mut changes: Vec<FileChangeRecord>) -> Self {
        let clean = changes.is_empty();
        let mut state = WorktreeState {
            clean,
            ..WorktreeState::default()
        };
        for record in changes.drain(..) {
            match record.change_type {
                FileChange::Modified => state.modified.push(record),
                FileChange::Added => state.added.push(record),
                FileChange::Deleted => state.deleted.push(record),
                FileChange::Renamed => state.renamed.push(record),
                FileChange::Copied => state.copied.push(record),
                FileChange::Untracked => state.untracked.push(record),
                // A conflict is a modification that must be resolved before
                // anything else makes sense; report it as modified so it is
                // never silently invisible.
                FileChange::Conflicted => state.modified.push(record),
            }
        }
        state
    }

    /// Every changed file, in a stable order: bucket order then path order.
    pub fn all_changes(&self) -> Vec<&FileChangeRecord> {
        let mut all: Vec<&FileChangeRecord> = self
            .modified
            .iter()
            .chain(&self.added)
            .chain(&self.deleted)
            .chain(&self.renamed)
            .chain(&self.copied)
            .chain(&self.untracked)
            .collect();
        all.sort_by(|a, b| a.path.cmp(&b.path));
        all
    }

    /// The changes present in `self` that were *not* present in `previous`.
    ///
    /// This is the whole basis of worktree event derivation. Two rules matter:
    ///
    /// 1. A path that was dirty before and is dirty now is **not** reported
    ///    again, unless the observation anchor (the commit it is measured
    ///    against) moved — the same uncommitted edit observed twice is one
    ///    event, not two.
    /// 2. A path that was dirty before and is now clean is **not** reported as
    ///    a deletion. That is the ordinary signature of a commit, not a
    ///    removal, and reporting it as `FILE_DELETED` would be wrong.
    pub fn new_changes(&self, previous: &WorktreeState) -> Vec<FileChangeRecord> {
        let prior: Vec<ChangeIdentity> = previous
            .all_changes()
            .into_iter()
            .map(ChangeIdentity::from)
            .collect();

        self.all_changes()
            .into_iter()
            .filter(|record| !prior.contains(&ChangeIdentity::from(*record)))
            .cloned()
            .collect()
    }
}

/// What makes two file-change records *the same change* for idempotency
/// purposes: the path it happened to, the path it came from, the commit it is
/// anchored against, and its content fingerprint.
///
/// The change **kind is deliberately excluded**. A file that appears and is
/// then edited is the same path at two different contents — two facts, one
/// addition and one modification. If the kind participated in identity, the
/// second observation would be mistaken for a brand-new change of a different
/// shape; with the fingerprint alone, both facts are recorded and each is
/// idempotent under re-observation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChangeIdentity {
    path: String,
    old_path: Option<String>,
    commit_sha: String,
    fingerprint: String,
}

impl From<&FileChangeRecord> for ChangeIdentity {
    fn from(record: &FileChangeRecord) -> Self {
        ChangeIdentity {
            path: record.path.clone(),
            old_path: record.old_path.clone(),
            commit_sha: record.commit_sha.clone(),
            fingerprint: record.fingerprint.clone(),
        }
    }
}

/// One file change, observed in a working tree or in one commit.
///
/// This is the *evidence* record. It carries no judgement about which task a
/// file belongs to or whether the change was a good idea.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangeRecord {
    /// The repository the change was observed in.
    pub repository_id: RepositoryId,
    /// The changed path, repository-relative and forward-slash separated.
    pub path: String,
    /// The path before a rename or copy; `None` for every other change kind.
    /// Present exactly when [`FileChange::has_old_path`] is true.
    pub old_path: Option<String>,
    /// What kind of change git reported.
    pub change_type: FileChange,
    /// Lines added, when Director counted them. `None` means line statistics
    /// were not requested — for untracked files, or a plain status read.
    pub insertions: Option<u32>,
    /// Lines removed, when Director counted them. See [`insertions`](Self::insertions).
    pub deletions: Option<u32>,
    /// A content fingerprint used only for idempotency: the git blob oid when
    /// one is known, a filesystem proxy when one is not, or `"deleted"` for a
    /// removal.
    ///
    /// This is what makes an uncommitted change *progress* observable. A file
    /// that appears and is then edited is two facts — one addition, one
    /// modification — and they are distinguished by content, not by path. A
    /// file re-observed with identical content is one fact, no matter how many
    /// times it is looked at.
    pub fingerprint: String,
    /// The commit this change is anchored against. For a committed change this
    /// is the commit that contains it; for an uncommitted working-tree change
    /// it is the `HEAD` the change is measured from. This is what makes an
    /// uncommitted edit observed three times collapse to one record.
    pub commit_sha: String,
    /// When Director observed the change.
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

impl FileChangeRecord {
    /// Construct a record, rejecting the incoherent combination of a
    /// rename/copy kind with no `old_path`.
    pub fn new(
        repository_id: RepositoryId,
        path: impl Into<String>,
        old_path: Option<String>,
        change_type: FileChange,
        fingerprint: impl Into<String>,
        commit_sha: impl Into<String>,
        observed_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<Self, RepositoryError> {
        let path = path.into();
        let commit_sha = commit_sha.into();
        if change_type.has_old_path() && old_path.is_none() {
            return Err(RepositoryError::MalformedChange {
                path,
                reason: format!("{change_type:?} requires an old path"),
            });
        }
        if !change_type.has_old_path() && old_path.is_some() {
            return Err(RepositoryError::MalformedChange {
                path,
                reason: format!("{change_type:?} must not carry an old path"),
            });
        }
        Ok(FileChangeRecord {
            repository_id,
            path,
            old_path,
            change_type,
            insertions: None,
            deletions: None,
            fingerprint: fingerprint.into(),
            commit_sha,
            observed_at,
        })
    }

    /// Attach line counts. Returns the record for chaining.
    pub fn with_line_counts(mut self, insertions: u32, deletions: u32) -> Self {
        self.insertions = Some(insertions);
        self.deletions = Some(deletions);
        self
    }
}

/// Normalized diff information for one change, with enough metadata for later
/// phases to build on without re-reading git.
///
/// This is deliberately *not* a semantic diff. No AST, no symbol resolution —
/// those are later phases. Line counts are as deep as this goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffInfo {
    /// The path before the change; `None` for an added file.
    pub old_path: Option<String>,
    /// The path after the change.
    pub new_path: String,
    /// What kind of change it is.
    pub change_type: FileChange,
    /// Lines added.
    pub insertions: u32,
    /// Lines removed.
    pub deletions: u32,
    /// The commit the diff is anchored against.
    pub commit_sha: String,
    /// The branch the diff was observed on, when known.
    pub branch: Option<String>,
    /// When the diff was observed.
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

/// A photograph of repository state at one observation, comparable against
/// another photograph.
///
/// This is the type the Director compares when it asks "has the project moved
/// on since I last looked?" — the same question [`crate::checkpoint::Checkpoint`]
/// asks, answered at the repository level instead of the task level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStateSnapshot {
    /// The project this snapshot describes.
    pub project_id: ProjectId,
    /// The repository it was taken of.
    pub repository_id: RepositoryId,
    /// The checked-out branch, or `None` under a detached HEAD.
    pub branch: Option<String>,
    /// The commit `HEAD` points at. Empty for a repository with no commits.
    pub head_commit: String,
    /// The working tree at observation time. Empty buckets mean clean.
    pub working_tree: WorktreeState,
    /// True when the working tree has no uncommitted change of any kind.
    pub working_tree_clean: bool,
    /// The commit Director had finished processing when this snapshot was
    /// taken. The anchor for the next incremental walk.
    pub last_observed_commit: String,
    /// When the observation was made.
    pub observed_at: chrono::DateTime<chrono::Utc>,
    /// The project's state version at this snapshot. Monotonically increases
    /// whenever an important repository-state change is recorded — see
    /// [`ProjectStateSnapshot::with_bumped_version`].
    pub state_version: u64,
}

impl ProjectStateSnapshot {
    /// Take the initial snapshot of a freshly observed repository. This
    /// establishes the baseline: everything after this is a *change*.
    pub fn initial(
        project_id: ProjectId,
        repository_id: RepositoryId,
        branch: Option<String>,
        head_commit: String,
        working_tree: WorktreeState,
        observed_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let clean = working_tree.clean;
        ProjectStateSnapshot {
            project_id,
            repository_id,
            branch,
            head_commit: head_commit.clone(),
            working_tree,
            working_tree_clean: clean,
            // The baseline commit is the head: nothing has been walked yet,
            // so there is no prior commit to anchor a range against.
            last_observed_commit: head_commit,
            observed_at,
            // The first observation establishes version 1.
            state_version: 1,
        }
    }

    /// The changes between a previous snapshot and this one, as coarse
    /// categories. Deterministic, total, and never LLM-derived.
    ///
    /// Note the ordering matters to callers: branch is reported before commit
    /// and commit before worktree, because a branch change *explains* a commit
    /// change and a commit change *explains* a worktree change.
    pub fn state_changes(&self, previous: &ProjectStateSnapshot) -> Vec<StateChangeKind> {
        let mut changes = Vec::new();

        if self.branch != previous.branch {
            changes.push(StateChangeKind::BranchChanged);
        }
        if self.head_commit != previous.head_commit {
            changes.push(StateChangeKind::CommitChanged);
        }
        if self.working_tree_clean != previous.working_tree_clean
            || !self
                .working_tree
                .new_changes(&previous.working_tree)
                .is_empty()
        {
            changes.push(StateChangeKind::WorktreeChanged);
        }

        changes
    }

    /// Whether this snapshot records any important change relative to
    /// `previous`. A change is *important* if it is one a Director would act
    /// on: a branch move, a new commit, or a different working tree.
    pub fn has_important_change(&self, previous: &ProjectStateSnapshot) -> bool {
        !self.state_changes(previous).is_empty()
    }

    /// Return a copy of this snapshot with `state_version` advanced by one.
    ///
    /// Phase 1 established no versioning mechanism, so Phase 2 introduces this
    /// one: the version lives on the snapshot, is set once at initialization,
    /// and is bumped exactly once per observation that records an important
    /// change. There is deliberately no second counter anywhere else.
    pub fn with_bumped_version(&self) -> Self {
        let mut next = self.clone();
        next.state_version += 1;
        next
    }
}

/// A category of change between two snapshots. The deterministic output of
/// comparing two [`ProjectStateSnapshot`]s.
///
/// Distinct from [`crate::state::StateComparison`]: that compares a *checkpoint*
/// against observed state to decide whether a resume is safe; this compares two
/// *observations* to decide what happened in git.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateChangeKind {
    /// The checked-out branch moved.
    BranchChanged,
    /// `HEAD` advanced or moved to a different commit.
    CommitChanged,
    /// The working tree differs — files entered, left, or changed their state.
    WorktreeChanged,
}

/// What kind of fact an [`ObservationEvent`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Director synchronized a repository against git. Emitted at most once
    /// per distinct observed state, so a no-op re-sync produces nothing new.
    RepositorySynced,
    /// The checked-out branch changed.
    BranchChanged,
    /// A commit Director had not seen before.
    CommitCreated,
    /// A file entered the tracked tree.
    FileAdded,
    /// A tracked file's content changed.
    FileModified,
    /// A file left the tree.
    FileDeleted,
    /// A file moved to a new path.
    FileRenamed,
    /// A file was copied to a new path.
    FileCopied,
}

impl EventKind {
    /// The snake_case name used as a component of an event's idempotency key.
    pub fn as_key_component(self) -> &'static str {
        match self {
            EventKind::RepositorySynced => "repository_synced",
            EventKind::BranchChanged => "branch_changed",
            EventKind::CommitCreated => "commit_created",
            EventKind::FileAdded => "file_added",
            EventKind::FileModified => "file_modified",
            EventKind::FileDeleted => "file_deleted",
            EventKind::FileRenamed => "file_renamed",
            EventKind::FileCopied => "file_copied",
        }
    }

    /// The [`EventKind`] that corresponds to a working-tree change kind.
    ///
    /// An untracked file maps to [`EventKind::FileAdded`]: from the project's
    /// point of view a file that is present but not yet tracked has still been
    /// added to the working tree. A conflicted file maps to
    /// [`EventKind::FileModified`], because it is a tracked file whose content
    /// is in dispute — never a silent no-event.
    pub fn for_file_change(change: FileChange) -> Option<Self> {
        match change {
            FileChange::Added | FileChange::Untracked => Some(EventKind::FileAdded),
            FileChange::Modified | FileChange::Conflicted => Some(EventKind::FileModified),
            FileChange::Deleted => Some(EventKind::FileDeleted),
            FileChange::Renamed => Some(EventKind::FileRenamed),
            FileChange::Copied => Some(EventKind::FileCopied),
        }
    }
}

/// Typed payload of an [`ObservationEvent`]. Kept structured rather than a free
/// `Value` so consumers can match on it without re-parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventData {
    /// Payload for [`EventKind::RepositorySynced`].
    Sync {
        /// The `HEAD` the sync landed on.
        commit: String,
        /// The branch it landed on, if any.
        branch: Option<String>,
        /// Whether the working tree was clean at sync time.
        working_tree_clean: bool,
    },
    /// Payload for [`EventKind::BranchChanged`].
    Branch {
        /// The branch that was checked out before.
        from: Option<String>,
        /// The branch checked out now.
        to: Option<String>,
    },
    /// Payload for [`EventKind::CommitCreated`].
    Commit(Box<CommitInfo>),
    /// Payload for every file event kind.
    File(FileChangeRecord),
}

/// One fact Director learned by observing git.
///
/// Every event has a **stable identity**: [`ObservationEvent::key_for`] turns
/// what the event describes into an [`EventId`], so the same commit or the same
/// uncommitted edit observed any number of times yields exactly one event. This
/// is the mechanism behind Phase 2's idempotency requirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationEvent {
    /// The event's stable identifier, derived from what it describes.
    pub event_id: EventId,
    /// The project the event belongs to.
    pub project_id: ProjectId,
    /// The repository the event was observed in.
    pub repository_id: RepositoryId,
    /// What kind of fact this is.
    pub kind: EventKind,
    /// When Director recorded it.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Where the fact came from. Always [`EVENT_SOURCE_GIT`] in this phase.
    pub source: String,
    /// The fact itself.
    pub data: EventData,
}

impl ObservationEvent {
    /// Compute the stable id for a fact before the event exists.
    ///
    /// The key is a digest over everything that makes the fact *this* fact:
    /// repository, kind, the commit it is anchored against, and — for file
    /// facts — the paths involved. Two calls with the same inputs always yield
    /// the same [`EventId`], which is what makes re-observation idempotent.
    pub fn key_for(
        repository_id: &RepositoryId,
        kind: EventKind,
        anchor: &str,
        path: Option<&str>,
        old_path: Option<&str>,
    ) -> EventId {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(repository_id.as_str().as_bytes());
        hasher.update(b"\x00");
        hasher.update(kind.as_key_component().as_bytes());
        hasher.update(b"\x00");
        hasher.update(anchor.as_bytes());
        hasher.update(b"\x00");
        if let Some(path) = path {
            hasher.update(path.as_bytes());
        }
        hasher.update(b"\x00");
        if let Some(old_path) = old_path {
            hasher.update(old_path.as_bytes());
        }

        let digest = hasher.finalize();
        // 16 hex characters is 64 bits — comfortably unique within one
        // repository's event stream, and short enough to stay readable.
        let hex: String = digest
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        EventId::from_string(format!("EVT-{hex}"))
    }

    /// Build a sync event for a snapshot.
    pub fn sync(snapshot: &ProjectStateSnapshot, timestamp: chrono::DateTime<chrono::Utc>) -> Self {
        let event_id = Self::key_for(
            &snapshot.repository_id,
            EventKind::RepositorySynced,
            &snapshot.head_commit,
            None,
            None,
        );
        ObservationEvent {
            event_id,
            project_id: snapshot.project_id.clone(),
            repository_id: snapshot.repository_id.clone(),
            kind: EventKind::RepositorySynced,
            timestamp,
            source: EVENT_SOURCE_GIT.to_string(),
            data: EventData::Sync {
                commit: snapshot.head_commit.clone(),
                branch: snapshot.branch.clone(),
                working_tree_clean: snapshot.working_tree_clean,
            },
        }
    }

    /// Build a branch-change event.
    pub fn branch_change(
        repository_id: &RepositoryId,
        project_id: &ProjectId,
        from: Option<String>,
        to: Option<String>,
        anchor: &str,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let event_id = Self::key_for(
            repository_id,
            EventKind::BranchChanged,
            anchor,
            to.as_deref(),
            from.as_deref(),
        );
        ObservationEvent {
            event_id,
            project_id: project_id.clone(),
            repository_id: repository_id.clone(),
            kind: EventKind::BranchChanged,
            timestamp,
            source: EVENT_SOURCE_GIT.to_string(),
            data: EventData::Branch { from, to },
        }
    }

    /// Build a commit event.
    pub fn commit(
        repository_id: &RepositoryId,
        project_id: &ProjectId,
        commit: CommitInfo,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let event_id = Self::key_for(
            repository_id,
            EventKind::CommitCreated,
            &commit.sha,
            None,
            None,
        );
        ObservationEvent {
            event_id,
            project_id: project_id.clone(),
            repository_id: repository_id.clone(),
            kind: EventKind::CommitCreated,
            timestamp,
            source: EVENT_SOURCE_GIT.to_string(),
            data: EventData::Commit(Box::new(commit)),
        }
    }

    /// Build a file-change event. Returns `None` for change kinds that have no
    /// event of their own (untracked, plain conflicted has one via modified).
    pub fn file(
        record: FileChangeRecord,
        project_id: &ProjectId,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> Option<Self> {
        let kind = EventKind::for_file_change(record.change_type)?;
        let event_id = Self::key_for(
            &record.repository_id,
            kind,
            &record.commit_sha,
            Some(&record.path),
            record.old_path.as_deref(),
        );
        Some(ObservationEvent {
            event_id,
            project_id: project_id.clone(),
            repository_id: record.repository_id.clone(),
            kind,
            timestamp,
            source: EVENT_SOURCE_GIT.to_string(),
            data: EventData::File(record),
        })
    }
}

/// Outcome of a repository synchronization.
///
/// The structured answer to "what happened when I synced?". Compare it to
/// [`ProjectStateSnapshot::state_changes`]: this is the *recorded* result of a
/// sync, that is the *pure* comparison the sync was built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncResult {
    /// What the sync concluded overall.
    pub status: SyncStatus,
    /// The branch changed between the previous snapshot and this one.
    pub branch_changed: bool,
    /// `HEAD` moved to a different commit.
    pub commit_changed: bool,
    /// The working tree changed.
    pub working_tree_changed: bool,
    /// How many events this sync added that were not already known. Zero for a
    /// no-op re-sync — this is the idempotency guarantee, observable.
    pub events_created: usize,
    /// The commit the sync started from, if Director had seen this repository
    /// before.
    pub previous_commit: Option<String>,
    /// The commit the sync landed on.
    pub current_commit: String,
    /// The project state version after the sync.
    pub state_version: u64,
}

/// What a sync concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    /// The repository was synced and at least one new fact was recorded.
    Synced,
    /// Observation succeeded but nothing had changed since last time.
    NoChange,
    /// The repository was synced for the first time and a baseline was
    /// established rather than a change detected.
    Baseline,
}

/// Errors the repository layer can produce.
///
/// Path handling is deliberately strict because repository paths are untrusted
/// input: a path that is not absolute, or that escapes its own root through a
/// `..` component, is rejected rather than normalized away.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    /// The path is not a git repository. Never silently accepted.
    #[error("not a git repository: {path}")]
    NotARepository {
        /// The rejected path.
        path: String,
    },
    /// The path does not exist at all.
    #[error("path does not exist: {path}")]
    PathMissing {
        /// The missing path.
        path: String,
    },
    /// The path was not acceptable for security reasons.
    #[error("unsafe repository path: {reason}")]
    UnsafePath {
        /// Why the path was rejected.
        reason: String,
    },
    /// Reading git failed in a way the observer could not interpret.
    #[error("git observation failed: {0}")]
    ObservationFailed(String),
    /// A change record contradicted itself.
    #[error("malformed change for {path}: {reason}")]
    MalformedChange {
        /// The path involved.
        path: String,
        /// What was wrong.
        reason: String,
    },
    /// The repository has no commits and the operation requires one.
    #[error("repository has no commits yet")]
    Empty,
    /// A sync raced with another sync and lost the version check.
    #[error("concurrent modification: stored version {stored} is newer than {observed}")]
    ConcurrentModification {
        /// The version the store now holds.
        stored: u64,
        /// The version the caller had read.
        observed: u64,
    },
    /// The store could not be read or written.
    #[error("repository store failed: {0}")]
    Store(String),
    /// No repository is registered with this id.
    #[error("repository not registered: {0}")]
    NotRegistered(String),
}

impl Repository {
    /// Register a repository record.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: RepositoryId,
        project_id: ProjectId,
        local_path: impl Into<String>,
        remote_url: Option<String>,
        default_branch: Option<String>,
        current_branch: Option<String>,
        current_commit: String,
        status: RepositoryStatus,
        last_observed_commit: Option<String>,
    ) -> Self {
        let now = chrono::Utc::now();
        Repository {
            id,
            project_id,
            local_path: local_path.into(),
            remote_url,
            default_branch,
            current_branch,
            current_commit,
            status,
            last_observed_at: now,
            last_observed_commit,
            observation_version: 1,
            created_at: now,
            updated_at: now,
        }
    }

    /// Record a change, stamping `updated_at`.
    pub fn touch(&mut self) {
        self.updated_at = chrono::Utc::now();
    }

    /// Record that the repository was observed again, stamping timestamps and
    /// advancing the observation counter.
    pub fn mark_observed(
        &mut self,
        current_branch: Option<String>,
        current_commit: String,
        status: RepositoryStatus,
    ) {
        let now = chrono::Utc::now();
        self.current_branch = current_branch;
        self.current_commit = current_commit;
        self.status = status;
        self.last_observed_at = now;
        self.updated_at = now;
        self.observation_version += 1;
    }

    /// Record the last commit Director finished processing.
    pub fn set_last_observed_commit(&mut self, sha: String) {
        self.last_observed_commit = Some(sha);
        self.updated_at = chrono::Utc::now();
    }

    /// Advance the observation version without re-reading git, for a sync that
    /// recorded an important change.
    pub fn bump_observation_version(&mut self) {
        self.observation_version += 1;
        self.updated_at = chrono::Utc::now();
    }

    /// Reject a repository path that is not safe to observe.
    ///
    /// The rules are deliberately simple and total: the path must be absolute,
    /// must contain no `..` component, and must not be the filesystem root.
    /// Canonicalization happens in the adapter, which is the layer that can
    /// touch the filesystem; this check operates on the string as given, so a
    /// caller cannot smuggle a traversal past it before canonicalization.
    pub fn validate_path(path: &str) -> Result<(), RepositoryError> {
        if path.is_empty() {
            return Err(RepositoryError::UnsafePath {
                reason: "path is empty".into(),
            });
        }
        if path.trim() != path {
            return Err(RepositoryError::UnsafePath {
                reason: "path has surrounding whitespace".into(),
            });
        }
        // Windows roots (`C:\`, `D:/`) are absolute; a bare `/` or `\` is not
        // something Director should ever observe.
        let looks_absolute = path.starts_with('/') || path.starts_with('\\') || {
            let bytes = path.as_bytes();
            bytes.len() >= 3
                && bytes[1] == b':'
                && (bytes[2] == b'\\' || bytes[2] == b'/')
                && bytes[0].is_ascii_alphabetic()
        };
        if !looks_absolute {
            return Err(RepositoryError::UnsafePath {
                reason: format!("repository path must be absolute, got {path:?}"),
            });
        }
        for component in path.split(['/', '\\']) {
            if component == ".." {
                return Err(RepositoryError::UnsafePath {
                    reason: "repository path must not contain a `..` component".into(),
                });
            }
        }
        Ok(())
    }

    /// The head commit abbreviated for display, keeping the full SHA in storage.
    pub fn short_commit(&self) -> &str {
        self.current_commit
            .get(..SHORT_SHA_LEN)
            .unwrap_or(&self.current_commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ProjectId, RepositoryId};

    fn repo_id() -> RepositoryId {
        RepositoryId::from_string("REPO-1")
    }
    fn project_id() -> ProjectId {
        ProjectId::from_string("PROJ-1")
    }

    fn record(path: &str, change_type: FileChange, commit: &str) -> FileChangeRecord {
        record_fp(path, change_type, commit, "fp-default")
    }

    fn record_fp(path: &str, change_type: FileChange, commit: &str, fp: &str) -> FileChangeRecord {
        let old_path: Option<String> = if change_type.has_old_path() {
            Some(format!("old/{path}"))
        } else {
            None
        };
        FileChangeRecord::new(
            repo_id(),
            path,
            old_path,
            change_type,
            fp,
            commit,
            chrono::Utc::now(),
        )
        .expect("a well-formed record")
    }

    fn snapshot(
        commit: &str,
        branch: Option<&str>,
        changes: Vec<FileChangeRecord>,
    ) -> ProjectStateSnapshot {
        ProjectStateSnapshot::initial(
            project_id(),
            repo_id(),
            branch.map(str::to_string),
            commit.to_string(),
            WorktreeState::from_changes(changes),
            chrono::Utc::now(),
        )
    }

    // --- path validation -------------------------------------------------

    #[test]
    fn an_absolute_path_is_accepted() {
        assert!(Repository::validate_path("/repo/checkout").is_ok());
        assert!(Repository::validate_path("C:\\repo\\checkout").is_ok());
        assert!(Repository::validate_path("D:/repo/checkout").is_ok());
    }

    #[test]
    fn a_relative_path_is_rejected() {
        assert!(Repository::validate_path("repo/checkout").is_err());
        assert!(Repository::validate_path("./repo").is_err());
    }

    #[test]
    fn a_path_traversal_is_rejected() {
        assert!(Repository::validate_path("/repo/../etc").is_err());
        assert!(Repository::validate_path("/repo/..").is_err());
    }

    #[test]
    fn an_empty_path_is_rejected() {
        assert!(Repository::validate_path("").is_err());
        assert!(Repository::validate_path(" /repo").is_err());
    }

    // --- change record coherence ----------------------------------------

    #[test]
    fn a_rename_requires_an_old_path() {
        let err = FileChangeRecord::new(
            repo_id(),
            "src/new.rs",
            None,
            FileChange::Renamed,
            "fp",
            "abc",
            chrono::Utc::now(),
        )
        .unwrap_err();
        assert!(matches!(err, RepositoryError::MalformedChange { .. }));
    }

    #[test]
    fn a_modified_change_must_not_carry_an_old_path() {
        assert!(FileChangeRecord::new(
            repo_id(),
            "src/lib.rs",
            Some("src/other.rs".to_string()),
            FileChange::Modified,
            "fp",
            "abc",
            chrono::Utc::now(),
        )
        .is_err());
    }

    #[test]
    fn line_counts_are_optional_but_attachable() {
        let r = record("src/lib.rs", FileChange::Modified, "abc").with_line_counts(10, 2);
        assert_eq!(r.insertions, Some(10));
        assert_eq!(r.deletions, Some(2));
        assert_eq!(record("a", FileChange::Added, "b").insertions, None);
    }

    // --- worktree normalization -----------------------------------------

    #[test]
    fn an_empty_change_list_is_clean() {
        assert!(WorktreeState::from_changes(vec![]).clean);
    }

    #[test]
    fn changes_are_bucketed_by_kind() {
        let state = WorktreeState::from_changes(vec![
            record("a.rs", FileChange::Modified, "c1"),
            record("b.rs", FileChange::Added, "c1"),
            record("c.rs", FileChange::Deleted, "c1"),
            record("d.rs", FileChange::Renamed, "c1"),
            record("e.rs", FileChange::Copied, "c1"),
            record("f.rs", FileChange::Untracked, "c1"),
        ]);
        assert!(!state.clean);
        assert_eq!(state.modified.len(), 1);
        assert_eq!(state.added.len(), 1);
        assert_eq!(state.deleted.len(), 1);
        assert_eq!(state.renamed.len(), 1);
        assert_eq!(state.copied.len(), 1);
        assert_eq!(state.untracked.len(), 1);
        assert_eq!(state.all_changes().len(), 6);
    }

    #[test]
    fn a_conflicted_file_is_reported_as_modified_not_dropped() {
        let state = WorktreeState::from_changes(vec![record("a.rs", FileChange::Conflicted, "c1")]);
        assert_eq!(state.modified.len(), 1);
    }

    #[test]
    fn a_renamed_file_keeps_both_paths() {
        let state = WorktreeState::from_changes(vec![record("a.rs", FileChange::Renamed, "c1")]);
        let renamed = &state.renamed[0];
        assert_eq!(renamed.path, "a.rs");
        assert_eq!(renamed.old_path.as_deref(), Some("old/a.rs"));
    }

    // --- change detection ------------------------------------------------

    #[test]
    fn identical_snapshots_have_no_changes() {
        let a = snapshot("abc", Some("main"), vec![]);
        let b = snapshot("abc", Some("main"), vec![]);
        assert!(a.state_changes(&b).is_empty());
        assert!(!a.has_important_change(&b));
    }

    #[test]
    fn a_commit_change_is_detected() {
        let a = snapshot("abc", Some("main"), vec![]);
        let b = snapshot("def", Some("main"), vec![]);
        assert_eq!(a.state_changes(&b), vec![StateChangeKind::CommitChanged]);
    }

    #[test]
    fn a_branch_change_is_detected_and_reported_first() {
        let a = snapshot("abc", Some("main"), vec![]);
        let b = snapshot("def", Some("feature/auth"), vec![]);
        let changes = a.state_changes(&b);
        assert_eq!(
            changes,
            vec![
                StateChangeKind::BranchChanged,
                StateChangeKind::CommitChanged
            ]
        );
    }

    #[test]
    fn a_new_worktree_change_is_detected() {
        let a = snapshot("abc", Some("main"), vec![]);
        let b = snapshot(
            "abc",
            Some("main"),
            vec![record("auth.py", FileChange::Modified, "abc")],
        );
        assert_eq!(a.state_changes(&b), vec![StateChangeKind::WorktreeChanged]);
    }

    #[test]
    fn the_same_uncommitted_change_is_not_a_new_change() {
        let previous =
            WorktreeState::from_changes(vec![record("auth.py", FileChange::Modified, "abc")]);
        let current =
            WorktreeState::from_changes(vec![record("auth.py", FileChange::Modified, "abc")]);
        assert!(current.new_changes(&previous).is_empty());
    }

    #[test]
    fn the_same_edit_after_a_new_commit_is_a_new_change() {
        let previous =
            WorktreeState::from_changes(vec![record("auth.py", FileChange::Modified, "abc")]);
        let current =
            WorktreeState::from_changes(vec![record("auth.py", FileChange::Modified, "def")]);
        assert_eq!(current.new_changes(&previous).len(), 1);
    }

    #[test]
    fn an_edit_of_the_same_file_is_a_new_change() {
        // The acceptance-test progression: a file appears, then is edited.
        // Different content at the same path is a new fact.
        let previous = WorktreeState::from_changes(vec![record_fp(
            "auth.py",
            FileChange::Added,
            "abc",
            "fp1",
        )]);
        let current = WorktreeState::from_changes(vec![record_fp(
            "auth.py",
            FileChange::Modified,
            "abc",
            "fp2",
        )]);
        assert_eq!(current.new_changes(&previous).len(), 1);
    }

    #[test]
    fn re_observing_the_same_content_is_not_a_new_change() {
        let previous = WorktreeState::from_changes(vec![record_fp(
            "auth.py",
            FileChange::Added,
            "abc",
            "fp1",
        )]);
        let current = WorktreeState::from_changes(vec![record_fp(
            "auth.py",
            FileChange::Added,
            "abc",
            "fp1",
        )]);
        assert!(current.new_changes(&previous).is_empty());
    }

    #[test]
    fn a_committed_change_is_not_reported_as_a_deletion() {
        // auth.py was modified-and-uncommitted before; now the tree is clean
        // because the edit was committed. That is not a deletion.
        let previous =
            WorktreeState::from_changes(vec![record("auth.py", FileChange::Modified, "abc")]);
        let current = WorktreeState::from_changes(vec![]);
        assert!(current.new_changes(&previous).is_empty());
    }

    #[test]
    fn a_transition_from_dirty_to_clean_is_a_worktree_change() {
        let a = snapshot(
            "abc",
            Some("main"),
            vec![record("x", FileChange::Modified, "abc")],
        );
        let b = snapshot("abc", Some("main"), vec![]);
        assert_eq!(a.state_changes(&b), vec![StateChangeKind::WorktreeChanged]);
    }

    // --- versioning ------------------------------------------------------

    #[test]
    fn the_initial_snapshot_establishes_version_one() {
        assert_eq!(snapshot("abc", Some("main"), vec![]).state_version, 1);
    }

    #[test]
    fn bumping_advances_the_version_by_one() {
        let s = snapshot("abc", Some("main"), vec![]);
        assert_eq!(s.with_bumped_version().state_version, 2);
        assert_eq!(
            s.with_bumped_version().with_bumped_version().state_version,
            3
        );
    }

    // --- idempotency -----------------------------------------------------

    #[test]
    fn the_same_fact_yields_the_same_event_id() {
        let a =
            ObservationEvent::key_for(&repo_id(), EventKind::CommitCreated, "abc123", None, None);
        let b =
            ObservationEvent::key_for(&repo_id(), EventKind::CommitCreated, "abc123", None, None);
        assert_eq!(a, b);
    }

    #[test]
    fn different_facts_yield_different_event_ids() {
        let commit =
            ObservationEvent::key_for(&repo_id(), EventKind::CommitCreated, "abc", None, None);
        let other =
            ObservationEvent::key_for(&repo_id(), EventKind::CommitCreated, "def", None, None);
        let file = ObservationEvent::key_for(
            &repo_id(),
            EventKind::FileModified,
            "abc",
            Some("auth.py"),
            None,
        );
        assert_ne!(commit, other);
        assert_ne!(commit, file);
    }

    #[test]
    fn event_ids_differ_across_repositories() {
        let a = ObservationEvent::key_for(
            &RepositoryId::from_string("REPO-1"),
            EventKind::CommitCreated,
            "abc",
            None,
            None,
        );
        let b = ObservationEvent::key_for(
            &RepositoryId::from_string("REPO-2"),
            EventKind::CommitCreated,
            "abc",
            None,
            None,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn a_rename_and_a_modification_of_the_same_path_differ() {
        let modified = ObservationEvent::key_for(
            &repo_id(),
            EventKind::FileModified,
            "abc",
            Some("auth.py"),
            None,
        );
        let renamed = ObservationEvent::key_for(
            &repo_id(),
            EventKind::FileRenamed,
            "abc",
            Some("auth/service.py"),
            Some("auth.py"),
        );
        assert_ne!(modified, renamed);
    }

    #[test]
    fn every_change_kind_has_an_event_kind() {
        assert_eq!(
            EventKind::for_file_change(FileChange::Added),
            Some(EventKind::FileAdded)
        );
        assert_eq!(
            EventKind::for_file_change(FileChange::Conflicted),
            Some(EventKind::FileModified)
        );
    }

    #[test]
    fn a_file_event_can_be_built_from_a_record() {
        let event = ObservationEvent::file(
            record("auth.py", FileChange::Added, "abc"),
            &project_id(),
            chrono::Utc::now(),
        )
        .expect("an added file has an event");
        assert_eq!(event.kind, EventKind::FileAdded);
        assert!(event.event_id.as_str().starts_with("EVT-"));
        assert_eq!(event.source, EVENT_SOURCE_GIT);
    }

    #[test]
    fn an_untracked_file_maps_to_a_file_added_event() {
        assert_eq!(
            EventKind::for_file_change(FileChange::Untracked),
            Some(EventKind::FileAdded)
        );
    }

    // --- round-tripping --------------------------------------------------

    #[test]
    fn repository_and_snapshot_round_trip_through_serde() {
        let repo = Repository::new(
            repo_id(),
            project_id(),
            "/repo/checkout",
            Some("https://example.com/checkout.git".into()),
            Some("main".into()),
            Some("feature/auth".into()),
            "a81c2d9".into(),
            RepositoryStatus::OnBranch,
            Some("a81c2d9".into()),
        );
        let json = serde_json::to_string(&repo).unwrap();
        let back: Repository = serde_json::from_str(&json).unwrap();
        assert_eq!(repo, back);

        let snap = snapshot(
            "abc",
            Some("main"),
            vec![record("a", FileChange::Added, "abc")],
        );
        let json = serde_json::to_string(&snap).unwrap();
        let back: ProjectStateSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn a_pre_phase2_commit_payload_still_deserializes() {
        // A payload written before Phase 2 widened CommitInfo must still read.
        let legacy = serde_json::json!({
            "sha": "abc",
            "summary": "fix",
            "author": "ada",
            "committed_at": "2026-01-01T00:00:00Z"
        });
        let parsed: CommitInfo = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.sha, "abc");
        assert!(parsed.parents.is_empty());
        assert_eq!(parsed.committer, "");
        assert_eq!(parsed.message, "");
    }
}
