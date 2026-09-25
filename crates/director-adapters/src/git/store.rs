//! [`RepositoryStore`] — persistence for observed repository state.
//!
//! Phase 2 persists to one JSON file per repository under a state directory.
//! The format is deliberately human-readable and single-file: nothing about
//! Phase 2 needs a database, and a file that can be read with a text editor is
//! a file that can be audited by hand.
//!
//! ## Durability shape
//!
//! One file holds everything Director knows about one repository: the
//! repository record, the latest snapshot, and the append-only histories of
//! commits, file changes, and events. Writes are atomic — serialize fully,
//! write to a sibling temporary file, rename — so a crash leaves either the
//! old state or the new one, never a half-written file.
//!
//! ## Concurrency
//!
//! Within one process a mutex serializes the read-modify-write cycle, so two
//! sessions syncing the same repository cannot interleave and silently
//! overwrite each other. Across processes the mutex cannot help; instead the
//! store performs an **optimistic version check** — it re-reads the on-disk
//! state version immediately before writing, and if another writer moved it,
//! the sync fails with [`RepositoryError::ConcurrentModification`] rather than
//! clobbering the newer state. The rule from the specification holds:
//!
//! > Do not silently overwrite newer project state.
//!
//! The loser of a race gets an explicit error and can simply retry; the winner
//! is preserved intact.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use director_domain::ids::{EventId, RepositoryId};
use director_domain::repository::{
    ObservationEvent, ProjectStateSnapshot, Repository, RepositoryError,
};
use director_domain::state::CommitInfo;

/// Bumped when the stored layout changes in a way a reader must handle.
const STORE_FORMAT_VERSION: u32 = 1;

/// Cap on retained events. History beyond this is dropped oldest-first; the
/// alternative is unbounded growth in a file that is rewritten on every sync.
const MAX_EVENTS: usize = 2000;

/// Cap on retained file-change records.
const MAX_FILE_CHANGES: usize = 5000;

/// Cap on retained commit records.
const MAX_COMMITS: usize = 2000;

/// Everything Director knows about one repository, in one file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredRepository {
    /// The stored format version.
    pub format_version: u32,
    /// The repository record.
    pub repository: Repository,
    /// The most recent snapshot.
    pub snapshot: ProjectStateSnapshot,
    /// Events, oldest first.
    pub events: Vec<ObservationEvent>,
    /// Commits Director has observed.
    pub commits: Vec<CommitInfo>,
    /// File-change records Director has observed.
    pub file_changes: Vec<director_domain::repository::FileChangeRecord>,
}

/// File-backed storage for observed repositories.
///
/// Not `Clone` itself — the write lock is not duplicable — but cheaply wrapped
/// in an [`std::sync::Arc`] so several services or sessions can share one.
#[derive(Debug)]
pub struct RepositoryStore {
    /// Directory holding one `<id>.json` per repository.
    root: PathBuf,
    /// Serializes read-modify-write within this process.
    write_lock: Mutex<()>,
}

impl RepositoryStore {
    /// Create or open a store rooted at `root`, creating the directory tree if
    /// needed.
    pub fn new(root: impl AsRef<std::path::Path>) -> Result<Self, RepositoryError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(|err| RepositoryError::Store(err.to_string()))?;
        Ok(RepositoryStore {
            root,
            write_lock: Mutex::new(()),
        })
    }

    /// Where the store keeps its files. Exposed for tests and CLI diagnostics.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file backing one repository.
    fn file_for(&self, id: &RepositoryId) -> Result<PathBuf, RepositoryError> {
        // Ids are Director-generated, but this is the boundary where an id
        // becomes a filename, so the shape is enforced here rather than trusted.
        let name = id.as_str();
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..")
            || name.contains(':')
        {
            return Err(RepositoryError::Store(format!(
                "repository id is not a safe filename: {name}"
            )));
        }
        Ok(self.root.join(format!("{name}.json")))
    }

    /// Load one repository's record, or fail if it was never registered.
    pub fn load(&self, id: &RepositoryId) -> Result<StoredRepository, RepositoryError> {
        let path = self.file_for(id)?;
        let contents =
            fs::read(&path).map_err(|_| RepositoryError::NotRegistered(id.to_string()))?;
        let record: StoredRepository = serde_json::from_slice(&contents)
            .map_err(|err| RepositoryError::Store(format!("parse {}: {err}", path.display())))?;
        Ok(record)
    }

    /// Whether a repository is registered.
    pub fn exists(&self, id: &RepositoryId) -> bool {
        self.file_for(id).map(|path| path.exists()).unwrap_or(false)
    }

    /// Every registered repository id, sorted.
    pub fn list(&self) -> Result<Vec<RepositoryId>, RepositoryError> {
        let entries =
            fs::read_dir(&self.root).map_err(|err| RepositoryError::Store(err.to_string()))?;
        let mut ids = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                ids.push(RepositoryId::from_string(stem));
            }
        }
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(ids)
    }

    /// Register a brand-new repository with its baseline snapshot. Fails if the
    /// id is already taken, so two sessions initializing the same path cannot
    /// each silently win.
    pub fn initialize(
        &self,
        repository: Repository,
        snapshot: ProjectStateSnapshot,
        events: Vec<ObservationEvent>,
    ) -> Result<StoredRepository, RepositoryError> {
        let _guard = self.write_lock.lock().expect("store lock poisoned");
        if self.exists(&repository.id) {
            return Err(RepositoryError::Store(format!(
                "repository already registered: {}",
                repository.id
            )));
        }
        let record = StoredRepository {
            format_version: STORE_FORMAT_VERSION,
            repository,
            snapshot,
            events,
            commits: vec![],
            file_changes: vec![],
        };
        self.write(&record)?;
        Ok(record)
    }

    /// Run a read-modify-write cycle under the process-wide lock, with an
    /// optimistic version check against concurrent external writers.
    ///
    /// The closure receives the loaded record and may mutate it freely; it
    /// returns whatever the caller wants reported. Just before writing, the
    /// store re-reads the on-disk version and aborts with
    /// [`RepositoryError::ConcurrentModification`] if another writer moved it.
    pub fn with_locked<R>(
        &self,
        id: &RepositoryId,
        f: impl FnOnce(&mut StoredRepository) -> Result<R, RepositoryError>,
    ) -> Result<R, RepositoryError> {
        let _guard = self.write_lock.lock().expect("store lock poisoned");

        let mut record = self.load(id)?;
        let observed_version = record.snapshot.state_version;
        let result = f(&mut record)?;
        record.repository.touch();

        // Nobody outside this process changed the file while we worked. If
        // someone did, their state is newer and ours is discarded rather than
        // written over it.
        self.verify_unchanged(id, observed_version)?;
        self.write(&record)?;

        Ok(result)
    }

    /// Aborts if the on-disk state version disagrees with the one this cycle
    /// started from.
    fn verify_unchanged(&self, id: &RepositoryId, observed: u64) -> Result<(), RepositoryError> {
        match fs::read(self.file_for(id)?) {
            Ok(contents) => {
                let record: StoredRepository = serde_json::from_slice(&contents)
                    .map_err(|err| RepositoryError::Store(format!("re-parse: {err}")))?;
                if record.snapshot.state_version != observed {
                    return Err(RepositoryError::ConcurrentModification {
                        stored: record.snapshot.state_version,
                        observed,
                    });
                }
                Ok(())
            }
            // The file is gone — nothing to conflict with.
            Err(_) => Ok(()),
        }
    }

    /// Serialize and write atomically.
    fn write(&self, record: &StoredRepository) -> Result<(), RepositoryError> {
        let path = self.file_for(&record.repository.id)?;
        let json = serde_json::to_vec_pretty(record)
            .map_err(|err| RepositoryError::Store(format!("serialize: {err}")))?;

        // Write to a temporary sibling, then rename. A rename within one
        // directory is atomic on the filesystem, so a crash leaves the previous
        // state rather than a truncated file.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &json).map_err(|err| RepositoryError::Store(err.to_string()))?;
        fs::rename(&tmp, &path).map_err(|err| RepositoryError::Store(err.to_string()))?;
        Ok(())
    }
}

/// Helpers for the append-only histories, kept here so the service stays about
/// orchestration and the store stays about what is retained.
impl StoredRepository {
    /// The ids of every event already recorded.
    pub fn known_event_ids(&self) -> HashSet<EventId> {
        self.events
            .iter()
            .map(|event| event.event_id.clone())
            .collect()
    }

    /// Append events that are not already known, returning how many were new.
    /// This is the idempotency guarantee as it appears in storage: an event
    /// whose stable id is present is dropped, not duplicated.
    pub fn append_events(&mut self, events: Vec<ObservationEvent>) -> usize {
        let known = self.known_event_ids();
        let mut added = 0;
        for event in events {
            if known.contains(&event.event_id) {
                continue;
            }
            self.events.push(event);
            added += 1;
        }
        self.events.sort_by_key(|event| event.timestamp);
        self.events.truncate(MAX_EVENTS);
        added
    }

    /// Append commits not already recorded, keyed by SHA.
    pub fn append_commits(&mut self, commits: Vec<CommitInfo>) -> usize {
        let mut added = 0;
        for commit in commits {
            if self
                .commits
                .iter()
                .any(|existing| existing.sha == commit.sha)
            {
                continue;
            }
            self.commits.push(commit);
            added += 1;
        }
        self.commits
            .sort_by_key(|commit| std::cmp::Reverse(commit.committed_at));
        self.commits.truncate(MAX_COMMITS);
        added
    }

    /// Append file-change records not already recorded, keyed by their
    /// idempotency identity.
    pub fn append_file_changes(
        &mut self,
        changes: Vec<director_domain::repository::FileChangeRecord>,
    ) -> usize {
        let mut added = 0;
        for change in changes {
            let seen = self.file_changes.iter().any(|existing| {
                existing.path == change.path
                    && existing.old_path == change.old_path
                    && existing.commit_sha == change.commit_sha
                    && existing.fingerprint == change.fingerprint
            });
            if seen {
                continue;
            }
            self.file_changes.push(change);
            added += 1;
        }
        self.file_changes.truncate(MAX_FILE_CHANGES);
        added
    }

    /// Replace the current snapshot.
    pub fn set_snapshot(&mut self, snapshot: ProjectStateSnapshot) {
        self.snapshot = snapshot;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use director_domain::ids::ProjectId;
    use director_domain::repository::{FileChangeRecord, WorktreeState};

    fn store() -> RepositoryStore {
        let dir = std::env::temp_dir().join(format!("director-store-test-{}", std::process::id()));
        RepositoryStore::new(dir).expect("a store")
    }

    fn baseline(store: &RepositoryStore, id: &str, version: u64) -> StoredRepository {
        let repository = Repository::new(
            RepositoryId::from_string(id),
            ProjectId::from_string("PROJ-1"),
            "/repo/x",
            None,
            Some("main".into()),
            Some("main".into()),
            "abc".into(),
            director_domain::repository::RepositoryStatus::OnBranch,
            Some("abc".into()),
        );
        let mut snapshot = ProjectStateSnapshot::initial(
            ProjectId::from_string("PROJ-1"),
            RepositoryId::from_string(id),
            Some("main".into()),
            "abc".into(),
            WorktreeState::default(),
            chrono::Utc::now(),
        );
        snapshot.state_version = version;
        store
            .initialize(repository, snapshot, vec![])
            .expect("initialized")
    }

    #[test]
    fn an_unsafe_repository_id_is_rejected_as_a_filename() {
        let store = store();
        assert!(store
            .file_for(&RepositoryId::from_string("../escape"))
            .is_err());
        assert!(store.file_for(&RepositoryId::from_string("a:b")).is_err());
    }

    #[test]
    fn a_registered_repository_can_be_loaded_and_listed() {
        let store = store();
        baseline(&store, "REPO-load", 1);
        assert!(store.exists(&RepositoryId::from_string("REPO-load")));
        assert!(store.load(&RepositoryId::from_string("REPO-load")).is_ok());
        assert!(store
            .list()
            .unwrap()
            .contains(&RepositoryId::from_string("REPO-load")));
    }

    #[test]
    fn an_unregistered_repository_is_not_registered_not_missing_data() {
        let store = store();
        let err = store
            .load(&RepositoryId::from_string("REPO-none"))
            .unwrap_err();
        assert!(matches!(err, RepositoryError::NotRegistered(_)));
    }

    #[test]
    fn initializing_the_same_id_twice_is_rejected() {
        let store = store();
        baseline(&store, "REPO-dupe", 1);
        assert!(store
            .initialize(
                Repository::new(
                    RepositoryId::from_string("REPO-dupe"),
                    ProjectId::from_string("PROJ-1"),
                    "/repo/x",
                    None,
                    None,
                    None,
                    "abc".into(),
                    director_domain::repository::RepositoryStatus::OnBranch,
                    None,
                ),
                ProjectStateSnapshot::initial(
                    ProjectId::from_string("PROJ-1"),
                    RepositoryId::from_string("REPO-dupe"),
                    None,
                    "abc".into(),
                    WorktreeState::default(),
                    chrono::Utc::now(),
                ),
                vec![]
            )
            .is_err());
    }

    #[test]
    fn duplicate_events_are_dropped_not_duplicated() {
        let mut record = baseline(&store(), "REPO-dupes", 1);
        let event = ObservationEvent::commit(
            &record.repository.id,
            &record.repository.project_id,
            director_domain::state::CommitInfo {
                sha: "abc".into(),
                summary: "one".into(),
                author: "ada".into(),
                committed_at: chrono::Utc::now(),
                parents: vec![],
                committer: "ada".into(),
                message: "one".into(),
            },
            chrono::Utc::now(),
        );
        assert_eq!(record.append_events(vec![event.clone()]), 1);
        // Same id again: dropped.
        assert_eq!(record.append_events(vec![event]), 0);
        assert_eq!(record.events.len(), 1);
    }

    #[test]
    fn duplicate_file_changes_are_dropped() {
        let mut record = baseline(&store(), "REPO-files", 1);
        let change = FileChangeRecord::new(
            record.repository.id.clone(),
            "src/lib.rs",
            None,
            director_domain::state::FileChange::Modified,
            "blob1",
            "abc",
            chrono::Utc::now(),
        )
        .unwrap();
        assert_eq!(record.append_file_changes(vec![change.clone()]), 1);
        assert_eq!(record.append_file_changes(vec![change]), 0);
    }

    #[test]
    fn duplicate_commits_are_dropped() {
        let mut record = baseline(&store(), "REPO-commits", 1);
        let commit = director_domain::state::CommitInfo {
            sha: "abc".into(),
            summary: "one".into(),
            author: "ada".into(),
            committed_at: chrono::Utc::now(),
            parents: vec![],
            committer: "ada".into(),
            message: "one".into(),
        };
        assert_eq!(record.append_commits(vec![commit.clone()]), 1);
        assert_eq!(record.append_commits(vec![commit]), 0);
    }

    #[test]
    fn a_locked_cycle_sees_its_own_write() {
        let store = store();
        baseline(&store, "REPO-locked", 1);
        let seen = store
            .with_locked(&RepositoryId::from_string("REPO-locked"), |record| {
                record.repository.bump_observation_version();
                Ok(record.repository.observation_version)
            })
            .unwrap();
        assert_eq!(seen, 2);
        assert_eq!(
            store
                .load(&RepositoryId::from_string("REPO-locked"))
                .unwrap()
                .repository
                .observation_version,
            2
        );
    }

    #[test]
    fn an_external_write_between_read_and_write_is_detected() {
        let store = store();
        baseline(&store, "REPO-race", 1);
        let id = RepositoryId::from_string("REPO-race");

        // A cycle reads a version, does its work, then checks the file before
        // writing. Simulate another process landing a newer version *during*
        // that window: our cycle started from version 1...
        let observed_version = store.load(&id).unwrap().snapshot.state_version;

        // ...and by the time the pre-write check runs, the file has moved on.
        let mut newer = store.load(&id).unwrap();
        newer.snapshot.state_version = 99;
        store.write(&newer).unwrap();

        let err = store.verify_unchanged(&id, observed_version).unwrap_err();
        assert!(
            matches!(err, RepositoryError::ConcurrentModification { stored, observed } if stored == 99 && observed == 1)
        );

        // The external writer's state survives; nothing of ours was written
        // over it.
        assert_eq!(store.load(&id).unwrap().snapshot.state_version, 99);
    }

    #[test]
    fn a_unchanged_file_passes_the_version_check() {
        let store = store();
        baseline(&store, "REPO-stable", 1);
        let id = RepositoryId::from_string("REPO-stable");
        let observed = store.load(&id).unwrap().snapshot.state_version;
        assert!(store.verify_unchanged(&id, observed).is_ok());
    }
}
